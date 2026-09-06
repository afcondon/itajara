//! Edits that take effect on a restart: threading a blank tape, and the
//! settled window-and-rotation restart.
//!
//! Split out of `engine.rs` on 2026-09-06 (REVIEW-daemon-debt step 1).

use std::sync::atomic::Ordering;

use super::{PLAYING, Shape};
use super::shared::Shared;

/// Thread an empty tape of `len` frames onto loop `li`: one blank layer,
/// playing, so the first recording closes itself at that length. What `blank`
/// does, and what `--fixed-secs` does to every loop at startup and after `c`.
pub fn thread_blank(sh: &Shared, li: usize, len: usize) {
    let lp = sh.lp(li);
    let now = sh.out_frames.load(Ordering::Acquire) as i64;
    sh.zero_layer(li, 0);
    lp.origin.store(now, Ordering::Release);
    lp.loop_len.store(len, Ordering::Release);
    lp.set_layer_shape(0, Shape { len, tail: 0, born: 0 });
    lp.n_layers.store(1, Ordering::Release);
    lp.threaded.store(true, Ordering::Relaxed);
    lp.state.set(PLAYING);
    sh.rebuild_env(li, 0);
}

/// Schedule the restart an edit asks for: a short way ahead so a moving
/// slider coalesces into one, and on the next bar line for a loop that is
/// on the grid or is the grid. See `Loop::edit_restart`.
pub(crate) fn schedule_restart(sh: &Shared, li: usize, win_in: i64, win_out: i64, rot: usize) {
    let lp = sh.lp(li);
    lp.pend_in.store(win_in, Ordering::Relaxed);
    lp.pend_out.store(win_out, Ordering::Relaxed);
    lp.pend_rot.store(rot, Ordering::Relaxed);
    lp.pend_set.store(true, Ordering::Release);
    let now = sh.out_frames.load(Ordering::Acquire) as i64;
    let soon = now + EDIT_SETTLE_FRAMES;
    let at = if lp.quantised() || sh.anchor.load(Ordering::Acquire) == li {
        sh.next_boundary(soon).unwrap_or(soon)
    } else {
        soon
    };
    lp.edit_restart.store(at, Ordering::Release);
}

/// How long the edits have to stop for before the pass restarts: 150 ms at
/// 48 kHz, which is longer than the gap between two slider events and
/// shorter than a hand pausing to listen.
const EDIT_SETTLE_FRAMES: i64 = 7_200;
