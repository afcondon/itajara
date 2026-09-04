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
  vocabulary, and the recipes. No word for a pedal, an MC6 or a Twister.
- **`tools/`** — `check-verbs.py` holds the vocabulary to `dispatch`, and
  `check-snapshot.py` holds the snapshot types to what `ws.rs` sends. Both
  read both sources rather than trusting a comment. Run them after touching
  either side.

Split out of `producing-with-your-feet` on 2026-09-04 with its history; that
app is the first consumer, and the reasons for the split are in its
`docs/DESIGN-HARVEST.md` §6. The design notes for the engine are still in
that repo's `docs/DESIGN-LOOPER.md`.

```
cd daemon && cargo build --release && cargo test
cd client && spago build
python3 tools/check-verbs.py && python3 tools/check-snapshot.py
```
