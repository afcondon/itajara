//! Live input metering — the "is this actually plugged in where I think it is"
//! instrument.
//!
//! The Audio4c presents eight channels to the host but has four analog inputs,
//! because the rest carry the internal mixer and the other host's audio. Which
//! USB channel corresponds to which physical jack is not something to take on
//! trust: getting it wrong means recording a silent channel, or measuring
//! latency down a path that is not the one the guitar is on, and neither
//! announces itself.
//!
//! So: play something into a jack and watch which number moves.
//!
//! This also answers the questions that come up forever afterwards — is the
//! pedalboard returning anything, is that input clipping, is the noise floor
//! where it should be.

use cpal::traits::{DeviceTrait, StreamTrait};
use std::error::Error;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

const REFRESH_HZ: u64 = 20;
/// Peaks fall at roughly this many dB per second once the sound stops, so a
/// transient stays visible long enough to read but does not stick.
const DECAY_DB_PER_SEC: f32 = 40.0;
const FLOOR_DB: f32 = -72.0;

pub struct Opts {
    pub device: String,
    pub seconds: u64,
    pub sample_rate: u32,
    /// Lines for a program instead of bars for a person. See `run_json`.
    pub json: bool,
}

impl Default for Opts {
    fn default() -> Self {
        Opts { device: String::new(), seconds: 30, sample_rate: 48_000, json: false }
    }
}

fn db(x: f32) -> f32 {
    20.0 * x.max(1e-9).log10()
}

/// A 30-cell bar from FLOOR_DB to 0, with the region above -6 dB marked
/// differently so approaching clipping reads at a glance rather than requiring
/// the number to be parsed.
fn bar(level_db: f32) -> String {
    const WIDTH: usize = 30;
    let frac = ((level_db - FLOOR_DB) / -FLOOR_DB).clamp(0.0, 1.0);
    let filled = (frac * WIDTH as f32).round() as usize;
    let hot_starts = (((-6.0 - FLOOR_DB) / -FLOOR_DB) * WIDTH as f32).round() as usize;

    (0..WIDTH)
        .map(|i| {
            if i >= filled {
                '·'
            } else if i >= hot_starts {
                '#'
            } else {
                '='
            }
        })
        .collect()
}

/// How often `--json` says what it heard.
const JSON_EVERY: Duration = Duration::from_millis(100);

/// **Every input's mean and peak, ten times a second, as lines of JSON.**
///
/// The mean is the point: on a DC-coupled input it is the voltage on the
/// socket, as a fraction of full scale, and that is how a control-voltage
/// output is measured (Quadrat's Calibrate page, the Outputs tab). Each line
/// covers the stretch since the one before, so a reader that changes a
/// voltage waits a line and then believes the next.
///
/// `{"rate":48000,"channels":16}` first, then
/// `{"mean":[…],"peak":[…]}` until `--seconds` have passed or it is stopped.
fn run_json(opts: Opts) -> Result<(), Box<dyn Error>> {
    use std::io::Write;
    use std::sync::Mutex;

    let candidate = crate::devices::find(&opts.device)?;
    let device = candidate.device;
    let cfg = crate::measure::choose_input(&device, 0, opts.sample_rate, crate::measure::Width::Widest)
        .ok_or_else(|| format!("{} offers no f32 input config", candidate.name))?;
    let channels = cfg.channels as usize;

    // Sums, peaks and a frame count since the last line. A lock in an audio
    // callback is wrong in an engine; this is a meter, and the other side
    // holds it for a few hundred nanoseconds ten times a second.
    struct Heard {
        sums: Vec<f64>,
        peaks: Vec<f32>,
        frames: u64,
    }
    let heard = Arc::new(Mutex::new(Heard { sums: vec![0.0; channels], peaks: vec![0.0; channels], frames: 0 }));

    let stream = {
        let heard = heard.clone();
        device.build_input_stream(
            &cfg,
            move |data: &[f32], _: &cpal::InputCallbackInfo| {
                let frames = data.len() / channels;
                if let Ok(mut heard) = heard.lock() {
                    for frame in data.chunks_exact(channels) {
                        for (channel, sample) in frame.iter().enumerate() {
                            heard.sums[channel] += *sample as f64;
                            heard.peaks[channel] = heard.peaks[channel].max(sample.abs());
                        }
                    }
                    heard.frames += frames as u64;
                }
            },
            |e| eprintln!("stream error: {}", e),
            None,
        )?
    };
    stream.play()?;

    let out = std::io::stdout();
    writeln!(out.lock(), r#"{{"rate":{},"channels":{}}}"#, cfg.sample_rate.0, channels)?;
    let started = Instant::now();
    while started.elapsed() < Duration::from_secs(opts.seconds) {
        std::thread::sleep(JSON_EVERY);
        let (means, peaks) = {
            let mut heard = heard.lock().map_err(|_| "the meter's lock was poisoned")?;
            if heard.frames == 0 {
                continue;
            }
            let frames = heard.frames as f64;
            let means: Vec<String> = heard.sums.iter().map(|sum| format!("{:.6}", sum / frames)).collect();
            let peaks: Vec<String> = heard.peaks.iter().map(|peak| format!("{:.6}", peak)).collect();
            heard.sums.iter_mut().for_each(|sum| *sum = 0.0);
            heard.peaks.iter_mut().for_each(|peak| *peak = 0.0);
            heard.frames = 0;
            (means, peaks)
        };
        let mut out = out.lock();
        // A reader that has gone away is the ordinary way for this to end.
        if writeln!(out, r#"{{"mean":[{}],"peak":[{}]}}"#, means.join(","), peaks.join(",")).is_err() || out.flush().is_err() {
            break;
        }
    }
    drop(stream);
    Ok(())
}

pub fn run(opts: Opts) -> Result<(), Box<dyn Error>> {
    if opts.json {
        return run_json(opts);
    }
    let candidate = crate::devices::find(&opts.device)?;
    let device = candidate.device;

    let cfg = crate::measure::choose_input(
        &device,
        0,
        opts.sample_rate,
        crate::measure::Width::Widest,
    )
    .ok_or_else(|| format!("{} offers no f32 input config", candidate.name))?;
    let channels = cfg.channels as usize;
    let sr = cfg.sample_rate.0;

    println!(
        "{} — {} input channels at {} Hz\n",
        candidate.name, channels, sr
    );
    println!("Play into one jack at a time and watch which channel moves.");
    println!("Running for {}s; Ctrl-C to stop early.\n", opts.seconds);

    // One f32-as-bits peak per channel, max-ed in the callback and drained by
    // the printing thread. Positive IEEE-754 floats order the same as their bit
    // patterns, so fetch_max on the bits is a real max.
    let peaks: Arc<Vec<AtomicU32>> =
        Arc::new((0..channels).map(|_| AtomicU32::new(0)).collect());
    // Never cleared — this is the "did it clip at any point while I was not
    // looking" record, which is the one that matters when setting gain.
    let holds: Arc<Vec<AtomicU32>> =
        Arc::new((0..channels).map(|_| AtomicU32::new(0)).collect());

    let stream = {
        let peaks = peaks.clone();
        let holds = holds.clone();
        device.build_input_stream(
            &cfg,
            move |data: &[f32], _: &cpal::InputCallbackInfo| {
                let frames = data.len() / channels;
                for ch in 0..channels {
                    let mut peak = 0f32;
                    for f in 0..frames {
                        peak = peak.max(data[f * channels + ch].abs());
                    }
                    let bits = peak.to_bits();
                    peaks[ch].fetch_max(bits, Ordering::AcqRel);
                    holds[ch].fetch_max(bits, Ordering::AcqRel);
                }
            },
            |e| eprintln!("stream error: {}", e),
            None,
        )?
    };
    stream.play()?;

    let mut shown: Vec<f32> = vec![FLOOR_DB; channels];
    let started = Instant::now();
    let mut first = true;

    while started.elapsed() < Duration::from_secs(opts.seconds) {
        std::thread::sleep(Duration::from_millis(1000 / REFRESH_HZ));

        if !first {
            // Back up over the block we printed last time.
            print!("\x1b[{}A", channels);
        }
        first = false;

        let decay = DECAY_DB_PER_SEC / REFRESH_HZ as f32;
        for ch in 0..channels {
            let peak = f32::from_bits(peaks[ch].swap(0, Ordering::AcqRel));
            let hold = f32::from_bits(holds[ch].load(Ordering::Acquire));
            let now = db(peak);
            shown[ch] = if now > shown[ch] { now } else { (shown[ch] - decay).max(FLOOR_DB) };

            let hold_db = db(hold);
            let hold_txt = if hold_db <= FLOOR_DB {
                "      —".to_string()
            } else if hold_db >= -0.1 {
                "  CLIP!".to_string()
            } else {
                format!("{:>7.1}", hold_db)
            };

            println!(
                "  ch {:>2}  {}  {:>7.1} dBFS   peak {}\x1b[K",
                ch,
                bar(shown[ch]),
                shown[ch],
                hold_txt
            );
        }
    }

    drop(stream);

    // The summary reports every channel at full range, including below the
    // meter's floor, because the distinction that matters here is invisible on
    // a meter: a channel sitting at *exactly* zero is not routed to the host at
    // all, while one at -95 dBFS is a live converter with nothing plugged into
    // it. The first is a configuration problem in the interface's own mixer;
    // the second is just a quiet room. Both look like silence.
    println!("\nPeak held per channel over the whole run:");
    let mut live = 0;
    let mut dead = 0;
    for ch in 0..channels {
        let peak = f32::from_bits(holds[ch].load(Ordering::Acquire));
        if peak == 0.0 {
            dead += 1;
            println!("  ch {:>2}   digital zero — not routed to the host", ch);
        } else {
            live += 1;
            println!("  ch {:>2}  {:>7.1} dBFS", ch, db(peak));
        }
    }

    if dead == channels {
        println!(
            "\n  Every channel is a hard zero. Nothing analog is reaching the host,\n  \
             which is an internal-routing question for the interface's own mixer\n  \
             rather than anything to do with cables."
        );
    } else if dead > 0 {
        println!(
            "\n  {} channel(s) carried signal or noise, {} were hard zeros. The zeros\n  \
             are either unrouted or genuinely muted upstream.",
            live, dead
        );
    }
    Ok(())
}
