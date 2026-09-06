//! One command, from wherever it came.
//!
//! Split out of `engine.rs` on 2026-09-06 (REVIEW-daemon-debt step 1). The
//! arms are in the order the file has always had them, and that order is
//! load-bearing — see the comments inside, and `tools/check-verbs.py`, which
//! reads this file by path.

use std::sync::atomic::Ordering;

use super::{
    ARM_REACH_MS,
    ARMED,
    CHANNELS,
    db_to_mag,
    decay_words,
    fade_words,
    fb_words,
    FIRE,
    FIRST,
    IDLE,
    MAX_FADE_MS,
    MULTIPLY,
    odds_words,
    OVERDUB,
    PLAYING,
    Shape,
    thresh_words,
    tone_words,
    vol_words,
};
use super::commit::{commit, draw_layer, take};
use super::copy::copy_layers;
use super::cycle::{
    dense,
    fix_next,
    free_length,
    multiply_end,
    multiply_start,
    place_at,
    rotate,
    set_bars,
    sparse,
    start_all,
    take_tempo,
};
use super::edit::{schedule_restart, thread_blank};
use super::export::{export_layers, export_set, save_take};
use super::guards::{busy_elsewhere, not_plain, not_writable};
use super::shared::Shared;

/// One command, from wherever it came.
///
/// Both the console and the socket land here, so a footswitch, a browser and a
/// terminal cannot drift into meaning different things by the same name. The
/// detail still goes to stdout — waveforms and level readings are for the
/// person sitting at the daemon — and what comes back is the short
/// acknowledgement a remote caller needs. Remote clients render from the state
/// snapshot rather than from these strings.
pub fn dispatch(sh: &Shared, sr: u32, line: &str) -> String {
    // A leading digit picks the loop: `3r` records loop 3 whatever is selected,
    // `3s2` spreads it. Every command can therefore address a loop explicitly,
    // which is what the footswitch path needs — the MC6 sends one fixed message
    // per switch and must not depend on a selection it cannot see. A switch
    // that means different things according to hidden state is precisely the
    // failure this design exists to avoid.
    //
    // A bare digit selects, which is a convenience for the console and for the
    // single-loop view, and nothing depends on it.
    // `@<ms>` on the end says how long ago the press actually happened.
    //
    // **The app knows and the daemon cannot.** A switch that may be
    // double-tapped cannot resolve until the window expires, so every command
    // from a footswitch arrives a fixed few hundred milliseconds after the
    // foot moved — and a looper that believes the arrival time records a loop
    // that much longer than it was played. Nothing in the sound says so, which
    // is the worst kind of wrong.
    //
    // Carried on the command rather than inferred, because only the sender was
    // there. Stripped for every command and spent only by the ones for which a
    // frame matters, so a client can stamp everything it sends without having
    // to know which those are.
    let (line, late_ms) = match line.rsplit_once('@') {
        Some((cmd, ms)) => match ms.trim().parse::<f64>() {
            Ok(v) if v >= 0.0 && v < 5000.0 => (cmd, v),
            // Out of range or unparseable: refuse rather than silently treating
            // it as on time, because a client that thinks it is compensating
            // and is not would be worse off than one that never tried.
            _ => return format!("`@{}` is not a lateness in milliseconds.", ms.trim()),
        },
        None => (line, 0.0),
    };
    let late = (late_ms / 1000.0 * sr as f64).round() as i64;

    let trimmed = line.trim();
    // The loop is the leading run of digits, however long: `3r` and `12r`
    // both parse, because no verb begins with a digit. Was one digit until
    // 2026-09-04, which capped a rig at ten loops for no reason of the engine's.
    let digits = trimmed.chars().take_while(|c| c.is_ascii_digit()).count();
    let (li, rest) = if digits > 0 {
        let n: usize = match trimmed[..digits].parse() {
            Ok(n) => n,
            Err(_) => return format!("`{}` is not a loop number.", &trimmed[..digits]),
        };
        if n >= sh.n_loops {
            return format!("there are {} loops, numbered 0 to {}.", sh.n_loops, sh.n_loops - 1);
        }
        (n, trimmed[digits..].trim())
    } else {
        (sh.sel(), trimmed)
    };
    if !trimmed.is_empty() && rest.is_empty() {
        sh.selected.store(li, Ordering::Relaxed);
        return format!("loop {} selected.", li);
    }
    let lp = sh.lp(li);
    {
        // **The window, the rotation and the peaks** — the editing verbs, all
        // non-destructive, all in arena positions (frames from the start of
        // the loop), because the page that sends them is looking at a picture
        // drawn in those. `in<f>` and `out<f>` set the window, `win` clears
        // it, `rot<f>` shifts the start by a signed number of frames, and
        // `pk<n>` asks for the loop's waveform in `n` buckets, which comes
        // back as its own message rather than in the ack.
        // `t` is the claim-the-past verb, matched below by its first letter;
        // spelled the same way here so `tone` is not mistaken for it.
        let claims_past = rest.as_bytes().first() == Some(&b't') && rest.strip_prefix("tone").is_none();
        // An overdub may go into a windowed loop — the write head follows the
        // play head through the window (`write_pos`), so what you play lands
        // where you heard it and outside the window nothing is touched.
        // Multiply changes the length the window is set against, and the
        // claim writes from the ring against the cycle, so those two still
        // want the whole loop.
        if lp.window().is_some() && (rest == "x" || claims_past) {
            return format!(
                "loop {} has a window; clear it with `win` before multiplying or claiming the past.",
                li
            );
        }
        match rest {
            _ if rest.starts_with("win") => {
                // The window goes; the rotation is its own edit and stays,
                // folded into the whole loop's span.
                let len = lp.loop_len.load(Ordering::Acquire).max(1);
                let (_, _, vr) = lp.edit_view();
                schedule_restart(sh, li, 0, 0, vr % len);
                return format!("loop {} plays the whole loop again.", li);
            }
            _ if rest.starts_with("in") || rest.starts_with("out") => {
                let is_in = rest.starts_with("in");
                let arg = rest[if is_in { 2 } else { 3 }..].trim();
                let len = lp.loop_len.load(Ordering::Acquire);
                if len == 0 {
                    return format!("loop {} has no length to window yet.", li);
                }
                let f: i64 = match arg.parse() {
                    Ok(f) => f,
                    Err(_) => return format!("`{}` wants a position in frames, which may be negative.", rest),
                };
                let l = len as i64;
                // Relative to what the hand has already set, not to what is
                // playing: a drag is a run of these, and each is against the
                // last.
                let (vi, vo, vr) = lp.edit_view();
                let (i, o) = if vi == 0 && vo == 0 { (0, l) } else { (vi, vo) };
                let (i, o) = if is_in { (f, o) } else { (i, f) };
                // One loop's worth of silence on either side is the most a
                // window can add: past that a loop is mostly rest, and the
                // arithmetic that draws it would rather say so than draw it.
                if i < -l || o > 2 * l {
                    return format!(
                        "loop {} is {} frames long; a window may reach from {} to {}, not {}.",
                        li,
                        len,
                        -l,
                        2 * l,
                        if is_in { i } else { o }
                    );
                }
                if i >= o {
                    return format!("a window has to end after it starts: in {} out {} on loop {}.", i, o, li);
                }
                let rot = vr % ((o - i) as usize).max(1);
                // The whole loop is no window, and is held as none.
                if i == 0 && o == l {
                    schedule_restart(sh, li, 0, 0, rot);
                    return format!("loop {} plays the whole loop again.", li);
                }
                schedule_restart(sh, li, i, o, rot);
                let srf = sr as f64;
                let padded = if i < 0 || o > l { " with silence" } else { "" };
                return format!(
                    "loop {} windows {:.3}–{:.3} s ({:.3} s of {:.3}{}).",
                    li,
                    i as f64 / srf,
                    o as f64 / srf,
                    (o - i) as f64 / srf,
                    len as f64 / srf,
                    padded
                );
            }
            _ if rest.starts_with("rot") => {
                let len = lp.loop_len.load(Ordering::Acquire);
                if len == 0 {
                    return format!("loop {} has nothing to rotate yet.", li);
                }
                let k: i64 = match rest[3..].trim().parse() {
                    Ok(k) => k,
                    Err(_) => return format!("`{}` wants a signed number of frames.", rest),
                };
                let (vi, vo, vr) = lp.edit_view();
                let span = if vi == 0 && vo == 0 { len as i64 } else { vo - vi }.max(1);
                let next = (vr as i64 + k).rem_euclid(span) as usize;
                schedule_restart(sh, li, vi, vo, next);
                return format!(
                    "loop {} starts {:.3} s into its cycle now ({:+.3} s).",
                    li,
                    next as f64 / sr as f64,
                    k as f64 / sr as f64
                );
            }
            _ if rest.starts_with("pk") => {
                let len = lp.loop_len.load(Ordering::Acquire);
                let n = lp.n_layers.load(Ordering::Acquire);
                if len == 0 || n == 0 {
                return format!("loop {} has nothing to draw.", li);
                }
                let buckets: usize = rest[2..].trim().parse().unwrap_or(600).clamp(16, 4000);
                let (i, o) = lp.window().unwrap_or((0, 0));
                // The picture spans the loop and whatever silence the window
                // reaches into, so a window that extends past an end is drawn
                // where it is rather than off the edge.
                let l = len as i64;
                let (from_all, to_all) = (i.min(0), o.max(l));
                let total = (to_all - from_all) as usize;
                let mut lo = Vec::with_capacity(buckets);
                let mut hi = Vec::with_capacity(buckets);
                for b in 0..buckets {
                let from = b * total / buckets;
                let to = ((b + 1) * total / buckets).max(from + 1).min(total);
                let (mut mn, mut mx) = (0.0f32, 0.0f32);
                for p in from..to {
                    let pos = from_all + p as i64;
                    if pos < 0 || pos >= l {
                        continue;
                    }
                    let v = sh.mix_at(li, n, pos as usize, false);
                    for ch in 0..CHANNELS {
                        mn = mn.min(v[ch]);
                        mx = mx.max(v[ch]);
                    }
                }
                lo.push(((mn * 1000.0).round() as i32).clamp(-1000, 1000));
                hi.push(((mx * 1000.0).round() as i32).clamp(-1000, 1000));
                }
                let json = format!(
                r#"{{"peaks":{{"loop":{},"frames":{},"from":{},"to":{},"buckets":{},"winIn":{},"winOut":{},"rot":{},"lo":[{}],"hi":[{}]}}}}"#,
                li,
                len,
                from_all,
                to_all,
                buckets,
                i,
                o,
                lp.rot.load(Ordering::Relaxed),
                lo.iter().map(|v| v.to_string()).collect::<Vec<_>>().join(","),
                hi.iter().map(|v| v.to_string()).collect::<Vec<_>>().join(",")
                );
                if let Ok(mut slot) = sh.peaks.lock() {
                *slot = json;
                }
                sh.peaks_seq.fetch_add(1, Ordering::Release);
                return format!("peaks for loop {}: {} buckets over {:.3} s.", li, buckets, len as f64 / sr as f64);
            }
            "x" => match lp.state.get() {
                MULTIPLY => return multiply_end(sh, li, sr),
                FIRST | OVERDUB => return format!("loop {} is still recording — finish that first.", li),
                _ => {
                    if let Some(other) = busy_elsewhere(sh, li) {
                        return other;
                    }
                    if let Some(no) = not_plain(lp, li) {
                        return no;
                    }
                    return multiply_start(sh, li, sr);
                }
            },
            "r" => match lp.state.get() {
                MULTIPLY => return multiply_end(sh, li, sr),
                FIRST | OVERDUB => return commit(sh, li, sr, late),
                // A second press while the loop is waiting for a sound takes the
                // arm back. There has to be a way out: the sound may never come,
                // and a loop holding the input for a recording that will never
                // begin locks out all five others. Asked before the claim checks
                // below, because it is this loop's own claim being released.
                ARMED => {
                    lp.state.set(IDLE);
                    lp.arm_from.store(i64::MIN, Ordering::Release);
                    return format!("loop {} has stopped listening.", li);
                }
                _ => {
                    if let Some(other) = busy_elsewhere(sh, li) {
                        return other;
                    }
                    if let Some(no) = not_writable(lp, li) {
                        return no;
                    }
                    let layer = lp.n_layers.load(Ordering::Acquire);
                    if layer >= sh.max_layers {
                        return format!(
                            "loop {} is at {} layers, the ceiling; undo one first.",
                            li, sh.max_layers
                        );
                    } else {
                        // An overdub sums into its layer, so anything left there
                        // from an undone take would bleed into the new one.
                        sh.zero_layer(li, layer);
                        // And the picture of it, which is now of audio that no
                        // longer exists. Redrawn at commit; blank until then,
                        // which reads as "being recorded" rather than as a lie.
                        sh.rebuild_env(li, layer);
                        // Anything above this layer has just been made
                        // unrecoverable, so redo must not offer it.
                        lp.redo_to.store(layer, Ordering::Release);
                        // Kept until the recording closes, because the pre-roll
                        // shift that spends it happens at commit.
                        lp.started_late.store(late, Ordering::Release);
                        // Level-armed: wait for a sound rather than starting on
                        // the press. Nothing else happens here — the input
                        // callback finds the crossing and sets the same request
                        // this would have, so there is one road into `FIRST` and
                        // not two.
                        //
                        // The press's own lateness is dropped, deliberately. It
                        // measures how late the *foot* was, and the foot is no
                        // longer what starts this recording; carrying it would
                        // back-date the loop past the note that began it.
                        if lp.level_arm.load(Ordering::Relaxed) {
                            lp.started_late.store(0, Ordering::Release);
                            lp.arm_from.store(i64::MIN, Ordering::Release);
                            lp.request_at.store(i64::MIN, Ordering::Release);
                            lp.state.set(ARMED);
                            return format!(
                                "loop {} is listening — it starts when something goes over {}.",
                                li,
                                thresh_words(sh)
                            );
                        }
                        // Only a FIRST recording needs a deadline. An overdub
                        // records from `origin`, so it is already on whatever
                        // grid its loop sits on and cannot be nudged off it.
                        //
                        // **Layers, not length** — the same correction as in the
                        // callback, and it had the same cause: "has a length"
                        // meant "has material" while the only way to get one was
                        // to record one. A loop that has been *told* how many
                        // bars it is has a length and nothing in it, and it is
                        // exactly the loop that most needs to start on the
                        // boundary: it will be four bars long either way, and
                        // four bars starting off the grid is four bars wrong.
                        // A one-pass layer starts on the press, like any
                        // overdub. It was first made to wait for the loop's
                        // own zero so the stacked layers would read from one
                        // start — and on a thirteen-second loop that was up to
                        // thirteen seconds of nothing after the press, with no
                        // sign of the wait. The layer spans the whole loop
                        // either way, and what you play lands where you heard
                        // it; the wait bought nothing the music could hear.
                        let one_pass = lp.one_pass.load(Ordering::Relaxed) && layer > 0;
                        let boundary = if lp.quant.load(Ordering::Relaxed)
                            && lp.n_layers.load(Ordering::Acquire) == 0
                        {
                            sh.next_boundary(sh.out_frames.load(Ordering::Acquire) as i64)
                        } else {
                            None
                        };
                        match boundary {
                            Some(t) => {
                                lp.request_at.store(t, Ordering::Release);
                                lp.request.set(ARMED);
                                let wait =
                                    (t - sh.out_frames.load(Ordering::Acquire) as i64).max(0);
                                return format!(
                                    "loop {} starts on the grid in {:.2} s.",
                                    li,
                                    wait as f64 / sr as f64
                                );
                            }
                            None => {
                                lp.request_at.store(i64::MIN, Ordering::Release);
                                lp.request.set(ARMED);
                                // The press you make most often, and until now
                                // the one that said least. Which layer matters:
                                // "recording" on an empty loop and "recording"
                                // onto layer 5 are different enough that a
                                // display showing neither is the reason nobody
                                // could tell an overdub had started.
                                return if lp.revox.load(Ordering::Relaxed) {
                                    // **Not "onto layer 2".** In Revox there is
                                    // one layer and the head is going over it;
                                    // naming a layer that will never exist is
                                    // how a mode gets blamed for making the
                                    // thing it was told not to make.
                                    format!("loop {} over the tape.", li)
                                } else if one_pass {
                                    format!(
                                        "loop {} adds layer {}, one pass, closing itself.",
                                        li,
                                        layer + 1
                                    )
                                } else if layer == 0 {
                                    format!("loop {} recording.", li)
                                } else {
                                    format!("loop {} overdubbing onto layer {}.", li, layer + 1)
                                };
                            }
                        }
                    }
                }
            },
            // Fire a one-shot: one pass from the top, now.
            //
            // **Lateness is not spent here, and that is a choice.** Every other
            // time-critical command in this daemon subtracts it, because they all
            // describe something that has already been captured and can be
            // re-dated. A fire describes something about to be *played*, and no
            // speaker can emit a frame that should have gone out 300 ms ago. The
            // alternative — starting the pass that far in, so it lands where the
            // foot meant it to — buys grid alignment with the attack, and the
            // attack is the reason anybody fires a one-shot. So it starts at the
            // top and is late; `g1` is how you ask for it to be on the grid.
            "f" => {
                let len = lp.loop_len.load(Ordering::Acquire);
                if len == 0 {
                    return format!("loop {} is empty; there is nothing to fire.", li);
                }
                if !lp.one_shot.load(Ordering::Relaxed) {
                    return format!(
                        "loop {} is not a one-shot; `{}one1` first, or it would just \
                         jump to the top and carry on.",
                        li, li
                    );
                }
                let now = sh.out_frames.load(Ordering::Acquire) as i64;
                match if lp.quant.load(Ordering::Relaxed) {
                    sh.next_boundary(now)
                } else {
                    None
                } {
                    Some(t) => {
                        lp.request_at.store(t, Ordering::Release);
                        lp.request.set(FIRE);
                        return format!(
                            "loop {} fires on the grid in {:.2} s.",
                            li,
                            (t - now).max(0) as f64 / sr as f64
                        );
                    }
                    None => {
                        lp.request_at.store(i64::MIN, Ordering::Release);
                        lp.request.set(FIRE);
                        return format!("loop {} fires.", li);
                    }
                }
            }
            // **Before the take guard, and that is load-bearing.** `t` is
            // matched as `starts_with('t')` — a char, not a word — so every
            // command beginning with a t reaches it first. `tone3000` was
            // silently being read as "claim the last 3000 seconds", which
            // answered with a refusal about cycles and left the tone unchanged:
            // a verb that has an arm, is spelled right, and never arrives.
            //
            // `tools/check-verbs.py` cannot catch this. It asks whether every
            // verb *has* an arm, not whether it reaches the one it meant, and
            // both were true here.
            // How much top the tape keeps, in hertz. Twenty thousand and up is
            // off outright rather than very nearly off.
            _ if rest.starts_with("tone") => {
                let arg = rest[4..].trim();
                if arg.is_empty() {
                    return format!("loop {} {}.", li, tone_words(lp));
                }
                match arg.parse::<f32>() {
                    Ok(hz) if (200.0..=20_000.0).contains(&hz) => {
                        lp.tone.store(hz.to_bits(), Ordering::Relaxed);
                        return format!("loop {} {}.", li, tone_words(lp));
                    }
                    Ok(hz) => return format!("tape tone wants 200 to 20000 Hz, not {}.", hz),
                    _ => return format!("tape tone wants hertz, not `{}`.", arg),
                }
            }
            l if l.starts_with('t') => {
                let secs = l[1..].trim().parse::<f64>().unwrap_or(8.0);
                return take(sh, li, sr, secs, late);
            }
            // **Above `s`, which prefix-matches**, and above the `t` guard for
            // the same reason `tone` is: both are char-matched and would eat a
            // longer verb whole. This file has been bitten twice that way.
            _ if rest.starts_with("src") => {
                let arg = rest[3..].trim();
                if arg.is_empty() {
                    let i = sh.src_of(li);
                    return format!("loop {} records from {}.", li, sh.sources[i].describe());
                }
                match arg.parse::<usize>() {
                    Ok(n) if n >= 1 && n <= sh.sources.len() => {
                        if lp.is_recording() || lp.is_armed() {
                            return format!(
                                "loop {} is listening or writing; changing its input \
                                 mid-take would splice two different rooms together.",
                                li
                            );
                        }
                        lp.src.store(n - 1, Ordering::Release);
                        return format!("loop {} records from {}.", li, sh.sources[n - 1].describe());
                    }
                    Ok(n) => {
                        return format!(
                            "there are {} sources ({}), not {}.",
                            sh.sources.len(),
                            sh.sources
                                .iter()
                                .enumerate()
                                .map(|(i, s)| format!("{} {}", i + 1, s.name))
                                .collect::<Vec<_>>()
                                .join(", "),
                            n
                        )
                    }
                    _ => return format!("`{}` is not a source number.", arg),
                }
            }

            // Fold to mono at playback. Not a capture decision — the audio
            // stays stereo — so this is free to try and free to undo.
            "mono" | "mono1" => {
                lp.mono.store(true, Ordering::Relaxed);
                return format!(
                    "loop {} folds to mono; pan places it rather than balancing it.",
                    li
                );
            }
            "mono0" => {
                lp.mono.store(false, Ordering::Relaxed);
                return format!("loop {} keeps its two channels; pan is a balance.", li);
            }

            // **Above `s`, which prefix-matches.** `s` is sparse-multiply and
            // takes anything beginning with an s, so `sp0.5` read as "sparse,
            // could not parse the count, use 2" and quietly did a multiply. It
            // cost half an hour and would have cost a take: the command was
            // acked by nothing and did something else entirely. Ordering fixes
            // it here; `s` itself was tightened to refuse a count it cannot
            // read, rather than inventing one.
            _ if rest.starts_with("sp") => {
                let arg = &rest[2..];
                match arg.parse::<f64>() {
                    // An eighth to four times. Below that a loop is a drone and
                    // linear interpolation is audibly a filter; above it, the
                    // aliasing this does nothing about becomes the loudest thing
                    // in the sound.
                    Ok(v) if v.abs() >= 0.125 && v.abs() <= 4.0 => {
                        if lp.is_recording() {
                            return format!("loop {} is recording; speed would move the grid under it.", li);
                        }
                        lp.want(v, lp.pendulum.load(Ordering::Relaxed));
                        return format!(
                            "loop {} plays at x{} {}.",
                            li,
                            v.abs(),
                            if v < 0.0 { "backwards" } else { "forwards" }
                        );
                    }
                    Ok(v) => {
                        return format!("speed wants 0.125 to 4, either sign, not {}.", v)
                    }
                    _ => return format!("speed wants a number, not `{}`.", arg),
                }
            }
            // The second multiply, and its two companions. Structural, so they
            // are instant and reversible — nothing here records anything.
            l if l.starts_with('s') => {
                // Bare `s` means two, which is the common case. Anything else
                // has to be a number: `unwrap_or(2)` here turned every typo
                // beginning with an s into a multiply nobody asked for.
                let arg = l[1..].trim();
                match if arg.is_empty() { Ok(2) } else { arg.parse::<usize>() } {
                    Ok(n) => return sparse(sh, li, sr, n),
                    Err(_) => return format!("`{}` is not a command; `s` wants a count.", l),
                }
            }
            // Grid sync for this loop. Explicit forms alongside the toggle for
            // the same reason `k` and `m` have them: a client that flips rather
            // than sets drifts out of step the first time a message is dropped
            // and never recovers.
            "g" | "g1" | "g0" => {
                let on = match rest {
                    "g1" => true,
                    "g0" => false,
                    _ => !lp.quant.load(Ordering::Relaxed),
                };
                lp.quant.store(on, Ordering::Relaxed);
                return match (on, sh.grid()) {
                    (false, _) => format!("loop {} is free.", li),
                    // **Where the bar came from, not who the anchor is.** This
                    // named `anchor` unconditionally, which was true while the
                    // grid was always a loop's cycle and prints "from loop 8" —
                    // one past the last loop, the sentinel for "nobody" — the
                    // moment Link is the one supplying it.
                    (true, Some((_, glen))) => {
                        let from = if sh.link_bar_frames.load(Ordering::Relaxed) > 0 {
                            "Link".to_string()
                        } else {
                            match sh.anchor.load(Ordering::Acquire) {
                                a if a < sh.n_loops => format!("loop {}", a),
                                _ => "nowhere".to_string(),
                            }
                        };
                        format!(
                            "loop {} follows the grid ({:.3} s, from {}).",
                            li,
                            glen as f64 / sr as f64,
                            from
                        )
                    }
                    // Worth saying plainly rather than reporting success: the
                    // setting took, but with nothing to align to it does
                    // nothing, and a loop that starts free when you asked for
                    // the grid is the kind of surprise that gets blamed on the
                    // engine much later.
                    (true, None) => format!(
                        "loop {} will follow the grid — but no loop has a length yet, \
                         so there is no grid. The first recording makes one.",
                        li
                    ),
                };
            }
            "o" => return rotate(sh, li),
            // Exact match, and it has to be: `b` would collide with `blank`,
            // and this file has been bitten twice by a prefix guard eating a
            // longer verb — see the note above `tone`.
            "bpm" => return take_tempo(sh, li, sr),
            // Rig-wide, and exact-matched for the reason `bpm` above it is:
            // `g` is already a verb (grid), so a prefix guard here would be a
            // collision rather than a convenience.
            "go" => return start_all(sh, sr),
            // The session transport, rig-wide, exact-matched for the same
            // reason `bpm` and `go` are: `p` is a verb already and `pan`,
            // `pend` and `ph` all start with it, so a prefix guard here would
            // be a collision dressed as a convenience.
            //
            // **This is the only verb in the engine that does nothing to
            // audio.** It exists because the drum machine is on the iPad and
            // the iPad follows Link's Start/Stop Sync and nothing else — so
            // "play the beat and record four bars of it" is two commands from
            // the app, and this is the first one. The second is an ordinary
            // grid-quantised `r`, which is waiting for the same bar line
            // link-spike is scheduling the start on.
            //
            // The ack is deliberately conditional. There is no reply from
            // link-spike, so success here means the bytes left the machine and
            // nothing more; saying "transport started" would be claiming to
            // know something this daemon cannot see.
            "play1" | "play0" => {
                let on = rest.ends_with('1');
                return match crate::link::set_playing(on, crate::link::DEFAULT_TEMPO_PORT) {
                    Ok(()) => format!(
                        "asked Link to {} — peers with start/stop sync follow{}.",
                        if on { "start on the next bar" } else { "stop" },
                        if on { " on the downbeat" } else { "" }
                    ),
                    Err(e) => format!("could not reach the transport: {}", e),
                };
            }
            "d" => return dense(sh, li),
            "z" => return free_length(sh, li, sr),
            // **Ahead of every prefix guard below and behind every one above.**
            // `len` shares two letters with `lev`, which is matched exactly, and
            // `ph` shares one with `pan` and `pend`, which are matched by their
            // own longer prefixes — so neither can be swallowed. That is worth
            // stating rather than trusting: a verb defined after a looser guard
            // is a verb that silently never runs, which has happened here once
            // already.
            // Rig-wide, so the loop prefix is ignored — the same shape as `arm`,
            // `k` and `m`, and said in the ack so a `3lq4` does not look like it
            // set something on loop 3.
            // Per-layer enable. `ly31` is layer 3 on, `ly30` is layer 3 off.
            // Layers count from one on the wire, as `ph` does, and the flag is
            // the *last* character so a two-digit layer still parses once
            // `--layers` grows. **Set, never flipped**, for the reason every
            // flag verb in here gives: a client that flips drifts out of step
            // the first time a message is dropped.
            _ if rest.starts_with("ly") => {
                let arg = rest[2..].trim();
                let (num, flag) = arg.split_at(arg.len().saturating_sub(1));
                let on = match flag {
                    "1" => true,
                    "0" => false,
                    _ => return format!("`{}` wants a layer number and then 1 or 0, as in `ly21`.", rest),
                };
                let n = lp.n_layers.load(Ordering::Acquire);
                return match num.parse::<usize>() {
                    Ok(l) if l >= 1 && l <= n => {
                        lp.l_on[l - 1].store(on, Ordering::Release);
                        format!("loop {} layer {} is {}.", li, l, if on { "on" } else { "off" })
                    }
                    Ok(l) => format!(
                        "loop {} has {} layer{}, not a layer {}.",
                        li,
                        n,
                        if n == 1 { "" } else { "s" },
                        l
                    ),
                    Err(_) => format!("`{}` wants a layer number and then 1 or 0, as in `ly21`.", rest),
                };
            }
            // A layer's own window: `lw<k>:<in>:<out>` sets it, `lw<k>` clears
            // it. In the layer's frames, like `in`/`out` are in the loop's.
            _ if rest.starts_with("lw") => {
                let arg = rest[2..].trim();
                let mut parts = arg.split(':');
                let k = parts.next().unwrap_or("").trim().parse::<usize>();
                let n = lp.n_layers.load(Ordering::Acquire);
                let Ok(k) = k else {
                    return format!("`{}` wants a layer number, as in `lw2:1000:625000` or `lw2`.", rest);
                };
                if k < 1 || k > n {
                    return format!("loop {} has {} layer{}, not a layer {}.", li, n, if n == 1 { "" } else { "s" }, k);
                }
                let l = k - 1;
                match (parts.next(), parts.next()) {
                    (None, _) => {
                        lp.l_win_in[l].store(0, Ordering::Relaxed);
                        lp.l_win_out[l].store(0, Ordering::Relaxed);
                        return format!("loop {} layer {} plays whole again.", li, k);
                    }
                    (Some(a), Some(b)) => {
                        let len = lp.l_len[l].load(Ordering::Acquire) as i64;
                        match (a.trim().parse::<i64>(), b.trim().parse::<i64>()) {
                            // Anywhere, so long as it overlaps the layer: the
                            // read is a range check, so silence either side
                            // costs nothing, and a thirteen-second window on a
                            // five-second layer is exactly the Arbhar's case.
                            (Ok(i), Ok(o)) if o > i && i < len && o > 0 && (o - i) as usize <= sh.max_frames => {
                                lp.l_win_out[l].store(0, Ordering::Relaxed);
                                lp.l_win_in[l].store(i, Ordering::Relaxed);
                                lp.l_win_out[l].store(o, Ordering::Release);
                                return format!(
                                    "loop {} layer {} plays {}..{} of {}{}.",
                                    li, k, i, o, len,
                                    if i < 0 || o > len { ", with silence" } else { "" }
                                );
                            }
                            (Ok(_), Ok(_)) => return format!("loop {} layer {}: a window is in before out, overlaps the layer, and fits the arena.", li, k),
                            _ => return format!("`{}` wants two frame counts, as in `lw2:1000:625000`.", rest),
                        }
                    }
                    _ => return format!("`{}` wants `lw<k>:<in>:<out>`, or `lw<k>` to clear.", rest),
                }
            }
            // Duplicate a layer onto a new layer of this loop: `dp<k>`. The
            // same audio twice, so the copy can carry a different window —
            // six slices of one take in one loop. The window comes with it,
            // to be moved.
            _ if rest.starts_with("dp") => {
                let n = lp.n_layers.load(Ordering::Acquire);
                return match rest[2..].trim().parse::<usize>() {
                    Ok(k) if k >= 1 && k <= n => {
                        if n >= sh.max_layers {
                            return format!("loop {} holds {} layers already; there is no room for another.", li, sh.max_layers);
                        }
                        if lp.is_recording() || lp.is_armed() {
                            return format!("loop {} is recording — finish it first.", li);
                        }
                        let src = k - 1;
                        let len = lp.l_len[src].load(Ordering::Acquire);
                        sh.zero_layer(li, n);
                        for p in 0..len {
                            for ch in 0..CHANNELS {
                                sh.write(li, n, p, ch, sh.read(li, src, p, ch));
                            }
                        }
                        lp.set_layer_shape(n, Shape {
                            len,
                            tail: lp.l_tail[src].load(Ordering::Relaxed),
                            born: lp.l_born[src].load(Ordering::Relaxed),
                        });
                        lp.l_period[n].store(lp.l_period[src].load(Ordering::Relaxed), Ordering::Release);
                        lp.l_phase[n].store(lp.l_phase[src].load(Ordering::Relaxed), Ordering::Release);
                        lp.l_win_in[n].store(lp.l_win_in[src].load(Ordering::Relaxed), Ordering::Relaxed);
                        lp.l_win_out[n].store(lp.l_win_out[src].load(Ordering::Relaxed), Ordering::Relaxed);
                        lp.l_on[n].store(true, Ordering::Release);
                        sh.rebuild_env(li, n);
                        lp.n_layers.store(n + 1, Ordering::Release);
                        lp.redo_to.store(n + 1, Ordering::Release);
                        format!("loop {} layer {} duplicated as layer {}.", li, k, n + 1)
                    }
                    Ok(k) => format!("loop {} has {} layer{}, not a layer {}.", li, n, if n == 1 { "" } else { "s" }, k),
                    Err(_) => format!("`{}` wants a layer number, as in `dp2`.", rest),
                };
            }
            _ if rest.starts_with("lq") => {
                return match rest[2..].trim().parse::<i64>() {
                    Ok(n) if n >= -1 && n <= 64 => {
                        sh.launch_q.store(n, Ordering::Relaxed);
                        match n {
                            -1 => "launches wait for the bar (rig-wide).".to_string(),
                            0 => "launches do not wait (rig-wide).".to_string(),
                            b => format!(
                                "launches wait for the next {} beat{} (rig-wide).",
                                b,
                                if b == 1 { "" } else { "s" }
                            ),
                        }
                    }
                    Ok(n) => format!("launch quantise wants -1 to 64 beats, not {}.", n),
                    Err(_) => format!("`{}` wants a number of beats.", rest),
                };
            }
            // **The next take is this many seconds.** `fix13` on an empty loop
            // gives it a length and no audio, so the first recording closes
            // itself there instead of waiting for a second press — the
            // `set_bars` state, reached in seconds rather than bars, for a
            // face whose module wants a fixed length (Arbhar's thirteen) and a
            // page that has promised to own no clock. Not a threaded tape:
            // that carries a silent layer so it can *play*, which would make
            // the next take an overdub, and an overdub never closes itself.
            _ if rest.starts_with("fix") => {
                let arg = rest[3..].trim();
                let secs = match arg.parse::<f64>() {
                    Ok(v) if v > 0.0 => v,
                    _ => return format!("`{}` wants a length in seconds, as in `fix13`.", rest),
                };
                return fix_next(sh, li, sr, secs);
            }
            _ if rest.starts_with("len") => {
                return match rest[3..].trim().parse::<usize>() {
                    Ok(n) => set_bars(sh, li, sr, n),
                    Err(_) => format!("`{}` wants a number of bars.", rest),
                };
            }
            _ if rest.starts_with("ph") => {
                return match rest[2..].trim().parse::<usize>() {
                    Ok(n) => place_at(sh, li, n.saturating_sub(1)),
                    Err(_) => format!("`{}` wants a slot number.", rest),
                };
            }
            // Returned rather than printed. This is the one command whose whole
            // point is *where* it put something, and a path printed on the
            // daemon's stdout is a path the app cannot show anyone — so the
            // message goes back as the ack and both callers display it
            // themselves. Printing here as well got it shown twice.
            // Rig-wide, so the loop prefix is ignored — said in the ack for
            // the same reason `arm` and `m` say it. Matched on two characters
            // rather than on `e`, because this file has been bitten twice by a
            // one-character guard silently eating a longer verb defined below
            // it, and `ex` costs nothing to be careful with.
            // Copy: `cp<src>` every layer of loop src into this loop, `cp<src>l<k>`
            // its k-th (from one, like `ly`). Onto an empty loop only. Two
            // characters, ahead of every `c`: `"c"` is an exact match and
            // cannot eat it, but the next `c` prefix someone adds could.
            l if l.starts_with("cp") => {
                let rest = &l[2..];
                let (src, layer) = match rest.split_once('l') {
                    Some((a, b)) => (a.trim().parse::<usize>(), Some(b.trim().parse::<usize>())),
                    None => (rest.trim().parse::<usize>(), None),
                };
                return match (src, layer) {
                    (Ok(sl), None) => copy_layers(sh, li, sl, None),
                    (Ok(sl), Some(Ok(k))) if k >= 1 => copy_layers(sh, li, sl, Some(k - 1)),
                    _ => format!("`{}` wants a source loop, and optionally `l<layer>` from one.", l),
                };
            }
            // Three characters, and ahead of `ex`: behind it, `exlriff` would
            // export the set as "lriff" and say so cheerfully.
            l if l.starts_with("exl") => return export_layers(sh, sr, &l[3..]),
            l if l.starts_with("ex") => return export_set(sh, sr, &l[2..]),
            l if l.starts_with('w') => return save_take(sh, li, sr, &l[1..]),
            // Take back an undo. Free, now that undo does not destroy what it
            // removes: the layer is still there with its shape intact, so this
            // is one number going back up.
            "y" => {
                let n = lp.n_layers.load(Ordering::Acquire);
                let ceiling = lp.redo_to.load(Ordering::Acquire);
                if n >= ceiling {
                    return if ceiling == 0 {
                        format!("loop {} has nothing to redo.", li)
                    } else {
                        format!("loop {} is already at its last take.", li)
                    };
                }
                lp.n_layers.store(n + 1, Ordering::Release);
                return format!("loop {} redone: {} layers playing.", li, n + 1);
            }
            "u" => {
                let n = lp.n_layers.load(Ordering::Acquire);
                if n == 0 {
                    return format!("loop {} has nothing to undo.", li);
                } else {
                    lp.n_layers.store(n - 1, Ordering::Release);
                    // **Not zeroed.** Undo used to destroy the audio as well as
                    // remove the layer, which made redo impossible — and the
                    // destruction was redundant: recording zeroes its layer
                    // before it starts, precisely so nothing left from an
                    // undone take can bleed into a new one. The belt was doing
                    // the braces' job and costing the only thing it prevented.
                    //
                    // `redo_to` is how far back up the layer stack still holds
                    // audio. Recording into a layer moves it, because a take
                    // that has been recorded over is not recoverable and
                    // offering to redo it would be a lie.
                    if n == 1 {
                        // Say what is being kept, or it reads as a fault. The
                        // length surviving an undo is the point — the click goes
                        // on at the tempo you found, so the next attempt lands on
                        // the same grid — but a length with nothing in it looks
                        // exactly like a looper that has stopped listening.
                        let len = lp.loop_len.load(Ordering::Acquire);
                        return format!(
                            "loop {} layer 1 removed. Empty now, but still {:.3} s long, so the \
                             next take lands on the same grid — `{}z` to forget the length.",
                            li,
                            len as f64 / sr as f64,
                            li
                        );
                    } else {
                        return format!("loop {} layer {} removed, {} left.", li, n, n - 1);
                    }
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
            // Silence a loop, or bring it back, without touching its origin.
            //
            // Explicit `h1`/`h0` alongside the flipping `h`, the same as the
            // click and the monitor: a dropped command must not leave the app
            // and the engine disagreeing about something the player cannot see
            // — and a stopped loop is invisible by definition.
            // Multi-letter from here on. Single letters were running out and a
            // config surface should read like what it does — `0rev1` and
            // `0pan32` say themselves in a log where `0v1` and `0n32` would
            // need the source open.
            "rev" | "rev1" | "rev0" => {
                let now = lp.speed();
                let back = match rest {
                    "rev1" => true,
                    "rev0" => false,
                    _ => now > 0.0,
                };
                // Direction changes the sign and keeps the rate, so reversing a
                // half-speed loop leaves it at half speed — the two are one
                // parameter and this is the arithmetic that says so.
                let want = now.abs() * if back { -1.0 } else { 1.0 };
                lp.want(want, lp.pendulum.load(Ordering::Relaxed));
                return format!(
                    "loop {} plays {} at x{}.",
                    li,
                    if back { "backwards" } else { "forwards" },
                    want.abs()
                );
            }
            // Forward, then back. Doubles the cycle, which is the point: a
            // pendulum that fitted into one cycle would be a different effect
            // wearing the name.
            // **An empty tape of a stated length.**
            //
            // Every other way a loop gets its length is by *recording* one: a
            // first take defines the cycle, a multiply extends it. A tape does
            // not work that way — you thread a loop of a chosen length and
            // then play onto it — so Revox needs a way to say "eight seconds,
            // empty, going round" before anything has been played.
            //
            // **One silent layer, not none.** Playback sums `0..n_layers` and
            // the layer being recorded sits *at* `n_layers`, so a loop with no
            // layers is silent even while something is being written into it.
            // In Revox that would matter: the erasing write goes into layer
            // zero, which is exactly the layer that has to be playing for the
            // tape to come round under your hands.
            //
            // Refused rather than applied when the loop has anything in it. It
            // is a way of *starting*, and quietly resizing a loop with material
            // in it would be a trim — a thing this engine does not have and
            // should not grow by accident.
            _ if rest.starts_with("blank") => {
                let arg = rest[5..].trim();
                let secs = match arg.parse::<f64>() {
                    Ok(v) if v > 0.0 => v,
                    Ok(_) => return format!("a tape wants a length in seconds, not {}.", arg),
                    _ => return format!("a tape wants a length in seconds, not `{}`.", arg),
                };
                if lp.is_recording() {
                    return format!("loop {} is recording; finish that first.", li);
                }
                // **Threaded, not recorded** is the test — not the layer count,
                // which cannot tell them apart because a threaded tape has one
                // layer in order to play at all.
                if lp.n_layers.load(Ordering::Acquire) > 0
                    && !lp.threaded.load(Ordering::Relaxed)
                {
                    return format!(
                        "loop {} has something in it; clear it before threading a tape.",
                        li
                    );
                }
                let mut len = (secs * sr as f64).round() as usize;
                if len > sh.max_frames {
                    return format!(
                        "{:.1} s is past --max-secs; the longest tape here is {:.1} s.",
                        secs,
                        sh.max_frames as f64 / sr as f64
                    );
                }
                // The grid rounds it, because only the engine knows where the
                // grid is — and a tape that does not line up with the anchor
                // loop is a tape that drifts against everything else.
                let mut said = String::new();
                if lp.quant.load(Ordering::Relaxed) {
                    if let Some((_, glen)) = sh.grid() {
                        let n = ((len as f64 / glen as f64).round() as usize).max(1);
                        len = n * glen;
                        said = format!(" ({} grid cycle{})", n, if n == 1 { "" } else { "s" });
                    }
                }
                thread_blank(sh, li, len);
                return format!(
                    "loop {} is an empty {:.3} s tape{}, going round.",
                    li,
                    len as f64 / sr as f64,
                    said
                );
            }
            // **Revox mode: the loop becomes a tape.**
            //
            // Entering flattens what is there to one layer, because a tape has
            // no layers and a mode that only half applied would be worse than
            // either. That is not reversible — `rvx0` stops the erasing but
            // does not unfold what was folded — and the ack says so, because a
            // player is entitled to know which of their presses was the one
            // that could not be taken back.
            "rvx" | "rvx1" | "rvx0" => {
                let on = match rest {
                    "rvx1" => true,
                    "rvx0" => false,
                    _ => !lp.revox.load(Ordering::Relaxed),
                };
                if lp.is_recording() {
                    return format!("loop {} is recording; finish that first.", li);
                }
                let was = lp.n_layers.load(Ordering::Acquire);
                lp.revox.store(on, Ordering::Relaxed);
                if on {
                    sh.flatten(li, sh.out_frames.load(Ordering::Acquire) as i64);
                    let now = lp.n_layers.load(Ordering::Acquire);
                    return format!(
                        "loop {} is a tape now, {} a pass{}. Undo is gone.",
                        li,
                        fb_words(lp),
                        if was > now {
                            format!(" ({} layers folded into one)", was)
                        } else {
                            String::new()
                        }
                    );
                }
                return format!("loop {} records in layers again; it is still one layer.", li);
            }
            // What a Revox pass leaves of what was under it, in decibels. Zero
            // is a tape that never erases and -60 is one that replaces.
            //
            // Its own number rather than `dec`'s: they are the same musical idea
            // by two mechanisms, one destroying and one not, and a single value
            // meaning "resolution here, erase head there" depending on a flag is
            // exactly the overload this engine keeps refusing.
            _ if rest.starts_with("fb") => {
                let arg = rest[2..].trim();
                if arg.is_empty() {
                    return format!("a Revox pass on loop {} leaves {} a pass.", li, fb_words(lp));
                }
                match arg.parse::<f32>() {
                    Ok(db) if db > 0.0 => {
                        return format!(
                            "feedback is a loss, so it wants zero or less; {} would run away.",
                            db
                        )
                    }
                    Ok(db) if db >= -60.0 => {
                        let g = if db <= -60.0 { 0.0 } else { 10f32.powf(db / 20.0) };
                        lp.fb.store(g.to_bits(), Ordering::Relaxed);
                        return format!("a Revox pass on loop {} leaves {} a pass.", li, fb_words(lp));
                    }
                    Ok(db) => return format!("feedback wants 0 to -60 dB, not {}.", db),
                    _ => return format!("feedback wants decibels, not `{}`.", arg),
                }
            }
            "pend" | "pend1" | "pend0" => {
                let want = match rest {
                    "pend1" => true,
                    "pend0" => false,
                    _ => !lp.pendulum.load(Ordering::Relaxed),
                };
                lp.want(lp.speed(), want);
                return format!(
                    "loop {} {}.",
                    li,
                    if want {
                        "swings forward then back"
                    } else {
                        "runs one way"
                    }
                );
            }
            // One pass per trigger, rather than turning for ever.
            //
            // A mode, not a gesture, because it costs a loop its place in the
            // phase-locked set: firing moves `origin`, which is the one thing
            // this engine otherwise never does. Making it something you switch on
            // means a loop cannot lose its grid by accident.
            "one" | "one1" | "one0" => {
                let on = match rest {
                    "one1" => true,
                    "one0" => false,
                    _ => !lp.one_shot.load(Ordering::Relaxed),
                };
                lp.one_shot.store(on, Ordering::Relaxed);
                if !on {
                    // Back to a loop, from wherever the last pass left it. Its
                    // `origin` has moved and stays moved — that is what firing
                    // did, and pretending otherwise would put the audio somewhere
                    // nobody chose.
                    lp.shot_end.store(i64::MIN, Ordering::Release);
                }
                return if on {
                    format!(
                        "loop {} is a one-shot: silent now, one pass each time it fires.",
                        li
                    )
                } else {
                    format!("loop {} turns for ever again.", li)
                };
            }
            // Wait for a sound instead of starting on the press.
            "lev" | "lev1" | "lev0" => {
                let on = match rest {
                    "lev1" => true,
                    "lev0" => false,
                    _ => !lp.level_arm.load(Ordering::Relaxed),
                };
                lp.level_arm.store(on, Ordering::Relaxed);
                // Turning it off under a loop that is already waiting has to end
                // the wait, or the loop keeps the input for a recording that can
                // no longer begin.
                if !on && lp.is_armed() {
                    lp.state.set(IDLE);
                    lp.arm_from.store(i64::MIN, Ordering::Release);
                }
                return if on {
                    format!(
                        "loop {} waits for a sound over {} and reaches {:.0} ms back past it.",
                        li,
                        thresh_words(sh),
                        ARM_REACH_MS
                    )
                } else {
                    format!("loop {} records on the press again.", li)
                };
            }
            // How much a pass costs the material already there, in decibels.
            //
            // Decibels rather than a gain, because that is the unit the effect
            // is actually thought in — "three down a pass" is a musical
            // statement where "point seven oh eight" is a number — and because
            // it makes the ladder on the pedal readable.
            _ if rest.starts_with("dec") => {
                let arg = rest[3..].trim();
                if arg.is_empty() {
                    return format!("loop {} {}.", li, decay_words(lp));
                }
                match arg.parse::<f32>() {
                    // Positive would be feedback above unity, which is not a
                    // longer decay, it is a loop that gets louder until it
                    // clips. Refused by name rather than clamped.
                    Ok(db) if db > 0.0 => {
                        return format!(
                            "decay is a loss, so it wants zero or less; {} per pass would \
                             run away.",
                            db
                        )
                    }
                    Ok(db) if db >= -60.0 => {
                        lp.decay
                            .store(10f32.powf(db / 20.0).to_bits(), Ordering::Relaxed);
                        return format!("loop {} {}.", li, decay_words(lp));
                    }
                    Ok(db) => return format!("decay wants 0 to -60 dB a pass, not {}.", db),
                    _ => return format!("decay wants decibels a pass, not `{}`.", arg),
                }
            }
            // Crossfade the wrap, in milliseconds. Zero is off.
            //
            // Says when it will do nothing. A loop whose layers kept no
            // continuation has nothing to fade *from*, so the setting takes and
            // is inaudible — which is the exact shape of failure this surface
            // exists to prevent, and costs one sentence to rule out.
            _ if rest.starts_with("xf") => {
                let arg = rest[2..].trim();
                if arg.is_empty() {
                    return format!("loop {} wraps with {}.", li, fade_words(lp, sr));
                }
                match arg.parse::<f64>() {
                    // Half a second is already far longer than a wrap wants; past
                    // that it is not a join, it is a different effect.
                    Ok(ms) if (0.0..=MAX_FADE_MS).contains(&ms) => {
                        lp.fade
                            .store((ms / 1000.0 * sr as f64).round() as usize, Ordering::Relaxed);
                        // Which layers can actually use it, said in numbers.
                        //
                        // All-or-nothing was the first version and it was the
                        // usual half-truth: a loop where two layers of three
                        // kept a continuation wraps two-thirds seamlessly and
                        // reported nothing at all, so the one hard join left
                        // would have been a click with no explanation anywhere.
                        let n = lp.n_layers.load(Ordering::Acquire);
                        let kept = (0..n).filter(|&l| lp.layer_tail(l) > 0).count();
                        return format!(
                            "loop {} wraps with {}.{}",
                            li,
                            fade_words(lp, sr),
                            match (ms > 0.0, kept, n) {
                                (false, _, _) | (_, _, 0) => String::new(),
                                (_, 0, _) => "  Nothing here kept a continuation, though, so \
                                              there is nothing to fade from."
                                    .into(),
                                (_, k, n) if k == n => String::new(),
                                (_, k, n) => format!(
                                    "  {} of {} layers kept a continuation; the rest still \
                                     join hard.",
                                    k, n
                                ),
                            }
                        );
                    }
                    Ok(ms) => return format!("the wrap fade wants 0 to {:.0} ms, not {}.", MAX_FADE_MS, ms),
                    _ => return format!("the wrap fade wants milliseconds, not `{}`.", arg),
                }
            }
            // How often a pass sounds. A probability rather than a ratio,
            // because the ladder the board offers (always, 3 in 4, 1 in 2, 1 in
            // 4, 1 in 8) is a choice the *app* makes about which values are
            // worth a press, and the engine should not have opinions about that
            // — the same reason speed takes a number and not a gear.
            _ if rest.starts_with("ch") => {
                let arg = rest[2..].trim();
                if arg.is_empty() {
                    return format!("loop {} sounds {}.", li, odds_words(lp.chance_of()));
                }
                match arg.parse::<f32>() {
                    Ok(p) if (0.0..=1.0).contains(&p) => {
                        lp.chance.store(p.to_bits(), Ordering::Relaxed);
                        // Forget the pass the last roll covered, or a loop set to
                        // always would stay silent until the cycle turned over —
                        // the switch would look like it had not worked, for up to
                        // a whole cycle, which is exactly long enough to press it
                        // again and undo what you just did.
                        lp.chance_pass.store(i64::MIN, Ordering::Relaxed);
                        lp.chance_sounds.store(true, Ordering::Relaxed);
                        return format!("loop {} sounds {}.", li, odds_words(p));
                    }
                    Ok(p) => return format!("chance wants 0 to 1, not {}.", p),
                    _ => return format!("chance wants a probability, not `{}`.", arg),
                }
            }
            // The level a sound has to reach. Rig-wide, like the click — it
            // describes the room and the instrument, not any one loop.
            _ if rest.starts_with("arm") => {
                let arg = rest[3..].trim();
                if arg.is_empty() {
                    return format!("a level-armed loop starts at {}.", thresh_words(sh));
                }
                match arg.parse::<f64>() {
                    // Full scale to the noise floor. Above zero can never be
                    // reached and below -80 is the converter's own hiss, so both
                    // are refused rather than accepted into a mode that would
                    // then never fire, or fire immediately.
                    Ok(db) if db <= 0.0 && db >= -80.0 => {
                        sh.arm_thresh.store(db_to_mag(db).to_bits(), Ordering::Relaxed);
                        return format!("a level-armed loop now starts at {}.", thresh_words(sh));
                    }
                    Ok(db) => return format!("the arm level wants 0 to -80 dBFS, not {}.", db),
                    _ => return format!("the arm level wants a number of dBFS, not `{}`.", arg),
                }
            }
            // This loop's level, in decibels, with **silence at the bottom
            // rather than a very quiet loop**. A fader that cannot reach zero
            // is a fader you do not trust, and -60 dB is inaudible anyway —
            // saying "silent" is more honest than reporting a number nobody can
            // hear.
            //
            // Above unity is refused rather than clamped, for the same reason
            // decay refuses positive: a level control that quietly declined to
            // do what it was told would be worse than one that says no.
            _ if rest.starts_with("vol") => {
                let arg = rest[3..].trim();
                if arg.is_empty() {
                    return format!("loop {} {}.", li, vol_words(lp));
                }
                match arg.parse::<f32>() {
                    Ok(db) if db > 0.0 => {
                        return format!("a loop plays at unity or below; {} dB would clip.", db)
                    }
                    Ok(db) if db >= -60.0 => {
                        let g = if db <= -60.0 { 0.0 } else { 10f32.powf(db / 20.0) };
                        lp.vol.store(g.to_bits(), Ordering::Relaxed);
                        return format!("loop {} {}.", li, vol_words(lp));
                    }
                    Ok(db) => return format!("level wants 0 to -60 dB, not {}.", db),
                    _ => return format!("level wants decibels, not `{}`.", arg),
                }
            }
            _ if rest.starts_with("pan") => {
                match rest[3..].parse::<usize>() {
                    Ok(v) if v <= 127 => {
                        lp.pan.store(v, Ordering::Relaxed);
                        let (l, r) = lp.pan_gains();
                        return format!(
                            "loop {} panned {} (L {:.2}, R {:.2}).",
                            li,
                            match v {
                                0..=10 => "hard left",
                                11..=52 => "left",
                                53..=74 => "centre",
                                75..=116 => "right",
                                _ => "hard right",
                            },
                            l,
                            r
                        );
                    }
                    // Says what was wrong rather than ignoring it. A config
                    // command that silently does nothing is the failure this
                    // whole surface is built against.
                    _ => return format!("pan wants 0-127, not `{}`.", &rest[3..]),
                }
            }
            "h" | "h1" | "h0" => {
                let want = match rest {
                    "h1" => false,
                    "h0" => true,
                    _ => !lp.muted.load(Ordering::Relaxed),
                };
                lp.muted.store(want, Ordering::Relaxed);
                return format!(
                    "loop {} {}.",
                    li,
                    if want { "stopped, still turning" } else { "playing" }
                );
            }
            "c" => {
                lp.cleared();
                lp.win_in.store(0, Ordering::Relaxed);
                lp.win_out.store(0, Ordering::Relaxed);
                lp.rot.store(0, Ordering::Relaxed);
                lp.edit_restart.store(0, Ordering::Relaxed);
                lp.pend_set.store(false, Ordering::Relaxed);
                for l in 0..sh.max_layers {
                    sh.zero_layer(li, l);
                    lp.set_layer_shape(l, Shape { len: 0, tail: 0, born: 0 });
                }
                sh.clear_env(li);
                sh.release_anchor(li);
                // A fixed rig does not have empty loops, it has empty tapes:
                // clearing puts the tape back so the next take closes itself.
                if sh.fixed_frames > 0 {
                    thread_blank(sh, li, sh.fixed_frames);
                    return format!(
                        "loop {} cleared; an empty {:.3} s tape again.",
                        li,
                        sh.fixed_frames as f64 / sr as f64
                    );
                }
                return format!("loop {} cleared.", li);
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
                return format!("click {}.", if on { "on" } else { "off" });
            }
            "m" | "m1" | "m0" => {
                let on = match line.trim() {
                    "m1" => true,
                    "m0" => false,
                    _ => !sh.monitor.load(Ordering::Relaxed),
                };
                sh.monitor.store(on, Ordering::Relaxed);
                return format!(
                    "input monitoring {}.{}",
                    if on { "on" } else { "off" },
                    if on {
                        "  (the interface's own direct monitoring is lower latency)"
                    } else {
                        ""
                    }
                );
            }
            "l" => {
                let inp = sh
                    .in_peak
                    .iter()
                    .map(|p| f32::from_bits(p.swap(0, Ordering::Relaxed)))
                    .fold(0.0f32, f32::max);
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
                let len = lp.loop_len.load(Ordering::Acquire);
                for l in 0..lp.n_layers.load(Ordering::Acquire) {
                    if len > 0 {
                        draw_layer(sh, li, l, len, sr);
                    }
                }
                println!(
                    "  {} layers, loop {} frames ({:.3} s), state {}, K {:+}{}",
                    lp.n_layers.load(Ordering::Acquire),
                    len,
                    len as f64 / sr as f64,
                    match lp.state.get() {
                        FIRST => "recording first",
                        OVERDUB => "overdubbing",
                        MULTIPLY => "multiplying",
                        PLAYING => "playing",
                        _ => "idle",
                    },
                    sh.k.load(Ordering::Acquire),
                    if lp.overflowed.load(Ordering::Relaxed) {
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
