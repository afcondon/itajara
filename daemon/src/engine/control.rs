//! The two threads that speak to the engine from outside the audio: the
//! console, and the closer that ends a take of a known length.
//!
//! Split out of `engine.rs` on 2026-09-06 (REVIEW-daemon-debt step 1).

use std::io::BufRead;
use std::time::Duration;
use std::sync::atomic::Ordering;
use std::sync::Arc;

use super::{FIRST, OVERDUB};
use super::commit::commit;
use super::dispatch::dispatch;
use super::shared::Shared;

/// Returns true only if the user actually asked to quit.
///
/// EOF is not a quit. Run headless — from a launcher, or with output
/// redirected — and `lines()` returns immediately, which must not be allowed to
/// take the audio engine and the socket down with it.
pub(crate) fn control_loop(sh: &Shared, sr: u32) -> bool {
    println!("Commands:  r = record/overdub toggle   x = multiply   t [secs] = take");
    println!("           s [n] = spread one in n   o = move it one slot   d = dense again");
    println!("           u = undo a layer   z = forget the length   c = both");
    println!("           w [name] = save the take (one file per layer + manifest)");
    println!("           g = follow the grid (the first loop's cycle) / free");
    println!(
        "           a leading digit picks the loop: 3r records loop 3, 3s2 spreads it,\n\
         \x20          a bare 3 selects it. {} loops, 0 to {}.",
        sh.n_loops,
        sh.n_loops - 1
    );
    println!("           k = click   m = input monitoring");
    println!("           l = levels   p = status + waveforms   q = quit\n");

    let stdin = std::io::stdin();
    for line in stdin.lock().lines() {
        let Ok(line) = line else { return false };
        if line.trim() == "q" {
            return true;
        }
        let ack = dispatch(sh, sr, &line);
        if !ack.is_empty() {
            println!("  {}", ack);
        }
    }
    false
}

/// Close a recording, as of the moment the foot went down rather than the
/// moment the command arrived.
///
/// `late` is how many frames ago the closing press happened. It is not a
/// nicety: a switch that may be double-tapped cannot resolve until the
/// double-tap window expires, so every close arrives a fixed few hundred
/// milliseconds after the press, and a free loop was coming out that much
/// longer than it was played. Nothing in the sound says so — overdubs are
/// modular against whatever length the loop ended up with, so everything still
/// stacks perfectly against a cycle nobody chose.
///
/// The fix is not to hurry the gesture but to un-do the delay: the audio for
/// those milliseconds is already in the arena, and the loop simply ends
/// earlier than the last frame recorded. Which is also why adding a double-tap
/// to a switch stopped costing anything recorded.
/// **The second press, made unnecessary.**
///
/// One thread for the whole rig, polling every five milliseconds for a first
/// recording that has reached the length it was told to be. Five is the same
/// interval `multiply_end` already waits at and is a fortieth of the shortest
/// bar anyone will use; the close it produces is quantised to the loop's own
/// length by construction, so the poll's own jitter never reaches the audio —
/// `commit` is handed the target frame, not the frame it woke up on.
///
/// A thread rather than the callback because closing a recording draws a layer
/// and sleeps. A poll rather than a scheduled wake because there are six loops
/// and one of them might be re-armed while another is closing, and a timer per
/// recording is a timer to cancel.
///
/// **It re-checks before it acts, and that is the cancellation.** A foot that
/// closes the take early leaves the state at `PLAYING`; a clear leaves the
/// length at zero; a new recording moves `rec_from`. Any of those and this
/// finds the world it was told about is gone, and does nothing. There is no
/// flag to forget to clear.
pub(crate) fn spawn_closer(sh: Arc<Shared>, sr: u32) {
    std::thread::spawn(move || loop {
        std::thread::sleep(Duration::from_millis(5));
        let now = sh.out_frames.load(Ordering::Acquire) as i64;
        for li in 0..sh.n_loops {
            let lp = sh.lp(li);
            let at = lp.close_at.load(Ordering::Acquire);
            if at == i64::MIN || now < at {
                continue;
            }
            // Taken before the check, so two ticks cannot both close one take.
            if lp
                .close_at
                .compare_exchange(at, i64::MIN, Ordering::AcqRel, Ordering::Relaxed)
                .is_err()
            {
                continue;
            }
            // A first take with a length, or a one-pass overdub: both set
            // `close_at`, nothing else does.
            if !matches!(lp.state.get(), FIRST | OVERDUB) {
                continue;
            }
            // `late` is how far past the target we woke, so `commit` closes the
            // loop at the length it was asked for rather than at the length the
            // poll happened to notice.
            let msg = commit(&sh, li, sr, now - at);
            println!("  {}", msg);
            sh.note_ack(&msg);
        }
    });
}
