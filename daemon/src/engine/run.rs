//! Starting the engine: the residual, the arena, the two streams, and the
//! supervisor that puts the device back when it goes.
//!
//! Split out of `engine.rs` on 2026-09-06 (REVIEW-daemon-debt step 1).

use cpal::traits::{DeviceTrait, StreamTrait};
use std::error::Error;
use std::io::IsTerminal;
use std::path::PathBuf;
use std::time::Duration;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use rand::rngs::SmallRng;
use rand::SeedableRng;

use crate::measure::{Width, choose_input, choose_output};

use super::{ARM_REACH_MS, CHANNELS, db_to_mag, IDLE, MAX_FADE_MS, NO_ANCHOR, Opts, PLAYING, Source};
use super::callbacks;
use super::control::{control_loop, spawn_closer};
use super::edit::thread_blank;
use super::loop_state::Loop;
use super::selftest::selftest;
use super::shared::Shared;

/// The stream error callback, latching device loss so the supervisor can act.
///
/// One per stream because cpal takes ownership of each.
fn err_cb(sh: Arc<Shared>) -> impl FnMut(cpal::StreamError) + Send + 'static {
    move |e| {
        eprintln!("stream error: {}", e);
        sh.device_lost.store(true, Ordering::Release);
    }
}

/// The residual in force, and where it came from.
///
/// The second half is not decoration. The residual is a *measurement*, it moves
/// when another client opens the device, and the failure mode is that nobody
/// notices — so the engine says which of the three sources it used every time it
/// starts, and admits when it is guessing.
pub(crate) struct Residual {
    pub samples: f64,
    pub source: String,
    /// What had the device open when the number was measured, if it is stored.
    /// Kept so the operator can compare it with what is running now; the
    /// comparison is `deepstar latency check`'s job, not the audio daemon's.
    pub clients: Option<String>,
}

/// Where DeepStar leaves the calibration it curates.
///
/// The canonical artefact is in Amphora, content-addressed, alongside the VCO
/// tables — this is its projection onto the filesystem, so the audio daemon
/// needs no HTTP client and starts with no dependency on a store being up. Same
/// division as everywhere else in the rig: the store holds the truth, and what
/// the realiser reads is compiled output.
///
/// Deliberately not JSON. It is a handful of scalars that a person reads exactly
/// once — at the moment they suspect it — and `residual_samples = 275` is more
/// use then than a brace.
pub(crate) fn calibration_path() -> Option<PathBuf> {
    std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".itajara").join("calibration.conf"))
}

pub(crate) fn resolve_residual(default: f64, given: bool, device: &str) -> Residual {
    // Given explicitly: the operator has measured for the configuration in
    // force and knows better than anything stored.
    if given {
        return Residual {
            samples: default,
            source: "--residual".into(),
            clients: None,
        };
    }
    if let Some(path) = calibration_path() {
        if let Ok(text) = std::fs::read_to_string(&path) {
            let mut fields = std::collections::HashMap::new();
            for line in text.lines() {
                let line = line.trim();
                if line.starts_with('#') || line.is_empty() {
                    continue;
                }
                if let Some((k, v)) = line.split_once('=') {
                    fields.insert(k.trim().to_string(), v.trim().to_string());
                }
            }
            // Keyed by device, because the residual is a property of the
            // interface and this rig has more than one. A calibration for
            // something else is not a calibration for this.
            let stored_device = fields.get("device").cloned().unwrap_or_default();
            let matches = stored_device.is_empty()
                || device.to_lowercase().contains(&stored_device.to_lowercase());
            if let (true, Some(v)) = (matches, fields.get("residual_samples")) {
                if let Ok(n) = v.parse::<f64>() {
                    return Residual {
                        samples: n,
                        source: format!(
                            "{} (measured {})",
                            path.display(),
                            fields
                                .get("measured_at")
                                .cloned()
                                .unwrap_or_else(|| "at an unrecorded time".into())
                        ),
                        clients: fields.get("clients").cloned(),
                    };
                }
            }
            if !matches {
                eprintln!(
                    "  calibration at {} is for {:?}, not {:?} — ignoring it.",
                    path.display(),
                    stored_device,
                    device
                );
            }
        }
    }
    Residual {
        samples: default,
        source: "the compiled default, which is an assumption".into(),
        clients: None,
    }
}

pub fn run(opts: Opts) -> Result<(), Box<dyn Error>> {
    let candidate = crate::devices::find(&opts.device)?;
    let device = candidate.device;

    // Said out loud at every start, because the whole failure mode here is a
    // number that quietly stopped being true. On 2026-08-19 the default was 23
    // samples short and nothing in the sound said so.
    let residual = resolve_residual(opts.residual, opts.residual_given, &candidate.name);
    println!(
        "Residual {:.0} samples, from {}.",
        residual.samples, residual.source
    );
    if let Some(clients) = &residual.clients {
        println!(
            "  measured with these also on the device: {}. \
             `deepstar latency check` compares that with what is running now.",
            clients
        );
    }

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
    let fixed_frames = opts.fixed_secs.map(|f| (f * sr_f).round() as usize).unwrap_or(0);
    if fixed_frames > max_frames {
        return Err(format!(
            "--fixed-secs {:.1} is longer than --max-secs {:.1}; a tape cannot outgrow the arena.",
            opts.fixed_secs.unwrap_or(0.0),
            opts.max_secs
        )
        .into());
    }
    let ring_len = (opts.ring_secs * sr_f).round() as usize;

    // **`--in-ch` becomes a source when nobody named any**, so an existing
    // command line keeps working and gets one mono source called "in".
    let sources: Vec<Source> = if opts.sources.is_empty() {
        vec![Source::mono("in", opts.in_ch)]
    } else {
        opts.sources.clone()
    };

    // A source naming a channel the device does not have would record silence
    // and say nothing, which is the shape of failure this engine exists to
    // refuse. Said at startup, where it can still be fixed.
    for s in &sources {
        for c in s.ch {
            if c >= in_channels {
                return Err(format!(
                    "source `{}` wants input channel {}, and {} has {}.",
                    s.name, c + 1, candidate.name, in_channels
                )
                .into());
            }
        }
    }

    println!("Device: {}", candidate.name);
    println!(
        "Playing output {}, at {} Hz. Sources: {}",
        opts.out_ch,
        sr,
        sources.iter().map(|s| s.describe()).collect::<Vec<_>>().join(", ")
    );
    println!(
        "Arena: {} loops x {} layers x {:.0} s x {} ch = {} MB.   \
         Pre-roll: {:.0} s x {} src x {} ch = {} MB.\n",
        opts.loops,
        opts.layers,
        opts.max_secs,
        CHANNELS,
        opts.loops * opts.layers * max_frames * CHANNELS * 4 / 1_048_576,
        opts.ring_secs,
        sources.len(),
        CHANNELS,
        ring_len * sources.len() * CHANNELS * 4 / 1_048_576
    );
    // **The footprint, said out loud and, on a terminal, asked about.** The
    // arena is committed lazily (see `zeroed_atomics`), so what is printed is
    // the ceiling rather than the cost — but a ceiling above physical memory
    // is a crash waiting for the loop that fills it, and that is refused. No
    // flag overrides the refusal: the source is right there for anyone who
    // wants to crash their own machine, and they can do it in their own name.
    let arena_len = opts
        .loops
        .checked_mul(opts.layers)
        .and_then(|n| n.checked_mul(max_frames))
        .and_then(|n| n.checked_mul(CHANNELS))
        .ok_or("that many loops, layers and seconds overflow the arena's arithmetic")?;
    let ring_elems = ring_len * sources.len() * CHANNELS;
    let total_bytes = (arena_len + ring_elems) as u64 * 4;
    let mb = |b: u64| b / 1_048_576;
    match physical_memory_bytes() {
        Some(phys) if total_bytes > phys => {
            return Err(format!(
                "{} MB of loop memory on a machine with {} MB: it will not fit. \
                 Fewer loops, fewer layers, or a shorter --max-secs.",
                mb(total_bytes),
                mb(phys)
            )
            .into());
        }
        Some(phys) if total_bytes > phys / 4 && !opts.yes => {
            let pct = total_bytes * 100 / phys;
            if std::io::stdin().is_terminal() {
                print!(
                    "That is {} MB, {}% of this machine's memory, committed only as loops fill. Go ahead? [y/N] ",
                    mb(total_bytes),
                    pct
                );
                let _ = std::io::Write::flush(&mut std::io::stdout());
                let mut line = String::new();
                let _ = std::io::stdin().read_line(&mut line);
                if !matches!(line.trim(), "y" | "Y" | "yes") {
                    return Err("stopped before allocating anything.".into());
                }
            } else {
                println!(
                    "({} MB is {}% of this machine's memory; no terminal to ask on, so going ahead — --yes says so on purpose.)",
                    mb(total_bytes),
                    pct
                );
            }
        }
        _ => {}
    }


    let sh = Arc::new(Shared {
        arena: zeroed_atomics(arena_len),
        max_frames,
        n_loops: opts.loops,
        max_layers: opts.layers,
        fixed_frames,
        ring: zeroed_atomics(ring_elems),
        ring_len,
        in_peak: (0..sources.len()).map(|_| AtomicU32::new(0)).collect(),
        sources,
        loops: (0..opts.loops).map(|_| Loop::new(opts.layers)).collect(),
        selected: AtomicUsize::new(0),
        anchor: AtomicUsize::new(NO_ANCHOR),
        out_frames: AtomicUsize::new(0),
        in_frames: AtomicUsize::new(0),
        k: AtomicI64::new(0),
        k_set: AtomicBool::new(false),
        p0: Mutex::new(None),
        buffer_frames: AtomicU32::new(0),
        click: AtomicBool::new(opts.click || opts.selftest.is_some()),
        preroll: AtomicUsize::new(
            (opts.preroll_ms / 1000.0 * sr_f).round().max(0.0) as usize,
        ),
        arm_thresh: AtomicU32::new(db_to_mag(opts.arm_db).to_bits()),
        arm_reach: AtomicUsize::new((ARM_REACH_MS / 1000.0 * sr_f).round() as usize),
        max_fade: (MAX_FADE_MS / 1000.0 * sr_f).round() as usize,
        monitor: AtomicBool::new(opts.monitor),
        out_peak: AtomicU32::new(0),
        p0_needed: AtomicBool::new(true),
        p0_frame: AtomicUsize::new(0),
        device_lost: AtomicBool::new(false),
        reopens: AtomicUsize::new(0),
        takes_dir: opts.takes_dir.clone(),
        ack: Mutex::new(String::new()),
        ack_seq: AtomicUsize::new(0),
        peaks: Mutex::new(String::new()),
        peaks_seq: AtomicUsize::new(0),
        link_micros: AtomicI64::new(0),
        link_beat: AtomicU64::new(0),
        link_tempo: AtomicU64::new(0),
        link_quantum: AtomicU64::new(0),
        link_frame: AtomicUsize::new(0),
        link_bar_frames: AtomicUsize::new(0),
        link_bar_origin: AtomicI64::new(0),
        launch_q: AtomicI64::new(-1),
        link_anchors: AtomicUsize::new(0),
        link_rejected: AtomicUsize::new(0),
    });

    if sh.fixed_frames > 0 {
        for li in 0..sh.n_loops {
            thread_blank(&sh, li, sh.fixed_frames);
        }
        println!(
            "Every loop is an empty {:.1} s tape (--fixed-secs): record, and it closes itself there.",
            sh.fixed_frames as f64 / sr_f
        );
    }

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
        // Seeded here rather than inside: `from_entropy` asks the operating
        // system, which is exactly the thing the callback may not do — and this
        // runs on the control thread, at stream build, where it costs nothing.
        // A fixed seed would make every session drop the same cycles, which is
        // the opposite of what anybody switches chance on for.
        let mut rng = SmallRng::from_entropy();
        // Per-buffer scratch, sized once here and carried into the callback,
        // which must not allocate: one placement gain and one fold flag per
        // loop, rewritten every buffer before they are read.
        let mut gains: Vec<(f32, f32)> = vec![(0.0, 0.0); sh.n_loops];
        let mut folds: Vec<bool> = vec![false; sh.n_loops];
        device.build_output_stream(
            &out_cfg,
            move |data: &mut [f32], info: &cpal::OutputCallbackInfo| {
                callbacks::output(&sh, ch, dual, out_channels, &mut rng, &mut gains, &mut folds, data, info)
            },
            err_cb(err_sh),
            None,
        )?
    };

    let in_stream = {
        // Cloned before the shadowing below moves `sh` into the callback.
        let err_sh = sh.clone();
        let sh = sh.clone();
        let residual = residual.samples;
        device.build_input_stream(
            &in_cfg,
            move |data: &[f32], info: &cpal::InputCallbackInfo| {
                callbacks::input(&sh, residual, in_channels, sr, sr_f, data, info)
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

    spawn_closer(sh.clone(), sr);

    if let Some(port) = opts.link_port {
        crate::link::spawn_listener(sh.clone(), sr, port);
    }

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
        // Whichever loop held the input, and every loop's pending request: an
        // outage invalidates all of them, not just the one that was recording.
        if let Some(li) = sh.recording_loop() {
            let lp = sh.lp(li);
            let n = lp.n_layers.load(Ordering::Acquire);
            sh.zero_layer(li, n);
            lp.state.set(if lp.loop_len.load(Ordering::Acquire) > 0 {
                PLAYING
            } else {
                IDLE
            });
            eprintln!("  the recording in progress on loop {} was dropped — it would have had a gap", li);
        }
        for li in 0..sh.n_loops {
            sh.lp(li).next.clear();
        }

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

/// A zeroed block the kernel commits lazily. `alloc_zeroed` hands back pages
/// that cost nothing until a loop is written into them, where writing zeros
/// element by element would touch — and so commit — every page at startup.
/// This is what lets `--loops 100 --max-secs 3600` start on a laptop and
/// cost only what gets recorded.
fn zeroed_atomics(n: usize) -> Vec<AtomicU32> {
    if n == 0 {
        return Vec::new();
    }
    let layout = std::alloc::Layout::array::<AtomicU32>(n).expect("arena size overflows a Layout");
    // SAFETY: an all-zero bit pattern is a valid `AtomicU32` (it is zero); the
    // layout is exactly the one `Vec<AtomicU32>` frees with; and the pointer
    // is checked before it is trusted.
    unsafe {
        let p = std::alloc::alloc_zeroed(layout) as *mut AtomicU32;
        if p.is_null() {
            std::alloc::handle_alloc_error(layout);
        }
        Vec::from_raw_parts(p, n, n)
    }
}

/// Physical memory, in bytes, where the platform will say.
fn physical_memory_bytes() -> Option<u64> {
    #[cfg(target_os = "macos")]
    {
        let out = std::process::Command::new("sysctl").args(["-n", "hw.memsize"]).output().ok()?;
        String::from_utf8(out.stdout).ok()?.trim().parse().ok()
    }
    #[cfg(target_os = "linux")]
    {
        let s = std::fs::read_to_string("/proc/meminfo").ok()?;
        let line = s.lines().find(|l| l.starts_with("MemTotal:"))?;
        let kb: u64 = line.split_whitespace().nth(1)?.parse().ok()?;
        Some(kb * 1024)
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        None
    }
}
