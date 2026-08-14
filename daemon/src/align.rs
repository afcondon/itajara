//! The self-test: does a recorded sound land where it was played?
//!
//! Everything a looper does rests on one arithmetic chain — given a captured
//! frame, which position in the loop does it belong at? Get it wrong and
//! overdubs sit slightly off the beat, the error compounds with every layer,
//! and nothing in the sound says so. You would just find, eventually, that the
//! thing does not feel right.
//!
//! So before any of it is built, test it end to end. Play a click at loop
//! position zero, record it back through a patch cable, and ask where the click
//! landed. If the arithmetic is right the answer is zero, and the answer is a
//! number rather than an opinion. This is the only part of a looper that can be
//! *verified* rather than judged by ear.
//!
//! ## Why this is done in frames and not in time
//!
//! The obvious implementation converts each input buffer's capture timestamp
//! into a loop position using the host clock. It is wrong, and this test is how
//! we found out: **the interface's sample clock and the host clock are not the
//! same clock.** On the Audio4c they differ by about 15.6 ppm, which is
//! 0.75 samples every second — measured here as a dead-straight line of −3, −9,
//! −18 and −36 samples at 4, 12, 24 and 48 seconds. A three-minute loop would
//! end up 135 samples out. Nothing about that announces itself.
//!
//! So no host-clock arithmetic survives past startup. Both streams are driven
//! by the same device clock, so their frame counters advance in lockstep
//! forever and differ only by a constant:
//!
//! ```text
//!     out_frame = in_frame + K
//! ```
//!
//! `K` is established once, at the first input callback, from the timestamps
//! and the measured offset — the only place the host clock is consulted at all:
//!
//! ```text
//!     K = (C0 − P0) × rate − in_frames_so_far − offset_samples
//!     offset_samples = residual − 2 × buffer
//! ```
//!
//! where `P0` is the playback instant of output frame zero, `C0` the capture
//! instant of the buffer `K` is computed from, and the two-buffer term is
//! cpal's over-account rather than delay. See `DESIGN-LOOPER.md` §10.
//!
//! After that it is integer addition, and it cannot drift.

use cpal::traits::{DeviceTrait, StreamTrait};
use std::error::Error;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU32, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::measure::{choose_input, choose_output, signed_secs, Width};

const BURST_FRAMES: usize = 16;

/// The level at which a click counts as having started. Must be the same figure
/// `measure` uses, so that the residual it derives and the position this test
/// checks refer to the same instant in the same waveform.
const DETECT: f32 = 0.01;

pub struct Opts {
    pub device: String,
    pub out_ch: usize,
    pub in_ch: usize,
    /// The interface's own transit, in samples, from `itajara sweep`. It is
    /// session state rather than a constant — see DESIGN-LOOPER §10 — so it is a
    /// flag rather than something baked in.
    pub residual: f64,
    pub loop_secs: f64,
    pub cycles: usize,
    pub amplitude: f32,
    pub sample_rate: u32,
    pub buffer: Option<u32>,
}

impl Default for Opts {
    fn default() -> Self {
        Opts {
            device: String::new(),
            out_ch: 0,
            in_ch: 0,
            residual: 252.0,
            loop_secs: 2.0,
            cycles: 4,
            amplitude: 0.5,
            sample_rate: 48_000,
            buffer: None,
        }
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
    let loop_len = (opts.loop_secs * sr_f).round() as usize;

    println!("Device: {}", candidate.name);
    println!(
        "Loop: {} frames ({:.2} s at {} Hz)   click on output {}, recording input {}",
        loop_len, opts.loop_secs, sr, opts.out_ch, opts.in_ch
    );

    // Where the recording lands. One atomic per frame holding the largest
    // magnitude seen at that position across all cycles — enough to find the
    // click, and lock-free so the callback never blocks.
    let recorded: Arc<Vec<AtomicU32>> =
        Arc::new((0..loop_len).map(|_| AtomicU32::new(0)).collect());

    // The playback instant of output frame zero, published by the output
    // callback on its first run and read by the input callback exactly once.
    let p0: Arc<Mutex<Option<cpal::StreamInstant>>> = Arc::new(Mutex::new(None));
    let out_frames = Arc::new(AtomicUsize::new(0));
    let in_frames = Arc::new(AtomicUsize::new(0));
    let k = Arc::new(AtomicI64::new(0));
    let k_set = Arc::new(AtomicBool::new(false));
    let seen_buffer = Arc::new(AtomicU32::new(0));
    let placed = Arc::new(AtomicUsize::new(0));
    let dropped = Arc::new(AtomicUsize::new(0));
    // Kept as a diagnostic rather than used: this is how far the host clock has
    // wandered from the device's frame count, which is the thing the frame
    // pairing above exists to be immune to.
    let host_drift = Arc::new(AtomicI64::new(0));

    let err = |e| eprintln!("stream error: {}", e);

    let out_stream = {
        let p0 = p0.clone();
        let out_frames = out_frames.clone();
        let seen_buffer = seen_buffer.clone();
        let host_drift = host_drift.clone();
        let ch = opts.out_ch;
        let amp = opts.amplitude;
        device.build_output_stream(
            &out_cfg,
            move |data: &mut [f32], info: &cpal::OutputCallbackInfo| {
                for s in data.iter_mut() {
                    *s = 0.0;
                }
                let frames = data.len() / out_channels;
                seen_buffer.store(frames as u32, Ordering::Relaxed);

                let base = out_frames.load(Ordering::Acquire);
                if base == 0 {
                    if let Ok(mut g) = p0.lock() {
                        if g.is_none() {
                            *g = Some(info.timestamp().playback);
                        }
                    }
                } else if let Ok(g) = p0.try_lock() {
                    if let Some(p0) = g.as_ref() {
                        let by_clock = signed_secs(p0, &info.timestamp().playback) * sr_f;
                        host_drift
                            .store((by_clock - base as f64).round() as i64, Ordering::Relaxed);
                    }
                }

                for f in 0..frames {
                    // Loop position is the device's own frame count, modulo the
                    // loop. No clock involved, so nothing to drift against.
                    if (base + f) % loop_len < BURST_FRAMES {
                        data[f * out_channels + ch] = amp;
                    }
                }
                out_frames.store(base + frames, Ordering::Release);
            },
            err,
            None,
        )?
    };

    let in_stream = {
        let p0 = p0.clone();
        let recorded = recorded.clone();
        let in_frames = in_frames.clone();
        let k = k.clone();
        let k_set = k_set.clone();
        let placed = placed.clone();
        let dropped = dropped.clone();
        let seen_buffer = seen_buffer.clone();
        let ch = opts.in_ch;
        let residual = opts.residual;
        device.build_input_stream(
            &in_cfg,
            move |data: &[f32], info: &cpal::InputCallbackInfo| {
                let frames = data.len() / in_channels;
                let base = in_frames.load(Ordering::Acquire);

                if !k_set.load(Ordering::Acquire) {
                    // The one and only consultation of the host clock. Everything
                    // after this is integer frames.
                    let Ok(g) = p0.try_lock() else {
                        dropped.fetch_add(frames, Ordering::Relaxed);
                        in_frames.store(base + frames, Ordering::Release);
                        return;
                    };
                    let Some(p0) = g.as_ref() else {
                        // Output has not started, so there is no timeline to pair
                        // against yet. Only happens in the first few ms.
                        dropped.fetch_add(frames, Ordering::Relaxed);
                        in_frames.store(base + frames, Ordering::Release);
                        return;
                    };
                    let buffer = seen_buffer.load(Ordering::Relaxed) as f64;
                    let offset_samples = residual - 2.0 * buffer;
                    let c0 = signed_secs(p0, &info.timestamp().capture) * sr_f;
                    k.store(
                        (c0 - base as f64 - offset_samples).round() as i64,
                        Ordering::Release,
                    );
                    k_set.store(true, Ordering::Release);
                }

                let k = k.load(Ordering::Acquire);
                for f in 0..frames {
                    let j = (base + f) as i64 + k;
                    if j < 0 {
                        dropped.fetch_add(1, Ordering::Relaxed);
                        continue;
                    }
                    let pos = (j as usize) % loop_len;
                    let v = data[f * in_channels + ch].abs();
                    recorded[pos].fetch_max(v.to_bits(), Ordering::AcqRel);
                    placed.fetch_add(1, Ordering::Relaxed);
                }
                in_frames.store(base + frames, Ordering::Release);
            },
            err,
            None,
        )?
    };

    out_stream.play()?;
    in_stream.play()?;

    let total = opts.loop_secs * opts.cycles as f64;
    println!("Running {} cycles ({:.1} s)...", opts.cycles, total);
    std::thread::sleep(Duration::from_secs_f64(total + 0.3));

    drop(in_stream);
    drop(out_stream);

    let buffer = seen_buffer.load(Ordering::Relaxed);
    println!(
        "\nBuffer {} frames, residual {:.0} samples, so raw offset {:.0}.  K = {:+}",
        buffer,
        opts.residual,
        opts.residual - 2.0 * buffer as f64,
        k.load(Ordering::Acquire)
    );
    println!(
        "Placed {} frames, dropped {}.",
        placed.load(Ordering::Relaxed),
        dropped.load(Ordering::Relaxed)
    );

    let hd = host_drift.load(Ordering::Relaxed);
    println!(
        "Host clock vs device frames after {:.0}s: {:+} samples ({:.1} ppm){}",
        total,
        hd,
        (hd as f64 / (total * sr_f)) * 1e6,
        if hd.abs() > 2 { "  — which the frame pairing ignores" } else { "" }
    );

    report(&recorded, loop_len, sr)
}

fn report(recorded: &[AtomicU32], loop_len: usize, sr: u32) -> Result<(), Box<dyn Error>> {
    let at = |i: usize| f32::from_bits(recorded[i].load(Ordering::Acquire));

    let mut peak = 0f32;
    let mut peak_at = 0usize;
    for i in 0..loop_len {
        let v = at(i);
        if v > peak {
            peak = v;
            peak_at = i;
        }
    }

    if peak < DETECT {
        return Err("nothing was recorded loudly enough to locate.\n\
             \n\
             Check the loopback cable is in the jacks named by --out-ch and\n\
             --in-ch, and try --amp 0.6."
            .into());
    }

    // Walk back from the peak to the onset, at the same threshold `measure`
    // triggers on — otherwise the two are reading different features of the
    // same burst and their disagreement gets mistaken for an alignment error.
    // Wrapping, because a click landing slightly early sits at the loop's end.
    let mut onset = peak_at;
    for _ in 0..loop_len {
        let prev = (onset + loop_len - 1) % loop_len;
        if at(prev) <= DETECT {
            break;
        }
        onset = prev;
    }

    let error: i64 = if onset > loop_len / 2 {
        onset as i64 - loop_len as i64
    } else {
        onset as i64
    };

    // The neighbourhood, because a number alone cannot distinguish "the
    // arithmetic is off" from "the converter's reconstruction filter rings
    // before the transient and detection caught the ring". Those want opposite
    // responses, and the waveform tells them apart at a glance.
    println!("\n  Recorded level around the onset (0 is where it was played):");
    for d in -10i64..=8 {
        let i = ((onset as i64 + d).rem_euclid(loop_len as i64)) as usize;
        let v = at(i);
        let bars = ((v / peak).max(0.0) * 40.0).round() as usize;
        println!(
            "    {:>+5}  {:>7.1} dBFS  {}{}",
            error + d,
            20.0 * (v.max(1e-9) as f64).log10(),
            "#".repeat(bars),
            if d == 0 { "   <- onset" } else { "" }
        );
    }

    println!(
        "\n  Click played at position 0, recorded at position {}.",
        onset
    );
    println!(
        "  Alignment error: {:+} samples ({:+.3} ms), peak {:.1} dBFS",
        error,
        error as f64 / sr as f64 * 1000.0,
        20.0 * (peak.max(1e-9) as f64).log10()
    );

    // A sample or two is the detector's own resolution — the burst has a rise
    // and the onset may sit a frame inside it. Anything more is the arithmetic.
    if error.abs() <= 2 {
        println!(
            "\n  Aligned. Recorded audio lands where it was played, so overdubs will\n  \
             stack without accumulating drift, and the calibration in use is the\n  \
             right one for this configuration."
        );
    } else {
        println!(
            "\n  ! !  Off by {} samples. Run this at several --cycles values: an error\n       \
             that grows with duration is a clock problem, and one that does not is\n       \
             the residual being wrong for this configuration.",
            error.abs()
        );
    }
    Ok(())
}
