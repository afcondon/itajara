//! The refusals a verb can meet before it acts.
//!
//! Split out of `engine.rs` on 2026-09-06 (REVIEW-daemon-debt step 1).

use std::sync::atomic::Ordering;

use super::loop_state::Loop;
use super::shared::Shared;

/// Refuse a verb that would change a loop while a take is on the way or under
/// way: listening for a sound, waiting for the bar, or writing.
///
/// Added 2026-09-06 (REVIEW-daemon-debt step 5b), where the Glassbox artifact
/// refuses `still-recording` and the daemon had no guard: `u`, `x`, `z`,
/// `blank`, `len` and `t` on an armed loop, `u`, `z` and `t` on a recording
/// one, and all of them on a loop waiting for the bar. A press there
/// decremented a layer under a live write, forgot a length mid-take, or
/// abandoned an arm to start a multiply. The three sentences say which wait
/// it is; every one ends the way `fix`'s refusal always has.
pub(crate) fn still_recording(lp: &Loop, li: usize) -> Option<String> {
    if lp.next.waits_for_boundary() {
        return Some(format!("loop {} is waiting for the bar; finish that first.", li));
    }
    if lp.is_armed() {
        return Some(format!("loop {} is listening for a sound; finish that first.", li));
    }
    if lp.is_recording() {
        return Some(format!("loop {} is recording; finish that first.", li));
    }
    None
}

/// Refuse a claim on the input when another loop already has it.
///
/// There is one converter, so only one loop can record at a time. Without this
/// the second loop would go to `FIRST` quite happily and then capture nothing,
/// because the input callback asks `recording_loop()` and gets the first match —
/// a loop that says it is recording, shows as recording, and is writing to no
/// buffer. Refusing out loud is the whole difference between a rule and a bug.
pub(crate) fn busy_elsewhere(sh: &Shared, li: usize) -> Option<String> {
    match sh.input_claimed() {
        Some(other) if other != li => Some(format!(
            "loop {} has the input ({}). One converter, one recording — finish that first.",
            other,
            sh.lp(other).state_name()
        )),
        _ => None,
    }
}

/// Why a loop at a speed cannot be recorded into.
///
/// Named rather than worked around. The input arrives at rate one and the loop's
/// grid is moving under it, so there is no honest place to put the samples —
/// and the answer that would look like it worked (resample the input, or quietly
/// snap back to rate one) is the answer this project keeps refusing.
/// Whether this loop can be written into *now*, which is a narrower question
/// than whether it is playing plainly.
///
/// **Speed and direction stopped being refusals on 2026-08-30.** The write head
/// follows the play head now (see the input callback), so a loop running
/// backwards or at half speed takes an overdub and gives back what you played.
/// That was the whole of the old refusal and it is gone.
///
/// What is left is two things the span-write cannot answer for:
///
///   - **A pendulum** reflects rather than wrapping, so `raw_pos` is not the
///     position — the fold happens after it. A write head reading raw would run
///     off the end and come back through the audio it just laid down.
///   - **A tape at speed.** Revox reads, filters and writes one slot per frame;
///     it is a physical model of a head passing over oxide, and a head that
///     covers two slots or half of one is a different machine. Threading a tape
///     is a deliberate act, so being told to put the speed back is fair.
///
/// And a *first* take still wants unity, because an empty loop has no play head
/// to follow: the linear write is all there is, and the speed it would be
/// played back at is not a thing the recording can compensate for.
pub(crate) fn not_writable(lp: &Loop, li: usize) -> Option<String> {
    if lp.pendulum.load(Ordering::Relaxed) {
        return Some(format!(
            "loop {} is swinging, and a write head cannot follow a playhead that \
             turns round mid-pass; `{}pend0` to record into it.",
            li, li
        ));
    }
    if lp.revox.load(Ordering::Relaxed) && !lp.plain() {
        return Some(format!(
            "loop {} is a tape running at x{}; a tape head passes over the oxide \
             once per frame, so put the speed back with `{}sp1` to record onto it.",
            li,
            lp.speed().abs(),
            li
        ));
    }
    if lp.loop_len.load(Ordering::Acquire) == 0 {
        return not_plain(lp, li);
    }
    None
}

pub(crate) fn not_plain(lp: &Loop, li: usize) -> Option<String> {
    if lp.plain() {
        return None;
    }
    Some(format!(
        "loop {} is playing at x{}{}; `{}sp1` to record into it.",
        li,
        lp.speed().abs(),
        if lp.pendulum.load(Ordering::Relaxed) {
            ", swinging"
        } else if lp.speed() < 0.0 {
            ", backwards"
        } else {
            ""
        },
        li
    ))
}
