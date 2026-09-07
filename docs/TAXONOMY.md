# A taxonomy of loops

*Draft 1, 2026-09-07. The terms this project uses for the things a looper
holds, the axes those things vary along, the species of loop we expect to
support, and the life-cycle each one runs. Checked against the daemon as it
stands after the seven refactors of 2026-09-06, not against how we remember
it. Where the daemon and a page disagree, or where a species has no home
yet, it says so.*

The occasion: on the Arbhar's Friend, "Add layer" makes a new layer that
replaces what you hear, and what was wanted was the sound-on-sound kind of
overdub. That turned out to be a question about what kinds of overdub
there are, which is a question about what kinds of loop there are — and
the answer was not written down anywhere.

---

## 1. The nouns

**Rig.** The whole engine: the sources, the ring, the loops, the click, the
monitor, the Link clock. One per daemon. What `LooperState` describes.

**Source.** One input the rig can record from: `board` (stereo), `di`
(mono), `ipad` (stereo). A loop records one source, chosen before or —
from the ring — after the fact.

**Ring.** The pre-roll: the last sixty seconds of *every* source, always
being written. Nothing in the ring is a loop; it is where a claim, a
level-arm's reach-back, and a late close's correction all come from. "Always
be recording" (DESIGN-LOOPER §6) names it.

**Loop.** One of the rig's eight slots. A loop has a *length* (or none), a
*cycle*, up to N *layers*, and a set of playback properties (sounding,
speed, direction, pan, window, rotation, one-shot, chance, decay…). A loop
is the unit of transport: it plays, it stops, it is cleared. The word
"loop" is also used for the audio that goes round; when that matters, say
**the material**.

**Cycle.** The length the loop's *first* take set. After a multiply the
loop's length is a whole number of cycles (`cycles`); the cycle is still
the beat-grid the loop's layers are placed against. DESIGN-LOOPER §4: the
first loop defines the cycle for the rig too, when Link is not the clock.

**Length.** How far round the loop goes before it comes back:
`cycles × cycle`. A loop can have a length and no material — see *sized*
below.

**Bar.** Link's bar, in frames. The grid a gridded loop's boundaries snap
to. Not a loop property; the rig's.

**Layer.** A clip inside a loop: its own audio, its own length, and where
in the cycle it sounds (`period`, `phase`), when it was born, its gain,
whether it is on, and its **window**. Layers are never mixed down; the mix
happens at playback (DESIGN-LOOPER §5). A layer is the unit of undo, of
solo and mute, of decay, and of provenance.

**Take.** One recording event, which yields one layer. Used loosely today —
`w` "saves the take" (the whole loop), the ring gives "a take", `NextTake`
is a plan — and worth tightening: **a take is the act; a layer is what it
leaves.** Saving writes the loop; claiming the past is a take whose act is
in the ring.

**Tail.** The continuation a take kept past its end. Never sounded; it is
what a seamless wrap is made from. Belongs to the layer.

**Window.** A slice of something that is all that sounds. Two of them: the
**loop window** (`in`/`out`, one per loop, the edit's slider) and the
**layer window** (`lw`, one per layer, may reach before the start or past
the end — the Arbhar's thirteen seconds on a shorter or longer layer). A
window belongs to the audio it was cut from: it survives undo, and it does
not survive a new take into the slot (found the hard way, 2026-09-07).

**Rotation.** Where zero is: the loop's start moved by some frames. An edit,
not material.

**Plan.** What the *next* take will be: on the bar or now, one pass or open,
back-dated to a sound or not. The daemon's `NextTake`. A plan is spent when
the take starts.

**Face.** A page's configuration for one module — not a daemon concept,
but a face carries *policy* (solo the newest, thirteen seconds, forget the
length on open record) that the taxonomy has to place.

---

## 2. The axes

A loop is a point in a space with these axes. Most species are named by
fixing two or three of them.

### A. Where the length comes from

| origin | how | daemon |
|---|---|---|
| **open** | the second press sets it | `r … r` |
| **fixed** | declared in seconds, closes itself | `fix<secs>` then `r` |
| **bars** | declared in Link bars, sizes an empty loop | `len<n>` |
| **claimed** | the last complete cycle, from the ring | `t` |
| **inherited** | the loop's own cycle (an overdub, a one-pass layer) | `r` on material, `fix` on material |
| **threaded** | a blank tape of so many seconds | `blank<secs>` |
| **slaved** | a multiple of another loop's cycle | *not built* (DESIGN-LOOPER §18 Q4 recommends it) |

### B. Boundary discipline — what the take is quantised to

| discipline | start | end | daemon |
|---|---|---|---|
| **free** | the press | the press | default |
| **gridded** | the bar | the bar, rounded | `g1` — start and close file a frame the lane fires at |
| **launch-quantised** | the next beat/bar | — | `lq<n>` (rig-wide) |
| **level-armed** | the first sound over a threshold, reaching 50 ms back | the press | `lev1` + `r`; `arm<db>` |
| **rounded** | the press | the cycle boundary after the second press | multiply's close |

Note that these compose: a gridded, level-armed take is legal, and the
plan is where the composition lives.

### C. Composition — what new material does to what is there

This is the axis the Arbhar question was really about. Six kinds, of which
the daemon has three and a half.

| kind | what happens | undo | daemon |
|---|---|---|---|
| **layer** | a new clip, mixed at playback; you hear the old while you add | whole layer | `r` on material — *one layer per take, however many passes* |
| **sound-on-sound** | passes **sum into the same layer** as they go round; no feedback, so nothing recedes | the whole layer, exactly (the ring holds what was added) | this is what a **held** overdub already does: an overdub is modular |
| **tape** | read → filter → write in place, with feedback; the old recedes each pass. Frippertronics. | gone — it is a tape | `rvx1` + `fb` + `tone`; `blank` threads one |
| **alternate** | a new layer *instead of* the old one sounding: layers are takes of one scene, one sounds at a time | whole layer | *a page rule* (the Friend's face solos the newest); the daemon has only `ly` |
| **replace** | new material displaces the old for exactly the span played (EDP Replace / Substitute) | the span | *not built* |
| **insert** | new cycles are spliced into the loop where the press was (EDP Insert) | the cycles | *not built* |

Two consequences worth stating:

- "Sound-on-sound" on the Friend does not need a daemon change. It is a
  plain `r`, held across passes, with the face's *silence-while-it-goes-
  down* rule **not** applied. The rule is what the face adds to make a
  layer an alternate; drop it for this gesture and the daemon does the rest.
- **Alternate** is the only composition kind that lives in a page. That is
  the two-surfaces problem in another form: if "this loop's layers are
  alternates" were a property of the loop, both surfaces would agree about
  it, and the solo rule would be a daemon rule with no snapshot-diffing.
  See §6.

### D. Time structure — how the material sits in time

| property | meaning | daemon |
|---|---|---|
| **multiply** | the loop grows by whole cycles while a layer records across them | `x … x` |
| **spread / rotate / dense** | a layer sounds once every *n* cycles, at slot *k*; or every time | `s<n>`, `o`, `d` — per-layer `period`/`phase` |
| **speed, direction** | rate, reverse, pendulum | `sp`, `rev`, `pend` |
| **window, rotation** | which slice sounds, and where zero is | `in`/`out`/`win`, `lw`, `rot` |
| **crossfade** | at the wrap | `xf` |

### E. Playback identity — how the loop behaves when it plays

| kind | meaning | daemon |
|---|---|---|
| **continuous** | goes round until stopped | default |
| **one-shot** | silent until fired; one pass per fire | `one1`, `f` |
| **chance** | sounds each pass with a probability | `ch` |
| **decaying** | every layer recedes from its own birth | `dec` (per layer, so it sounds like tape, not a fader) |
| **sounding / silenced** | on or off, still turning | `h` |

### F. Provenance — what the layer knows about how it was made

Source (built), born (built), width (built: `mono`), the MIDI clip and the
believed board state and send lineage (DESIGN-LOOPER §8, not built). Not a
kind of loop; a property every layer carries, and the reason the re-render
and re-amp operations can exist later.

### G. Relation to other loops

Today every loop is independent, and the pages do the coordinating (Stop
All is six commands). The candidates for a relation:

| relation | meaning | status |
|---|---|---|
| **slaved** | length is 1×/2×/4× another loop's cycle, phase-locked | recommended in DESIGN-LOOPER §18, not built |
| **transport group** | start, stop, clear, fire together | pages fake it; not a daemon concept |
| **scene** | a set of loops that is one musical moment; switching scenes swaps what sounds | not built; the Friend's "alternates" is a scene inside one loop |

"Group" has been used for all three. It should be used for none of them
until one is chosen; see §6.

---

## 3. The species

The kinds of loop we expect to support, each named by the axes it fixes.
Everything not fixed is free.

| species | A length | B boundary | C composition | E identity | where it lives |
|---|---|---|---|---|---|
| **the pedal loop** | open | free (gridded when the rig has a clock) | layer; sound-on-sound when held | continuous | PWYF's default. The EDP loop. |
| **the grab loop** | bars from Link | gridded | layer | continuous | PWYF's grab bank: source on the iPad, grid on, `len` set on entry |
| **the fixed loop** | fixed (13 s) | free, closes itself | **alternate** | continuous | the Arbhar's Friend; every layer the module's length |
| **the tape** | threaded, or any loop re-threaded | free | tape | continuous, decaying by feedback | Revox mode; Frippertronics |
| **the claimed loop** | claimed | — (already happened) | layer | continuous | `t`; retrospective record. Also the first take of any species can be claimed rather than played. |
| **the clip** | any | any | any | **one-shot** | `one1` + `f`; a loop used as a sample |
| **the slaved loop** | slaved | inherits the master's | layer | continuous | not built |

A **sized** loop is not a species; it is a state every species can be in:
a length and no material, after the last layer was undone or after
`len`. It keeps the grid for the next take. Only the fixed loop wants it
gone (open means open), and that page forgets the length itself.

The species are not modes. A loop moves between them: a pedal loop that is
re-threaded becomes a tape; a claimed loop is a pedal loop whose first take
was in the ring; any loop with `one1` is a clip. What is fixed is the
*face's* expectation — the Friend expects fixed loops and nothing else.

---

## 4. The life-cycles

### The loop's machine

One machine serves every species. It is the daemon's `Phase`, held to the
Glassbox artifact `itajara-loop.json` by replay:

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

Twenty-one legal pairs; the rest log and are refused by the guards. What
the seven refactors established is that the *species* do not need machines
of their own: they differ in the **plan** (`NextTake`: at a boundary or
now, one pass or open, back-dated or not) and in the **composition**, which
is a property of the write (layer / summed / tape), not of the phase.

Two states the machine does not name, and should:

- **sized**: `Idle` or `Playing` with a length and no layers. Both pages
  had to learn to tell it from "playing" the hard way.
- **windowed**: not a phase, but a loop with a window refuses `x` and `t`,
  which makes it behave like one from a page's point of view.

### The layer's life

```
   (slot free) ──take──► born ──► sounding ◄──► off (ly / solo)
                           │                      │
                           └──── undone (kept; redo brings it back) ──┐
                                        │                             │
                    new take into the slot, or c: dropped ◄───────────┘
```

A layer's window, gain, period and phase are its own and survive `off` and
`undone`; they go when the layer is dropped. `set_shape` is the moment of
birth and is where the slot is made clean.

### The take's life (the plan)

```
   none ──fix / len / g / lev──► planned ──r──► requested ──boundary / sound──► started ──r / closer──► committed
                                    │                                                 │
                                    └────────── spent (a stale plan cannot outlive its press) ◄──────┘
```

### What the pages add

Each surface runs a machine of its own *over* the snapshot, and this is
where the species acquire their behaviour:

- **PWYF** (`Machine.purs`): a press is a statement; a duty on a loop
  becomes commands by reading the loop's phase. It adds the grab loop's
  setup and the arm gesture (`lev1` then `r`). It adds no composition kind.
- **the Friend** (`App.purs`): adds **alternate** — silence the sounding
  layer before a take, solo the newest when it lands, repair a loop with
  none sounding — and *open means open*. It keeps `growing`/`soloed` to
  know which loops are its own. This is a policy that has leaked twice.

---

## 5. Other people's words for these

| theirs | ours | note |
|---|---|---|
| EDP **Record / Overdub / Multiply / Undo** | take / layer / multiply / undo | the same gestures |
| EDP **Insert, Replace, Substitute** | — | not built (§2 C) |
| EDP **SUS** (hold-for-momentary) | — | a page concern: PWYF's switches-as-transitions |
| EDP **Feedback** | `dec` per layer; `fb` on a tape | the layered one is *not* feedback: nothing is destroyed |
| **Frippertronics** | the tape (`rvx`) with a long `blank` | two Revoxes = one tape with feedback |
| Loopy Pro **clip** | the clip (`one1` + `f`) | |
| Loopy Pro **start nudge** | not needed: the ring back-dates | DESIGN-LOOPER §6 |
| Loopy Pro **independent loops** | rejected for slaved | §18 Q4 |
| Ableton **Looper "Overdub"** | held `r` | modular sum |
| Boss RC **track** | loop | |
| Morphagene **reel / splice / gene** | loop / layer-with-window / (sub-window, not built) | daemon-debt review: granular reading may belong in the browser |
| Arbhar **layer** (13 s) | the fixed loop's layer | |
| Repeater **slip** | `rot` | |
| **retrospective record** | claim (`t`), and the level-arm's reach-back | |

---

## 6. Decisions this asks for

Ordered by how much they would change.

1. **Alternate as a loop property, not a page rule.** A loop declares that
   its layers are alternates (`alt1`, say); the daemon then silences the
   sounding layer when a take starts and solos the new one when it lands,
   and both surfaces see it. This ends the `growing`/`soloed` bookkeeping
   and the two-surfaces leak at its root. The Friend sets it on the loops
   it records into; PWYF never does. *Recommended.*
2. **Sound-on-sound on the Friend** is a held `r` with the alternate rule
   not applied: a third gesture on the face ("Overdub", hold to sum), or a
   face setting. No daemon change. If (1) is done, the daemon must let a
   held `r` on an alternate loop mean "sum into the sounding layer" — which
   is a small rule: an overdub on an alternate loop writes into the layer
   that sounds, rather than a new one.
3. **Say "sized" and "windowed" on the wire.** Two derived states every
   page has to compute; the snapshot can name them.
4. **Retire "group" until one relation is chosen.** Slaved lengths are the
   designed one and the cheapest; a transport group is what the pages fake
   today; a scene is the most expressive and the least designed. Pick
   slaved first if any.
5. **"Take" means the act.** `w` saves *the loop*; `t` *claims*; the plan is
   the plan. Rename in the acks and the pages when convenient.
6. **Replace and Insert** stay unbuilt until a gesture wants them. The write
   path could do Replace cheaply (write where the head is, gain zero
   beneath); Insert changes the length mid-loop and is the one to be
   suspicious of.

*Not asked, because answered:* the machine is one; the species are plans
and compositions; nothing found in the refactors needs undoing.
