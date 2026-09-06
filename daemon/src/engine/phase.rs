//! The loop's phase: `Phase`, the pairs a loop may move between, and the
//! words the wire spells them with.
//!
//! Made on 2026-09-06 (REVIEW-daemon-debt step 5a) from the six `u8`
//! constants that used to live in `mod.rs` and were stored at seventeen
//! sites from three threads. The byte is the same byte — `Phase` is
//! `repr(u8)` with the values the constants had, so nothing on the wire or
//! in a running daemon moves — and the storage is still one `AtomicU8` on
//! `Loop`, read with the same load the audio thread always did. What is new
//! is that **one function stores it**: `Loop::enter`, which reads the phase
//! it is leaving, checks the pair against `LEGAL`, and stores. The frame the
//! transition belongs to is *passed* to it, so the output callback stays the
//! only thing that stamps frames and the control thread keeps reading
//! `out_frames` at the moment it acts, exactly as before.
//!
//! # The table
//!
//! `LEGAL` is derived from the code as it stood, not from the design: every
//! pair one of the seventeen sites could produce on the path it was written
//! for, one comment per pair naming the site. Two kinds of pair are
//! deliberately *not* in it, and both log rather than change anything:
//!
//! - pairs only a race between two threads can produce — `x` starting a
//!   multiply while a grid request is still pending (`Multiply → Overdub`),
//!   or a `c` landing inside `commit`'s drain sleep (`Idle → Playing` from
//!   `commit`'s second store);
//! - self-transitions nothing produces (`Armed → Armed`, `First → First`,
//!   `Overdub → Overdub`, `Multiply → Multiply`).
//!
//! `Idle → Idle` and `Playing → Playing` *are* produced — clearing an idle
//! loop, forgetting a sized-and-empty loop's length, re-threading a tape,
//! `commit`'s revox branch storing `Playing` a second time — and are legal.
//!
//! Six pairs used to be in the table only because a verb was not guarded
//! against the phase it found: `x`, `z`, `blank` and `t` on an `Armed`
//! loop, `z` and `t` on a `First` one. The Glassbox artifact
//! (`purescript-glassbox/core/machines/itajara-loop.json`) refuses every
//! one of them, and since step 5b (2026-09-06) so does the daemon —
//! `guards::still_recording` — so `Armed → Multiply` is gone and the other
//! five pairs are produced by their honest sites alone. The same step made
//! a cancelled arm return to what the loop held (`Loop::disarm`), which is
//! where `Armed → Playing` comes from now. `engine/conformance.rs` replays
//! the artifact's table through the engine to hold the two together.
//!
//! An illegal pair is not an error in release: `enter` logs it once per
//! pair and performs the store anyway, so the daemon behaves exactly as it
//! did while the log says what the table missed. Under `cfg(test)` an
//! illegal pair panics, which is what makes the table a test.

use std::fmt;
#[cfg(not(test))]
use std::sync::atomic::{AtomicU64, Ordering};

/// Transport states. `repr(u8)` with the values the old constants had,
/// because the audio thread reads the byte every buffer and the byte is
/// what the tests' snapshot hash and any running daemon hold.
#[repr(u8)]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Phase {
    Idle = 0,
    /// Waiting for the output callback to stamp the exact frame recording begins.
    ///
    /// Also, and for a long time only nominally, the state a **level-armed** loop
    /// sits in while it listens. `ARMED` was written as a request value and never
    /// once set as a state — `is_armed()` could not return true, and the `armed`
    /// field has been going out in every snapshot reading `false` since the socket
    /// existed. Level-arm is what it was always describing: the loop has claimed
    /// the input and is not yet writing to it.
    Armed = 1,
    /// Recording the first loop: linear, and its length becomes the cycle.
    First = 2,
    /// Recording an overdub: modular, into a buffer one cycle long.
    Overdub = 3,
    /// Playing, not recording.
    Playing = 4,
    /// Recording across several cycles, to make the loop an integer multiple longer
    /// with what is already there repeating underneath. The EDP's `Multiply`.
    Multiply = 5,
}

impl Phase {
    /// Every phase, in byte order.
    #[cfg(test)]
    pub(crate) const ALL: [Phase; 6] = [
        Phase::Idle,
        Phase::Armed,
        Phase::First,
        Phase::Overdub,
        Phase::Playing,
        Phase::Multiply,
    ];

    /// The phase a stored byte means. Anything outside the six is `Idle`,
    /// which is what every reader of the byte fell through to before.
    pub(crate) fn from_u8(v: u8) -> Phase {
        match v {
            1 => Phase::Armed,
            2 => Phase::First,
            3 => Phase::Overdub,
            4 => Phase::Playing,
            5 => Phase::Multiply,
            _ => Phase::Idle,
        }
    }

    /// The byte the phase is stored as.
    pub(crate) fn as_u8(self) -> u8 {
        self as u8
    }

    /// The word the wire spells this phase with. One spelling: the snapshot's
    /// `"state"` and `busy_elsewhere`'s ack both come through here.
    pub(crate) fn wire_word(self) -> &'static str {
        match self {
            Phase::Idle => "idle",
            Phase::Armed => "armed",
            Phase::First => "recordingFirst",
            Phase::Overdub => "overdubbing",
            Phase::Playing => "playing",
            Phase::Multiply => "multiplying",
        }
    }
}

impl fmt::Display for Phase {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.wire_word())
    }
}

/// Every `(from, to)` pair a store site produces, with the site that
/// produces it — 21 rows since step 5b, 22 before. See the module comment
/// for what is left out and why.
pub(crate) const LEGAL: &[(Phase, Phase)] = &[
    // -- from Idle --------------------------------------------------------
    // `Loop::cleared` (`c`) on a loop that is already idle; `free_length`
    // (`z`) on a sized-and-empty loop, which is idle with a length.
    (Phase::Idle, Phase::Idle),
    // `dispatch` `r` with level-arm on, on an empty or sized loop.
    (Phase::Idle, Phase::Armed),
    // output callback: an `ARMED` request on a loop with no layers (`r` on
    // an empty or sized loop, now or on the bar line).
    (Phase::Idle, Phase::First),
    // `take` (`t`) with no length; `copy_layers` (`cp`) onto an empty loop;
    // `thread_blank` (`blank`, `c` on a fixed rig, `--fixed-secs` at start).
    (Phase::Idle, Phase::Playing),
    // `multiply_start` (`x`) on a sized-and-empty loop.
    (Phase::Idle, Phase::Multiply),
    // -- from Armed -------------------------------------------------------
    // `disarm` — `r` taking the arm back, `lev0` under one — on a loop with
    // no layers; `cleared` (`c`).
    (Phase::Armed, Phase::Idle),
    // output callback: the level crossing's request on a loop with no layers.
    (Phase::Armed, Phase::First),
    // output callback: the level crossing's request on a loop with layers.
    (Phase::Armed, Phase::Overdub),
    // `disarm` on a loop with layers, a threaded tape included: it was
    // playing before it listened, and is again.
    (Phase::Armed, Phase::Playing),
    // -- from First -------------------------------------------------------
    // `cleared` (`c`); `supervise` on a lost device, no length yet.
    (Phase::First, Phase::Idle),
    // `commit` (`r`, or the closer); `supervise` on a lost device with a
    // length.
    (Phase::First, Phase::Playing),
    // -- from Overdub -----------------------------------------------------
    // `cleared` (`c`).
    (Phase::Overdub, Phase::Idle),
    // `commit` (`r`, or the closer for a one-pass); `supervise` on a lost
    // device.
    (Phase::Overdub, Phase::Playing),
    // -- from Playing -----------------------------------------------------
    // `cleared` (`c`); `free_length` (`z`) after every layer was undone,
    // which leaves the loop playing with a length and no layers.
    (Phase::Playing, Phase::Idle),
    // `dispatch` `r` with level-arm on, on a loop with layers.
    (Phase::Playing, Phase::Armed),
    // output callback: an `ARMED` request on a loop whose layers were all
    // undone — length kept, so it is a first take again.
    (Phase::Playing, Phase::First),
    // output callback: an `ARMED` request on a loop with layers, a threaded
    // tape included.
    (Phase::Playing, Phase::Overdub),
    // `commit`'s revox branch, storing `Playing` again after its drain;
    // `copy_layers` onto a threaded tape; `thread_blank` re-threading a
    // tape; the output callback's `PLAYING` request, which nothing sends
    // since Start All became `FIRE`.
    (Phase::Playing, Phase::Playing),
    // `multiply_start` (`x`).
    (Phase::Playing, Phase::Multiply),
    // -- from Multiply ----------------------------------------------------
    // `cleared` (`c`).
    (Phase::Multiply, Phase::Idle),
    // `multiply_end` (`x` or `r`), by either exit; `supervise` on a lost
    // device.
    (Phase::Multiply, Phase::Playing),
];

/// Whether `from → to` is in the table.
pub(crate) fn legal(from: Phase, to: Phase) -> bool {
    LEGAL.iter().any(|&(f, t)| f == from && t == to)
}

/// One bit per `(from, to)` pair: which illegal pairs have been logged.
/// Six by six is thirty-six bits, and `fetch_or` is the whole of the
/// bookkeeping, so the once-only holds across threads without a lock.
#[cfg(not(test))]
static LOGGED: AtomicU64 = AtomicU64::new(0);

/// Report an illegal pair, once per pair for the life of the process.
///
/// Off the audio thread this is a line on stderr; on it, the same — a
/// pair the table missed is worth a print it would never otherwise get,
/// and it happens at most thirty-six times ever.
#[cfg(not(test))]
pub(crate) fn note_illegal(index: usize, from: Phase, to: Phase, at: i64) {
    let bit = 1u64 << (from.as_u8() as u32 * 6 + to.as_u8() as u32);
    if LOGGED.fetch_or(bit, Ordering::Relaxed) & bit == 0 {
        eprintln!(
            "loop {}: phase {} -> {} at frame {} is not in the table (stored anyway; tell the table)",
            index, from, to, at
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The byte does not move: the six values are the constants' values, in
    /// their order, and every byte round-trips.
    #[test]
    fn the_byte_is_the_old_byte() {
        assert_eq!(Phase::ALL.map(|p| p.as_u8()), [0, 1, 2, 3, 4, 5]);
        for p in Phase::ALL {
            assert_eq!(Phase::from_u8(p.as_u8()), p);
        }
        assert_eq!(Phase::from_u8(6), Phase::Idle, "a request value falls through to idle");
        assert_eq!(Phase::from_u8(255), Phase::Idle);
    }

    /// The wire's words, exactly as `state_name` spelled them.
    #[test]
    fn the_wire_words_are_unchanged() {
        assert_eq!(
            Phase::ALL.map(|p| p.to_string()),
            ["idle", "armed", "recordingFirst", "overdubbing", "playing", "multiplying"]
        );
    }

    /// The table has no duplicate rows and no self-transition it did not
    /// mean: only `Idle` and `Playing` re-enter themselves.
    #[test]
    fn the_table_is_a_set_and_names_its_self_transitions() {
        for (i, a) in LEGAL.iter().enumerate() {
            for b in &LEGAL[i + 1..] {
                assert_ne!(a, b, "duplicate row");
            }
        }
        for p in Phase::ALL {
            assert_eq!(
                legal(p, p),
                matches!(p, Phase::Idle | Phase::Playing),
                "self-transition {} -> {}",
                p,
                p
            );
        }
    }
}
