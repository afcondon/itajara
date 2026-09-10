//! **Capture: recording that is not looping.**
//!
//! The Workshop drove the looper with nine verbs and three of them existed
//! purely to turn the looper *off* — `alt0`, `snd0`, `grid0` — with a fourth,
//! `c`, run before every take because a loop holds one take and loop 7 was
//! described in its own comment as "a tape head, not a loop".
//!
//! That is a model mismatch, and on 2026-09-09 it cost a take: a swept run
//! caught five hits of twelve, because **level-arm, a looper concept, leaked
//! into a sampler use** and the quiet end of the sweep never tripped the arm.
//!
//! But the mismatch was in the *engine*, not the *device*. The hard-won part —
//! the aggregate, `--source hits=ES-9:1`, float32, DC handling, twenty-four
//! channels in a stable order — is `aggregate.rs` and knows nothing about
//! loops. So: a capture mode beside `engine/`, sharing the device and the
//! input callback and nothing else. Arm, record to a buffer, stop, write.
//!
//! ## What it deliberately does not have
//!
//! No layers, no phases, no undo, no window, no rotation, no quantise, no
//! alternates, no solo, no grid, no length. A capture has a start and an end
//! and that is the whole of its state. Every one of those absences is a class
//! of bug that cannot arrive uninvited.
//!
//! ## Two things it does have, because the Workshop needs them
//!
//! **A head trim.** Playing a take by hand, you cannot press Record and pick up
//! a stick in the same instant, so a take begins with silence you did not mean.
//! The looper answers that with a level *arm* — it does not record until a
//! sound arrives — and that is what lost the swept take. Here the recording
//! starts the moment you ask and the threshold only decides **where the file
//! begins**, resolved at write time over audio already in hand. A quiet first
//! hit can no longer be missed, because nothing waits for it; at worst the trim
//! is in the wrong place, and the audio it would have cut is still there.
//!
//! **A count.** A take recorded to a bar count closes itself, which is the only
//! way to get a loop whose head and tail are the same moment. Given in frames
//! by the caller, who is the one holding the tempo.
//!
//! ## The buffer
//!
//! Allocated on the first capture and never again — from the *control* thread,
//! because the audio callback may not allocate. `OnceLock::get` in the callback
//! is an atomic load and nothing more, so a daemon that never captures pays
//! nothing for this module at all.

use std::path::Path;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering};

/// Interleaved stereo, like everything else that reaches a file here.
pub const CHANNELS: usize = 2;

pub struct Capture {
    /// How much room a capture has, in frames. Fixed at startup.
    pub cap_frames: usize,
    /// Allocated on first use. See the module note: the callback only ever
    /// *reads* this handle.
    buf: OnceLock<Vec<AtomicU32>>,
    on: AtomicBool,
    /// The input channels this capture reads, resolved from the source when it
    /// started. Held here so the callback needs nothing but this struct.
    ch0: AtomicUsize,
    ch1: AtomicUsize,
    /// Which source, 1-based, for the snapshot to name.
    src: AtomicUsize,
    frames: AtomicUsize,
    /// The buffer filled and the rest of the take was dropped. **Loud, not
    /// silent**: an unattended run that quietly recorded the first six minutes
    /// of a nine-minute grid would produce a set that looks complete.
    full: AtomicBool,
    /// Trim the head to the first sound at write time, or keep it whole.
    armed: AtomicBool,
    /// Stop after this many frames, or zero to run until told.
    stop_at: AtomicUsize,
    /// The extremes seen, so a capture that heard nothing can say so before
    /// anything is written.
    ///
    /// **A pair rather than a peak, because the inputs are DC-coupled.** The
    /// ES-9 rests at about -0.023 — a magnitude of 0.023 where the ear hears
    /// nothing at all — so `max(|v|)` on a silent input reads as signal and a
    /// silence warning built on it could never fire. Half the spread between
    /// the extremes removes the offset exactly and costs one more atomic.
    ///
    /// Seeded to the empty interval rather than to zero, because zero is not a
    /// sample: against a constant -0.023 it would make itself the other
    /// extreme and report half the offset as signal.
    lo: AtomicU32,
    hi: AtomicU32,
}

impl Capture {
    pub fn new(cap_frames: usize) -> Self {
        Capture {
            cap_frames,
            buf: OnceLock::new(),
            on: AtomicBool::new(false),
            ch0: AtomicUsize::new(0),
            ch1: AtomicUsize::new(1),
            src: AtomicUsize::new(0),
            frames: AtomicUsize::new(0),
            full: AtomicBool::new(false),
            armed: AtomicBool::new(false),
            stop_at: AtomicUsize::new(0),
            lo: AtomicU32::new(f32::INFINITY.to_bits()),
            hi: AtomicU32::new(f32::NEG_INFINITY.to_bits()),
        }
    }

    pub fn is_on(&self) -> bool { self.on.load(Ordering::Acquire) }
    pub fn frames(&self) -> usize { self.frames.load(Ordering::Acquire) }
    pub fn source(&self) -> usize { self.src.load(Ordering::Relaxed) }
    pub fn full(&self) -> bool { self.full.load(Ordering::Relaxed) }
    pub fn armed(&self) -> bool { self.armed.load(Ordering::Relaxed) }
    pub fn stop_at(&self) -> usize { self.stop_at.load(Ordering::Relaxed) }
    /// Half the spread between the extremes: the signal, with any DC offset
    /// on the input removed.
    pub fn peak(&self) -> f32 {
        let lo = f32::from_bits(self.lo.load(Ordering::Relaxed));
        let hi = f32::from_bits(self.hi.load(Ordering::Relaxed));
        if hi < lo { 0.0 } else { (hi - lo) * 0.5 }
    }
    /// True once something has been captured and not yet written or dropped.
    pub fn holds(&self) -> bool { !self.is_on() && self.frames() > 0 }

    pub fn set_armed(&self, on: bool) { self.armed.store(on, Ordering::Relaxed) }
    pub fn set_stop_at(&self, frames: usize) { self.stop_at.store(frames, Ordering::Relaxed) }

    /// **Begin.** Called from the control thread; allocates on the first call.
    ///
    /// Whatever was held is discarded, and that is the whole of "clear": a
    /// capture holds one take, and a take that landed on top of another is the
    /// bug this page exists to avoid. There is no undo because there is
    /// nothing a second take could be layered onto.
    pub fn start(&self, src1: usize, ch: [usize; CHANNELS]) {
        self.buf.get_or_init(|| {
            (0..self.cap_frames * CHANNELS).map(|_| AtomicU32::new(0)).collect()
        });
        self.ch0.store(ch[0], Ordering::Relaxed);
        self.ch1.store(ch[1], Ordering::Relaxed);
        self.src.store(src1, Ordering::Relaxed);
        self.frames.store(0, Ordering::Release);
        self.full.store(false, Ordering::Relaxed);
        self.lo.store(f32::INFINITY.to_bits(), Ordering::Relaxed);
        self.hi.store(f32::NEG_INFINITY.to_bits(), Ordering::Relaxed);
        self.on.store(true, Ordering::Release);
    }

    /// **End**, and say how much was caught. Idempotent.
    pub fn stop(&self) -> usize {
        self.on.store(false, Ordering::Release);
        self.frames()
    }

    /// Throw it away. Only from the control thread, and only when nothing is
    /// recording — otherwise the callback would be writing into a length that
    /// had just been reset under it.
    pub fn discard(&self) {
        self.on.store(false, Ordering::Release);
        self.frames.store(0, Ordering::Release);
        self.full.store(false, Ordering::Relaxed);
        self.lo.store(f32::INFINITY.to_bits(), Ordering::Relaxed);
        self.hi.store(f32::NEG_INFINITY.to_bits(), Ordering::Relaxed);
    }

    /// **The audio callback's whole share of this module.**
    ///
    /// A bounds check, a copy, a peak. No locks, no allocation, no branch on
    /// anything but its own flag — so a daemon that is not capturing pays one
    /// atomic load per buffer.
    pub fn take(&self, data: &[f32], in_channels: usize) {
        if !self.on.load(Ordering::Acquire) {
            return;
        }
        let Some(buf) = self.buf.get() else { return };
        let (c0, c1) = (self.ch0.load(Ordering::Relaxed), self.ch1.load(Ordering::Relaxed));
        if c0 >= in_channels || c1 >= in_channels {
            return;
        }
        let at = self.frames.load(Ordering::Acquire);
        let frames = data.len() / in_channels;
        let room = self.cap_frames.saturating_sub(at);
        let n = frames.min(room);
        if n < frames {
            self.full.store(true, Ordering::Relaxed);
        }
        let mut lo = f32::from_bits(self.lo.load(Ordering::Relaxed));
        let mut hi = f32::from_bits(self.hi.load(Ordering::Relaxed));
        for f in 0..n {
            let l = data[f * in_channels + c0];
            let r = data[f * in_channels + c1];
            buf[(at + f) * CHANNELS].store(l.to_bits(), Ordering::Relaxed);
            buf[(at + f) * CHANNELS + 1].store(r.to_bits(), Ordering::Relaxed);
            lo = lo.min(l).min(r);
            hi = hi.max(l).max(r);
        }
        self.lo.store(lo.to_bits(), Ordering::Relaxed);
        self.hi.store(hi.to_bits(), Ordering::Relaxed);
        self.frames.store(at + n, Ordering::Release);
        // **A capture closes itself at its count**, here rather than on a
        // timer, because this is the only place that knows the frame. Zero
        // means run until told, which is every take played by hand.
        let stop = self.stop_at.load(Ordering::Relaxed);
        if stop > 0 && at + n >= stop {
            self.on.store(false, Ordering::Release);
        }
        if n < frames {
            self.on.store(false, Ordering::Release);
        }
    }

    /// One frame, one channel. For writing and drawing, never for the callback.
    pub fn at(&self, frame: usize, ch: usize) -> f32 {
        match self.buf.get() {
            Some(b) if frame < self.cap_frames => {
                f32::from_bits(b[frame * CHANNELS + ch].load(Ordering::Relaxed))
            }
            _ => 0.0,
        }
    }

    /// **Where the file begins.**
    ///
    /// Whole unless a trim was asked for; and even then, resolved over audio
    /// already recorded rather than by refusing to record. `reach` is how far
    /// back from the crossing to start — the crossing is not the start of the
    /// note, and what comes before it is already in hand.
    pub fn head(&self, thresh: f32, reach: usize) -> usize {
        if !self.armed() {
            return 0;
        }
        let n = self.frames();
        for f in 0..n {
            if self.at(f, 0).abs().max(self.at(f, 1).abs()) >= thresh {
                return f.saturating_sub(reach);
            }
        }
        // Nothing crossed. Keeping the whole take is the safe answer: a trim
        // that found no sound has no opinion about where the sound is, and
        // throwing the take away over it would discard the evidence of why.
        0
    }

    /// The picture, in `buckets` pairs of extremes over `from..to`.
    pub fn picture(&self, from: usize, to: usize, buckets: usize) -> (Vec<i32>, Vec<i32>) {
        let total = to.saturating_sub(from);
        let mut lo = Vec::with_capacity(buckets);
        let mut hi = Vec::with_capacity(buckets);
        for b in 0..buckets {
            let a = from + b * total / buckets;
            let z = (from + (b + 1) * total / buckets).max(a + 1).min(to);
            let (mut mn, mut mx) = (0.0f32, 0.0f32);
            for p in a..z {
                for ch in 0..CHANNELS {
                    let v = self.at(p, ch);
                    mn = mn.min(v);
                    mx = mx.max(v);
                }
            }
            lo.push(((mn * 1000.0).round() as i32).clamp(-1000, 1000));
            hi.push(((mx * 1000.0).round() as i32).clamp(-1000, 1000));
        }
        (lo, hi)
    }

    /// **Write it as a take**: one WAV and a `take.json`, so everything
    /// downstream that already reads a take reads this one too.
    ///
    /// Version 1 with a single layer, deliberately: a capture has no layers,
    /// and inventing a shape for it would mean teaching every reader a second
    /// one. `msm`, the Friend's server and the daemon's own reloader all take
    /// this as what it is.
    pub fn write(&self, dir: &Path, sr: u32, from: usize, to: usize) -> Result<usize, String> {
        let len = to.saturating_sub(from);
        if len == 0 {
            return Err("nothing captured, so there is nothing to write.".into());
        }
        if len > crate::wav::MAX_FRAMES {
            return Err("the capture is longer than a WAV can address.".into());
        }
        std::fs::create_dir_all(dir).map_err(|e| format!("could not make {}: {}", dir.display(), e))?;
        let samples: Vec<f32> = (from..to)
            .flat_map(|p| (0..CHANNELS).map(move |ch| (p, ch)))
            .map(|(p, ch)| self.at(p, ch))
            .collect();
        let file = "capture-00.wav";
        std::fs::write(dir.join(file), crate::wav::wav_bytes(&samples, sr, CHANNELS as u16))
            .map_err(|e| format!("could not write {}: {}", file, e))?;
        // No timestamp, for the same reason `save_take` carries none: two
        // captures of identical audio should produce identical bytes.
        let manifest = format!(
            concat!(
                "{{\n  \"version\": 1,\n  \"sampleRate\": {},\n",
                "  \"loopFrames\": {},\n  \"loopSecs\": {:.6},\n  \"layers\": [\n",
                "    {{\"file\":\"{}\",\"len\":{},\"channels\":{},\"period\":1,\"phase\":0}}\n",
                "  ]\n}}\n"
            ),
            sr, len, len as f64 / sr as f64, file, len, CHANNELS
        );
        std::fs::write(dir.join("take.json"), manifest)
            .map_err(|e| format!("wrote the audio but not the manifest: {}", e))?;
        Ok(len)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Four buffers of a two-channel interleave, as the callback would hand
    /// them over: `n` frames, `in_channels` wide, reading channels `ch`.
    fn feed(c: &Capture, in_channels: usize, ch: [usize; 2], frames: &[[f32; 2]]) {
        let mut data = vec![0.0f32; frames.len() * in_channels];
        for (f, v) in frames.iter().enumerate() {
            data[f * in_channels + ch[0]] = v[0];
            data[f * in_channels + ch[1]] = v[1];
        }
        c.take(&data, in_channels);
    }

    /// **A capture reads the channels its source names, and nothing else.**
    ///
    /// The aggregate is twenty-four channels wide in an order that is not
    /// stable across reboots, which is why a source is named rather than
    /// numbered. A capture that read channel 0 because that is where audio
    /// usually is would record the wrong instrument and sound exactly like a
    /// capture that worked.
    #[test]
    fn a_capture_reads_its_source_and_nothing_else() {
        let c = Capture::new(16);
        c.start(4, [6, 7]);
        feed(&c, 24, [6, 7], &[[0.5, -0.25], [0.1, 0.2]]);
        assert_eq!(c.frames(), 2);
        assert_eq!(c.at(0, 0), 0.5);
        assert_eq!(c.at(0, 1), -0.25);
        assert_eq!(c.at(1, 0), 0.1);
    }

    /// **The buffer filling is loud, not silent.**
    ///
    /// An unattended run that quietly recorded the first six minutes of a
    /// nine-minute grid would produce a set that looks complete. So the
    /// capture stops, says it filled, and keeps what it has.
    #[test]
    fn filling_the_buffer_stops_and_says_so() {
        let c = Capture::new(3);
        c.start(1, [0, 1]);
        feed(&c, 2, [0, 1], &[[0.1, 0.1]; 5]);
        assert_eq!(c.frames(), 3, "kept what fitted");
        assert!(c.full(), "and said it filled");
        assert!(!c.is_on(), "and stopped");
    }

    /// **A count closes the capture at its frame**, which is what makes a
    /// take recorded to a bar count a loop rather than nearly one.
    #[test]
    fn a_count_closes_the_capture() {
        let c = Capture::new(100);
        c.start(1, [0, 1]);
        c.set_stop_at(4);
        feed(&c, 2, [0, 1], &[[0.2, 0.2]; 3]);
        assert!(c.is_on(), "three of four is not four");
        feed(&c, 2, [0, 1], &[[0.2, 0.2]; 3]);
        assert!(!c.is_on(), "closed itself at the count");
        assert!(!c.full(), "reaching a count is not filling the buffer");
    }

    /// **The head trim is not a level arm**, and this is the difference that
    /// cost a take to learn.
    ///
    /// A level arm does not record until a sound arrives, so the quiet end of
    /// a sweep never starts it — five hits of twelve. Here everything is
    /// recorded and the threshold only says where the file begins, resolved
    /// afterwards over audio already in hand. Unarmed, the take is whole; and
    /// a threshold nothing crossed keeps the whole take rather than throwing
    /// it away, because a trim that found no sound has no opinion about where
    /// the sound is.
    #[test]
    fn the_head_trim_only_moves_the_start() {
        let c = Capture::new(100);
        c.start(1, [0, 1]);
        let mut frames = vec![[0.0f32, 0.0]; 20];
        frames[10] = [0.6, 0.6];
        feed(&c, 2, [0, 1], &frames);
        assert_eq!(c.frames(), 20, "everything was recorded either way");

        assert_eq!(c.head(0.3, 4), 0, "unarmed, the take is whole");
        c.set_armed(true);
        assert_eq!(c.head(0.3, 4), 6, "the crossing at 10, less the reach of 4");
        assert_eq!(c.head(0.3, 100), 0, "a reach past the start clamps to it");
        assert_eq!(c.head(0.9, 4), 0, "nothing crossed, so the take is kept whole");
    }

    /// **Half the spread, not the peak**, because the inputs are DC-coupled:
    /// an ES-9 at rest sits around -0.023 and `max(|v|)` would call that
    /// signal. A warning that can never fire is worse than none.
    #[test]
    fn silence_is_measured_with_the_offset_removed() {
        let c = Capture::new(100);
        c.start(1, [0, 1]);
        feed(&c, 2, [0, 1], &[[-0.023, -0.023]; 8]);
        assert!(c.peak() < 1.0e-3, "a DC offset is not signal: {}", c.peak());
        feed(&c, 2, [0, 1], &[[0.477, -0.523]; 8]);
        assert!((c.peak() - 0.5).abs() < 1.0e-6, "a 1.0 swing about the offset: {}", c.peak());
    }

    /// Starting again discards what was held. There is nothing a second take
    /// could be layered onto, so this is the whole of "clear".
    #[test]
    fn starting_again_is_the_whole_of_clearing() {
        let c = Capture::new(100);
        c.start(1, [0, 1]);
        feed(&c, 2, [0, 1], &[[0.5, 0.5]; 6]);
        c.stop();
        assert!(c.holds());
        c.start(1, [0, 1]);
        assert_eq!(c.frames(), 0);
        assert!(!c.holds(), "recording, so it holds nothing finished");
    }
}
