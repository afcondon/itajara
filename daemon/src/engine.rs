//! The looper proper: transport, layers, record, overdub, undo.
//!
//! Built on the arithmetic `align` proves. Two rules carry over and neither is
//! negotiable:
//!
//! - **Loop position is a device frame count**, never a host-clock instant. The
//!   two clocks differ by ~15.6 ppm here (DESIGN-LOOPER §10) and anything
//!   derived from the host clock walks away from the audio at 0.75 samples a
//!   second.
//! - **`out_frame = in_frame + K`**, with `K` established once at the first
//!   input callback and never recomputed. After that it is integer addition.
//!
//! ## Layers, not mixdown
//!
//! Every overdub is its own buffer, summed at playback (§4). Undo is then free,
//! and so is muting, reversing or re-rendering one layer while the rest play.
//!
//! ## How memory is handled, and why it looks odd
//!
//! Audio callbacks must not allocate, and two callbacks need access to the same
//! layer storage — the input side writes the layer being recorded while the
//! output side reads the ones already committed. The usual answers are unsafe
//! aliasing or a lock-free handoff of buffer ownership.
//!
//! Instead every sample is an `AtomicU32` holding f32 bits, accessed `Relaxed`.
//! On any machine this runs on that compiles to exactly the same load and store
//! as a plain `f32` — the atomics buy the absence of undefined behaviour, not
//! synchronisation, and cost nothing. The whole arena is allocated once at
//! startup, so no callback ever touches the allocator.
//!
//! The price is a fixed ceiling on loop length and layer count, which is what
//! `--max-secs` and `MAX_LAYERS` are. At the defaults the arena is 46 MB.

use cpal::traits::{DeviceTrait, StreamTrait};
use std::error::Error;
use std::io::BufRead;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU32, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::measure::{choose_input, choose_output, signed_secs, Width};

pub const MAX_LAYERS: usize = 8;

/// Transport states, as a `u8` because the audio thread reads it every buffer.
const IDLE: u8 = 0;
/// Waiting for the output callback to stamp the exact frame recording begins.
const ARMED: u8 = 1;
/// Recording the first loop: linear, and its length becomes the cycle.
const FIRST: u8 = 2;
/// Recording an overdub: modular, into a buffer one cycle long.
const OVERDUB: u8 = 3;
/// Playing, not recording.
const PLAYING: u8 = 4;
/// Recording across several cycles, to make the loop an integer multiple longer
/// with what is already there repeating underneath. The EDP's `Multiply`.
const MULTIPLY: u8 = 5;

pub struct Opts {
    pub device: String,
    pub in_ch: usize,
    pub out_ch: usize,
    pub residual: f64,
    pub max_secs: f64,
    pub sample_rate: u32,
    pub buffer: Option<u32>,
    pub click: bool,
    pub selftest: Option<f64>,
    pub ring_secs: f64,
    /// How far before the press the first recording actually begins, pulled
    /// from the ring. A tap is always a little late; this makes that harmless
    /// instead of clipping the attack off the front of the loop.
    pub preroll_ms: f64,
    /// Send the mix to `out_ch` and `out_ch + 1` rather than one channel. On by
    /// default: monitors are a pair, and a loop in one ear is not a loop you can
    /// judge.
    pub dual: bool,
    /// Pass the live input through to the output. Off by default because the
    /// interface's own direct monitoring is strictly better — it costs no
    /// latency, where this costs the round trip plus a buffer. Useful on
    /// headphones with nothing else in the room.
    pub monitor: bool,
    /// TCP port for the app to connect on. None keeps the daemon console-only.
    pub ws_port: Option<u16>,
}

impl Default for Opts {
    fn default() -> Self {
        Opts {
            device: String::new(),
            in_ch: 0,
            out_ch: 0,
            residual: 252.0,
            max_secs: 30.0,
            sample_rate: 48_000,
            buffer: None,
            click: false,
            selftest: None,
            ring_secs: 60.0,
            preroll_ms: 0.0,
            dual: true,
            monitor: false,
            ws_port: None,
        }
    }
}

/// Everything both callbacks and the control thread touch.
pub struct Shared {
    arena: Vec<AtomicU32>,
    max_frames: usize,
    /// The pre-roll. The input callback writes every frame it ever receives here
    /// whether anything is recording or not, indexed by input frame modulo its
    /// length — so the last `ring_secs` of playing are always retrievable.
    ///
    /// This is the thing a pedal cannot do. Sixty seconds is 11 MB; a 720 has
    /// no such memory to spare and so must be told to record *before* the good
    /// bit happens. Here the good bit can be claimed afterwards.
    ring: Vec<AtomicU32>,
    ring_len: usize,
    pub loop_len: AtomicUsize,
    pub n_layers: AtomicUsize,
    /// The output frame at which loop position zero sits.
    pub origin: AtomicI64,
    state: AtomicU8Wrapper,
    /// Set by the control thread, consumed by the output callback, which is the
    /// only place a transition can be stamped to an exact frame.
    request: AtomicU8Wrapper,
    pub out_frames: AtomicUsize,
    in_frames: AtomicUsize,
    pub k: AtomicI64,
    pub k_set: AtomicBool,
    pub p0: Mutex<Option<cpal::StreamInstant>>,
    buffer_frames: AtomicU32,
    pub click: AtomicBool,
    /// Highest position the first recording reached, so a loop can be closed at
    /// the right length even though the input trails the output.
    reached: AtomicUsize,
    overflowed: AtomicBool,
    preroll: AtomicUsize,
    /// Output frame at which the layer being recorded has its position zero.
    /// Equal to `origin` for a first recording; for a multiply it is the cycle
    /// boundary the multiply started on, which is also where the *new* loop's
    /// position zero will end up.
    rec_from: AtomicI64,
    pub monitor: AtomicBool,
    pub out_peak: AtomicU32,
    pub in_peak: AtomicU32,
    /// Latched by cpal's stream error callback. Unplugging the USB bus kills
    /// both streams, and until this existed the daemon carried on serving a
    /// confident socket from a dead engine: `r` set the request, no output
    /// callback ever consumed it, and the state sat at `idle` for ever. The
    /// only tell was two meters reading digital zero.
    /// Ask the output callback to stamp a fresh `p0`. Set at startup and again
    /// after every reopen — `p0` used to be taken only when `out_frames` was
    /// zero, which meant that after a recovery it could never be retaken, `K`
    /// could never be recomputed, and every subsequent recording silently wrote
    /// nothing at all.
    pub p0_needed: AtomicBool,
    /// The output frame `p0` was stamped at. Zero at startup, which is why the
    /// original arithmetic could get away without it; not zero after a reopen.
    pub p0_frame: AtomicUsize,
    pub device_lost: AtomicBool,
    /// How many times the device has been reopened. Worth surfacing rather
    /// than hiding — a rig that silently recovers six times in a session is
    /// telling you something about the cable.
    pub reopens: AtomicUsize,
}

/// `AtomicU8` under a name that makes the intent obvious at the use sites.
struct AtomicU8Wrapper(std::sync::atomic::AtomicU8);
impl AtomicU8Wrapper {
    fn new(v: u8) -> Self {
        AtomicU8Wrapper(std::sync::atomic::AtomicU8::new(v))
    }
    fn get(&self) -> u8 {
        self.0.load(Ordering::Acquire)
    }
    fn set(&self, v: u8) {
        self.0.store(v, Ordering::Release)
    }
    fn take(&self) -> u8 {
        self.0.swap(0, Ordering::AcqRel)
    }
}

impl Shared {
    pub fn state_name(&self) -> &'static str {
        match self.state.get() {
            ARMED => "armed",
            FIRST => "recordingFirst",
            OVERDUB => "overdubbing",
            MULTIPLY => "multiplying",
            PLAYING => "playing",
            _ => "idle",
        }
    }
    pub fn is_armed(&self) -> bool {
        self.state.get() == ARMED
    }
    pub fn is_recording(&self) -> bool {
        matches!(self.state.get(), FIRST | OVERDUB | MULTIPLY)
    }

    fn cell(&self, layer: usize, pos: usize) -> &AtomicU32 {
        &self.arena[layer * self.max_frames + pos]
    }
    fn read(&self, layer: usize, pos: usize) -> f32 {
        f32::from_bits(self.cell(layer, pos).load(Ordering::Relaxed))
    }
    fn write(&self, layer: usize, pos: usize, v: f32) {
        self.cell(layer, pos).store(v.to_bits(), Ordering::Relaxed)
    }
    fn add(&self, layer: usize, pos: usize, v: f32) {
        let c = self.cell(layer, pos);
        let cur = f32::from_bits(c.load(Ordering::Relaxed));
        c.store((cur + v).to_bits(), Ordering::Relaxed)
    }
    /// The captured sample for an input frame, if the ring still holds it.
    fn ring_at(&self, in_frame: i64) -> Option<f32> {
        if in_frame < 0 {
            return None;
        }
        let newest = self.in_frames.load(Ordering::Acquire) as i64;
        // Leave a buffer's grace at the trailing edge: the input callback is
        // still writing, and a frame about to be overwritten is not a frame.
        let oldest = newest - self.ring_len as i64 + self.buffer_frames.load(Ordering::Relaxed) as i64;
        if in_frame < oldest || in_frame >= newest {
            return None;
        }
        let i = (in_frame as usize) % self.ring_len;
        Some(f32::from_bits(self.ring[i].load(Ordering::Relaxed)))
    }

    fn zero_layer(&self, layer: usize) {
        for i in 0..self.max_frames {
            self.cell(layer, i).store(0, Ordering::Relaxed);
        }
    }
}

/// The stream error callback, latching device loss so the supervisor can act.
///
/// One per stream because cpal takes ownership of each.
fn err_cb(sh: Arc<Shared>) -> impl FnMut(cpal::StreamError) + Send + 'static {
    move |e| {
        eprintln!("stream error: {}", e);
        sh.device_lost.store(true, Ordering::Release);
    }
}

pub fn run(opts: Opts) -> Result<(), Box<dyn Error>> {
    let candidate = crate::devices::find(&opts.device)?;
    let device = candidate.device;

    let mut in_cfg = choose_input(&device, opts.in_ch, opts.sample_rate, Width::Widest)
        .ok_or_else(|| format!("{} has no f32 input config", candidate.name))?;
    let mut out_cfg = choose_output(&device, opts.out_ch, opts.sample_rate, Width::Narrowest)
        .ok_or_else(|| format!("{} has no f32 output config", candidate.name))?;
    if let Some(n) = opts.buffer {
        in_cfg.buffer_size = cpal::BufferSize::Fixed(n);
        out_cfg.buffer_size = cpal::BufferSize::Fixed(n);
    }

    let sr = in_cfg.sample_rate.0;
    let sr_f = sr as f64;
    let in_channels = in_cfg.channels as usize;
    let out_channels = out_cfg.channels as usize;
    let max_frames = (opts.max_secs * sr_f).round() as usize;
    let ring_len = (opts.ring_secs * sr_f).round() as usize;

    println!("Device: {}", candidate.name);
    println!(
        "Recording input {}, playing output {}, at {} Hz.",
        opts.in_ch, opts.out_ch, sr
    );
    println!(
        "Arena: {} layers x {:.0} s = {} MB.   Pre-roll: {:.0} s = {} MB.\n",
        MAX_LAYERS,
        opts.max_secs,
        MAX_LAYERS * max_frames * 4 / 1_048_576,
        opts.ring_secs,
        ring_len * 4 / 1_048_576
    );

    let sh = Arc::new(Shared {
        arena: (0..MAX_LAYERS * max_frames).map(|_| AtomicU32::new(0)).collect(),
        max_frames,
        ring: (0..ring_len).map(|_| AtomicU32::new(0)).collect(),
        ring_len,
        loop_len: AtomicUsize::new(0),
        n_layers: AtomicUsize::new(0),
        origin: AtomicI64::new(0),
        state: AtomicU8Wrapper::new(IDLE),
        request: AtomicU8Wrapper::new(0),
        out_frames: AtomicUsize::new(0),
        in_frames: AtomicUsize::new(0),
        k: AtomicI64::new(0),
        k_set: AtomicBool::new(false),
        p0: Mutex::new(None),
        buffer_frames: AtomicU32::new(0),
        click: AtomicBool::new(opts.click || opts.selftest.is_some()),
        reached: AtomicUsize::new(0),
        overflowed: AtomicBool::new(false),
        preroll: AtomicUsize::new(
            (opts.preroll_ms / 1000.0 * sr_f).round().max(0.0) as usize,
        ),
        rec_from: AtomicI64::new(0),
        monitor: AtomicBool::new(opts.monitor),
        out_peak: AtomicU32::new(0),
        in_peak: AtomicU32::new(0),
        p0_needed: AtomicBool::new(true),
        p0_frame: AtomicUsize::new(0),
        device_lost: AtomicBool::new(false),
        reopens: AtomicUsize::new(0),
    });

    // Both streams are rebuilt on recovery, so building them lives in a closure
    // rather than inline. Everything it captures is either `Arc` or `Copy`.
    let build_streams = |device: &cpal::Device|
     -> Result<(cpal::Stream, cpal::Stream), Box<dyn Error>> {

    let out_stream = {
        // Cloned before the shadowing below moves `sh` into the callback.
        let err_sh = sh.clone();
        let sh = sh.clone();
        let ch = opts.out_ch;
        let dual = opts.dual;
        device.build_output_stream(
            &out_cfg,
            move |data: &mut [f32], info: &cpal::OutputCallbackInfo| {
                for s in data.iter_mut() {
                    *s = 0.0;
                }
                let frames = data.len() / out_channels;
                sh.buffer_frames.store(frames as u32, Ordering::Relaxed);

                let base = sh.out_frames.load(Ordering::Acquire);
                if sh.p0_needed.load(Ordering::Relaxed) {
                    // `try_lock` because this is the audio thread; if the lock
                    // is contended the next buffer will do just as well.
                    if let Ok(mut g) = sh.p0.try_lock() {
                        *g = Some(info.timestamp().playback);
                        sh.p0_frame.store(base, Ordering::Release);
                        sh.p0_needed.store(false, Ordering::Release);
                    }
                }

                // Transitions are stamped here because this is the only thread
                // that knows the exact frame, and a loop boundary a buffer out
                // is a loop boundary that is audibly wrong.
                match sh.request.take() {
                    ARMED => {
                        sh.reached.store(0, Ordering::Release);
                        let n = sh.n_layers.load(Ordering::Acquire);
                        if n < MAX_LAYERS {
                            if sh.loop_len.load(Ordering::Acquire) == 0 {
                                // Only the first recording lays down the grid.
                                // Re-stamping origin on every arm would drag the
                                // whole loop to position zero the instant you
                                // hit record — playback reads origin too. The
                                // self-test cannot catch that, because both
                                // sides move together.
                                sh.origin.store(base as i64, Ordering::Release);
                                sh.rec_from.store(base as i64, Ordering::Release);
                                sh.state.set(FIRST);
                            } else {
                                // An overdub is modular against the existing
                                // grid, so it records from the same reference
                                // the loop plays from.
                                sh.rec_from
                                    .store(sh.origin.load(Ordering::Acquire), Ordering::Release);
                                sh.state.set(OVERDUB);
                            }
                        }
                    }
                    PLAYING => sh.state.set(PLAYING),
                    IDLE => {}
                    _ => {}
                }

                let loop_len = sh.loop_len.load(Ordering::Acquire);
                let n = sh.n_layers.load(Ordering::Acquire);
                let origin = sh.origin.load(Ordering::Acquire);

                // Monitoring reads the freshest frames the pre-roll holds. One
                // buffer behind the converters, so the interface's own direct
                // monitoring beats it — this is for headphones with nothing
                // else in the room.
                let monitor = sh.monitor.load(Ordering::Relaxed);
                let mon_from = sh.in_frames.load(Ordering::Acquire) as i64 - frames as i64;

                let mut peak = 0.0f32;
                for f in 0..frames {
                    let out_frame = (base + f) as i64;
                    let mut v = 0.0f32;

                    if loop_len > 0 {
                        let pos = (out_frame - origin).rem_euclid(loop_len as i64) as usize;
                        for l in 0..n {
                            v += sh.read(l, pos);
                        }
                        if sh.click.load(Ordering::Relaxed) && pos < 16 {
                            v += 0.4;
                        }
                    }
                    if monitor {
                        if let Some(m) = sh.ring_at(mon_from + f as i64) {
                            v += m;
                        }
                    }

                    peak = peak.max(v.abs());
                    data[f * out_channels + ch] = v;
                    if dual && ch + 1 < out_channels {
                        data[f * out_channels + ch + 1] = v;
                    }
                }
                sh.out_peak.fetch_max(peak.to_bits(), Ordering::Relaxed);
                sh.out_frames.store(base + frames, Ordering::Release);
            },
            err_cb(err_sh),
            None,
        )?
    };

    let in_stream = {
        // Cloned before the shadowing below moves `sh` into the callback.
        let err_sh = sh.clone();
        let sh = sh.clone();
        let ch = opts.in_ch;
        let residual = opts.residual;
        device.build_input_stream(
            &in_cfg,
            move |data: &[f32], info: &cpal::InputCallbackInfo| {
                let frames = data.len() / in_channels;
                let base = sh.in_frames.load(Ordering::Acquire);

                if !sh.k_set.load(Ordering::Acquire) {
                    // The one consultation of the host clock in the whole engine.
                    let Ok(g) = sh.p0.try_lock() else {
                        sh.in_frames.store(base + frames, Ordering::Release);
                        return;
                    };
                    let Some(p0) = g.as_ref() else {
                        sh.in_frames.store(base + frames, Ordering::Release);
                        return;
                    };
                    let buffer = sh.buffer_frames.load(Ordering::Relaxed) as f64;
                    let offset = residual - 2.0 * buffer;
                    let c0 = signed_secs(p0, &info.timestamp().capture) * sr_f;
                    // `p0_frame` is zero at startup, so this is the same
                    // arithmetic as before for the case that always worked.
                    let p0_frame = sh.p0_frame.load(Ordering::Acquire) as f64;
                    sh.k.store(
                        (p0_frame + c0 - base as f64 - offset).round() as i64,
                        Ordering::Release,
                    );
                    sh.k_set.store(true, Ordering::Release);
                }

                // Always, regardless of transport state. This is what makes
                // the past claimable.
                let mut peak = 0.0f32;
                for f in 0..frames {
                    let v = data[f * in_channels + ch];
                    peak = peak.max(v.abs());
                    let i = (base + f) % sh.ring_len;
                    sh.ring[i].store(v.to_bits(), Ordering::Relaxed);
                }
                sh.in_peak.fetch_max(peak.to_bits(), Ordering::Relaxed);

                let state = sh.state.get();
                if state != FIRST && state != OVERDUB && state != MULTIPLY {
                    sh.in_frames.store(base + frames, Ordering::Release);
                    return;
                }

                let k = sh.k.load(Ordering::Acquire);
                let origin = sh.rec_from.load(Ordering::Acquire);
                let loop_len = sh.loop_len.load(Ordering::Acquire);
                let layer = sh.n_layers.load(Ordering::Acquire);
                if layer >= MAX_LAYERS {
                    sh.in_frames.store(base + frames, Ordering::Release);
                    return;
                }

                for f in 0..frames {
                    let out_frame = (base + f) as i64 + k;
                    let rel = out_frame - origin;
                    if rel < 0 {
                        continue;
                    }
                    let v = data[f * in_channels + ch];

                    if state == FIRST || state == MULTIPLY {
                        // Linear. Its length becomes the cycle, so it must not
                        // wrap — and it stops rather than overwriting.
                        let pos = rel as usize;
                        if pos >= sh.max_frames {
                            sh.overflowed.store(true, Ordering::Relaxed);
                            continue;
                        }
                        sh.write(layer, pos, v);
                        sh.reached.fetch_max(pos + 1, Ordering::Relaxed);
                    } else {
                        // Modular: an overdub may go round as many times as it
                        // likes, summing into the same cycle.
                        if loop_len == 0 {
                            continue;
                        }
                        let pos = (rel % loop_len as i64) as usize;
                        sh.add(layer, pos, v);
                        sh.reached.fetch_max(loop_len, Ordering::Relaxed);
                    }
                }
                sh.in_frames.store(base + frames, Ordering::Release);
            },
            err_cb(err_sh),
            None,
        )?
    };

        Ok((out_stream, in_stream))
    };

    let (mut out_stream, mut in_stream) = build_streams(&device)?;
    out_stream.play()?;
    in_stream.play()?;
    std::thread::sleep(Duration::from_millis(300));

    if let Some(port) = opts.ws_port {
        crate::ws::serve(sh.clone(), sr, port);
    }

    if let Some(secs) = opts.selftest {
        let r = selftest(&sh, sr, secs);
        drop(in_stream);
        drop(out_stream);
        return r;
    }

    // With a socket open, the console moves to its own thread so the main one
    // can watch the device. cpal's streams are not `Send` on this platform, so
    // whichever thread built them is the only one that may replace them — and
    // supervision behind a blocking read of stdin would only begin once the
    // console closed, which is precisely backwards.
    if opts.ws_port.is_some() {
        let sh_console = sh.clone();
        std::thread::spawn(move || {
            // `q` means stop the daemon, not stop this thread. EOF means the
            // console was never there, and the socket carries on regardless.
            if control_loop(&sh_console, sr) {
                std::process::exit(0);
            }
            println!("(console closed; still serving the socket and watching the device)");
        });
    } else {
        let _ = control_loop(&sh, sr);
    }

    // stdin closing is not a reason to stop.
    //
    // Run headless — from a launcher, or with output redirected — and
    // `lines()` returns immediately at EOF. Exiting there would take the audio
    // engine and the socket down with it the instant the daemon stopped being
    // attached to a terminal, which is exactly when it is meant to be working.
    // With a socket open there is still a client to serve, so park instead.
    if opts.ws_port.is_some() {
        supervise(&sh, &opts.device, &build_streams, &mut out_stream, &mut in_stream);
    }

    drop(in_stream);
    drop(out_stream);
    Ok(())
}

/// Watch the device, and put it back when it goes.
///
/// Two detectors, because they catch different faults. cpal *reports* an
/// unplugged interface through the error callback — that is the loud case. But
/// a stream can also simply stop being called with no error at all, and that is
/// the one that cost an afternoon: the socket kept serving plausible snapshots
/// while both meters read digital zero and every command vanished into a
/// request nothing would ever consume. So the frame counter is watched too, and
/// a transport that claims to be running while its frames stand still is
/// treated as lost whether or not anyone said so.
///
/// Never returns. Ctrl-C or a kill stops the daemon.
fn supervise<F>(
    sh: &Arc<Shared>,
    device_name: &str,
    build: &F,
    out_stream: &mut cpal::Stream,
    in_stream: &mut cpal::Stream,
) where
    F: Fn(&cpal::Device) -> Result<(cpal::Stream, cpal::Stream), Box<dyn Error>>,
{
    const TICK_MS: u64 = 250;
    /// Ticks of a motionless frame counter before we stop giving it the benefit
    /// of the doubt. Comfortably longer than any buffer, short enough that the
    /// app says so before you have finished wondering.
    const STALL_TICKS: u32 = 8;

    let mut last_frames = sh.out_frames.load(Ordering::Acquire);
    let mut still = 0u32;

    loop {
        std::thread::sleep(Duration::from_millis(TICK_MS));

        let frames = sh.out_frames.load(Ordering::Acquire);
        if frames == last_frames {
            still += 1;
        } else {
            still = 0;
            last_frames = frames;
        }

        let reported = sh.device_lost.load(Ordering::Acquire);
        let stalled = still >= STALL_TICKS;
        if !reported && !stalled {
            continue;
        }

        eprintln!(
            "device {} — reopening {}",
            if reported { "reported lost" } else { "stopped answering" },
            device_name
        );

        // A recording that spans an outage has a hole in it, and a hole in a
        // layer is worse than no layer: it will be discovered later, in the
        // mix, with no way to tell what went wrong. So abandon whatever was
        // being captured and keep only what was already committed.
        match sh.state.get() {
            FIRST | OVERDUB | MULTIPLY => {
                let n = sh.n_layers.load(Ordering::Acquire);
                sh.zero_layer(n);
                sh.state.set(if sh.loop_len.load(Ordering::Acquire) > 0 {
                    PLAYING
                } else {
                    IDLE
                });
                eprintln!("  the recording in progress was dropped — it would have had a gap");
            }
            _ => {}
        }
        sh.request.take();

        // Both streams restart independently, so the input↔output pairing has
        // to be established again from scratch. Everything downstream reads
        // `k_set`, so clearing it is enough to make them wait for a fresh K
        // rather than trust a stale one.
        sh.k_set.store(false, Ordering::Release);
        if let Ok(mut g) = sh.p0.lock() {
            *g = None;
        }
        sh.p0_needed.store(true, Ordering::Release);

        // Reopen. The device has to be looked up again — after a USB cycle the
        // old handle refers to something that no longer exists.
        loop {
            std::thread::sleep(Duration::from_millis(750));
            let found = match crate::devices::find(device_name) {
                Ok(c) => c,
                Err(_) => continue,
            };
            match build(&found.device) {
                Ok((new_out, new_in)) => {
                    let played = new_out.play().and_then(|_| new_in.play());
                    if played.is_err() {
                        continue;
                    }
                    *out_stream = new_out;
                    *in_stream = new_in;
                    sh.device_lost.store(false, Ordering::Release);
                    sh.reopens.fetch_add(1, Ordering::Release);
                    last_frames = sh.out_frames.load(Ordering::Acquire);
                    still = 0;
                    eprintln!("  {} is back.", found.name);
                    break;
                }
                Err(_) => continue,
            }
        }
    }
}

fn commit(sh: &Shared, sr: u32) {
    let state = sh.state.get();
    if state != FIRST && state != OVERDUB {
        return;
    }
    // Let the input drain: it trails the output by K, so the last frames of the
    // loop have not arrived yet. Without this the tail of every recording is
    // missing, which is exactly the kind of fault that sounds like "feel".
    sh.state.set(PLAYING);
    std::thread::sleep(Duration::from_millis(60));

    if state == FIRST {
        let mut len = sh.reached.load(Ordering::Acquire);
        if len == 0 {
            println!("  nothing recorded.");
            return;
        }
        // Pre-roll: a tap is always a little late, so back-date the loop's start
        // and fill the front from the ring. The attack that would have been
        // clipped off is already captured; it just has to be claimed.
        let pre = sh.preroll.load(Ordering::Acquire);
        let layer = sh.n_layers.load(Ordering::Acquire);
        let origin = sh.origin.load(Ordering::Acquire);
        let new_origin = origin - pre as i64;
        if pre > 0 && len + pre > sh.max_frames {
            // Shifting anyway would run off the end of this layer's slice and
            // into the next one's, which is silent corruption rather than an
            // error. Refuse instead.
            println!(
                "  pre-roll skipped: the loop plus pre-roll would exceed --max-secs."
            );
        } else if pre > 0 && new_origin >= 0 {
            // Shift what was recorded up by `pre`, backwards so the move does
            // not eat its own tail, then fill the vacated front from the ring.
            for pos in (0..len).rev() {
                let v = sh.read(layer, pos);
                sh.write(layer, pos + pre, v);
            }
            for pos in 0..pre {
                sh.write(layer, pos, 0.0);
            }
            let got = fill_from_ring(sh, layer, new_origin, pre, false);
            sh.origin.store(new_origin, Ordering::Release);
            len += pre;
            println!(
                "  pre-roll: {:.0} ms recovered from before the tap ({} of {} frames).",
                pre as f64 / sr as f64 * 1000.0,
                got,
                pre
            );
        }
        sh.loop_len.store(len, Ordering::Release);
        println!(
            "  loop set: {} frames ({:.3} s), {:.1} bpm if that is one bar of 4/4",
            len,
            len as f64 / sr as f64,
            240.0 / (len as f64 / sr as f64)
        );
    }
    let layer = sh.n_layers.fetch_add(1, Ordering::AcqRel);
    let len = sh.loop_len.load(Ordering::Acquire);
    if len > 0 {
        draw_layer(sh, layer, len, sr);
    }
    println!(
        "  committed. {} layer{} playing.",
        layer + 1,
        if layer == 0 { "" } else { "s" }
    );
}

/// What a layer actually contains, drawn.
///
/// "How do I know what has been recorded?" is a fair question to ask of a
/// machine whose entire state is invisible, and it is the question this whole
/// project exists to answer better than a single LED does. Hearing it is the
/// real answer; this is the one available the instant a pass ends, and it
/// distinguishes silence from quiet, a full loop from a half-empty one, and a
/// clipped take from a clean one at a glance.
fn draw_layer(sh: &Shared, layer: usize, len: usize, sr: u32) {
    const COLS: usize = 56;
    const RAMP: [char; 8] = [' ', '.', ':', '-', '=', '+', '*', '#'];

    let mut peak = 0.0f32;
    let mut sum = 0.0f64;
    let mut bins = [0.0f32; COLS];
    for i in 0..len {
        let v = sh.read(layer, i).abs();
        peak = peak.max(v);
        sum += (v * v) as f64;
        let b = i * COLS / len;
        bins[b] = bins[b].max(v);
    }
    let rms = (sum / len.max(1) as f64).sqrt() as f32;

    if peak < 1e-6 {
        println!("  layer {}: silent.", layer);
        return;
    }

    let bar: String = bins
        .iter()
        .map(|&v| {
            // Against the layer's own peak, so a quiet take still shows its
            // shape rather than a flat line.
            let f = (v / peak).clamp(0.0, 1.0);
            RAMP[((f.sqrt() * 7.0).round() as usize).min(7)]
        })
        .collect();

    println!("  |{}|", bar);
    println!(
        "  layer {}   {:.2} s   peak {:.1} dBFS   rms {:.1} dBFS{}",
        layer,
        len as f64 / sr as f64,
        20.0 * (peak.max(1e-9) as f64).log10(),
        20.0 * (rms.max(1e-9) as f64).log10(),
        if peak >= 0.999 { "   CLIPPED" } else { "" }
    );
}

/// Fill a stretch of a layer from the pre-roll, addressing it in *output* frames
/// so it lands on the same grid live recording uses.
///
/// Returns how many frames were actually available. A short answer is not an
/// error — it means the request reached back further than the ring holds, and
/// the caller should say so rather than silently hand over a loop with a
/// truncated front.
fn fill_from_ring(sh: &Shared, layer: usize, from_out: i64, len: usize, additive: bool) -> usize {
    let k = sh.k.load(Ordering::Acquire);
    let mut got = 0;
    for pos in 0..len {
        let Some(v) = sh.ring_at(from_out + pos as i64 - k) else {
            continue;
        };
        if additive {
            sh.add(layer, pos, v);
        } else {
            sh.write(layer, pos, v);
        }
        got += 1;
    }
    got
}

/// Claim the recent past as a loop or a layer.
///
/// The feature no pedal can offer, and the one most likely to change how the
/// thing gets used: you played something good and did not hit record, so hit it
/// afterwards. With no loop yet, `secs` of the past becomes the loop and sets
/// the cycle. With a loop running, the last complete cycle becomes a new layer,
/// landing on the existing grid because the fill is addressed in output frames.
fn take(sh: &Shared, sr: u32, secs: f64) {
    if !sh.k_set.load(Ordering::Acquire) {
        println!("  no input has arrived yet.");
        return;
    }
    let layer = sh.n_layers.load(Ordering::Acquire);
    if layer >= MAX_LAYERS {
        println!("  {} layers is the ceiling; undo one first.", MAX_LAYERS);
        return;
    }

    let loop_len = sh.loop_len.load(Ordering::Acquire);
    let cur = sh.out_frames.load(Ordering::Acquire) as i64;

    let (from_out, len, what) = if loop_len == 0 {
        let len = ((secs * sr as f64).round() as usize).min(sh.max_frames);
        (cur - len as i64, len, "loop")
    } else {
        // The last cycle that has actually finished. Anything else would be a
        // partial pass presented as a whole one.
        let origin = sh.origin.load(Ordering::Acquire);
        let done = (cur - origin).div_euclid(loop_len as i64);
        if done < 1 {
            println!("  not one complete cycle has gone by yet.");
            return;
        }
        (origin + (done - 1) * loop_len as i64, loop_len, "layer")
    };

    if from_out < 0 {
        println!("  that reaches back before the engine started.");
        return;
    }

    sh.zero_layer(layer);
    let got = fill_from_ring(sh, layer, from_out, len, false);
    if got == 0 {
        println!("  the pre-roll does not reach back that far.");
        return;
    }
    if got < len {
        println!(
            "  only {:.2} s of the {:.2} s asked for was still in the pre-roll.",
            got as f64 / sr as f64,
            len as f64 / sr as f64
        );
    }

    if loop_len == 0 {
        sh.loop_len.store(len, Ordering::Release);
        sh.origin.store(from_out, Ordering::Release);
        sh.state.set(PLAYING);
        println!(
            "  took the last {:.3} s as the {}: {} frames, {:.1} bpm if that is one bar of 4/4",
            len as f64 / sr as f64,
            what,
            len,
            240.0 / (len as f64 / sr as f64)
        );
    } else {
        println!("  took the last complete cycle as a new {}.", what);
    }
    let taken = sh.n_layers.fetch_add(1, Ordering::AcqRel);
    draw_layer(sh, taken, sh.loop_len.load(Ordering::Acquire), sr);
    println!(
        "  {} layer{} playing.",
        taken + 1,
        if taken == 0 { "" } else { "s" }
    );
}

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
fn multiply_start(sh: &Shared, sr: u32) {
    let loop_len = sh.loop_len.load(Ordering::Acquire);
    if loop_len == 0 {
        println!("  nothing to multiply — record a loop first.");
        return;
    }
    if sh.n_layers.load(Ordering::Acquire) >= MAX_LAYERS {
        println!("  {} layers is the ceiling; undo one first.", MAX_LAYERS);
        return;
    }

    let origin = sh.origin.load(Ordering::Acquire);
    let cur = sh.out_frames.load(Ordering::Acquire) as i64;
    let cyc = (cur - origin).div_euclid(loop_len as i64);
    let from = origin + cyc * loop_len as i64;

    let layer = sh.n_layers.load(Ordering::Acquire);
    sh.zero_layer(layer);
    sh.rec_from.store(from, Ordering::Release);
    sh.reached.store(0, Ordering::Release);
    sh.state.set(MULTIPLY);

    // The part of this cycle already played is in the pre-roll; claim it, so
    // the multiply really does begin on the boundary.
    let behind = (cur - from) as usize;
    if behind > 0 {
        let got = fill_from_ring(sh, layer, from, behind, false);
        sh.reached.fetch_max(got, Ordering::Relaxed);
        println!(
            "  multiplying from the start of this cycle — {:.2} s of it recovered \
             from the pre-roll.",
            got as f64 / sr as f64
        );
    } else {
        println!("  multiplying from this cycle's start.");
    }
    println!("  play across as many cycles as you want, then x again.");
}

/// End a multiply: round to whole cycles and grow the loop to fit.
///
/// Rounding rather than truncating, because at nine tenths of the way through
/// the fourth cycle you meant four. Which means sometimes waiting for the
/// boundary to arrive rather than cutting the loop short at the press.
fn multiply_end(sh: &Shared, sr: u32) {
    let loop_len = sh.loop_len.load(Ordering::Acquire);
    let from = sh.rec_from.load(Ordering::Acquire);
    let cur = sh.out_frames.load(Ordering::Acquire) as i64;
    let elapsed = (cur - from).max(0) as f64;

    let n = ((elapsed / loop_len as f64).round() as usize).max(1);
    let new_len = n * loop_len;
    if new_len > sh.max_frames {
        println!(
            "  {} cycles would be {:.1} s, past the --max-secs ceiling of {:.1} s. \
             Stopping at the old length.",
            n,
            new_len as f64 / sr as f64,
            sh.max_frames as f64 / sr as f64
        );
        sh.state.set(PLAYING);
        return;
    }

    // If the rounding went up, the last cycle has not finished yet. Wait for it
    // rather than hand back a loop that is short by however late the press was.
    let target = from + new_len as i64;
    if target > cur {
        println!(
            "  rounding up to {} cycles; waiting {:.2} s for the boundary.",
            n,
            (target - cur) as f64 / sr as f64
        );
        while (sh.out_frames.load(Ordering::Acquire) as i64) < target {
            std::thread::sleep(Duration::from_millis(5));
        }
    }
    // And let the input drain past it, since it trails by K.
    sh.state.set(PLAYING);
    std::thread::sleep(Duration::from_millis(60));

    // Everything that was playing has to fill the new, longer cycle — that is
    // what "with the original repeating underneath" means. The multiply began
    // on a cycle boundary, so each existing layer simply repeats from its own
    // position zero.
    let layers = sh.n_layers.load(Ordering::Acquire);
    for l in 0..layers {
        for c in 1..n {
            for pos in 0..loop_len {
                let v = sh.read(l, pos);
                sh.write(l, c * loop_len + pos, v);
            }
        }
    }

    // The new loop's position zero is where the multiply began.
    sh.origin.store(from, Ordering::Release);
    sh.loop_len.store(new_len, Ordering::Release);

    println!(
        "  x{}: loop is now {:.3} s ({} cycles of {:.3} s).",
        n,
        new_len as f64 / sr as f64,
        n,
        loop_len as f64 / sr as f64
    );
    let layer = sh.n_layers.fetch_add(1, Ordering::AcqRel);
    draw_layer(sh, layer, new_len, sr);
    println!("  committed. {} layers playing.", layer + 1);
}

/// Returns true only if the user actually asked to quit.
///
/// EOF is not a quit. Run headless — from a launcher, or with output
/// redirected — and `lines()` returns immediately, which must not be allowed to
/// take the audio engine and the socket down with it.
fn control_loop(sh: &Shared, sr: u32) -> bool {
    println!("Commands:  r = record/overdub toggle   x = multiply   t [secs] = take");
    println!("           u = undo   c = clear   k = click   m = input monitoring");
    println!("           l = levels   p = status + waveforms   q = quit\n");

    let stdin = std::io::stdin();
    for line in stdin.lock().lines() {
        let Ok(line) = line else { return false };
        if line.trim() == "q" {
            return true;
        }
        let ack = dispatch(sh, sr, &line);
        if !ack.is_empty() {
            println!("  {}", ack);
        }
    }
    false
}

/// One command, from wherever it came.
///
/// Both the console and the socket land here, so a footswitch, a browser and a
/// terminal cannot drift into meaning different things by the same name. The
/// detail still goes to stdout — waveforms and level readings are for the
/// person sitting at the daemon — and what comes back is the short
/// acknowledgement a remote caller needs. Remote clients render from the state
/// snapshot rather than from these strings.
pub fn dispatch(sh: &Shared, sr: u32, line: &str) -> String {
    {
        match line.trim() {
            "x" => match sh.state.get() {
                MULTIPLY => multiply_end(sh, sr),
                FIRST | OVERDUB => println!("  finish this recording first."),
                _ => multiply_start(sh, sr),
            },
            "r" => match sh.state.get() {
                MULTIPLY => multiply_end(sh, sr),
                FIRST | OVERDUB => commit(sh, sr),
                _ => {
                    let layer = sh.n_layers.load(Ordering::Acquire);
                    if layer >= MAX_LAYERS {
                        println!("  {} layers is the ceiling; undo one first.", MAX_LAYERS);
                    } else {
                        // An overdub sums into its layer, so anything left there
                        // from an undone take would bleed into the new one.
                        sh.zero_layer(layer);
                        sh.request.set(ARMED);
                        println!("  recording...");
                    }
                }
            },
            l if l.starts_with('t') => {
                let secs = l[1..].trim().parse::<f64>().unwrap_or(8.0);
                take(sh, sr, secs);
            }
            "u" => {
                let n = sh.n_layers.load(Ordering::Acquire);
                if n == 0 {
                    println!("  nothing to undo.");
                } else {
                    sh.n_layers.store(n - 1, Ordering::Release);
                    sh.zero_layer(n - 1);
                    println!("  layer {} removed, {} left.", n, n - 1);
                }
            }
            // Deliberate device-loss injection, so the recovery path can be
            // proved rather than hoped for. It is the same argument as the
            // alignment self-test: this is a part of a looper that can be
            // verified, so it should be.
            "!lose" => {
                sh.device_lost.store(true, Ordering::Release);
                println!("  simulating device loss.");
            }
            "c" => {
                sh.state.set(IDLE);
                sh.n_layers.store(0, Ordering::Release);
                sh.loop_len.store(0, Ordering::Release);
                for l in 0..MAX_LAYERS {
                    sh.zero_layer(l);
                }
                println!("  cleared.");
            }
            // `k` and `m` flip, which is right at a console and wrong over a
            // wire: a client that sets rather than flips drifts out of step the
            // first time a command is dropped, and never recovers. So the
            // explicit forms exist alongside, and the app uses those.
            "k" | "k1" | "k0" => {
                let on = match line.trim() {
                    "k1" => true,
                    "k0" => false,
                    _ => !sh.click.load(Ordering::Relaxed),
                };
                sh.click.store(on, Ordering::Relaxed);
                println!("  click {}.", if on { "on" } else { "off" });
            }
            "m" | "m1" | "m0" => {
                let on = match line.trim() {
                    "m1" => true,
                    "m0" => false,
                    _ => !sh.monitor.load(Ordering::Relaxed),
                };
                sh.monitor.store(on, Ordering::Relaxed);
                println!(
                    "  input monitoring {}.{}",
                    if on { "on" } else { "off" },
                    if on {
                        "  (the interface's own direct monitoring is lower latency)"
                    } else {
                        ""
                    }
                );
            }
            "l" => {
                let inp = f32::from_bits(sh.in_peak.swap(0, Ordering::Relaxed));
                let out = f32::from_bits(sh.out_peak.swap(0, Ordering::Relaxed));
                println!(
                    "  in {:>7.1} dBFS   out {:>7.1} dBFS   (peak since last check)",
                    20.0 * (inp.max(1e-9) as f64).log10(),
                    20.0 * (out.max(1e-9) as f64).log10()
                );
                if inp < 1e-6 {
                    println!("    nothing at all is arriving on input {}.", "the chosen channel");
                }
            }
            "p" => {
                let len = sh.loop_len.load(Ordering::Acquire);
                for l in 0..sh.n_layers.load(Ordering::Acquire) {
                    if len > 0 {
                        draw_layer(sh, l, len, sr);
                    }
                }
                println!(
                    "  {} layers, loop {} frames ({:.3} s), state {}, K {:+}{}",
                    sh.n_layers.load(Ordering::Acquire),
                    len,
                    len as f64 / sr as f64,
                    match sh.state.get() {
                        FIRST => "recording first",
                        OVERDUB => "overdubbing",
                        MULTIPLY => "multiplying",
                        PLAYING => "playing",
                        _ => "idle",
                    },
                    sh.k.load(Ordering::Acquire),
                    if sh.overflowed.load(Ordering::Relaxed) {
                        "   (a recording hit the arena ceiling)"
                    } else {
                        ""
                    }
                );
            }
            "" => {}
            other => return format!("unknown command {:?}", other),
        }
    }
    String::new()
}

/// Record one cycle of the engine's own click through a loopback cable and ask
/// where it ended up. Same question `align` asks, but through the real transport
/// and the real layer storage — so it tests what will actually run.
fn selftest(sh: &Shared, sr: u32, secs: f64) -> Result<(), Box<dyn Error>> {
    let len = (secs * sr as f64).round() as usize;
    println!("Self-test: {} frame loop ({:.2} s), recording one cycle.", len, secs);

    sh.loop_len.store(len, Ordering::Release);
    sh.request.set(ARMED);
    std::thread::sleep(Duration::from_secs_f64(secs * 2.0 + 0.3));
    commit(sh, sr);
    std::thread::sleep(Duration::from_millis(200));

    let (e0, p0) = onset_of(sh, 0, len)
        .ok_or("nothing recorded — is the loopback cable patched from the output \
                jack to the input jack named by --out-ch / --in-ch?")?;
    println!(
        "  layer 0: click played at 0, recorded at {:+} samples ({:+.3} ms), peak {:.1} dBFS",
        e0,
        e0 as f64 / sr as f64 * 1000.0,
        20.0 * (p0.max(1e-9) as f64).log10()
    );

    // Now the property a looper actually stands on: that an overdub recorded
    // while listening to an existing layer lands on top of it. The click is
    // switched off, so the only thing going down the cable is layer 0 playing
    // back. If it returns to the same position, layers stack — and if it did
    // not, every overdub would sit a little further out than the last.
    println!("\nOverdub pass: click off, recording layer 0's own playback.");
    sh.click.store(false, Ordering::Relaxed);
    sh.request.set(ARMED);
    std::thread::sleep(Duration::from_secs_f64(secs * 2.0 + 0.3));
    commit(sh, sr);
    std::thread::sleep(Duration::from_millis(200));

    let (e1, p1) = onset_of(sh, 1, len)
        .ok_or("the overdub recorded nothing, though the first pass worked")?;
    println!(
        "  layer 1: layer 0's click returned at {:+} samples ({:+.3} ms), peak {:.1} dBFS",
        e1,
        e1 as f64 / sr as f64 * 1000.0,
        20.0 * (p1.max(1e-9) as f64).log10()
    );

    // Third: claim a cycle that was never recorded. Both existing layers carry
    // the click, so playback has one at position zero; a retroactive take of
    // the last complete cycle must land it there too. This is the pre-roll path
    // rather than the live-record path, and it uses different code to reach the
    // same grid — so it deserves its own check.
    println!("\nRetroactive take: claiming the last complete cycle from the pre-roll.");
    std::thread::sleep(Duration::from_secs_f64(secs * 1.5));
    take(sh, sr, 0.0);
    std::thread::sleep(Duration::from_millis(100));

    let e2 = match onset_of(sh, 2, len) {
        Some((e, p)) => {
            println!(
                "  layer 2: taken from the past, click at {:+} samples ({:+.3} ms), peak {:.1} dBFS",
                e,
                e as f64 / sr as f64 * 1000.0,
                20.0 * (p.max(1e-9) as f64).log10()
            );
            e
        }
        None => return Err("the retroactive take captured nothing".into()),
    };

    // Fourth: grow the loop while it plays. The claim being tested is not the
    // arithmetic but the bookkeeping — that everything already recorded repeats
    // into the new length, which is what "with the original playing underneath"
    // means and is the whole point of the gesture.
    println!("\nMultiply: growing the loop while it plays.");
    multiply_start(sh, sr);
    std::thread::sleep(Duration::from_secs_f64(secs * 2.2));
    multiply_end(sh, sr);
    std::thread::sleep(Duration::from_millis(100));

    let new_len = sh.loop_len.load(Ordering::Acquire);
    if new_len % len != 0 {
        return Err(format!(
            "the multiplied loop is {} frames, not a whole multiple of {}",
            new_len, len
        )
        .into());
    }
    let n = new_len / len;

    // Layer 0 carried the click at position zero. After a multiply it should
    // carry it at every cycle boundary, because it was repeated to fill.
    let mut missing = Vec::new();
    for c in 0..n {
        let mut best = 0f32;
        for d in 0..64usize {
            best = best.max(sh.read(0, (c * len + d) % new_len).abs());
            if c * len + len > d {
                let back = (c * len + new_len - d - 1) % new_len;
                best = best.max(sh.read(0, back).abs());
            }
        }
        if best < 0.01 {
            missing.push(c);
        }
    }
    println!(
        "  loop is now x{} ({:.2} s), and layer 0 repeats at {} of {} cycle boundaries.",
        n,
        new_len as f64 / sr as f64,
        n - missing.len(),
        n
    );
    if !missing.is_empty() {
        return Err(format!(
            "the original does not repeat underneath — it is missing at cycle(s) {:?}. \
             A multiply that drops what it was multiplying is worse than no multiply",
            missing
        )
        .into());
    }

    let slip = e1 - e0;
    println!("\n  Layer-to-layer slip: {:+} samples.", slip);
    if e2.abs() > 2 {
        return Err(format!(
            "live recording aligns but the retroactive take is {} samples out — the \
             pre-roll is being addressed on a different grid than the live path",
            e2.abs()
        )
        .into());
    }

    if e0.abs() <= 2 && slip.abs() <= 2 {
        println!(
            "\n  Aligned through the real transport and the real layer storage, and\n  \
             overdubs stack on top of what they were recorded against. Eight layers\n  \
             deep will be as tight as one."
        );
        Ok(())
    } else if e0.abs() > 2 {
        Err(format!(
            "layer 0 is off by {} samples through the engine, though `align` passes — \
             the fault is in the transport, not the calibration",
            e0.abs()
        )
        .into())
    } else {
        Err(format!(
            "layer 0 lands correctly but the overdub slips {} samples against it. That \
             compounds: eight layers would end up {} samples apart",
            slip.abs(),
            slip.abs() * 8
        )
        .into())
    }
}

/// Onset position of the loudest thing in a layer, as a signed offset from loop
/// position zero, with its peak. Wrapping, because something landing slightly
/// early sits at the end of the loop rather than the start.
fn onset_of(sh: &Shared, layer: usize, len: usize) -> Option<(i64, f32)> {
    let mut peak = 0f32;
    let mut peak_at = 0usize;
    for i in 0..len {
        let v = sh.read(layer, i).abs();
        if v > peak {
            peak = v;
            peak_at = i;
        }
    }
    if peak < 0.01 {
        return None;
    }
    let mut onset = peak_at;
    for _ in 0..len {
        let prev = (onset + len - 1) % len;
        if sh.read(layer, prev).abs() <= 0.01 {
            break;
        }
        onset = prev;
    }
    let e = if onset > len / 2 { onset as i64 - len as i64 } else { onset as i64 };
    Some((e, peak))
}
