//! **Jam: a long recording that goes to disk as it is made.**
//!
//! A capture (`crate::capture`) holds its whole take in memory and writes it
//! at the end, which is right for a sweep: it is minutes long, and Quadrat
//! trims its head, counts it to a bar and draws it, all over audio in hand.
//! A jam is the other shape: an hour of the modular, a rehearsal, anything
//! recorded the way Ableton records two channels. Held in memory that would be
//! 1.4 GB an hour, and a daemon that died at minute fifty would lose all of it.
//!
//! So the audio callback writes into a **ring** (`RING_SECS`), and a thread of
//! the jam's own drains it to a WAV several times a second. The header is
//! rewritten every couple of seconds, so if the daemon dies the file on disk is
//! a valid WAV of everything up to the last rewrite.
//!
//! ## What it does not have
//!
//! No head trim, no count, no bar start, no picture: those are a capture's,
//! for sweeps. A jam has a source, a name, a start and a stop.
//!
//! ## What it says loudly
//!
//! - **`dropped`**: frames the callback could not put in the ring because the
//!   writer had fallen a whole ring behind. They are lost, and the file is that
//!   much shorter than the time that passed. It should never happen; if it
//!   does, the snapshot says so for the rest of the jam.
//! - **`full`**: the file reached what a WAV can address (about three hours of
//!   stereo float at 48 kHz) and the jam stopped itself.
//!
//! The callback's share is the same as a capture's: one atomic load when no
//! jam is running, and a copy when one is. It never allocates, never locks and
//! never touches the file.

use std::fs::File;
use std::io::{Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread::JoinHandle;
use std::time::Duration;

/// Interleaved stereo, like every other file this daemon writes.
pub const CHANNELS: usize = 2;

/// How far the writer may fall behind before frames are lost. Thirty seconds
/// is about 11 MB of float stereo at 48 kHz, allocated on the first jam.
pub const RING_SECS: f64 = 30.0;

/// How often the writer drains the ring.
const DRAIN_MS: u64 = 100;

/// Drains between header rewrites: two seconds.
const HEADER_EVERY: u32 = 20;

/// The file the audio goes in, inside the take's directory.
pub const FILE: &str = "jam-00.wav";

/// What the callback and the writer share.
struct Ring {
    buf: OnceLock<Vec<AtomicU32>>,
    /// Capacity, in frames.
    cap: usize,
    on: AtomicBool,
    ch0: AtomicUsize,
    ch1: AtomicUsize,
    /// Which source, 1-based, for the snapshot to name.
    src: AtomicUsize,
    /// Frames the callback has put in, since this jam began. Only the callback
    /// writes it.
    written: AtomicUsize,
    /// Frames the writer has taken out. Only the writer writes it.
    drained: AtomicUsize,
    dropped: AtomicUsize,
    full: AtomicBool,
}

/// The writer thread of the jam that is running, and where it writes.
struct Running {
    dir: PathBuf,
    handle: JoinHandle<Result<usize, String>>,
}

pub struct Jam {
    ring: Arc<Ring>,
    running: Mutex<Option<Running>>,
    /// The name the next jam will take, if one was given (`jname`).
    next_name: Mutex<String>,
    /// The name of the jam running, or of the last one.
    name: Mutex<String>,
}

impl Jam {
    pub fn new(sample_rate: f64) -> Self {
        Jam {
            ring: Arc::new(Ring {
                buf: OnceLock::new(),
                cap: (RING_SECS * sample_rate).round() as usize,
                on: AtomicBool::new(false),
                ch0: AtomicUsize::new(0),
                ch1: AtomicUsize::new(1),
                src: AtomicUsize::new(0),
                written: AtomicUsize::new(0),
                drained: AtomicUsize::new(0),
                dropped: AtomicUsize::new(0),
                full: AtomicBool::new(false),
            }),
            running: Mutex::new(None),
            next_name: Mutex::new(String::new()),
            name: Mutex::new(String::new()),
        }
    }

    pub fn is_on(&self) -> bool { self.ring.on.load(Ordering::Acquire) }
    pub fn source(&self) -> usize { self.ring.src.load(Ordering::Relaxed) }
    pub fn frames(&self) -> usize { self.ring.written.load(Ordering::Acquire) }
    /// Frames recorded and not yet on disk.
    pub fn behind(&self) -> usize {
        self.frames().saturating_sub(self.ring.drained.load(Ordering::Acquire))
    }
    pub fn dropped(&self) -> usize { self.ring.dropped.load(Ordering::Relaxed) }
    pub fn full(&self) -> bool { self.ring.full.load(Ordering::Relaxed) }
    pub fn name(&self) -> String { self.name.lock().map(|n| n.clone()).unwrap_or_default() }
    /// A writer has not been collected: a jam is running, or has stopped and
    /// `finish` has not run yet.
    pub fn busy(&self) -> bool { self.running.lock().map(|r| r.is_some()).unwrap_or(true) }

    /// Name the next jam.
    pub fn set_next_name(&self, name: &str) {
        if let Ok(mut n) = self.next_name.lock() {
            *n = name.to_string();
        }
    }

    /// The name the next jam will take: the one given, or `jam-<seconds>`.
    pub fn take_next_name(&self) -> String {
        let given = self.next_name.lock().map(|mut n| std::mem::take(&mut *n)).unwrap_or_default();
        if given.is_empty() {
            "jam-".to_string()
                + &std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs())
                    .unwrap_or(0)
                    .to_string()
        } else {
            given
        }
    }

    /// **Begin**, into `dir/FILE`. From the control thread: allocates the ring
    /// on the first jam and starts the writer. Refuses a directory that exists,
    /// because a jam is never written over another take.
    pub fn start(&self, name: &str, dir: &Path, src1: usize, ch: [usize; CHANNELS], sr: u32) -> Result<(), String> {
        let mut running = self.running.lock().map_err(|_| "the jam's state is poisoned".to_string())?;
        if running.is_some() {
            return Err("a jam is already running; `jend` it first.".into());
        }
        if dir.exists() {
            return Err(format!("there is already a take called {}; name the jam with `jname`.", name));
        }
        std::fs::create_dir_all(dir).map_err(|e| format!("could not make {}: {}", dir.display(), e))?;
        let r = &self.ring;
        r.buf.get_or_init(|| (0..r.cap * CHANNELS).map(|_| AtomicU32::new(0)).collect());
        r.ch0.store(ch[0], Ordering::Relaxed);
        r.ch1.store(ch[1], Ordering::Relaxed);
        r.src.store(src1, Ordering::Relaxed);
        r.written.store(0, Ordering::Release);
        r.drained.store(0, Ordering::Release);
        r.dropped.store(0, Ordering::Relaxed);
        r.full.store(false, Ordering::Relaxed);
        let path = dir.join(FILE);
        let ring = self.ring.clone();
        // The file is opened here, before the callback is told, so a disk that
        // refuses it refuses the jam rather than a recording already running.
        let file = File::create(&path).map_err(|e| format!("could not make {}: {}", path.display(), e))?;
        r.on.store(true, Ordering::Release);
        let handle = std::thread::Builder::new()
            .name("jam-writer".into())
            .spawn(move || drain(ring, file, sr))
            .map_err(|e| {
                self.ring.on.store(false, Ordering::Release);
                format!("could not start the writer: {}", e)
            })?;
        *running = Some(Running { dir: dir.to_path_buf(), handle });
        if let Ok(mut n) = self.name.lock() {
            *n = name.to_string();
        }
        Ok(())
    }

    /// **Stop.** The callback stops at once; the writer finishes what is in
    /// the ring. `finish` collects it.
    pub fn stop(&self) {
        self.ring.on.store(false, Ordering::Release);
    }

    /// Wait for the writer, and write `take.json` beside the audio. For the
    /// slow thread: the writer drains what is left, which takes at most one
    /// drain interval plus the disk.
    pub fn finish(&self, sr: u32) -> Result<(PathBuf, usize), String> {
        let run = self
            .running
            .lock()
            .map_err(|_| "the jam's state is poisoned".to_string())?
            .take()
            .ok_or_else(|| "no jam to finish.".to_string())?;
        let frames = run.handle.join().map_err(|_| "the jam's writer panicked".to_string())??;
        let manifest = format!(
            concat!(
                "{{\n  \"version\": 1,\n  \"sampleRate\": {},\n",
                "  \"loopFrames\": {},\n  \"loopSecs\": {:.6},\n  \"layers\": [\n",
                "    {{\"file\":\"{}\",\"len\":{},\"channels\":{},\"period\":1,\"phase\":0}}\n",
                "  ]\n}}\n"
            ),
            sr, frames, frames as f64 / sr as f64, FILE, frames, CHANNELS
        );
        std::fs::write(run.dir.join("take.json"), manifest)
            .map_err(|e| format!("wrote the audio but not the manifest: {}", e))?;
        Ok((run.dir, frames))
    }

    /// **The audio callback's whole share.** A copy into the ring, or nothing.
    pub fn take(&self, data: &[f32], in_channels: usize) {
        let r = &*self.ring;
        if !r.on.load(Ordering::Acquire) {
            return;
        }
        let Some(buf) = r.buf.get() else { return };
        let (c0, c1) = (r.ch0.load(Ordering::Relaxed), r.ch1.load(Ordering::Relaxed));
        if in_channels == 0 || c0 >= in_channels || c1 >= in_channels {
            return;
        }
        let frames = data.len() / in_channels;
        let at = r.written.load(Ordering::Relaxed);
        let room = r.cap.saturating_sub(at - r.drained.load(Ordering::Acquire).min(at));
        let limit = crate::wav::MAX_FRAMES / CHANNELS;
        let left = limit.saturating_sub(at);
        let mut n = frames.min(room);
        if n < frames {
            r.dropped.fetch_add(frames - n, Ordering::Relaxed);
        }
        if left <= n {
            n = left;
            r.full.store(true, Ordering::Relaxed);
            r.on.store(false, Ordering::Release);
        }
        for f in 0..n {
            let p = (at + f) % r.cap;
            buf[p * CHANNELS].store(data[f * in_channels + c0].to_bits(), Ordering::Relaxed);
            buf[p * CHANNELS + 1].store(data[f * in_channels + c1].to_bits(), Ordering::Relaxed);
        }
        r.written.store(at + n, Ordering::Release);
    }
}

/// The writer: drain the ring into the file until the jam stops and the ring
/// is empty, rewriting the header as it goes.
fn drain(r: Arc<Ring>, mut file: File, sr: u32) -> Result<usize, String> {
    let io = |e: std::io::Error| format!("the jam's file: {}", e);
    let header = crate::wav::wav_bytes(&[], sr, CHANNELS as u16);
    let at = |tag: &[u8]| header.windows(4).position(|w| w == tag).map(|i| i as u64 + 4);
    let (fact_at, data_at) = match (at(b"fact"), at(b"data")) {
        // `fact`'s size field, then its frame count; `data`'s size field.
        (Some(f), Some(d)) => (f + 4, d),
        _ => return Err("the WAV header has no fact or data chunk".into()),
    };
    file.write_all(&header).map_err(io)?;
    let buf = r.buf.get().ok_or("the jam's ring was never made")?;
    let mut done = 0usize;
    let mut ticks = 0u32;
    let mut bytes: Vec<u8> = Vec::new();
    let mut stopping = false;
    loop {
        // Read `on` before `written`: a stop seen here means every frame the
        // callback will ever write is either counted below or in the one
        // buffer it was writing when told, which the extra pass collects.
        let last_pass = stopping;
        stopping = !r.on.load(Ordering::Acquire);
        let upto = r.written.load(Ordering::Acquire);
        if upto > done {
            bytes.clear();
            for p in done..upto {
                let i = (p % r.cap) * CHANNELS;
                for ch in 0..CHANNELS {
                    bytes.extend_from_slice(&f32::from_bits(buf[i + ch].load(Ordering::Relaxed)).to_le_bytes());
                }
            }
            file.write_all(&bytes).map_err(io)?;
            done = upto;
            r.drained.store(done, Ordering::Release);
        }
        ticks = ticks.wrapping_add(1);
        if ticks % HEADER_EVERY == 0 {
            patch(&mut file, done, fact_at, data_at).map_err(io)?;
        }
        if last_pass && stopping {
            break;
        }
        std::thread::sleep(Duration::from_millis(DRAIN_MS));
    }
    patch(&mut file, done, fact_at, data_at).map_err(io)?;
    file.sync_all().map_err(io)?;
    Ok(done)
}

/// Make the header true of `frames` frames: the RIFF size, `fact`'s frame
/// count and `data`'s size. Then back to the end, where the audio goes.
fn patch(file: &mut File, frames: usize, fact_at: u64, data_at: u64) -> std::io::Result<()> {
    let data_len = (frames * CHANNELS * 4) as u32;
    let riff_len = (data_at as u32 + 4 - 8) + data_len;
    file.seek(SeekFrom::Start(4))?;
    file.write_all(&riff_len.to_le_bytes())?;
    file.seek(SeekFrom::Start(fact_at))?;
    file.write_all(&(frames as u32).to_le_bytes())?;
    file.seek(SeekFrom::Start(data_at))?;
    file.write_all(&data_len.to_le_bytes())?;
    file.seek(SeekFrom::End(0))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("itajara-jam-{}-{}", tag, std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        d
    }

    /// Four channels in, channels 2 and 3 taken, `n` frames of a ramp.
    fn buffer(n: usize, from: usize) -> Vec<f32> {
        (0..n).flat_map(|f| {
            let v = (from + f) as f32 / 1000.0;
            [9.0, 9.0, v, -v]
        }).collect()
    }

    #[test]
    fn a_jam_writes_its_source_to_a_valid_wav_and_a_take() {
        let dir = temp_dir("valid");
        let jam = Jam::new(1000.0);
        jam.start("t", &dir, 2, [2, 3], 1000).expect("started");
        for k in 0..10 {
            jam.take(&buffer(50, k * 50), 4);
        }
        jam.stop();
        let (_, frames) = jam.finish(1000).expect("finished");
        assert_eq!(frames, 500);
        let bytes = std::fs::read(dir.join(FILE)).unwrap();
        let header = crate::wav::wav_bytes(&[], 1000, 2);
        assert_eq!(bytes.len(), header.len() + 500 * 2 * 4, "header plus the audio, nothing else");
        // RIFF size covers everything after its own field.
        assert_eq!(u32::from_le_bytes(bytes[4..8].try_into().unwrap()) as usize, bytes.len() - 8);
        let sample = |frame: usize, ch: usize| {
            let o = header.len() + (frame * 2 + ch) * 4;
            f32::from_le_bytes(bytes[o..o + 4].try_into().unwrap())
        };
        assert_eq!(sample(0, 0), 0.0);
        assert_eq!(sample(499, 0), 0.499, "channel 2 of the input, in order");
        assert_eq!(sample(499, 1), -0.499, "channel 3 of the input");
        assert!(std::fs::read_to_string(dir.join("take.json")).unwrap().contains("\"loopFrames\": 500"));
        assert_eq!(jam.dropped(), 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_jam_refuses_a_take_that_exists() {
        let dir = temp_dir("exists");
        std::fs::create_dir_all(&dir).unwrap();
        let jam = Jam::new(1000.0);
        assert!(jam.start("t", &dir, 1, [0, 1], 1000).is_err());
        assert!(!jam.is_on());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_ring_the_writer_has_not_drained_drops_and_says_so() {
        // The ring holds RING_SECS at 10 Hz: 300 frames. Fill it faster than
        // the writer's first drain can empty it.
        let dir = temp_dir("drops");
        let jam = Jam::new(10.0);
        jam.start("t", &dir, 1, [0, 1], 10).expect("started");
        let big: Vec<f32> = vec![0.25; 1000 * 2];
        jam.take(&big, 2);
        assert_eq!(jam.frames(), 300, "the ring's worth went in");
        assert_eq!(jam.dropped(), 700, "and the rest is counted, not hidden");
        jam.stop();
        let (_, frames) = jam.finish(10).expect("finished");
        assert_eq!(frames, 300);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
