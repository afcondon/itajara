# Itajara

The looper engine for the rig, named for *Epinephelus itajara*, the goliath
grouper, which booms.

A Rust daemon that holds the audio interface directly and keeps eight loops
of layered audio going round; a PureScript client that every surface on it
shares; and a pair of Halogen views. Two apps sit on it today — the
pedalboard (`producing-with-your-feet`) and the Friends (one page, a face per
eurorack module) — and neither owns it. What follows is the whole thing in
one place: what the words mean, what the engine can do, how to talk to it,
how to run it, how it is built, and how it is held to itself.

- [1. What it is, and what it is not](#1-what-it-is-and-what-it-is-not)
- [2. The repository](#2-the-repository)
- [3. The vocabulary](#3-the-vocabulary)
- [4. The machines](#4-the-machines)
- [5. The verbs](#5-the-verbs)
- [6. The wire](#6-the-wire)
- [7. Running it](#7-running-it)
- [8. Inside the engine](#8-inside-the-engine)
- [9. Holding it together](#9-holding-it-together)
- [10. Checklists](#10-checklists)
- [11. Facts worth knowing before they cost a session](#11-facts-worth-knowing-before-they-cost-a-session)
- [12. Lineage and further reading](#12-lineage-and-further-reading)

---

## 1. What it is, and what it is not

**It is a pedal.** The load-bearing decision (DESIGN-LOOPER §2): the daemon
records what it is given, plays it back in layers, and answers a press with
one sentence. It knows sources, loops, layers and Link's bar. It has no word
for a pedal, an MC6, a Twister, an Arbhar or a browser; those are the
surfaces' words, and the surfaces keep them.

**It is the fourth always-recording device on the board.** Onward grabs
fragments, MOOD captures a few seconds, Habit holds thirty; Itajara keeps
sixty seconds of every input in a ring and lets you decide afterwards — claim
the last cycle, back-date a late close, reach fifty milliseconds behind a
level-armed start. "Always be recording" (DESIGN-LOOPER §6) is the idea most
likely to change how the thing gets used.

**It layers and never mixes down.** Every take is its own buffer, summed at
playback. Undo is free, so is muting or reversing one layer while the rest
play, so is a per-layer decay that sounds like tape rather than a fader.
*Store everything, flatten late*, the same rule `MidiClip` follows in
Triggerfish.

**It is not a MIDI device, an HTTP client, or a supervisor.** The app owns
MIDI (the reason is in `producing-with-your-feet/docs/DESIGN-HARVEST.md`
§6); `scripts/publish-take.mjs` is a separate process for the same reason;
Bosun starts and restarts the daemon and the daemon only has to be
startable.

**It is one engine under several surfaces**, and that is the source of most
of its design pressure. A face's policy is not the rig's; when two pages
want different things of one loop, the answer has been to make the
difference a property of the loop (see *alternates*, §3) rather than a rule
in a page. Where that is not yet done, the working rule is **one app on the
looper at a time** when their semantics differ.

---

## 2. The repository

```
itajara/                       github.com/afcondon/itajara, MIT
  daemon/     the engine: Rust + cpal, one binary `itajara`
    src/engine/   the looper proper (§8)
    src/ws.rs     the WebSocket: snapshot out, verbs in (§6)
    src/{align,measure,levels,devices,link,wav}.rs   the measurement and I/O modules
    scripts/      rig.py, conformance.py, publish-take.mjs (§9, §7)
  client/     the PureScript half every surface needs
    Foreign.LooperSocket        the socket, its watchdog, the snapshot decoder
    Data.Looper.Verb            the vocabulary as an ADT and its wire form
    Data.Looper.Duty            what a control can ask for (meaning, no device)
    Data.Looper.Machine         what each duty means against the live snapshot
    Data.Looper.Recipes         the generated recipes (one source for modal, suite, doc)
  surface/    Halogen views over the client's types
    Itajara.Surface.Wave        a layer's envelope as the loop now plays it
    Itajara.Surface.Edit        the Edit panel (window, rotation, layer windows)
    looper.css                  one rendering of the class names they draw with
  tools/      check-verbs.py, check-snapshot.py (§9)
  docs/       TAXONOMY.md, REVIEW-daemon-debt-2026-09-05.md, this file
```

Two consumers, checked out beside this one and reaching `client/` and
`surface/` by path (`extraPackages` with `path: ../itajara/client`):

- **`producing-with-your-feet`** — the pedalboard app; the first consumer,
  from which this repo was split with its history on 2026-09-04. Keeps
  everything with feet on it: the MC6 banks, the Twister, the switchboard.
- **`friends-of-itajara`** — the open-source looper for people with a
  sample-playing module and no pedalboard: one page, `?face=arbhar` first,
  Notes and Harvest panels, and a zero-dependency Node server that runs
  `msm harvest`. github.com/afcondon/FriendsOfItajara.

One source, two apps, the same picture: the pedalboard's Edit panel *is*
`Itajara.Surface.Edit`. That is what makes the seam real rather than claimed.

---

## 3. The vocabulary

The full treatment — the axes, the species, the life-cycles, the decisions —
is `docs/TAXONOMY.md`. This is the working set.

### The nouns

| word | meaning |
|---|---|
| **rig** | the whole engine: sources, ring, loops, click, monitor, Link. One per daemon; what the top of the snapshot describes |
| **source** | one input the rig can record from — `board` (stereo), `di` (mono), `ipad` (stereo) as the rig is launched today. A loop records one, chosen before or, from the ring, after |
| **ring** | the pre-roll: the last sixty seconds of *every* source, always being written. Where a claim, a reach-back and a late close's correction come from |
| **loop** | one of the rig's slots (eight today). Has a length or none, a cycle, up to N layers, and its playback properties. The unit of transport. When "loop" would mean the audio, say *the material* |
| **cycle** | the length the loop's first take set. After a multiply the length is a whole number of cycles; the cycle is still the grid its layers sit against |
| **length** | how far round before it comes back: cycles × cycle. A loop can have a length and no material |
| **sized** | a loop with a length and no layers — after the last layer is undone, or after `len`. Keeps the grid for the next take. On the wire since 2026-09-07 |
| **bar** | Link's bar, in frames; the rig's, not a loop's |
| **layer** | a clip inside a loop: its own audio and length, where in the cycle it sounds (period, phase), when it was born, its gain, whether it is on, its window. The unit of undo, of solo and mute, of decay, of provenance |
| **take** | one recording act, which yields one layer. `w` *saves the loop*; `t` *claims* the past; the plan is the plan |
| **tail** | the continuation a take kept past its end; never sounded; what a seamless wrap is made of |
| **window** | the slice that sounds. The *loop window* (`in`/`out`) is the edit's slider; a *layer window* (`lw`) may reach before the start or past the end. A window belongs to the audio it was cut from: it survives undo and does not survive a new take |
| **rotation** | where zero is; an edit, not material |
| **plan** | what the next take will be — on the bar or now, one pass or open, back-dated to a sound or not. `NextTake` in the engine; spent when the take starts |
| **alternates** | a loop that has declared its layers are takes of one scene, of which one sounds at a time, the newest. A loop property (`alt`), so every surface sees it |
| **face** | a page's configuration for one module; not an engine concept, but a face carries policy the taxonomy has to place |

### The axes

A loop is a point in a space with these axes; a species fixes two or three.

- **Where the length comes from:** open (the second press), fixed (seconds),
  bars (Link), claimed (the ring), inherited (the loop's own cycle),
  threaded (a blank tape), tied (a multiple of a reference loop's cycle —
  designed, not built).
- **Boundary discipline:** free, gridded (start and close on the bar),
  launch-quantised, level-armed (the first sound, reaching back), rounded
  (multiply's close). These compose; the plan is where.
- **Composition — what new material does to what is there:** *layer* (a new
  clip, mixed at playback), *sound-on-sound* (passes sum into one layer;
  what a held overdub does), *tape* (read → filter → write with feedback;
  Frippertronics), *alternate* (a new layer instead of the old one
  sounding), *replace* and *insert* (the Echoplex's; not built).
- **Time structure:** multiply, spread/rotate/dense, speed and direction,
  window and rotation, crossfade.
- **Playback identity:** continuous, one-shot, chance, decaying, sounding or
  silenced.
- **Provenance:** source, born, width now; the MIDI clip and the believed
  board state later.
- **Relation to other loops:** *tied* (length), *ganged* (transport),
  *scene* (a musical moment). The words "group" and "slaved" are retired.

### The species

| species | fixes | lives in |
|---|---|---|
| the pedal loop | open; free or gridded; layer, sound-on-sound when held | the pedalboard's default; the Echoplex loop |
| the grab loop | bars from Link; gridded; source on the iPad | the pedalboard's grab bank |
| the fixed loop | fixed (13 s); closes itself; alternates | the Arbhar's Friend |
| the tape | threaded or re-threaded; tape composition | Revox mode |
| the claimed loop | claimed from the ring | `t` — retrospective record |
| the clip | one-shot identity | `one1` + `f` |
| the tied loop | a multiple of a reference loop | not built |

The species are not modes: a loop moves between them. What is fixed is a
face's expectation.

---

## 4. The machines

**One machine serves every species.** In the engine it is `Phase`, six
values — Idle, Armed, First, Overdub, Multiply, Playing — stored by one
function (`Loop::enter`) against a table of twenty-one legal pairs. In
Glassbox (`purescript-hylograph-libs/purescript-glassbox/core/machines/
itajara-loop.json`) it is twelve states, because the artifact names phase
*and plan* together: empty, sized, tape, three kinds of armed, open and sized
recording, open and one-pass overdubbing, playing, multiplying. The species
differ in the plan and the composition, not in the machine.

```
            r (lev off)            r / closer
   Idle ─────────────────► First ─────────────► Playing ◄──┐
    │                                              │  ▲     │ r … r
    │ r (lev on)      sound / lev0                 │  │     │ (overdub)
    └──────────► Armed ─────────────► First/Overdub┘  │     │
                   │  r (taken back)                  │     ▼
                   └──────────────────────────────────┘  Overdub
                                                       x … x
                              Playing ────────────────────────► Multiply ──► Playing
   any ──── c ────► Idle           z (sized, no layers) ────► Idle
```

**The layer's life is the second artifact** (`itajara-layer.json`): free,
writing, summing, sounding, off, undone; 65 vectors. It carries the rule a fault taught:
everything a layer holds belongs to the audio; the shape declaration on the
way out of writing is the one moment the slot is made clean; undo keeps the
settings because it keeps the audio; a summed pass enters without zeroing
and leaves with only a redraw. Top, next-to-redo and was-on are facts the
loop holds.

**The plan's life stays a drawing** — the loop artifact already folds it in.

Both artifacts are replayed through the engine by `cargo test` (§9). The
artifact is the spec; when the two disagree, one of them is changed on
purpose and the review doc says which.

**What the pages add.** Each surface runs a machine of its own over the
snapshot. The pedalboard's `Data.Looper.Machine` turns a press into commands
by reading the loop's phase — a press is a statement, a turn is sometimes an
accident. The Friend adds *open means open* and, since the alternate
property moved into the engine, one thing more: it declares `alt` on the
loops it records into.

---

## 5. The verbs

Every command is a short string: an optional loop digit, a word from a
fixed table, and an argument the word admits. `3r` records loop 3; a bare
`3` selects it; a word with no digit acts on the selected loop. **Loops
count from zero on the wire** and from one on the pages. Flags take `1`,
`0`, or nothing to toggle. The table in `daemon/src/engine/verb.rs` is the
authority; the tokenizer matches whole words, longest first, so `rev` is
never read as `r`.

| word | arg | does |
|---|---|---|
| **the edit** | | |
| `win` | — | clear the loop's window |
| `in` / `out` | frames | set the loop window's start / end |
| `rot` | frames | shift the loop's start by a signed count |
| `pk` | n | ask for the loop's waveform in *n* buckets; comes back as its own message |
| **record, multiply, fire, claim** | | |
| `r` | — | record / overdub / close; arms under `lev1`; on an alternate loop with material, an open `r` sums into the sounding layer |
| `x` | — | multiply: grow the loop by whole cycles while a layer records; `x` again ends it, rounded |
| `f` | — | fire a one-shot from the top |
| `t` | secs | claim the past: with no loop, the last *secs* as the loop; with one, the last complete cycle as a new layer |
| `src` | n | which source this loop records |
| `mono` | flag | record this loop mono |
| `tone` | Hz | the tape's low-pass |
| **speed and structure** | | |
| `sp` | rate | playback rate (negative is not how you reverse; see `rev`) |
| `s` | n | spread: the newest layer sounds once every *n* cycles |
| `ph` | n | which of those *n* slots it lands on |
| `o` | — | rotate a spread layer one slot |
| `d` | — | dense: every cycle again |
| `g` | flag | on the grid: start and close on Link's bar |
| `z` | — | forget the length (a sized loop becomes empty) |
| `bpm` | — | take the tempo from this loop's length |
| `go` | — | start the session transport (Link start; the only verb that touches no audio) |
| `play` | flag | ask Link's transport to start / stop |
| `lq` | beats | launch quantise, rig-wide: -1 a bar, 0 none |
| **layers and the next take** | | |
| `ly` | k + flag | layer *k* on / off (`ly21`); on an alternate loop, on means solo |
| `lw` | k[:in:out] | a layer's own window, in frames of the loop, may reach past either end; bare `lw2` clears it |
| `dp` | k | duplicate layer *k* as a new layer, window and all |
| `cp` | src[l k] | copy loop *src* — or its layer *k* — onto this empty loop |
| `fix` | secs | the next take is exactly *secs* and closes itself; on material, the next layer is one pass |
| `len` | bars | size an empty loop in bars; declare the bar on a clockless first loop; resize otherwise |
| `alt` | flag | this loop's layers are alternates: one sounds, the newest |
| **undo** | | |
| `u` / `y` | — | undo the newest layer (kept for redo) / redo |
| `c` | — | clear: layers, length, and the loop's habits |
| **the tape** | | |
| `blank` | secs | thread a blank tape of *secs*: a silent layer, going round |
| `rvx` | flag | Revox mode: passes write in place with feedback; undo is gone |
| `fb` | dB | what a pass leaves of what was under it |
| `rev` | flag | play backwards |
| `pend` | flag | pendulum: forward then back |
| `one` | flag | one-shot: silent until fired |
| `lev` | flag | level-arm: `r` waits for a sound over the threshold and reaches 50 ms back |
| **the resolutions** | | |
| `arm` | dB | the level a level-armed loop waits for |
| `dec` | dB | per-layer decay: every layer recedes from its own birth |
| `xf` | ms | crossfade the wrap with the continuation |
| `ch` | 0..1 | chance a pass sounds |
| `vol` | dB | the loop's level |
| `pan` | 0..127 | place it |
| `h` | flag | sounding on / off, still turning |
| **to and from disk** | | |
| `w` | name | save the loop: one WAV per layer and a manifest, under the takes directory |
| `ex` / `exl` | name | export the set / the layers |
| **rig-wide** | | |
| `k` / `m` | flag | click / input monitoring |
| `l` / `p` | — | the console's level meter / status readout |

**Refusals are acks.** A press that cannot act answers with why — "loop 2 is
listening for a sound; finish that first", "loop 4 has the input" — and the
sentence ends the way the same refusal has always ended, because the
Glassbox replay matches refusals by their tags and the tags were named after
these sentences. A verb that says nothing is a bug (§11).

**The guards, in the daemon's order:** another loop holds the converter
(`busy_elsewhere`); the write head cannot follow this play head (pendulum, or
a tape at speed); not playing plainly; at the layer ceiling; still recording
(armed, waiting for the bar, or writing). Speed and direction stopped being
refusals on 2026-08-30 — the write head follows the play head, so an overdub
into a reversed or half-speed loop lands where you heard it.

---

## 6. The wire

**One WebSocket.** The daemon listens on `--ws-port`; on the rig that is
`23028`, brokered by Bosun as `:3028`. Text frames in are verbs; text frames
out are snapshots, thirty a second, plus the occasional peaks message.

**The snapshot** (`ws.rs`, `rig_json` → `loop_json` → `layer_json`) is one
JSON object:

- **rig level** — the shape (`nLoops`, `maxLayers`, `sampleRate`, `maxSecs`,
  `fixedSecs`, `ringSecs`, `sources[]`), the meters (`inDb`, `outDb`,
  `armDb`), the clock (`linkTempo`, `linkQuantum`, `linkBarFrames`,
  `barFrames`, `barOrigin`, `launchQ`, `linkAnchors`, `linkRejected`), the
  health (`audioAlive`, `deviceLost`, `reopens`, `calibrated`, `k`), the
  modes (`click`, `monitor`), `selected`, and **the ack path**: `ack` (the
  last sentence) and `ackSeq` (its number; a page shows an ack when the
  number moves, so two identical refusals are two lines).
- **per loop** (`loops[]`) — `index`, `state` (`idle` / `armed` /
  `recordingFirst` / `overdubbing` / `multiplying` / `playing`), `layers`,
  `loopFrames`, `loopSecs`, `pos`, `phase`, the flags (`armed`, `recording`,
  `quant`, `muted`, `reverse`, `pendulum`, `oneShot`, `levelArm`, `firing`,
  `skipping`, `revox`, `mono`, `alt`, `sized`, `windowed`), the numbers
  (`pan`, `speed`, `chance`, `fadeMs`, `decayDb`, `volDb`, `fbDb`, `toneHz`,
  `cycles`, `winIn`, `winOut`, `rot`, `src`, `pendingAt`), the take in hand
  (`recFrames`, `recEnv`), and `shapes[]`.
- **per layer** (`shapes[]`) — `len`, `period`, `phase`, `tail`, `gain`,
  `born`, `on`, `lwIn`, `lwOut`, `env` (the envelope as bytes).

`Foreign.LooperSocket` mirrors this field for field, and
`tools/check-snapshot.py` plus a unit test in `ws.rs` hold the two together
in both directions. The snapshot's hash is pinned in a test so a change to
the wire is a change you meant.

**Peaks.** `pk<n>` answers in the ack ("peaks for loop 0: 600 buckets over
6.167 s.") and sends the buckets as a separate message, so the picture is not
in every snapshot.

**The daemon's stdout** is the other observation post. Every command appears
as `[cmd] 5r@0` (unconditional; the `@0` is the caller lane) and every ack as
`[app] loop 5 recording.`; commits print the layer's envelope as a row of
glyphs with peak and RMS. Under Bosun this is
`/private/tmp/bosun-serve-<service>-worker.log`. When a page and a person
disagree about what happened, this log is right.

**The watchdog.** `LooperSocket.js` times the snapshots and declares the
socket dead when they stop — except in a hidden tab, where browsers deliver
bursts and a gap nobody watched is not evidence. A background tab is a dead
looper; drive the pages from a focused one.

---

## 7. Running it

```
itajara devices                              # what CoreAudio can see
itajara levels  --device AUDIO4c             # which jack is which host channel
itajara align   --device AUDIO4c             # prove the in/out frame arithmetic
itajara measure --device AUDIO4c ...         # the latency measurement
itajara loop    --device AUDIO4c [options]   # the looper
```

The rig's command line, as Bosun spawns it:

```
itajara loop --device AUDIO4c --out-ch 0 --ws-port 23028 --link \
             --source board=1,2 --source di=3 --source ipad=5,6
```

| option | meaning |
|---|---|
| `--device`, `--in-ch`, `--out-ch`, `--rate`, `--buffer`, `--mono-out` | the interface and how it is opened |
| `--source name=ch[,ch]` | a named source on those host channels; repeatable; `--source` decides what `src<n>` can choose |
| `--loops`, `--layers`, `--max-secs` | the arena: loops × layers × seconds × 2 channels × 4 bytes, allocated zeroed so the kernel commits pages as loops fill. No ceiling but memory; the daemon prints the figure, asks on a terminal above a quarter of physical memory, refuses above all of it |
| `--fixed-secs` | a fixed rig: every slot is a tape of one length, and `c` lands on a tape |
| `--ring-secs`, `--preroll-ms` | the pre-roll ring, and how far a level-armed start reaches back |
| `--link`, `--link-port` | subscribe to link-spike's `/link/anchor` for the bar |
| `--ws`, `--ws-port` | the socket |
| `--takes-dir` | where `w` writes (`~/.itajara/takes/` by default) |
| `--arm-db`, `--click`, `--monitor`, `--cycles`, `--loop-secs` | starting values of things verbs can change |
| `--yes` | skip the memory question, for supervisors |
| `--selftest`, `--json`, `--amp`, `--residual`, `--seconds`, `--repeats` | the self-test and the measurement modes |

**Under Bosun.** The daemon is a brokered service of `bosun serve`
(`alpha-victor-echo-kilo:worker`, public `:3028`, internal `:23028`). Stop
and start it through the control surface, never by pid — a hand-started
daemon loses the race to the one Bosun respawns, and you end up testing the
wrong binary:

```
curl -X POST 'http://localhost:3997/control/stop?port=3028'
curl -X POST 'http://localhost:3997/control/spawn?port=3028'
```

Bosun runs the registered command from the registered directory, so after
`cargo build --release` the next spawn is the new binary. Any take in a loop
is dropped by a restart.

**Takes on disk.** `w<name>` writes one WAV per layer and a manifest under
the takes directory. `scripts/publish-take.mjs` puts the *manifest* into
Amphora, the Atlantis artefact store — the audio stays on disk, since
Amphora's payloads are text — so a phrase becomes a thing the rig can name.
A separate process on purpose: the daemon holds buffers and the sample
clock, not an HTTP client.

**The console.** With a terminal attached, the same verbs work on stdin and
`p` prints status with waveforms, `l` the meters, `q` quits. When the
console closes the daemon says so and goes on serving the socket.

---

## 8. Inside the engine

`daemon/src/engine/`, fifteen files since the 2026-09-06 refactor. The
review that ordered it is `docs/REVIEW-daemon-debt-2026-09-05.md`.

| file | owns |
|---|---|
| `mod.rs` | the two rules below, the arena's story, the module list |
| `shared.rs` | `Shared`: the arena, the ring, the rig-wide atomics, the ack and peaks slots |
| `loop_state.rs` | `Loop`: every per-loop atomic; `enter`, `cleared`, `disarm`; the alternate rules (`hush`, `unhush`, `solo`, `repair_alt`) |
| `layer.rs` | `Layer`: len, tail, born, period, phase, gain, on, window; `set_shape`, `pos`, `windowed_pos` |
| `phase.rs` | the `Phase` enum and the twenty-one legal pairs; illegal pairs log in release and panic under test |
| `next_take.rs` | `NextTake`: the plan — request, request-at-frame, one-pass, arm-from |
| `verb.rs` | the word table and the tokenizer |
| `guards.rs` | the refusals a verb meets before it acts |
| `dispatch.rs` | the arms: one per word |
| `lane.rs` | **one control lane**: an mpsc channel owns `dispatch`; the quantised close and the multiply end file a frame the lane fires at; slow work (`w`, `ex`, `exl`, `pk`) runs on one thread with the ack routed back |
| `callbacks.rs` | the input and output callbacks: `stamp` (a request becomes a take at an exact frame), `crossed` (a level-arm found its sound), the mix |
| `commit.rs` | closing a take: measuring, the late-frames correction, the summed pass, the claim from the ring (`take`) |
| `cycle.rs` | multiply, spread and rotate, tempo |
| `edit.rs`, `copy.rs`, `export.rs` | the loop window and rotation; `cp` and `dp`; `w`, `ex`, `exl` |
| `control.rs`, `run.rs` | the console; the run loop and the supervisor (`drop_takes` on a lost device) |
| `conformance.rs`, `selftest.rs`, `tests.rs` | the Glassbox replays; the self-test; the unit suite and its helpers |

**Two rules that are not negotiable** (`mod.rs`): loop position is a device
frame count, never a host-clock instant — the two clocks differ by ~15.6
ppm and anything host-derived walks away from the audio; and
`out_frame = in_frame + K`, with `K` established once at the first input
callback and never recomputed.

**The arena.** Every sample is an `AtomicU32` holding f32 bits, accessed
`Relaxed` — the same load and store as a plain float on any machine this
runs on; the atomics buy the absence of undefined behaviour between the
input callback writing one layer and the output callback reading the others.
Allocated once at startup; no callback touches the allocator.

**Where decisions are made.** The callbacks do arithmetic and consume
requests at exact frames; they never decide. Every decision — a phase
change, a guard, an alternate's solo, which slot a take writes into
(`rec_slot`) — is made on the control lane and published as atomics the
callbacks read. That is what let the phase become an enum with one store
site, and what lets Glassbox's table be replayed against the engine at all.

---

## 9. Holding it together

```
cd daemon && cargo build --release && cargo test --release   # 84 tests
cd client && spago build                                      # zero warnings
cd surface && spago build
python3 tools/check-verbs.py                                  # the vocabulary → dispatch
python3 tools/check-snapshot.py                               # the wire → the client types (needs a running daemon)
```

**The Glassbox replays.** `engine/conformance.rs` reads
`purescript-glassbox/conformance/vectors/itajara-loop.json` — every (state ×
event × fact assignment) the artifact admits — builds the rig in each
vector's state, delivers the event the way the daemon does, and compares
the resulting state and refusal tag. 339 of 353 replayed, the rest skipped
by name for a reason the test prints. `engine/layer_conformance.rs` does
the same for the layer artifact's 65 vectors: 63 replayed, two skipped for
want of a verb. Its first run found one real hole — redo had no
still-recording guard — and two rules where the engine was right and the
artifact changed. Set `GLASSBOX_DIR` if the sibling checkout is elsewhere;
with no vectors the test says so and passes, so a lone checkout still builds.

**The two checkers** read both sources rather than trusting a comment.
`check-verbs.py` proves every verb the client can send has an arm;
`check-snapshot.py` proves every field the client declares is on the wire.
Run them after touching either side.

**Against the live daemon.** `daemon/scripts/rig.py` is a dependency-free
WebSocket client (`Rig().snapshot()`, `.send("5r")`) for reading engine
truth from a script. `daemon/scripts/conformance.py` sends every verb the
app can send and records what came back — phase A against an empty scratch
loop (a refusal is an ack), phase B with `--with-audio` against a loaded
one. It refuses to run if the scratch loop is not empty or anything is
recording, restores the rig-wide modes it touched, and reports whether the
rig was left as found. Read its docstring before running it on a rig with
takes in it; its author once destroyed one.

**Testing the pages is a human's job.** Driving them through a browser
automation leaves the tab hidden, which throttles the poll loop and
disables the watchdog by design; the display then looks frozen for reasons
that are not bugs. A person presses; the daemon's log is read.

---

## 10. Checklists

**Adding a verb.** A row in `verb.rs` (word, argument kind); an arm in
`dispatch.rs` that *returns* its sentence (a `println!` is an ack the app
never hears); a constructor in `Data.Looper.Verb` with its wire form; a
duty in `Data.Looper.Duty` if a control can ask for it, with its word and
its description; `check-verbs.py`; a unit test; the recipe if it is one. If
the verb has a phase to change, it is a row in the Glassbox artifact first.

**Adding a snapshot field.** The emitter in `ws.rs`; the pinned hash and
fixtures in its tests; the record and decoder in `Foreign.LooperSocket`;
`check-snapshot.py`. Derived states the pages would otherwise compute
(`sized`, `windowed`) go on the wire, not in the pages.

**Adding a setting to a layer.** Ask which it belongs to. If it belongs to
the audio — as len, tail, born, period, phase, gain, on and the window all
do — it is reset in `Layer::set_shape` and it survives undo. If it were to
describe the slot instead, it would be the first such thing; say why in the
artifact's notes.

**Adding a policy to a page.** Ask whether it is the face's or the loop's.
If a second surface would need to know it to behave correctly — as solo the
newest was — it is a loop property in the daemon, declared by the page that
wants it. A snapshot-diff rule in a page will fire on the other surface's
work.

**Adding a state.** It is a state when it changes what the host must do on
entry, or what more than one event does; it is a fact when only a guard
reads it at the moment of a press. Write it in the artifact, regenerate the
vectors (`make vectors` in the Glassbox repo), and let the replay tell you
where the engine disagrees.

---

## 11. Facts worth knowing before they cost a session

- **Loops count from zero on the wire** and from one on every page. `5r` is
  the sixth loop; "loop 6" in an ack is the same loop.
- **One converter, one recording.** A second loop asked to record while
  another has the input is refused out loud, because silently it would go
  to First and capture nothing.
- **An overdub is modular.** Frames recorded after the closing press wrapped
  and summed onto the head; `commit` takes them back off exactly, from the
  ring, and keeps them as the continuation. A doubled transient at the loop
  point is this, not a length error.
- **A held overdub is sound-on-sound** — passes sum into one layer. On an
  alternate loop an open `r` sums into the layer that sounds; `fix` still
  makes a new alternate.
- **Undo keeps the length.** The next take lands on the same grid; the loop
  is *sized*, and the wire says so. A page that wants an open take there
  forgets the length first.
- **A window belongs to the audio.** It survives undo and is dropped by a
  new take. Before 2026-09-07 a slot kept its window across a clear, and a
  take played only where the old window fell.
- **Silence is a refusal you did not hear.** Fourteen verbs once acked to
  stdout only; the app could not tell a refused press from a lost one.
  Every arm returns its sentence now, and `conformance.py` counts the silent
  ones.
- **Hand-started daemons lose.** Bosun respawns the registered command; a
  daemon you started by hand may be the one that dies, and the page will be
  talking to the other one.
- **A background tab is a dead looper**, and a cached bundle is a stale one.
  Reload the page in a focused tab after a bundle; the `?v=` on the script
  tag exists for this.
- **The rig is the truth.** When a page shows something odd, ask the
  snapshot (`rig.py`) and the daemon's log before the page.

---

## 12. Lineage and further reading

- `docs/TAXONOMY.md` — the vocabulary in full: nouns, axes, species,
  life-cycles, the decisions of 2026-09-07 and their status.
- `docs/REVIEW-daemon-debt-2026-09-05.md` — why the engine looked as it did,
  the seven refactors, and the before/after.
- `producing-with-your-feet/docs/DESIGN-LOOPER.md` — the original design:
  the pedal decision, prior art (the Echoplex Digital Pro, the Electrix
  Repeater), the clock, layers, always-be-recording, the source matrix,
  provenance, latency, the foot, memory. Still the reference for *why*.
- `producing-with-your-feet/docs/DESIGN-HARVEST.md` §6 — the reasons for the
  split into three packages and why the app owns MIDI.
- `friends-of-itajara/docs/DESIGN.md` — the Friends: one app, a face per
  module; where the pieces live; saving, harvesting and the datasheet.
- `purescript-glassbox/core/machines/itajara-loop.json` and
  `itajara-layer.json` — the two machines as artifacts, with their notes.
- Marginalia 286.

Split out of `producing-with-your-feet` on 2026-09-04 with its history.
MIT.
