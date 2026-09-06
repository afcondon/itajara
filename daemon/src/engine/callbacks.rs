//! The two audio callbacks, as named functions.
//!
//! Lifted whole out of the closures in `run` on 2026-09-06 (REVIEW-daemon-debt
//! step 1): what each closure captured is now what each function is passed,
//! and the bodies are the bodies the closures had.

use std::sync::atomic::Ordering;

use rand::rngs::SmallRng;

use crate::measure::signed_secs;

use super::{ARMED, CHANNELS, ENV_BUCKETS, FIRE, FIRST, IDLE, MULTIPLY, OVERDUB, PLAYING};
use super::shared::Shared;

/// The output callback: mix every loop into one buffer, and stamp every
/// pending transition to the frame it belongs to.
///
/// Called once per buffer from the closure `run` builds; `rng`, `gains` and
/// `folds` are the closure's own scratch, made once at stream build and
/// carried in so that nothing here allocates.
pub(super) fn output(
    sh: &Shared,
    ch: usize,
    dual: bool,
    out_channels: usize,
    rng: &mut SmallRng,
    gains: &mut [(f32, f32)],
    folds: &mut [bool],
    data: &mut [f32],
    info: &cpal::OutputCallbackInfo,
) {
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
            let s = sh.loop_at(li, out_frame, rng, true);
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
}

/// The input callback: establish `K` once, keep every source's pre-roll
/// ring, listen for a level-armed loop's threshold, and write the recording
/// loop's layer.
///
/// Called once per buffer from the closure `run` builds; `residual` is the
/// calibrated sample count `run` resolved at start.
pub(super) fn input(
    sh: &Shared,
    residual: f64,
    in_channels: usize,
    sr: u32,
    sr_f: f64,
    data: &[f32],
    info: &cpal::InputCallbackInfo,
) {
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
}
