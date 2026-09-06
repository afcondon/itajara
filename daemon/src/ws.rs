//! The socket the browser talks to.
//!
//! The split is the one in DESIGN-LOOPER §7: this daemon owns buffers, the
//! sample clock and latency compensation; the app owns UX and MIDI. So the app
//! is also the MIDI hub — a footswitch press arrives at the app as a CC, and the
//! app sends the corresponding command here. The daemon never opens a MIDI port,
//! which keeps exactly one process talking to the MC6.
//!
//! Two directions, deliberately asymmetric:
//!
//! - **In:** the same command strings the console takes, through the same
//!   `dispatch`. A footswitch, a browser button and a terminal cannot drift
//!   into meaning different things by the same name if there is only one
//!   place that decides what a name means.
//! - **Out:** a state snapshot, pushed continuously rather than requested.
//!   A looper's whole problem is that its state is invisible, so the display
//!   should never have to ask.
//!
//! Synchronous, one thread per connection, no async runtime. There will be one
//! or two clients, and a looper that needs a scheduler to serve a status line
//! has its priorities wrong.

use std::io::ErrorKind;
use std::net::TcpListener;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

use crate::engine::{dispatch, Layer, Shared};

/// How often the snapshot goes out. Fast enough for a position readout to look
/// continuous, slow enough to be free.
const PUSH_HZ: u64 = 30;

pub fn serve(sh: Arc<Shared>, sr: u32, port: u16) {
    let listener = match TcpListener::bind(("127.0.0.1", port)) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("looper: could not bind port {}: {}", port, e);
            eprintln!("        the app will show as disconnected; everything else still works.");
            return;
        }
    };
    println!("Socket: ws://127.0.0.1:{}", port);

    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(stream) = stream else { continue };
            let sh = sh.clone();
            std::thread::spawn(move || {
                if let Err(e) = talk(sh, sr, stream) {
                    // A browser tab closing is the ordinary case, not a fault.
                    let msg = e.to_string();
                    if !msg.contains("Connection closed") && !msg.contains("reset") {
                        eprintln!("looper: client gone ({})", msg);
                    }
                }
            });
        }
    });
}

fn talk(
    sh: Arc<Shared>,
    sr: u32,
    stream: std::net::TcpStream,
) -> Result<(), Box<dyn std::error::Error>> {
    // The read timeout is what lets one thread do both jobs: it turns a blocking
    // read into "check for a command, then push the state", at the push rate.
    stream.set_read_timeout(Some(Duration::from_millis(1000 / PUSH_HZ)))?;
    let mut ws = tungstenite::accept(stream)?;
    println!("  app connected.");

    // Liveness is measured, not assumed. This thread only reads shared atomics,
    // so it will happily serve a plausible-looking snapshot from an engine whose
    // audio callbacks stopped — which is exactly what happened when the USB bus
    // was unplugged mid-session, and it cost an afternoon of looking for a MIDI
    // fault. Watching the output frame counter makes the failure visible from
    // the app instead.
    // **Edits run in order, on one worker per connection.** Every command
    // used to run on a thread of its own, so that the ones that block on
    // purpose — ending a multiply waits for the cycle boundary — would not
    // hold up the snapshot. That also let a burst of edits from a slider
    // execute out of order: a straggler landing after the restart had fired
    // re-applied an older window and restarted the loop again, which was
    // "it keeps restarting after I stop". So the editing verbs go through a
    // channel and run in the order they were sent; everything else keeps its
    // own thread, because a press must not queue behind a multiply.
    let (tx, rx) = std::sync::mpsc::channel::<String>();
    {
        let sh = sh.clone();
        std::thread::spawn(move || {
            for cmd in rx {
                let ack = dispatch(&sh, sr, &cmd);
                if !ack.is_empty() {
                    println!("  [app] {}", ack);
                    sh.note_ack(&ack);
                }
            }
        });
    }

    let mut last_frames = sh.out_frames.load(Ordering::Acquire);
    let mut still = 0u32;
    // Peaks go out once per answer, as their own message before the next
    // snapshot; a connection that arrives later does not get the old one.
    let mut peaks_seen = sh.peaks_seq.load(Ordering::Acquire);
    // At 30 Hz, a second and a half of a motionless counter. Longer than any
    // buffer, shorter than anyone's patience.
    const STILL_LIMIT: u32 = PUSH_HZ as u32 * 3 / 2;

    loop {
        match ws.read() {
            Ok(tungstenite::Message::Text(cmd)) => {
                // **Every command, before anything is decided about it.**
                //
                // The ack below is conditional — `dispatch` returns a string
                // for some arms and `println!`s for others — so for fourteen
                // verbs, the transport among them, a command used to arrive
                // and leave no trace anywhere at all. Nothing outside this
                // process could then answer "did the app send it, or did the
                // app not send it": not the snapshot, not stdout, not the
                // client. A press that did nothing and a press that never
                // happened looked the same from every angle.
                //
                // So this is unconditional and comes first. It is the only
                // place that knows a command arrived rather than what it
                // meant, and that is exactly the fact worth keeping.
                println!("  [cmd] {}", cmd.trim());
                // On its own thread, because some commands block on purpose.
                // Ending a multiply waits for the cycle boundary to arrive —
                // up to half a cycle — and committing waits for the input to
                // drain. Running those here would freeze the state push for
                // exactly as long, which is precisely when the display most
                // needs to be moving. The snapshot must keep flowing whatever
                // the engine is busy doing.
                if is_edit(&cmd) {
                    // A closed worker means this connection is on its way out.
                    if tx.send(cmd.to_string()).is_err() {
                        return Ok(());
                    }
                } else {
                    let sh = sh.clone();
                    std::thread::spawn(move || {
                        let ack = dispatch(&sh, sr, &cmd);
                        if !ack.is_empty() {
                            println!("  [app] {}", ack);
                            sh.note_ack(&ack);
                        }
                    });
                }
            }
            Ok(tungstenite::Message::Close(_)) => {
                println!("  app disconnected.");
                return Ok(());
            }
            Ok(_) => {}
            Err(tungstenite::Error::Io(e))
                if e.kind() == ErrorKind::WouldBlock || e.kind() == ErrorKind::TimedOut =>
            {
                // No command this tick. Expected, thirty times a second.
            }
            Err(e) => return Err(Box::new(e)),
        }

        let frames = sh.out_frames.load(Ordering::Acquire);
        if frames == last_frames {
            still = still.saturating_add(1);
        } else {
            still = 0;
            last_frames = frames;
        }
        let alive = still < STILL_LIMIT && !sh.device_lost.load(Ordering::Acquire);

        let seq = sh.peaks_seq.load(Ordering::Acquire);
        if seq != peaks_seen {
            peaks_seen = seq;
            let p = sh.peaks.lock().map(|s| s.clone()).unwrap_or_default();
            if !p.is_empty() {
                ws.send(tungstenite::Message::Text(p))?;
            }
        }
        ws.send(tungstenite::Message::Text(snapshot(&sh, sr, alive)))?;
    }
}

/// One layer, as JSON: the `LayerShape` the other side reads.
///
/// **The only place a layer is written to the wire**, and it has to stay
/// that way. It used to be two hand-written format strings — one under each
/// loop, one at the top level for the selected loop — and both are read as
/// one `LayerShape` type on the other side, coerced from JSON with nothing
/// checking. Sending three fields here and six there did not fail politely:
/// the app compares snapshots to decide whether to redraw, that comparison
/// reads `env`, and PureScript's array equality opens with `xs.length`, so a
/// missing field threw a TypeError ten times a second and froze the display
/// while the socket, the commands and the audio all stayed healthy
/// (2026-08-23). Two serialisers for one type was the whole bug; if the two
/// places ever need to diverge, give them their own types on both sides.
///
/// `tail` is the continuation held past this layer's end — never sounded,
/// and the only material a seamless wrap could be made from. Reported so the
/// display can say a loop has it rather than leaving it invisible. `env` is
/// forty-eight bytes, and only for layers that exist — small enough to ride
/// here rather than needing a message of its own, a request to trigger it,
/// and a way for it to be out of date.
fn layer_json(layer: &Layer) -> String {
    let (len, period, phase) = layer.shape();
    format!(
        r#"{{"len":{},"period":{},"phase":{},"tail":{},"gain":{:.5},"born":{},"on":{},"lwIn":{},"lwOut":{},"env":[{}]}}"#,
        len,
        period,
        phase,
        layer.tail(),
        layer.gain(),
        layer.born(),
        layer.on(),
        layer.window().map(|w| w.0).unwrap_or(0),
        layer.window().map(|w| w.1).unwrap_or(0),
        layer
            .env()
            .iter()
            .map(|b| b.to_string())
            .collect::<Vec<_>>()
            .join(",")
    )
}

/// One loop, as JSON: the `LoopState` the other side reads, one of the
/// `loops` that `rig_json` carries.
///
/// **The only place a loop is written to the wire.** Its layers go through
/// `layer_json`, and the reason given there is the reason for this function
/// too. `cur` is the output frame the whole snapshot is drawn at — read once
/// by the caller, so eight positions describe one instant.
fn loop_json(sh: &Shared, li: usize, sr: u32, cur: i64) -> String {
    let lp = sh.lp(li);
    let len = lp.loop_len.load(Ordering::Acquire);
    // Through the engine's own playhead rather than subtracting `origin`
    // here, so the display cannot disagree with the audio about where a
    // loop is — which it would the moment speed or a pendulum was on.
    let pos = lp.play_pos(cur, len) as i64;
    let shapes: Vec<String> = (0..lp.n_layers.load(Ordering::Acquire))
        .map(|l| layer_json(&lp.layers[l]))
        .collect();
    format!(
        concat!(
            r#"{{"index":{},"state":"{}","layers":{},"loopFrames":{},"#,
            r#""loopSecs":{:.4},"pos":{},"phase":{:.5},"armed":{},"#,
            r#""recording":{},"quant":{},"muted":{},"reverse":{},"pan":{},"#,
            r#""speed":{:.4},"pendulum":{},"oneShot":{},"levelArm":{},"#,
            r#""firing":{},"chance":{:.4},"skipping":{},"fadeMs":{:.1},"decayDb":{:.2},"#,
            r#""volDb":{:.2},"revox":{},"fbDb":{:.2},"toneHz":{:.0},"cycles":{},"winIn":{},"winOut":{},"rot":{},"#,
            r#""src":{},"mono":{},"pendingAt":{},"recFrames":{},"recEnv":[{}],"shapes":[{}]}}"#
        ),
        li,
        lp.state_name(),
        lp.n_layers.load(Ordering::Acquire),
        len,
        len as f64 / sr as f64,
        pos,
        if len > 0 { pos as f64 / len as f64 } else { 0.0 },
        lp.is_armed(),
        lp.is_recording(),
        lp.quantised(),
        lp.muted.load(Ordering::Relaxed),
        // Direction is the sign of speed in the engine; it is reported
        // separately as well because the display asks "which way round
        // is this" far more often than it asks "how fast".
        lp.speed() < 0.0,
        lp.pan.load(Ordering::Relaxed),
        lp.speed().abs(),
        lp.pendulum.load(Ordering::Relaxed),
        // The two modes. Reported because the pedal cannot show them and
        // because they change what a *tap* means: a tap on a one-shot
        // fires it where a tap on any other loop stops it, and the app
        // has to know which before the foot lands.
        lp.one_shot.load(Ordering::Relaxed),
        lp.level_arm.load(Ordering::Relaxed),
        // Inside a pass, or between them. The playhead never stops — it
        // cannot — so `pos` alone shows a one-shot sweeping along while
        // it is silent, which is a display describing something nobody
        // can hear.
        lp.firing(cur),
        // How often this loop plays, and whether it is sitting this
        // pass out. `skipping` reads the mixer's decision and never
        // makes one — a snapshot that rolled would decide passes on
        // whether anybody was looking.
        lp.chance_of(),
        lp.skipping(cur, len),
        // In milliseconds rather than frames, so the display never has
        // to know the sample rate to say what a switch did.
        lp.fade.load(Ordering::Relaxed) as f64 / sr as f64 * 1000.0,
        // In decibels a pass, the unit it was asked for. Zero holds for
        // ever, which is what every loop did before this existed.
        {
            let d = lp.decay_of();
            if d >= 1.0 { 0.0 } else { 20.0 * (d.max(1e-9) as f64).log10() }
        },
        // This loop's level, in decibels, unity at zero. **Silence is
        // reported as the floor rather than as negative infinity**,
        // which is not a number JSON has and not a number a knob can be
        // put at — the client's own scale bottoms out at -60 and reads
        // that as off.
        {
            let g = f32::from_bits(lp.vol.load(Ordering::Relaxed));
            if g >= 1.0 { 0.0 }
            else if g <= 0.0 { -60.0 }
            else { 20.0 * (g.max(1e-9) as f64).log10() }
        },
        // Frames until a scheduled transition fires, or -1 for nothing
        // pending. A display that can show "starts in 1.4 s" is the
        // difference between a deliberate wait and a dead button.
        // Whether this loop is a tape, and what a pass over it leaves.
        // Reported because it changes what every other control means —
        // undo is gone, an overdub makes no layer — and a mode you
        // cannot see is a mode you will be surprised by.
        lp.revox.load(Ordering::Relaxed),
        {
            let g = f32::from_bits(lp.fb.load(Ordering::Relaxed));
            if g >= 1.0 { 0.0 }
            else if g <= 0.0 { -60.0 }
            else { 20.0 * (g.max(1e-9) as f64).log10() }
        },
        f32::from_bits(lp.tone.load(Ordering::Relaxed)),
        // How many bars this loop has been told it is. Zero means never
        // told, which reads as one everywhere — reported as stored, so
        // the app can tell "one bar" from "nobody has said".
        lp.cycles.load(Ordering::Acquire),
        // As the hand has set them — pending while an edit waits to
        // be applied — so the page shows what was asked, not the past.
        lp.edit_view().0,
        lp.edit_view().1,
        lp.edit_view().2,
        // **One-based, because it is an index into a list a person
        // reads.** The wire counts loops from zero and that is a debt
        // being carried; a field added today should not add to it.
        sh.src_of(li) + 1,
        lp.mono.load(Ordering::Relaxed),
        lp.pending_in(cur),
        lp.rec_frames(sh.out_frames.load(Ordering::Acquire) as i64),
        // **The take in hand, drawn while it is being played.** Empty
        // whenever nothing is recording, so the display has one test
        // rather than having to work the state out for itself — and so
        // a finished take stops being drawn twice the instant it
        // becomes a layer.
        if lp.is_recording() {
            lp.rec_env_bytes()
                .iter()
                .map(|b| b.to_string())
                .collect::<Vec<_>>()
                .join(",")
        } else {
            String::new()
        },
        shapes.join(","),
    )
}

/// The rig, as JSON: the `LooperState` the other side reads, which is the top
/// level of the snapshot.
///
/// **Rig-level facts only** — the device, the clock, the arena, the sources,
/// the last ack — and then `loops`, one `loop_json` each. Nothing about any
/// one loop is written here. It used to be: the selected loop's `state`,
/// `layers`, `loopFrames`, `loopSecs`, `pos`, `phase`, `armed`, `recording`
/// and `shapes` were repeated at the top level for a page written when there
/// was one loop, deliberately and temporarily, until every surface read
/// `loops[i]`; they all do, and the repetition went on 2026-09-06.
///
/// Hand-rolled rather than pulling in a serialiser: the shape is fixed, small,
/// and this way it is obvious at a glance what the app is being told. If it
/// grows a variable shape, that is the moment to reach for serde and not
/// before.
fn rig_json(sh: &Shared, sr: u32, alive: bool) -> String {
    let cur = sh.out_frames.load(Ordering::Acquire) as i64;
    let each: Vec<String> = (0..sh.n_loops).map(|li| loop_json(sh, li, sr, cur)).collect();

    // Peaks are swapped out, so each reader gets the peak since the last read
    // rather than a decaying maximum. With one client that is exactly right;
    // with two they share, which is a meter problem and not a correctness one.
    // The loudest of every source. One number for a strip that has one meter;
    // per-source metering is a display question and this is the honest summary
    // until there is somewhere to put four of them.
    let in_peak = sh
        .in_peak
        .iter()
        .map(|p| f32::from_bits(p.swap(0, Ordering::Relaxed)))
        .fold(0.0f32, f32::max);
    let out_peak = f32::from_bits(sh.out_peak.swap(0, Ordering::Relaxed));

    // The last thing a command said, carried in every snapshot rather than sent
    // once. A client that reloads still sees it, and one that misses a frame has
    // not missed the only copy.
    let ack = sh.ack.lock().map(|g| g.clone()).unwrap_or_default();

    let tempo = f64::from_bits(sh.link_tempo.load(Ordering::Relaxed));
    let quantum = f64::from_bits(sh.link_quantum.load(Ordering::Relaxed));

    format!(
        concat!(
            r#"{{"maxLayers":{},"sampleRate":{},"#,
            r#""inDb":{:.1},"outDb":{:.1},"click":{},"monitor":{},"armDb":{:.1},"#,
            r#""calibrated":{},"k":{},"#,
            r#""audioAlive":{},"deviceLost":{},"reopens":{},"#,
            r#""ack":"{}","ackSeq":{},"linkTempo":{:.4},"linkQuantum":{:.4},"#,
            r#""linkBarFrames":{},"linkAnchors":{},"linkRejected":{},"#,
            r#""barFrames":{},"barOrigin":{},"launchQ":{},"#,
            r#""maxSecs":{:.3},"fixedSecs":{:.3},"ringSecs":{:.3},"selected":{},"nLoops":{},"sources":[{}],"loops":[{}]}}"#
        ),
        sh.max_layers,
        sr,
        db(in_peak),
        db(out_peak),
        sh.click.load(Ordering::Relaxed),
        sh.monitor.load(Ordering::Relaxed),
        // The level a sound has to reach to start a level-armed loop, in
        // decibels. Rig-wide, like the click — it describes the room and the
        // instrument, not any one loop.
        //
        // Reported because it is on a knob now, and a knob that holds a value
        // nothing can read back is a knob that can only be wrong.
        {
            let m = f32::from_bits(sh.arm_thresh.load(Ordering::Relaxed));
            if m <= 0.0 { -80.0 } else { (20.0 * (m.max(1e-9) as f64).log10()).max(-80.0) }
        },
        sh.k_set.load(Ordering::Acquire),
        sh.k.load(Ordering::Acquire),
        alive,
        sh.device_lost.load(Ordering::Acquire),
        sh.reopens.load(Ordering::Acquire),
        escape(&ack),
        sh.ack_seq.load(Ordering::Acquire),
        tempo,
        quantum,
        // Zero rather than null when there is no clock: the app's snapshot type
        // is a flat record of plain values, and one nullable field would make
        // every reader of it handle an absence that `linkAnchors == 0` already
        // states more precisely.
        crate::engine::bar_frames(tempo, quantum, sr).unwrap_or(0),
        sh.link_anchors.load(Ordering::Acquire),
        sh.link_rejected.load(Ordering::Relaxed),
        // **The bar the engine is actually using**, which is not always Link's:
        // with no clock it is the first loop's cycle divided by however many
        // bars that loop has been declared to be. `linkBarFrames` above is what
        // the clock says and is zero without one; this is what lengths are
        // counted in either way, and the app should read this one.
        sh.grid().map(|(_, len)| len).unwrap_or(0),
        sh.grid().map(|(o, _)| o).unwrap_or(0),
        sh.launch_q.load(Ordering::Relaxed),
        // **The shape**, with `nLoops`, `maxLayers` and `sampleRate`: what a
        // surface lays itself out from. Static for the daemon's life, and in
        // every snapshot anyway, because the display never has to ask.
        sh.max_frames as f64 / sr as f64,
        sh.fixed_frames as f64 / sr as f64,
        sh.ring_len as f64 / sr as f64,
        // The loop a console verb with no loop digit addresses. Once the loop
        // whose fields were repeated above; now only that.
        sh.sel(),
        sh.n_loops,
        // **The sources, named, in the order a `src<n>` counts them.** Without
        // this the app would have a number and no way to say what it meant, and
        // "input 2" on an encoder is the numbering problem all over again.
        sh.sources
            .iter()
            .map(|s| format!(r#"{{"name":"{}","mono":{}}}"#, escape(&s.name), s.is_mono()))
            .collect::<Vec<_>>()
            .join(","),
        each.join(","),
    )
}

/// The whole visible state of the engine, as JSON: one `rig_json`. This is the
/// message; that is the type. Three emitters write it — `rig_json`,
/// `loop_json`, `layer_json` — one per type on the wire, each called once per
/// instance, and `check-snapshot.py` holds the running daemon to the
/// PureScript side.
pub(crate) fn snapshot(sh: &Shared, sr: u32, alive: bool) -> String {
    rig_json(sh, sr, alive)
}

/// Enough JSON string escaping for the one free-text field in the snapshot.
///
/// Acks carry filesystem paths and error text from the OS, neither of which
/// this code chose, so they can contain quotes and backslashes — and an
/// unescaped one would not corrupt the ack, it would make the whole snapshot
/// unparseable and take the display down with it.
fn escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 8);
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out
}

fn db(x: f32) -> f64 {
    // Floored rather than -inf, because JSON has no infinity and a meter with a
    // bottom is more useful than one without.
    (20.0 * (x.max(1e-9) as f64).log10()).max(-120.0)
}

/// The verbs that must keep their order: the window, the rotation and the
/// waveform. Spelled the way `dispatch` spells them, after the loop digits.
fn is_edit(cmd: &str) -> bool {
    let rest = cmd.trim().trim_start_matches(|c: char| c.is_ascii_digit());
    ["in", "out", "win", "rot", "pk", "ly", "lw", "dp"].iter().any(|v| rest.starts_with(v))
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::Ordering;

    use super::{layer_json, loop_json, rig_json};
    use crate::engine::tests::fixture;
    use crate::engine::{ENV_BUCKETS, Layer};

    /// **The wire did not move.** The literal is one layer's object as the
    /// pre-`Layer` emitter wrote it (loop 1's third layer of the engine's
    /// `fixture`, captured on `main` at de1c1a0): same keys, same order, same
    /// number formatting. `check-snapshot.py` holds the running daemon to the
    /// PureScript readers; this holds the function to the text.
    #[test]
    fn a_layer_is_written_as_it_always_was() {
        let layer = Layer::new();
        layer.len.store(300, Ordering::Release);
        layer.born.store(1, Ordering::Release);
        layer.gain.store(10f32.powf(-6.0 / 20.0).to_bits(), Ordering::Release);
        layer.win_in.store(100, Ordering::Relaxed);
        layer.win_out.store(200, Ordering::Release);
        *layer.env.lock().unwrap() = vec![211; ENV_BUCKETS];
        assert_eq!(
            layer_json(&layer),
            format!(
                r#"{{"len":300,"period":1,"phase":0,"tail":0,"gain":0.50119,"born":1,"on":true,"lwIn":100,"lwOut":200,"env":[{}]}}"#,
                ["211"; ENV_BUCKETS].join(",")
            )
        );
        // And a bare one, which is what an empty slot's layers are.
        assert_eq!(
            layer_json(&Layer::new()),
            r#"{"len":0,"period":1,"phase":0,"tail":0,"gain":1.00000,"born":0,"on":true,"lwIn":0,"lwOut":0,"env":[]}"#
        );
    }

    /// **The loop did not move either.** Loop 2 of the engine's `fixture` —
    /// one sparse layer at three-quarter speed, folded to mono and panned —
    /// as the pre-`loop_json` emitter wrote it into `loops[]` (captured on
    /// `main` at c6b52b2), and an empty slot beside it. Same keys, same order,
    /// same number formatting; only the top level shrank.
    #[test]
    fn a_loop_is_written_as_it_always_was() {
        let sh = fixture();
        let cur = sh.out_frames.load(Ordering::Acquire) as i64;
        assert_eq!(
            loop_json(&sh, 2, 48_000, cur),
            format!(
                concat!(
                    r#"{{"index":2,"state":"idle","layers":1,"loopFrames":1000,"loopSecs":0.0208,"pos":0,"phase":0.00000,"armed":false,"recording":false,"quant":false,"muted":false,"reverse":false,"pan":30,"speed":0.7500,"pendulum":false,"oneShot":false,"levelArm":false,"firing":false,"chance":1.0000,"skipping":false,"fadeMs":0.0,"decayDb":0.00,"volDb":-1.94,"revox":false,"fbDb":-3.00,"toneHz":6500,"cycles":0,"winIn":0,"winOut":0,"rot":0,"src":1,"mono":true,"pendingAt":-1,"recFrames":0,"recEnv":[],"#,
                    r#""shapes":[{{"len":250,"period":4,"phase":2,"tail":0,"gain":1.00000,"born":0,"on":true,"lwIn":0,"lwOut":0,"env":[{}]}}]}}"#
                ),
                ["229"; ENV_BUCKETS].join(",")
            )
        );
        assert_eq!(
            loop_json(&sh, 4, 48_000, cur),
            r#"{"index":4,"state":"idle","layers":0,"loopFrames":0,"loopSecs":0.0000,"pos":0,"phase":0.00000,"armed":false,"recording":false,"quant":false,"muted":false,"reverse":false,"pan":64,"speed":1.0000,"pendulum":false,"oneShot":false,"levelArm":false,"firing":false,"chance":1.0000,"skipping":false,"fadeMs":0.0,"decayDb":0.00,"volDb":0.00,"revox":false,"fbDb":-3.00,"toneHz":6500,"cycles":0,"winIn":0,"winOut":0,"rot":0,"src":1,"mono":false,"pendingAt":-1,"recFrames":0,"recEnv":[],"shapes":[]}"#
        );
    }

    /// **The top level is the rig and nothing else.** Its keys, in the order
    /// they are written; none of the nine that used to repeat the selected
    /// loop; and `loops` is `loop_json` once per loop, in order.
    #[test]
    fn the_rig_is_only_the_rig() {
        let sh = fixture();
        let text = rig_json(&sh, 48_000, true);
        let v: serde_json::Value = serde_json::from_str(&text).expect("rig_json is JSON");
        let obj = v.as_object().expect("an object");
        let mut keys: Vec<&str> = obj.keys().map(|k| k.as_str()).collect();
        keys.sort_unstable();
        let mut expected = vec![
            "maxLayers", "sampleRate", "inDb", "outDb", "click", "monitor", "armDb",
            "calibrated", "k", "audioAlive", "deviceLost", "reopens", "ack", "ackSeq",
            "linkTempo", "linkQuantum", "linkBarFrames", "linkAnchors", "linkRejected",
            "barFrames", "barOrigin", "launchQ", "maxSecs", "fixedSecs", "ringSecs",
            "selected", "nLoops", "sources", "loops",
        ];
        expected.sort_unstable();
        assert_eq!(keys, expected);
        for gone in ["state", "layers", "loopFrames", "loopSecs", "pos", "phase", "armed", "recording", "shapes"] {
            assert!(!obj.contains_key(gone), "{} is a loop's, not the rig's", gone);
        }
        assert_eq!(text.starts_with(r#"{"maxLayers":"#), true, "{}", text);
        // The loops are the loop emitter's text, verbatim and in order.
        let cur = sh.out_frames.load(Ordering::Acquire) as i64;
        let loops: Vec<String> = (0..sh.n_loops).map(|li| loop_json(&sh, li, 48_000, cur)).collect();
        assert!(text.ends_with(&format!(r#","loops":[{}]}}"#, loops.join(","))));
        assert_eq!(obj["loops"].as_array().map(|a| a.len()), Some(sh.n_loops));
    }
}
