//! The phase machine held to the Glassbox artifact.
//!
//! `purescript-glassbox/core/machines/itajara-loop.json` is this engine's
//! phase machine written down as data — twelve states, thirteen events,
//! twelve facts, one config — and `conformance/vectors/itajara-loop.json`
//! is every (state × event × guard assignment) the artifact admits, with
//! what the Glassbox runtime answers for each. This module replays that
//! table through the engine: build the rig in the vector's `from` state
//! with its `facts` and `config`, deliver the `event` the way the daemon
//! does — a verb through `dispatch`, or what the closer, the input
//! callback, the output callback or the supervisor does — and compare the
//! state the engine is then in, read through `artifact_state`, with the
//! vector's `current`; and for a refusal, the ack's tag with the vector's
//! `refusal`. Commands are printed, not asserted: what a command does when
//! carried out is the host seam, outside the claim on both sides.
//!
//! Made 2026-09-06 (REVIEW-daemon-debt step 5b). The artifact is the spec;
//! every mismatch the first replay found was reconciled one way or the
//! other and is listed in the review doc — the daemon gained the guards the
//! artifact was prescriptive about and a cancelled arm that returns to what
//! the loop held; the artifact gained a state for the sound-then-bar wait
//! and a `no-grid` refusal for `len` on material.
//!
//! # The mapping: artifact state ← engine
//!
//! | artifact | engine |
//! |---|---|
//! | `empty` | `Idle`, no length, nothing filed for a frame to come |
//! | `sized` | `Idle` with a length; **or** `Playing` with a length and no layers — every layer undone, or a sized first take dropped by an outage. The byte cannot separate these two from `empty` and `playing`; this is where it is done |
//! | `tape` | `Playing`, `threaded`, with its one silent layer |
//! | `playing` | `Playing` with layers, not threaded |
//! | `armed-by-level` | `Armed`, nothing filed for a frame: listening |
//! | `armed-by-sound` | `Armed` with an `ARMED` request filed for a boundary: the crossing found under quantise, waiting for the bar, still holding the input |
//! | `armed-for-grid` | `Idle` (or `Playing` with no layers) with an `ARMED` request filed for a boundary |
//! | `recording-open` | `First`, `close_at` unset |
//! | `recording-sized` | `First`, `close_at` set |
//! | `overdubbing-open` | `Overdub`, `close_at` unset |
//! | `overdubbing-one-pass` | `Overdub`, `close_at` set |
//! | `multiplying` | `Multiply` |
//!
//! A request with no deadline is consumed on the next buffer; the replay
//! consumes it (`callbacks::stamp`) before reading the state, so it never
//! shows here. `Playing` with neither layers nor length is nothing the
//! engine produces and is read as `empty`.
//!
//! # Config and facts → the rig
//!
//! | artifact | how `rig_in` puts the rig in it |
//! |---|---|
//! | `fixed-rig` | `Shared.fixed_frames = 500` |
//! | `input-held-elsewhere` | loop 7 entered into `First` |
//! | `writable` false | `pendulum` on — which also makes `plain` false: the engine has no loop that is unwritable and plain |
//! | `plain` false | speed ½ — which on a loop with no length also makes it unwritable, since `not_writable` asks `not_plain` there |
//! | `at-ceiling` | `max_layers` layers laid |
//! | `level-arm` | `level_arm` on |
//! | `quantised` | `quant` on, and a Link bar of 400 frames from origin 0, since a `g1` with no bar records now — the artifact's `record` rule does not read `has-grid`, so the replay supplies one |
//! | `one-pass` | `next.plan_one_pass()` |
//! | `last-layer` | one layer rather than two |
//! | `has-length` | `loop_len = 100` |
//! | `has-layers` | one layer of 100 (a first take's worth). A layer implies a length: a vector that says has-layers and not has-length is skipped |
//! | `threaded` | the layer is a blank tape (`thread_blank`). A tape is its layer, so `threaded` without `has-layers` is realised as neither |
//! | `has-grid` | a Link bar of 400 frames |
//!
//! Facts a vector does not name are at their rest value: writable and
//! plain, nothing held elsewhere, no modes on, and the content the `from`
//! state implies (a `playing` loop has one layer of 100).
//!
//! # Events → what the test does
//!
//! | artifact | test |
//! |---|---|
//! | `record`, `multiply`, `undo`, `clear`, `fix`, `size`, `blank`, `free`, `level-off` | `0r`, `0x`, `0u`, `0c`, `0fix0.1`, `0len1`, `0blank0.1`, `0z`, `0lev0` through `dispatch`, at 1 kHz |
//! | `sound` | what the input callback does when this loop is `armed_loop()`: `callbacks::crossed` at now |
//! | `boundary` | what the output callback does at the frame the pending request names: `callbacks::stamp` there |
//! | `closed` | what the closer does: take `close_at`, and `commit` at that frame |
//! | `lost` | what the supervisor does: `run::drop_takes` |
//!
//! Every event is followed by `stamp` at now, which is the buffer the
//! callback would have run after the press. `multiplying` is closed with
//! the clock at exactly one cycle, since the test-side `dispatch` settles a
//! close by waiting for its boundary on the calling thread (the lane does
//! not; step 7).

use std::path::PathBuf;
use std::sync::atomic::Ordering;

use serde_json::Value;

use super::{Phase, Shared};
use super::callbacks;
use super::commit::commit;
use super::dispatch::dispatch;
use super::edit::thread_blank;
use super::run::drop_takes;
use super::tests::{lay, one_layer_loop, rig};

/// A test sample rate, so `fix0.1` is a hundred frames.
const SR: u32 = 1000;
/// The arena, in frames.
const LEN: usize = 1000;
/// Where the output clock stands when a vector's rig is built: off the bar
/// line, so a wait for the bar is a wait.
const NOW: usize = 50;
/// A bar, in frames, when a vector wants one. Past `NOW + STAMP_FRAMES`, so
/// a settle does not fire a request meant for the boundary.
const BAR: usize = 400;
/// The buffer the settling `stamp` covers.
const STAMP_FRAMES: usize = 16;
/// A layer's, or a sized loop's, length.
const LOOP: usize = 100;

/// The artifact state the engine is in, for loop `li`. The table in the
/// module comment, as code.
pub(crate) fn artifact_state(sh: &Shared, li: usize) -> &'static str {
    let lp = sh.lp(li);
    let len = lp.loop_len.load(Ordering::Acquire);
    let layers = lp.n_layers.load(Ordering::Acquire);
    let waits = lp.next.waits_for_boundary();
    let closes = lp.close_at.load(Ordering::Acquire) != i64::MIN;
    match lp.phase() {
        Phase::Armed if waits => "armed-by-sound",
        Phase::Armed => "armed-by-level",
        Phase::First if closes => "recording-sized",
        Phase::First => "recording-open",
        Phase::Overdub if closes => "overdubbing-one-pass",
        Phase::Overdub => "overdubbing-open",
        Phase::Multiply => "multiplying",
        Phase::Idle | Phase::Playing if waits => "armed-for-grid",
        Phase::Playing if layers > 0 && lp.threaded.load(Ordering::Relaxed) => "tape",
        Phase::Playing if layers > 0 => "playing",
        Phase::Idle | Phase::Playing if len > 0 => "sized",
        Phase::Idle | Phase::Playing => "empty",
    }
}

/// The refusal tag an ack carries, by the sentence the daemon has always
/// used for it — the artifact's tags were named after these acks. In order,
/// because an `input-held` ack ends the way a `still-recording` one does.
fn refusal_tag(ack: &str) -> Option<&'static str> {
    const TABLE: &[(&str, &str)] = &[
        ("has the input", "input-held"),
        ("is swinging", "not-writable"),
        ("is a tape running at", "not-writable"),
        ("is playing at x", "not-plain"),
        ("the ceiling", "at-ceiling"),
        ("finish that first", "still-recording"),
        ("nothing to undo", "nothing-to-undo"),
        ("nothing to multiply", "nothing-to-multiply"),
        ("has something in it", "has-material"),
        ("still playing —", "has-material"),
        ("no length set", "no-length"),
        ("no bar yet", "no-grid"),
    ];
    TABLE
        .iter()
        .find(|(needle, _)| ack.contains(needle))
        .map(|(_, tag)| *tag)
}

/// One line of the artifact's table.
struct Vector {
    from: String,
    event: String,
    config: serde_json::Map<String, Value>,
    facts: serde_json::Map<String, Value>,
    outcome: String,
    refusal: Option<String>,
    current: String,
    commands: Vec<String>,
}

impl Vector {
    fn fact(&self, id: &str) -> Option<bool> {
        self.facts.get(id).and_then(Value::as_bool)
    }
    fn config(&self, id: &str) -> Option<bool> {
        self.config.get(id).and_then(Value::as_bool)
    }
    fn describe(&self) -> String {
        let facts: Vec<String> = self
            .facts
            .iter()
            .map(|(k, v)| format!("{}={}", k, v))
            .collect();
        let config: Vec<String> = self
            .config
            .iter()
            .map(|(k, v)| format!("{}={}", k, v))
            .collect();
        format!(
            "{} --{}--> {} [{}{}] facts {{{}}} config {{{}}}",
            self.from,
            self.event,
            self.current,
            self.outcome,
            self.refusal
                .as_ref()
                .map(|r| format!(" {}", r))
                .unwrap_or_default(),
            facts.join(", "),
            config.join(", ")
        )
    }
}

/// Where the vectors are: `$GLASSBOX_DIR`, or the sibling checkout.
fn vectors_path() -> PathBuf {
    let dir = match std::env::var_os("GLASSBOX_DIR") {
        Some(d) => PathBuf::from(d),
        None => PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../../purescript-hylograph-libs/purescript-glassbox"),
    };
    dir.join("conformance/vectors/itajara-loop.json")
}

fn read_vectors(path: &PathBuf) -> Option<Vec<Vector>> {
    let text = std::fs::read_to_string(path).ok()?;
    let json: Value = serde_json::from_str(&text).expect("the vectors file is JSON");
    assert_eq!(json["glassbox-vectors"], 1, "vectors schema version");
    assert_eq!(json["machine"], "itajara-loop");
    let vectors = json["vectors"]
        .as_array()
        .expect("vectors[]")
        .iter()
        .map(|v| Vector {
            from: v["from"].as_str().unwrap().to_string(),
            event: v["event"].as_str().unwrap().to_string(),
            config: v["config"].as_object().cloned().unwrap_or_default(),
            facts: v["facts"].as_object().cloned().unwrap_or_default(),
            outcome: v["outcome"].as_str().unwrap().to_string(),
            refusal: v["refusal"].as_str().map(str::to_string),
            current: v["current"].as_str().unwrap().to_string(),
            commands: v["commands"]
                .as_array()
                .map(|a| a.iter().filter_map(|c| c.as_str().map(str::to_string)).collect())
                .unwrap_or_default(),
        })
        .collect();
    Some(vectors)
}

/// A rig with loop 0 in the vector's `from` state under its facts and
/// config, or the reason it cannot be built.
fn rig_in(v: &Vector) -> Result<Shared, String> {
    let from = v.from.as_str();
    let armed = matches!(from, "armed-by-level" | "armed-by-sound" | "armed-for-grid");
    let has_layers = v.fact("has-layers").unwrap_or(matches!(
        from,
        "tape" | "playing" | "overdubbing-open" | "overdubbing-one-pass" | "multiplying"
    ));
    let has_length = v.fact("has-length").unwrap_or(has_layers || matches!(from, "sized" | "recording-sized"));
    if has_layers && !has_length {
        return Err("a layer implies a length: the engine has no loop with layers and no length".into());
    }
    // A tape is its layer.
    let threaded = has_layers && (v.fact("threaded").unwrap_or(from == "tape"));

    let mut sh = rig(LEN);
    if v.config("fixed-rig").unwrap_or(false) {
        sh.fixed_frames = 500;
    }
    sh.out_frames.store(NOW, Ordering::Release);
    let quantised = v.fact("quantised").unwrap_or(false) || armed && from != "armed-by-level";
    if v.fact("has-grid").unwrap_or(false) || quantised {
        sh.link_bar_frames.store(BAR, Ordering::Relaxed);
        sh.link_bar_origin.store(0, Ordering::Relaxed);
    }
    if v.fact("input-held-elsewhere").unwrap_or(false) {
        sh.lp(7).enter(Phase::First, NOW as i64);
    }

    let lp = sh.lp(0);
    // Content.
    let layers = if v.fact("at-ceiling").unwrap_or(false) {
        sh.max_layers
    } else if has_layers {
        if v.fact("last-layer").unwrap_or(true) { 1 } else { 2 }
    } else {
        0
    };
    if threaded {
        thread_blank(&sh, 0, LOOP);
        for l in 1..layers {
            lay(&sh, 0, l, LOOP, 0.0);
        }
    } else if layers > 0 {
        one_layer_loop(&sh, 0, LOOP, 0.25);
        for l in 1..layers {
            lay(&sh, 0, l, LOOP, 0.25);
        }
        lp.enter(Phase::Playing, NOW as i64);
    } else if has_length {
        lp.loop_len.store(LOOP, Ordering::Release);
        lp.origin.store(NOW as i64, Ordering::Release);
    }
    // Modes and guards.
    if !v.fact("writable").unwrap_or(true) {
        lp.pendulum.store(true, Ordering::Relaxed);
    }
    if !v.fact("plain").unwrap_or(true) {
        lp.speed.store(0.5f64.to_bits(), Ordering::Relaxed);
    }
    if v.fact("level-arm").unwrap_or(false) || from == "armed-by-level" || from == "armed-by-sound" {
        lp.level_arm.store(true, Ordering::Relaxed);
    }
    if quantised {
        lp.quant.store(true, Ordering::Relaxed);
    }
    if v.fact("one-pass").unwrap_or(false) || from == "overdubbing-one-pass" {
        lp.next.plan_one_pass();
    }
    // The phase, by the road the daemon takes to it.
    let expect = |ack: String, word: &str| -> Result<(), String> {
        if ack.contains(word) {
            Ok(())
        } else {
            Err(format!("building `{}`: `r` answered {:?}", from, ack))
        }
    };
    match from {
        "empty" | "sized" | "tape" | "playing" => {}
        "armed-by-level" => expect(dispatch(&sh, SR, "0r"), "listening")?,
        "armed-by-sound" => {
            expect(dispatch(&sh, SR, "0r"), "listening")?;
            callbacks::crossed(&sh, 0, NOW as i64);
        }
        "armed-for-grid" => expect(dispatch(&sh, SR, "0r"), "starts on the grid")?,
        "recording-open" | "recording-sized" => {
            expect(dispatch(&sh, SR, "0r"), "recording")?;
            callbacks::stamp(&sh, 0, NOW, STAMP_FRAMES);
            // The input has written a take's worth, so `commit` has
            // something to close.
            lp.reached.store(LOOP, Ordering::Release);
        }
        "overdubbing-open" | "overdubbing-one-pass" => {
            expect(dispatch(&sh, SR, "0r"), "layer")?;
            callbacks::stamp(&sh, 0, NOW, STAMP_FRAMES);
        }
        "multiplying" => expect(dispatch(&sh, SR, "0x"), "multiplying")?,
        other => return Err(format!("no way to build `{}`", other)),
    }
    callbacks::stamp(&sh, 0, NOW, STAMP_FRAMES);
    let got = artifact_state(&sh, 0);
    if got != from {
        return Err(format!("building `{}` left the loop `{}`", from, got));
    }
    Ok(sh)
}

/// Deliver the event to loop 0 the way the daemon does, and settle the
/// callback. The ack, for a press; nothing for a runtime event.
fn deliver(sh: &Shared, v: &Vector) -> Option<String> {
    let lp = sh.lp(0);
    let now = sh.out_frames.load(Ordering::Acquire);
    let ack = match v.event.as_str() {
        "record" | "multiply" => {
            if v.from == "multiplying" {
                // `multiply_end` rounds to whole cycles and waits for the
                // boundary on this thread: put the clock on it.
                let from = lp.rec_from.load(Ordering::Acquire);
                let len = lp.loop_len.load(Ordering::Acquire);
                sh.out_frames.store((from + len as i64) as usize, Ordering::Release);
            }
            Some(dispatch(sh, SR, if v.event == "record" { "0r" } else { "0x" }))
        }
        "undo" => Some(dispatch(sh, SR, "0u")),
        "clear" => Some(dispatch(sh, SR, "0c")),
        "fix" => Some(dispatch(sh, SR, "0fix0.1")),
        "size" => Some(dispatch(sh, SR, "0len1")),
        "blank" => Some(dispatch(sh, SR, "0blank0.1")),
        "free" => Some(dispatch(sh, SR, "0z")),
        "level-off" => Some(dispatch(sh, SR, "0lev0")),
        "sound" => {
            if sh.armed_loop() == Some(0) {
                callbacks::crossed(sh, 0, now as i64);
            }
            None
        }
        "boundary" => {
            let due = lp.next.due_in(now as i64);
            if due >= 0 {
                let at = now + due as usize;
                sh.out_frames.store(at, Ordering::Release);
                callbacks::stamp(sh, 0, at, STAMP_FRAMES);
            }
            None
        }
        "closed" => {
            let at = lp.close_at.load(Ordering::Acquire);
            if at != i64::MIN
                && lp
                    .close_at
                    .compare_exchange(at, i64::MIN, Ordering::AcqRel, Ordering::Relaxed)
                    .is_ok()
            {
                sh.out_frames.store(at as usize, Ordering::Release);
                if matches!(lp.phase(), Phase::First | Phase::Overdub) {
                    commit(sh, 0, SR, 0);
                }
            }
            None
        }
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
/// state after, read through `artifact_state`, must be the vector's; a
/// refusal must carry the vector's tag, and a move or stay must not be a
/// refusal. Vectors the engine cannot be put in are skipped by name and
/// counted; a missing vectors file is reported and passes, so a checkout
/// without the sibling repository still builds and tests.
#[test]
fn the_engine_replays_the_artifact_s_table() {
    let path = vectors_path();
    let Some(vectors) = read_vectors(&path) else {
        println!(
            "conformance: no vectors at {} — set GLASSBOX_DIR to the purescript-glassbox checkout; nothing replayed",
            path.display()
        );
        return;
    };
    let mut replayed = 0usize;
    let mut skipped: Vec<(String, String)> = Vec::new();
    let mut mismatches: Vec<String> = Vec::new();
    for v in &vectors {
        let sh = match rig_in(v) {
            Ok(sh) => sh,
            Err(why) => {
                skipped.push((why, v.describe()));
                continue;
            }
        };
        let ack = deliver(&sh, v);
        replayed += 1;
        let got = artifact_state(&sh, 0);
        let tag = ack.as_deref().and_then(refusal_tag);
        println!(
            "  {}  | engine: {} {} | commands owed: [{}]",
            v.describe(),
            got,
            match &ack {
                Some(a) => format!("ack {:?}", a),
                None => String::new(),
            },
            v.commands.join(", ")
        );
        if got != v.current {
            mismatches.push(format!(
                "{}: the engine is in `{}`{}",
                v.describe(),
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
        "conformance: replayed {} / skipped {} of {} vectors from {}",
        replayed,
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
        replayed * 10 >= vectors.len() * 9,
        "replayed {} of {} vectors, under nine in ten",
        replayed,
        vectors.len()
    );
}

/// The mapping reads what the byte cannot say: undoing the last layer and
/// dropping a sized first take both leave `Playing` with a length and no
/// layers, and both are `sized`.
#[test]
fn the_mapping_reads_sized_from_a_playing_byte() {
    let sh = rig(LEN);
    one_layer_loop(&sh, 0, LOOP, 0.25);
    sh.lp(0).enter(Phase::Playing, 0);
    assert_eq!(artifact_state(&sh, 0), "playing");
    assert!(dispatch(&sh, SR, "0u").contains("Empty now"));
    assert_eq!(sh.lp(0).phase(), Phase::Playing);
    assert_eq!(artifact_state(&sh, 0), "sized");
    assert!(dispatch(&sh, SR, "0z").contains("length forgotten"));
    assert_eq!(artifact_state(&sh, 0), "empty");
    assert!(dispatch(&sh, SR, "0blank0.1").contains("tape"));
    assert_eq!(artifact_state(&sh, 0), "tape");
}

/// **`t` waits like everything else.** The artifact has no event for the
/// claim, so no vector reaches it; the guard is held here instead: a
/// listening, waiting or recording loop refuses it with `still-recording`.
#[test]
fn claiming_the_past_waits_for_the_take_in_hand() {
    let sh = rig(LEN);
    sh.k_set.store(true, Ordering::Release);
    dispatch(&sh, SR, "0lev1");
    assert!(dispatch(&sh, SR, "0r").contains("listening"));
    let ack = dispatch(&sh, SR, "0t0.1");
    assert_eq!(refusal_tag(&ack), Some("still-recording"), "{}", ack);
    assert_eq!(sh.lp(0).phase(), Phase::Armed, "the arm stands");
    assert!(dispatch(&sh, SR, "0r").contains("stopped listening"), "give the input back");
    // Waiting for the bar.
    sh.link_bar_frames.store(BAR, Ordering::Relaxed);
    sh.out_frames.store(NOW, Ordering::Release);
    dispatch(&sh, SR, "1g1");
    assert!(dispatch(&sh, SR, "1r").contains("starts on the grid"));
    let ack = dispatch(&sh, SR, "1t0.1");
    assert!(ack.contains("waiting for the bar"), "{}", ack);
    assert!(sh.lp(1).next.waits_for_boundary(), "the request stands");
    // Mid-take.
    sh.lp(2).enter(Phase::First, 0);
    assert_eq!(refusal_tag(&dispatch(&sh, SR, "2t0.1")), Some("still-recording"));
    assert_eq!(sh.lp(2).phase(), Phase::First);
}
