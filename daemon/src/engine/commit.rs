//! Closing a recording: `commit`, the layer it draws, and the ring it fills
//! the front from — and `take`, which is a commit of the past.
//!
//! Split out of `engine.rs` on 2026-09-06 (REVIEW-daemon-debt step 1).

use std::time::Duration;
use std::sync::atomic::Ordering;

use super::{CHANNELS, Phase, Shape};
use super::shared::Shared;

pub(crate) fn commit(sh: &Shared, li: usize, sr: u32, late: i64) -> String {
    let lp = sh.lp(li);
    let state = lp.phase();
    if state != Phase::First && state != Phase::Overdub {
        // The callers only reach here from FIRST or OVERDUB, so this is a guard
        // rather than a path — but it answers anyway. Returning nothing is how
        // fourteen verbs became invisible, and a guard is exactly the sort of
        // thing that stops being unreachable without anyone noticing.
        return format!("loop {} is not recording.", li);
    }

    // The frame the foot went down on. Taken before anything below sleeps —
    // the quantised path waits for a boundary, which would move it.
    let closed_at = sh.out_frames.load(Ordering::Acquire) as i64 - late.max(0);

    // A quantised first recording gets a length that is a whole number of grid
    // cycles, decided here rather than taken from what happened to be captured.
    // Rounding to nearest means a press slightly late loses the overhang and a
    // press slightly early waits — which is right, because the intent was a
    // whole number of cycles either way, and a human aiming at a boundary
    // misses it in both directions.
    //
    // The wait happens BEFORE the state flips, so the loop keeps recording up
    // to the boundary. Flipping first and waiting after would hand back a loop
    // whose last fraction of a cycle is silence.
    let quantised_len = if state == Phase::First && lp.quant.load(Ordering::Relaxed) {
        sh.grid().and_then(|(_, glen)| {
            let from = lp.origin.load(Ordering::Acquire);
            let cur = closed_at;
            let elapsed = (cur - from).max(0) as f64;
            let n = ((elapsed / glen as f64).round() as usize).max(1);
            let len = n * glen;
            if len > sh.max_frames {
                println!("  {} grid cycles would exceed --max-secs; closing free.", n);
                return None;
            }
            let target = from + len as i64;
            if target > cur {
                println!(
                    "  waiting {:.2} s for the grid boundary ({} cycle{}).",
                    (target - cur) as f64 / sr as f64,
                    n,
                    if n == 1 { "" } else { "s" }
                );
                while (sh.out_frames.load(Ordering::Acquire) as i64) < target {
                    std::thread::sleep(Duration::from_millis(5));
                }
            }
            Some(len)
        })
    } else {
        None
    };

    // Frames of continuation past this layer's end, filled in by whichever
    // branch below runs and handed to `set_layer_shape` at the bottom — one
    // place that decides a layer's shape, rather than two that each remember to
    // set part of it.
    let mut tail = 0usize;

    // Let the input drain: it trails the output by K, so the last frames of the
    // loop have not arrived yet. Without this the tail of every recording is
    // missing, which is exactly the kind of fault that sounds like "feel".
    lp.enter(Phase::Playing, sh.out_frames.load(Ordering::Acquire) as i64);
    std::thread::sleep(Duration::from_millis(60));

    if state == Phase::First {
        let reached = lp.reached.load(Ordering::Acquire);
        // **Asked for beats counted.** Taken rather than read, so a take that is
        // closed by a foot instead of by `closer` cannot leave it armed for the
        // next one.
        let declared = lp.rec_len.swap(0, Ordering::AcqRel);
        let mut len = quantised_len.or(if declared > 0 { Some(declared) } else { None })
        .unwrap_or_else(|| {
            if late <= 0 {
                return reached;
            }
            // What was played, rather than what was captured. The frames after
            // the press stay in the arena and are simply never read: playback
            // is `pos % len`, so anything past the end does not exist.
            let origin = lp.origin.load(Ordering::Acquire);
            let want = (closed_at - origin).max(0) as usize;
            // Only ever shorter. If the input has not caught up to the press —
            // it trails the output by K — then `reached` is the honest answer
            // and claiming further would claim silence.
            want.min(reached)
        });
        if declared > 0 && reached < declared {
            // Should not happen — `commit` sleeps a drain before reading
            // `reached` precisely so the input can catch up — but if it ever
            // does, the last few frames of the loop are silence rather than
            // audio. Said out loud rather than quietly shortening the loop,
            // because shortening it is the failure this is here to prevent.
            println!(
                "  input was {} frames behind the declared length; the tail is silent.",
                declared - reached
            );
        }
        if len == 0 {
            return format!("loop {} recorded nothing.", li);
        }
        if late > 0 && quantised_len.is_none() && len < reached {
            println!(
                "  closed as of the press, {:.0} ms before the command: {} frames dropped.",
                late as f64 / sr as f64 * 1000.0,
                reached - len
            );
        }
        // Pre-roll: a tap is always a little late, so back-date the loop's start
        // and fill the front from the ring. The attack that would have been
        // clipped off is already captured; it just has to be claimed.
        // Never for a quantised loop: the pre-roll shifts `origin` backwards to
        // reclaim the attack, and moving origin is exactly what must not happen
        // to a loop that was started on a boundary. Alignment beats the last
        // few milliseconds of the attack, and a loop that drifts off the grid
        // by its pre-roll would be a bug nobody could see the cause of.
        // Measured beats configured: `started_late` is how late the press that
        // began this recording actually was, where `--preroll-ms` is a guess
        // applied to every take alike. Falls back to the guess when nothing
        // measured it, so a rig that cannot time its presses still works.
        let pre = if quantised_len.is_some() {
            0
        } else {
            let measured = lp.started_late.load(Ordering::Acquire);
            if measured > 0 {
                measured as usize
            } else {
                sh.preroll.load(Ordering::Acquire)
            }
        };
        let layer = lp.n_layers.load(Ordering::Acquire);
        let origin = lp.origin.load(Ordering::Acquire);
        let new_origin = origin - pre as i64;
        if pre > 0 && reached.max(len) + pre > sh.max_frames {
            // Shifting anyway would run off the end of this layer's slice and
            // into the next one's, which is silent corruption rather than an
            // error. Refuse instead.
            println!(
                "  pre-roll skipped: the loop plus pre-roll would exceed --max-secs."
            );
        } else if pre > 0 && new_origin >= 0 {
            // Shift what was recorded up by `pre`, backwards so the move does
            // not eat its own tail, then fill the vacated front from the ring.
            //
            // **Everything recorded, not just the loop.** The frames past `len`
            // are the continuation — what the player kept playing while the
            // gesture was still being worked out — and shifting only the loop
            // would leave them a `pre` behind and overlapped by the shifted
            // material. They are never sounded, so nothing would have said so.
            let moved = reached.max(len).min(sh.max_frames - pre);
            for pos in (0..moved).rev() {
                for ch in 0..CHANNELS {
                    let v = sh.read(li, layer, pos, ch);
                    sh.write(li, layer, pos + pre, ch, v);
                }
            }
            for pos in 0..pre {
                for ch in 0..CHANNELS {
                    sh.write(li, layer, pos, ch, 0.0);
                }
            }
            let got = fill_from_ring(sh, li, layer, new_origin, pre, 0, false);
            lp.origin.store(new_origin, Ordering::Release);
            // **A declared length is a promise about length; the pre-roll is
            // about where the loop starts.**
            //
            // Growing `len` here is right for a take whose length came from
            // what was played — you keep everything, plus the attack that was
            // clipped off the front. It is wrong for a take that was *told* how
            // long to be. Recipe 2 asks for four bars, arms, and plays: the
            // close fires at exactly four bars, and then this line added the
            // hundred milliseconds of recovered attack on top, so an 8.000 s
            // loop committed at 8.1 and sat beside the 8.000 s loop it was
            // supposed to match. Andrew saw it as two slots reading 8.0 and 8.1.
            //
            // Not by closing earlier, which was the other candidate: that would
            // spend the pre-roll out of the take instead of off the end, and
            // leave nothing past the loop point for the wrap crossfade to reach
            // into. Keeping the length shifts the last `pre` frames past the
            // end, where `tail` picks them up and `sample_at` uses them — the
            // material is not discarded, it becomes the continuation.
            if declared == 0 {
                len += pre;
            }
            println!(
                "  pre-roll: {:.0} ms recovered from before the tap ({} of {} frames).",
                pre as f64 / sr as f64 * 1000.0,
                got,
                pre
            );
        }
        lp.loop_len.store(len, Ordering::Release);
        // **And how many bars that is**, where anything knows what a bar is.
        //
        // `commit` set a length and never a bar count, which was invisible
        // while the count only mattered to loops that had been *told* one:
        // `cycles` is zero for a freely recorded loop and zero reads as one
        // everywhere. So an eight-second take showed "1 bar" on the encoder,
        // and `bpm` on it would have offered a tempo four times too slow.
        //
        // Rounded to the nearest, and at least one. A take aimed at four bars
        // misses in both directions and the nearest is what was meant; a take
        // shorter than a bar is one bar, because zero is the value that means
        // "nobody has said" and this is somebody saying.
        //
        // Only with a clock. Without one the first loop *is* the pulse and its
        // whole length is one cycle — the clockless behaviour `loop_grid`
        // depends on — so writing a count here would be inventing a metre from
        // nothing.
        let bar = sh.link_bar_frames.load(Ordering::Relaxed);
        if bar > 0 && len > 0 {
            let bars = ((len as f64 / bar as f64).round() as usize).max(1);
            lp.cycles.store(bars, Ordering::Release);
        }
        // What was recorded past the end, kept rather than trimmed. A first
        // recording writes linearly, so the continuation is already sitting
        // there and costs nothing to keep — it only had to not be thrown away.
        tail = (reached.max(len) + if quantised_len.is_some() { 0 } else { pre })
            .saturating_sub(len);
        // The first loop to acquire a length becomes the grid the rest
        // can align to — first rather than chosen, because that is how a
        // looper has always worked: what you played first is what the
        // rest fits around. A compare-exchange, so later calls are no-ops.
        sh.claim_anchor(li);
        println!(
            "  loop set: {} frames ({:.3} s), {:.1} bpm if that is one bar of 4/4",
            len,
            len as f64 / sr as f64,
            240.0 / (len as f64 / sr as f64)
        );
    }
    // An overdub is modular, so the frames recorded after the press did not
    // land past the end — they wrapped and SUMMED onto the head of their own
    // layer. That is a doubled transient at the loop point, not a length error,
    // and it is why an overdub needs undoing where a first recording only
    // needed measuring.
    //
    // Undone exactly, because the ring holds the very samples that were added:
    // subtract them where they landed, and write them where they belong — past
    // the end, as the continuation, the same place a first recording keeps it.
    // The material is not discarded, because it is the thing a seamless loop is
    // made of.
    if state == Phase::Overdub && late > 0 {
        let layer = lp.n_layers.load(Ordering::Acquire);
        let len = lp.loop_len.load(Ordering::Acquire);
        let k = sh.k.load(Ordering::Acquire);
        let rec_from = lp.rec_from.load(Ordering::Acquire);
        // The furthest output frame the input actually reached. From the
        // callback, not from a clock: `in_frames` keeps advancing after the
        // flip to PLAYING, so it names frames that were never recorded, and
        // subtracting those would gouge real audio out of the loop head.
        let last = lp.rec_reached.load(Ordering::Acquire);
        let mut undone = 0usize;
        let mut kept = 0usize;
        let src = sh.src_of(li);
        if len > 0 {
            for f in closed_at..last {
                let Some(v0) = sh.ring_at(src, f - k, 0) else { continue };
                let pos = (f - rec_from).rem_euclid(len as i64) as usize;
                let at = len + (f - closed_at) as usize;
                for ch in 0..CHANNELS {
                    let v = if ch == 0 {
                        v0
                    } else {
                        sh.ring_at(src, f - k, ch).unwrap_or(0.0)
                    };
                    sh.add(li, layer, pos, ch, -v);
                    if at < sh.max_frames {
                        sh.write(li, layer, at, ch, v);
                    }
                }
                undone += 1;
                if at < sh.max_frames {
                    kept += 1;
                }
            }
        }
        tail = kept;
        if undone > 0 {
            println!(
                "  {:.0} ms recorded after the press unwrapped from the loop head, \
                 kept as continuation ({} frames).",
                undone as f64 / sr as f64 * 1000.0,
                kept
            );
        }
    }

    let layer = lp.n_layers.load(Ordering::Acquire);
    let len = lp.loop_len.load(Ordering::Acquire);

    // **A Revox pass makes no layer.** It went over the tape, so what changed
    // is layer zero and there is nothing new to shape or to count. The picture
    // has to be redrawn because the audio under it moved, which is the one
    // thing this branch still owes.
    if lp.revox.load(Ordering::Relaxed) {
        lp.enter(Phase::Playing, sh.out_frames.load(Ordering::Acquire) as i64);
        sh.rebuild_env(li, 0);
        return format!(
            "loop {} over the tape: {:.3} s, one layer.",
            li,
            len as f64 / sr as f64
        );
    }

    // Born on the pass it was committed on, which is when it starts existing as
    // something to be heard — and so when it starts getting older.
    lp.set_layer_shape(layer, Shape { len, tail, born: lp.pass_index(closed_at, len) });
    sh.rebuild_env(li, layer);
    lp.add_layer();
    if len > 0 {
        draw_layer(sh, li, layer, len, sr);
    }
    // The length belongs in the ack even though the snapshot also carries it:
    // this is the sentence that appears when the press lands, and "committed"
    // on its own cannot be told apart from the previous "committed". The
    // detail prints above stay on the console — they are several lines each and
    // diagnostic rather than an outcome.
    format!(
        "loop {} committed: {:.3} s, {} layer{} playing.",
        li,
        len as f64 / sr as f64,
        layer + 1,
        if layer == 0 { "" } else { "s" }
    )
}

/// What a layer actually contains, drawn.
///
/// "How do I know what has been recorded?" is a fair question to ask of a
/// machine whose entire state is invisible, and it is the question this whole
/// project exists to answer better than a single LED does. Hearing it is the
/// real answer; this is the one available the instant a pass ends, and it
/// distinguishes silence from quiet, a full loop from a half-empty one, and a
/// clipped take from a clean one at a glance.
pub(crate) fn draw_layer(sh: &Shared, li: usize, layer: usize, len: usize, sr: u32) {
    const COLS: usize = 56;
    const RAMP: [char; 8] = [' ', '.', ':', '-', '=', '+', '*', '#'];

    let mut peak = 0.0f32;
    let mut sum = 0.0f64;
    let mut bins = [0.0f32; COLS];
    for i in 0..len {
        let v = (0..CHANNELS)
            .map(|ch| sh.read(li, layer, i, ch).abs())
            .fold(0.0f32, f32::max);
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
pub(crate) fn fill_from_ring(
    sh: &Shared,
    li: usize,
    layer: usize,
    from_out: i64,
    len: usize,
    at: usize,
    additive: bool,
) -> usize {
    let k = sh.k.load(Ordering::Acquire);
    // The loop's own source. A retroactive take lands wherever the press said,
    // and it takes the past of the input that loop is pointed at — which is the
    // whole reason every source keeps a ring of its own.
    let src = sh.src_of(li);
    let mut got = 0;
    for pos in 0..len {
        let Some(v0) = sh.ring_at(src, from_out + pos as i64 - k, 0) else {
            continue;
        };
        for ch in 0..CHANNELS {
            let v = if ch == 0 { v0 } else {
                sh.ring_at(src, from_out + pos as i64 - k, ch).unwrap_or(0.0)
            };
            if additive {
                sh.add(li, layer, at + pos, ch, v);
            } else {
                sh.write(li, layer, at + pos, ch, v);
            }
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
pub(crate) fn take(sh: &Shared, li: usize, sr: u32, secs: f64, late: i64) -> String {
    let lp = sh.lp(li);
    if !sh.k_set.load(Ordering::Acquire) {
        return "no input has arrived yet.".to_string();
    }
    let layer = lp.n_layers.load(Ordering::Acquire);
    if layer >= sh.max_layers {
        return format!(
            "loop {} is at {} layers, the ceiling; undo one first.",
            li, sh.max_layers
        );
    }

    let loop_len = lp.loop_len.load(Ordering::Acquire);
    // As of the press, not as of the command. Claiming the past is the one
    // gesture where the boundary is the whole point — you press because the
    // good bit has just finished — so the few hundred milliseconds a footswitch
    // takes to resolve would otherwise be claimed as part of it.
    let cur = sh.out_frames.load(Ordering::Acquire) as i64 - late.max(0);

    let (from_out, len, what) = if loop_len == 0 {
        let len = ((secs * sr as f64).round() as usize).min(sh.max_frames);
        (cur - len as i64, len, "loop")
    } else {
        // The last cycle that has actually finished. Anything else would be a
        // partial pass presented as a whole one.
        let origin = lp.origin.load(Ordering::Acquire);
        let done = (cur - origin).div_euclid(loop_len as i64);
        if done < 1 {
            return format!("loop {}: not one complete cycle has gone by yet.", li);
        }
        (origin + (done - 1) * loop_len as i64, loop_len, "layer")
    };

    if from_out < 0 {
        return "that reaches back before the engine started.".to_string();
    }

    sh.zero_layer(li, layer);
    let got = fill_from_ring(sh, li, layer, from_out, len, 0, false);
    if got == 0 {
        return "the pre-roll does not reach back that far.".to_string();
    }
    // A short take is not a failed one: it succeeded with less than was asked
    // for. So this is a PREFIX to whatever the outcome turns out to be, not a
    // branch of its own — the app has to be told both that it worked and that
    // it is shorter than you meant, in one sentence.
    let shortfall = if got < len {
        format!(
            "only {:.2} s of the {:.2} s asked for was still in the pre-roll — ",
            got as f64 / sr as f64,
            len as f64 / sr as f64
        )
    } else {
        String::new()
    };

    let headline;
    if loop_len == 0 {
        lp.loop_len.store(len, Ordering::Release);
        // The first loop to acquire a length becomes the grid the rest
        // can align to — first rather than chosen, because that is how a
        // looper has always worked: what you played first is what the
        // rest fits around. A compare-exchange, so later calls are no-ops.
        sh.claim_anchor(li);
        lp.origin.store(from_out, Ordering::Release);
        lp.enter(Phase::Playing, sh.out_frames.load(Ordering::Acquire) as i64);
        headline = format!(
            "loop {} took the last {:.3} s as the {}: {} frames, {:.1} bpm if that is one bar of 4/4",
            li,
            len as f64 / sr as f64,
            what,
            len,
            240.0 / (len as f64 / sr as f64)
        );
    } else {
        headline = format!("loop {} took the last complete cycle as a new {}.", li, what);
    }
    let taken = lp.n_layers.load(Ordering::Acquire);
    // The continuation comes from the ring too, so a claimed layer wraps as
    // seamlessly as a recorded one.
    //
    // It is free where it is available and empty where it is not, and the ring
    // says which: claiming the last complete *cycle* means what followed it has
    // already gone by, but claiming the last few seconds as the loop itself ends
    // at now, and nothing has followed now. `ring_at` refuses a frame it does
    // not hold, so the second case simply keeps nothing rather than reading a
    // minute-old slot as though it were the future.
    let taken_len = lp.loop_len.load(Ordering::Acquire);
    let want = sh.max_fade.min(sh.max_frames.saturating_sub(taken_len));
    let tail = fill_from_ring(sh, li, taken, from_out + taken_len as i64, want, taken_len, false);
    // A claimed layer is born now, not when the audio in it was played. It
    // starts being heard at this instant, and decay is about what you can hear.
    lp.set_layer_shape(taken, Shape { len: taken_len, tail, born: lp.pass_index(cur, taken_len) });
    sh.rebuild_env(li, taken);
    lp.add_layer();
    draw_layer(sh, li, taken, lp.loop_len.load(Ordering::Acquire), sr);
    format!(
        "{}{} — {} layer{} playing.",
        shortfall,
        headline.trim_end_matches('.'),
        taken + 1,
        if taken == 0 { "" } else { "s" }
    )
}
