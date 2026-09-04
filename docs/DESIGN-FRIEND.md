# The Friend — one looper, a face per module

**Status:** built to first light 2026-09-04, from a conversation. Picks up
`producing-with-your-feet/docs/DESIGN-HARVEST.md` §6 (three strata) and §7
step 10, which is where the idea came from; this is the design of the app
itself.

---

## 1. What it is

A looper for people with a sample-playing eurorack module, a sound source
and a Mac: the Itajara daemon with a page on it. **One app, and a face per
module.** When we say "Arbhar's Friend" we mean the Friend wearing its
Arbhar face; Morphagene's, Rample's and QD's Friends are the same bundle
with a different row of one table. The differences between them — how many
layers the module holds, how long one is, what a loop and a layer *become*
on the stick, how the daemon should be started — are data, so they are
`Friend.Face` and nothing else knows them.

**And the face is not the skin.** A face is a configuration; how the page
looks is a stylesheet. The two vary independently: the open-source build
ships a plain skin (`friend.css`, light, sans-serif, a grid of cards), and
a house-styled skin for a particular manufacturer — the Instruo one, to
share with Jason Lim as a video — is a second stylesheet that lives *outside
this repository*, replaces `friend.css`, and leaves the class names alone.
The public app is deliberately nobody's house style, so that sharing a
styled version with its maker is a gift rather than a copy.

---

## 2. Where the pieces live

```
itajara/
  daemon/    the engine (Rust)
  client/    A + B: socket, snapshot, verbs, recipes,
             Data.Looper.Duty (the vocabulary), Data.Looper.Machine (meaning)
  surface/   Halogen views over the client's types:
             Itajara.Surface.Wave (a layer's envelope as the loop plays it)
             Itajara.Surface.Edit (the Edit panel), and looper.css —
             one rendering of the class names they draw with
  friend/    the app: Friend.Face (the table), Friend.App (the page), Main
  tools/     check-verbs, check-snapshot
```

`producing-with-your-feet` consumes `client` and `surface` by path and is
the first consumer of both — its Looper page's Edit panel *is*
`Itajara.Surface.Edit` and its slots draw with `Itajara.Surface.Wave`.
That is what makes the seam real rather than claimed: one source, two apps,
the same picture. What the pedalboard keeps is everything with feet on it —
`Data.Looper.Banks` (MC6), `Data.Looper.Twister`, `Switchboard`, the slot
grid in the pedal's order — and one number, `Data.Looper.Surface.nLoops = 8`,
which is what *that* surface is laid out for. The machine no longer has a
loop count: "all loops" is the length of the daemon's array.

The three strata of HARVEST §6 are now three packages, and the leak it
named (`Duty` defined in the MC6 module) is closed by construction: the
client package cannot import a bank.

---

## 3. The page

One Halogen component, `Friend.App`. From the top:

- **Header** — the face's name, what it writes for whom, and the socket's
  truth in one line: looking, connected (with the URL), connected-but-silent
  (the age, so a dead engine behind a live socket is not "connected"), lost,
  or absent. Absent shows the face's daemon command, with `<device>` left for
  the reader.
- **Shape** — what the daemon reports (`nLoops × maxLayers × maxSecs` at the
  rate) beside what the face needs, and a warning when the daemon was started
  with fewer layers than a unit holds: *"start it with --layers 6 to fill
  one"*. The check is the face's, `Face.shapeNote`.
- **Loop cards**, one per loop the daemon has, laid out by the snapshot. Each
  says its state, its length (bars too when it is on the grid), and where it
  goes — *→ scene 3* — because that is the thing the harvest will do and the
  reason a loop's layers are grouped as they are. Each layer is a row: a
  checkbox (in or out of the mix, `ly<n>1|0`) and its envelope as the loop
  now plays it. Six buttons: Record (which says what the next press *does*,
  because `r` is one verb that opens, closes, overdubs or cancels), Overdub,
  Play/Stop, Undo, Clear, Edit.
- **Controls** — click, stop all, clear all; a take name; **Save for
  \<module\>**.
- **Log** — the daemon's acks by sequence and the app's own notes, newest
  first.
- **Edit** — the shared panel in a modal over the loop in focus, asking for
  its peaks only when the picture would differ (loop, layer count, newest
  birth).

Every button goes through `Machine.perform` against `rigOf` — the same
function a footswitch goes through in the pedalboard — with an empty grab
list, since this page can reach every loop. The one exception is the named
save: no switch can carry a name, so the vocabulary has no slot for one, and
`SaveAll` sends `<n>w<take>-<n>` per loop with material through the same
`runAction` (logged, sent, refused-if-no-daemon) but not through `perform`.

**No Twister, no MC6, no MIDI at all** in this first cut. HARVEST §6 sized a
Twister-for-those-who-have-one as `Data.Looper.Twister` plus a WebMIDI port;
that module is 1,650 lines and still in the pedalboard, and moving it is C's
problem (§7 step 11), not the Friend's.

---

## 4. Saving, and what is not built yet

`Save for Arbhar` sends one verb, `exl<take>`, and the daemon writes
`~/.itajara/takes/<take>/loop-<n>/` for every loop that holds something —
the layers raw, a version-1 `take.json` in each folder so it reloads as a
plain take, and one `export.json` at version 2 for the set carrying the
window, rotation, bars, tempo, source and per-layer gain and birth. That is
exactly the material a scene is made of, with the edit recorded beside it
rather than applied to it. The page says so under the button. The shaping
into the module's own layout — 24-bit, `<bank>_<scene>_scene/1_…6_.wav`,
the 10 s window with the loop's own first 3 s as tail, a `preset.txt` — is
HARVEST §4's `msm harvest`, and `Face.harvest` is `false` on every face
until it exists. When it does, the Friend needs a way to run it: the browser
cannot spawn a process and the daemon must not, so the Friend grows a small
Node server with `POST /harvest` (the pedalboard's `pwyf-store` is that seam
there), and `make serve` stops being `python3 -m http.server`.

Also not built, in the order they are likely wanted:

1. `msm harvest` for Arbhar; then `Face.harvest = true` and the button means
   what it says.
2. Per-loop level and source on the card. The daemon has both; the page
   shows neither yet.
3. Keyboard: number keys select a loop, space is Record. Recording with a
   mouse is a compromise, and the first thing a player will ask for.
4. The Instruo skin, out of tree.
5. The Twister, for those who have one.

---

## 5. Running it

```
cd daemon && cargo build --release
./target/release/itajara loop --device <device> --layers 6 --yes
cd ../friend && make serve        # bundles, copies looper.css, serves static/ on :3029
open http://localhost:3029/?face=arbhar
```

The page connects to `ws://127.0.0.1:3028` (the client's `defaultUrl`) and
reconnects by itself; start the daemon in either order.
