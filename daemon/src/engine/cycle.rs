//! A loop's cycle: how long it is (multiply, bars, fix, free), where its
//! layers land in it (sparse, place, rotate, dense), the tempo it implies,
//! and starting every cycle from the top together.
//!
//! Split out of `engine.rs` on 2026-09-06 (REVIEW-daemon-debt step 1).

use std::time::Duration;
use std::sync::atomic::Ordering;

use super::{FIRE, IDLE, MAX_BARS, MAX_PERIOD, MULTIPLY, PLAYING, Shape};
use super::commit::{draw_layer, fill_from_ring};
use super::shared::Shared;

/// Begin a multiply: keep the loop playing and start recording across it.
///
/// The EDP's gesture, and the one this whole thing was asked for — two bars
/// down, a couple of taps, and you are recording eight with the two repeating
/// underneath.
///
/// **It starts at the beginning of the cycle you are in, not when you pressed.**
/// The pre-roll holds that cycle already, so the part you have played of it is
/// recovered rather than lost, and the multiply lands on the grid instead of
/// wherever your foot happened to be. Pressing late is free.
pub(crate) fn multiply_start(sh: &Shared, li: usize, sr: u32) -> String {
    let lp = sh.lp(li);
    let loop_len = lp.loop_len.load(Ordering::Acquire);
    if loop_len == 0 {
        return format!("loop {} has nothing to multiply — record a loop first.", li);
    }
    if lp.n_layers.load(Ordering::Acquire) >= sh.max_layers {
        return format!(
            "loop {} is at {} layers, the ceiling; undo one first.",
            li, sh.max_layers
        );
    }

    let origin = lp.origin.load(Ordering::Acquire);
    let cur = sh.out_frames.load(Ordering::Acquire) as i64;
    let cyc = (cur - origin).div_euclid(loop_len as i64);
    let from = origin + cyc * loop_len as i64;

    let layer = lp.n_layers.load(Ordering::Acquire);
    sh.zero_layer(li, layer);
    lp.rec_from.store(from, Ordering::Release);
    lp.reached.store(0, Ordering::Release);
    lp.state.set(MULTIPLY);

    // The part of this cycle already played is in the pre-roll; claim it, so
    // the multiply really does begin on the boundary.
    let behind = (cur - from) as usize;
    // One sentence, not three. At a console three lines read as a paragraph; in
    // a single-line display the last one wins and the other two never existed.
    // The instruction ("x again to end it") is the part worth keeping, because
    // a multiply is the one gesture that is not finished when you let go.
    if behind > 0 {
        let got = fill_from_ring(sh, li, layer, from, behind, 0, false);
        lp.reached.fetch_max(got, Ordering::Relaxed);
        format!(
            "loop {} multiplying from the start of this cycle ({:.2} s recovered from \
             the pre-roll) — play across as many cycles as you want, then x again.",
            li,
            got as f64 / sr as f64
        )
    } else {
        format!(
            "loop {} multiplying from this cycle's start — play across as many cycles \
             as you want, then x again.",
            li
        )
    }
}

/// End a multiply: round to whole cycles and grow the loop to fit.
///
/// Rounding rather than truncating, because at nine tenths of the way through
/// the fourth cycle you meant four. Which means sometimes waiting for the
/// boundary to arrive rather than cutting the loop short at the press.
pub(crate) fn multiply_end(sh: &Shared, li: usize, sr: u32) -> String {
    let lp = sh.lp(li);
    let loop_len = lp.loop_len.load(Ordering::Acquire);
    let from = lp.rec_from.load(Ordering::Acquire);
    let cur = sh.out_frames.load(Ordering::Acquire) as i64;
    let elapsed = (cur - from).max(0) as f64;

    let n = ((elapsed / loop_len as f64).round() as usize).max(1);
    let new_len = n * loop_len;
    if new_len > sh.max_frames {
        lp.state.set(PLAYING);
        return format!(
            "loop {}: {} cycles would be {:.1} s, past the --max-secs ceiling of {:.1} s. \
             Stopping at the old length.",
            li,
            n,
            new_len as f64 / sr as f64,
            sh.max_frames as f64 / sr as f64
        );
    }

    // If the rounding went up, the last cycle has not finished yet. Wait for it
    // rather than hand back a loop that is short by however late the press was.
    let target = from + new_len as i64;
    // Said in the ACK rather than only here, and after the fact rather than
    // before it: this call blocks until the boundary arrives, so a message sent
    // now could not reach the app before the outcome does anyway. It matters
    // because a press that appears to do nothing for half a cycle is exactly
    // the kind of pause that gets pressed again.
    let rounded = if target > cur {
        format!(
            " (rounded up, waited {:.2} s for the boundary)",
            (target - cur) as f64 / sr as f64
        )
    } else {
        String::new()
    };
    if target > cur {
        while (sh.out_frames.load(Ordering::Acquire) as i64) < target {
            std::thread::sleep(Duration::from_millis(5));
        }
    }
    // And let the input drain past it, since it trails by K.
    lp.state.set(PLAYING);
    std::thread::sleep(Duration::from_millis(60));

    // "With the original repeating underneath" now costs nothing. Every existing
    // layer keeps its own length at `period = 1`, and the mix wraps it inside
    // the longer cycle by itself. This used to copy the audio n times, which
    // worked and threw away the structure: afterwards there was no one-bar thing
    // to make sparse, alternate or move, because it had been smeared across four
    // bars of buffer. The multiply began on a cycle boundary, so each layer's
    // position zero still lands where it did.

    // The new loop's position zero is where the multiply began.
    lp.origin.store(from, Ordering::Release);
    lp.loop_len.store(new_len, Ordering::Release);

    let layer = lp.n_layers.load(Ordering::Acquire);
    // A multiplied layer ends where the multiply ended; nothing follows it. Born
    // at zero because a multiply redefines the cycle, so every pass count on
    // this loop starts again from here.
    lp.set_layer_shape(layer, Shape { len: new_len, tail: 0, born: 0 });
    sh.rebuild_env(li, layer);
    lp.add_layer();
    draw_layer(sh, li, layer, new_len, sr);
    format!(
        "loop {} x{}: now {:.3} s ({} cycles of {:.3} s){} — {} layers playing.",
        li,
        n,
        new_len as f64 / sr as f64,
        n,
        loop_len as f64 / sr as f64,
        rounded,
        layer + 1
    )
}

/// The other multiply: keep the layer one bar long and give it room.
///
/// Ordinary multiply asks "how many bars of this?" and answers by repeating it.
/// This asks "how *often*?" and answers by leaving the rest silent. `s 2` on a
/// one-bar layer gives `B ~`; again gives `B ~ ~ ~`; again `B ~ ~ ~ ~ ~ ~ ~`.
/// Everything else keeps repeating underneath, so the loop grows without the
/// newest thing in it getting busier — which is the opposite of what a looper
/// usually does to you.
///
/// It takes no time. Ordinary multiply costs you n cycles of playing, because it
/// is recording; this is structural, so it lands on the next boundary and you
/// have not committed to anything you cannot take back with `d`.
///
/// **Growth is in whole multiples of the current cycle**, which is not an
/// arbitrary restriction: every layer's length divides the cycle, so a cycle
/// that is a multiple of the old one still divides evenly by all of them. Grow
/// by anything else and some other layer gets cut off mid-phrase at the wrap.
/// How often the newest layer sounds, **absolutely**, and nothing else.
///
/// Two things changed here on 2026-08-27, both because the surface grew a knob
/// for this and a knob holds a value rather than repeating a gesture.
///
/// **Absolute, not multiplicative.** `s4` used to mean *sound four times less
/// often than you already do*, so pressing it twice gave one in eight and there
/// was no way back except `d`. That is the right shape for a footswitch and the
/// wrong one for a knob: a knob asks "what should this be", and a control whose
/// meaning depends on where it has been cannot be read off the engine.
///
/// **And it no longer changes the loop's length.** It used to do both — set the
/// period *and* grow the loop by the same factor — which meant "how often does
/// this sound" and "how long is this loop" were one gesture and could not be
/// set independently. They are two knobs now: `len` says how many bars, this
/// says how often the material lands in them. A four-bar loop whose phrase
/// sounds every bar and a four-bar loop whose phrase sounds once are the same
/// length and different music, and neither was reachable before.
///
/// The way back is `d`, which is this with `n = 1`.
pub(crate) fn sparse(sh: &Shared, li: usize, _sr: u32, n: usize) -> String {
    let lp = sh.lp(li);
    let layers = lp.n_layers.load(Ordering::Acquire);
    if layers == 0 {
        return "nothing to spread — record a loop first.".into();
    }
    if n < 1 || n > MAX_PERIOD {
        return format!("`every` wants 1 to {}, not {}.", MAX_PERIOD, n);
    }
    let l = layers - 1;
    let (len, _, phase) = lp.layer_shape(l);
    if len == 0 {
        return "that layer has no length.".into();
    }
    lp.layers[l].period.store(n, Ordering::Release);
    // A phase that is now past the end would silence the layer outright, which
    // is not what asking for a different spacing means.
    lp.layers[l].phase.store(phase % n, Ordering::Release);
    if n == 1 {
        format!("layer {} sounds every time round.", l + 1)
    } else {
        format!(
            "layer {} sounds once every {}, on slot {}.",
            l + 1,
            n,
            (phase % n) + 1
        )
    }
}

/// Which slot of its period the newest layer lands on — the absolute form of
/// `o`, for the same reason `s` became absolute.
///
/// **Wraps rather than refusing.** The range depends on the period, and the app
/// deliberately does not make one knob's range depend on another's value: that
/// would make the pure position-to-value function need the snapshot. So any
/// slot is legal here and lands somewhere sensible, and turning past the end
/// comes round to the start, which is what a placement control should do
/// anyway.
pub(crate) fn place_at(sh: &Shared, li: usize, n: usize) -> String {
    let lp = sh.lp(li);
    let layers = lp.n_layers.load(Ordering::Acquire);
    if layers == 0 {
        return "nothing to place.".into();
    }
    let l = layers - 1;
    let (_, period, _) = lp.layer_shape(l);
    let slot = n % period.max(1);
    lp.layers[l].phase.store(slot, Ordering::Release);
    if period <= 1 {
        format!(
            "layer {} sounds every time round, so there is only one slot; \
             `{}s<n>` first to make room.",
            l + 1,
            li
        )
    } else {
        format!("layer {} is on slot {} of {}.", l + 1, slot + 1, period)
    }
}

/// **Size an empty loop in seconds**, so its first take closes itself.
///
/// The same state `set_bars` leaves an empty loop in — a length, zero layers,
/// `origin` at now — without the grid: no rounding to bars, no claim on the
/// anchor. The loop it makes is the one `r` already knows how to record into
/// (`want > 0` in the arm branch arms `close_at`), so nothing downstream is
/// new. On a loop with anything in it, the length is settled and this arms a
/// **one-pass overdub** instead: the next `r` starts on the press and
/// closes itself a loop length later — "record another one" for a module that
/// wants every layer the same length. Resizing material stays `len`'s job.
pub(crate) fn fix_next(sh: &Shared, li: usize, sr: u32, secs: f64) -> String {
    let lp = sh.lp(li);
    if lp.is_recording() || lp.is_armed() {
        return format!("loop {} is recording; finish that first.", li);
    }
    // With material in it the length is settled, and "fix the next take" can
    // only mean one thing: another layer of that length, one pass, closing
    // itself. The seconds asked for are noted if they differ, not obeyed —
    // layers of two lengths in one loop is a different instrument.
    if lp.n_layers.load(Ordering::Acquire) > 0 {
        let len = lp.loop_len.load(Ordering::Acquire);
        lp.next.plan_one_pass();
        let have = len as f64 / sr as f64;
        return format!(
            "loop {}'s next record adds one layer of {:.3} s{}.",
            li,
            have,
            if (have - secs).abs() > 0.01 {
                format!(" (not {:.1} s: every layer is the loop's length)", secs)
            } else {
                String::new()
            }
        );
    }
    let want = (secs * sr as f64).round() as usize;
    if want > sh.max_frames {
        return format!(
            "{:.1} s is past --max-secs; the longest take here is {:.1} s.",
            secs,
            sh.max_frames as f64 / sr as f64
        );
    }
    let now = sh.out_frames.load(Ordering::Acquire) as i64;
    lp.origin.store(now, Ordering::Release);
    lp.loop_len.store(want, Ordering::Release);
    lp.threaded.store(false, Ordering::Relaxed);
    format!(
        "loop {} is set to {:.3} s; record and it closes itself.",
        li,
        want as f64 / sr as f64
    )
}

/// **How many bars this loop is.** One verb, and which of its three jobs it is
/// doing depends on what the loop already is — said out loud in the ack every
/// time, because the difference is the whole of it.
///
/// * **Empty** — sizes it. The loop gets a length and no audio, and the next
///   recording closes itself there instead of waiting for a second press.
/// * **The anchor, with no clock** — *declares* it. The audio is untouched and
///   the pulse becomes a fraction of it, which is the only way a clockless
///   session gets a loop shorter than its first take. Resizing the thing that
///   defines the pulse would move everything that follows it, so it doesn't.
/// * **Anything else with material in it** — resizes it. The layers keep their
///   own lengths and wrap inside the new one, which is what `multiply_end` has
///   always done at the end of a multiply.
pub(crate) fn set_bars(sh: &Shared, li: usize, sr: u32, n: usize) -> String {
    let lp = sh.lp(li);
    if n < 1 || n > MAX_BARS {
        return format!("a loop wants 1 to {} bars, not {}.", MAX_BARS, n);
    }
    if lp.is_recording() {
        return format!("loop {} is recording; finish that first.", li);
    }
    let layers = lp.n_layers.load(Ordering::Acquire);
    let anchor = sh.anchor.load(Ordering::Acquire);
    let clocked = sh.link_bar_frames.load(Ordering::Relaxed) > 0;

    // Declaring: the audio stays exactly as it is and the number beside it
    // changes, which divides the pulse for everything that follows.
    if layers > 0 && li == anchor && !clocked {
        let len = lp.loop_len.load(Ordering::Acquire);
        lp.cycles.store(n, Ordering::Release);
        return format!(
            "loop {} is {} bar{} — the bar is now {:.3} s. Nothing was moved.",
            li,
            n,
            if n == 1 { "" } else { "s" },
            (len / n.max(1)) as f64 / sr as f64
        );
    }

    let Some((origin, bar)) = sh.grid() else {
        return format!(
            "no bar yet: there is no clock and no loop has a length. \
             Record something first, or start Link."
        );
    };
    let want = n * bar;
    if want > sh.max_frames {
        return format!(
            "{} bars would be {:.1} s, past the ceiling of {:.1} s.",
            n,
            want as f64 / sr as f64,
            sh.max_frames as f64 / sr as f64
        );
    }

    if layers == 0 {
        // Sized and empty: a length with nothing in it, which is a state the
        // engine did not have. A threaded tape is the neighbouring idea and is
        // not this one — that carries a silent layer so it can *play*, and it
        // would make the next recording an overdub. This stays at zero layers
        // so the next recording is a first recording, and closes itself.
        let now = sh.out_frames.load(Ordering::Acquire) as i64;
        let start = if lp.quant.load(Ordering::Relaxed) {
            let elapsed = now - origin;
            origin + (elapsed.div_euclid(bar as i64) + 1) * bar as i64
        } else {
            now
        };
        lp.origin.store(start, Ordering::Release);
        lp.loop_len.store(want, Ordering::Release);
        lp.cycles.store(n, Ordering::Release);
        sh.claim_anchor(li);
        return format!(
            "loop {} is set to {} bar{} ({:.3} s); record and it closes itself.",
            li,
            n,
            if n == 1 { "" } else { "s" },
            want as f64 / sr as f64
        );
    }

    // Resizing something with material in it. Growing is always safe; shrinking
    // below the longest layer would cut audio, and a length control that
    // silently trims is a length control you cannot use in a hurry.
    let longest = (0..layers).map(|l| lp.layer_shape(l).0).max().unwrap_or(0);
    if want < longest {
        return format!(
            "loop {} has a {:.3} s layer in it; {} bar{} would be {:.3} s. \
             Undo it or clear the loop first.",
            li,
            longest as f64 / sr as f64,
            n,
            if n == 1 { "" } else { "s" },
            want as f64 / sr as f64
        );
    }
    lp.loop_len.store(want, Ordering::Release);
    lp.cycles.store(n, Ordering::Release);
    format!(
        "loop {} is {} bar{} ({:.3} s); its layers keep their own lengths.",
        li,
        n,
        if n == 1 { "" } else { "s" },
        want as f64 / sr as f64
    )
}

/// **Take the session tempo from this loop.**
///
/// The other half of `set_bars`. That verb has three jobs — size an empty loop,
/// declare the bar count of a clockless anchor, resize something with material
/// in it — and *declaring* was reachable only with no clock, because with one
/// there was nothing to tell. There is now: link-spike answers
/// `/link/set-tempo`, and a tempo sent there reaches every peer on the session.
///
/// So this is declaring, with a clock. The loop says "I am `cycles` bars long
/// and `loop_len` frames", and those two numbers are a tempo.
///
/// ## Why this is not warping
///
/// **No audio moves.** `loop_len` is frames; loops play at frame rate and stay
/// phase-locked to each other whatever the bar is. What a bar length reaches is
/// the click, quantised launches and closes, `set_bars` arithmetic — and the
/// rest of the Link session. The principle is the one the whole bar model runs
/// on, at rig scale: *move the grid to the audio, never the audio to the grid.*
///
/// It also takes the tempo from the loop's **average** over its bars, not from
/// the timing within them. Play four bars a little long and the click comes to
/// you; play them unevenly and they stay uneven. That is the floor-looper
/// behaviour and it is the point.
///
/// ## What it costs when other loops exist
///
/// Nothing to them — they are frames and do not move, and they stay in
/// relation to each other. What moves is the click and everything downstream of
/// Link, so loops recorded against the old click are now out with the click and
/// still in with each other. Sometimes that is exactly the intent and sometimes
/// it is a disaster, so the ack counts them and says so rather than deciding.
pub(crate) fn take_tempo(sh: &Shared, li: usize, sr: u32) -> String {
    let lp = sh.lp(li);
    if lp.is_recording() || lp.is_armed() {
        return format!("loop {} is still being written; finish that first.", li);
    }
    let len = lp.loop_len.load(Ordering::Acquire);
    if len == 0 {
        return format!(
            "loop {} has no length, so there is no tempo in it. Record it, or \
             `{}len<n>` first.",
            li, li
        );
    }
    // **A loop nobody has counted may not set the tempo**, and this guard was
    // bought at the cost of putting the whole rig on 29.56 bpm.
    //
    // `cycles` is zero for "nobody has said" and reads as one everywhere else,
    // which is harmless where a wrong count means a wrong ring. Here it meant an
    // eight-second take offering 29.56 bpm — inside Link's 20..999, so the
    // range check passed, so it went out to Ableton and the modular. A
    // plausible wrong answer is the failure mode this whole rig is built to
    // avoid, and the range check cannot catch it: for a four-bar take at any
    // ordinary tempo, one quarter of the truth is still an ordinary tempo.
    //
    // With a clock `commit` now counts the bars of every take, so this only
    // ever refuses a loop that genuinely has no count — which is the clockless
    // case, where there is no session to tell anyway.
    let bars = lp.cycles.load(Ordering::Acquire);
    if bars == 0 {
        return format!(
            "nobody has said how many bars loop {} is, and a tempo taken from a \
             guess would be wrong by exactly that guess. `{}len<n>` first.",
            li, li
        );
    }
    let secs = len as f64 / sr as f64;
    let quantum = f64::from_bits(sh.link_quantum.load(Ordering::Relaxed));
    let bpm = tempo_of(len, bars, sr, quantum);

    // **Refused rather than clamped.** link-spike clamps to Link's documented
    // 20..999, and a clamp here would be a lie: a tempo outside that range does
    // not mean the loop is strange, it means the bar count is wrong — four bars
    // read as one, or a two-second loop declared as thirty-two. Saying which
    // number to look at is worth more than a tempo nobody asked for.
    if !(20.0..=999.0).contains(&bpm) {
        return format!(
            "loop {} is {:.3} s over {} bar{}, which is {:.1} bpm — outside 20 to 999. \
             The bar count is the number to look at.",
            li,
            secs,
            bars,
            if bars == 1 { "" } else { "s" },
            bpm
        );
    }

    if let Err(e) = crate::link::set_tempo(bpm, crate::link::DEFAULT_TEMPO_PORT) {
        return format!("could not set the tempo: {}", e);
    }

    // Everything that would now disagree with the click, counted. Not a
    // refusal — re-deciding the tempo around the loop that came out well is a
    // real move — but it is never something to discover afterwards.
    let others = (0..sh.n_loops)
        .filter(|&o| o != li && sh.lp(o).n_layers.load(Ordering::Acquire) > 0)
        .count();
    let heard = sh.link_anchors.load(Ordering::Relaxed) > 0;

    format!(
        "tempo taken from loop {}: {:.3} s over {} bar{} is {:.2} bpm.{}{}",
        li,
        secs,
        bars,
        if bars == 1 { "" } else { "s" },
        bpm,
        if others > 0 {
            format!(
                " {} other loop{} keep their audio but no longer agree with the click.",
                others,
                if others == 1 { " does and it" } else { "s do and they" }
            )
        } else {
            String::new()
        },
        if heard {
            ""
        } else {
            " No anchor has ever arrived, so nothing here can confirm link-spike took it."
        }
    )
}

/// The tempo a loop implies: its bars over its seconds, in beats.
///
/// Split out so it can be tested without a socket or a `Shared` — the rest of
/// `take_tempo` is guards and a UDP send, and this is the only part that can be
/// arithmetically wrong.
///
/// Beats to the bar come from Link where it is known and are four where it is
/// not, which is the same assumption `launch_grid` makes: a quantum of zero
/// means "nobody has said", not "no beats".
pub(crate) fn tempo_of(len: usize, bars: usize, sr: u32, quantum: f64) -> f64 {
    let secs = len as f64 / sr as f64;
    let beats_per_bar = if quantum >= 1.0 { quantum } else { 4.0 };
    60.0 * beats_per_bar * bars.max(1) as f64 / secs
}

/// Every loop that holds something, from the top, together.
///
/// **Not eight unmutes.** `h1` restores audibility and leaves each loop wherever
/// its own phase had got to, so a set of a four-bar, a three-bar and a one-bar
/// loop came back in whatever relationship they happened to be in — and since
/// the lengths differ, "where they happened to be" is not a musical fact about
/// anything. Starting the set means *starting* it, which means one origin for
/// all of them.
///
/// It reuses the request the fire switch sends, because `FIRE` already is this:
/// stamp the origin, put the playhead at the top (at the end, going backwards),
/// unmute. The only thing it adds is a `shot_end`, and that is read only
/// through `firing()`, which tests `one_shot` first — so on a loop that is not
/// a one-shot it is a number nothing consults.
///
/// **One deadline for all of them, computed once here.** That is the whole
/// point: eight loops each asking `next_boundary` at eight slightly different
/// moments is eight answers, and the set would land ragged in exactly the way
/// this exists to prevent. It is `next_boundary` rather than the bar outright,
/// so Start All lands on whatever `launch quantise` is already set to and does
/// not become a second opinion about when a launch happens.
pub(crate) fn start_all(sh: &Shared, sr: u32) -> String {
    let now = sh.out_frames.load(Ordering::Acquire) as i64;
    let at = sh.next_boundary(now);
    let mut n = 0usize;
    let mut busy = 0usize;
    for li in 0..sh.n_loops {
        let lp = sh.lp(li);
        if lp.loop_len.load(Ordering::Acquire) == 0 {
            continue;
        }
        // A take in progress is not part of the set yet, and restarting the loop
        // being written into would move the origin out from under the recording.
        if lp.is_recording() || lp.is_armed() {
            busy += 1;
            continue;
        }
        lp.next.set(FIRE, at.unwrap_or(i64::MIN));
        n += 1;
    }
    if n == 0 {
        return if busy > 0 {
            "nothing to start — everything with audio in it is still recording.".into()
        } else {
            "nothing to start — no loop has anything in it.".into()
        };
    }
    format!(
        "{} loop{} start from the top together{}.{}",
        n,
        if n == 1 { "" } else { "s" },
        match at {
            Some(t) => format!(" on the grid in {:.2} s", (t - now).max(0) as f64 / sr as f64),
            None => String::new(),
        },
        if busy > 0 {
            format!(" {} still recording, left alone.", busy)
        } else {
            String::new()
        }
    )
}

pub(crate) fn rotate(sh: &Shared, li: usize) -> String {
    let lp = sh.lp(li);
    let layers = lp.n_layers.load(Ordering::Acquire);
    if layers == 0 {
        return "nothing to move.".into();
    }
    let l = layers - 1;
    let (_, period, phase) = lp.layer_shape(l);
    if period <= 1 {
        return "that layer sounds every time round — spread it first.".into();
    }
    let next = (phase + 1) % period;
    lp.layers[l].phase.store(next, Ordering::Release);
    format!("layer {} moved to slot {} of {}.", l + 1, next + 1, period)
}

/// Put the newest layer back to sounding every time round.
///
/// The loop keeps the length it grew to, because that length is now shared with
/// everything else that was recorded against it. Shrinking it would be a
/// different and much less reversible operation.
pub(crate) fn dense(sh: &Shared, li: usize) -> String {
    let lp = sh.lp(li);
    let layers = lp.n_layers.load(Ordering::Acquire);
    if layers == 0 {
        return "nothing to fill.".into();
    }
    let l = layers - 1;
    lp.layers[l].period.store(1, Ordering::Release);
    lp.layers[l].phase.store(0, Ordering::Release);
    format!("layer {} sounds every time round again.", l + 1)
}

/// Forget the loop's length, so the next recording lays down a new grid.
///
/// Undo removes a layer and deliberately keeps the length: erasing a first take
/// while holding onto the tempo you found is worth having, and the click goes on
/// running at it so the next attempt lands on the same grid. But without a way to
/// let go of it, undoing everything left the engine "stuck" at a length with
/// nothing in it — the transport still running, the record button still offering
/// an overdub, and no route back to an open-ended first recording short of `c`.
///
/// So the three erasures are distinct, and worth keeping distinct: `u` drops a
/// layer, this drops the grid, `c` drops both.
///
/// Refused while layers exist, because the length is what they are addressed by.
/// Clearing it under them would leave a mix reading positions in a cycle that no
/// longer has a size.
pub(crate) fn free_length(sh: &Shared, li: usize, sr: u32) -> String {
    let lp = sh.lp(li);
    let n = lp.n_layers.load(Ordering::Acquire);
    if n > 0 {
        return format!(
            "{} layer{} still playing — undo or clear them first; the length is what they sit in.",
            n,
            if n == 1 { "" } else { "s" }
        );
    }
    let was = lp.loop_len.load(Ordering::Acquire);
    if was == 0 {
        return "no length set — the next recording will set one.".into();
    }
    lp.loop_len.store(0, Ordering::Release);
    lp.reached.store(0, Ordering::Release);
    lp.state.set(IDLE);
    // A take planned for the length just forgotten is no longer the take
    // that was planned; the ack promises the next recording sets a length,
    // and one still waiting for the grid would have recorded open-ended.
    lp.next.clear();
    sh.release_anchor(li);
    format!(
        "length forgotten (was {:.3} s). The next recording sets a new one.",
        was as f64 / sr as f64
    )
}
