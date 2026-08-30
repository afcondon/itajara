//! Where the bar comes from.
//!
//! The looper can measure a cycle but it cannot know how many *bars* that cycle
//! was: `loop_len` is a number of frames and nothing in the engine has ever had
//! an opinion about metre. So "quantise to one bar" has no referent until
//! something outside says what a bar is — and on this rig something already
//! does. link-spike publishes `/link/anchor` at about 10 Hz, carrying the affine
//! map from wall clock to Link's beat grid, and purerl-tidal (57121) and
//! es9-daemon (57123) have been consuming it for months.
//!
//! Itajara is the third consumer, on 57125. Adding it means one line in
//! link-spike's `ANCHOR_TARGETS`, which carries a comment inviting exactly that.
//!
//! **What this module does and does not do.** It takes the anchor and keeps
//! tempo, beat, quantum and the moment each arrived. From tempo alone comes the
//! length of a bar in frames, which is enough to quantise a loop's *length* —
//! to round a recording to a whole number of bars. It does **not** yet place us
//! within the bar, because that needs the output frame counter tied to wall
//! clock, and the honest join for that is `p0`/`p0_frame` — the same pairing the
//! K calibration rests on. es9-daemon deferred the identical step for the
//! identical reason, and guessing at it would produce a phase that looks
//! plausible and is wrong, which is worse than one that is absent. `link_frame`
//! is recorded now precisely so that work has its anchor point when it happens.

use std::net::UdpSocket;
use std::sync::atomic::Ordering;
use std::sync::Arc;

use rosc::{OscPacket, OscType};

use crate::engine::Shared;

/// Itajara's slot in the anchor fan-out, after purerl-tidal (57121) and
/// es9-daemon (57123).
pub const DEFAULT_ANCHOR_PORT: u16 = 57125;

/// link-spike's inbound OSC socket — the same one the MIDI dispatch verbs go
/// to, and where `/link/set-tempo` is answered.
///
/// **The one place this daemon talks outward.** Everywhere else it listens:
/// anchors in, commands in over the WebSocket, audio in and out. Setting the
/// tempo is different in kind, because Link is a *session* — the number reaches
/// Ableton, purerl-tidal and es9-daemon's tempo-relative rates, not just this
/// looper. That is a reason to be deliberate about it, not a reason to avoid
/// it: the alternative is a click you cannot bring into line with what you
/// played.
pub const DEFAULT_TEMPO_PORT: u16 = 57122;

/// Ask Link for a tempo, through link-spike.
///
/// **Fire and forget, and honest about it.** There is no reply to this — the
/// evidence that it worked is the next `/link/anchor`, arriving about a tenth
/// of a second later with the new tempo in it. So this reports only whether the
/// bytes left the machine, and the caller says the rest.
///
/// `OscType::Double` rather than `Float`: link-spike accepts either, and a
/// tempo derived from a frame count has more significant digits than an f32
/// keeps. Rounding it to a float here would put the click a fraction of a
/// percent off the loop it was taken from, which over a few minutes is exactly
/// the drift this is meant to end.
pub fn set_tempo(bpm: f64, port: u16) -> Result<(), String> {
    let msg = rosc::OscMessage {
        addr: "/link/set-tempo".into(),
        args: vec![OscType::Double(bpm)],
    };
    let bytes = rosc::encoder::encode(&OscPacket::Message(msg))
        .map_err(|e| format!("could not encode the tempo message: {}", e))?;
    // A fresh ephemeral socket per call rather than one held open. This happens
    // once in a while at human speed, never from the audio callback, and a
    // socket held for months is a socket that can go stale without anything
    // saying so.
    let sock = UdpSocket::bind(("127.0.0.1", 0))
        .map_err(|e| format!("could not open a socket to send it on: {}", e))?;
    sock.send_to(&bytes, ("127.0.0.1", port))
        .map_err(|e| format!("could not reach link-spike on {}: {}", port, e))?;
    Ok(())
}

/// Start or stop the Link session's transport, through link-spike.
///
/// **The verb the iPad answers to.** Patterning, AUM and Ableton all follow
/// Link's Start/Stop Sync; none of them follow anything else this rig sends.
/// So the way to make a drum machine play on cue is not to reach for its own
/// transport at all — it is to start the session, and let every peer that
/// cares come in on the downbeat together.
///
/// Fire and forget, like `set_tempo`, and for the same reason: link-spike is
/// the only thing that can see the session and there is no reply channel back.
/// What confirms it is hearing the drums.
///
/// link-spike schedules a start for the **next bar line** without moving the
/// beat grid, which is why a grab and a `g`-quantised recording land together
/// — both are waiting for the same downbeat, derived from the same anchor.
pub fn set_playing(on: bool, port: u16) -> Result<(), String> {
    let msg = rosc::OscMessage {
        addr: "/link/play".into(),
        args: vec![OscType::Int(if on { 1 } else { 0 })],
    };
    let bytes = rosc::encoder::encode(&OscPacket::Message(msg))
        .map_err(|e| format!("could not encode the transport message: {}", e))?;
    let sock = UdpSocket::bind(("127.0.0.1", 0))
        .map_err(|e| format!("could not open a socket to send it on: {}", e))?;
    sock.send_to(&bytes, ("127.0.0.1", port))
        .map_err(|e| format!("could not reach link-spike on {}: {}", port, e))?;
    Ok(())
}

/// Listen for `/link/anchor` and keep the newest one.
///
/// Binding failure is reported and then tolerated: no Link means no bar, which
/// costs quantisation, and a looper that refuses to start because a clock is
/// absent would be a looper that cannot be used alone. The count of anchors
/// seen is what tells the app which of those two worlds it is in.
pub fn spawn_listener(sh: Arc<Shared>, sr: u32, port: u16) {
    let socket = match UdpSocket::bind(("127.0.0.1", port)) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("looper: no Link anchor on {}: {}", port, e);
            eprintln!("        free loops still work; bar quantisation will not.");
            return;
        }
    };
    println!("Link:   listening for /link/anchor on 127.0.0.1:{}", port);

    std::thread::spawn(move || {
        let mut buf = [0u8; 1024];
        // Announced on arrival and on change, like every other thing this
        // daemon discovers about its world. A clock that is present and a clock
        // that is absent otherwise look the same from the console, and the bar
        // is about to be the thing loop lengths are rounded to.
        let mut announced: Option<(f64, f64)> = None;
        loop {
            let Ok((n, _from)) = socket.recv_from(&mut buf) else { continue };
            let Ok((_, packet)) = rosc::decoder::decode_udp(&buf[..n]) else { continue };
            let OscPacket::Message(msg) = packet else { continue };
            if msg.addr != "/link/anchor" {
                continue;
            }
            let (Some(micros), Some(beat), Some(tempo), Some(quantum)) = (
                as_i64(msg.args.first()),
                as_f64(msg.args.get(1)),
                as_f64(msg.args.get(2)),
                as_f64(msg.args.get(3)),
            ) else {
                sh.link_rejected.fetch_add(1, Ordering::Relaxed);
                continue;
            };

            // A tempo of zero would make a bar infinitely long and a quantum of
            // zero would divide by it. Neither has ever been sent, which is the
            // reason to check: an anchor that changed shape would otherwise be
            // adopted silently and show up as a looper that quantises to
            // nonsense. Counting the refusals makes that visible instead.
            if !(tempo > 0.0 && tempo < 1000.0) || !(quantum >= 1.0 && quantum <= 64.0) {
                sh.link_rejected.fetch_add(1, Ordering::Relaxed);
                continue;
            }

            sh.link_micros.store(micros, Ordering::Relaxed);
            sh.link_beat.store(beat.to_bits(), Ordering::Relaxed);
            sh.link_tempo.store(tempo.to_bits(), Ordering::Relaxed);
            sh.link_quantum.store(quantum.to_bits(), Ordering::Relaxed);
            // Which output frame we were on when this landed: the half of the
            // wall-clock-to-frame join that has to be captured at the moment of
            // arrival and cannot be recovered later.
            let at = sh.out_frames.load(Ordering::Acquire);
            sh.link_frame.store(at, Ordering::Relaxed);

            // **And here is the other half, finally joined.** The engine has
            // had the tempo since Link landed and has never been able to use
            // it, because a tempo gives a bar's *length* and not where the bar
            // *falls* — `Shared::grid` says so and offers another loop's cycle
            // instead. Both numbers are in scope on this line and nowhere else,
            // so the join is made here and stored as a phase the audio side can
            // read without knowing anything about wall clocks.
            //
            // `beat` counts beats from Link's zero; a bar is `quantum` of them.
            // So the bar containing this anchor began `beat mod quantum` beats
            // ago, and every bar line is that frame plus a whole number of bar
            // lengths — forwards or backwards, which is why the origin is
            // signed and may sit before the stream started.
            match crate::engine::bar_frames(tempo, quantum, sr) {
                Some(bar) => {
                    sh.link_bar_origin.store(
                        crate::engine::bar_origin(beat, quantum, tempo, at, sr),
                        Ordering::Relaxed,
                    );
                    sh.link_bar_frames.store(bar, Ordering::Relaxed);
                }
                // Refused rather than left stale: a bar length that no longer
                // matches the tempo is worse than no bar at all, because
                // everything downstream would go on trusting it.
                None => sh.link_bar_frames.store(0, Ordering::Relaxed),
            }
            sh.link_anchors.fetch_add(1, Ordering::Release);

            let changed = match announced {
                Some((t, q)) => (t - tempo).abs() > 0.01 || (q - quantum).abs() > 0.01,
                None => true,
            };
            if changed {
                announced = Some((tempo, quantum));
                match crate::engine::bar_frames(tempo, quantum, sr) {
                    Some(f) => println!(
                        "  Link: {:.2} bpm, {} to the bar — a bar is {} frames ({:.3} s).",
                        tempo, quantum, f, f as f64 / sr as f64
                    ),
                    None => println!("  Link: {:.2} bpm, but no usable bar length.", tempo),
                }
            }
        }
    });
}

fn as_f64(a: Option<&OscType>) -> Option<f64> {
    match a {
        Some(OscType::Double(d)) => Some(*d),
        Some(OscType::Float(f)) => Some(*f as f64),
        _ => None,
    }
}

fn as_i64(a: Option<&OscType>) -> Option<i64> {
    match a {
        Some(OscType::Long(l)) => Some(*l),
        Some(OscType::Int(i)) => Some(*i as i64),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use rosc::{encoder, OscMessage, OscPacket, OscType};

    /// The shape link-spike actually sends, copied from its `send_anchor`:
    /// `(Long unix_micros, Double beat, Double tempo, Double quantum)`.
    /// What this pins down is the argument *order* — the thing a reader gets
    /// wrong, and the thing that would yield a plausible tempo taken from the
    /// beat slot.
    fn anchor(micros: i64, beat: f64, tempo: f64, quantum: f64) -> Vec<u8> {
        encoder::encode(&OscPacket::Message(OscMessage {
            addr: "/link/anchor".into(),
            args: vec![
                OscType::Long(micros),
                OscType::Double(beat),
                OscType::Double(tempo),
                OscType::Double(quantum),
            ],
        }))
        .expect("encode")
    }

    #[test]
    fn tempo_comes_from_the_third_argument_not_the_second() {
        let bytes = anchor(1_700_000_000_000_000, 8.25, 120.0, 4.0);
        let (_, packet) = rosc::decoder::decode_udp(&bytes).expect("decode");
        let OscPacket::Message(msg) = packet else { panic!("not a message") };
        assert_eq!(msg.addr, "/link/anchor");
        assert_eq!(super::as_f64(msg.args.get(1)), Some(8.25));
        assert_eq!(super::as_f64(msg.args.get(2)), Some(120.0));
        assert_eq!(super::as_f64(msg.args.get(3)), Some(4.0));
        assert_eq!(super::as_i64(msg.args.first()), Some(1_700_000_000_000_000));
    }

    #[test]
    fn a_bar_is_quantum_beats_at_the_given_tempo() {
        // 120 bpm, 4 beats to the bar, 48 kHz: a beat is half a second, a bar
        // is two seconds, so 96000 frames.
        assert_eq!(crate::engine::bar_frames(120.0, 4.0, 48_000), Some(96_000));
        // 90 bpm in 3: a beat is 2/3 s, a bar is 2 s.
        assert_eq!(crate::engine::bar_frames(90.0, 3.0, 48_000), Some(96_000));
        assert_eq!(crate::engine::bar_frames(0.0, 4.0, 48_000), None);
    }

    /// **The half of the join that could not be made before.** A bar's length
    /// came from the tempo; where the bar *falls* needed the beat position and
    /// the frame counter in the same place, which only happens at the moment an
    /// anchor lands.
    #[test]
    fn a_bar_origin_is_the_frame_the_current_bar_began_on() {
        // 120 bpm, 4/4, 48k: a beat is 24000 frames and a bar is 96000. An
        // anchor taken exactly on a downbeat names the frame it arrived on.
        assert_eq!(crate::engine::bar_origin(8.0, 4.0, 120.0, 500_000, 48_000), 500_000);
        // One beat into the bar: the bar began 24000 frames ago.
        assert_eq!(crate::engine::bar_origin(9.0, 4.0, 120.0, 500_000, 48_000), 476_000);
        // Three and a half beats in.
        assert_eq!(crate::engine::bar_origin(11.5, 4.0, 120.0, 500_000, 48_000), 416_000);
    }

    /// It is a phase and not an event, so a bar line before the stream started
    /// is a correct answer rather than an error to clamp away.
    #[test]
    fn a_bar_origin_may_sit_before_the_stream_started() {
        assert!(crate::engine::bar_origin(3.0, 4.0, 120.0, 1_000, 48_000) < 0);
    }

    /// Three to the bar is three beats, not three quarters of four — the metre
    /// is the quantum's business and the beat stays a beat.
    #[test]
    fn the_quantum_is_beats_to_the_bar_not_a_fraction_of_one() {
        assert_eq!(crate::engine::bar_frames(120.0, 3.0, 48_000), Some(72_000));
        assert_eq!(crate::engine::bar_origin(4.0, 3.0, 120.0, 100_000, 48_000), 76_000);
    }
}
