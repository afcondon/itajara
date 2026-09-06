//! One loop: `Loop`, its fields and its own arithmetic.
//!
//! Split out of `engine.rs` on 2026-09-06 (REVIEW-daemon-debt step 1). The
//! file is `loop_state` because `loop` is a keyword.

use std::sync::atomic::{
    AtomicBool,
    AtomicI64,
    AtomicU32,
    AtomicU64,
    AtomicU8,
    AtomicUsize,
    Ordering,
};
use super::{
    AtomicU8Wrapper,
    ENV_BUCKETS,
    Layer,
    NextTake,
    Phase,
    Shape,
    to_byte,
};
use super::phase::legal;
#[cfg(not(test))]
use super::phase::note_illegal;

/// One loop: its layers, its cycle, and where it stands in it.
///
/// Split out of `Shared` when the engine went from one loop to six. What lives
/// here is what a loop can have an opinion of its own about; what stays on
/// `Shared` is what belongs to the rig — the single input's pre-roll, the frame
/// counters, the latency calibration, the clock. The division is not stylistic:
/// there is one audio device, so there is one K and one ring no matter how many
/// loops there are, and duplicating those per loop would be six chances to
/// disagree about what time it is.
pub struct Loop {
    pub loop_len: AtomicUsize,
    /// **How many bars this loop is**, and the only place metre enters a loop.
    ///
    /// `loop_len` is frames and always has been; this says what those frames
    /// mean. `loop_len == cycles * bar` is the invariant everything else leans
    /// on, and it is what lets one loop be four bars while another is one
    /// without either of them being "the grid".
    ///
    /// One thing it does that nothing else can: on the **anchor**, with no
    /// clock, it divides the pulse. Record a phrase, say it was four bars, and
    /// the bar becomes a quarter of it — which is the only way a clockless
    /// session gets a loop shorter than its first take. See `Shared::loop_grid`.
    ///
    /// Zero means "not declared", which reads as one everywhere. Kept distinct
    /// from one so a loop that has never been told anything can be told
    /// something by the first thing that measures it.
    pub cycles: AtomicUsize,
    /// The output frame a running **first** recording should close itself at,
    /// or `i64::MIN` for "wait for a press".
    ///
    /// Set only when the loop already knew its length before recording began —
    /// which, once there is a clock or a declared bar count, is every recording
    /// after the very first one of a clockless session. That is the whole point
    /// of it: the second press exists because the engine did not know how long
    /// you meant, and as soon as it does, asking for it is ceremony.
    ///
    /// Read by `closer`, not by the callback, because closing a recording draws
    /// a layer and sleeps and neither belongs in an audio thread.
    pub close_at: AtomicI64,
    /// The length a running **first** recording was told to be, or zero for
    /// "whatever gets captured".
    ///
    /// **`commit` measures, and a declared loop must not be measured.** Its
    /// fallback is `reached` — the frames the input actually delivered — which
    /// trails the output by `K` and by the drain `commit` sleeps for, so a loop
    /// told it was one bar came back 26 ms short of one. Sonically nothing; on
    /// the grid it is a loop that walks away from every other loop a pass at a
    /// time, and the cause is invisible because the take sounds right.
    ///
    /// So the number that was asked for wins over the number that was counted.
    /// Only for a length that was declared *before* a note was played — a free
    /// take still gets exactly what it captured, because there nothing was
    /// asked for.
    pub rec_len: AtomicUsize,
    pub n_layers: AtomicUsize,
    /// The layers, `max_layers` of them from construction. Those at
    /// `n_layers` and above are not playing; the ones up to `redo_to` are
    /// kept for redo. See `Layer`.
    pub(crate) layers: Vec<Layer>,
    /// The output frame at which this loop's position zero sits.
    ///
    /// Per loop, which is what lets eight loops of different lengths run at once
    /// without any of them being the master. Whether they *should* be free of
    /// each other is a musical question, and the answer is a quantisation
    /// policy applied when a loop closes — not a shared origin, which would
    /// decide it here and for ever.
    pub origin: AtomicI64,
    /// Silenced, but still turning.
    ///
    /// **Phase-locked, deliberately.** The playhead keeps advancing while a loop
    /// is stopped, so bringing it back is not "start again" but "become audible
    /// again, where you would have been". With eight loops that is the only
    /// behaviour worth having: a loop that restarted from its own zero would
    /// come back out of phase with everything it was recorded against.
    ///
    /// It is also why this is a flag rather than a state. Stopping is
    /// orthogonal to the record machine — a loop can be stopped while playing
    /// or while overdubbing — and folding it into `state` would make the
    /// machine describe two things at once. `Data.Loopy`, removed from the app
    /// long before this existed, had already reached the same conclusion and
    /// called it `PhaseMuted`.
    ///
    /// The alternative — moving `origin` — is the one thing that must never
    /// happen to a loop that closed on a grid boundary.
    pub muted: AtomicBool,
    /// Loop frames travelled per output frame. Negative plays backwards.
    ///
    /// A *resolution*, like `period` and `phase`: the samples are untouched and
    /// the playhead is simply asked to move at a different rate, so speed costs
    /// nothing to change and nothing to change back.
    ///
    /// **Direction is the sign, not a separate flag.** It was a flag for a day,
    /// and a flag is a second source of truth about which way the playhead is
    /// going — SuperDirt has always spelt backwards as a negative `speed`, and
    /// splitting them here would invent a distinction the rest of the rig does
    /// not make. Folding it in also removed a click: mirroring `pos` to
    /// `len - 1 - pos` jumps the playhead across the loop at the instant you
    /// press it, where a sign change simply turns round where it stands.
    ///
    /// `f64` in an `AtomicU64` because the position it drives is an absolute
    /// frame count, and `f32` runs out of mantissa at 16.7 M — about six
    /// minutes at 48 k, which is well inside what a long take can reach.
    pub speed: AtomicU64,
    /// Forward, then back: the playhead reflects at each end instead of
    /// wrapping, so a cycle takes twice as long and the loop is heard both ways
    /// round.
    ///
    /// Free, given speed. A pendulum is a triangle where a plain loop is a
    /// sawtooth, and the fold is two lines in the same place the wrap already
    /// happens — which is why it is here rather than on the list of things
    /// waiting for engine work.
    pub pendulum: AtomicBool,
    /// **Which input this loop records from.** An index into `Shared::sources`.
    ///
    /// Per loop rather than per rig, because `ClaimPast` decides *afterwards*
    /// which loop a moment belongs to — so the moment has to have been captured
    /// on every source, and the loop says which one it wants when it takes it.
    ///
    /// Survives a clear, like the other things that describe how you work
    /// rather than what is in the loop. Clearing a slot you had pointed at the
    /// drum machine and finding it back on the guitar would be a surprise in
    /// the middle of the one gesture that is supposed to be a fresh start.
    pub src: AtomicUsize,
    /// **Fold this loop's two channels together at playback.**
    ///
    /// A playback decision and deliberately not a capture one: the audio is
    /// always kept in stereo, so this is instantly reversible and costs nothing
    /// to try. On, the two channels are summed and `pan` is a true pan — which
    /// is what you want for a source with no meaningful stereo content and a
    /// place you want it to sit. Off, they pass through and `pan` is a balance.
    ///
    /// Andrew asked for this as a capture-time option; at playback it is
    /// strictly better, because nothing is thrown away by a choice made before
    /// the take.
    pub mono: AtomicBool,
    /// Where the playhead sits at `origin`, in loop frames.
    ///
    /// Zero until something changes speed. Playback is `warp + (frame -
    /// origin) * speed`, so at `warp = 0, speed = 1` it is exactly the
    /// subtraction it has always been, down to the bit — which is what keeps
    /// the alignment self-test a regression test rather than a new claim.
    ///
    /// It exists because a speed change must not move the audio. Rescaling the
    /// whole history would jump the playhead by however far it had already
    /// come; instead the callback records where the loop *is* and rescales only
    /// what happens next.
    pub(crate) warp: AtomicU64,
    /// The window: an in and an out point in arena positions, or both zero
    /// for none. **Non-destructive.** Playback goes round `in..out` and so
    /// does the render, which is what makes an edit you can hear the edit
    /// you export. Recording is refused while one is set — see `dispatch`.
    ///
    /// **Signed, and allowed past the loop's ends.** An in point before zero
    /// or an out point past the length is silence — up to one loop's worth
    /// on either side — so a window can *extend* a loop as well as trim it:
    /// counter-rotate at the start and the loop gains rest before its
    /// downbeat, and nothing in the arena has to move to make room.
    pub(crate) win_in: AtomicI64,
    pub(crate) win_out: AtomicI64,
    /// Where the cycle starts, as an offset into the window (or the loop):
    /// position zero of a pass is arena position `start + rot`. Shifting the
    /// starting point of a loop without moving a sample, so a render begins
    /// where you chose rather than where the take happened to close.
    pub rot: AtomicUsize,
    /// The frame at which an edit restarts the pass, or zero for none.
    ///
    /// **An edit you can hear is a restart, debounced.** Moving the in point
    /// while the loop plays should play the loop *from* the new in point —
    /// that is what "trim from the start" means to the ear — but a slider
    /// sends a dozen edits a second while it moves, and a dozen restarts a
    /// second is static. So each edit schedules the restart a short way
    /// ahead and the next edit moves it on; the callback fires it when the
    /// edits have stopped. A loop on the grid, or the one the grid comes
    /// from, restarts on the next bar line instead, so the edit lands
    /// where a bar starts and the grid does not move.
    pub(crate) edit_restart: AtomicI64,
    /// The edit itself, held until the restart fires. **Applying an edit
    /// the moment it arrives was the tearing sound**: each slider event
    /// moved the start under the playhead, and a dozen jumps a second is a
    /// dozen clicks. So the window and rotation the hand is setting sit
    /// here, the loop goes on playing what it was, and when the edits have
    /// stopped the callback applies them and restarts in one move. The
    /// snapshot reports these when they are set, so the page shows the hand
    /// rather than the past.
    pub(crate) pend_in: AtomicI64,
    pub(crate) pend_out: AtomicI64,
    pub(crate) pend_rot: AtomicUsize,
    pub(crate) pend_set: AtomicBool,
    /// A pending speed and pendulum, consumed by the output callback.
    ///
    /// The same argument as `request_at`: only the callback knows the frame, and
    /// re-anchoring `warp` against a frame the control thread guessed would be
    /// out by up to a buffer — 21 ms of jump at 1024 frames, which is a click.
    pub(crate) cfg_speed: AtomicU64,
    pub(crate) cfg_pend: AtomicBool,
    pub(crate) cfg_armed: AtomicBool,
    /// Stereo placement, 0 hard left to 127 hard right, 64 centre.
    ///
    /// Equal-power, and the gains are computed once per buffer rather than once
    /// per frame — six loops times two `cos` calls is nothing at buffer rate and
    /// wasteful at sample rate.
    pub pan: AtomicUsize,
    /// Which loop this is, for the one log line `enter` can write.
    pub(crate) index: usize,
    /// The phase, as its byte. Private: `enter` is the only writer and
    /// `phase` the reader, so the table in `phase.rs` sees every move.
    state: AtomicU8Wrapper,
    /// The plan for the next recording and the transition that starts it:
    /// the phase asked for and its frame, a one-pass overdub, a level
    /// crossing's back-date. Set by the control thread, taken once by the
    /// output callback, cleared in one place. See `NextTake`.
    pub(crate) next: NextTake,
    /// Whether this loop's transitions wait for the grid.
    ///
    /// Off by default, so a rig that never asks for it behaves exactly as it
    /// did — which is also what keeps the self-test a regression test rather
    /// than a description of new behaviour.
    pub(crate) quant: AtomicBool,
    /// Highest position the first recording reached, so a loop can be closed at
    /// the right length even though the input trails the output.
    pub(crate) reached: AtomicUsize,
    /// Highest output frame the input callback actually wrote for this
    /// recording, one past the end.
    ///
    /// Asked rather than inferred. Undoing an overdub's wrapped tail means
    /// subtracting exactly the samples that were added, and "how far did the
    /// input get" cannot be worked out from a clock afterwards: the flip to
    /// PLAYING stops the writes, but frames keep arriving and the drain sleep
    /// lets an in-flight callback finish. Reading `in_frames` afterwards
    /// therefore names frames that were never recorded, and subtracting those
    /// would gouge real audio out of the loop head — a ghost where there had
    /// been a doubling.
    pub(crate) rec_reached: AtomicI64,
    /// How far back up the layer stack still holds audio, so undo can be
    /// taken back.
    ///
    /// Undo no longer zeroes what it removes, so an undone layer is still
    /// there and can simply be counted back in. This is the highest layer
    /// index that is still recoverable; recording into a layer moves it,
    /// because a take that has been recorded over is not recoverable and
    /// offering to redo it would be a lie.
    pub(crate) redo_to: AtomicUsize,
    pub(crate) overflowed: AtomicBool,
    /// How late the press that started this recording was, in frames.
    ///
    /// The app knows when the MIDI arrived and the daemon does not, so lateness
    /// travels on the command (`0r@312`) and is kept here until the recording
    /// closes — which is the only moment it can be spent, because the pre-roll
    /// shift happens at commit.
    ///
    /// Zero means "no measurement", and the compiled `--preroll-ms` is used
    /// instead. That is deliberately not the same as a measured zero: a rig
    /// that cannot time its own presses should still be able to say "always
    /// reach back 40 ms" by hand.
    pub(crate) started_late: AtomicI64,
    /// Output frame at which the layer being recorded has its position zero.
    /// Equal to `origin` for a first recording; for a multiply it is the cycle
    /// boundary the multiply started on, which is also where the *new* loop's
    /// position zero will end up.
    pub(crate) rec_from: AtomicI64,
    /// Play one pass and stop, rather than turning for ever.
    ///
    /// A mode rather than a state, like `muted` and for the same reason: it is
    /// orthogonal to the record machine. A one-shot can be recorded into, undone
    /// and overdubbed exactly as any other loop; the only thing it changes is
    /// what happens between fires, which is silence.
    pub one_shot: AtomicBool,
    /// The output frame the current pass ends at, or `i64::MIN` for "not
    /// sounding".
    ///
    /// `i64::MIN` rather than a separate flag so that switching the mode on puts
    /// a loop straight into the silence it will spend most of its life in — one
    /// comparison in the mixer, no second thing to keep in step.
    pub(crate) shot_end: AtomicI64,
    /// Wait for a sound rather than starting on the press.
    ///
    /// The other half of *"we can't go back in time, but we're monitoring
    /// continuously"*: with the ring running, arming costs nothing and the
    /// recording can begin before the command that caused it.
    pub level_arm: AtomicBool,
    /// How many frames of the wrap are crossfaded with the layer's continuation.
    /// Zero is off, which is the default.
    ///
    /// **A resolution applied at playback, not an edit.** The samples are never
    /// touched: the mixer reads two of them near a wrap instead of one. So the
    /// length can be changed while the loop plays, turned off, and undone by
    /// turning it off — the same standing as speed, pan and direction, and the
    /// same reason. *Store everything, flatten late.*
    pub fade: AtomicUsize,
    /// How much of itself a layer keeps from one pass to the next. `1.0` holds
    /// for ever, which is the default and what a looper has always done.
    ///
    /// **The parameter that separates Frippertronics from song looping.** Two
    /// Revoxes with the second one feeding back below unity is this number, and
    /// so is what a tape echo does to its repeats. Without it every layer plays
    /// at full for ever and the only shape a loop can have is the one it was
    /// given.
    ///
    /// A resolution at playback like speed, pan and the wrap fade — nothing is
    /// scaled in the arena — so a loop that has faded to nothing is still all
    /// there, and turning decay off brings it back.
    pub decay: AtomicU32,
    /// This loop's own level, as a linear gain. `1.0` is unity, which is where
    /// every loop starts.
    ///
    /// **Added 2026-08-25, and the engine went without it for a reason that
    /// stopped being true.** A looper whose loops are either in or out needs no
    /// faders: mute says everything. What changed is the Twister — eight loops
    /// with a knob each, and the first thing a hand does with a knob is set how
    /// loud something is. `chance` was standing in for it and is not a level;
    /// it is a gate on whole passes.
    ///
    /// A resolution at playback like speed, pan and decay: nothing is scaled in
    /// the arena, so turning a loop down and back up loses nothing.
    ///
    /// Multiplied into the pan gains once per buffer, so it costs nothing in
    /// the frame loop.
    pub vol: AtomicU32,
    /// The envelope of the recording **that is happening right now**, as
    /// `ENV_BUCKETS` bytes on the same absolute -60 dBFS scale as the committed
    /// ones.
    ///
    /// **Atomics rather than the `env` mutex**, because this is written from the
    /// audio callback and that one is not. `rebuild_env` runs on the command
    /// path at commit and can afford a lock; a live picture cannot, and a
    /// callback that blocks on a mutex is a callback that eventually misses a
    /// buffer.
    ///
    /// Empty of meaning while nothing is recording — cleared when a recording
    /// starts rather than when it ends, so what you see is always the take in
    /// hand and never the last one.
    pub rec_env: Vec<AtomicU8>,
    /// **Revox mode: the loop is a tape, and an overdub writes over it.**
    ///
    /// Everywhere else in this engine a pass is non-destructive — layers are
    /// kept whole and `decay` is a *resolution* applied at playback, which is
    /// why turning decay off brings a faded loop back. That is the right
    /// default and it is not what a tape does. Two Revoxes with the second one
    /// feeding back below unity erase as they record: what is under the head
    /// comes back quieter each time round, and there is no version of it that
    /// was not erased.
    ///
    /// So this is a mode you opt into, and the price is stated rather than
    /// hidden: **undo goes away**, because there is nothing kept to go back to.
    /// Entering flattens the loop to one layer — a tape has no layers — and
    /// leaving does not unflatten it.
    /// Whether this loop's one layer is a **threaded empty tape** — a length
    /// with nothing played onto it yet.
    ///
    /// The distinction `n_layers` cannot make. A threaded tape has one layer so
    /// that it *plays* (see `blank`), which makes it indistinguishable from a
    /// recorded loop by layer count alone — and that made the length knob a
    /// one-shot: the first turn threaded eight seconds and every turn after was
    /// refused as "there is something in it". There is not. Adjusting the length
    /// of a tape you have not played onto is exactly how you choose a length.
    ///
    /// Cleared the moment anything is recorded, which is the moment resizing
    /// would become a trim.
    pub threaded: AtomicBool,
    pub revox: AtomicBool,
    /// What a Revox pass leaves of what was under it, as a linear gain. `1.0`
    /// is a tape that never erases; `0.0` is one that replaces.
    ///
    /// **Its own value rather than `decay`'s**, deliberately. They are the same
    /// musical idea by two mechanisms — one destroys and one does not — and a
    /// single number meaning "resolution here, erase-head there" depending on a
    /// flag is the kind of overload this codebase spends whole comments
    /// regretting. `dec` still works in Revox mode and still does what it always
    /// did.
    pub fb: AtomicU32,
    /// How much top the tape keeps, as a corner frequency in hertz.
    ///
    /// **Tape loses the high end before it loses the level**, and losing only
    /// the level is what makes a digital feedback loop sound like a digital
    /// feedback loop: the last repeat is the first one, quieter, with every
    /// edge still on it. A pass over a real head comes back a little duller, and
    /// twenty passes come back as a wash.
    ///
    /// One pole, applied to what is already on the tape as the head goes over
    /// it. Not a simulation of anything — no head bump, no wow, no hiss — and
    /// that is the point: the whole of the effect is that each pass costs you a
    /// little of the top, and each pass costs it again.
    ///
    /// **In Revox only, and that is a fact about the design rather than a
    /// shortcut.** Outside it, `decay` is a *resolution* applied at playback
    /// with nothing in the arena touched, which is what lets a faded loop come
    /// back — a filter there would have to be a different filter per layer per
    /// pass count, cascaded as deep as the loop is old. Here the erasing has
    /// already happened, so darkening it is one multiply and it is permanent
    /// for the same reason everything else in this mode is.
    ///
    /// At or above 20 kHz it is bypassed rather than approximated, so "off" is
    /// off and not "very nearly".
    pub tone: AtomicU32,
    /// The one-pole's memory, carried across buffers and across the wrap —
    /// which is right, because the head does not stop at the splice.
    pub tape_lp: AtomicU32,
    /// How often a pass sounds, as a probability. `1.0` is always.
    ///
    /// A gate on the mix and nothing else — the playhead keeps turning, `origin`
    /// never moves, and the pass count keeps counting. Exactly the shape of
    /// `muted`, and phase-locked for the same reason: a loop that plays one
    /// cycle in four has to come back on the cycle it would have been on, or it
    /// is not one cycle in four of anything.
    pub chance: AtomicU32,
    /// Which pass the last roll was for, and what it came up.
    ///
    /// The roll happens in the mixer, which runs per frame — so it has to be
    /// remembered, or a one-in-four loop would flicker at sample rate instead of
    /// dropping cycles. One roll per pass, held for the whole pass.
    pub(crate) chance_pass: AtomicI64,
    pub(crate) chance_sounds: AtomicBool,
}

impl Loop {
    pub(crate) fn new(index: usize, max_layers: usize) -> Self {
        Loop {
            loop_len: AtomicUsize::new(0),
            cycles: AtomicUsize::new(0),
            close_at: AtomicI64::new(i64::MIN),
            rec_len: AtomicUsize::new(0),
            n_layers: AtomicUsize::new(0),
            layers: (0..max_layers).map(|_| Layer::new()).collect(),
            origin: AtomicI64::new(0),
            muted: AtomicBool::new(false),
            speed: AtomicU64::new(1.0f64.to_bits()),
            pendulum: AtomicBool::new(false),
            src: AtomicUsize::new(0),
            mono: AtomicBool::new(false),
            warp: AtomicU64::new(0.0f64.to_bits()),
            win_in: AtomicI64::new(0),
            win_out: AtomicI64::new(0),
            rot: AtomicUsize::new(0),
            edit_restart: AtomicI64::new(0),
            pend_in: AtomicI64::new(0),
            pend_out: AtomicI64::new(0),
            pend_rot: AtomicUsize::new(0),
            pend_set: AtomicBool::new(false),
            cfg_speed: AtomicU64::new(1.0f64.to_bits()),
            cfg_pend: AtomicBool::new(false),
            cfg_armed: AtomicBool::new(false),
            pan: AtomicUsize::new(64),
            index,
            state: AtomicU8Wrapper::new(Phase::Idle.as_u8()),
            next: NextTake::new(),
            quant: AtomicBool::new(false),
            reached: AtomicUsize::new(0),
            rec_reached: AtomicI64::new(0),
            redo_to: AtomicUsize::new(0),
            overflowed: AtomicBool::new(false),
            rec_from: AtomicI64::new(0),
            started_late: AtomicI64::new(0),
            one_shot: AtomicBool::new(false),
            shot_end: AtomicI64::new(i64::MIN),
            level_arm: AtomicBool::new(false),
            fade: AtomicUsize::new(0),
            decay: AtomicU32::new(1.0f32.to_bits()),
            vol: AtomicU32::new(1.0f32.to_bits()),
            rec_env: (0..ENV_BUCKETS).map(|_| AtomicU8::new(0)).collect(),
            threaded: AtomicBool::new(false),
            revox: AtomicBool::new(false),
            fb: AtomicU32::new(10f32.powf(-3.0 / 20.0).to_bits()),
            tone: AtomicU32::new(6500.0f32.to_bits()),
            tape_lp: AtomicU32::new(0.0f32.to_bits()),
            chance: AtomicU32::new(1.0f32.to_bits()),
            chance_pass: AtomicI64::new(i64::MIN),
            chance_sounds: AtomicBool::new(true),
        }
    }

    pub fn speed(&self) -> f64 {
        f64::from_bits(self.speed.load(Ordering::Relaxed))
    }

    /// Whether the playhead is doing the plain thing: forward, at rate one, from
    /// `origin`.
    ///
    /// Everything that *writes* asks this first. Recording at a speed is a
    /// different instrument — the input arrives at rate one and would have to be
    /// resampled into a buffer whose grid is moving — and the honest answer for
    /// now is to refuse and say so, rather than record something nobody asked
    /// for. Playback is where speed belongs, and playback is where it is.
    pub fn plain(&self) -> bool {
        self.speed() == 1.0
            && !self.pendulum.load(Ordering::Relaxed)
            && f64::from_bits(self.warp.load(Ordering::Relaxed)) == 0.0
            && self.window().is_none()
    }

    /// The window and rotation as the hand has set them — pending if an edit
    /// is waiting to be applied, live otherwise. What the snapshot reports.
    pub fn edit_view(&self) -> (i64, i64, usize) {
        if self.pend_set.load(Ordering::Acquire) {
            (
                self.pend_in.load(Ordering::Relaxed),
                self.pend_out.load(Ordering::Relaxed),
                self.pend_rot.load(Ordering::Relaxed),
            )
        } else {
            (
                self.win_in.load(Ordering::Relaxed),
                self.win_out.load(Ordering::Relaxed),
                self.rot.load(Ordering::Relaxed),
            )
        }
    }

    /// The window as `(in, out)`, or `None` for the whole loop.
    pub fn window(&self) -> Option<(i64, i64)> {
        let i = self.win_in.load(Ordering::Relaxed);
        let o = self.win_out.load(Ordering::Relaxed);
        if i == 0 && o == 0 {
            None
        } else {
            Some((i, o))
        }
    }

    /// One trip round, as `(start, span)` in arena positions: the window's,
    /// or the loop's. `start` may be negative and `start + span` may pass the
    /// length — that is the silence a window can add. A window that has
    /// stopped making sense (an emptied loop, say) counts as none.
    pub fn cycle(&self, len: usize) -> (i64, usize) {
        let l = len as i64;
        match self.window() {
            Some((i, o)) if o > i && i >= -l && o <= 2 * l && len > 0 => (i, (o - i) as usize),
            _ => (0, len),
        }
    }

    /// Cycle position `c` (0 to span) as an arena position: rotated inside
    /// the window, so position zero of a pass is `start + rot`.
    fn place(&self, start: i64, span: usize, c: f64) -> f64 {
        let r = self.rot.load(Ordering::Relaxed) % span.max(1);
        let mut q = c + r as f64;
        if q >= span as f64 {
            q -= span as f64;
        }
        start as f64 + q
    }

    /// Where the playhead is, in loop frames, at an output frame.
    ///
    /// Fractional, which is the whole of what speed costs: at any rate but one
    /// the playhead lands between samples, and the mix has to interpolate.
    ///
    /// The pendulum fold happens here rather than in the caller because it is a
    /// property of *where the playhead is*, not of what is read there — and
    /// keeping it here means the display and the audio cannot disagree about
    /// which way round a loop currently is.
    /// Where the playhead is *before* it is folded back into the loop: how far
    /// it has travelled since `origin`, in loop frames, without wrapping.
    ///
    /// Both the position and the pass count come out of this one expression, so
    /// "where in the cycle" and "which cycle" cannot come to disagree — which
    /// they would the first time speed or a pendulum was involved and only one
    /// of them was taught about it.
    pub(crate) fn raw_pos(&self, out_frame: i64) -> f64 {
        let warp = f64::from_bits(self.warp.load(Ordering::Relaxed));
        let origin = self.origin.load(Ordering::Acquire);
        warp + (out_frame - origin) as f64 * self.speed()
    }

    /// How many complete trips through the loop have gone by, counting from
    /// `origin`. Negative before it, which is honest rather than clamped.
    ///
    /// One *pass* is what chance rolls for, and a pendulum's pass is there and
    /// back — the same span `pass_frames` measures, so a swinging loop that
    /// plays one cycle in four drops a whole there-and-back rather than half of
    /// one.
    pub fn pass_index(&self, out_frame: i64, len: usize) -> i64 {
        if len == 0 {
            return 0;
        }
        let (_, cyc) = self.cycle(len);
        let span = if self.pendulum.load(Ordering::Relaxed) { 2 * cyc } else { cyc } as f64;
        (self.raw_pos(out_frame) / span).floor() as i64
    }

    pub fn play_pos(&self, out_frame: i64, len: usize) -> f64 {
        if len == 0 {
            return 0.0;
        }
        let raw = self.raw_pos(out_frame);
        let (start, span) = self.cycle(len);
        let lenf = span as f64;
        let c = if self.pendulum.load(Ordering::Relaxed) {
            // A triangle where a plain loop is a sawtooth. `2 * len` is one
            // there-and-back, and the second half is read as the reflection of
            // the first — so the turn happens at the ends of the audio rather
            // than at an arbitrary point, which is what makes it sound like a
            // tape reversing rather than a jump.
            let q = raw.rem_euclid(2.0 * lenf);
            if q < lenf {
                q
            } else {
                (2.0 * lenf - q).min(lenf - 1.0).max(0.0)
            }
        } else {
            raw.rem_euclid(lenf)
        };
        self.place(start, span, c)
    }

    /// Where an input frame at cycle position `raw` lands in the arena: the
    /// same slot the play head is reading at that moment — through the window
    /// and the rotation, exactly as `place` puts it — or nowhere, when the
    /// window reaches into the silence past an end, which has no slot.
    ///
    /// **This is what lets a windowed loop take an overdub.** The write head
    /// used to follow the *cycle* (`rel % len`), which is the play head only
    /// when there is no window and no rotation; with either set, what you
    /// played would land somewhere you never heard it. Recording into a
    /// windowed loop was refused for exactly that reason, and a rotated one
    /// quietly got it wrong. Now both heads read the same map.
    pub fn write_pos(&self, raw: i64, len: usize) -> Option<usize> {
        if len == 0 {
            return None;
        }
        let (start, span) = self.cycle(len);
        let span_i = span.max(1) as i64;
        let r = (self.rot.load(Ordering::Relaxed) as i64) % span_i;
        let q = (raw.rem_euclid(span_i) + r) % span_i;
        let a = start + q;
        if a >= 0 && (a as usize) < len { Some(a as usize) } else { None }
    }

    /// Adopt a new speed and pendulum without moving the audio.
    ///
    /// Called only from the output callback, at a frame it knows exactly. The
    /// playhead is read under the old settings and `warp` is chosen so the new
    /// ones put it in the same place — after which everything downstream is
    /// arithmetic and nothing is stored about how it got there.
    pub(crate) fn adopt(&self, out_frame: i64, len: usize, speed: f64, pend: bool) {
        // The cycle position, not the arena one: `raw_pos` counts from the
        // start of the window and before the rotation, so that is what the
        // new warp has to reproduce.
        let (start, span) = self.cycle(len);
        let r = self.rot.load(Ordering::Relaxed) % span.max(1);
        let here = (self.play_pos(out_frame, len) - start as f64 - r as f64).rem_euclid(span.max(1) as f64);
        self.speed.store(speed.to_bits(), Ordering::Relaxed);
        self.pendulum.store(pend, Ordering::Relaxed);
        if len == 0 {
            // Nothing to hold in place. An empty loop has no position to
            // preserve and its `origin` has not been stamped yet, so anchoring
            // against it would store a number about a frame that means nothing.
            self.warp.store(0.0f64.to_bits(), Ordering::Relaxed);
            return;
        }
        let origin = self.origin.load(Ordering::Acquire);
        let warp = here - (out_frame - origin) as f64 * speed;
        if speed == 1.0 && !pend {
            // Coming back to rate one, the offset is a whole-frame shift of
            // where position zero sits — so put it there and have done, rather
            // than carry it as a fraction for ever. That restores the exact
            // integer arithmetic (and with it the no-interpolation path), and
            // it makes `origin` tell the truth again: a loop that spent a while
            // at half speed really has drifted off the grid it closed on, and
            // this is where it says so.
            //
            // Rounding loses at most half a sample of position, once, at a
            // moment the player asked for a change anyway.
            self.origin
                .store(origin - warp.round() as i64, Ordering::Release);
            self.warp.store(0.0f64.to_bits(), Ordering::Relaxed);
        } else {
            self.warp.store(warp.to_bits(), Ordering::Relaxed);
        }
    }

    /// How many output frames one trip through this loop takes, at whatever
    /// speed and direction it is currently set to.
    ///
    /// Only a one-shot needs it — everything else wraps and never asks how long
    /// a pass was — but it is the arithmetic most likely to be quietly wrong, so
    /// it is a function with tests rather than three lines inside a callback.
    pub(crate) fn pass_frames(&self, len: usize) -> i64 {
        // A pendulum goes there and back before it has been round once.
        let (_, cyc) = self.cycle(len);
        let span = if self.pendulum.load(Ordering::Relaxed) { 2 * cyc } else { cyc };
        // Direction does not change how long a pass takes, only which end it
        // starts at — so the rate is the magnitude.
        let rate = self.speed().abs().max(1e-6);
        (span as f64 / rate).round() as i64
    }

    /// Back to forward, rate one, no offset — what a cleared loop plays at.
    pub(crate) fn plainly(&self) {
        self.speed.store(1.0f64.to_bits(), Ordering::Relaxed);
        self.pendulum.store(false, Ordering::Relaxed);
        self.warp.store(0.0f64.to_bits(), Ordering::Relaxed);
        self.cfg_speed.store(1.0f64.to_bits(), Ordering::Relaxed);
        self.cfg_pend.store(false, Ordering::Relaxed);
        self.cfg_armed.store(false, Ordering::Relaxed);
    }

    /// Everything a clear forgets about one loop.
    ///
    /// Lifted out of the `c` arm of `dispatch` because it had grown to twenty
    /// lines in the middle of a very long match, and a list that long inside a
    /// match arm is a list nothing can test. It was missing `quant`: measured
    /// on the running daemon 2026-08-24, every other mode reset across a clear
    /// and `grid` stayed lit, so a cleared slot silently waited for the next
    /// bar before it began recording — a surprise you diagnose as a broken
    /// footswitch rather than as a setting.
    ///
    /// The rule this encodes: **a cleared slot has nobody's habits.** A loop
    /// that came back at half speed, backwards, hard left, firing once and
    /// waiting for a sound would be a haunting, and the switch that cleared it
    /// said nothing about any of that.
    ///
    /// Audio-side clearing — layer shapes, the envelope, the anchor — stays
    /// with the caller, which is the only thing holding `Shared`.
    pub(crate) fn cleared(&self, at: i64) {
        self.enter(Phase::Idle, at);
        // An empty loop that is still silenced would refuse to record audibly
        // for a reason nothing on screen could explain.
        self.muted.store(false, Ordering::Relaxed);
        // And for the same reason, at full level. A cleared slot sitting at
        // -58 dB is silenced by a different mechanism and looks identical from
        // outside — which is exactly how it was found: a loop recorded happily
        // into a cleared slot and made no sound, and `Clear All` did not fix it
        // because clearing was the thing that had failed to reset it.
        self.vol.store(1.0f32.to_bits(), Ordering::Relaxed);
        // A cleared slot is not still a tape. The feedback amount survives,
        // like the other settings that describe how you work rather than what
        // is in the loop.
        self.revox.store(false, Ordering::Relaxed);
        self.threaded.store(false, Ordering::Relaxed);
        // The filter's memory is audio, not a setting: it goes with the audio.
        // `tone` and `fb` describe how you work and stay.
        self.tape_lp.store(0.0f32.to_bits(), Ordering::Relaxed);
        self.plainly();
        self.pan.store(64, Ordering::Relaxed);
        self.one_shot.store(false, Ordering::Relaxed);
        self.shot_end.store(i64::MIN, Ordering::Release);
        self.level_arm.store(false, Ordering::Relaxed);
        self.quant.store(false, Ordering::Relaxed);
        self.fade.store(0, Ordering::Relaxed);
        self.decay.store(1.0f32.to_bits(), Ordering::Relaxed);
        self.chance.store(1.0f32.to_bits(), Ordering::Relaxed);
        self.chance_pass.store(i64::MIN, Ordering::Relaxed);
        self.chance_sounds.store(true, Ordering::Relaxed);
        self.n_layers.store(0, Ordering::Release);
        self.redo_to.store(0, Ordering::Release);
        self.loop_len.store(0, Ordering::Release);
        // **Everything that says how long this loop is, together.**
        //
        // `loop_len` went to zero here from the beginning and `cycles` did not,
        // which was harmless while a bar count could only come from a recording
        // — the two were made and destroyed at the same moment. `len<n>` broke
        // that: it sizes an *empty* loop, so after a clear this slot said "no
        // length" and "four bars" at the same time.
        //
        // What that cost is worth writing down, because it looked like an
        // engine fault and was not. The Twister's ring is drawn from `cycles`,
        // so a cleared loop still showed four bars — and the app writes ring
        // positions back to the device, so the encoder physically sat at four.
        // Turning it "to 4" was then impossible: it was already there, no CC
        // moved, no `len4` was sent, and the next take recorded open-ended. The
        // second run of a recipe failed while the first one worked, which is the
        // signature of state that outlives the thing it describes.
        //
        // The same argument reaches `close_at` and `rec_len`: both describe a
        // recording that is no longer going to happen, and a stale `close_at`
        // is a timer pointed at a take nobody has played yet.
        self.cycles.store(0, Ordering::Release);
        self.close_at.store(i64::MIN, Ordering::Release);
        self.rec_len.store(0, Ordering::Release);
        // And the plan for the next take, whole: a request still pending, its
        // frame, a one-pass, a back-date. A cleared loop has no next take.
        self.next.clear();
    }

    /// Ask for a speed and pendulum. Applied by the callback, at its own frame.
    pub(crate) fn want(&self, speed: f64, pend: bool) {
        self.cfg_speed.store(speed.to_bits(), Ordering::Relaxed);
        self.cfg_pend.store(pend, Ordering::Relaxed);
        self.cfg_armed.store(true, Ordering::Release);
    }

    /// Left and right gain for this loop's pan setting, equal-power.
    ///
    /// At centre both are `1/sqrt(2)`, so a centred loop is the same loudness
    /// as a hard-panned one — which linear panning would not give, and which
    /// matters when six loops are being placed against each other.
    pub fn pan_gains(&self) -> (f32, f32) {
        let p = self.pan_position();
        let theta = p * std::f32::consts::FRAC_PI_2;
        (theta.cos(), theta.sin())
    }

    /// The same knob, read as a **balance** — for a loop that is already two
    /// channels and is not being folded.
    ///
    /// Equal-power panning is for *placing a signal*. Applied to a stereo pair
    /// it does two wrong things at once: at centre it takes 3 dB off both sides
    /// for no reason, and turning it collapses a field that was recorded rather
    /// than inventing one. What the knob should mean there is what it means on
    /// a mixer: leave one side alone and take the other down.
    ///
    /// So: unity both sides at centre, and one side falling linearly to silence
    /// at the end of the travel. Attenuating only — no side is ever boosted, so
    /// a balanced loop can never be louder than the loop that was recorded, and
    /// there is no headroom to lose.
    pub fn balance_gains(&self) -> (f32, f32) {
        let p = self.pan_position();
        ((2.0 * (1.0 - p)).min(1.0), (2.0 * p).min(1.0))
    }

    /// The knob's travel as a fraction, with **the detent at exactly a half**.
    ///
    /// It was `v / 127.0`, which cannot put centre in the middle: 127 is odd,
    /// so 64 lands on 0.5039 and a centred loop came out 0.07 dB down on the
    /// left. Inaudible, and it stayed unnoticed for exactly that reason — but
    /// export writes these gains *into the file*, and a stereo take whose
    /// centre is not centred is the sort of tilt that gets chased later in
    /// somebody else's mixer.
    ///
    /// So the two halves of the travel are scaled separately, which costs a
    /// slope change of one part in 128 at the detent and buys an exact middle,
    /// an exact hard left and an exact hard right.
    fn pan_position(&self) -> f32 {
        let v = self.pan.load(Ordering::Relaxed).min(127) as f32;
        if v <= 64.0 {
            v / 128.0
        } else {
            0.5 + (v - 64.0) / 126.0
        }
    }

    /// The phase: one Acquire load of the byte, as the audio thread has
    /// always read it.
    pub(crate) fn phase(&self) -> Phase {
        Phase::from_u8(self.state.get())
    }

    /// **The one place the phase is stored.** `at` is the output frame the
    /// transition belongs to — the callback's stamp, or the control thread's
    /// `out_frames` read at that moment — passed rather than read here, so
    /// the callback remains the only stamper. A pair outside `phase::LEGAL`
    /// is stored anyway and logged once, so behaviour is what it was and
    /// the log names what the table missed; under test it panics, which is
    /// how the table is enforced.
    pub(crate) fn enter(&self, to: Phase, at: i64) {
        let from = self.phase();
        if !legal(from, to) {
            #[cfg(test)]
            panic!(
                "loop {}: phase {} -> {} at frame {} is not in the table",
                self.index, from, to, at
            );
            #[cfg(not(test))]
            note_illegal(self.index, from, to, at);
        }
        self.state.set(to.as_u8());
    }

    /// Take a level arm back: the loop returns to what it held before it
    /// listened, and the plan goes whole.
    ///
    /// **To what it held, not to idle** (2026-09-06, REVIEW-daemon-debt step
    /// 5b). Both roads out of `Armed` — the second `r`, and `lev0` under the
    /// wait — stored `Idle`, so a loop with layers that was armed from
    /// `Playing` came back reading idle with its layers intact and the mixer
    /// still summing them: a byte saying one thing about a loop doing
    /// another. The artifact returns to tape, playing, sized or empty by what
    /// is in the loop, and the byte is derived the same way here: layers (a
    /// threaded tape has one) mean it was playing; none mean it was idle,
    /// with whatever length it had kept.
    ///
    /// The whole plan goes, not just the back-date: a crossing found a buffer
    /// ago may already have set the request, and a cancelled arm that still
    /// recorded would be the worst kind of surprise.
    pub(crate) fn disarm(&self, at: i64) {
        let held = self.n_layers.load(Ordering::Acquire) > 0
            || self.threaded.load(Ordering::Relaxed);
        self.enter(if held { Phase::Playing } else { Phase::Idle }, at);
        self.next.clear();
    }

    pub fn state_name(&self) -> &'static str {
        self.phase().wire_word()
    }
    pub fn is_armed(&self) -> bool {
        self.phase() == Phase::Armed
    }
    pub fn is_recording(&self) -> bool {
        matches!(self.phase(), Phase::First | Phase::Overdub | Phase::Multiply)
    }
    /// How far a first take (or a multiply) has got, in frames: what a
    /// progress bar is drawn from while the loop has no length yet. Zero
    /// when nothing linear is being written — an overdub's progress is the
    /// play position, which the snapshot already carries.
    pub fn rec_frames(&self, now: i64) -> usize {
        match self.phase() {
            Phase::First | Phase::Multiply => self.reached.load(Ordering::Relaxed),
            // A one-pass overdub knows its close, so how far it has come is
            // the loop's length less what is left. Any other overdub has no
            // "how far": it goes round until told to stop, and reports zero.
            Phase::Overdub => match self.close_at.load(Ordering::Acquire) {
                i64::MIN => 0,
                at => {
                    let len = self.loop_len.load(Ordering::Acquire) as i64;
                    (len - (at - now)).clamp(0, len) as usize
                }
            },
            _ => 0,
        }
    }
    /// True when this loop wants the input — armed counts, because arming is a
    /// claim on the one converter the rig has.
    pub fn wants_input(&self) -> bool {
        self.is_armed() || self.is_recording()
    }
    pub fn quantised(&self) -> bool {
        self.quant.load(Ordering::Relaxed)
    }
    /// Whether a one-shot is inside a pass at this frame.
    ///
    /// Reported as well as mixed with, because the playhead does not stop
    /// between passes — it cannot, the arithmetic has no way to hold still —
    /// and a display reading `pos` alone shows a one-shot sweeping merrily
    /// along while it is silent. That is the same shape of lie the legend told
    /// about a bank nobody was standing on.
    pub fn firing(&self, out_frame: i64) -> bool {
        self.one_shot.load(Ordering::Relaxed) && out_frame < self.shot_end.load(Ordering::Acquire)
    }
    pub fn decay_of(&self) -> f32 {
        f32::from_bits(self.decay.load(Ordering::Relaxed))
    }
    /// What this layer is currently worth, after however many passes it has
    /// lived through. One for every layer of a loop that is not decaying.
    /// This layer's envelope, or empty when it has none yet.
    /// Forget the live picture. Called when a recording *starts*.
    pub fn clear_rec_env(&self) {
        for b in self.rec_env.iter() {
            b.store(0, Ordering::Relaxed);
        }
    }

    /// The live picture, or empty when nothing is being recorded — which the
    /// caller decides, because only it knows the state.
    pub fn rec_env_bytes(&self) -> Vec<u8> {
        self.rec_env.iter().map(|b| b.load(Ordering::Relaxed)).collect()
    }

    /// Raise one bucket to a peak. `fetch_max` rather than a store: a bucket
    /// spans hundreds of frames and the loudest of them is the one worth
    /// drawing, which is the same thing `rebuild_env` does with a `max` over a
    /// range.
    pub fn mark_rec_env(&self, bucket: usize, peak: f32) {
        if let Some(b) = self.rec_env.get(bucket) {
            b.fetch_max(to_byte(peak), Ordering::Relaxed);
        }
    }

    pub fn layer_gain(&self, layer: usize) -> f32 {
        self.layers[layer].gain()
    }

    pub fn layer_on(&self, layer: usize) -> bool {
        self.layers[layer].on()
    }

    pub fn layer_born(&self, layer: usize) -> i64 {
        self.layers[layer].born()
    }
    /// Recompute every layer's decay gain for the buffer starting at
    /// `out_frame`. Called once a buffer from the output callback, which is the
    /// only thread that knows the frame.
    pub(crate) fn age(&self, out_frame: i64) {
        let d = self.decay_of();
        let now = self.pass_index(out_frame, self.loop_len.load(Ordering::Acquire));
        for l in 0..self.layers.len() {
            let g = if d >= 1.0 {
                1.0
            } else {
                // Clamped because nothing is louder than silence twice, and an
                // exponent from a loop that has been running all afternoon
                // should not be asked of `powi`.
                let age = (now - self.layers[l].born.load(Ordering::Relaxed)).clamp(0, 4096);
                d.powi(age as i32)
            };
            self.layers[l].gain.store(g.to_bits(), Ordering::Relaxed);
        }
    }
    pub fn chance_of(&self) -> f32 {
        f32::from_bits(self.chance.load(Ordering::Relaxed))
    }
    /// Whether chance has any say over this loop at the moment.
    ///
    /// One function because two things ask: the mixer, which rolls, and the
    /// snapshot, which reports. Written twice they would drift, and the way they
    /// would drift is the quiet one — the display saying a loop is sitting a
    /// pass out while it is audibly overdubbing.
    ///
    /// Never while recording. Overdubbing onto something you cannot hear is a
    /// way to record a mistake twice, which is the same argument that un-stops a
    /// loop before an overdub.
    pub(crate) fn chance_applies(&self) -> bool {
        self.chance_of() < 1.0 && !self.is_recording()
    }
    /// Whether chance is holding this pass back.
    ///
    /// **Reads the decision, never makes it.** The snapshot thread calls this
    /// thirty times a second; rolling here would consume randomness the mixer
    /// was going to use and, worse, would decide passes on whether anybody
    /// happened to be looking. The mixer owns the roll, this only reports it.
    pub fn skipping(&self, out_frame: i64, len: usize) -> bool {
        self.chance_applies()
            && self.chance_pass.load(Ordering::Relaxed) == self.pass_index(out_frame, len)
            && !self.chance_sounds.load(Ordering::Relaxed)
    }
    /// Frames until a scheduled transition fires, or `-1` when nothing is
    /// pending or it has no deadline.
    pub fn pending_in(&self, now: i64) -> i64 {
        self.next.due_in(now)
    }
    pub fn layer_tail(&self, layer: usize) -> usize {
        self.layers[layer].tail()
    }
    pub fn layer_shape(&self, layer: usize) -> (usize, usize, usize) {
        self.layers[layer].shape()
    }
    /// One more layer playing, and the redo ceiling raised to match.
    ///
    /// Together, always: `redo_to` is how far back up the stack still holds
    /// audio, and every path that lands a layer — commit, a retroactive take,
    /// the end of a multiply — is a path where it has just moved. Beside each
    /// increment they would drift, and the failure would be a redo that raised
    /// a layer nobody recorded.
    pub(crate) fn add_layer(&self) {
        let n = self.n_layers.fetch_add(1, Ordering::AcqRel);
        self.redo_to.store(n + 1, Ordering::Release);
    }
    /// See `Layer::set_shape`.
    pub(crate) fn set_layer_shape(&self, layer: usize, s: Shape) {
        self.layers[layer].set_shape(s)
    }
    /// See `Layer::pos`.
    pub(crate) fn layer_pos(&self, layer: usize, pos: usize) -> Option<usize> {
        self.layers[layer].pos(pos)
    }

    /// The layer's own window, or none.
    pub fn layer_window(&self, layer: usize) -> Option<(i64, i64)> {
        self.layers[layer].window()
    }

    /// See `Layer::windowed_pos`.
    pub(crate) fn windowed_pos(&self, layer: usize, pos: usize) -> Option<Option<usize>> {
        self.layers[layer].windowed_pos(pos)
    }
}
