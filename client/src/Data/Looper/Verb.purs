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
-- | ## Two tests, and only one of them is an oracle
-- |
-- | `test/Main.purs` pins every spelling — but against constants a human typed
-- | while reading `engine.rs`. That catches an accidental edit to `render` and
-- | nothing at all on the far side of the wire.
-- |
-- | `tools/check-verbs.py` is the other half: it reads **both** sources and
-- | checks that every verb this module can render has an arm in `dispatch`. Run
-- | it after touching either side. It found a mistake in this file's own
-- | comments the first time it ran — `t` had been written off as unimplemented
-- | because its arm is a char guard rather than a string match, and a grep for
-- | `"t"` finds nothing.
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
  , addressed
  , render
  , at
  ) where

import Prelude

import Control.Alternative (guard)
import Data.Int as Int
import Data.Maybe (Maybe)
import Data.String.CodeUnits as SCU

-- | One instruction to the daemon.
data Verb
  -- | Record: start a first take, close one, or overdub — the daemon decides
  -- | which from the loop's phase, and that is the whole point of it being one
  -- | verb. See `Foreign.LooperSocket.LoopPhase`.
  = Record
  -- | Multiply: extend by whole cycles while it runs, and close on the next
  -- | press. Asks "how many bars of this?".
  | Multiply
  -- | How often the newest layer sounds, in cycles of its own length.
  -- |
  -- | **Absolute since 2026-08-27, and it no longer changes the loop's length.**
  -- | It used to mean *sound n times less often than you already do* and grow
  -- | the loop by the same factor — one gesture setting two things, which is the
  -- | right shape for a footswitch and the wrong one for a knob. `Bars` says how
  -- | long; this says how often the material lands in it. A four-bar loop whose
  -- | phrase sounds every bar and one whose phrase sounds once are the same
  -- | length and different music, and neither was reachable before.
  -- |
  -- | Bare `s` means two in the daemon; we always send the count, because a
  -- | default that only one caller relies on is a mode.
  | Spread Int
  -- | Which slot of its period the newest layer lands on — one-based on the
  -- | wire, because that is how the daemon counts them back to you.
  -- |
  -- | **Wraps rather than refusing.** Its range depends on the period, and
  -- | `Data.Looper.Twister.fromKnob` is deliberately a pure function of a
  -- | position — making one knob's range depend on another's *value* would make
  -- | it need the snapshot. So any slot is legal and the daemon wraps it.
  | Place' Int
  -- | How many bars a loop is. See `Data.Looper.Banks.SetBars` for the three
  -- | things this means depending on what the loop already is.
  | Bars Int
  -- | What a launch waits for, in beats. **Rig-wide**, so the loop prefix the
  -- | wire carries is ignored — `-1` is a bar and is the default, `0` is none.
  | LaunchQ Int
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
  -- | **Take the session tempo from this loop**, which is the one verb here
  -- | whose effect leaves the rig: link-spike passes it to Link and every peer
  -- | follows — Ableton, purerl-tidal, the modular's tempo-relative rates.
  -- |
  -- | It moves no audio. `loop_len` is frames and loops stay phase-locked to
  -- | each other whatever a bar is; what a tempo reaches is the click, the
  -- | quantised launches and closes, and everything downstream of Link. The
  -- | principle is the bar model's own, at rig scale: move the grid to the
  -- | audio, never the audio to the grid.
  | TakeTempo
  -- | **Which input a loop records from**, one-based. Named on the wire by
  -- | number and by name in every ack, because the daemon owns the table.
  | Source Int
  -- | Fold a loop's two channels together at playback. Reversible by
  -- | construction: the audio is always kept in stereo.
  | Mono Boolean
  -- | Clear the loop: layers and length together.
  | Clear
  -- | Fire a one-shot: one pass, rather than turning for ever.
  | Fire
  -- | Claim what just happened, from the daemon's pre-roll ring — the one thing
  -- | no pedal can do, since it needs sixty seconds of input kept whether
  -- | anything was recording or not.
  -- |
  -- | Takes seconds; the daemon defaults to 8 when the argument is absent or
  -- | unreadable, and we send nothing, so this is `t` and means eight seconds.
  -- | Carried as a constructor without a payload until there is a control for
  -- | the duration, rather than hard-coding 8 here and pretending it was
  -- | chosen.
  -- |
  -- | Its arm in `dispatch` is a **char guard** — `l if l.starts_with('t')` —
  -- | not a string match, which is worth knowing: a grep for `"t"` in engine.rs
  -- | finds nothing and reads exactly like an unimplemented verb. It is not.
  -- | What is true is that `take` reports only to stdout and returns unit, so a
  -- | successful claim is silent on the ack path.
  | ClaimPast
  -- | Write the loop's layers out as WAV files, under this name; empty takes the
  -- | daemon's default. The daemon returns where it put them, which is the
  -- | whole point of the command and the reason it answers on the ack rather
  -- | than on its own stdout.
  -- |
  -- | **Send this with `at`, never bare.** Unprefixed it saves the daemon's
  -- | selected loop, and nothing on the six-loop surface writes that field — so
  -- | it silently wrote loop 1 whatever the board was focused on, and said it
  -- | had succeeded. True of every per-loop verb here; it is called out on this
  -- | one because this is the one it actually happened to.
  | SaveTake String

  -- | Render every loop that holds something to its own WAV, and a manifest
  -- | beside them.
  -- |
  -- | **Send this bare, unlike `SaveTake`.** It is a `sh.` command rather than
  -- | an `lp.` one — it is about the set, not a loop — so a leading digit would
  -- | be noise, in the same family as `Click` and `Monitor`.
  -- |
  -- | And it is a different artefact from Save, not a better one. Save writes
  -- | one loop's *layers*, raw: itajara's own format, the thing you reload to
  -- | keep overdubbing. This writes *loops*, flattened and rendered, which is
  -- | what Ableton, Loopy and msm mean by the word. What the render leaves out
  -- | — chance, one-shot, mute — the manifest records, because every one of
  -- | those receivers can do them itself.
  | ExportSet String

  -- | Every loop that holds something, from the top, **together**.
  -- |
  -- | This was eight `Sounding true`s, and eight unmutes is not a start: mute
  -- | never moved a playhead, so the set came back in whatever phase
  -- | relationship it happened to be in — which, when the loops are four bars,
  -- | three and one, is not a musical fact about anything.
  -- |
  -- | Bare and rig-wide, and the deadline is computed once inside the daemon:
  -- | eight commands asking for the next boundary at eight slightly different
  -- | moments would get eight answers and land ragged, which is the thing this
  -- | exists to prevent. It lands on whatever `launch quantise` says, so it is
  -- | not a second opinion about when a launch happens.
  | StartAll


  -- | Audible, or silenced but still turning. `Sounding false` is `h0`.
  -- |
  -- | Note the daemon's digit is audibility, not hush: `h1` clears `muted`.
  | Sounding Boolean
  -- | Whether this loop waits for the grid — the anchor loop's cycle — before
  -- | starting, and rounds its length to a whole number of those.
  | OnGrid Boolean
  | Reversed Boolean
  -- | Make this loop a tape, or stop. **Entering flattens it to one layer and
  -- | that is not reversible** — `Revox false` stops the erasing and does not
  -- | unfold what was folded.
  | Revox Boolean
  | Pendulum Boolean
  -- | One pass per trigger rather than turning for ever.
  | OneShot Boolean
  -- | Wait for a sound rather than for a foot.
  | LevelArm Boolean
  -- | The metronome, and input monitoring. Global rather than per-loop in the
  -- | engine, but addressed the same way.
  -- |
  -- | **There was a `ClickToggle` here until 2026-08-25**, rendering the bare
  -- | flipping `k`, and it carried a paragraph excusing itself: the machine was
  -- | given a `Rig` that held the loops and the focus and not the global flags,
  -- | so there was nothing to compute `Click (not current)` from. `Rig` carries
  -- | them now. The excuse expired and the constructor went with it, which is
  -- | the rule at the top of this module doing its job: what the app does not
  -- | send is not constructible.
  | Click Boolean
  | Monitor Boolean

  -- | Loop frames per output frame, as a **multiplier**: `1.0` is unity.
  -- |
  -- | The daemon takes 0.125 to 4 either sign, and the sign is the direction —
  -- | so `Rate (-2.0)` is twice speed backwards and `Reversed` is the same fact
  -- | said the other way. Refused rather than clamped outside that range,
  -- | because below an eighth the interpolation is audibly a filter and above
  -- | four the aliasing is the loudest thing in the sound.
  -- |
  -- | **This said "as a percentage: 100 is unity" until 2026-08-25**, which no
  -- | caller ever believed — the Speed bank has always sent `Rate 0.25`. A
  -- | comment that disagrees with every call site is worse than none, because
  -- | the next surface is written from the comment.
  | Rate Number
  -- | Where it sits in the stereo field, 0-127, 64 centre.
  | Place Int
  -- | How much of the wrap is crossfaded with the layer's continuation, in
  -- | milliseconds. Zero is a hard join, and half a second is the ceiling —
  -- | past that it is not a join, it is a different effect.
  | Fade Number
  -- | How much a pass costs the material already there, in decibels. Zero holds
  -- | for ever; -60 is the floor. Positive is refused by the daemon rather than
  -- | clamped, because feedback above unity is not a longer decay, it is a loop
  -- | that gets louder until it clips.
  | Decay Number
  -- | The level a sound has to reach before a level-armed loop starts, in
  -- | decibels; 0 to -80.
  -- |
  -- | **Rig-wide, so it goes unprefixed** — it describes the room and the
  -- | instrument, not any one loop, which is the daemon's own reasoning and the
  -- | same shape as `Click` and `Monitor`.
  | ArmLevel Number
  -- | Thread an empty tape of this many seconds: a loop with a length and one
  -- | silent layer, going round, ready to be played onto.
  -- |
  -- | **The only way a loop gets a length without being recorded.** Refused when
  -- | the loop has anything in it — resizing a loop with material in it would be
  -- | a trim, which this engine does not have.
  | Blank Number
  -- | What a Revox pass leaves of what was under it, in decibels; 0 to -60.
  | Feedback Number
  -- | How much top a Revox pass keeps, in hertz; 200 to 20000, and 20000 is
  -- | off. Tape loses the high end before it loses the level, and losing only
  -- | the level is what makes a feedback loop sound digital.
  | Tone Number
  -- | This loop's own level, in decibels; `0.0` is unity and `-60.0` is
  -- | silence. Above unity is refused rather than clamped, like `Decay`.
  -- |
  -- | **The engine had no level until 2026-08-25**, and the reason it managed
  -- | without one is worth keeping: a looper whose loops are either in or out
  -- | needs no faders, because mute says everything. What changed is a
  -- | controller with a knob per loop — and the first thing a hand does with a
  -- | knob is set how loud something is.
  | Level Number
  -- | How often a pass sounds, as a **probability**. `1.0` is always.
  -- |
  -- | Zero to one, and refused outside it. Same correction as `Rate` above and
  -- | on the same day: this claimed percentages while `chanceLadder` has always
  -- | held 1.0, 0.75, 0.5.
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
  Place' n -> "ph" <> show n
  Bars n -> "len" <> show n
  LaunchQ n -> "lq" <> show n
  Rotate -> "o"
  Dense -> "d"
  Undo -> "u"
  Redo -> "y"
  ForgetLength -> "z"
  TakeTempo -> "bpm"
  Source n -> "src" <> show n
  Mono on -> flag "mono" on
  Clear -> "c"
  Fire -> "f"
  ClaimPast -> "t"
  SaveTake name -> "w" <> name
  ExportSet name -> "ex" <> name
  StartAll -> "go"

  Sounding on -> flag "h" on
  OnGrid on -> flag "g" on
  Reversed on -> flag "rev" on
  -- `rvx`, not `rev`: reverse got there first and a prefix collision on the
  -- wire is a command that silently means something else.
  Revox on -> flag "rvx" on
  Pendulum on -> flag "pend" on
  OneShot on -> flag "one" on
  LevelArm on -> flag "lev" on
  Click on -> flag "k" on
  Monitor on -> flag "m" on

  Rate n -> "sp" <> show n
  Place n -> "pan" <> show n
  Fade n -> "xf" <> show n
  Decay n -> "dec" <> show n
  ArmLevel n -> "arm" <> show n
  Blank n -> "blank" <> show n
  Feedback n -> "fb" <> show n
  Tone n -> "tone" <> show n
  Level n -> "vol" <> show n
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

-- | Which loop a rendered command is addressed to, and what is left of it.
-- |
-- | **The inverse of `at`, and proved to be one.** Reading a wire string back
-- | is normally the thing this module exists to prevent — there is one place a
-- | verb becomes a string and no second spelling — so this earns its place the
-- | way `fromKnob` and `toKnob` do: the suite walks every loop and every verb
-- | and asserts `addressed (at i v)` gives back exactly `i` and `render v`.
-- | A drift between the two is a failing test rather than a wrong label.
-- |
-- | It exists because the log had a **reporting** bug of the worst kind: it
-- | printed the wire, the wire counts loops from zero, and every surface a
-- | human reads counts from one. So selecting Loop 2 and asking for four bars
-- | logged `→ 1len4`, which is correct and reads as loop one. Behaviour that is
-- | right and says it in the wrong dialect is indistinguishable from behaviour
-- | that is wrong, and costs exactly as much to chase.
addressed :: String -> Maybe { loop :: Int, verb :: String }
addressed s = do
  let digits = SCU.takeWhile isDigit s
  guard (digits /= "")
  n <- Int.fromString digits
  pure { loop: n, verb: SCU.dropWhile isDigit s }
  where
  -- The wire's own alphabet, not Unicode's: a loop prefix is `0`-`7` and
  -- nothing else ever appears there.
  isDigit c = c >= '0' && c <= '9'
