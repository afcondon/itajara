# Itajara

The looper engine for the rig, named for *Epinephelus itajara*, the goliath
grouper, which booms.

- **`daemon/`** — the engine. A Rust/cpal daemon that opens the audio
  interface directly: record, overdub as layers, undo, multiply, a pre-roll
  ring so the past can be claimed, Link for the bar, and a WebSocket that
  pushes a state snapshot thirty times a second. Verbs go in as short
  strings through one `dispatch`; a footswitch, a browser button and a
  terminal cannot mean different things by the same name.
- **`client/`** — the PureScript half every surface on the daemon needs:
  the socket with its liveness watchdog, the snapshot decoder, the verb
  vocabulary, the recipes, and the meaning layer — `Data.Looper.Duty`, what
  a control can ask for, and `Data.Looper.Machine`, what each duty means
  against the daemon's own snapshot. No word for a pedal, an MC6 or a
  Twister.
- **`surface/`** — Halogen views over the client's types that any page on
  the daemon can draw: a layer's envelope as the loop now plays it, and the
  Edit panel. Plus `looper.css`, one rendering of the class names they use.

**The Friends** — the looper app for people with a sample-playing module,
a sound source and a Mac, one face per module — live in their own
repository, [FriendsOfItajara](https://github.com/afcondon/FriendsOfItajara),
and consume `client/` and `surface/` by path from beside this one.
- **`tools/`** — `check-verbs.py` holds the vocabulary to `dispatch`, and
  `check-snapshot.py` holds the snapshot types to what `ws.rs` sends. Both
  read both sources rather than trusting a comment. Run them after touching
  either side.

**How big can it be?** As big as memory. `--loops` and `--layers` have no
ceiling; the arena is `loops × layers × --max-secs × 2 channels × 4 bytes`,
allocated zeroed so the kernel commits pages only as loops fill (an 11 GB
ceiling costs about 45 MB until you record into it). At startup the daemon
prints the ceiling, asks on a terminal if it is more than a quarter of
physical memory, and refuses if it is more than all of it. There is no
flag past the refusal: the source is right here. `--yes` skips the
question for scripts and supervisors; with no terminal it goes ahead and
says so.

**What a client can know.** Every snapshot carries the shape — `nLoops`,
`maxLayers`, `sampleRate`, `maxSecs`, `fixedSecs`, `ringSecs` — so a page
lays itself out from the daemon rather than from a constant.

Split out of `producing-with-your-feet` on 2026-09-04 with its history; that
app is the first consumer, and the reasons for the split are in its
`docs/DESIGN-HARVEST.md` §6. The design notes for the engine are still in
that repo's `docs/DESIGN-LOOPER.md`.

```
cd daemon && cargo build --release && cargo test
cd client && spago build
cd surface && spago build
python3 tools/check-verbs.py && python3 tools/check-snapshot.py
```
