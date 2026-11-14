//! **Pitch: what note is this input playing, right now.**
//!
//! For tuning an oscillator before a calibration sweep (Quadrat's Calibrate
//! page), where a reading every couple of seconds is no use to a hand on a
//! knob. A surface asks for one source to be listened to (`tune<n>`), and
//! every snapshot then carries that source's pitch.
//!
//! **Nothing here touches the audio callback.** The engine already keeps a
//! ring of every source, always (`Shared::ring`), written by the callback
//! for its own reasons. A thread of its own reads the newest stretch of the
//! chosen source out of that ring twenty times a second, finds its pitch,
//! and leaves the answer in two atomics for the snapshot to read. If this
//! thread were slow, or wrong, or panicked, the audio would not know.
//!
//! The detector is the McLeod Pitch Method, with the window and thresholds
//! `msm` uses to read a calibration sweep from its recording
//! (`SamplesProject/msm/src/audio/pitch.rs`), so the note shown while tuning
//! is the note the sweep will measure. Both came from DeepStar's probe.
//!
//! **A detector asked about an inharmonic signal does not fail; it answers.**
//! This reports a reading. Believing it is the surface's business.

use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use pitch_detection::detector::mcleod::McLeodDetector;
use pitch_detection::detector::PitchDetector;

use crate::engine::{Shared, CHANNELS};

/// The stretch one reading analyses: about 341 ms at 48 kHz, which holds
/// eleven periods of C1. The same as `msm`'s.
pub const WINDOW: usize = 16384;
/// Near-silence is not a pitch.
const POWER_THRESHOLD: f64 = 5.0;
/// How periodic the window must be, 0 to 1.
const CLARITY_THRESHOLD: f64 = 0.6;
/// How often the chosen source is read.
const EVERY: Duration = Duration::from_millis(50);

/// Which source is being listened to, and what was last heard.
pub struct Tap {
    /// 1-based, or zero for none.
    src: AtomicUsize,
    /// The pitch in Hz as bits, zero for none found.
    hz: AtomicU64,
    /// How periodic the window was, 0 to 1, as bits.
    clarity: AtomicU64,
}

impl Tap {
    pub fn new() -> Self {
        Tap { src: AtomicUsize::new(0), hz: AtomicU64::new(0), clarity: AtomicU64::new(0) }
    }

    /// Listen to a source (1-based), or to none (0). The last reading is
    /// forgotten: it was of something else.
    pub fn listen(&self, src: usize) {
        self.src.store(src, Ordering::Release);
        self.set(0.0, 0.0);
    }

    pub fn source(&self) -> usize {
        self.src.load(Ordering::Acquire)
    }

    /// The last pitch found and how periodic it was; zeros if none.
    pub fn reading(&self) -> (f64, f64) {
        (f64::from_bits(self.hz.load(Ordering::Relaxed)), f64::from_bits(self.clarity.load(Ordering::Relaxed)))
    }

    fn set(&self, hz: f64, clarity: f64) {
        self.hz.store(hz.to_bits(), Ordering::Relaxed);
        self.clarity.store(clarity.to_bits(), Ordering::Relaxed);
    }
}

/// The fundamental of a mono signal and how periodic it was, or nothing if
/// it is too quiet or not periodic enough.
pub fn detect(signal: &[f64], sample_rate: usize) -> Option<(f64, f64)> {
    let size = signal.len();
    if size < 256 || sample_rate == 0 {
        return None;
    }
    let mut detector = McLeodDetector::<f64>::new(size, size / 2);
    detector
        .get_pitch(signal, sample_rate, POWER_THRESHOLD, CLARITY_THRESHOLD)
        .map(|pitch| (pitch.frequency as f64, pitch.clarity as f64))
}

/// The newest `WINDOW` frames of a source, mixed to one channel with their
/// mean removed (the ES-9's inputs are DC-coupled). `None` until the ring
/// has that much in it.
fn newest(sh: &Shared, src: usize) -> Option<Vec<f64>> {
    let head = sh.in_frames.load(Ordering::Acquire);
    if head < WINDOW || sh.ring_len < WINDOW || src == 0 || src > sh.sources.len() {
        return None;
    }
    let source = src - 1;
    let mut window = Vec::with_capacity(WINDOW);
    for frame in head - WINDOW..head {
        let at = (source * sh.ring_len + frame % sh.ring_len) * CHANNELS;
        let mut sum = 0.0f64;
        for channel in 0..CHANNELS {
            sum += f32::from_bits(sh.ring[at + channel].load(Ordering::Relaxed)) as f64;
        }
        window.push(sum / CHANNELS as f64);
    }
    let mean = window.iter().sum::<f64>() / WINDOW as f64;
    for sample in window.iter_mut() {
        *sample -= mean;
    }
    Some(window)
}

/// Start the listening thread. It does nothing but sleep until a source is
/// chosen.
pub fn spawn(sh: Arc<Shared>, sample_rate: u32) {
    std::thread::Builder::new()
        .name("pitch".into())
        .spawn(move || loop {
            let src = sh.pitch.source();
            if src == 0 {
                std::thread::sleep(Duration::from_millis(200));
                continue;
            }
            match newest(&sh, src).and_then(|window| detect(&window, sample_rate as usize)) {
                // Only if the choice has not changed while this was being
                // worked out: a reading of the last source is not this one's.
                Some((hz, clarity)) if sh.pitch.source() == src => sh.pitch.set(hz, clarity),
                Some(_) => {}
                None => sh.pitch.set(0.0, 0.0),
            }
            std::thread::sleep(EVERY);
        })
        .expect("the pitch thread");
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cents(measured: f64, reference: f64) -> f64 {
        1200.0 * (measured / reference).log2()
    }

    fn sine(hz: f64, sr: usize, n: usize) -> Vec<f64> {
        (0..n).map(|i| (2.0 * std::f64::consts::PI * hz * (i as f64) / (sr as f64)).sin()).collect()
    }

    #[test]
    fn sines_are_read_within_five_cents() {
        for &hz in &[65.41, 110.0, 261.63, 440.0, 1000.0, 2093.0] {
            let (found, clarity) = detect(&sine(hz, 48000, WINDOW), 48000).expect("a pitch");
            assert!(cents(found, hz).abs() < 5.0, "sine {hz} Hz read as {found}");
            assert!(clarity > 0.9, "clarity {clarity}");
        }
    }

    #[test]
    fn a_saw_is_not_read_an_octave_out() {
        let saw: Vec<f64> = (0..WINDOW).map(|i| 2.0 * (196.0 * (i as f64) / 48000.0).fract() - 1.0).collect();
        let (found, _) = detect(&saw, 48000).expect("a pitch");
        assert!(cents(found, 196.0).abs() < 10.0, "saw 196 Hz read as {found}");
    }

    /// How steadily a perfectly steady tone reads, as the window slides
    /// along it the way the listening thread's does. This is the detector's
    /// own wander: anything beyond it on the needle is the oscillator (or
    /// the voltage driving it) moving, not the reading.
    #[test]
    fn a_steady_tone_reads_steadily() {
        let sr = 48000usize;
        for &hz in &[65.178f64, 261.63, 1046.5] {
            let long: Vec<f64> = (0..WINDOW + 60 * 2400).map(|i| 2.0 * (hz * (i as f64) / (sr as f64)).fract() - 1.0).collect();
            let readings: Vec<f64> = (0..60).map(|step| detect(&long[step * 2400..step * 2400 + WINDOW], sr).expect("a pitch").0).collect();
            let low = readings.iter().cloned().fold(f64::INFINITY, f64::min);
            let high = readings.iter().cloned().fold(0.0, f64::max);
            let range = cents(high, low);
            println!("saw {hz} Hz: {low:.4} to {high:.4} Hz, {range:.3} cents over 60 windows");
            assert!(range < 0.5, "a steady {hz} Hz saw wandered {range:.3} cents");
        }
    }

    #[test]
    fn silence_is_not_a_pitch() {
        assert!(detect(&vec![0.0; WINDOW], 48000).is_none());
    }

    #[test]
    fn choosing_another_source_forgets_the_reading() {
        let tap = Tap::new();
        tap.listen(2);
        tap.set(440.0, 0.98);
        assert_eq!(tap.reading(), (440.0, 0.98));
        tap.listen(3);
        assert_eq!(tap.source(), 3);
        assert_eq!(tap.reading(), (0.0, 0.0));
    }
}
