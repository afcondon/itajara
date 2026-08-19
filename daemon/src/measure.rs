//! Round-trip latency measurement, and the routing map that comes with it.
//!
//! This exists before any looping code because of one fact: audio recorded
//! through an interface lands late by the full output→input round trip, and on
//! a looper **that error compounds on every overdub**. Layer three arrives
//! three round trips behind layer one, and nothing in the sound tells you it is
//! happening — it just gradually stops feeling like it is in time. Guessing the
//! number, or taking the interface's published figure, is not good enough,
//! because the figure that matters includes the converters, the buffer size in
//! force right now, and whatever is patched in between.
//!
//! So: emit a click, listen for it coming back, and count.
//!
//! The measurement is only as good as its clock. cpal hands each callback a
//! `StreamInstant` — for output, when the first frame of this buffer will
//! *reach the converter*; for input, when the first frame was *captured*. Both
//! derive from the host clock, so subtracting one from the other gives a real
//! interval rather than a difference of two callback-scheduling accidents.
//! Wall-clock timing at callback entry would be off by a buffer or more.
//!
//! **Every input channel is listened to, not just the one expected to answer.**
//! Two reasons, and the second is the important one. An interface with more USB
//! channels than jacks does not say which is which, so hearing the click name
//! its own channel is the cheapest possible routing map. And if the interface
//! has any internal monitoring path, a channel can hear the click with no cable
//! attached at all — which would otherwise yield a confident, precise, entirely
//! fictional latency. Listening everywhere makes that visible instead.
//!
//! Two paths are worth measuring and they are different numbers:
//!
//!   - **interface only** — a cable from an output back to an input. This is
//!     the floor, and the number to compensate a dry loop by.
//!   - **through the board** — output → pedalboard → input. This is the number
//!     that matters for anything recorded wet, and it is larger by whatever the
//!     pedals' converters add.

use cpal::traits::{DeviceTrait, StreamTrait};
use std::error::Error;
use std::sync::atomic::{AtomicU32, AtomicU8, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

const PHASE_SETTLE: u8 = 0;
const PHASE_NOISE: u8 = 1;
const PHASE_ARMED: u8 = 2;
const PHASE_LISTENING: u8 = 3;

/// Long enough to carry through a converter, short enough that the leading edge
/// is still the leading edge. A single-sample impulse is too quiet to clear the
/// noise floor on a line input with anything patched in front of it.
const BURST_FRAMES: usize = 16;

/// How long to keep listening after each click. Every channel gets the whole
/// window rather than the first one to answer stopping the others.
const LISTEN_MS: u64 = 400;

#[derive(Clone)]
pub struct Opts {
    pub device: String,
    pub out_ch: usize,
    pub repeats: usize,
    pub amplitude: f32,
    pub sample_rate: u32,
    /// Ask CoreAudio for a specific callback size. The reason this is a knob is
    /// diagnostic: if the measured offset moves with buffer size, it is an
    /// artifact of how buffering is accounted for and the calibration has to be
    /// stored per buffer size. If it stays put, it is a property of the
    /// interface and one constant will do.
    pub buffer: Option<u32>,
    /// Emit one machine-readable object on stdout and nothing else.
    ///
    /// So a conductor can run the calibration rather than a person reading a
    /// paragraph — which is the difference between a number that gets measured
    /// when the configuration changes and one that gets measured once.
    pub json: bool,
}

impl Default for Opts {
    fn default() -> Self {
        Opts {
            device: String::new(),
            out_ch: 0,
            repeats: 8,
            amplitude: 0.5,
            sample_rate: 48_000,
            buffer: None,
            json: false,
        }
    }
}

/// How to break ties once a config is wide enough and hits the target rate.
#[derive(Clone, Copy, PartialEq)]
pub enum Width {
    /// Fewest channels that still reach the one we need — open the plain stereo
    /// pair rather than a sixteen-channel config we will not use.
    Narrowest,
    /// Every channel the device has. Metering and routing discovery want this:
    /// the point is to find out which channel the signal is on, so hiding any
    /// of them defeats it.
    Widest,
}

/// Pick an f32 config with more than `min_index` channels, at the target rate
/// if the device will do it.
fn choose<I>(configs: I, min_index: usize, target: u32, width: Width) -> Option<cpal::StreamConfig>
where
    I: Iterator<Item = cpal::SupportedStreamConfigRange>,
{
    let takes = |c: &cpal::SupportedStreamConfigRange| {
        c.min_sample_rate().0 <= target && target <= c.max_sample_rate().0
    };

    let mut best: Option<cpal::SupportedStreamConfigRange> = None;
    for c in configs {
        if c.sample_format() != cpal::SampleFormat::F32 {
            continue;
        }
        if (c.channels() as usize) <= min_index {
            continue;
        }
        let better = match &best {
            None => true,
            Some(b) => {
                // Reaching the target rate outranks any channel-count preference:
                // a measurement across a resampler is not a measurement.
                if takes(&c) != takes(b) {
                    takes(&c)
                } else if width == Width::Narrowest {
                    c.channels() < b.channels()
                } else {
                    c.channels() > b.channels()
                }
            }
        };
        if better {
            best = Some(c);
        }
    }

    best.map(|c| {
        let rate = if takes(&c) { cpal::SampleRate(target) } else { c.max_sample_rate() };
        c.with_sample_rate(rate).into()
    })
}

pub fn choose_input(
    device: &cpal::Device,
    min_index: usize,
    target: u32,
    width: Width,
) -> Option<cpal::StreamConfig> {
    choose(device.supported_input_configs().ok()?, min_index, target, width)
}

pub fn choose_output(
    device: &cpal::Device,
    min_index: usize,
    target: u32,
    width: Width,
) -> Option<cpal::StreamConfig> {
    choose(device.supported_output_configs().ok()?, min_index, target, width)
}

/// What one input channel had to say across the whole run.
struct ChannelResult {
    latencies_ms: Vec<f64>,
    peak: f32,
}

/// One complete click-and-listen run at one buffer size.
struct Probe {
    channels: Vec<ChannelResult>,
    sr: u32,
    /// What CoreAudio actually gave, which is not always what was asked for.
    buffer: u32,
}

fn probe(opts: &Opts, verbose: bool) -> Result<Probe, Box<dyn Error>> {
    let candidate = crate::devices::find(&opts.device)?;
    let device = candidate.device;
    if verbose {
        println!("Device: {}", candidate.name);
    }

    let mut in_cfg = choose_input(&device, 0, opts.sample_rate, Width::Widest)
        .ok_or_else(|| format!("{} has no f32 input config", candidate.name))?;
    let mut out_cfg = choose_output(&device, opts.out_ch, opts.sample_rate, Width::Narrowest)
        .ok_or_else(|| {
            format!(
                "{} has no f32 output config with more than {} channels",
                candidate.name, opts.out_ch
            )
        })?;

    if let Some(n) = opts.buffer {
        in_cfg.buffer_size = cpal::BufferSize::Fixed(n);
        out_cfg.buffer_size = cpal::BufferSize::Fixed(n);
    }

    if in_cfg.sample_rate != out_cfg.sample_rate {
        return Err(format!(
            "input is at {} Hz and output at {} Hz; a measurement across a rate \
             conversion is meaningless",
            in_cfg.sample_rate.0, out_cfg.sample_rate.0
        )
        .into());
    }

    let sr = in_cfg.sample_rate.0;
    let in_channels = in_cfg.channels as usize;
    let out_channels = out_cfg.channels as usize;
    if verbose {
        println!(
            "Streams: {} Hz   listening on all {} inputs   clicking on output {} of {}",
            sr, in_channels, opts.out_ch, out_channels
        );
    }

    let phase = Arc::new(AtomicU8::new(PHASE_SETTLE));
    // f32 bits in atomics. Positive IEEE-754 floats order the same as their bit
    // patterns as integers, so fetch_max on the bits is a genuine max.
    let noise: Arc<Vec<AtomicU32>> = Arc::new((0..in_channels).map(|_| AtomicU32::new(0)).collect());
    let thresholds: Arc<Vec<AtomicU32>> =
        Arc::new((0..in_channels).map(|_| AtomicU32::new(1.0f32.to_bits())).collect());
    let peaks: Arc<Vec<AtomicU32>> = Arc::new((0..in_channels).map(|_| AtomicU32::new(0)).collect());
    // Locking in an audio callback is not something the engine will do. Here the
    // critical sections are one pointer store each, run a handful of times in a
    // measurement that lasts seconds, and the alternative is a lock-free queue
    // that would obscure what this file is for.
    let fired: Arc<Mutex<Option<cpal::StreamInstant>>> = Arc::new(Mutex::new(None));
    let heard: Arc<Vec<Mutex<Option<cpal::StreamInstant>>>> =
        Arc::new((0..in_channels).map(|_| Mutex::new(None)).collect());
    // What CoreAudio actually gave us, which is not always what was asked for.
    let seen_in = Arc::new(AtomicU32::new(0));
    let seen_out = Arc::new(AtomicU32::new(0));

    let err = |e| eprintln!("stream error: {}", e);

    let out_stream = {
        let phase = phase.clone();
        let fired = fired.clone();
        let seen_out = seen_out.clone();
        let ch = opts.out_ch;
        let amp = opts.amplitude;
        device.build_output_stream(
            &out_cfg,
            move |data: &mut [f32], info: &cpal::OutputCallbackInfo| {
                for s in data.iter_mut() {
                    *s = 0.0;
                }
                seen_out.store((data.len() / out_channels) as u32, Ordering::Relaxed);
                if phase.load(Ordering::Acquire) != PHASE_ARMED {
                    return;
                }
                let frames = data.len() / out_channels;
                for f in 0..BURST_FRAMES.min(frames) {
                    data[f * out_channels + ch] = amp;
                }
                // The burst starts at frame 0, so the buffer's own playback
                // instant is the click's instant — no offset to add.
                if let Ok(mut g) = fired.lock() {
                    *g = Some(info.timestamp().playback);
                }
                phase.store(PHASE_LISTENING, Ordering::Release);
            },
            err,
            None,
        )?
    };

    let in_stream = {
        let phase = phase.clone();
        let noise = noise.clone();
        let thresholds = thresholds.clone();
        let peaks = peaks.clone();
        let heard = heard.clone();
        let seen_in = seen_in.clone();
        device.build_input_stream(
            &in_cfg,
            move |data: &[f32], info: &cpal::InputCallbackInfo| {
                let frames = data.len() / in_channels;
                seen_in.store(frames as u32, Ordering::Relaxed);
                match phase.load(Ordering::Acquire) {
                    PHASE_NOISE => {
                        for ch in 0..in_channels {
                            let mut peak = 0f32;
                            for f in 0..frames {
                                peak = peak.max(data[f * in_channels + ch].abs());
                            }
                            noise[ch].fetch_max(peak.to_bits(), Ordering::AcqRel);
                        }
                    }
                    PHASE_LISTENING => {
                        for ch in 0..in_channels {
                            let thr = f32::from_bits(thresholds[ch].load(Ordering::Acquire));
                            let mut peak = 0f32;
                            let mut onset: Option<usize> = None;
                            for f in 0..frames {
                                let v = data[f * in_channels + ch].abs();
                                peak = peak.max(v);
                                if onset.is_none() && v > thr {
                                    onset = Some(f);
                                }
                            }
                            peaks[ch].fetch_max(peak.to_bits(), Ordering::AcqRel);

                            if let Some(f) = onset {
                                if let Ok(mut g) = heard[ch].lock() {
                                    // Only the first crossing in the window counts;
                                    // later buffers are the tail, or a reflection.
                                    if g.is_none() {
                                        // Frame f of this buffer, not frame 0 — this
                                        // is where the sample accuracy comes from.
                                        *g = info.timestamp().capture.add(Duration::from_nanos(
                                            f as u64 * 1_000_000_000 / sr as u64,
                                        ));
                                    }
                                }
                            }
                        }
                    }
                    _ => {}
                }
            },
            err,
            None,
        )?
    };

    out_stream.play()?;
    in_stream.play()?;

    // Let CoreAudio settle before believing anything it says.
    std::thread::sleep(Duration::from_millis(300));

    phase.store(PHASE_NOISE, Ordering::Release);
    std::thread::sleep(Duration::from_millis(500));
    phase.store(PHASE_SETTLE, Ordering::Release);

    let got_in = seen_in.load(Ordering::Relaxed);
    let got_out = seen_out.load(Ordering::Relaxed);
    if got_in != got_out {
        return Err(format!(
            "input callbacks are {} frames and output {} — the model that relates \
             the two timestamp streams assumes they match",
            got_in, got_out
        )
        .into());
    }
    if verbose {
        match opts.buffer {
            Some(asked) if asked != got_in => println!(
                "Buffer: asked for {} frames, got {} — CoreAudio declined",
                asked, got_in
            ),
            _ => println!("Buffer: {} frames per callback", got_in),
        }
    }

    if verbose {
        println!("\nNoise floor per input, and the threshold set from it:");
    }
    for ch in 0..in_channels {
        let floor = f32::from_bits(noise[ch].load(Ordering::Acquire));
        // Eight times the noise floor, with an absolute minimum so a silent
        // input does not give a threshold of zero and "detect" the first
        // dithered sample.
        let thr = (floor * 8.0).max(0.01);
        thresholds[ch].store(thr.to_bits(), Ordering::Release);
        if verbose {
            println!(
                "  ch {:>2}  floor {:>8.1} dBFS   threshold {:>8.1} dBFS{}",
                ch,
                dbfs(floor),
                dbfs(thr),
                if floor > 0.05 { "   <- noisy; may trigger on the wrong thing" } else { "" }
            );
        }
    }

    let mut results: Vec<ChannelResult> =
        (0..in_channels).map(|_| ChannelResult { latencies_ms: Vec::new(), peak: 0.0 }).collect();

    if verbose {
        println!("\nClicking {} times:", opts.repeats);
    }
    for i in 0..opts.repeats {
        *fired.lock().unwrap() = None;
        for ch in 0..in_channels {
            *heard[ch].lock().unwrap() = None;
            peaks[ch].store(0, Ordering::Release);
        }

        phase.store(PHASE_ARMED, Ordering::Release);
        std::thread::sleep(Duration::from_millis(LISTEN_MS));
        phase.store(PHASE_SETTLE, Ordering::Release);

        let f = fired.lock().unwrap().clone();
        let Some(f) = f else {
            if verbose {
                println!("  {:>2}.  the click never went out", i + 1);
            }
            continue;
        };

        let mut answered: Vec<String> = Vec::new();
        for ch in 0..in_channels {
            results[ch].peak = results[ch]
                .peak
                .max(f32::from_bits(peaks[ch].load(Ordering::Acquire)));
            let h = heard[ch].lock().unwrap().clone();
            if let Some(h) = h {
                let secs = signed_secs(&f, &h);
                results[ch].latencies_ms.push(secs * 1000.0);
                answered.push(format!("ch{} {:.2}ms", ch, secs * 1000.0));
            }
        }

        if verbose {
            if answered.is_empty() {
                println!("  {:>2}.  silence on every input", i + 1);
            } else {
                println!("  {:>2}.  {}", i + 1, answered.join("   "));
            }
        }

        // Let any tail decay so the next click is not detected against a ring.
        std::thread::sleep(Duration::from_millis(200));
    }

    drop(in_stream);
    drop(out_stream);

    Ok(Probe { channels: results, sr, buffer: got_in })
}

pub fn run(opts: Opts) -> Result<(), Box<dyn Error>> {
    let mut p = probe(&opts, true)?;
    report(&mut p.channels, p.sr, opts.repeats)
}

/// Click every output in turn and see which input answers.
///
/// With one cable patched from an output jack to an input jack, exactly one
/// (output channel, input channel) pair responds — which names both ends from a
/// single run. That is the whole point: the host channel behind an output jack
/// is otherwise as unknowable as the one behind an input jack, and an interface
/// with more channels than jacks tells you neither.
///
/// Anything that answers on more than one pair is an internal routing path, and
/// worth knowing about before it corrupts a measurement.
pub fn map(opts: Opts) -> Result<(), Box<dyn Error>> {
    let candidate = crate::devices::find(&opts.device)?;
    let out_cfg = choose_output(&candidate.device, 0, opts.sample_rate, Width::Widest)
        .ok_or_else(|| format!("{} has no f32 output config", candidate.name))?;
    let out_channels = out_cfg.channels as usize;
    drop(out_cfg);

    println!("Device: {}\n", candidate.name);
    println!(
        "Clicking each of the {} outputs in turn, listening on every input.\n\
         With one cable patched, one pair should answer — and that names the host\n\
         channel behind both jacks at once.\n",
        out_channels
    );

    let mut found: Vec<(usize, usize, f64, f32)> = Vec::new();

    println!("  out ch    heard on");
    for out_ch in 0..out_channels {
        let mut o = opts.clone();
        o.out_ch = out_ch;
        o.repeats = 2;

        let p = match probe(&o, false) {
            Ok(p) => p,
            Err(e) => {
                println!("  {:>6}    failed: {}", out_ch, e);
                continue;
            }
        };

        let mut answers: Vec<String> = Vec::new();
        for (in_ch, c) in p.channels.iter().enumerate() {
            if c.latencies_ms.is_empty() {
                continue;
            }
            let mut lats = c.latencies_ms.clone();
            lats.sort_by(|a, b| a.partial_cmp(b).unwrap());
            let m = median(&lats);
            let samples = m / 1000.0 * p.sr as f64
                // Undo the two-buffer over-account so this table shows real
                // transit rather than a figure dominated by buffer size.
                + 2.0 * p.buffer as f64;
            answers.push(format!(
                "ch {} ({:+.0} sm, {:.1} dBFS)",
                in_ch,
                samples,
                dbfs(c.peak)
            ));
            found.push((out_ch, in_ch, samples, c.peak));
        }

        println!(
            "  {:>6}    {}",
            out_ch,
            if answers.is_empty() { "—".to_string() } else { answers.join("   ") }
        );
    }

    match found.len() {
        0 => println!(
            "\n  Nothing answered anywhere. Either no cable is patched, or its output\n  \
             end is in a jack this device does not drive from any host channel."
        ),
        1 => {
            let (o, i, s, _) = found[0];
            println!(
                "\n  One pair: output channel {} -> input channel {}, {:.0} samples of\n  \
                 real transit. Whichever jacks that cable is in, those are their host\n  \
                 channels.",
                o, i, s
            );
        }
        n => println!(
            "\n  {} pairs answered. If only one cable is patched, the rest are internal\n  \
             routing inside the interface — and a latency measured across one of those\n  \
             crossed no converter, so it is not a latency at all.",
            n
        ),
    }
    Ok(())
}

const SWEEP_BUFFERS: [u32; 5] = [64, 128, 256, 512, 1024];

/// Measure at several buffer sizes and separate the two things mixed together
/// in a single reading.
///
/// A single measurement cannot tell a real converter delay from a bookkeeping
/// error in the timestamps, because both are just a number of samples. Varying
/// the buffer size separates them, because only one of them moves: whatever is
/// proportional to buffer size is accounting, and whatever survives is physics.
///
/// The residual is the number the engine wants. The slope is the correction the
/// engine must apply to raw timestamp arithmetic — and since the engine chooses
/// its own buffer size, it can apply it exactly.
pub fn sweep(opts: Opts) -> Result<(), Box<dyn Error>> {
    // In `--json` the prose is not merely quieter, it is absent: a conductor
    // parsing stdout must not have to know which lines are commentary.
    let say = |s: String| {
        if !opts.json {
            println!("{}", s)
        }
    };
    let candidate = crate::devices::find(&opts.device)?;
    say(format!("Device: {}\n", candidate.name));
    say(format!(
        "Measuring at several buffer sizes. A single reading cannot tell a real\n\
         converter delay from a bookkeeping error in the timestamps; varying the\n\
         buffer separates them, because only one of them moves.\n"
    ));

    // (buffer frames, measured samples, channel, spread ms)
    let mut points: Vec<(f64, f64, usize, f64)> = Vec::new();
    let mut sr = opts.sample_rate;

    say(format!("  buffer     ch    measured     spread"));
    for &b in SWEEP_BUFFERS.iter() {
        let mut o = opts.clone();
        o.buffer = Some(b);

        let p = match probe(&o, false) {
            Ok(p) => p,
            Err(e) => {
                say(format!("  {:>6}     —    failed: {}", b, e));
                continue;
            }
        };
        sr = p.sr;

        let best = p
            .channels
            .iter()
            .enumerate()
            .filter(|(_, c)| !c.latencies_ms.is_empty())
            .max_by(|(_, a), (_, c)| a.peak.partial_cmp(&c.peak).unwrap());

        let Some((ch, res)) = best else {
            say(format!("  {:>6}     —    silence on every input", b));
            continue;
        };

        let mut lats = res.latencies_ms.clone();
        lats.sort_by(|a, c| a.partial_cmp(c).unwrap());
        let m_ms = median(&lats);
        let spread = lats[lats.len() - 1] - lats[0];
        let samples = m_ms / 1000.0 * p.sr as f64;

        say(format!(
            "  {:>6}  {:>4}  {:>+8.0} sm   {:>6.3} ms{}",
            p.buffer,
            ch,
            samples,
            spread,
            if spread > 0.5 { "   <- unstable" } else { "" }
        ));
        points.push((p.buffer as f64, samples, ch, spread));
    }

    if points.len() < 2 {
        return Err("need at least two buffer sizes to separate accounting from \
                    physics; check the cable and try `measure` on its own first"
            .into());
    }

    // Slope from consecutive pairs. With exact data these agree to the sample,
    // and disagreement is itself the finding — it would mean the relationship is
    // not linear and no single constant describes the interface.
    let mut slopes: Vec<f64> = Vec::new();
    for w in points.windows(2) {
        let (b0, m0, _, _) = w[0];
        let (b1, m1, _, _) = w[1];
        if (b1 - b0).abs() > f64::EPSILON {
            slopes.push((m1 - m0) / (b1 - b0));
        }
    }
    slopes.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let slope = median(&slopes);
    let slope_spread = slopes[slopes.len() - 1] - slopes[0];

    let residuals: Vec<f64> = points.iter().map(|&(b, m, _, _)| m - slope * b).collect();
    let mut sorted = residuals.clone();
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let residual = median(&sorted);
    let residual_spread = sorted[sorted.len() - 1] - sorted[0];

    say(format!("\n  implied constant at each size, once the slope is removed:"));
    for (i, &(b, _, _, _)) in points.iter().enumerate() {
        say(format!("    buffer {:>5}   {:>+8.1} samples", b as u32, residuals[i]));
    }

    say(format!(
        "\n  Model:  measured = {:.0} {} {:.2} x buffer",
        residual,
        if slope < 0.0 { "-" } else { "+" },
        slope.abs()
    ));

    // The two ways this measurement can fail to mean anything. In `--json` they
    // are reported as `usable: false` with the reason, rather than as a missing
    // object — a conductor has to be able to tell "the interface is not
    // describable by one constant" from "the tool did not run".
    if slope_spread.abs() > 0.05 {
        let why = format!(
            "the slope varies by {:.2} across buffer sizes, so the relationship is \
             not linear and no single constant describes this interface",
            slope_spread
        );
        say(format!(
            "\n  ! !  The slope is not consistent across sizes ({:.2} of variation).\n       \
             The relationship is not linear, so no single constant describes this\n       \
             interface and the calibration has to be stored per buffer size.",
            slope_spread
        ));
        emit_sweep_json(&opts, &candidate.name, sr, residual, slope, &points, &residuals, Some(&why));
        return Ok(());
    }

    if residual_spread.abs() > 8.0 {
        let why = format!(
            "the residual varies by {:.0} samples across buffer sizes, so it is not \
             buffer-independent and has to be stored per buffer size",
            residual_spread
        );
        say(format!(
            "\n  ! !  The residual varies by {:.0} samples across buffer sizes, so it\n       \
             is not buffer-independent after all. Treat it as per-buffer-size.",
            residual_spread
        ));
        emit_sweep_json(&opts, &candidate.name, sr, residual, slope, &points, &residuals, Some(&why));
        return Ok(());
    }

    let buffers = -slope;
    say(format!(
        "\n  The slope is {:.2} buffers per buffer. {}",
        buffers,
        if (buffers - 2.0).abs() < 0.05 {
            "That is exactly one buffer on each\n  side — the timestamps over-account for the \
             output pipeline and the input\n  pipeline alike. It is bookkeeping, not delay, and \
             it cancels once the\n  buffer size is known."
        } else {
            "That is not the two buffers a symmetric\n  over-account would give, so read it with \
             some suspicion."
        }
    ));

    say(format!(
        "\n  Buffer-independent residual: {:.0} samples, {:.2} ms at {} Hz.\n  \
         That is the interface's own converter round trip — it does not move when\n  \
         the buffer does, which is what makes it physics rather than accounting,\n  \
         and it is the number recordings have to be compensated by.",
        residual,
        residual / sr as f64 * 1000.0,
        sr
    ));

    say(format!(
        "\n  For the engine:  true_offset_samples = measured_samples + {:.0} x buffer_frames",
        buffers
    ));
    emit_sweep_json(&opts, &candidate.name, sr, residual, slope, &points, &residuals, None);
    Ok(())
}

/// The sweep as one object, for whoever is going to store it.
///
/// Hand-rolled rather than serde, because this crate has no JSON dependency and
/// one field of one shape does not justify acquiring one. The evidence goes in
/// alongside the answer — every buffer size and what it implied — so a stored
/// calibration can be argued with later rather than merely believed.
#[allow(clippy::too_many_arguments)]
fn emit_sweep_json(
    opts: &Opts,
    device: &str,
    sr: u32,
    residual: f64,
    slope: f64,
    points: &[(f64, f64, usize, f64)],
    residuals: &[f64],
    unusable: Option<&str>,
) {
    if !opts.json {
        return;
    }
    let per: Vec<String> = points
        .iter()
        .zip(residuals.iter())
        .map(|(&(b, m, ch, _), &r)| {
            format!(
                r#"{{"buffer":{},"measured":{:.1},"implied":{:.1},"channel":{}}}"#,
                b as u32, m, r, ch
            )
        })
        .collect();
    println!(
        concat!(
            r#"{{"kind":"audio-latency.v1","device":"{}","sample_rate":{},"#,
            r#""out_ch":{},"residual_samples":{:.0},"residual_ms":{:.4},"#,
            r#""slope_buffers":{:.2},"usable":{},"reason":"{}","points":[{}]}}"#
        ),
        escape(device),
        sr,
        opts.out_ch,
        residual,
        residual / sr as f64 * 1000.0,
        -slope,
        unusable.is_none(),
        escape(unusable.unwrap_or("")),
        per.join(",")
    );
}

fn escape(s: &str) -> String {
    s.chars()
        .flat_map(|c| match c {
            '"' => vec!['\\', '"'],
            '\\' => vec!['\\', '\\'],
            '\n' => vec!['\\', 'n'],
            c => vec![c],
        })
        .collect()
}

fn dbfs(x: f32) -> f64 {
    20.0 * (x.max(1e-9) as f64).log10()
}

/// Signed, deliberately. A negative interval is not nonsense: it means the input
/// saw the click sooner than CoreAudio's reported playback instant, which is
/// exactly what a virtual loopback device does — it copies the buffer in
/// software, so the output latency it advertises is never actually spent. Real
/// converters with a real path between them give positive numbers.
pub fn signed_secs(fired: &cpal::StreamInstant, heard: &cpal::StreamInstant) -> f64 {
    match heard.duration_since(fired) {
        Some(d) => d.as_secs_f64(),
        None => -fired.duration_since(heard).map(|d| d.as_secs_f64()).unwrap_or(0.0),
    }
}

fn median(sorted: &[f64]) -> f64 {
    let n = sorted.len();
    if n % 2 == 0 {
        (sorted[n / 2 - 1] + sorted[n / 2]) / 2.0
    } else {
        sorted[n / 2]
    }
}

fn report(results: &mut [ChannelResult], sr: u32, repeats: usize) -> Result<(), Box<dyn Error>> {
    let responders: Vec<usize> = (0..results.len())
        .filter(|&ch| !results[ch].latencies_ms.is_empty())
        .collect();

    if responders.is_empty() {
        return Err("nothing came back on any input.\n\
             \n\
             The usual causes, in order of likelihood:\n\
               - no signal path from the output back to an input. The loopback is\n\
                 not implied; for an interface-only figure that means a cable.\n\
               - wrong output channel. Try --out-ch; it is zero-based.\n\
               - output muted, or its level at zero, in the interface's own mixer.\n\
               - amplitude too low for the input's gain staging. Try --amp 0.9."
            .into());
    }

    println!("\n  channel   heard   median latency        samples      peak");
    for &ch in &responders {
        results[ch]
            .latencies_ms
            .sort_by(|a, b| a.partial_cmp(b).unwrap());
        let m = median(&results[ch].latencies_ms);
        let n = results[ch].latencies_ms.len();
        let lo = results[ch].latencies_ms[0];
        let hi = results[ch].latencies_ms[n - 1];
        println!(
            "  ch {:>2}    {:>2}/{:<2}   {:>8.3} ms         {:>7.0}   {:>7.1} dBFS{}",
            ch,
            n,
            repeats,
            m,
            m / 1000.0 * sr as f64,
            dbfs(results[ch].peak),
            if hi - lo > 1.0 { "   (unstable)" } else { "" }
        );
    }

    // The routing map is the first thing to read off this. One responder is the
    // ordinary case and names the channel that cable arrives on.
    if responders.len() > 1 {
        println!(
            "\n  ! !  {} inputs heard the same click. If only one cable is patched,\n       \
             the others are an internal monitoring path inside the interface —\n       \
             and a latency measured down one of those is fiction, because no\n       \
             converter was crossed.",
            responders.len()
        );
    }

    // Then the number itself, from the strongest responder.
    let best = *responders
        .iter()
        .max_by(|&&a, &&b| results[a].peak.partial_cmp(&results[b].peak).unwrap())
        .unwrap();
    let lats = &results[best].latencies_ms;
    let m = median(lats);
    let spread = lats[lats.len() - 1] - lats[0];

    println!(
        "\n  Strongest responder: ch {}  —  {:.3} ms, {:.0} samples at {} Hz",
        best,
        m,
        m / 1000.0 * sr as f64,
        sr
    );
    println!("  spread {:.3} ms over {} clicks", spread, lats.len());

    if lats.len() < repeats {
        println!(
            "\n  Only {} of {} clicks were heard. An intermittent path is worse than\n  \
             a slow one — check the cable before trusting the median.",
            lats.len(),
            repeats
        );
    }

    // The spread is the part to read. A tight cluster means the number can be
    // used as a constant; a wide one means something is resampling, or the
    // buffer size is being renegotiated, and compensating by the median would
    // be compensating by a number that was never true.
    if spread > 1.0 {
        println!(
            "\n  ! !  {:.1} ms of spread is too much to compensate with a constant.\n       \
             Check that nothing is resampling and that the buffer size is fixed.",
            spread
        );
    } else if m < 0.0 {
        // Negative used to be reported here as evidence of a virtual loopback.
        // That was wrong: the sweep showed the timestamps over-account by one
        // buffer on each side, so any path shorter than two buffers reads below
        // zero, cable and converters and all.
        println!(
            "\n  Negative, which is expected rather than alarming: the timestamps\n  \
             over-account by about one buffer on each side, so a path shorter than\n  \
             two buffers reads below zero. Run `sweep` to separate that bookkeeping\n  \
             from the interface's own round trip."
        );
    } else {
        println!("\n  Tight enough to use as a constant.");
    }
    Ok(())
}
