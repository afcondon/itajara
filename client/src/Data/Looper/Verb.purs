-- | What you can ask Itajara to do, as a type.
-- |
-- | ## Why this exists
-- |
-- | The daemon takes commands as short text — `r` records, `h0` silences, `sp75`
-- | plays at three-quarter speed. Two places in this app compose that text:
-- | `Data.Looper.command`, which maps a CC to a command for the pedal surface,
-- | and `Data.Looper.Machine`, which maps a footswitch duty to one for the board.
-- | Both wrote the words out as string literals, and neither knew what the other
-- | knew.
-- |
-- | **A mistyped verb costs a press and says nothing useful.** `h1` for `hl` is
-- | not a type error; it is a switch that does nothing, on a board being used
-- | with both hands full. The daemon answers `unknown command "hl"`, which is
-- | the right answer and arrives on the ack path — but a refusal and a typo
-- | read identically from a chair, and one of them is a bug in this repo.
-- |
-- | So the vocabulary is a type, `render` is the only place a verb becomes text,
-- | and the two tables now spell from the same book.
-- |
-- | ## The grammar, which is the daemon's and not ours
-- |
-- | `dispatch` in `itajara/src/engine.rs` accepts three shapes, and this type has
-- | one family per shape:
-- |
-- | * **bare** — `r`, `c`, `u`, a word on its own;
-- | * **flag** — `h`, `g`, `rev`, with an optional `1` or `0`. The bare form
-- |   toggles and the suffixed forms set. **We always set**, for the daemon's
-- |   own stated reason: *"a client that flips rather than sets drifts out of
-- |   step the first time a message is dropped and never recovers."*
-- | * **numeric** — `sp75`, `pan64`, a word with a number stuck to it.
-- |
-- | Anything the app does not send is deliberately absent. This is the app's
-- | half of the vocabulary, not a model of the whole daemon; `l`, `p` and the
-- | other console-only verbs have no business being constructible here.
-- |
-- | ## Not to be confused with `Data.MC6.Verb`
-- |
-- | Same word, different language. That one classifies what a *footswitch*
-- | means on the MC6 — navigation, action, preset, scene — and lives entirely
-- | on the device side. This one is what the *daemon* accepts. They never meet,
-- | but a module importing both must alias them apart.
module Data.Looper.Verb
  ( Verb(..)
  , render
  , at
  ) where

import Prelude

-- | One instruction to the daemon.
data Verb
  -- | Record: start a first take, close one, or overdub — the daemon decides
  -- | which from the loop's phase, and that is the whole point of it being one
  -- | verb. See `Foreign.LooperSocket.LoopPhase`.
  = Record
  -- | Multiply: extend by whole cycles while it runs, and close on the next
  -- | press. Asks "how many bars of this?".
  | Multiply
  -- | Spread one-in-`n`: the layer keeps its length and the loop grows around
  -- | it, so the pass sounds `n` times less often. Asks "how often?" where
  -- | `Multiply` asks "how long?" — structural, instant and reversible, and it
  -- | records nothing.
  -- |
  -- | Bare `s` means two in the daemon; we always send the count, because a
  -- | default that only one caller relies on is a mode.
  | Spread Int
  -- | Move a spread layer one slot later in its cycle.
  | Rotate
  -- | Undo the spread: sound every time round again.
  | Dense
  -- | Undo one layer. Deliberately keeps the loop's length, so the next take
  -- | lands on the same grid.
  | Undo
  -- | Put back what `Undo` took.
  | Redo
  -- | Let go of the length that `Undo` kept.
  | ForgetLength
  -- | Clear the loop: layers and length together.
  | Clear
  -- | Fire a one-shot: one pass, rather than turning for ever.
  | Fire
  -- | Claim what just happened, from the daemon's pre-roll ring.
  -- |
  -- | **Not implemented by the daemon.** The ring is written on every input
  -- | frame and `ring_at` can read it back, but `dispatch` has no arm for `t` —
  -- | so this returns `unknown command "t"` on the ack path. Kept because the
  -- | surface offers it and the engine is half-built for it, not because it
  -- | works.
  | ClaimPast
  -- | Write the loop's layers out as WAV files, under this name; empty takes the
  -- | daemon's default. The daemon returns where it put them, which is the
  -- | whole point of the command and the reason it answers on the ack rather
  -- | than on its own stdout.
  | SaveTake String

  -- | Flip the metronome rather than setting it.
  -- |
  -- | **The one toggling form still sent**, and it contradicts the rule stated
  -- | at the top of this module. It survives because `Data.Looper.Machine.act`
  -- | is given `Rig`, which carries the loops and the focus and not the global
  -- | flags — so there is nothing there to compute `Click (not current)` from.
  -- | Harmless today only because the app also mirrors the daemon's reported
  -- | `click` back on every poll, so a dropped command is corrected within
  -- | 33 ms. `Click` is the form to prefer wherever the current value is known.
  | ClickToggle

  -- | Audible, or silenced but still turning. `Sounding false` is `h0`.
  -- |
  -- | Note the daemon's digit is audibility, not hush: `h1` clears `muted`.
  | Sounding Boolean
  -- | Whether this loop waits for the grid — the anchor loop's cycle — before
  -- | starting, and rounds its length to a whole number of those.
  | OnGrid Boolean
  | Reversed Boolean
  | Pendulum Boolean
  -- | One pass per trigger rather than turning for ever.
  | OneShot Boolean
  -- | Wait for a sound rather than for a foot.
  | LevelArm Boolean
  -- | The metronome, and input monitoring. Global rather than per-loop in the
  -- | engine, but addressed the same way.
  | Click Boolean
  | Monitor Boolean

  -- | Loop frames per output frame, as a percentage: `100` is unity.
  | Rate Number
  -- | Where it sits in the stereo field, 0-127, 64 centre.
  | Place Int
  -- | How much of the wrap is crossfaded with the layer's continuation, in
  -- | milliseconds. Zero is a hard join.
  | Fade Number
  -- | How much a pass costs the material already there, in decibels. Zero holds
  -- | for ever.
  | Decay Number
  -- | How often a pass sounds, as a percentage. `100` is always.
  | Chance Number

derive instance Eq Verb
derive instance Ord Verb

-- | Deliberately **no `Show` instance.** `render` is a wire encoding, and
-- | wiring it to `Show` would make `show` the thing that talks to the daemon —
-- | which is the one job `Show` must not have. Call `render` where the text is
-- | wanted; there is no second way to spell a verb, which is the point.

-- | The daemon's own text for a verb. **The only place a verb becomes a
-- | string** — that is the entire job of this module.
render :: Verb -> String
render = case _ of
  Record -> "r"
  Multiply -> "x"
  Spread n -> "s" <> show n
  Rotate -> "o"
  Dense -> "d"
  Undo -> "u"
  Redo -> "y"
  ForgetLength -> "z"
  Clear -> "c"
  Fire -> "f"
  ClaimPast -> "t"
  SaveTake name -> "w" <> name
  ClickToggle -> "k"

  Sounding on -> flag "h" on
  OnGrid on -> flag "g" on
  Reversed on -> flag "rev" on
  Pendulum on -> flag "pend" on
  OneShot on -> flag "one" on
  LevelArm on -> flag "lev" on
  Click on -> flag "k" on
  Monitor on -> flag "m" on

  Rate n -> "sp" <> show n
  Place n -> "pan" <> show n
  Fade n -> "xf" <> show n
  Decay n -> "dec" <> show n
  Chance n -> "ch" <> show n

-- | The explicit form of a flag, never the bare toggling form. See the note on
-- | the module about why.
flag :: String -> Boolean -> String
flag word on = word <> (if on then "1" else "0")

-- | A verb addressed to one loop: `3r` is "record on loop 3".
-- |
-- | Every command accepts this prefix, and the board path always uses it. The
-- | daemon keeps a selection too, but as its own comment says, *"selection that
-- | only some callers depend on is a mode, and a mode that a footswitch could
-- | fall out of step with is the thing this design is trying not to have."*
at :: Int -> Verb -> String
at i v = show i <> render v
