//! The plan for the next recording: `NextTake`.
//!
//! Made on 2026-09-06 (REVIEW-daemon-debt step 4) from fields that lived
//! side by side on `Loop` and had to be set and cleared together, and were
//! not. Each field keeps the atomic type and the orderings it had on `Loop`,
//! so the audio thread reads exactly what it read before; what changed is
//! that there is one place to set the plan, one place to consume it, and one
//! place to abandon it.
//!
//! # What is the plan, and what is not
//!
//! The rule: a field belongs here iff it is written *before* a take starts
//! and consumed at the moment the output callback turns the request into
//! `FIRST` or `OVERDUB`. Everything that was suspected is classified below,
//! from the code rather than from the review's list.
//!
//! | field          | is                                                       |
//! |----------------|----------------------------------------------------------|
//! | `request`      | **plan** — the phase asked for; set by `r`, `f`, Start All and the level crossing, consumed once by the callback |
//! | `request_at`   | **plan** — the output frame the request takes effect on, or `i64::MIN` for the next buffer; born and consumed with `request` |
//! | `one_pass`     | **plan** — set by `fix` on a loop with material, spent when the overdub is stamped; nothing reads it after |
//! | `arm_from`     | **plan** — the back-date frame the input callback found at the threshold crossing, swapped out when the take is stamped |
//! | `close_at`     | running take — written *at* the transition (or by the closer's own compare-exchange), read by the closer while the take runs |
//! | `rec_len`      | running take — the declared length, written at the transition and taken by `commit` when the take closes |
//! | `started_late` | running take — how late the press was; written on `r` but spent by `commit`, not by the transition, and overwritten by every `r` |
//! | `reached`, `rec_reached`, `rec_from` | running take — how far it got and where its zero is |
//! | `level_arm`    | mode — survives every take; says how `r` behaves, not what the next take is |
//! | `quant`        | mode — the same; the plan reads it to choose a frame and stores the frame, not the mode |
//! | `threaded`     | content fact — "this one layer is an empty tape"; read by `blank` and `copy` as a fact about what is in the loop, and cleared by the callback because recording makes it false, not because it was consumed |
//! | `loop_len` on zero layers | content fact — the loop's length, read by the mixer every frame; the plan reads it at the transition (`want`) and does not own it |
//!
//! `request` also carries `FIRE` and `PLAYING`, which are transitions but
//! not takes. They share the slot because the callback stamps them the same
//! way; `take` hands them over the same way and leaves the take's own
//! qualifiers (`one_pass`, `arm_from`) standing, because a Start All going
//! past does not cancel a `fix`.
//!
//! # The three places
//!
//! - **set**: `set` (a phase and its frame), `set_from` (the level
//!   crossing's `ARMED` with its back-date), `listen` (an arm that waits for
//!   a sound: no frame and no back-date until the crossing supplies them),
//!   `plan_one_pass` (`fix` on a loop with material). All on the control
//!   thread, except `set_from`, which the input callback calls.
//! - **take**: `take`, once per due request, in the output callback.
//! - **clear**: `clear`, from `Loop::cleared`, a cancelled arm (`r` on an
//!   `ARMED` loop, `lev0` under one), `free_length`, and the device-loss
//!   path, which drops every loop's plan.

use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};

use super::{ARMED, AtomicU8Wrapper};

/// The plan for the next recording, and the pending transition that starts
/// it. Four atomics, read lock-free by the callback.
pub(crate) struct NextTake {
    /// Set by the control thread, consumed by the output callback, which is the
    /// only place a transition can be stamped to an exact frame.
    request: AtomicU8Wrapper,
    /// The output frame the pending request should take effect on, or
    /// `i64::MIN` for "the next buffer", which is what every request used to be.
    ///
    /// This is what makes a loop start *on* a boundary rather than within a
    /// buffer of one. Sleeping on the control thread until the boundary and
    /// then setting the request would still land at the start of whichever
    /// buffer came next — up to a full buffer late, and a buffer is 21 ms at
    /// 1024 frames, which is an audible flam against a loop already playing.
    /// The callback is the only thread that knows the frame, so the frame is
    /// what it is told.
    request_at: AtomicI64,
    /// **The next overdub is one pass**: it starts at the loop's own zero and
    /// closes itself a loop length later, as a layer the length of the loop.
    /// Set by `fix` on a loop with material and spent when the overdub starts,
    /// so a stale one cannot outlive the press it was sent with by more than
    /// a moment. What a module that wants every layer the same length means
    /// by "record another one".
    one_pass: AtomicBool,
    /// The output frame a pending recording should be back-dated to, or
    /// `i64::MIN` for none.
    ///
    /// Written by the input callback at the threshold crossing, read by the
    /// output callback when it stamps the recording. The two cannot be the same
    /// frame — the crossing is found on the input thread and the transition is
    /// stamped on the output one — so the difference is handed to `started_late`
    /// and spent as pre-roll, which is the machinery a late footswitch already
    /// built.
    arm_from: AtomicI64,
}

/// What the callback takes: every field of the plan, read once.
pub(crate) struct Taken {
    /// The phase asked for: `ARMED`, `FIRE` or `PLAYING`.
    pub(crate) phase: u8,
    /// The frame it was asked for, or `i64::MIN` for "now".
    pub(crate) at: i64,
    /// Whether the overdub this starts closes itself one loop length on.
    pub(crate) one_pass: bool,
    /// The frame the take is back-dated to, or `i64::MIN` for none.
    pub(crate) arm_from: i64,
}

impl NextTake {
    pub(crate) fn new() -> Self {
        NextTake {
            request: AtomicU8Wrapper::new(0),
            request_at: AtomicI64::new(i64::MIN),
            one_pass: AtomicBool::new(false),
            arm_from: AtomicI64::new(i64::MIN),
        }
    }

    /// Ask for `phase` at output frame `at`, or on the next buffer for
    /// `i64::MIN`. The frame is stored before the request, so the callback
    /// that sees the request sees its frame.
    pub(crate) fn set(&self, phase: u8, at: i64) {
        self.request_at.store(at, Ordering::Release);
        self.request.set(phase);
    }

    /// The level crossing's arm: `ARMED` at `at` (or the next buffer), the
    /// take back-dated to `from` (or not, for `i64::MIN`). Written on the
    /// input thread.
    pub(crate) fn set_from(&self, at: i64, from: i64) {
        self.arm_from.store(from, Ordering::Release);
        self.request_at.store(at, Ordering::Release);
        self.request.set(ARMED);
    }

    /// Wait for a sound: no frame and no back-date yet. The input callback
    /// supplies both at the crossing, through `set_from`.
    pub(crate) fn listen(&self) {
        self.arm_from.store(i64::MIN, Ordering::Release);
        self.request_at.store(i64::MIN, Ordering::Release);
    }

    /// The next overdub closes itself one loop length on.
    pub(crate) fn plan_one_pass(&self) {
        self.one_pass.store(true, Ordering::Relaxed);
    }

    /// Whether the next overdub is planned as one pass.
    pub(crate) fn is_one_pass(&self) -> bool {
        self.one_pass.load(Ordering::Relaxed)
    }

    /// Whether a transition is waiting for the callback.
    pub(crate) fn is_pending(&self) -> bool {
        self.request.get() != 0
    }

    /// Whether there is no plan at all: nothing pending, no frame, no
    /// back-date, no one-pass. What `clear` leaves.
    #[cfg(test)]
    pub(crate) fn is_empty(&self) -> bool {
        !self.is_pending()
            && self.request_at.load(Ordering::Acquire) == i64::MIN
            && !self.is_one_pass()
            && self.arm_from.load(Ordering::Acquire) == i64::MIN
    }

    /// Frames until the pending transition fires, or `-1` when nothing is
    /// pending or it has no deadline.
    pub(crate) fn due_in(&self, now: i64) -> i64 {
        if self.request.get() == 0 {
            return -1;
        }
        match self.request_at.load(Ordering::Acquire) {
            i64::MIN => -1,
            at => (at - now).max(0),
        }
    }

    /// Consume the pending transition, if there is one and it is due before
    /// output frame `before`.
    ///
    /// Peek, not take, until it is due: a request with a deadline in the
    /// future has to survive this buffer and be reconsidered on the next.
    /// Consuming first and re-arming would lose it if the control thread
    /// never looked again. Due if it has no deadline, or its deadline falls
    /// before `before` — a deadline in the past means the control thread
    /// was late, and being late is not a reason to wait a whole cycle more.
    ///
    /// A take (`ARMED`) takes its qualifiers with it; a `FIRE` or `PLAYING`
    /// leaves them, because it is not the take they describe.
    pub(crate) fn take(&self, before: i64) -> Option<Taken> {
        let phase = self.request.get();
        if phase == 0 {
            return None;
        }
        let at = self.request_at.load(Ordering::Acquire);
        if at != i64::MIN && at >= before {
            return None;
        }
        self.request.set(0);
        self.request_at.store(i64::MIN, Ordering::Release);
        let (one_pass, arm_from) = if phase == ARMED {
            (
                self.one_pass.swap(false, Ordering::AcqRel),
                self.arm_from.swap(i64::MIN, Ordering::AcqRel),
            )
        } else {
            (false, i64::MIN)
        };
        Some(Taken { phase, at, one_pass, arm_from })
    }

    /// Abandon the plan: nothing pending, no frame, no back-date, no
    /// one-pass. The one place any of these goes back to empty outside
    /// `take`.
    pub(crate) fn clear(&self) {
        self.request.set(0);
        self.request_at.store(i64::MIN, Ordering::Release);
        self.one_pass.store(false, Ordering::Relaxed);
        self.arm_from.store(i64::MIN, Ordering::Release);
    }
}
