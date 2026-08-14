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

        ws.send(tungstenite::Message::Text(snapshot(&sh, sr)))?;
    }
}

/// The whole visible state of the engine, as JSON.
///
/// Hand-rolled rather than pulling in a serialiser: the shape is fixed, small,
/// and this way it is obvious at a glance what the app is being told. If it
/// grows a variable shape, that is the moment to reach for serde and not
/// before.
fn snapshot(sh: &Shared, sr: u32) -> String {
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

    format!(
        concat!(
            r#"{{"state":"{}","layers":{},"maxLayers":{},"loopFrames":{},"#,
            r#""loopSecs":{:.4},"pos":{},"phase":{:.5},"sampleRate":{},"#,
            r#""inDb":{:.1},"outDb":{:.1},"click":{},"monitor":{},"#,
            r#""armed":{},"recording":{},"calibrated":{},"k":{}}}"#
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
    )
}

fn db(x: f32) -> f64 {
    // Floored rather than -inf, because JSON has no infinity and a meter with a
    // bottom is more useful than one without.
    (20.0 * (x.max(1e-9) as f64).log10()).max(-120.0)
}
