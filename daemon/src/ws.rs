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

use crate::engine::{dispatch, Shared};

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
    let mut last_frames = sh.out_frames.load(Ordering::Acquire);
    let mut still = 0u32;
    // At 30 Hz, a second and a half of a motionless counter. Longer than any
    // buffer, shorter than anyone's patience.
    const STILL_LIMIT: u32 = PUSH_HZ as u32 * 3 / 2;

    loop {
        match ws.read() {
            Ok(tungstenite::Message::Text(cmd)) => {
                // On its own thread, because some commands block on purpose.
                // Ending a multiply waits for the cycle boundary to arrive —
                // up to half a cycle — and committing waits for the input to
                // drain. Running those here would freeze the state push for
                // exactly as long, which is precisely when the display most
                // needs to be moving. The snapshot must keep flowing whatever
                // the engine is busy doing.
                let sh = sh.clone();
                std::thread::spawn(move || {
                    let ack = dispatch(&sh, sr, &cmd);
                    if !ack.is_empty() {
                        println!("  [app] {}", ack);
                        // Which is where it used to stop. Printing to the
                        // daemon's stdout told whoever was looking at a
                        // terminal, and the app — the only thing that is
                        // definitely watching — learned nothing at all.
                        sh.note_ack(&ack);
                    }
                });
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

        ws.send(tungstenite::Message::Text(snapshot(&sh, sr, alive)))?;
    }
}

/// The whole visible state of the engine, as JSON.
///
/// Hand-rolled rather than pulling in a serialiser: the shape is fixed, small,
/// and this way it is obvious at a glance what the app is being told. If it
/// grows a variable shape, that is the moment to reach for serde and not
/// before.
fn snapshot(sh: &Shared, sr: u32, alive: bool) -> String {
    let cur = sh.out_frames.load(Ordering::Acquire) as i64;

    // One object per loop, and separately the SELECTED loop's numbers repeated
    // at the top level.
    //
    // The duplication is deliberate and temporary. The app's `LooperState` is a
    // flat record describing one loop, written when there was one; promoting it
    // to an array is a change to every reader of it. Keeping the old fields
    // pointed at the selected loop means the existing Looper page keeps working
    // untouched while the six-loop display is built against `loops` — rather
    // than the display and the engine both being new at once, which is how you
    // end up debugging two things and knowing neither.
    let each: Vec<String> = (0..crate::engine::N_LOOPS)
        .map(|li| {
            let lp = sh.lp(li);
            let len = lp.loop_len.load(Ordering::Acquire);
            // Through the engine's own playhead rather than subtracting `origin`
            // here, so the display cannot disagree with the audio about where a
            // loop is — which it would the moment speed or a pendulum was on.
            let pos = lp.play_pos(cur, len) as usize;
            let shapes: Vec<String> = (0..lp.n_layers.load(Ordering::Acquire))
                .map(|l| {
                    let (slen, period, phase) = lp.layer_shape(l);
                    // `tail` is the continuation held past this layer's end —
                    // never sounded, and the only material a seamless wrap
                    // could be made from. Reported so the display can say a
                    // loop has it rather than leaving it invisible.
                    format!(
                        r#"{{"len":{},"period":{},"phase":{},"tail":{},"gain":{:.5}}}"#,
                        slen, period, phase, lp.layer_tail(l), lp.layer_gain(l)
                    )
                })
                .collect();
            format!(
                concat!(
                    r#"{{"index":{},"state":"{}","layers":{},"loopFrames":{},"#,
                    r#""loopSecs":{:.4},"pos":{},"phase":{:.5},"armed":{},"#,
                    r#""recording":{},"quant":{},"muted":{},"reverse":{},"pan":{},"#,
                    r#""speed":{:.4},"pendulum":{},"oneShot":{},"levelArm":{},"#,
                    r#""firing":{},"chance":{:.4},"skipping":{},"fadeMs":{:.1},"decayDb":{:.2},"#,
                    r#""pendingAt":{},"shapes":[{}]}}"#
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
                // Frames until a scheduled transition fires, or -1 for nothing
                // pending. A display that can show "starts in 1.4 s" is the
                // difference between a deliberate wait and a dead button.
                lp.pending_in(cur),
                shapes.join(","),
            )
        })
        .collect();

    let sel = sh.sel();
    let cl = sh.lp(sel);
    let loop_len = cl.loop_len.load(Ordering::Acquire);
    let pos = cl.play_pos(cur, loop_len) as usize;

    // Peaks are swapped out, so each reader gets the peak since the last read
    // rather than a decaying maximum. With one client that is exactly right;
    // with two they share, which is a meter problem and not a correctness one.
    let in_peak = f32::from_bits(sh.in_peak.swap(0, Ordering::Relaxed));
    let out_peak = f32::from_bits(sh.out_peak.swap(0, Ordering::Relaxed));

    // Each layer's own length and where it sounds. Without this the app can draw
    // a loop and not what is in it: two takes of the same length look identical
    // when one of them plays one bar in four, and that is precisely the thing
    // the display exists to make visible.
    let shapes: Vec<String> = (0..cl.n_layers.load(Ordering::Acquire))
        .map(|l| {
            let (len, period, phase) = cl.layer_shape(l);
            format!(
                r#"{{"len":{},"period":{},"phase":{}}}"#,
                len, period, phase
            )
        })
        .collect();

    // The last thing a command said, carried in every snapshot rather than sent
    // once. A client that reloads still sees it, and one that misses a frame has
    // not missed the only copy.
    let ack = sh.ack.lock().map(|g| g.clone()).unwrap_or_default();

    let tempo = f64::from_bits(sh.link_tempo.load(Ordering::Relaxed));
    let quantum = f64::from_bits(sh.link_quantum.load(Ordering::Relaxed));

    format!(
        concat!(
            r#"{{"state":"{}","layers":{},"maxLayers":{},"loopFrames":{},"#,
            r#""loopSecs":{:.4},"pos":{},"phase":{:.5},"sampleRate":{},"#,
            r#""inDb":{:.1},"outDb":{:.1},"click":{},"monitor":{},"#,
            r#""armed":{},"recording":{},"calibrated":{},"k":{},"#,
            r#""audioAlive":{},"deviceLost":{},"reopens":{},"shapes":[{}],"#,
            r#""ack":"{}","ackSeq":{},"linkTempo":{:.4},"linkQuantum":{:.4},"#,
            r#""linkBarFrames":{},"linkAnchors":{},"linkRejected":{},"#,
            r#""selected":{},"nLoops":{},"loops":[{}]}}"#
        ),
        cl.state_name(),
        cl.n_layers.load(Ordering::Acquire),
        crate::engine::MAX_LAYERS,
        loop_len,
        loop_len as f64 / sr as f64,
        pos,
        if loop_len > 0 { pos as f64 / loop_len as f64 } else { 0.0 },
        sr,
        db(in_peak),
        db(out_peak),
        sh.click.load(Ordering::Relaxed),
        sh.monitor.load(Ordering::Relaxed),
        cl.is_armed(),
        cl.is_recording(),
        sh.k_set.load(Ordering::Acquire),
        sh.k.load(Ordering::Acquire),
        alive,
        sh.device_lost.load(Ordering::Acquire),
        sh.reopens.load(Ordering::Acquire),
        shapes.join(","),
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
        sel,
        crate::engine::N_LOOPS,
        each.join(","),
    )
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
