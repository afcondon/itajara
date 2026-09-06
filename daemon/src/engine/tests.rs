use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU32, AtomicU64, AtomicUsize};
use std::sync::Mutex;

use rand::rngs::SmallRng;
use rand::{Rng, SeedableRng};

use super::*;
use super::cycle::tempo_of;

const LEN: usize = 1000;

/// A `Shared` small enough to build in a test, so the renderer can be
/// asked what it actually produces.
///
/// **Duplicated from the literal in `start`, on purpose.** The alternative
/// was a constructor taking thirty arguments so that one caller could pass
/// zeros. Adding a field to `Shared` breaks this at compile time, which is
/// the only kind of drift worth defending against here.
fn rig(max_frames: usize) -> Shared {
    Shared {
        arena: (0..DEFAULT_LOOPS * DEFAULT_LAYERS * max_frames * CHANNELS)
            .map(|_| AtomicU32::new(0))
            .collect(),
        max_frames,
        n_loops: DEFAULT_LOOPS,
        max_layers: DEFAULT_LAYERS,
        fixed_frames: 0,
        ring: (0..CHANNELS).map(|_| AtomicU32::new(0)).collect(),
        ring_len: 1,
        in_peak: vec![AtomicU32::new(0)],
        sources: vec![Source::mono("test", 0)],
        loops: (0..DEFAULT_LOOPS).map(|_| Loop::new(DEFAULT_LAYERS)).collect(),
        selected: AtomicUsize::new(0),
        anchor: AtomicUsize::new(NO_ANCHOR),
        out_frames: AtomicUsize::new(0),
        in_frames: AtomicUsize::new(0),
        k: AtomicI64::new(0),
        k_set: AtomicBool::new(false),
        p0: Mutex::new(None),
        buffer_frames: AtomicU32::new(0),
        click: AtomicBool::new(false),
        preroll: AtomicUsize::new(0),
        arm_thresh: AtomicU32::new(0.01f32.to_bits()),
        arm_reach: AtomicUsize::new(0),
        max_fade: 0,
        monitor: AtomicBool::new(false),
        out_peak: AtomicU32::new(0),
        p0_needed: AtomicBool::new(false),
        p0_frame: AtomicUsize::new(0),
        device_lost: AtomicBool::new(false),
        reopens: AtomicUsize::new(0),
        takes_dir: std::env::temp_dir().join("itajara-test-takes"),
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
    }
}

/// Fill one layer with a constant, and declare its shape.
fn lay(sh: &Shared, li: usize, layer: usize, len: usize, v: f32) {
    for p in 0..len {
        for ch in 0..CHANNELS {
            sh.cell(li, layer, p, ch).store(v.to_bits(), Ordering::Relaxed);
        }
    }
    let lp = sh.lp(li);
    lp.l_len[layer].store(len, Ordering::Release);
    lp.l_period[layer].store(1, Ordering::Release);
    lp.l_phase[layer].store(0, Ordering::Release);
    lp.l_tail[layer].store(0, Ordering::Release);
    lp.l_gain[layer].store(1.0f32.to_bits(), Ordering::Release);
    lp.n_layers.store(layer + 1, Ordering::Release);
}

/// A loop of `len` holding one layer, at the origin, ready to render.
fn one_layer_loop(sh: &Shared, li: usize, len: usize, v: f32) {
    lay(sh, li, 0, len, v);
    let lp = sh.lp(li);
    lp.loop_len.store(len, Ordering::Release);
    lp.origin.store(0, Ordering::Relaxed);
}

/// **A rendered loop is one cycle, and the placement is inside it.**
///
/// The bar-on-the-third-of-four case, which is the one that made me reach
/// for an LCM that turns out not to exist: `layer_pos` slots by
/// `(pos / layer_len) % period`, so the four bars are already `loop_len`
/// and a cycle is the whole of it. If that ever stops being true this test
/// goes quiet in exactly the wrong way — silent thirds and a fourth that
/// sounds — so it asserts each quarter separately.
/// What the output callback does when an edit has settled, for tests
/// that have no callback: apply the held edit and restart at zero.
fn settle(sh: &Shared, li: usize) {
    let lp = sh.lp(li);
    assert!(lp.pend_set.load(Ordering::Acquire), "an edit was held");
    lp.win_in.store(lp.pend_in.load(Ordering::Relaxed), Ordering::Relaxed);
    lp.win_out.store(lp.pend_out.load(Ordering::Relaxed), Ordering::Relaxed);
    lp.rot.store(lp.pend_rot.load(Ordering::Relaxed), Ordering::Relaxed);
    lp.pend_set.store(false, Ordering::Release);
    lp.edit_restart.store(0, Ordering::Release);
}

/// **A window and a rotation move nothing.** The loop is a ramp, so every
/// rendered sample says which arena position it came from.
#[test]
fn a_window_and_a_rotation_change_where_a_pass_starts_and_ends() {
    let sh = rig(LEN);
    one_layer_loop(&sh, 0, 100, 0.0);
    for p in 0..100 {
        for ch in 0..CHANNELS {
            sh.write(0, 0, p, ch, p as f32);
        }
    }
    let first = |sh: &Shared| sh.render_loop(0).expect("renders")[0];
    assert_eq!(first(&sh), 0.0);
    assert!(dispatch(&sh, 48_000, "0rot10").contains("starts"));
    assert_eq!(first(&sh), 0.0, "held, not applied: the loop plays on as it was");
    assert!(sh.lp(0).edit_restart.load(Ordering::Relaxed) > 0, "an edit schedules a restart");
    settle(&sh, 0);
    assert_eq!(first(&sh), 10.0, "a rotation starts the pass later in the arena");
    assert!(dispatch(&sh, 48_000, "0in20").contains("windows"));
    assert!(dispatch(&sh, 48_000, "0out60").contains("windows"), "the second edit is against the first");
    settle(&sh, 0);
    let out = sh.render_loop(0).expect("renders");
    assert_eq!(out.len(), 40 * CHANNELS, "the render is the window");
    assert_eq!(out[0], 30.0, "start of the window, plus the rotation");
    assert_eq!(out[(40 - 1) * CHANNELS], 29.0, "and it wraps inside the window");
    assert!(dispatch(&sh, 48_000, "0x").contains("window"), "no multiplying a window; an overdub is allowed now");
    assert!(dispatch(&sh, 48_000, "0out201").contains("may reach"), "one loop of silence is the most");
    assert!(dispatch(&sh, 48_000, "0win").contains("whole loop"));
    settle(&sh, 0);
    assert!(dispatch(&sh, 48_000, "0rot-10").contains("starts"));
    settle(&sh, 0);
    assert_eq!(first(&sh), 0.0, "and back");
    assert!(dispatch(&sh, 48_000, "0pk16").contains("16 buckets"));
    assert!(sh.peaks.lock().unwrap().contains(r#""buckets":16"#));
    // Extension: silence before and after, the content where it was.
    assert!(dispatch(&sh, 48_000, "0in-20").contains("with silence"));
    assert!(dispatch(&sh, 48_000, "0out120").contains("with silence"));
    settle(&sh, 0);
    let out = sh.render_loop(0).expect("renders");
    assert_eq!(out.len(), 140 * CHANNELS, "twenty of rest, the loop, twenty of rest");
    assert_eq!(out[0], 0.0, "rest before the loop");
    assert_eq!(out[21 * CHANNELS], 1.0, "the loop, where it was");
    assert_eq!(out[125 * CHANNELS], 0.0, "rest after it");
    assert!(dispatch(&sh, 48_000, "0in-200").contains("may reach"));
    assert!(dispatch(&sh, 48_000, "0pk16").contains("16 buckets"));
    assert!(sh.peaks.lock().unwrap().contains(r#""from":-20,"to":120"#));
}

/// **`exl` is a take per loop and one manifest for the set.** The
/// folders are what `w` writes, so each reloads as a plain take; the
/// set manifest is version 2 and carries the edit rather than applying it.
#[test]
fn exporting_layers_writes_a_take_per_loop_and_one_set_manifest() {
    let mut sh = rig(LEN);
    sh.takes_dir = std::env::temp_dir().join(format!("itajara-exl-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&sh.takes_dir);
    // Loop 0: two layers. Loop 2: one, windowed. Loop 1: nothing.
    one_layer_loop(&sh, 0, 100, 0.25);
    lay(&sh, 0, 1, 100, 0.5);
    sh.lp(0).n_layers.store(2, Ordering::Release);
    one_layer_loop(&sh, 2, 80, 0.75);
    sh.lp(2).n_layers.store(1, Ordering::Release);
    assert!(dispatch(&sh, 48_000, "2in10").contains("windows"));
    settle(&sh, 2);

    let ack = dispatch(&sh, 48_000, "exlriff");
    assert!(ack.contains("layers of 2 loops"), "ack was: {}", ack);
    assert!(ack.contains("loop-1, loop-3"), "ack was: {}", ack);
    let dir = sh.takes_dir.join("riff");
    for f in ["loop-1/layer-00.wav", "loop-1/layer-01.wav", "loop-1/take.json",
              "loop-3/layer-00.wav", "loop-3/take.json", "export.json"] {
        assert!(dir.join(f).exists(), "missing {}", f);
    }
    assert!(!dir.join("loop-2").exists(), "an empty loop gets no folder");
    let m = std::fs::read_to_string(dir.join("export.json")).unwrap();
    assert!(m.contains("\"version\": 2"), "{}", m);
    assert!(m.contains("\"kind\": \"layers\""), "{}", m);
    assert!(m.contains(r#""loop":3"#), "{}", m);
    assert!(m.contains(r#""window":{"in":10,"out":80}"#), "the edit is recorded: {}", m);
    assert!(m.contains(r#""window":null"#), "and its absence: {}", m);
    assert!(m.contains(r#""source":"test""#), "{}", m);
    assert!(m.contains(r#""gain":1.00000"#), "{}", m);
    // The whole layer went to disk, not the window: 80 frames of stereo.
    let wav = std::fs::read(dir.join("loop-3/layer-00.wav")).unwrap();
    assert!(wav.len() > 80 * CHANNELS * 4, "the layer was cropped to the window");
    // And `ex` still means the set: this must not have been eaten by `exl`.
    assert!(dispatch(&sh, 48_000, "exriff2").contains("exported 2 loops"));
    let _ = std::fs::remove_dir_all(&sh.takes_dir);
}

/// **The write head is the play head.** Through a window, a rotation,
/// both, and a window that reaches into silence: for every frame, where
/// an overdub would write is where the ear was — or nowhere.
#[test]
fn the_write_head_lands_where_the_play_head_is() {
    let sh = rig(LEN);
    one_layer_loop(&sh, 0, 100, 0.0);
    let lp = sh.lp(0);
    let check = |what: &str| {
        for out_frame in 0..400i64 {
            let heard = lp.play_pos(out_frame, 100);
            let wrote = lp.write_pos(out_frame - lp.origin.load(Ordering::Relaxed), 100);
            if heard >= 0.0 && heard < 100.0 {
                assert_eq!(wrote, Some(heard.floor() as usize), "{}: frame {}", what, out_frame);
            } else {
                assert_eq!(wrote, None, "{}: frame {} heard silence at {}", what, out_frame, heard);
            }
        }
    };
    check("whole loop");
    assert!(dispatch(&sh, 48_000, "0rot30").contains("starts"));
    settle(&sh, 0);
    check("rotated");
    assert!(dispatch(&sh, 48_000, "0in20").contains("windows"));
    assert!(dispatch(&sh, 48_000, "0out60").contains("windows"));
    settle(&sh, 0);
    check("windowed and rotated");
    assert!(dispatch(&sh, 48_000, "0in-20").contains("with silence"));
    settle(&sh, 0);
    check("window into silence before the loop");
    // And the refusal now names what it still refuses.
    assert!(dispatch(&sh, 48_000, "0x").contains("multiplying"));
    assert!(!dispatch(&sh, 48_000, "0r").contains("window"), "an overdub goes in");
}

/// **A layer's own window plays that stretch, and the picture ignores it.**
#[test]
fn a_layer_window_plays_its_slice_and_the_picture_sees_the_arena() {
    let sh = rig(LEN);
    one_layer_loop(&sh, 0, 100, 0.0);
    for p in 0..100 {
        for ch in 0..CHANNELS {
            sh.write(0, 0, p, ch, p as f32);
        }
    }
    assert!(dispatch(&sh, 48_000, "0lw1:20:60").contains("plays 20..60"));
    assert_eq!(sh.mix_at(0, 1, 5, true)[0], 25.0, "the window's start plus the position");
    assert_eq!(sh.mix_at(0, 1, 45, true)[0], 25.0, "and it comes round inside the span");
    assert_eq!(sh.mix_at(0, 1, 5, false)[0], 5.0, "the picture reads the arena as stored");
    assert!(dispatch(&sh, 48_000, "0lw1:-10:30").contains("with silence"));
    assert_eq!(sh.mix_at(0, 1, 5, true)[0], 0.0, "silence where the window reaches before the layer");
    assert!(dispatch(&sh, 48_000, "0lw1").contains("whole again"));
    assert_eq!(sh.mix_at(0, 1, 5, true)[0], 5.0);
    // Duplicate: the same audio as a new layer, window and all.
    assert!(dispatch(&sh, 48_000, "0lw1:20:60").contains("plays"));
    assert!(dispatch(&sh, 48_000, "0dp1").contains("duplicated as layer 2"));
    assert_eq!(sh.lp(0).n_layers.load(Ordering::Acquire), 2);
    assert_eq!(sh.read(0, 1, 50, 0), 50.0);
    assert_eq!(sh.lp(0).layer_window(1), Some((20, 60)));
    assert!(dispatch(&sh, 48_000, "0lw2:40:80").contains("plays 40..80"));
    assert_eq!(sh.lp(0).layer_window(0), Some((20, 60)), "each layer keeps its own");
    assert!(dispatch(&sh, 48_000, "0dp3").contains("not a layer 3"));
    // The copy to another loop carries the layers' windows.
    assert!(dispatch(&sh, 48_000, "1cp0").contains("copied 2 layers"));
    assert_eq!(sh.lp(1).layer_window(1), Some((40, 80)));
}

/// **A copy lands whole, in phase, and only on an empty loop.**
/// **A fixed next take.** `fix` leaves a length and no layers, which is
/// the state the arm branch already turns into a self-closing first take;
/// on a loop with material it arms a one-pass layer, starting on the press.
/// It refuses a length past the arena, and nonsense.
#[test]
fn fixing_the_next_take_sizes_an_empty_loop_and_nothing_else() {
    let sh = rig(LEN);
    // A 1 kHz "sample rate", so half a second fits the test arena.
    let ack = dispatch(&sh, 1000, "1fix0.5");
    assert!(ack.contains("closes itself"), "{}", ack);
    let lp = sh.lp(1);
    assert_eq!(lp.loop_len.load(Ordering::Acquire), 500);
    assert_eq!(lp.n_layers.load(Ordering::Acquire), 0, "sized, not threaded");
    assert!(!lp.threaded.load(Ordering::Relaxed));
    // On material it arms one pass and leaves the length alone.
    one_layer_loop(&sh, 0, 100, 0.25);
    let ack = dispatch(&sh, 1000, "0fix0.5");
    assert!(ack.contains("adds one layer of 0.100 s") && ack.contains("not 0.5 s"), "{}", ack);
    assert_eq!(sh.lp(0).loop_len.load(Ordering::Acquire), 100, "untouched");
    assert!(sh.lp(0).one_pass.load(Ordering::Relaxed));
    // The record that follows starts now, and says it will close itself.
    let ack = dispatch(&sh, 1000, "0r");
    assert!(ack.contains("one pass"), "{}", ack);
    assert_eq!(sh.lp(0).request_at.load(Ordering::Acquire), i64::MIN, "on the press, not at zero");
    // Not past the arena, and not nonsense.
    assert!(dispatch(&sh, 1000, "2fix2").contains("past --max-secs"));
    assert!(dispatch(&sh, 1000, "2fix").contains("wants a length"));
    assert!(dispatch(&sh, 1000, "2fix0").contains("wants a length"));
    assert_eq!(sh.lp(2).loop_len.load(Ordering::Acquire), 0);
    // `f` itself is an exact match and still its own verb.
    assert!(!dispatch(&sh, 1000, "2f").contains("wants a length"));
}

/// **A verb is a whole word, and the loop and the lateness still come off
/// the ends first.** The three commands the prefix guards misread, each
/// sent the way the board sends them — addressed, and stamped late — and
/// what is not a word answered as such.
#[test]
fn a_verb_is_read_whole_with_its_loop_and_its_lateness() {
    let sh = rig(LEN);
    // `tone3000` is a tone, not a claim of the past three thousand seconds.
    let ack = dispatch(&sh, 48_000, "3tone3000@250");
    assert!(ack.contains("loop 3") && ack.contains("3.0 kHz"), "ack was: {}", ack);
    // `t` is still the claim, and answers as one: nothing has arrived here.
    assert_eq!(dispatch(&sh, 48_000, "3t3000@250"), "no input has arrived yet.");
    // `sp0.5` is a speed, not a sparse multiply that could not read a count.
    assert!(dispatch(&sh, 48_000, "2sp0.5@10").contains("x0.5"));
    assert!(dispatch(&sh, 48_000, "2s4").contains("nothing to spread"));
    // `exl` and `ex` are read by the longest name-verb, not by their order;
    // the export test proves the files, this proves the reading.
    assert!(dispatch(&sh, 48_000, "exlriff").contains("nothing"), "exl on an empty rig");
    // Not a word is not a command, wherever it sits.
    assert_eq!(dispatch(&sh, 48_000, "1size13"), "unknown command \"size13\"");
    assert_eq!(dispatch(&sh, 48_000, "size13@100"), "unknown command \"size13\"");
    assert_eq!(dispatch(&sh, 48_000, "tx"), "unknown command \"tx\"");
    // A bare word stays bare, and a flag takes only a flag.
    assert_eq!(dispatch(&sh, 48_000, "0x1"), "unknown command \"x1\"");
    assert_eq!(dispatch(&sh, 48_000, "0g5"), "unknown command \"g5\"");
    assert_eq!(dispatch(&sh, 48_000, "play"), "unknown command \"play\"");
    // The lateness is judged before the word is read, as it always was.
    assert!(dispatch(&sh, 48_000, "3tone3000@x").contains("not a lateness"));
    // A bare loop number still selects, and an empty line still says nothing.
    assert_eq!(dispatch(&sh, 48_000, "3"), "loop 3 selected.");
    assert_eq!(dispatch(&sh, 48_000, ""), "");
    // An addressed flag sets rather than flips: `3k1` twice is on, on.
    assert_eq!(dispatch(&sh, 48_000, "3k1"), "click on.");
    assert_eq!(dispatch(&sh, 48_000, "3k1"), "click on.");
    assert_eq!(dispatch(&sh, 48_000, "k0"), "click off.");
}

#[test]
fn copying_lands_whole_and_in_phase_on_an_empty_loop_only() {
    let sh = rig(LEN);
    one_layer_loop(&sh, 0, 100, 0.25);
    lay(&sh, 0, 1, 100, 0.5);
    sh.lp(0).n_layers.store(2, Ordering::Release);
    sh.lp(0).origin.store(77, Ordering::Relaxed);
    assert!(dispatch(&sh, 48_000, "0in10").contains("windows"));
    settle(&sh, 0);

    let ack = dispatch(&sh, 48_000, "2cp0");
    assert!(ack.contains("copied 2 layers"), "{}", ack);
    let to = sh.lp(2);
    assert_eq!(to.n_layers.load(Ordering::Acquire), 2);
    assert_eq!(to.loop_len.load(Ordering::Acquire), 100);
    assert_eq!(to.origin.load(Ordering::Relaxed), 77, "in phase with the source");
    assert_eq!(sh.read(2, 1, 50, 0), 0.5, "the audio came across");
    assert!(to.window().is_none(), "the source's window stays its own");
    // Not onto a loop that holds something.
    assert!(dispatch(&sh, 48_000, "2cp0").contains("not empty"));
    // One layer, from one.
    assert!(dispatch(&sh, 48_000, "3cp0l2").contains("layer 2 of loop 0"));
    assert_eq!(sh.lp(3).n_layers.load(Ordering::Acquire), 1);
    assert_eq!(sh.read(3, 0, 50, 0), 0.5);
    assert!(dispatch(&sh, 48_000, "4cp0l3").contains("not 3"));
    // Nothing from nothing, and not onto itself.
    assert!(dispatch(&sh, 48_000, "4cp5").contains("nothing to copy"));
    assert!(dispatch(&sh, 48_000, "0cp0").contains("itself"));
    // `c` is still clear: the two-character arm did not eat it.
    assert!(!dispatch(&sh, 48_000, "3c").contains("wants a source"));
}

/// **Off is a switch, not a gain.** The layer stays whole and comes
/// back with one verb; while it is off the render, which is the mix, has
/// nothing from it.
#[test]
fn a_layer_switched_off_leaves_the_mix_and_comes_back_whole() {
    let sh = rig(LEN);
    one_layer_loop(&sh, 0, 100, 0.5);
    let lp = sh.lp(0);
    assert_eq!(sh.render_loop(0).expect("renders")[0], 0.5);
    assert!(dispatch(&sh, 48_000, "0ly10").contains("off"));
    assert!(!lp.layer_on(0));
    assert_eq!(sh.render_loop(0).expect("still renders")[0], 0.0, "parked, not gone");
    assert!(dispatch(&sh, 48_000, "0ly11").contains("on"));
    assert_eq!(sh.render_loop(0).expect("renders")[0], 0.5, "back whole");
    assert!(dispatch(&sh, 48_000, "0ly20").contains("not a layer"));
    assert!(dispatch(&sh, 48_000, "0ly1").contains("wants a layer number"));
}

#[test]
fn a_sparse_layer_renders_where_it_lands() {
    let sh = rig(LEN);
    let bar = 100;
    one_layer_loop(&sh, 0, bar, 0.5);
    let lp = sh.lp(0);
    lp.loop_len.store(4 * bar, Ordering::Release);
    lp.l_period[0].store(4, Ordering::Release);
    lp.l_phase[0].store(2, Ordering::Release); // the third of four
    let out = sh.render_loop(0).expect("renders");
    assert_eq!(out.len(), 4 * bar * CHANNELS, "one cycle, not more");
    let quarter = |q: usize| out[q * bar * CHANNELS];
    assert_eq!(quarter(0), 0.0);
    assert_eq!(quarter(1), 0.0);
    assert_eq!(quarter(2), 0.5, "the third quarter is where it was placed");
    assert_eq!(quarter(3), 0.0);
}

/// **Chance, one-shot and mute are not baked in.**
///
/// The rule the export rests on: those three decide whether you hear a loop
/// this time round, and every receiver these files go to can decide that
/// for itself. A render that honoured them would hand Ableton one roll of
/// the dice and call it the loop — and worse, a muted loop would export as
/// a folder of silence with nothing to say why.
#[test]
fn the_render_ignores_what_only_decides_whether_you_hear_it() {
    let sh = rig(LEN);
    one_layer_loop(&sh, 0, 100, 0.5);
    let lp = sh.lp(0);
    lp.chance.store(0.0f32.to_bits(), Ordering::Relaxed);
    lp.one_shot.store(true, Ordering::Relaxed);
    lp.muted.store(true, Ordering::Relaxed);
    let out = sh.render_loop(0).expect("renders anyway");
    assert!(out.iter().any(|&v| v != 0.0), "silence would be the bug");

    // And the live path still honours all three, which is the other half of
    // the claim: this is a second mode, not a change of behaviour.
    let mut rng = SmallRng::seed_from_u64(1);
    assert_eq!(sh.loop_at(0, 0, &mut rng, true), [0.0; CHANNELS]);
}

/// Speed is audio, so it *is* rendered — and it changes the file's length.
#[test]
fn half_speed_renders_twice_the_file() {
    let sh = rig(LEN);
    one_layer_loop(&sh, 0, 100, 0.5);
    sh.lp(0).speed.store(0.5f64.to_bits(), Ordering::Relaxed);
    let out = sh.render_loop(0).expect("renders");
    assert_eq!(out.len(), 200 * CHANNELS);
}

/// **The span write puts one input frame's worth into the loop, whatever
/// the rate.**
///
/// The law the overdub-at-speed branch rests on, checked as arithmetic
/// rather than through the audio callback — which needs a device. For one
/// input frame the head covers `[a, b)`, and the weights it hands out are
/// each slot's share of that interval. Two properties matter and both are
/// here: the weights sum to the span, so half speed averages its two frames
/// into one slot instead of doubling them; and at unity there is exactly one
/// slot at weight one, so the fast path and the moving path agree at the
/// only rate where both run.
fn spans(a: f64, b: f64) -> Vec<(i64, f32)> {
    let (lo, hi) = if a <= b { (a, b) } else { (b, a) };
    let mut out = Vec::new();
    let mut slot = lo.floor() as i64;
    while (slot as f64) < hi {
        let cover =
            (((slot + 1) as f64).min(hi) - (slot as f64).max(lo)).max(0.0) as f32;
        if cover > 0.0 {
            out.push((slot, cover));
        }
        slot += 1;
    }
    out
}

#[test]
fn one_input_frame_lands_once_however_fast_the_head_is_moving() {
    // Unity: one slot, full weight. The same answer the linear branch gives,
    // which is why that branch can stay and be trusted.
    assert_eq!(spans(10.0, 11.0), vec![(10, 1.0)]);

    // Backwards at unity: one slot, full weight, walking down. No
    // resampling at all — this is the case that is exact.
    assert_eq!(spans(11.0, 10.0), vec![(10, 1.0)]);

    // Half speed: two consecutive input frames share a slot at half each,
    // which is their average and not their sum. Getting this wrong is a
    // loop that comes back 6 dB hot and only when it is slowed down.
    let first = spans(10.0, 10.5);
    let second = spans(10.5, 11.0);
    assert_eq!(first, vec![(10, 0.5)]);
    assert_eq!(second, vec![(10, 0.5)]);
    let total: f32 = first.iter().chain(second.iter()).map(|(_, w)| w).sum();
    assert!((total - 1.0).abs() < 1e-6, "two frames, one slot's worth");

    // Double speed: one frame fills two slots outright — a zero-order hold,
    // which is the honest thing to do with samples that were never taken.
    assert_eq!(spans(10.0, 12.0), vec![(10, 1.0), (11, 1.0)]);

    // A stopped loop writes nowhere. There is no position for it to go to,
    // and picking one would smear a note into a single slot for as long as
    // a foot stayed down.
    assert!(spans(10.0, 10.0).is_empty());

    // Every weight is a share of a slot, so none can exceed one — the
    // property that keeps a rate the arena has never seen from writing
    // something louder than was played.
    for (num, den) in [(1, 3), (2, 3), (7, 4), (13, 5)] {
        let step = num as f64 / den as f64;
        for (_, w) in spans(3.25, 3.25 + step) {
            assert!(w <= 1.0 + 1e-6, "weight {} over span {}", w, step);
        }
    }
}

/// An empty loop is skipped rather than exported as silence.
#[test]
fn nothing_recorded_renders_to_nothing() {
    let sh = rig(LEN);
    assert!(sh.render_loop(0).is_none());
}

/// A loop with its position zero at output frame zero.
fn at_origin() -> Loop {
    let lp = Loop::new(DEFAULT_LAYERS);
    lp.origin.store(0, Ordering::Relaxed);
    lp
}

/// **A cleared loop must not remember how long it was.**
///
/// The failure this pins was invisible from inside the engine: a slot with
/// `loop_len == 0` and `cycles == 4` behaves correctly at every call site —
/// `loop_grid` checks the length first and bails — so nothing here went
/// wrong. What went wrong was on the surface. The Twister draws the bars
/// ring from `cycles` and writes ring positions *back* to the device, so
/// the encoder physically sat at four bars on a loop that had none, and
/// turning it to four moved nothing and sent nothing. The next take
/// recorded open-ended, and it did so only on the second run of a recipe.
///
/// So the assertion is not about behaviour, it is about **agreement**: two
/// fields describe one fact and they have to be cleared together. That is
/// the class of bug this project keeps finding — see `sized-but-empty`, the
/// same pair read the other way round.
/// The whole of what `bpm` computes, and the case it was asked for.
#[test]
fn a_loop_that_ran_long_gives_back_a_slower_tempo() {
    let sr = 48_000;
    // Four bars at 120 in four: 2 s a bar, 8 s the loop.
    assert!((tempo_of(8 * sr as usize, 4, sr, 4.0) - 120.0).abs() < 1e-9);

    // The case this exists for. You played four bars against a 120 click
    // and took 8.15 s over them; the click comes to you rather than the
    // audio being stretched to it.
    let long = (8.15 * sr as f64) as usize;
    let bpm = tempo_of(long, 4, sr, 4.0);
    assert!(bpm < 120.0, "running long must give a slower tempo, got {}", bpm);
    assert!((bpm - 117.79).abs() < 0.01, "got {}", bpm);

    // Metre comes from Link, so the same audio in three is a faster tempo —
    // three beats to fill the same bar. A hard-coded four would be right in
    // 4/4 and quietly wrong everywhere else.
    assert!((tempo_of(8 * sr as usize, 4, sr, 3.0) - 90.0).abs() < 1e-9);
    // A quantum nobody has sent reads as four rather than as none.
    assert_eq!(tempo_of(8 * sr as usize, 4, sr, 0.0), tempo_of(8 * sr as usize, 4, sr, 4.0));

    // Bars, not cycles: the same audio called one bar is a quarter of the
    // tempo of the same audio called four.
    assert!(
        (tempo_of(8 * sr as usize, 1, sr, 4.0) * 4.0 - tempo_of(8 * sr as usize, 4, sr, 4.0))
            .abs()
            < 1e-9
    );
}

/// **Balance is not pan, and the difference is what a centred loop sounds
/// like.**
///
/// The knob was equal-power throughout, which is right for placing one
/// signal and wrong for two that are already in a field: at centre it takes
/// 3 dB off both sides for nothing, and turning it collapses a width that
/// was recorded rather than inventing one.
#[test]
fn a_stereo_loop_is_balanced_and_a_folded_one_is_panned() {
    let lp = Loop::new(DEFAULT_LAYERS);

    // Centre. A balance leaves both sides alone; a pan is 3 dB down on each
    // because it is spending the difference on placing a mono signal.
    lp.pan.store(64, Ordering::Relaxed);
    let (bl, br) = lp.balance_gains();
    // **Exactly**, not nearly. See `pan_position`: dividing the whole
    // travel by 127 put centre at 0.5039 and left every centred loop 0.07 dB
    // down on one side, which export would now write into the file.
    assert_eq!((bl, br), (1.0, 1.0));
    let (pl, pr) = lp.pan_gains();
    assert!((pl - 0.707).abs() < 0.02 && (pr - 0.707).abs() < 0.02, "{} {}", pl, pr);

    // Hard over: silence on the far side, unity on the near one.
    lp.pan.store(0, Ordering::Relaxed);
    let (bl, br) = lp.balance_gains();
    assert!((bl - 1.0).abs() < 1e-6 && br.abs() < 1e-6);
    lp.pan.store(127, Ordering::Relaxed);
    let (bl, br) = lp.balance_gains();
    assert!(bl.abs() < 1e-6 && (br - 1.0).abs() < 1e-6);

    // **Attenuating only, at every position.** A balance that boosted would
    // make a loop louder than it was recorded and there is no headroom to
    // spend on that.
    for v in 0..=127u8 {
        lp.pan.store(v as usize, Ordering::Relaxed);
        let (l, r) = lp.balance_gains();
        assert!(l <= 1.0 + 1e-6 && r <= 1.0 + 1e-6, "at {}: {} {}", v, l, r);
        assert!(l >= 0.0 && r >= 0.0);
    }
}

/// A mono jack is a source whose two channels are the same input, and
/// nothing downstream needs a special case for it.
#[test]
fn a_one_channel_source_reads_the_same_input_twice() {
    let s = Source::mono("di", 2);
    assert_eq!(s.ch, [2, 2]);
    assert!(s.is_mono());
    assert_eq!(s.describe(), "di (in 3)");

    let board = Source { name: "board".into(), ch: [0, 1] };
    assert!(!board.is_mono());
    assert_eq!(board.describe(), "board (in 1+2)");
}

#[test]
fn clearing_forgets_the_length_and_the_bar_count_together() {
    let lp = Loop::new(DEFAULT_LAYERS);
    // Sized and empty, as `len<n>` leaves it: four bars of a two-second bar.
    lp.loop_len.store(4 * 96_000, Ordering::Release);
    lp.cycles.store(4, Ordering::Release);
    lp.rec_len.store(4 * 96_000, Ordering::Release);
    lp.close_at.store(1_234_567, Ordering::Release);

    lp.cleared();

    assert_eq!(lp.loop_len.load(Ordering::Acquire), 0, "kept its length");
    assert_eq!(
        lp.cycles.load(Ordering::Acquire),
        0,
        "kept its bar count, so the encoder still reads four bars on an \
         empty loop and cannot be turned to four"
    );
    assert_eq!(lp.rec_len.load(Ordering::Acquire), 0, "kept an asked-for length");
    assert_eq!(
        lp.close_at.load(Ordering::Acquire),
        i64::MIN,
        "kept a timer pointed at a take nobody has played"
    );
    // And is indistinguishable from one that was never touched, on every
    // field that describes a length.
    let fresh = Loop::new(DEFAULT_LAYERS);
    assert_eq!(
        lp.loop_len.load(Ordering::Acquire),
        fresh.loop_len.load(Ordering::Acquire)
    );
    assert_eq!(
        lp.cycles.load(Ordering::Acquire),
        fresh.cycles.load(Ordering::Acquire)
    );
}

#[test]
fn rate_one_is_the_subtraction_it_always_was() {
    let lp = at_origin();
    // Exactly integral, which is what lets the mix skip interpolation and
    // read one sample per layer in the ordinary case.
    for f in [0i64, 1, 999, 1000, 1001, 48_000_000] {
        let p = lp.play_pos(f, LEN);
        assert_eq!(p, p.floor(), "frame {} landed between samples", f);
        assert_eq!(p as i64, f.rem_euclid(LEN as i64));
    }
    assert!(lp.plain());
}

#[test]
fn half_speed_travels_half_as_far() {
    let lp = at_origin();
    lp.adopt(0, LEN, 0.5, false);
    assert_eq!(lp.play_pos(0, LEN), 0.0);
    assert_eq!(lp.play_pos(400, LEN), 200.0);
    // And wraps after two thousand output frames rather than one.
    assert_eq!(lp.play_pos(1999, LEN), 999.5);
    assert_eq!(lp.play_pos(2000, LEN), 0.0);
    // Recording into it is refused, because the grid is moving.
    assert!(!lp.plain());
}

#[test]
fn a_negative_rate_walks_backwards_and_reappears_at_the_far_end() {
    let lp = at_origin();
    lp.adopt(0, LEN, -1.0, false);
    assert_eq!(lp.play_pos(0, LEN), 0.0);
    assert_eq!(lp.play_pos(1, LEN), 999.0);
    assert_eq!(lp.play_pos(400, LEN), 600.0);
}

#[test]
fn a_pendulum_reflects_rather_than_wrapping() {
    let lp = at_origin();
    lp.adopt(0, LEN, 1.0, true);
    assert_eq!(lp.play_pos(250, LEN), 250.0);
    // Turns at the end of the audio, not at an arbitrary point...
    assert_eq!(lp.play_pos(1200, LEN), 800.0);
    // ...and so takes two lengths to come back to where it started.
    assert_eq!(lp.play_pos(2000, LEN), 0.0);
    // Never off the end, which a naive `2 * len - q` would be at the turn.
    for f in 0..4000i64 {
        let p = lp.play_pos(f, LEN);
        assert!(p >= 0.0 && p < LEN as f64, "frame {} gave {}", f, p);
    }
}

/// The property the whole `warp` field exists for.
#[test]
fn changing_speed_does_not_move_the_playhead() {
    for &(from, to) in &[
        (1.0, 0.5),
        (1.0, 2.0),
        (1.0, -1.0),
        (0.5, -2.0),
        (-1.5, 0.25),
        (2.0, 1.0),
    ] {
        for &at in &[1i64, 777, 123_456, 9_999_999] {
            let lp = at_origin();
            lp.adopt(0, LEN, from, false);
            let before = lp.play_pos(at, LEN);
            lp.adopt(at, LEN, to, false);
            let after = lp.play_pos(at, LEN);
            // Half a sample, and only when returning to rate one, where the
            // offset is folded into `origin` as a whole number of frames.
            assert!(
                (before - after).abs() <= 0.5,
                "x{} -> x{} at {} moved the playhead from {} to {}",
                from, to, at, before, after
            );
        }
    }
}

/// Coming back to rate one has to restore the exact arithmetic, or a loop
/// that had once been at a speed could never be recorded into again.
#[test]
fn returning_to_rate_one_makes_a_loop_recordable_again() {
    let lp = at_origin();
    lp.adopt(0, LEN, 0.5, false);
    lp.adopt(4321, LEN, 1.0, false);
    assert!(lp.plain(), "still carrying an offset after returning to x1");
    let p = lp.play_pos(9999, LEN);
    assert_eq!(p, p.floor());
}

#[test]
fn clearing_forgets_every_resolution() {
    let lp = at_origin();
    lp.adopt(0, LEN, -0.25, true);
    lp.plainly();
    assert!(lp.plain());
    assert_eq!(lp.speed(), 1.0);
    assert!(!lp.pendulum.load(Ordering::Relaxed));
}

/// A cleared slot has nobody's habits.
///
/// Written after `quant` was found surviving a clear on the running daemon
/// (2026-08-24) — every other mode reset and `grid` stayed lit, so a cleared
/// slot silently waited for the next bar before recording. The list is
/// exhaustive on purpose: the previous version of this test checked three
/// fields, and the field it did not check was the one that was wrong.
#[test]
fn a_cleared_slot_has_nobody_s_habits() {
    let lp = at_origin();

    // Turn on everything a player can turn on.
    lp.adopt(0, LEN, -0.5, true);
    lp.muted.store(true, Ordering::Relaxed);
    lp.pan.store(100, Ordering::Relaxed);
    lp.one_shot.store(true, Ordering::Relaxed);
    lp.level_arm.store(true, Ordering::Relaxed);
    lp.quant.store(true, Ordering::Relaxed);
    lp.fade.store(250, Ordering::Relaxed);
    lp.decay.store(0.5f32.to_bits(), Ordering::Relaxed);
    lp.chance.store(0.5f32.to_bits(), Ordering::Relaxed);
    lp.vol.store(0.001f32.to_bits(), Ordering::Relaxed);
    lp.n_layers.store(3, Ordering::Release);
    lp.loop_len.store(LEN, Ordering::Release);

    lp.cleared();

    assert_eq!(lp.speed(), 1.0, "speed");
    assert!(!lp.pendulum.load(Ordering::Relaxed), "pendulum");
    assert!(!lp.muted.load(Ordering::Relaxed), "muted");
    assert_eq!(lp.pan.load(Ordering::Relaxed), 64, "pan");
    assert!(!lp.one_shot.load(Ordering::Relaxed), "one shot");
    assert!(!lp.level_arm.load(Ordering::Relaxed), "level arm");
    assert!(!lp.quant.load(Ordering::Relaxed), "quantise");
    assert_eq!(lp.fade.load(Ordering::Relaxed), 0, "fade");
    assert_eq!(f32::from_bits(lp.decay.load(Ordering::Relaxed)), 1.0, "decay");
    assert_eq!(f32::from_bits(lp.chance.load(Ordering::Relaxed)), 1.0, "chance");
    assert_eq!(f32::from_bits(lp.vol.load(Ordering::Relaxed)), 1.0, "level");
    assert_eq!(lp.n_layers.load(Ordering::Acquire), 0, "layers");
    assert_eq!(lp.loop_len.load(Ordering::Acquire), 0, "length");
}

/// How long one pass lasts, which is the only number a one-shot needs and
/// the only place in the engine that has to know a cycle can be finite.
#[test]
fn a_pass_lasts_as_long_as_the_speed_makes_it() {
    let lp = at_origin();
    assert_eq!(lp.pass_frames(LEN), LEN as i64);
    lp.adopt(0, LEN, 0.5, false);
    assert_eq!(lp.pass_frames(LEN), 2 * LEN as i64, "half speed, twice as long");
    lp.adopt(0, LEN, 2.0, false);
    assert_eq!(lp.pass_frames(LEN), LEN as i64 / 2);
}

/// Direction is not duration. Backwards takes exactly as long as forwards,
/// which is easy to get wrong when direction lives in the sign of the number
/// being divided by.
#[test]
fn backwards_takes_just_as_long_and_a_pendulum_takes_twice() {
    let lp = at_origin();
    lp.adopt(0, LEN, -1.0, false);
    assert_eq!(lp.pass_frames(LEN), LEN as i64);
    lp.adopt(0, LEN, -0.5, true);
    assert_eq!(
        lp.pass_frames(LEN),
        4 * LEN as i64,
        "there and back at half speed"
    );
}

/// Which pass we are on, which is what chance rolls for. Worth stating as a
/// property because it has to keep step with `play_pos` through speed,
/// direction and the pendulum — the two come out of one expression exactly
/// so this cannot drift, and this is what says so.
#[test]
fn a_pass_is_one_trip_through_the_loop_however_long_that_takes() {
    let lp = at_origin();
    assert_eq!(lp.pass_index(0, LEN), 0);
    assert_eq!(lp.pass_index(LEN as i64 - 1, LEN), 0);
    assert_eq!(lp.pass_index(LEN as i64, LEN), 1);
    // Before `origin` is behind the loop's own beginning, and says so
    // rather than clamping to zero and claiming a pass that never ran.
    assert_eq!(lp.pass_index(-1, LEN), -1);

    // Half speed: a pass takes twice as many output frames.
    lp.adopt(0, LEN, 0.5, false);
    assert_eq!(lp.pass_index(2 * LEN as i64 - 1, LEN), 0);
    assert_eq!(lp.pass_index(2 * LEN as i64, LEN), 1);

    // A pendulum's pass is there and back, so a swinging loop set to one
    // cycle in four drops a whole round trip rather than half of one.
    let sw = at_origin();
    sw.adopt(0, LEN, 1.0, true);
    assert_eq!(sw.pass_index(2 * LEN as i64 - 1, LEN), 0);
    assert_eq!(sw.pass_index(2 * LEN as i64, LEN), 1);
}

/// The gate the mixer applies — `gen::<f32>() < p` — comes out at the rate
/// the label promises.
///
/// The generator itself is `rand`'s and needs no test from us; what is worth
/// asserting is that the *gate* opens as often as the rung says, because
/// every rung on the ladder except the first lives in the tail and a
/// comparison written the wrong way round would still look plausible.
#[test]
fn a_pass_sounds_as_often_as_the_rung_says() {
    let mut rng = SmallRng::seed_from_u64(0xDEAD_BEEF_CAFE_F00D);
    const N: usize = 40_000;
    for p in [1.0f32, 0.75, 0.5, 0.25, 0.125] {
        let hits = (0..N).filter(|_| rng.gen::<f32>() < p).count() as f64 / N as f64;
        assert!(
            (hits - p as f64).abs() < 0.01,
            "at {} the gate opened {:.4} of the time",
            p,
            hits
        );
    }
}

/// The whole point of keeping the tail: the loop point stops being a step in
/// the waveform.
///
/// A first recording is cut, so frame `len - 1` is followed at playback by
/// frame `0` — which is not what followed it when it was played. Here the
/// performance is a sine whose period does not divide the loop length, so
/// the naked splice is a large step; the fade should bring it down to
/// roughly what one sample of the signal moves anyway.
#[test]
fn the_wrap_stops_being_a_step_in_the_waveform() {
    const LEN: usize = 997;
    const N: usize = 64;
    // One continuous performance, sampled past the loop's end. `head` is what
    // was kept as the loop; `tail` is what carried on.
    let x = |i: usize| (i as f32 * 0.021_37).sin();
    let head = |i: usize| x(i);
    let tail = |j: usize| x(LEN + j);

    // How much the signal moves in one sample, at its steepest. Anything
    // near this is not a discontinuity, it is the waveform.
    let natural = (1..LEN).map(|i| (x(i) - x(i - 1)).abs()).fold(0.0f32, f32::max);

    let naked = (head(0) - head(LEN - 1)).abs();
    assert!(
        naked > natural * 20.0,
        "the test signal does not actually have a bad splice: {} vs {}",
        naked,
        natural
    );

    // Now walk the wrap with the fade on, and measure the biggest step
    // anywhere across it — including back out of the fade at `p = n`.
    let faded = |p: usize| wrap_mix(head(p), tail(p), p, N);
    let mut worst = (faded(0) - head(LEN - 1)).abs();
    for p in 1..N {
        worst = worst.max((faded(p) - faded(p - 1)).abs());
    }
    worst = worst.max((head(N) - faded(N - 1)).abs());
    assert!(
        worst < natural * 2.0,
        "the crossfaded wrap still steps by {} where the signal itself moves {}",
        worst,
        natural
    );
}

/// And that it arrives where it should at both ends, which is what makes the
/// continuity above hold rather than being an accident of one signal.
#[test]
fn a_wrap_fade_starts_on_the_continuation_and_ends_on_the_recording() {
    const N: usize = 100;
    // Head and tail held at opposite constants, so the blend is readable.
    assert!(wrap_mix(1.0, 0.0, 0, N) < 0.02, "does not start on the continuation");
    assert!(wrap_mix(1.0, 0.0, N - 1, N) > 0.98, "does not end on the recording");
    // Correlated material — the usual case, since the two ends are one
    // performance a cycle apart — keeps its level all the way through. This
    // is what linear buys and equal-power would not.
    for p in 0..N {
        assert!(
            (wrap_mix(0.7, 0.7, p, N) - 0.7).abs() < 1e-5,
            "the level moved at {}",
            p
        );
    }
}

/// Decay is per layer, counted from its own birth — which is the whole of
/// what makes it sound like tape rather than like a fader. New material
/// enters at full while everything underneath goes on receding.
#[test]
fn every_layer_recedes_from_its_own_beginning() {
    let lp = at_origin();
    lp.loop_len.store(LEN, Ordering::Release);
    // Six decibels a pass: a half each time round.
    lp.decay.store(10f32.powf(-6.0206 / 20.0).to_bits(), Ordering::Relaxed);
    // Layer 0 laid at the start; layer 1 laid three passes later.
    lp.set_layer_shape(0, Shape { len: LEN, tail: 0, born: 0 });
    lp.set_layer_shape(1, Shape { len: LEN, tail: 0, born: 3 });

    lp.age(3 * LEN as i64);
    assert!(
        (lp.layer_gain(0) - 0.125).abs() < 0.01,
        "three passes old should be an eighth, got {}",
        lp.layer_gain(0)
    );
    assert!(
        (lp.layer_gain(1) - 1.0).abs() < 0.01,
        "a layer laid this pass enters at full, got {}",
        lp.layer_gain(1)
    );

    // Three passes further on they have both lost the same amount, which is
    // what "from its own beginning" means.
    lp.age(6 * LEN as i64);
    assert!((lp.layer_gain(1) - 0.125).abs() < 0.01);
    assert!(lp.layer_gain(0) < lp.layer_gain(1));
}

/// And that turning it off brings everything back, because nothing was
/// scaled in the arena — the whole reason it is a resolution and not an edit.
#[test]
fn decay_is_a_resolution_and_undoes_by_being_turned_off() {
    let lp = at_origin();
    lp.loop_len.store(LEN, Ordering::Release);
    lp.set_layer_shape(0, Shape { len: LEN, tail: 0, born: 0 });
    lp.decay.store(0.5f32.to_bits(), Ordering::Relaxed);
    lp.age(8 * LEN as i64);
    assert!(lp.layer_gain(0) < 0.01, "should have faded away by now");
    lp.decay.store(1.0f32.to_bits(), Ordering::Relaxed);
    lp.age(8 * LEN as i64);
    assert_eq!(lp.layer_gain(0), 1.0, "turning decay off must bring it back");
}

/// The envelope's scale is absolute and logarithmic, which is the whole of
/// what makes the picture useful.
///
/// Per-layer normalisation is what a waveform editor does, and it would
/// destroy the one job this has: a quiet loop must not draw as tall as a
/// loud one. Linear against full scale would be honest and useless — a take
/// peaking at -20 dBFS is a tenth of the height and one at -40 is invisible.
#[test]
fn a_quieter_loop_draws_shorter_and_stays_visible() {
    assert_eq!(to_byte(0.0), 0, "silence is nothing");
    assert_eq!(to_byte(1.0), 255, "full scale is everything");
    // Twelve decibels down should be visibly shorter and still plainly
    // there. On a linear scale it would be a quarter; here it is four
    // fifths, which is what keeps forty decibels of range legible.
    let loud = to_byte(1.0) as i32;
    let quiet = to_byte(0.251) as i32; // -12 dBFS
    assert!(quiet < loud - 30, "-12 dB did not read as quieter: {}", quiet);
    assert!(quiet > loud / 2, "-12 dB fell off the picture: {}", quiet);
    // The floor is the floor, and below it there is nothing to draw.
    assert_eq!(to_byte(0.0001), 0, "-80 dBFS is under the floor");
    // Monotone, or two loudnesses could draw the same height.
    let mut last = 0u8;
    for i in 1..=100 {
        let b = to_byte(i as f32 / 100.0);
        assert!(b >= last, "not monotone at {}", i);
        last = b;
    }
}

/// A one-shot is silent until it is fired, and silent again after one pass.
/// The whole mode is this comparison, so it is worth stating as a property
/// rather than trusting to a mixer branch nobody reads twice.
#[test]
fn a_one_shot_sounds_only_inside_its_pass() {
    let lp = at_origin();
    lp.one_shot.store(true, Ordering::Relaxed);
    assert!(!lp.firing(0), "silent before it has ever been fired");
    // Fired at 500: audible for one pass and not a frame more.
    lp.shot_end.store(500 + lp.pass_frames(LEN), Ordering::Release);
    assert!(lp.firing(500));
    assert!(lp.firing(500 + LEN as i64 - 1));
    assert!(!lp.firing(500 + LEN as i64));
    // And a loop that is not a one-shot is never "firing", whatever is left
    // in `shot_end` from before the mode was switched off.
    lp.one_shot.store(false, Ordering::Relaxed);
    assert!(!lp.firing(500));
}
