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
5. **`Phase` as an enum with one `transition` function.** The qualifiers
   move inside the constructors; the 17 sites become calls; the callback
   stays the only stamper of frames.
6. **One serialiser per type, and drop the top-level duplication** once the
   pedalboard reads `loops[i]` (it already can).
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
