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
    let loop_len = sh.loop_len.load(Ordering::Acquire);
    let origin = sh.origin.load(Ordering::Acquire);
    let cur = sh.out_frames.load(Ordering::Acquire) as i64;
    let pos = if loop_len > 0 {
        (cur - origin).rem_euclid(loop_len as i64) as usize
    } else {
        0
    };

    // Peaks are swapped out, so each reader gets the peak since the last read
    // rather than a decaying maximum. With one client that is exactly right;
    // with two they share, which is a meter problem and not a correctness one.
    let in_peak = f32::from_bits(sh.in_peak.swap(0, Ordering::Relaxed));
    let out_peak = f32::from_bits(sh.out_peak.swap(0, Ordering::Relaxed));

    // Each layer's own length and where it sounds. Without this the app can draw
    // a loop and not what is in it: two takes of the same length look identical
    // when one of them plays one bar in four, and that is precisely the thing
    // the display exists to make visible.
    let shapes: Vec<String> = (0..sh.n_layers.load(Ordering::Acquire))
        .map(|l| {
            let (len, period, phase) = sh.layer_shape(l);
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
            r#""linkBarFrames":{},"linkAnchors":{},"linkRejected":{}}}"#
        ),
        sh.state_name(),
        sh.n_layers.load(Ordering::Acquire),
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
        sh.is_armed(),
        sh.is_recording(),
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
