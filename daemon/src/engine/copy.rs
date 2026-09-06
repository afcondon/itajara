//! Copying a loop's layers onto an empty loop.
//!
//! Split out of `engine.rs` on 2026-09-06 (REVIEW-daemon-debt step 1).

use std::sync::atomic::Ordering;

use super::{CHANNELS, PLAYING, Shape};
use super::shared::Shared;

/// Copy loop `src`'s layers — all of them, or one — into loop `dst`, which must
/// be empty.
///
/// ## Onto empty loops only
///
/// The copy defines the destination: its length is the source's, its layers
/// are the source's, and it starts in phase with the source (same `origin`),
/// so a copy is a second voice of the same take, not a stranger. What a copy
/// onto a loop that already holds something would *mean* — reconcile two
/// lengths, tile, crop — is a design question this refuses to answer by
/// accident; the ack says so. A threaded blank (`--fixed-secs`, or `len` on
/// an empty loop) counts as empty: it holds silence and a length, and the
/// copy replaces both.
///
/// ## Whole layers, no window
///
/// The source's window and rotation are its own and are not copied: the
/// layers land whole and the destination plays the whole of them, so the
/// destination can be windowed to a *different* thirteen seconds. That is the
/// point — one long take, copied to several empty loops, windowed
/// differently in each, is several sources for a granular module.
///
/// Off the audio thread, like every command: nothing reads an empty loop's
/// arena, so the copy can take its time.
pub(crate) fn copy_layers(sh: &Shared, dst: usize, src: usize, layer: Option<usize>) -> String {
    if src >= sh.n_loops {
        return format!("there is no loop {}.", src);
    }
    if src == dst {
        return format!("loop {} onto itself is nothing.", dst);
    }
    let from = sh.lp(src);
    let to = sh.lp(dst);
    if from.is_recording() || from.is_armed() {
        return format!("loop {} is still recording — finish it before copying from it.", src);
    }
    if to.is_recording() || to.is_armed() {
        return format!("loop {} is recording — a copy lands on an empty loop.", dst);
    }
    let n_from = from.n_layers.load(Ordering::Acquire);
    let src_len = from.loop_len.load(Ordering::Acquire);
    if n_from == 0 || src_len == 0 {
        return format!("loop {} has nothing to copy.", src);
    }
    let blank = to.threaded.load(Ordering::Relaxed);
    if !blank && (to.n_layers.load(Ordering::Acquire) > 0 || to.loop_len.load(Ordering::Acquire) > 0) {
        return format!(
            "loop {} is not empty — copies land on empty loops only; `{}c` first if you mean it.",
            dst, dst
        );
    }
    let chosen: Vec<usize> = match layer {
        Some(k) if k >= n_from => return format!("loop {} has {} layer{}, not {}.", src, n_from, if n_from == 1 { "" } else { "s" }, k + 1),
        Some(k) => vec![k],
        None => (0..n_from).collect(),
    };
    let chosen: Vec<usize> = chosen.into_iter().filter(|&l| from.layers[l].len.load(Ordering::Acquire) > 0).collect();
    if chosen.is_empty() {
        return format!("loop {} has nothing to copy.", src);
    }
    if chosen.len() > sh.max_layers {
        return format!("loop {} has {} layers to copy and a loop here holds {}.", src, chosen.len(), sh.max_layers);
    }

    // Empty first, so nothing of a former take survives past a shorter copy.
    to.n_layers.store(0, Ordering::Release);
    for j in 0..sh.max_layers {
        sh.zero_layer(dst, j);
    }
    for (j, &l) in chosen.iter().enumerate() {
        let len = from.layers[l].len.load(Ordering::Acquire);
        for p in 0..len {
            for ch in 0..CHANNELS {
                sh.write(dst, j, p, ch, sh.read(src, l, p, ch));
            }
        }
        to.set_layer_shape(j, Shape {
            len,
            tail: from.layers[l].tail.load(Ordering::Relaxed),
            born: from.layers[l].born.load(Ordering::Relaxed),
        });
        to.layers[j].period.store(from.layers[l].period.load(Ordering::Relaxed), Ordering::Release);
        to.layers[j].phase.store(from.layers[l].phase.load(Ordering::Relaxed), Ordering::Release);
        // A layer's own window is the slice the player chose of it, so it
        // travels with the layer; the loop's window does not.
        to.layers[j].win_in.store(from.layers[l].win_in.load(Ordering::Relaxed), Ordering::Relaxed);
        to.layers[j].win_out.store(from.layers[l].win_out.load(Ordering::Relaxed), Ordering::Relaxed);
        to.layers[j].on.store(true, Ordering::Release);
        sh.rebuild_env(dst, j);
    }
    to.loop_len.store(src_len, Ordering::Release);
    to.cycles.store(from.cycles.load(Ordering::Acquire), Ordering::Release);
    to.quant.store(from.quant.load(Ordering::Relaxed), Ordering::Relaxed);
    to.origin.store(from.origin.load(Ordering::Relaxed), Ordering::Release);
    to.win_in.store(0, Ordering::Relaxed);
    to.win_out.store(0, Ordering::Relaxed);
    to.rot.store(0, Ordering::Relaxed);
    to.pend_set.store(false, Ordering::Release);
    to.threaded.store(false, Ordering::Relaxed);
    to.n_layers.store(chosen.len(), Ordering::Release);
    to.redo_to.store(chosen.len(), Ordering::Release);
    to.state.set(PLAYING);

    match layer {
        Some(k) => format!(
            "copied layer {} of loop {} onto loop {} ({:.3} s), whole and in phase; window it as you like.",
            k + 1, src, dst, src_len as f64 / 48_000.0
        ),
        None => format!(
            "copied {} layer{} of loop {} onto loop {}, whole and in phase; window it as you like.",
            chosen.len(), if chosen.len() == 1 { "" } else { "s" }, src, dst
        ),
    }
}
