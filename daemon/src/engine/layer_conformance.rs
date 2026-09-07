//! One layer slot held to the Glassbox artifact.
//!
//! `purescript-glassbox/core/machines/itajara-layer.json` is the life of one
//! layer slot of one loop written down as data — six states, ten events,
//! four facts — and `conformance/vectors/itajara-layer.json` is every
//! (state × event × fact assignment) the artifact admits. This module is
//! the sibling of `conformance`, which holds the loop's phase machine to
//! `itajara-loop.json`, and it follows it closely: the same vector reader
//! (borrowed from there), the same shape of build, deliver and compare.
//! Build the rig with loop 0 in the vector's `from` state at a chosen slot,
//! deliver the `event` the way the daemon does, and compare the state the
//! slot is then in, read through `layer_state`, with the vector's
//! `current`; and for a refusal, the ack's tag with the vector's `refusal`.
//! Commands are printed, not asserted, as in the loop's replay.
//!
//! Made 2026-09-07, the day the artifact was: the inherited-window fault
//! (`Layer::set_shape`) is what the artifact exists to say, and this is
//! the table replayed through the engine that carries the one-line fix.
//!
//! # The mapping: artifact state ← engine
//!
//! For slot `slot` of the loop, with `n = n_layers`, `r = redo_to` and
//! `rec_slot` the slot the take in hand writes into:
//!
//! | artifact | engine |
//! |---|---|
//! | `writing` | `First`, `Overdub` or `Multiply`, `rec_slot == slot`, and `slot == n`: a new layer being written |
//! | `summing` | `Overdub`, `rec_slot == slot`, and `slot < n`: a pass into a counted layer — an alternate loop's open overdub |
//! | `sounding` | `slot < n` and the layer is `on`, and not the write target |
//! | `off` | `slot < n` and not `on` |
//! | `undone` | `n <= slot < r`: kept for redo |
//! | `free` | otherwise: `slot >= n` and `slot >= r` |
//!
//! # Facts → the rig
//!
//! The facts are the loop's, so they decide *which slot* is watched and
//! what sits around it. Loop 0, the arena and constants of `conformance`.
//!
//! | artifact | how `rig_in` puts the rig in it |
//! |---|---|
//! | `top` | true (default): one layer, slot 0. False: two layers, slot 0 — slot 1 is the top |
//! | `next-to-redo` | two layers, both undone (`0u` twice). True (default): slot 0. False: slot 1 |
//! | `was-on` | true (default): the layer is left on before it is undone. False: `0ly<k>0` first |
//! | `first-take` | true (default for `writing`): `0r` on an empty loop. False: `0r` on a loop with one layer, an overdub into slot 1 |
//!
//! `summing` is built as `0alt1` on a loop with one sounding layer, then
//! `0r`: the open overdub sums into the layer that sounds. `writing` is
//! `0r` on an empty loop, settled with `stamp`, with `reached` set so a
//! commit has something to close — as the loop's replay does.
//!
//! # Events → what the test does
//!
//! | artifact | test |
//! |---|---|
//! | `start` | `0r`: a new take into the next free slot. On a slot being written, `0t0.1` — `r` there is the press that closes the take (the artifact's `commit`), and the artifact names `t` among `start`'s verbs |
//! | `sum` | `0r` on a loop whose `alt` the build set before anything was turned off, since `alt1` solos the newest layer |
//! | `commit` | for an open write, `0r` again: the press closes it, and the ack says `committed` or `another pass`. Nothing, otherwise: there is no write to close |
//! | `on`, `off` | `0ly<k>1`, `0ly<k>0`, with `k = slot + 1` |
//! | `window` | `0lw<k>:0:50` |
//! | `undo`, `redo`, `clear` | `0u`, `0y`, `0c` |
//! | `lost` | what the supervisor does: `run::drop_takes` |
//!
//! Every event is followed by `stamp` at now, as in the loop's replay.
//!
//! # Refusals — the addressing rule
//!
//! The artifact refuses per *slot*; the daemon refuses per *loop*, and
//! some of the artifact's refusals are about a verb reaching the wrong
//! slot — which the daemon's addressing makes impossible rather than
//! refuses out loud. Two kinds, handled apart:
//!
//! **Tags the daemon voices**, matched by ack text. `still-writing` is
//! `still_recording`'s three sentences ("finish that first", "is
//! recording", "is listening", "waiting for the bar"). The daemon's
//! *counting* refusals — "has N layers, not a layer K", "nothing to undo",
//! "nothing to redo", "already at its last take" — say that a slot is not
//! counted without saying why, so the tag is read from the slot's own
//! state: `free` → `nothing-here`, `undone` → `not-counted`, `writing`
//! or `summing` → `still-writing` (the daemon counts the layer under write
//! as not yet a layer), `sounding` or `off` → `not-undone` (the two redo
//! sentences on a counted slot).
//!
//! **Tags the daemon cannot voice**, because the verb never reaches the
//! slot: `occupied` (a take never starts over a counted slot; `r` goes
//! into the next free one), `not-top` (undo reaches the top, not this
//! slot), `not-next` (redo reaches the lowest undone), `not-undone` when
//! the redo reaches a slot above, and `nothing-here` / `not-counted` for
//! `sum` (the summed pass reaches the layer that sounds; with no sounding
//! layer at all, `r` is a `start`, so the build gives the loop a sounding
//! layer beneath the watched slot). These are delivered anyway, THIS
//! slot's state must be unchanged, the tag is not compared, and they are
//! counted apart as "refused by addressing".
//!
//! # Where the artifact and the engine disagree
//!
//! The first replay (2026-09-07) found two, and both were settled by
//! changing the artifact, because the engine had reasons: `summing × on /
//! off / window` — the engine takes `ly` and `lw` on a layer a pass is
//! summing into, and the artifact now says `stay`; and `writing × lost`
//! for an overdub — `drop_takes` zeroes a partial layer ("a layer with a
//! gap is worse than no layer"), and the artifact now goes to `free`
//! whatever the take was, its `first-take` fact gone. The loop artifact's
//! `overdubbing-open × lost → playing [close-layer]` still names the wrong
//! command for that exit; its replay cannot see it and it is a known lie.
//! Only the two `sum`-under-a-write vectors are skipped now, for want of a
//! verb.
//! - `writing × sum` and `summing × sum`: `r` under a write is the press
//!   that closes it, and `sum` has no other verb, so the daemon cannot
//!   receive the event.

use std::sync::atomic::Ordering;

use super::callbacks;
use super::conformance::{read_vectors, vectors_path, Vector, LEN, LOOP, NOW, SR, STAMP_FRAMES};
use super::dispatch::dispatch;
use super::run::drop_takes;
use super::tests::{lay, one_layer_loop, rig};
use super::{Phase, Shared};

/// The artifact state slot `slot` of loop `li` is in. The table in the
/// module comment, as code.
pub(crate) fn layer_state(sh: &Shared, li: usize, slot: usize) -> &'static str {
    let lp = sh.lp(li);
    let n = lp.n_layers.load(Ordering::Acquire);
    let r = lp.redo_to.load(Ordering::Acquire);
    let phase = lp.phase();
    let target = lp.is_recording() && lp.rec_slot.load(Ordering::Acquire) == slot;
    if target && slot == n {
        "writing"
    } else if target && slot < n && phase == Phase::Overdub {
        "summing"
    } else if slot < n {
        if lp.layers[slot].on() { "sounding" } else { "off" }
    } else if slot < r {
        "undone"
    } else {
        "free"
    }
}

/// The refusal tag an ack carries, seen from a slot in state `state`. See
/// the module comment: the daemon's counting refusals do not say why the
/// slot is not counted, so the slot does.
fn layer_refusal_tag(ack: &str, state: &str) -> Option<&'static str> {
    const WRITING: &[&str] = &["finish that first", "is recording", "is listening", "waiting for the bar"];
    const COUNTING: &[&str] = &["not a layer", "nothing to undo", "nothing to redo", "already at its last take"];
    if WRITING.iter().any(|w| ack.contains(w)) {
        return Some("still-writing");
    }
    if COUNTING.iter().any(|w| ack.contains(w)) {
        return Some(match state {
            "free" => "nothing-here",
            "undone" => "not-counted",
            "writing" | "summing" => "still-writing",
            _ => "not-undone",
        });
    }
    None
}

/// Whether the vector's refusal is one the daemon cannot voice because
/// the verb never reaches the slot.
fn by_addressing(v: &Vector) -> bool {
    match v.refusal.as_deref() {
        Some("occupied" | "not-top" | "not-next" | "not-undone") => true,
        Some("nothing-here" | "not-counted") => v.event == "sum",
        _ => false,
    }
}

/// Why a vector is not replayed, before any rig is built: an event the
/// daemon has no verb for from that state.
fn not_replayed(v: &Vector) -> Option<String> {
    match (v.from.as_str(), v.event.as_str()) {
        ("writing" | "summing", "sum") => Some(
            "no verb: `r` under a write is the press that closes it, and `sum` has no other".into(),
        ),
        _ => None,
    }
}

/// A rig with loop 0 in the vector's `from` state at a chosen slot, under
/// its facts, or the reason it cannot be built. The slot is the second
/// half of the answer.
fn rig_in(v: &Vector) -> Result<(Shared, usize), String> {
    if let Some(why) = not_replayed(v) {
        return Err(why);
    }
    let from = v.from.as_str();
    let sum = v.event == "sum";
    let sh = rig(LEN);
    sh.out_frames.store(NOW, Ordering::Release);
    let lp = sh.lp(0);
    let expect = |ack: String, word: &str| -> Result<(), String> {
        if ack.contains(word) {
            Ok(())
        } else {
            Err(format!("building `{}`: the daemon answered {:?}", from, ack))
        }
    };
    // A playing loop of `layers` layers of a first take's worth each.
    // `lay` counts the layer without moving `redo_to`, as `add_layer`
    // would have; the stack is set to what a commit leaves.
    let playing = |layers: usize| {
        one_layer_loop(&sh, 0, LOOP, 0.25);
        for l in 1..layers {
            lay(&sh, 0, l, LOOP, 0.25);
        }
        lp.redo_to.store(layers, Ordering::Release);
        lp.enter(Phase::Playing, NOW as i64);
    };
    let slot = match from {
        "free" => {
            if sum {
                // The pass goes into the layer that sounds; the watched
                // slot is the free one above it.
                playing(1);
                expect(dispatch(&sh, SR, "0alt1"), "alternates")?;
                1
            } else {
                0
            }
        }
        "writing" => {
            if v.fact("first-take").unwrap_or(true) {
                expect(dispatch(&sh, SR, "0r"), "recording")?;
                callbacks::stamp(&sh, 0, NOW, STAMP_FRAMES);
                // The input has written a take's worth, so `commit` has
                // something to close.
                lp.reached.store(LOOP, Ordering::Release);
                0
            } else {
                playing(1);
                expect(dispatch(&sh, SR, "0r"), "overdubbing onto layer 2")?;
                callbacks::stamp(&sh, 0, NOW, STAMP_FRAMES);
                1
            }
        }
        "summing" => {
            playing(1);
            expect(dispatch(&sh, SR, "0alt1"), "alternates")?;
            expect(dispatch(&sh, SR, "0r"), "sums into layer 1")?;
            callbacks::stamp(&sh, 0, NOW, STAMP_FRAMES);
            0
        }
        "sounding" | "off" => {
            playing(if v.fact("top").unwrap_or(true) { 1 } else { 2 });
            // Before anything is turned off: `alt1` solos the newest.
            if sum {
                expect(dispatch(&sh, SR, "0alt1"), "alternates")?;
            }
            if from == "off" {
                expect(dispatch(&sh, SR, "0ly10"), "is off")?;
            }
            0
        }
        "undone" => {
            playing(2);
            if sum {
                // One counted layer for the pass to reach, one undone
                // above it to watch.
                expect(dispatch(&sh, SR, "0u"), "removed")?;
                expect(dispatch(&sh, SR, "0alt1"), "alternates")?;
                1
            } else {
                let slot = if v.fact("next-to-redo").unwrap_or(true) { 0 } else { 1 };
                if !v.fact("was-on").unwrap_or(true) {
                    expect(dispatch(&sh, SR, &format!("0ly{}0", slot + 1)), "is off")?;
                }
                expect(dispatch(&sh, SR, "0u"), "removed")?;
                expect(dispatch(&sh, SR, "0u"), "removed")?;
                slot
            }
        }
        other => return Err(format!("no way to build `{}`", other)),
    };
    callbacks::stamp(&sh, 0, NOW, STAMP_FRAMES);
    let got = layer_state(&sh, 0, slot);
    if got != from {
        return Err(format!("building `{}` left slot {} `{}`", from, slot, got));
    }
    Ok((sh, slot))
}

/// Deliver the event to slot `slot` of loop 0 the way the daemon does,
/// and settle the callback. The ack, for a press; nothing for a runtime
/// event.
fn deliver(sh: &Shared, v: &Vector, slot: usize) -> Option<String> {
    let lp = sh.lp(0);
    let k = slot + 1;
    let ack = match v.event.as_str() {
        "start" => Some(dispatch(
            sh,
            SR,
            if matches!(v.from.as_str(), "writing" | "summing") { "0t0.1" } else { "0r" },
        )),
        "sum" => {
            assert!(lp.alt.load(Ordering::Relaxed), "the build set `alt`");
            Some(dispatch(sh, SR, "0r"))
        }
        "commit" => {
            if matches!(lp.phase(), Phase::First | Phase::Overdub) {
                let ack = dispatch(sh, SR, "0r");
                assert!(
                    ack.contains("committed") || ack.contains("another pass"),
                    "the press closed the write: {:?}",
                    ack
                );
                Some(ack)
            } else {
                None
            }
        }
        "on" => Some(dispatch(sh, SR, &format!("0ly{}1", k))),
        "off" => Some(dispatch(sh, SR, &format!("0ly{}0", k))),
        "window" => Some(dispatch(sh, SR, &format!("0lw{}:0:50", k))),
        "undo" => Some(dispatch(sh, SR, "0u")),
        "redo" => Some(dispatch(sh, SR, "0y")),
        "clear" => Some(dispatch(sh, SR, "0c")),
        "lost" => {
            drop_takes(sh);
            None
        }
        other => panic!("no way to deliver `{}`", other),
    };
    let now = sh.out_frames.load(Ordering::Acquire);
    callbacks::stamp(sh, 0, now, STAMP_FRAMES);
    ack
}

/// **Every vector the artifact admits, replayed through the engine.** The
/// slot's state after, read through `layer_state`, must be the vector's;
/// a refusal must carry the vector's tag, and a move or stay must not be
/// a refusal — except where the refusal is one of addressing, where the
/// slot must simply be untouched. Vectors the engine cannot be put in, or
/// that it disagrees with, are skipped by name and counted; a missing
/// vectors file is reported and passes.
#[test]
fn the_engine_replays_the_layer_s_table() {
    let path = vectors_path("itajara-layer");
    let Some(vectors) = read_vectors(&path, "itajara-layer") else {
        println!(
            "layer conformance: no vectors at {} — set GLASSBOX_DIR to the purescript-glassbox checkout; nothing replayed",
            path.display()
        );
        return;
    };
    let mut replayed = 0usize;
    let mut addressing = 0usize;
    let mut skipped: Vec<(String, String)> = Vec::new();
    let mut mismatches: Vec<String> = Vec::new();
    for v in &vectors {
        let (sh, slot) = match rig_in(v) {
            Ok(built) => built,
            Err(why) => {
                skipped.push((why, v.describe()));
                continue;
            }
        };
        let ack = deliver(&sh, v, slot);
        replayed += 1;
        let got = layer_state(&sh, 0, slot);
        let tag = ack.as_deref().and_then(|a| layer_refusal_tag(a, got));
        let lp = sh.lp(0);
        let unvoiced = by_addressing(v) && tag.is_none();
        println!(
            "  {}  | slot {}: {} {}| n {} redo_to {}{} | commands owed: [{}]",
            v.describe(),
            slot,
            got,
            match &ack {
                Some(a) => format!("ack {:?} ", a),
                None => String::new(),
            },
            lp.n_layers.load(Ordering::Acquire),
            lp.redo_to.load(Ordering::Acquire),
            if unvoiced { " | refused by addressing" } else { "" },
            v.commands.join(", ")
        );
        if unvoiced {
            addressing += 1;
            if got != v.from {
                mismatches.push(format!(
                    "{}: the verb reached slot {} after all — it is `{}`",
                    v.describe(),
                    slot,
                    got
                ));
            }
            continue;
        }
        if got != v.current {
            mismatches.push(format!(
                "{}: slot {} is `{}`{}",
                v.describe(),
                slot,
                got,
                ack.as_ref().map(|a| format!(" (ack {:?})", a)).unwrap_or_default()
            ));
        }
        let want = v.refusal.as_deref();
        if tag != want {
            mismatches.push(format!(
                "{}: the engine {} (ack {:?})",
                v.describe(),
                match tag {
                    Some(t) => format!("refused `{}`", t),
                    None => "did not refuse".to_string(),
                },
                ack.unwrap_or_default()
            ));
        }
    }
    println!(
        "layer conformance: replayed {} ({} refused by addressing) / skipped {} of {} vectors from {}",
        replayed,
        addressing,
        skipped.len(),
        vectors.len(),
        path.display()
    );
    let mut reasons: Vec<(String, usize)> = Vec::new();
    for (why, _) in &skipped {
        match reasons.iter_mut().find(|(r, _)| r == why) {
            Some((_, n)) => *n += 1,
            None => reasons.push((why.clone(), 1)),
        }
    }
    for (why, n) in &reasons {
        println!("  skipped {}: {}", n, why);
    }
    for (why, which) in &skipped {
        println!("    {} — {}", which, why);
    }
    for m in &mismatches {
        println!("MISMATCH {}", m);
    }
    assert!(mismatches.is_empty(), "{} mismatches (above)", mismatches.len());
    assert!(
        replayed * 10 >= vectors.len() * 8,
        "replayed {} of {} vectors, under eight in ten",
        replayed,
        vectors.len()
    );
}

/// The mapping walks one slot round its life: free, writing, sounding,
/// off, undone, and back through redo as it was — off, since that is how
/// it was undone (`was-on`), and its window kept, since undo keeps the
/// audio and everything that belongs to it.
#[test]
fn the_mapping_walks_a_slot_through_its_life() {
    let sh = rig(LEN);
    sh.out_frames.store(NOW, Ordering::Release);
    let lp = sh.lp(0);
    assert_eq!(layer_state(&sh, 0, 0), "free");
    assert!(dispatch(&sh, SR, "0r").contains("recording"));
    callbacks::stamp(&sh, 0, NOW, STAMP_FRAMES);
    assert_eq!(layer_state(&sh, 0, 0), "writing");
    assert_eq!(layer_state(&sh, 0, 1), "free", "only the slot under write is writing");
    lp.reached.store(LOOP, Ordering::Release);
    assert!(dispatch(&sh, SR, "0r").contains("committed"));
    assert_eq!(layer_state(&sh, 0, 0), "sounding");
    assert!(dispatch(&sh, SR, "0ly10").contains("is off"));
    assert_eq!(layer_state(&sh, 0, 0), "off");
    assert!(dispatch(&sh, SR, "0lw1:0:50").contains("plays 0..50"));
    assert!(dispatch(&sh, SR, "0u").contains("Empty now"));
    assert_eq!(layer_state(&sh, 0, 0), "undone");
    assert_eq!(lp.layer_window(0), Some((0, 50)), "undo keeps the window with the audio");
    assert!(dispatch(&sh, SR, "0y").contains("redone"));
    assert_eq!(layer_state(&sh, 0, 0), "off", "back as it was undone");
    assert!(dispatch(&sh, SR, "0ly11").contains("is on"));
    assert_eq!(layer_state(&sh, 0, 0), "sounding");
    // A new take into the slot is where it is made clean.
    assert!(dispatch(&sh, SR, "0u").contains("Empty now"));
    assert!(dispatch(&sh, SR, "0r").contains("recording"));
    callbacks::stamp(&sh, 0, NOW, STAMP_FRAMES);
    assert_eq!(layer_state(&sh, 0, 0), "writing");
    assert_eq!(lp.redo_to.load(Ordering::Acquire), 0, "the kept take is dropped");
    lp.reached.store(LOOP, Ordering::Release);
    assert!(dispatch(&sh, SR, "0r").contains("committed"));
    assert_eq!(layer_state(&sh, 0, 0), "sounding");
    assert_eq!(lp.layer_window(0), None, "`set_shape` dropped the window");
}

/// A summed pass leaves the slot `summing` and then `sounding`, with
/// `n_layers` and `redo_to` where they were: a pass is more of the layer,
/// not another one.
#[test]
fn a_summed_pass_is_not_another_layer() {
    let sh = rig(LEN);
    sh.out_frames.store(NOW, Ordering::Release);
    one_layer_loop(&sh, 0, LOOP, 0.25);
    let lp = sh.lp(0);
    lp.redo_to.store(1, Ordering::Release);
    lp.enter(Phase::Playing, NOW as i64);
    assert!(dispatch(&sh, SR, "0alt1").contains("alternates"));
    assert_eq!(layer_state(&sh, 0, 0), "sounding");
    assert!(dispatch(&sh, SR, "0r").contains("sums into layer 1"));
    callbacks::stamp(&sh, 0, NOW, STAMP_FRAMES);
    assert_eq!(layer_state(&sh, 0, 0), "summing");
    assert_eq!(layer_state(&sh, 0, 1), "free", "the pass makes no new layer");
    assert_eq!(lp.n_layers.load(Ordering::Acquire), 1);
    assert!(dispatch(&sh, SR, "0r").contains("another pass"));
    assert_eq!(layer_state(&sh, 0, 0), "sounding");
    assert_eq!(lp.n_layers.load(Ordering::Acquire), 1);
    assert_eq!(lp.redo_to.load(Ordering::Acquire), 1);
}
