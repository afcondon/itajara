//! One layer of a loop: `Layer`, its fields and its own arithmetic.
//!
//! Split out of `Loop` on 2026-09-06 (REVIEW-daemon-debt step 3). A layer
//! has become a clip — its own length, tail, birth, gain, on/off, window,
//! period and phase — and eleven parallel `Vec`s indexed by layer were a
//! struct-of-arrays where an array-of-structs is the honest shape. Every
//! field keeps the atomic type and the orderings its array had, so the audio
//! thread reads exactly what it read before; only the address changed.

use std::sync::atomic::{
    AtomicBool,
    AtomicI64,
    AtomicU32,
    AtomicUsize,
    Ordering,
};
use std::sync::Mutex;

use super::Shape;

/// One layer: a clip with its own read parameters.
pub(crate) struct Layer {
    /// The layer's own length, and where in the cycle it sounds.
    ///
    /// A layer is **not** stretched to fill the loop. It keeps the length it was
    /// recorded at, sounds once every `period` of its own lengths, and sits at
    /// slot `phase` within that period. Playback resolves all three.
    ///
    /// This is what makes two kinds of multiply one mechanism. `period = 1` is
    /// an ordinary layer, repeating every time round — which is what the old
    /// code achieved by copying the audio n times into the longer cycle. Set
    /// `period = 4, phase = 3` and the same bar sounds once in four: `~ ~ ~ B`.
    /// Since nothing was flattened, it can go back, or move, or alternate,
    /// afterwards. Tiling could not: it destroyed the fact that there was a
    /// one-bar thing there at all, which is the same reason a `MidiClip` in
    /// Triggerfish stores every note and bakes in no tempo.
    pub(crate) len: AtomicUsize,
    /// How many frames of *continuation* sit past the layer's end.
    ///
    /// The audio that was played after the loop closed. It is not spare and it
    /// is not rubbish: it is the only material that can make the wrap seamless,
    /// because a crossfade at the loop point needs to know what would have come
    /// next — and what would have come next is exactly what the player kept
    /// playing while the gesture was still being worked out.
    ///
    /// Never sounded. Playback is `pos % len`, so anything past the end does
    /// not exist until something asks for it. *Store everything, flatten late*,
    /// which is the same rule `MidiClip` follows in Triggerfish for the same
    /// reason: the lossy step belongs at the end, where it can be undone.
    pub(crate) tail: AtomicUsize,
    /// The pass this layer was laid on, which is where its decay counts from.
    ///
    /// Per layer rather than per loop, and that is the whole of what makes decay
    /// sound like tape rather than like a fader: new material enters at full
    /// while everything underneath goes on receding from its own beginning. It
    /// is also what a single feedback gain cannot do, because a feedback gain
    /// destroys as it goes and has no idea how old anything is.
    pub(crate) born: AtomicI64,
    /// The layer's envelope, as `ENV_BUCKETS` bytes on the scale
    /// `ENV_FLOOR_DB` describes.
    ///
    /// A mutex rather than atomics because nothing real-time goes near it: the
    /// control thread writes it when a layer's content changes, and the socket
    /// thread reads it to build a snapshot. The audio thread has no business
    /// here at all.
    ///
    /// One mutex per layer, where the loop used to hold one over all of its
    /// layers' envelopes. Nothing ever needed two layers' pictures under one
    /// lock: the writer redraws one layer, the reader copies one layer, and
    /// the clear takes them one at a time — which a snapshot could already
    /// see between, because it locked once per layer.
    pub(crate) env: Mutex<Vec<u8>>,
    /// This layer's decay gain right now, recomputed once per buffer.
    ///
    /// Cached because it only changes at a pass boundary and the mixer runs per
    /// frame. Six loops times eight layers of `powi` once a buffer is nothing;
    /// the same arithmetic per frame would be real.
    pub(crate) gain: AtomicU32,
    /// Whether the layer is in the mix at all.
    ///
    /// **Off is not a gain of zero.** The gain is what decay and the mixer
    /// own; this is a switch the player throws — a layer parked for now, kept
    /// whole, back with one verb. Reset to on whenever the layer is written
    /// fresh, because a new take that arrived silent would be the mute bug
    /// with a new name.
    pub(crate) on: AtomicBool,
    /// A window of the layer's own, in the layer's frames: `[in, out)`, or
    /// `0, 0` for none. A layer with one plays that stretch, coming round
    /// inside the loop's cycle, so six layers of one long take can each be a
    /// different thirteen seconds of it — which is what a granular module's
    /// scene is. The loop's window (`win_in`/`win_out`) is the pedalboard's
    /// and still applies first; this applies inside it. Playback only: the
    /// picture (`pk`) reads the arena raw so the window can be drawn on it.
    pub(crate) win_in: AtomicI64,
    pub(crate) win_out: AtomicI64,
    pub(crate) period: AtomicUsize,
    pub(crate) phase: AtomicUsize,
}

impl Layer {
    pub(crate) fn new() -> Self {
        Layer {
            len: AtomicUsize::new(0),
            tail: AtomicUsize::new(0),
            born: AtomicI64::new(0),
            gain: AtomicU32::new(1.0f32.to_bits()),
            on: AtomicBool::new(true),
            env: Mutex::new(Vec::new()),
            period: AtomicUsize::new(1),
            win_in: AtomicI64::new(0),
            win_out: AtomicI64::new(0),
            phase: AtomicUsize::new(0),
        }
    }

    /// This layer's envelope, or empty when it has none yet.
    pub fn env(&self) -> Vec<u8> {
        self.env
            .lock()
            .map(|e| e.clone())
            .unwrap_or_default()
    }
    pub fn gain(&self) -> f32 {
        f32::from_bits(self.gain.load(Ordering::Relaxed))
    }

    pub fn on(&self) -> bool {
        self.on.load(Ordering::Relaxed)
    }

    /// The pass this layer was laid on. Reported so a quiet layer can say why
    /// it is quiet: `gain` alone shows that it has receded and not how far back
    /// it started, and the difference between "born three passes ago" and "born
    /// with the loop" is the whole of what per-layer decay means.
    pub fn born(&self) -> i64 {
        self.born.load(Ordering::Relaxed)
    }
    /// How much continuation this layer holds past its end, for a crossfade.
    pub fn tail(&self) -> usize {
        self.tail.load(Ordering::Acquire)
    }
    pub fn shape(&self) -> (usize, usize, usize) {
        (
            self.len.load(Ordering::Relaxed),
            self.period.load(Ordering::Relaxed).max(1),
            self.phase.load(Ordering::Relaxed),
        )
    }
    /// A freshly committed layer: its own length, sounding every time round.
    ///
    /// Written *before* `n_layers` is incremented everywhere it is used. The
    /// output callback plays `0..n_layers`, so publishing the layer first and
    /// its length second leaves a window in which the mix reads a length of
    /// zero and drops it — a buffer of silence at the exact moment a take
    /// lands, which is the least forgivable place for one.
    /// Declare what a layer is: its length, its continuation, and when it was
    /// born.
    ///
    /// **The tail is a parameter rather than something left alone**, because it
    /// is now read at playback and a stale one is audible. `take` and the
    /// multiply family write a layer without a continuation and used to leave
    /// whatever the slot held before; the samples there had been zeroed, so the
    /// wrap would have crossfaded into silence — a loop fading in from nothing
    /// every cycle, for a reason nothing on screen could explain.
    pub(crate) fn set_shape(&self, s: Shape) {
        self.len.store(s.len, Ordering::Release);
        self.period.store(1, Ordering::Release);
        self.phase.store(0, Ordering::Release);
        self.tail.store(s.tail, Ordering::Release);
        self.born.store(s.born, Ordering::Release);
        self.gain.store(1.0f32.to_bits(), Ordering::Release);
        self.on.store(true, Ordering::Release);
    }
    /// Where in the layer's own buffer the loop position `pos` falls — or `None`
    /// when the layer is silent there.
    ///
    /// Called once per layer per frame in the output callback, so the dense case
    /// skips the division: a layer at `period = 1` sounds everywhere, and asking
    /// which slot it is in has no answer worth computing.
    pub(crate) fn pos(&self, pos: usize) -> Option<usize> {
        let len = self.len.load(Ordering::Relaxed);
        if len == 0 {
            return None;
        }
        let period = self.period.load(Ordering::Relaxed).max(1);
        if period > 1 {
            let slot = (pos / len) % period;
            if slot != self.phase.load(Ordering::Relaxed) % period {
                return None;
            }
        }
        Some(pos % len)
    }

    /// The layer's own window, or none.
    pub fn window(&self) -> Option<(i64, i64)> {
        let i = self.win_in.load(Ordering::Relaxed);
        let o = self.win_out.load(Ordering::Relaxed);
        if o > i { Some((i, o)) } else { None }
    }

    /// Where a windowed layer is read at cycle position `pos`: its window's
    /// start plus the position inside the window's span, coming round; or
    /// nowhere, where the window reaches past the layer's audio. `None` when
    /// the layer has no window — the caller falls back to `pos`.
    pub(crate) fn windowed_pos(&self, pos: usize) -> Option<Option<usize>> {
        let (i, o) = self.window()?;
        let len = self.len.load(Ordering::Relaxed) as i64;
        let span = (o - i).max(1);
        let p = i + (pos as i64).rem_euclid(span);
        Some(if p >= 0 && p < len { Some(p as usize) } else { None })
    }
}
