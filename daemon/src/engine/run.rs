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

use crate::measure::{Width, choose_input, choose_output, signed_secs};

use super::{
    ARM_REACH_MS, ARMED, CHANNELS, db_to_mag, ENV_BUCKETS, FIRE, FIRST, IDLE, MAX_FADE_MS, MULTIPLY,
    NO_ANCHOR, Opts, OVERDUB, PLAYING, Source,
};
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
                // Chance's generator, owned outright by the thread that rolls
                // it. No atomic and no sharing, because there is no sharing: the
                // mixer is the only thing that rolls, and it runs here.
                //
                // `SmallRng` is xoshiro256++ — pure arithmetic over its own
                // state, so it is as safe here as the `cos` next door. What must
                // never appear in a callback is `thread_rng()`, which reseeds
                // from the operating system every 64 KiB and so hides a
                // `getrandom` syscall at a moment nobody chose.
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
                // Every loop's pending transition, stamped to this frame. Six
                // `take`s a buffer, and each is a swap on an uncontended atomic.
                for li in 0..sh.n_loops {
                    let lp = sh.lp(li);
                    // Speed first, and at the buffer start, because adopting it
                    // reads the playhead and everything below may move `origin`.
                    if lp.cfg_armed.swap(false, Ordering::Acquire) {
                        lp.adopt(
                            base as i64,
                            lp.loop_len.load(Ordering::Acquire),
                            f64::from_bits(lp.cfg_speed.load(Ordering::Relaxed)),
                            lp.cfg_pend.load(Ordering::Relaxed),
                        );
                    }
                    // Decay, at the buffer start and for the same reason: it
                    // only changes at a pass boundary, so a `powi` per layer per
                    // buffer is free where per frame would not be.
                    lp.age(base as i64);
                    // Peek, not take: a request with a deadline in the future
                    // has to survive this buffer and be reconsidered on the
                    // next. Consuming first and re-arming would lose it if the
                    // control thread never looked again.
                    let pending = lp.request.get();
                    if pending == 0 {
                        continue;
                    }
                    let at = lp.request_at.load(Ordering::Acquire);
                    // Due if it has no deadline, or its deadline falls inside
                    // this buffer, or has already gone by — a deadline in the
                    // past means the control thread was late, and being late is
                    // not a reason to wait a whole cycle more.
                    if at != i64::MIN && at >= (base + frames) as i64 {
                        continue;
                    }
                    // The frame the transition belongs to. `origin` and
                    // `rec_from` are stamped with this rather than with the
                    // buffer start, which is what makes the alignment exact:
                    // the flag flips at buffer granularity, but everything
                    // downstream reads the frame.
                    let stamp = if at == i64::MIN { base as i64 } else { at.max(base as i64) };
                    lp.request.set(0);
                    lp.request_at.store(i64::MIN, Ordering::Release);
                    match pending {
                        ARMED => {
                            lp.reached.store(0, Ordering::Release);
                            lp.rec_reached.store(0, Ordering::Release);
                            // A level-armed recording knows the frame the sound
                            // crossed the threshold, and that frame is earlier
                            // than the one this request can be stamped at — the
                            // crossing is found on the input thread. Hand the
                            // difference to `started_late`, which is the same
                            // road a late footswitch already travels: `commit`
                            // shifts `origin` back by it and fills the front
                            // from the ring.
                            match lp.arm_from.swap(i64::MIN, Ordering::AcqRel) {
                                i64::MIN => {}
                                want => lp
                                    .started_late
                                    .store((stamp - want).max(0), Ordering::Release),
                            }
                            let n = lp.n_layers.load(Ordering::Acquire);
                            if n < sh.max_layers {
                                // **Layers, not length.** This asked whether the
                                // loop had a length, which was the same question
                                // while the only way to have one was to have
                                // recorded one. A loop can now be *sized and
                                // empty* — told how many bars it is before
                                // anything is played into it — and that is a
                                // first recording with a length, not an overdub
                                // of nothing.
                                if n == 0 {
                                    // Only the first recording lays down the grid.
                                    // Re-stamping origin on every arm would drag the
                                    // whole loop to position zero the instant you
                                    // hit record — playback reads origin too. The
                                    // self-test cannot catch that, because both
                                    // sides move together.
                                    //
                                    // Safe on a sized-and-empty loop for the same
                                    // reason it is unsafe elsewhere: there is no
                                    // audio, so there is nothing for zero to move
                                    // away from.
                                    lp.origin.store(stamp, Ordering::Release);
                                    lp.rec_from.store(stamp, Ordering::Release);
                                    lp.clear_rec_env();
                                    lp.threaded.store(false, Ordering::Relaxed);
                                    lp.state.set(FIRST);
                                    // If the length was known before a note was
                                    // played, the close is known too. Arm it here
                                    // and let `closer` do the work — an audio
                                    // callback must not be the thing that draws a
                                    // layer.
                                    let want = lp.loop_len.load(Ordering::Acquire);
                                    lp.rec_len.store(want, Ordering::Release);
                                    lp.close_at.store(
                                        if want > 0 { stamp + want as i64 } else { i64::MIN },
                                        Ordering::Release,
                                    );
                                } else {
                                    // An overdub is modular against the existing
                                    // grid, so it records from the same reference
                                    // the loop plays from.
                                    lp.rec_from
                                        .store(lp.origin.load(Ordering::Acquire), Ordering::Release);
                                    lp.clear_rec_env();
                                    lp.threaded.store(false, Ordering::Relaxed);
                                    // A one-pass overdub knows its close the
                                    // moment it starts: one loop length on.
                                    // Any other overdub clears the deadline,
                                    // because a `close_at` left from a pass
                                    // that was closed early by hand would
                                    // otherwise fire on this one.
                                    let len = lp.loop_len.load(Ordering::Acquire);
                                    lp.close_at.store(
                                        if lp.one_pass.swap(false, Ordering::AcqRel) && len > 0 {
                                            stamp + len as i64
                                        } else {
                                            i64::MIN
                                        },
                                        Ordering::Release,
                                    );
                                    lp.state.set(OVERDUB);
                                }
                            }
                        }
                        PLAYING => lp.state.set(PLAYING),
                        // **The one place `origin` moves.**
                        //
                        // Everywhere else in this engine a loop's zero is fixed
                        // at the moment it was recorded and stays there. That is
                        // what phase-locking means and it is why stopping a loop
                        // and starting it again puts it back where it would have
                        // been rather than where it began — the alternative,
                        // moving `origin`, is called out on `muted` as "the one
                        // thing that must never happen to a loop that closed on a
                        // grid boundary".
                        //
                        // A one-shot is the documented exception, and has to be:
                        // the entire gesture is *play this, from the top, now*.
                        // Which is also why the mode is a mode — a loop that can
                        // be fired is a loop that has given up its place in the
                        // phase-locked set, and that should be a thing you turn
                        // on rather than a thing a footswitch does to you.
                        FIRE => {
                            let len = lp.loop_len.load(Ordering::Acquire);
                            if len > 0 {
                                lp.origin.store(stamp, Ordering::Release);
                                // Backwards, the top of the pass is the *end*.
                                // Starting at zero and stepping negative wraps
                                // there anyway, one sample later and audibly.
                                let from = if lp.speed() < 0.0 { (len - 1) as f64 } else { 0.0 };
                                lp.warp.store(from.to_bits(), Ordering::Relaxed);
                                lp.shot_end
                                    .store(stamp + lp.pass_frames(len), Ordering::Release);
                                // A fired loop is audible by definition. Leaving
                                // `muted` set would make the switch do nothing
                                // for a reason nothing on screen could explain.
                                lp.muted.store(false, Ordering::Relaxed);
                            }
                        }
                        IDLE => {}
                        _ => {}
                    }
                }

                // **The click follows the grid, and ticks beats.**
                //
                // It followed the selected loop's cycle, one blip a time round,
                // and the note beside it said what to do about that: *when bar
                // quantisation lands, the click should follow Link instead —
                // that will be a grid rather than a guess.* It has landed.
                //
                // Two things were wrong with the old one and both bit the same
                // workflow. It needed a recorded loop to exist — `click_len > 0`
                // — so there was **no click before the first take**, which is
                // the one moment you need to count yourself in. And one blip a
                // cycle is not a count-in: four are, with the first one louder.
                //
                // Falls back to the selected loop when there is no grid at all,
                // which is a rig with no clock and nothing recorded — where the
                // old behaviour was the only answer available and still is.
                let (click_origin, click_len, click_beats) = match sh.grid() {
                    Some((o, bar)) => {
                        let q = f64::from_bits(sh.link_quantum.load(Ordering::Relaxed));
                        (o, bar, if q >= 1.0 { q.round() as usize } else { 4 })
                    }
                    None => {
                        let li = sh.sel();
                        (
                            sh.lp(li).origin.load(Ordering::Acquire),
                            sh.lp(li).loop_len.load(Ordering::Acquire),
                            1,
                        )
                    }
                };
                let click_beat = (click_len / click_beats.max(1)).max(1);

                // Monitoring reads the freshest frames the pre-roll holds. One
                // buffer behind the converters, so the interface's own direct
                // monitoring beats it — this is for headphones with nothing
                // else in the room.
                let monitor = sh.monitor.load(Ordering::Relaxed);
                let mon_from = sh.in_frames.load(Ordering::Acquire) as i64 - frames as i64;
                // **Monitor whatever is about to be written to.** Monitoring
                // exists to hear yourself while you play, and what you are
                // playing into is the loop that is armed or recording. Falls
                // back to the first source when nothing is, which is what a rig
                // with one source has always done.
                let mon_src = (0..sh.n_loops)
                    .find(|&li| {
                        let lp = sh.lp(li);
                        lp.is_armed() || lp.is_recording()
                    })
                    .map(|li| sh.src_of(li))
                    .unwrap_or(0);

                let mut peak = 0.0f32;
                // Once per buffer, not once per frame: six loops times two
                // trig calls is free here and wasteful inside the frame loop.
                for li in 0..sh.n_loops {
                    let lp = sh.lp(li);
                    // Level folded into the placement gains rather than applied
                    // in the frame loop: it is a per-buffer constant like the
                    // pan itself, and one multiply here is eight thousand fewer
                    // down there.
                    let v = f32::from_bits(lp.vol.load(Ordering::Relaxed));
                    // **Two different controls wearing one knob.** A loop that
                    // is folded to mono is a single signal being *placed*, so
                    // the equal-power pan is right. A loop that is not is two
                    // signals already in a field, and panning them would
                    // collapse it — what that knob means there is *balance*,
                    // which attenuates one side and leaves the other alone.
                    // See `Loop::mono` and `balance_gains`.
                    let fold = lp.mono.load(Ordering::Relaxed);
                    let (l, r) = if fold { lp.pan_gains() } else { lp.balance_gains() };
                    folds[li] = fold;
                    gains[li] = (l * v, r * v);
                }

                // Edits that have settled restart their pass here, at the
                // frame they asked for: position zero of the cycle lands on
                // it, and any speed offset is dropped so that is exact.
                for li in 0..sh.n_loops {
                    let lp = sh.lp(li);
                    let at = lp.edit_restart.load(Ordering::Acquire);
                    if at != 0 && base as i64 + frames as i64 > at {
                        if lp.pend_set.load(Ordering::Acquire) {
                            lp.win_in.store(lp.pend_in.load(Ordering::Relaxed), Ordering::Relaxed);
                            lp.win_out.store(lp.pend_out.load(Ordering::Relaxed), Ordering::Relaxed);
                            lp.rot.store(lp.pend_rot.load(Ordering::Relaxed), Ordering::Relaxed);
                            lp.pend_set.store(false, Ordering::Release);
                        }
                        lp.origin.store(at, Ordering::Release);
                        lp.warp.store(0.0f64.to_bits(), Ordering::Relaxed);
                        lp.edit_restart.store(0, Ordering::Release);
                    }
                }

                for f in 0..frames {
                    let out_frame = (base + f) as i64;
                    let mut vl = 0.0f32;
                    let mut vr = 0.0f32;

                    for li in 0..sh.n_loops {
                        let s = sh.loop_at(li, out_frame, &mut rng, true);
                        if folds[li] {
                            // Averaged rather than summed: two channels of the
                            // same performance are correlated, so adding them
                            // would be 6 dB louder than either and a fold would
                            // change the level as well as the width.
                            let m = (s[0] + s[1]) * 0.5;
                            vl += m * gains[li].0;
                            vr += m * gains[li].1;
                        } else {
                            vl += s[0] * gains[li].0;
                            vr += s[1] * gains[li].1;
                        }
                    }
                    // The click sits in the middle. It is a reference, not
                    // material, and a reference that moves is not one.
                    let mut v = 0.0f32;
                    if click_len > 0 && sh.click.load(Ordering::Relaxed) {
                        let pos = (out_frame - click_origin).rem_euclid(click_len as i64) as usize;
                        // The downbeat is louder, which is the whole of what
                        // makes four blips a count-in rather than a rattle.
                        if pos < 16 {
                            v += 0.5;
                        } else if pos % click_beat < 16 {
                            v += 0.22;
                        }
                    }
                    vl += v;
                    vr += v;

                    // **The monitor keeps its sides**, where the click does not:
                    // it is the thing you are about to record, so hearing it
                    // collapsed would be hearing something other than what
                    // lands in the loop.
                    if monitor {
                        if let Some(m) = sh.ring_at(mon_src, mon_from + f as i64, 0) {
                            vl += m;
                        }
                        if let Some(m) = sh.ring_at(mon_src, mon_from + f as i64, 1) {
                            vr += m;
                        }
                    }
                    peak = peak.max(vl.abs()).max(vr.abs());
                    data[f * out_channels + ch] = vl;
                    if dual && ch + 1 < out_channels {
                        data[f * out_channels + ch + 1] = vr;
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
        let residual = residual.samples;
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

                // **Every source, always, regardless of transport state.**
                // This is what makes the past claimable — and it is why the
                // source is a per-loop choice rather than a rig-wide one. A
                // moment you did not know you wanted has to have been captured
                // on whichever input it happened on.
                for (si, src) in sh.sources.iter().enumerate() {
                    let mut peak = 0.0f32;
                    for f in 0..frames {
                        let i = (si * sh.ring_len + (base + f) % sh.ring_len) * CHANNELS;
                        for ch in 0..CHANNELS {
                            let v = data[f * in_channels + src.ch[ch]];
                            peak = peak.max(v.abs());
                            sh.ring[i + ch].store(v.to_bits(), Ordering::Relaxed);
                        }
                    }
                    sh.in_peak[si].fetch_max(peak.to_bits(), Ordering::Relaxed);
                }

                // A level-armed loop is *listening*, not recording — it is not
                // `recording_loop()` and nothing below will write for it. What it
                // needs is the frame the sound crossed the threshold, found here
                // because this is the only place that sees individual input
                // frames. Per-buffer would do at 21 ms granularity, but the
                // frames are already in hand and the crossing is the one number
                // the whole mode turns on.
                //
                // The crossing is not the start of the note, so the recording is
                // dated `ARM_REACH_MS` before it. That costs nothing: the ring
                // has been running since the daemon started.
                if let Some(li) = sh.armed_loop() {
                    let thresh = f32::from_bits(sh.arm_thresh.load(Ordering::Relaxed));
                    // **The armed loop's own input, not the rig's.** Arm a drum
                    // loop and it should wait for a drum; arm a guitar loop and
                    // it should wait for a guitar. One shared peak would have
                    // each of them starting on the other, which is the sort of
                    // thing you would blame on the threshold for an hour.
                    let asrc = &sh.sources[sh.src_of(li)];
                    let apeak = f32::from_bits(sh.in_peak[sh.src_of(li)].load(Ordering::Relaxed));
                    if apeak >= thresh {
                        if let Some(f) = (0..frames).find(|&f| {
                            (0..CHANNELS)
                                .any(|c| data[f * in_channels + asrc.ch[c]].abs() >= thresh)
                        })
                        {
                            let lp = sh.lp(li);
                            let k = sh.k.load(Ordering::Acquire);
                            let at = (base + f) as i64 + k;
                            // Quantised wins, as it does for a footswitch: a loop
                            // told to start on the grid starts on the grid,
                            // whatever asked for it. There is no back-dating then
                            // — the boundary is ahead, not behind.
                            match if lp.quant.load(Ordering::Relaxed) {
                                sh.next_boundary(at)
                            } else {
                                None
                            } {
                                Some(t) => {
                                    lp.arm_from.store(i64::MIN, Ordering::Release);
                                    lp.request_at.store(t, Ordering::Release);
                                }
                                None => {
                                    let reach = sh.arm_reach.load(Ordering::Relaxed) as i64;
                                    lp.arm_from.store(at - reach, Ordering::Release);
                                    lp.request_at.store(i64::MIN, Ordering::Release);
                                }
                            }
                            lp.request.set(ARMED);
                        }
                    }
                }

                // Which loop the input belongs to, asked rather than remembered.
                // There is one converter, so at most one loop can be recording;
                // a separate "who has the input" field would be a second source
                // of truth able to disagree with the loops' own states.
                let Some(li) = sh.recording_loop() else {
                    sh.in_frames.store(base + frames, Ordering::Release);
                    return;
                };
                let lp = sh.lp(li);
                let state = lp.state.get();

                let k = sh.k.load(Ordering::Acquire);
                let origin = lp.rec_from.load(Ordering::Acquire);
                let loop_len = lp.loop_len.load(Ordering::Acquire);
                let layer = lp.n_layers.load(Ordering::Acquire);
                let revox = lp.revox.load(Ordering::Relaxed);
                // Whether the playhead is anywhere other than where a linear
                // write would put it. Once a buffer, because it is a property of
                // the loop and not of a frame — and `plain` is the same question
                // every writer in this file has always asked, so the two cannot
                // come to disagree about what unity means.
                let moving = !lp.plain();
                let fb = f32::from_bits(lp.fb.load(Ordering::Relaxed));
                // The one-pole's coefficient, worked out once a buffer rather
                // than once a frame — it only changes when the knob does.
                let tone = f32::from_bits(lp.tone.load(Ordering::Relaxed));
                let tone_a = if tone >= 20_000.0 {
                    1.0
                } else {
                    1.0 - (-2.0 * std::f32::consts::PI * tone / sr as f32).exp()
                };
                // The recording loop's own input, and its own tape memory per
                // channel. Revox's one-pole runs along the tape, so the two
                // sides need separate memories or the filter would cross-feed
                // them and the stereo would collapse a little on every pass.
                let rec_src = &sh.sources[sh.src_of(li)];
                let mut lp_mem = [f32::from_bits(lp.tape_lp.load(Ordering::Relaxed)); CHANNELS];
                if layer >= sh.max_layers && !revox {
                    sh.in_frames.store(base + frames, Ordering::Release);
                    return;
                }

                for f in 0..frames {
                    let out_frame = (base + f) as i64 + k;
                    let rel = out_frame - origin;
                    if rel < 0 {
                        continue;
                    }
                    if state == FIRST || state == MULTIPLY {
                        // Linear. Its length becomes the cycle, so it must not
                        // wrap — and it stops rather than overwriting.
                        let pos = rel as usize;
                        if pos >= sh.max_frames {
                            lp.overflowed.store(true, Ordering::Relaxed);
                            continue;
                        }
                        let mut loudest = 0.0f32;
                        for ch in 0..CHANNELS {
                            let v = data[f * in_channels + rec_src.ch[ch]];
                            sh.write(li, layer, pos, ch, v);
                            loudest = loudest.max(v.abs());
                        }
                        lp.reached.fetch_max(pos + 1, Ordering::Relaxed);
                        lp.rec_reached.fetch_max(out_frame + 1, Ordering::Relaxed);
                        // **A first take has no length yet**, so its picture
                        // cannot be laid out against one. It is drawn against
                        // the arena instead and rescales itself when the loop
                        // closes — which is what a tape counter does, and it
                        // means the bar fills left to right as you play rather
                        // than sitting empty until you stop.
                        lp.mark_rec_env(pos * ENV_BUCKETS / sh.max_frames, loudest);
                    } else {
                        // Modular: an overdub may go round as many times as it
                        // likes, summing into the same cycle.
                        if loop_len == 0 {
                            continue;
                        }
                        // **The write head follows the PLAY head.**
                        //
                        // At unity the two are the same ramp, and this is the
                        // fast branch that has always been here: one input
                        // frame, one slot. At any other rate they are not, and
                        // a linear write would put what you played somewhere
                        // you never heard it — which is why recording into a
                        // loop at speed used to be refused outright rather than
                        // done wrongly.
                        //
                        // The moving branch below spans instead of picking. One
                        // input frame covers an interval of the loop, and it is
                        // added to every slot that interval touches, weighted by
                        // how much of that slot it covers. That one rule gives
                        // all three cases without a case for any of them:
                        //
                        //   - **backwards** walks the interval down, one slot to
                        //     one frame, exactly and with no resampling at all;
                        //   - **half speed** has two input frames sharing a slot
                        //     at half weight each, which is their average — so
                        //     what comes back is what you played, not twice as
                        //     loud;
                        //   - **double speed** has one input frame filling two
                        //     slots at full weight, a zero-order hold.
                        //
                        // And at unity the interval is exactly one slot at
                        // weight one, which is the branch above — so the common
                        // path keeps its single write and this cannot change
                        // what it does.
                        if moving {
                            let a = lp.raw_pos(out_frame);
                            let b = lp.raw_pos(out_frame + 1);
                            let (lo, hi) = if a <= b { (a, b) } else { (b, a) };
                            let mut slot = lo.floor() as i64;
                            let mut first = true;
                            // A stopped loop has `hi == lo` and writes nothing,
                            // which is right: there is nowhere for it to go.
                            while (slot as f64) < hi {
                                let cover = (((slot + 1) as f64).min(hi)
                                    - (slot as f64).max(lo))
                                    .max(0.0) as f32;
                                if cover > 0.0 {
                                    let Some(p) = lp.write_pos(slot, loop_len) else {
                                        slot += 1;
                                        continue;
                                    };
                                    for ch in 0..CHANNELS {
                                        let v = data[f * in_channels + rec_src.ch[ch]];
                                        sh.add(li, layer, p, ch, v * cover);
                                    }
                                    if first {
                                        let loudest = (0..CHANNELS)
                                            .map(|ch| {
                                                data[f * in_channels + rec_src.ch[ch]].abs()
                                            })
                                            .fold(0.0f32, f32::max);
                                        lp.mark_rec_env(
                                            p * ENV_BUCKETS / loop_len,
                                            loudest,
                                        );
                                        first = false;
                                    }
                                }
                                slot += 1;
                            }
                            lp.reached.fetch_max(loop_len, Ordering::Relaxed);
                            lp.rec_reached.fetch_max(out_frame + 1, Ordering::Relaxed);
                            continue;
                        }
                        // Through the window and the rotation: the slot the
                        // play head is on, or none, past an end.
                        let Some(pos) = lp.write_pos(rel, loop_len) else {
                            continue;
                        };
                        // **Revox writes over the tape; everything else writes
                        // beside it.** In Revox mode there is one layer by
                        // construction and the overdub goes into *that*, not
                        // into a new one — which is why `layer` is zero here and
                        // why the loop does not grow a layer per pass.
                        for ch in 0..CHANNELS {
                            let v = data[f * in_channels + rec_src.ch[ch]];
                            if revox {
                                // What is on the tape, dulled, quieter, with the
                                // new sound on top of it. The filter runs along
                                // the tape rather than along time, which is the
                                // same thing while the head is moving and the
                                // reason the memory survives the wrap.
                                let cur = sh.read(li, 0, pos, ch);
                                lp_mem[ch] += tone_a * (cur - lp_mem[ch]);
                                sh.write(li, 0, pos, ch, lp_mem[ch] * fb + v);
                            } else {
                                sh.add(li, layer, pos, ch, v);
                            }
                        }
                        lp.reached.fetch_max(loop_len, Ordering::Relaxed);
                        lp.rec_reached.fetch_max(out_frame + 1, Ordering::Relaxed);
                        // An overdub already knows the cycle it is going round,
                        // so its picture is laid out against that and fills in
                        // wherever the playhead is — including on the second and
                        // third time round, which is why this is a peak and not
                        // a store.
                        let loudest = (0..CHANNELS)
                            .map(|ch| data[f * in_channels + rec_src.ch[ch]].abs())
                            .fold(0.0f32, f32::max);
                        lp.mark_rec_env(pos * ENV_BUCKETS / loop_len, loudest);
                    }
                }
                // One stored memory for two, which is a small lie that costs
                // nothing: it is the seed for the next buffer's filter and the
                // two sides are within a sample of each other by construction.
                lp.tape_lp.store(lp_mem[0].to_bits(), Ordering::Relaxed);
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
            sh.lp(li).request.take();
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
