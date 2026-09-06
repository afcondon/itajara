# The daemon after the Friends: a debt review

*2026-09-05. Prompted by Andrew's question, after two days of extending the
daemon for the Arbhar face: "do we have tech debt because it evolved to do
one thing and is now being extended to an almost entirely different
use-case? If this were PureScript I'd have concrete suggestions." This is
the attempt to have them anyway. Measurements by a read-only survey of
`daemon/src`; judgement mine; decisions Andrew's.*

## The short answer

Yes, and it is of a specific kind. The daemon's model is still "a pedal
looper with six loops on a grid", and the last month's features — Revox,
windows and rotation, layer windows, solo, copy and duplicate, fixed takes,
one-pass layers — were each added correctly *as patches to that model*
rather than by re-cutting it. The code records this itself: 120 bold
"**X, not Y**" comments, and the same correction ("layers, not length")
applied verbatim in two places. Nothing is wrong today. The cost is that
each new verb costs more than the last and the next silent bug is easier to
write than the last one was.

Three things are not debt and should stay: the atomics-and-callback design
(right for audio), the one-`dispatch` text protocol (right for a daemon many
surfaces speak to), and the daemon's ignorance of faces and use-cases (the
Unix shape; the Friends compose verbs, the daemon does not know an Arbhar
from a pedal). The question is whether the *primitives* are the right
generic ones, and the measurements say the primitives have shifted under
the code.

## The measurements

| | |
|---|---|
| `engine.rs` | 8 328 lines, 34 tests; 12 % of the file is tests |
| `struct Loop` | **61 fields**, all atomics or parallel `Vec`s; ~10 are documented shadows or qualifiers of another field |
| `struct Shared` | 44 fields |
| Per-layer data | **11 parallel `Vec`s** (`l_len`, `l_tail`, `l_born`, `l_gain`, `l_on`, `l_win_in`, `l_win_out`, `l_period`, `l_phase`, `env`, plus `n_layers`) |
| Phase | six `u8` constants; `state.set` at **17 sites in 9 functions, from 3 threads** |
| `fn dispatch` | **1 423 lines, 60 arms, 24 prefix guards**; three live bugs recorded in its own comments from arm order (`t`, `s`, `ex`) |
| `fn run` | 944 lines; the output callback (343) and input callback (313) are two flat closures inside it |
| `fn commit` | 335 lines |
| `snapshot` (ws.rs) | 277 lines of hand-rolled `format!`; the per-layer emitter is written **twice**, which is the exact cause of the 2026-08-23 freeze |
| Callers of `dispatch` | three threads (edit worker, per-command thread, console), no lock |

## Where the model shifted

Read as types, the way Andrew would read a PureScript module:

**Phase is a sum type, encoded as a byte plus qualifiers.** `IDLE | ARMED |
FIRST | OVERDUB | PLAYING | MULTIPLY` is the byte. But what a phase *means*
now depends on flags beside it: `threaded` (a PLAYING loop with one silent
layer that is really empty), `one_pass` (an OVERDUB that will close itself),
`close_at` and `rec_len` (a FIRST that knows its length), `request` and
`request_at` (a phase that is about to be something else), `level_arm` and
`arm_from` (an ARMED that is waiting for a sound rather than a frame). Each
pair is the "two fields, one fact" pattern. In PureScript this would be
`data Phase = Idle | Armed ArmedBy | Recording Take | Overdubbing Pass |
Playing | Multiplying …` with the qualifiers *inside* the constructors, and
half the bugs of the last month (the `n == 0` test that had to become
"layers, not length", twice; the closer that only knew FIRST) would have been
compile errors.

**"The next take" is a value that exists before any audio does.** Sized
loops (`len`, `fix`), threaded tapes (`blank`), level arming, one-pass, grid
boundaries: all are *plans* for a recording, and they live in seven fields
that must be set and cleared together. `Loop::cleared` clears some; the
callback clears others; `fix_next` sets two. This is the field cluster most
likely to produce the next stale-state bug.

**A layer has become a clip.** It has its own length, tail, birth, gain,
on/off, window, period and phase — and the Arbhar face treats layers as
alternates, not as parts of one sum. Eleven parallel `Vec`s indexed by
layer is a struct-of-arrays where an array-of-structs is now the honest
shape. The snapshot already emits a layer as one object with ten keys; the
engine does not have that object.

**Verbs are a grammar, parsed as prefixes.** `dispatch` matches `rest` by
`starts_with` in file order, so `size` would be eaten by `s`, `tone` was
eaten by `t`, `exl` had to go above `ex`. `tools/check-verbs.py` exists to
catch this, and its own comments say it cannot catch all of it. The grammar
is actually regular: `[loop digits] verb-letters [arg] [@late]`. A tokenizer
and a table keyed on the *whole* verb word would make arm order irrelevant
and make the checker's job trivial.

**Time is stamped in three places.** The command thread decides *what*, the
audio callback stamps *when*, and the closer thread decides *when to stop*.
That split is right — only the callback knows the frame — but the
transitions themselves are written out at 17 sites with no single function
saying which are legal. A `transition(lp, from, to, at)` that is the *only*
writer of `state` would give the state machine one home without moving any
timing.

**One type, two serialisers.** The snapshot's own comment names it: "two
serialisers for one type is the whole bug". The per-layer string is written
twice; the top-level duplicates ~18 of the selected loop's fields for a
legacy reader and calls it "deliberate and temporary". `check-snapshot.py`
guards it from outside; a single `fn layer_json(&Layer)` would guard it from
inside.

## If the daemon is to be a base

Andrew, on reading the line drawn above: agreed, "it's just that we
probably could make it a base for a LOT of useful things now that we have
it" — a software Morphagene as the Morphagene's Friend, say, so a harvest
can be auditioned before it goes to the modular.

That ambition sharpens the review rather than changing it. Every module
emulation is *playback shaping over clips*: a Morphagene reel is a loop, a
splice is a layer window, a gene is a sub-window read with grain overlap,
varispeed is `speed` with pitch (which exists), slide is rotation, morph is
the number of overlapping reads. Arbhar is the same vocabulary with
different names; Rample and the QD are the degenerate case (one-shot,
`one`). None of that is *recording*; all of it is per-layer *reading*. So
the primitive the emulations want is exactly step 3 below — a `Layer` that
is a clip with its own read parameters — plus a read path richer than
"position modulo length". The daemon's job stays what it is: hold the
audio, read it sample-accurately, say what it is doing. The face's job is
to name the knobs.

Two cautions. Granular reading is a different renderer from looping, and it
may belong in the browser (Web Audio over the exported file) rather than in
the callback — the test of a harvest is the *file*, and reading the file is
the more honest rehearsal. And an emulation is only as good as its
fidelity to the module's actual behaviour, which nobody but the module can
tell us; a Morphagene's Friend should be built against recordings of the
real one, the way DeepStar calibrates against the real VCO.

## What I would do, in order

Each is a mechanical refactor with the tests and the two checkers as the
net; none changes behaviour; each can land alone.

1. **Split `engine.rs` into modules.** `loop.rs` (Loop, Layer, Phase),
   `dispatch.rs`, `callbacks.rs` (output and input, as named functions with
   named sub-steps), `commit.rs`, `edit.rs` (windows, rotation, layer
   windows), `copy.rs`, `export.rs`, `tests/`. Zero semantic risk, and every
   later step becomes reviewable. Do this first or nothing else is.

   **Step 1 — done 2026-09-06**, branch `daemon/modules`. `engine.rs` is
   `engine/`: `mod.rs` (constants, `Opts`, `Source`, ack words, bar
   arithmetic, wiring), `loop_state.rs` (`Loop`), `shared.rs` (`Shared`),
   `run.rs` (residual, arena, streams, supervise), `callbacks.rs` (output
   and input, lifted whole out of `run`'s closures), `control.rs` (console,
   closer), `commit.rs` (commit, draw_layer, fill_from_ring, take),
   `cycle.rs` (multiply, sparse/place/rotate/dense, fix, bars, tempo,
   start_all, free), `guards.rs` (busy_elsewhere, not_writable, not_plain),
   `dispatch.rs` (dispatch alone, arms in their order; `check-verbs` reads
   it by path), `edit.rs` (thread_blank, schedule_restart), `copy.rs`,
   `export.rs`, `selftest.rs`, `tests.rs` (one module, as before). Nothing
   renamed or reordered; the only visibility change is `pub(crate)` where a
   field or function now crosses a file boundary. 43 tests, both checkers,
   zero warnings.
2. **Verb tokenizer and table.** Parse `loop / verb / arg / @late` once;
   match on the verb word exactly. Retire the prefix guards and the
   ordering comments. `check-verbs.py` then compares two tables, which is
   what it always wanted to do.

   **Step 2 — done 2026-09-06**, branch `daemon/verb-table`. The grammar
   is `[loop digits] word [arg] [@late]`; `dispatch` still takes the loop
   and the lateness off the ends exactly as before, and `verb::tokenize`
   (new `engine/verb.rs`) reads what is left as the leading run of ASCII
   letters (a `!` may open it, for `!lose`) plus the trimmed rest. If the
   word is in `VERBS` — a `const` slice of 54 `Verb { word, arg }` rows,
   `Arg` being `None | Flag | Number | Int | Text | Name` — that is the
   command; only if it is not, the longest `Name` verb (`exl`, `ex`, `w`)
   that begins the letters is, and the rest is the name, so `exlriff` is
   `exl` + `riff` with no order consulted. Anything else is `unknown
   command`, in the words the arm always used: `size13` is refused rather
   than multiplied, `tone3000` is a tone, `sp0.5` a speed. The kind also
   keeps a bare word bare — `x1`, `g5` stay unknown, as they were when
   `x` and `g` were exact arms. The `match` is exact on the word, every
   body reads `arg`, and the ordering comments are gone; the one arm that
   changed shape is `k`/`m`, which read `line.trim()` and so flipped
   instead of setting when addressed (`3k1`) — they read `arg` now.
   `check-verbs.py` no longer looks for shadows, since there is nothing to
   shadow with: it tokenizes every spelling `render` can produce by the
   same rule and checks it lands on itself bare and argued, and checks
   `VERBS` and the arms name the same words; `the_table_and_the_match_agree`
   in `verb.rs` holds that from inside. 48 tests, both checkers, zero
   warnings.
3. **`Layer` as a struct.** `Vec<Layer>` behind the existing lock
   discipline; one `layer_json`. The Arbhar face's whole vocabulary (`ly`,
   `lw`, `dp`, `cp…l`) is then operations on one type — and it is the
   foundation any module emulation stands on.

   **Step 3 — done 2026-09-06**, branch `daemon/layer-struct`. New
   `engine/layer.rs`: `pub(crate) struct Layer { len: AtomicUsize, tail:
   AtomicUsize, born: AtomicI64, env: Mutex<Vec<u8>>, gain: AtomicU32,
   on: AtomicBool, win_in: AtomicI64, win_out: AtomicI64, period:
   AtomicUsize, phase: AtomicUsize }` — each field the atomic its `Vec`
   held, with the doc comment that was on the array. `Loop.layers:
   Vec<Layer>`, `max_layers` of them from construction; `n_layers` and
   `redo_to` are what they were. Every `lp.l_xxx[k]` is `lp.layers[k].xxx`
   with the same orderings; `set_layer_shape`, `layer_pos`, `layer_window`,
   `windowed_pos`, `layer_shape`, `layer_gain`, `layer_on`, `layer_born`
   and `layer_tail` have their bodies on `Layer` and stay on `Loop` as
   delegations, so no caller outside the engine moved (`layer_env` went,
   since its one caller now reads the layer). The envelope mutex is **one
   per layer** rather than one over all of them: `rebuild_env` writes one
   layer, the snapshot copies one layer, and `clear_env` takes them in
   turn — nothing ever held two layers' pictures under one lock, and a
   snapshot could already see between two layers because it locked once
   per layer. `ws.rs` has one `layer_json(&Layer)`, called from both
   places the per-layer object is emitted, and a test holding its output
   to a literal captured from the old emitter. The proof is two tests
   committed *before* the refactor (de1c1a0): `fixture()` in
   `engine/tests.rs` — a continuation and wrap fade under a loop window
   with a rotation, a layer off, a layer window, decay with differing
   births, a sparse layer at a fractional speed folded and panned, and a
   threaded blank — rendered for 4096 frames through the callback's own
   `loop_at` plus each `render_loop`, FNV-1a over the sample bits
   (`1122269442957771175`), and the whole `ws::snapshot` text hashed the
   same way (`5753937055540615430`). Both constants unchanged after. 52
   tests, both checkers, zero warnings.
4. **`NextTake` as one value.** Length, close, one-pass, threaded, level-arm
   and boundary in one struct, written by the command thread, read once by
   the callback at the transition, cleared in one place. Swapped whole (a
   `Mutex` is fine; it is read once per buffer).

   **Step 4 — done 2026-09-06**, branch `daemon/next-take`. Not a
   `Mutex` after all: the same four atomics with the same orderings, so
   the callback stays lock-free and the rendering hash is a proof of
   sameness rather than an argument. The classification came from the
   code, by the rule *written before the take starts and consumed at the
   moment the callback turns the request into `FIRST` or `OVERDUB`*:

   | field | is |
   |---|---|
   | `request` | plan — the phase asked for |
   | `request_at` | plan — its frame, born and consumed with it |
   | `one_pass` | plan — set by `fix` on material, spent when the take is stamped |
   | `arm_from` | plan — the crossing's back-date, swapped out at the stamp |
   | `close_at` | running take — written at the transition, read by the closer |
   | `rec_len` | running take — written at the transition, taken by `commit` |
   | `started_late` | running take — written on `r`, spent by `commit`, overwritten by every `r` |
   | `reached`, `rec_reached`, `rec_from` | running take |
   | `level_arm`, `quant` | mode — survive every take |
   | `threaded` | content fact — "one layer that is an empty tape"; `blank` and `copy` read it as such |
   | `loop_len` on zero layers | content fact — the mixer reads it; the plan reads it at the stamp and does not own it |

   So the plan is smaller than the review guessed — four fields, not
   seven — and the length of a sized-and-empty loop is deliberately not
   in it: it is what the plan *reads*. New `engine/next_take.rs`:
   `pub(crate) struct NextTake { request: AtomicU8Wrapper, request_at:
   AtomicI64, one_pass: AtomicBool, arm_from: AtomicI64 }`, fields
   private, on `Loop` as `next`. Set: `set(phase, at)` from `r`, `f` and
   Start All; `set_from(at, from)` from the input callback's crossing;
   `listen()` for an `r` that waits for a sound; `plan_one_pass()` from
   `fix`. Taken: `take(before) -> Option<Taken>` in the output callback,
   which peeks until the request is due and then reads every field —
   a `FIRE` or `PLAYING` leaves `one_pass`/`arm_from` standing, since a
   Start All going past does not cancel a `fix`. Cleared: `clear()` from
   `Loop::cleared` (which used to clear `arm_from` and `one_pass` and
   leave a pending request behind), the `r` that cancels an `ARMED` loop
   and the `lev0` under one (both used to clear `arm_from` alone),
   `free_length` (which cleared nothing), and the device-loss path in
   `run` (which took `request` alone). Nothing resets a plan field
   individually any more; the resets of `threaded`, `close_at` and
   `rec_len` at the stamp and in `fix_next`/`copy` stay where they are,
   being facts about the take now running or the loop's content. Three
   stale cases closed as a consequence, each covered by a test: a
   cancelled arm dropping a crossing already written, `c` or `z`
   dropping a record waiting for the grid, and a one-pass surviving a
   first take to fire on the overdub after it. `AtomicU8Wrapper::take`
   went with its last caller. 55 tests, both hash constants unchanged,
   both checkers, zero warnings.
5. **`Phase` as an enum with one `transition` function.** The qualifiers
   move inside the constructors; the 17 sites become calls; the callback
   stays the only stamper of frames.

   **Step 5a — done 2026-09-06**, branch `daemon/phase`. The enum, the
   one writer and the table; the qualifiers stay beside the byte for now,
   and conformance against the Glassbox artifact is 5b. New
   `engine/phase.rs`: `pub(crate) enum Phase { Idle, Armed, First,
   Overdub, Playing, Multiply }`, `repr(u8)` with the constants' values,
   `from_u8`/`as_u8`, and `Display` giving the wire word (`idle`, `armed`,
   `recordingFirst`, `overdubbing`, `playing`, `multiplying`) — one
   spelling, which `state_name` and so the snapshot and `busy_elsewhere`
   now read from. The storage is the same `AtomicU8` on `Loop`, private
   now, read by `Loop::phase()` with the same Acquire load, and stored by
   **`Loop::enter(&self, to: Phase, at: i64)`** and nothing else: it reads
   the phase it is leaving, checks the pair against `phase::LEGAL`, and
   stores. An illegal pair in release is stored anyway and logged once per
   pair (loop, from, to, frame); under test it panics. `at` is passed, not
   read — the callback hands over its `stamp`, the control thread reads
   `out_frames` at the moment it acts (or the `cur`/`now` it had just
   read), and `cleared` takes the frame from its caller for the same
   reason. `ARMED`, `PLAYING` and `FIRE` remain as `u8` for the request
   byte `NextTake` carries, which is a superset of the phases; `IDLE`,
   `FIRST`, `OVERDUB`, `MULTIPLY` are gone.

   The seventeen `state.set` sites, before → after, all seventeen now
   `enter` calls on the same thread at the same moment: output callback
   ×3 (`FIRST` → `enter(First, stamp)`, `OVERDUB` → `enter(Overdub,
   stamp)`, the `PLAYING` request → `enter(Playing, stamp)`); `commit` ×2
   (`PLAYING` before the drain and again in the revox branch, each with a
   fresh `out_frames`); `take` ×1 (`PLAYING` → `enter(Playing, now)`);
   `multiply_start` (`MULTIPLY` → `enter(Multiply, cur)`); `multiply_end`
   ×2 (`PLAYING` at the ceiling refusal with `cur`, `PLAYING` after the
   boundary with a fresh read); `free_length` (`IDLE`); `copy_layers`
   (`PLAYING` on the destination); `thread_blank` (`PLAYING` → `enter(
   Playing, now)`); `dispatch` ×3 (`r` cancelling an arm → `Idle`, `r`
   under level arm → `Armed`, `lev0` under an arm → `Idle`); `Loop::
   cleared` (`IDLE`, frame from the `c` arm); `supervise` (`PLAYING` or
   `IDLE` on a lost device).

   The table, 22 rows, derived from the sites: Idle → {Idle, Armed,
   First, Playing, Multiply}; Armed → {Idle, First, Overdub, Playing,
   Multiply}; First → {Idle, Playing}; Overdub → {Idle, Playing};
   Playing → {Idle, Armed, First, Overdub, Playing, Multiply}; Multiply →
   {Idle, Playing}. Self-transitions: `Idle → Idle` (clearing an idle
   loop; `z` on a sized-and-empty loop) and `Playing → Playing`
   (`commit`'s second store, `cp` onto a threaded tape, re-threading a
   tape, the `PLAYING` request that nothing sends since Start All became
   `FIRE`) are legal; the other four are not produced and are not legal.
   Left out on purpose, and so logged if they ever happen: pairs only a
   thread race makes — `x` with a grid request still pending (`Multiply →
   Overdub`), a `c` inside `commit`'s or `multiply_end`'s sleep (`Idle →
   Playing` from their second store). Six rows are in the table only
   because a verb is not guarded against the phase it finds, each marked
   in the source: `x`, `z`, `blank` and `t` on an `Armed` loop (`Armed →
   Multiply`, `Armed → Idle`, `Armed → Playing`), `z` and `t` on a `First`
   one (`First → Idle`, `First → Playing`). Tests: 60 (55 + the three in
   `phase.rs` + the two in `tests.rs`), both hash constants unchanged, both
   checkers, zero warnings.

   **Artifact versus code, for 5b** (`itajara-loop.json`, 11 states,
   unchanged by this step). The artifact's states are the byte plus the
   facts, so its `sized` is `Idle` with a length *or* `Playing` with a
   length and no layers (after undo-all), its `tape` is `Playing` +
   `threaded`, and its `armed-for-grid` is `Idle` with a request pending
   — three artifact states the byte cannot tell apart, which is the whole
   of what 5b has to map. Code produces, artifact refuses: `Armed →
   Multiply` (`x` on a listening loop; the artifact's notes already name
   it), `Armed → Idle` by `z`, `Armed → Playing` by `blank` and by `t`,
   `First → Idle` by `z` mid-take, `First → Playing` by `t` mid-take
   (`t` has no state guard at all beyond the layer ceiling, and the
   artifact has no `take` event). Code differs: a cancelled level arm
   (`r` again, or `lev0`) always goes to `Idle`, where the artifact
   returns to `tape`/`playing`/`sized`/`empty` by the facts — a loop with
   layers that was armed from `Playing` comes back reading `idle` with
   its layers intact; and a lost device during a sized first take goes to
   `Playing` (length kept, no layers) where the artifact says `sized`.
   Artifact allows, code never stores: `undo` on the last layer (`playing
   → sized`; the code leaves the byte at `Playing`), and `lost` from
   `armed-by-level` (`stay`; the code stays too, but `next.clear()` drops
   the crossing already found). Matching: every `clear` to `Idle` (or
   `tape` on a fixed rig, which is `Idle` then `Playing`), `closed` on a
   sized take or one-pass → `Playing`, `multiply`/`record` ending a
   multiply → `Playing`, `lost` from an open first take → `Idle` and from
   an overdub or multiply → `Playing`, `sized` on `multiply` →
   `Multiply`.

   **Step 5b — done 2026-09-06**, branch `daemon/conformance`. The
   artifact's table, replayed through the engine. New
   `engine/conformance.rs` (test-only): `artifact_state(sh, li)` reads the
   byte, the plan and the content facts into one of the artifact's twelve
   states — the three the byte cannot separate are `sized` (`Idle` with a
   length, *or* `Playing` with a length and no layers), `armed-for-grid`
   (`Idle` or `Playing` with an `ARMED` request filed for a boundary) and
   `armed-by-sound` (`Armed` with one) — and `rig_in` builds loop 0 in a
   vector's `from` state under its facts and config, by the road the
   daemon takes to it (`r`, `x`, `blank` through `dispatch`; the crossing
   through `callbacks::crossed`; the stamp through `callbacks::stamp`,
   which is the callback's consumption of the plan lifted into a function
   so a test can run it at a chosen frame; `closed` as the closer does
   it; `lost` as `run::drop_takes`, lifted the same way). The vectors are
   read from `$GLASSBOX_DIR` or the sibling checkout; absent, the test
   says so and passes. Commands are printed, not asserted.

   **Replayed 339 / skipped 14 of 353.** The 14 all say `has-layers` and
   not `has-length`, which no loop can be — a layer implies a length —
   and are the only skip. Every other assignment is realised, including
   the two the engine cannot hold independently (`writable` false makes
   `plain` false; `plain` false on an empty loop makes it unwritable),
   which the guard order makes harmless and the doc comment records.

   Every mismatch the first run found, one decision each:

   | mismatch | decision | where |
   |---|---|---|
   | `x` on `Armed` started a multiply; artifact refuses `still-recording` | daemon: refuse | `dispatch` `x`, via `guards::still_recording` |
   | `u` on an armed, waiting or recording loop took a layer; artifact refuses | daemon: refuse | `dispatch` `u` |
   | `z` on `Armed`, `First`, or a grid wait forgot the length; artifact refuses | daemon: refuse (reverses step 4's "`z` drops a grid wait"; `c` still does) | `cycle::free_length` |
   | `blank`, `len` on `Armed` or a grid wait; `fix` on a grid wait; artifact refuses | daemon: refuse | `dispatch` `blank`, `cycle::set_bars`, `cycle::fix_next` |
   | `t` on `Armed`, `First`, `Overdub`, `Multiply` or a grid wait wrote the live layer (no artifact event) | daemon: refuse | `dispatch` `t` |
   | cancelled level arm (`r` again, `lev0`) went to `Idle` with its layers still summing; artifact returns to what the loop held | daemon: `Loop::disarm` — layers (a tape has one) → `Playing`, none → `Idle`, length kept | `loop_state.rs`, both sites |
   | lost device on a sized first take: code `Playing` + length + no layers, artifact `sized` | mapping: that *is* `sized`; no change | `artifact_state` |
   | undo of the last layer: code keeps `Playing`, artifact `sized` | mapping; no change | `artifact_state` |
   | `armed-by-level --sound--> armed-for-grid` under quantise: the engine keeps the byte `Armed`, still holds the input, `r` cancels it and `lost` drops the crossing and leaves it listening — none of which `armed-for-grid` does | artifact: twelfth state `armed-by-sound`; `armed-for-grid` is entered with no layers only, so its exits read `has-length` alone | `itajara-loop.json`, notes |
   | `len` on `playing`/`tape` with no bar: engine refuses `no-grid`, artifact `stay` | artifact: `not has-grid → no-grid` first, as on `empty` and `sized` | `itajara-loop.json` |
   | `lost` from a listening loop whose crossing was found: 5a's "the code drops the crossing" | artifact: `armed-by-sound --lost--> armed-by-level`, which is what the code does | `itajara-loop.json` |

   Refusals added (verb × state → tag, ack): `x`, `u`, `z`, `blank`,
   `len`, `t` on `Armed` → `still-recording`, "loop N is listening for a
   sound; finish that first."; the same six plus `fix` on a loop waiting
   for the bar → `still-recording`, "loop N is waiting for the bar; finish
   that first."; `u`, `z`, `t` on `First`/`Overdub`/`Multiply` →
   `still-recording`, "loop N is recording; finish that first." (the
   sentence `fix`, `blank` and `len` already used). `fix` on `Armed` now
   says "listening for a sound" where it said "is recording".

   `phase::LEGAL` is 21 rows: `Armed → Multiply` is gone, and `Armed →
   Playing` is now `disarm` on a loop with layers rather than `t`/`blank`
   unguarded. The site-pair test drives the cancel both ways and asserts
   the four verbs refuse and leave the phase standing. Tests: 63 (60 +
   the replay + the mapping + the `t` guard), both hash constants
   unchanged, both checkers, zero warnings. Glassbox side, on
   `glassbox-rs`: artifact amended (notes say why), 353 vectors (was 328),
   renders regenerated, `spago test` and `cargo test` pass.

   **What remains unmapped.** `input-held-elsewhere` is `wants_input`,
   which does not count a loop waiting for the bar: two loops can be
   filed for the same boundary and the second records nothing — the bug
   `busy_elsewhere` exists to refuse, one state short. `quantised`
   without a bar records now; the artifact's `record` rule reads
   `quantised` alone, so the replay supplies a bar whenever it is set.
   `multiplying` entered from `sized` and then lost comes back `sized`
   (no layers) where the artifact says `playing`; the vectors build
   `multiplying` from `playing`, so it is not reached. `len` on material
   that would shrink below a layer refuses with an argument the machine
   does not know. The pedalboard's PureScript half (`Socket.phaseOf`,
   `Machine.perform`) is not yet held to the same file; the snapshot's
   `state` still carries the byte's word, not the artifact's id.
6. **One serialiser per type, and drop the top-level duplication** once the
   pedalboard reads `loops[i]` (it already can).

   **Step 6 — done 2026-09-06**, branch `daemon/snapshot`. Three emitters
   in `ws.rs`, one per type on the wire, each called once per instance:
   `layer_json(&Layer) -> String` (step 3's, untouched), `loop_json(sh:
   &Shared, li: usize, sr: u32, cur: i64) -> String` for one entry of
   `loops`, and `rig_json(sh: &Shared, sr: u32, alive: bool) -> String`
   for the top level — rig-level fields and `loops` only. `snapshot` is
   `rig_json` by its old name, for `talk` and the tests. **Removed from the
   top level**, the nine fields that repeated the selected loop's: `state`,
   `layers`, `loopFrames`, `loopSecs`, `pos`, `phase`, `armed`,
   `recording`, `shapes`. Every retained field keeps its name, order and
   number formatting; `selected` stays, as the loop a console verb with no
   loop digit addresses (no surface reads it). Nothing per-loop or
   per-layer moved: `a_loop_is_written_as_it_always_was` holds `loop_json`
   to loop 2 of the fixture and to an empty slot, both captured from the
   old emitter at c6b52b2, beside `a_layer_is_written_as_it_always_was`;
   `the_rig_is_only_the_rig` holds the top level's key set, refuses the
   nine, and checks `loops` is `loop_json` once per loop in order. The
   snapshot hash moved, deliberately and in its own commit,
   `5753937055540615430` → `3623273475480213597`, because the top level
   shrank; the render hash `1122269442957771175` did not. Readers:
   `LooperState` in `client/src/Foreign/LooperSocket.purs` lost the nine
   (`phaseOf` stays row-polymorphic, its comment now history); the surface
   needed no change (Edit and Wave already read `loops[focus]`);
   `check-snapshot.py` lost its `top-level shapes[0]` location, three
   places now; the pedalboard's `Component.Looper.Page` — `transport`,
   `readout`, `nextPress`, `phaseBar` — takes the focused loop from `loops`
   where it read the top level (its hand-written stubs in `Twister.purs`
   and `test/Main.purs` are `LoopState` values and lost nothing); the
   Friends needed no source change and was rebundled. Against the running
   daemon, still on the old wire, `check-snapshot.py` prints OK with the
   nine listed as "sent but not declared". 65 tests, both checkers, zero
   warnings. Deploy order: pages first — they read only what both wires
   carry — the daemon after.
7. **One control lane.** `dispatch` runs on three threads unlocked. It is
   probably safe because everything is atomic, but "probably" is the word;
   the edit worker already exists — route presses through a second worker
   and the console through one of the two.

## What I would not do

- Move any use-case knowledge into the daemon. A face is a client concept;
  `fix`, `lw`, `ly` are generic and the Friends compose them. A Morphagene's
  Friend is a face over clip-reading verbs, not a `morphagene` mode.
- Rewrite in another language or to another architecture. The atomics and
  callbacks are the right shape for what this is; the debt is in how the
  state is *typed*, not in how it is *scheduled*.
- Do it all at once. Step 1 alone pays for itself; steps 2–5 each remove a
  class of bug the file has already had; 6 and 7 are hygiene.

## What it would cost

Steps 1 and 2 are a day together. 3 and 4 a day each. 5 is the one that
touches the callbacks and deserves a quiet day with the rig off. 6 and 7 an
afternoon. Roughly a working week, none of it blocking the Friends or the
pedalboard, all of it landing behind 43 daemon tests and two checkers that
already exist because the debt was felt before it was named.

## Addendum: Glassbox

Andrew floated dogfooding Glassbox (`purescript-hylograph-libs/
purescript-glassbox`: a state machine as a data artifact, run and drawn
from the same value, with a conformance runner whose claim is "same events
→ same state, same effects, same refusal tag") when the Add-layer wait
looked like a confused front end. It was not the front end that day, but it
is the right tool for **step 5**. The loop's phase machine is implemented
twice already — in Rust (`state.set` at 17 sites) and in PureScript
(`Socket.phaseOf`, `Machine.perform`'s guards) — and the two agree only by
care. Authored once as a Glassbox artifact, the machine becomes the spec
both sides are checked against: the PureScript side runs it directly; the
Rust side is driven through the enumerated (state × event × guard) table by
a test, the way `check-verbs` and `check-snapshot` already hold the two
sides together from outside. The refusal tags are the daemon's acks. That
would turn "one `transition` function" from a discipline into a checked
property, and the diagram would be the daemon's first honest picture of
itself.
