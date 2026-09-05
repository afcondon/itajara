-- | What the looper's controls *mean*, with no controller in the room.
-- |
-- | Everything here was lifted out of `Data.Looper.Banks` on 2026-09-04, and
-- | the reason is the one `DESIGN-HARVEST.md` §6 gives: the vocabulary of the
-- | machine — a `Duty`, the `Subject` it is about, the sections a surface can
-- | show (`BankSlot`), the ladders a step climbs — lived in the file that lays
-- | out MC6 footswitches, so nothing could speak the looper's language without
-- | importing a footswitch box. Now `Banks` maps switches to duties, the
-- | Twister maps encoders to duties, and a web button maps a click to a duty,
-- | and none of them is the definition of one.
-- |
-- | **Nothing in here may import `Data.MC6.*`, `Data.Twister` or Halogen.**
-- | That is the whole property; the module is otherwise the same text it was.
-- |
-- | `BankSlot` stays, and deliberately: the "banks" are the information
-- | architecture of the looper's controls — the loops, one loop, its config,
-- | quantise, speed, grab, pan — and the MC6 layout is one projection of that.
-- | A page with no pedal can still be organised the same way.
module Data.Looper.Duty
  ( BankSlot(..)
  , allSlots
  , slotIndex
  , slotFromIndex
  , slotName
  , slotId
  , shortSlot
  , Jump(..)
  , Duty(..)
  , Subject(..)
  , dutyLabel
  , dutyName
  , levelWord
  , onOff
  , Rung
  , ladderLine
  , nextRung
  , rungWord
  , onRung
  , chanceLadder
  , stepChance
  , chanceWord
  , fadeLadder
  , stepFade
  , fadeWord
  , decayLadder
  , stepDecay
  , decayWord
  , rateWord
  , placeWord
  ) where

import Prelude

import Data.Array as Array
import Data.Int as Int
import Data.Number as Number
import Data.String (joinWith)
import Data.Maybe (Maybe(..), fromMaybe, maybe)

-- | The banks, as roles rather than numbers. Which MC6 bank each lands on is a
-- | deployment question (`banks` takes a base); which bank a press came from is
-- | not, and that is what travels in the CC.
data BankSlot
  = LoopBank
  -- | One loop's verbs, whichever loop you came from. See `own LoopPage`.
  | LoopPage
  | ConfigBank
  | QuantiseBank
  | SpeedBank
  -- | The two loops the six loop switches cannot reach, and a way to fill them
  -- | from something that is not a guitar. See `own GrabBank`.
  | GrabBank
  | PanBank

derive instance Eq BankSlot
derive instance Ord BankSlot

allSlots :: Array BankSlot
allSlots =
  [ LoopBank, LoopPage, ConfigBank, QuantiseBank, SpeedBank, GrabBank, PanBank ]

-- | **Seven is the last one that fits.** The CC block is `16 * (index + 1)`, so
-- | Pan sits at 112 and its twelfth switch is 123 — four short of the 127 a
-- | seven-bit value stops at. An eighth bank would put switches above 128,
-- | where they stop being data and start being status bytes, and the frame
-- | carrying them would truncate. The byte-range test in `test/Main` is what
-- | would catch it, and it would catch it as a wall rather than as a warning.
slotIndex :: BankSlot -> Int
slotIndex = case _ of
  LoopBank -> 0
  LoopPage -> 1
  ConfigBank -> 2
  QuantiseBank -> 3
  SpeedBank -> 4
  GrabBank -> 5
  PanBank -> 6

slotFromIndex :: Int -> Maybe BankSlot
slotFromIndex = case _ of
  0 -> Just LoopBank
  1 -> Just LoopPage
  2 -> Just ConfigBank
  3 -> Just QuantiseBank
  4 -> Just SpeedBank
  5 -> Just GrabBank
  6 -> Just PanBank
  _ -> Nothing

-- | The bank's name on the device's screen. Eight characters, like a label.
slotName :: BankSlot -> String
slotName = case _ of
  LoopBank -> "Loops"
  -- Not "Loop 3". The device cannot be relabelled fast enough to track which
  -- loop is in hand — an upload is well over a second — so the pedal names the
  -- *page* and the computer names the loop. The standing division of labour.
  LoopPage -> "The Loop"
  ConfigBank -> "Loop Cfg"
  QuantiseBank -> "Quantise"
  SpeedBank -> "Speed"
  GrabBank -> "Grab"
  PanBank -> "Pan"

slotId :: BankSlot -> String
slotId = case _ of
  LoopBank -> "loops"
  LoopPage -> "loop"
  ConfigBank -> "config"
  QuantiseBank -> "quantise"
  SpeedBank -> "speed"
  GrabBank -> "grab"
  PanBank -> "pan"

data Jump = ToSlot BankSlot | ToBoard

derive instance Eq Jump

-- | What a switch is *for*.
-- |
-- | **The one table.** Three things have to agree about every switch: the eight
-- | characters the MC6 prints on its screen, the words the app shows for the
-- | six that have no screen, and the command the press actually sends. They
-- | used to be two tables keyed by a switch index — the layout said switch 9
-- | was "Clear", `Data.Looper.Machine` said switch 9 sent `c`, and nothing
-- | linked them but the number. A layout edit that moved Clear would have left
-- | a switch labelled one thing and doing another, and nothing would have
-- | failed to compile.
-- |
-- | So a switch carries a value rather than a string, and the label, the long
-- | name and the meaning are all *functions of that value*. Relabelling and
-- | rewiring stop being separate acts. It is the same move as `Emit` in
-- | `Data.MC6.Model`: closed alternatives are an ADT, never a string.
data Duty
  -- | One of the six loops. The index is the loop, not the switch, because on
  -- | the loop bank they coincide and everywhere else they must not.
  -- |
  -- | **A place, not a verb.** This used to be seven verbs in a trenchcoat: a
  -- | tap meant record, or close, or overdub, or cancel an arm, or fire, or
  -- | stop, or start, depending on what the daemon last reported — and nothing
  -- | underfoot said which. Now it selects the loop and opens its page, where
  -- | each of those has its own switch with its own name on the screen.
  -- |
  -- | The cost is a second press to start a take on a loop you are not already
  -- | standing on. It buys back more than it costs: the switch carries one
  -- | gesture, so it is on `ActionPress` and reports the instant your foot
  -- | lands, where before every loop press waited out the double-tap window.
  = SelectLoop Int
  -- | Start writing, stop writing, or take back a wait — whichever the loop is
  -- | ready for.
  -- |
  -- | **Still context-dependent, and honestly so.** `r` is one command in the
  -- | daemon and it means "toggle the write head": it opens a first recording,
  -- | closes one, opens and closes an overdub, and cancels a loop that is
  -- | listening. Splitting that across four switches would be splitting a thing
  -- | the engine does not split.
  | RecordLoop
  -- | Add a pass to what is already there, bringing a stopped loop back first.
  -- |
  -- | Overdubbing something you cannot hear is a way to record a mistake twice,
  -- | which is why this unmutes rather than refusing.
  | OverdubLoop
  -- | Stop it, or start it again — and fire it, if it is a one-shot.
  -- |
  -- | The one-shot case is not an overload sneaking back in. A one-shot is
  -- | silent between passes *by definition*, so it has no playing and stopped to
  -- | move between; firing is the only thing this switch could mean there.
  | Transport
  -- | Wait for a sound instead of starting on the press.
  -- |
  -- | `lev1` and then `r`, which is the mode plus the gesture in one press —
  -- | because "start when I play" is something you decide in the moment, not
  -- | something you go to a config bank to arrange. The mode stays visible in
  -- | Modes and on screen; this is the shortcut, not a second source of truth.
  | ArmLoop
  -- | Deeper into the family. Labelled with the destination's own name.
  | Enter BankSlot
  -- | Up, or out. Labelled with where it goes, so "< Config" and "< Board"
  -- | cannot drift from the jump the device was programmed with.
  | Back Jump
  | StopAll
  | Undo
  | ClearLoop
  -- | Half speed, or back to unity — one switch, because underfoot the pair is
  -- | one idea and the knob that can express the rest of the range is on the
  -- | Twister.
  -- |
  -- | **Anything slow goes back to one**, not just a half: set the loop to a
  -- | quarter from the encoder and this is still the way out, which is what you
  -- | want from a footswitch you press without looking.
  | HalfSpeed
  | SaveTake
  -- | The whole set, rendered — see `Data.Looper.Verb.ExportSet` for what that
  -- | means and how it differs from Save.
  | ExportSet
  | ClickToggle
  -- | One layer in or out of the mix, by the number the slot shows. No switch
  -- | carries this and no encoder does yet; it is a checkbox on the page, and
  -- | it goes through the machine like everything else so that it is one
  -- | route in.
  | LayerOn Int Boolean
  -- | The editing duties: the window's two ends, the whole loop again, a
  -- | shift of where a pass starts, and a request for the waveform. From the
  -- | Edit panel only; no switch or encoder carries them.
  | WindowIn Int
  | WindowOut Int
  | ClearWindow
  | ShiftStart Int
  | AskPeaks Int
  -- | Copy the layers of another loop — all of them, or one — onto this one.
  -- | A drag from one slot to another; the machine refuses unless the target
  -- | is empty, because what a copy onto a loop that holds something would
  -- | mean is a question to design, not to answer by accident.
  | CopyLoop Int
  | CopyLayer Int Int
  -- | One layer sounds and the rest are parked — the module's own way, the
  -- | Layer knob. A lone layer stays on.
  | SoloLayer Int
  -- | A layer's own window (layer, in, out), its clearing, and a duplicate of
  -- | it as a new layer of the same loop.
  | LayerWindow Int Int Int
  | ClearLayerWindow Int
  | DupLayer Int
  | Reverse
  | Pendulum
  -- | One pass per trigger, rather than turning for ever.
  -- |
  -- | **The one gesture that moves a loop's zero.** Everywhere else in this rig
  -- | a loop's position is fixed at the moment it was recorded — that is what
  -- | phase-locking means, and it is why stopping a loop and starting it again
  -- | puts it back where it would have been rather than where it began. A
  -- | one-shot has to start from the top, so firing it moves `origin`, and a
  -- | loop that can be fired has given up its place in the phase-locked set.
  -- |
  -- | Which is precisely why it is a mode and not a gesture: losing your grid
  -- | should be something you switch on, never something a footswitch does to
  -- | you on a bank you did not mean to be standing on.
  | OneShot
  -- | Wait for a sound instead of starting on the press.
  -- |
  -- | Free, because the pre-roll ring is already running: the recording begins
  -- | fifty milliseconds *before* the threshold was crossed, so the attack that
  -- | crossed it is in the take rather than clipped off the front of it. The
  -- | same trick as claiming the past and as un-doing gesture latency — the
  -- | third thing the ring has paid for.
  | LevelArm
  -- | Step this loop's chance of sounding down the ladder, wrapping at the end.
  -- |
  -- | **A value on one switch, which is what the config family kept wanting.**
  -- | Chance had a bank of five to itself before it worked at all. It is a value
  -- | chosen from a few, like speed and pan, so five switches is the shape it
  -- | *looks* like it wants — but a Chance bank reached from the Modes bank
  -- | reached from Loop Cfg is four deep, and three was already one too many.
  -- |
  -- | So it steps instead, and the step is computed here from what the engine
  -- | last reported rather than counted on the device. That is the difference
  -- | between this and the MC6's own scroll counters: the device would keep its
  -- | own position, and a device that keeps state is the one thing here that
  -- | cannot be told it is wrong. The app is looking at the engine thirty times
  -- | a second.
  -- |
  -- | The pedal says "Chance" and the screen says which rung — the standing
  -- | division of labour, and the reason the board can be programmed once.
  | StepChance
  -- | Step how much of the wrap is crossfaded with what followed it.
  -- |
  -- | The other half of *store everything, flatten late*: the frames recorded
  -- | after the loop closed were kept rather than trimmed, and this is what they
  -- | were kept for. A first recording is cut, so the frame after the last one
  -- | is not the frame that followed it when it was played — the join is a step
  -- | in the waveform. Arriving at the head through the continuation makes it
  -- | continuous, because the two are one performance either side of one
  -- | instant.
  -- |
  -- | Applied at playback, so it costs nothing to change and nothing to undo.
  | StepFade
  -- | Step how much a pass costs the material already there.
  -- |
  -- | **The parameter that separates Frippertronics from song looping.** Two
  -- | Revoxes with the second one feeding back below unity is this number, and
  -- | so is what a tape echo does to its repeats. Without it every layer plays
  -- | at full for ever and the only shape a loop can have is the one it was
  -- | given.
  -- |
  -- | Per layer, counted from each one's own birth, so new material enters at
  -- | full while everything underneath recedes — which a single feedback gain
  -- | cannot do, because it destroys as it goes and has no idea how old
  -- | anything is. Here it is a resolution at playback: a loop faded to nothing
  -- | is still all there, and turning decay off brings it back.
  | StepDecay
  -- | Claim the recent past. **The one thing a pedal cannot do**, and the
  -- | reason for a sixty-second ring: you played something good and did not
  -- | hit record, so hit it afterwards. It had no footswitch at all until
  -- | now, while the switch labelled "Take" saved a WAV — two different takes,
  -- | and the wrong one had the fast slot.
  | ClaimPast
  -- | Put back the last undone layer. Free now that undo keeps what it
  -- | removes.
  | Redo
  -- | Bring every loop back, the counterpart of stopping them all.
  | StartAll
  -- | Start the beat and record a whole number of bars of it, in one press.
  -- |
  -- | **The one duty that reaches outside this rig.** Everything else here
  -- | ends at itajara; this begins by asking Link's transport to start, which
  -- | is the only cue an iPad app takes. Patterning, Xynthesizr and AUM all
  -- | follow Start/Stop Sync and none of them follow anything else the board
  -- | can send, so "play the beat" has to be said to the session rather than
  -- | to the app.
  -- |
  -- | The rest is ordinary: the grid on, the length declared, and record.
  -- | link-spike schedules the transport for the next bar line without moving
  -- | the beat grid, and a grid-quantised recording is waiting for that same
  -- | bar line — so the take opens on the drum machine's downbeat rather than
  -- | somewhere inside its first bar.
  -- |
  -- | On a loop that already holds something this is an overdub, because that
  -- | is what a recording is on a loop with material. Layering a hat over a
  -- | kick is therefore the same press twice and needs no switch of its own.
  | Grab Int
  -- | Link's transport, started or stopped, for the whole session.
  -- |
  -- | Only the stop is on a switch. The start is inside `Grab`, where it
  -- | belongs — starting the beat without recording it is a thing you would do
  -- | from the iPad, which is in your hands at that point anyway.
  -- |
  -- | **It stops Ableton too**, and everything else on the session. That is
  -- | not a leak in the abstraction, it is what a session transport is, and it
  -- | is why the long name says Link rather than saying iPad.
  | LinkPlay Boolean
  | ClearAll
  | Free
  -- | Quantised launch. The bar count is carried and does not yet do anything —
  -- | the engine's grid is the anchor loop's cycle, not a bar — so this is a
  -- | promise the meaning table has to keep honestly.
  | Grid Int
  | Rate Number
  | Place Int

  -- ## The verbs the CC table had and this one did not
  --
  -- Until 2026-08-25 there were two vocabularies: this one, reached by foot,
  -- and the CC table in `Data.Looper` reached by the page. `Multiply` lived
  -- only in the second — the MC6 loop family has never had a switch for it —
  -- so the *reference* surface could ask for something the machine had no word
  -- for. Folding the page onto the machine (`DESIGN-TWISTER` §4) means this
  -- type gains everything the CC table could express.

  -- | Extend by whole cycles while it runs, and close on the next press. Asks
  -- | "how many bars of this?" where `SpreadLoop` asks "how often?".
  | MultiplyLoop
  -- | The layer keeps its length and the loop grows around it, so the pass
  -- | sounds one cycle in `n`. Structural, instant and reversible; it records
  -- | nothing.
  | SpreadLoop Int
  -- | Move a spread layer one slot later in its cycle.
  | RotateLoop
  -- | Sound every cycle again — the way back from `SpreadLoop`, and the reason
  -- | spreading is safe to try mid-take.
  | DenseLoop
  -- | Let go of the length that `Undo` deliberately kept. The third of the
  -- | three erasures, and the one that is a between-takes decision.
  | ForgetLength
  -- | **Take the session tempo from this loop.**
  -- |
  -- | The other half of `SetBars`. That duty has three jobs and the ack says
  -- | which one you got — size an empty loop, declare the bar count of a
  -- | clockless anchor, resize something with material in it — and *declaring*
  -- | was reachable only with no clock, because with one there was nothing to
  -- | tell. There is now: link-spike answers `/link/set-tempo`.
  -- |
  -- | **The only duty in this vocabulary whose effect leaves the rig.**
  -- | Everything else here addresses a loop or the daemon; this one reaches
  -- | Ableton, purerl-tidal and the modular's tempo-relative rates, because
  -- | Link is a session rather than a clock we read. That is a reason for it to
  -- | be a deliberate press rather than a knob, not a reason to leave the click
  -- | fighting what you played.
  | TakeTempo
  -- | **Which input this loop records from.**
  -- |
  -- | Per loop and not per rig, and `ClaimPast` is the argument: the pre-roll
  -- | exists so you need not decide in advance, and a global input selector
  -- | would put that decision straight back in front of you. So every source
  -- | keeps its own ring and a loop says which it wants.
  | SetSource Int
  -- | Fold this loop's two channels at playback, which also turns its `pan`
  -- | from a balance into a placement. A playback decision, so nothing is lost
  -- | by trying it.
  | MonoToggle
  | Mono Boolean
  -- | Input monitoring. Global in the engine, like the click.
  | MonitorToggle

  -- ## Value duties
  --
  -- The same parameters as `StepChance`, `StepFade` and `StepDecay`, carrying a
  -- value rather than a direction. **The step duties are defined in terms of
  -- these** (`Data.Looper.Machine`): a ladder is a *rendering* of a parameter
  -- for a surface that can only press, and the value is the parameter. One
  -- place where chance becomes a command means a footswitch and a knob cannot
  -- disagree — the argument `Data.Looper.Verb` makes about spellings, one level
  -- up.
  --
  -- `Rate` and `Place` above are the same family; they got here first, put
  -- there by the Speed and Pan banks.

  -- | The two global flags, as values rather than flips.
  -- |
  -- | `ClickToggle` and `MonitorToggle` above are defined in terms of these, the
  -- | same way the `Step*` family is defined in terms of `Chance` and friends. A
  -- | footswitch programmed as an MC6 *native toggle* sends 127 and 0 on
  -- | alternate presses and must **set** from that value — flipping there would
  -- | flip twice per press — while a momentary switch has no value to carry and
  -- | must ask what the current one is. Both, one meaning.
  | Click Boolean
  | Monitor Boolean

  -- | Whether this loop waits for the grid, as a value — and the flip that a
  -- | surface with no value to send needs.
  -- |
  -- | **`Grid n` and `Free` are the MC6's rendering of these**, and delegate to
  -- | them the way `StepChance` delegates to `Chance`. They had to be: a control
  -- | whose press always sets *on* cannot be pressed twice, which is fine on a
  -- | bank where `Free` sits beside it with a switch of its own and wrong
  -- | anywhere else. The Twister found it — one encoder for a flag means the
  -- | encoder has to flip.
  | OnGrid Boolean
  | GridToggle

  -- | Make this loop a tape, or stop being one. The flip, for a control with
  -- | no value to send; `Revox` is the form with.
  | RevoxToggle
  | Revox Boolean
  -- | Thread an empty tape of this many seconds.
  | Blank Number
  -- | What a Revox pass leaves of what was under it, in decibels.
  | Feedback Number
  -- | How much top a Revox pass keeps, in hertz.
  | Tone Number
  -- | How many layers should be live — the undo stack as a **position** rather
  -- | than as two buttons.
  -- |
  -- | Undo and Redo are one axis and had two controls, which on a device whose
  -- | encoders report an absolute position is a waste of the thing it is good
  -- | at. `perform` compares this with what the daemon reports and sends the
  -- | difference as `u`s or `y`s, so the knob is a scrub through the stack and
  -- | the ring shows how deep you are.
  -- |
  -- | Nudge-proof by arithmetic rather than by luck: eight layers across 128
  -- | steps is sixteen steps a layer, and the press guard only has to cover two.
  | Layers Int
  -- | The rig's level-arm threshold, in decibels. Not per loop.
  | ArmLevel Number
  -- | This loop's level, in decibels. Zero is unity, -60 is silence.
  | Level Number
  -- | How often a pass sounds, as a probability. `1.0` is always.
  | Chance Number
  -- | How much of the wrap is crossfaded, in milliseconds. Zero is a hard join.
  | Fade Number
  -- | How much a pass costs what is already there, in decibels. Zero holds.
  | Decay Number

  -- | **How many bars this loop is**, and the only place metre reaches a loop.
  -- |
  -- | One duty doing three jobs, decided by what the loop already is and said
  -- | out loud in the ack every time:
  -- |
  -- | * **empty** — sizes it, and the next recording closes itself there
  -- |   instead of waiting for a second press;
  -- | * **the first loop with no clock** — *declares* what you played. `4` on a
  -- |   four-bar phrase makes the bar a quarter of it and touches no audio,
  -- |   which is the only way a clockless session gets a loop shorter than its
  -- |   first take;
  -- | * **anything else with material in it** — resizes, and the layers keep
  -- |   their own lengths inside the new one.
  -- |
  -- | The three are one control because they are one question — *how many bars
  -- | is this* — asked of a loop in three states. Splitting them would make the
  -- | player decide which verb they meant, which is a decision about the engine
  -- | rather than about the music.
  | SetBars Int
  -- | How often the newest layer sounds, in cycles of its own length. `1` is
  -- | every time round, which is what `Dense` asks for by another name.
  | Every Int
  -- | Which of those cycles it lands on. One-based, and wraps.
  | PlaceAt Int
  -- | What a launch waits for, in beats. **Rig-wide**, like the click and the
  -- | arm threshold — `-1` is a bar and is the default, `0` is none.
  -- |
  -- | Beats rather than fractions of a bar so it means the same thing in 3/4 as
  -- | in 4/4: a quantum of three does not turn "one beat" into a third of a
  -- | bar, it stays a beat.
  | Launch Int
  -- | Named, unimplemented, and still occupying its switch. Carries what it
  -- | would be called and what it is waiting for, so a press answers with the
  -- | reason rather than with silence.
  | NotYet String String
  | Nothing_

derive instance Eq Duty

-- | Which loop a duty is about.
-- |
-- | **An argument to `Data.Looper.Machine.perform`, not a field of `Duty`**, and
-- | that is the whole point of it: with the subject explicit there is no way to
-- | send a per-loop verb without having said which loop, because the compiler
-- | asks. The bug it kills is a class rather than an instance — `SaveTake`
-- | wrote loop 1 whatever the board was focused on for as long as the CC table
-- | rendered bare, and every other per-loop verb on that table had the same
-- | fault waiting.
-- |
-- | The MC6 always passes `Focused`: six switches cannot name eight loops in a
-- | parameter gesture. The Twister passes `OnLoop i` for its per-loop encoders,
-- | because there every loop has its own knob and turning one must not steal
-- | focus from another.
data Subject
  = Focused
  | OnLoop Int

derive instance Eq Subject

-- | The eight characters the MC6 prints. Refused rather than truncated by
-- | `Data.MC6.Model.shortName` downstream, so a label that will not fit is a
-- | build-time problem and not a mystery on the device.
dutyLabel :: Duty -> String
dutyLabel = case _ of
  SelectLoop i -> "Loop " <> show (i + 1)
  RecordLoop -> "Record"
  OverdubLoop -> "Overdub"
  Transport -> "Stop/Go"
  ArmLoop -> "Arm"
  Enter slot -> slotName slot
  Back (ToSlot slot) -> "< " <> shortSlot slot
  Back ToBoard -> "< Board"
  StopAll -> "Stop All"
  StartAll -> "Start All"
  Undo -> "Undo"
  Redo -> "Redo"
  ClearLoop -> "Clear"
  HalfSpeed -> "Half Spd"
  ClearAll -> "Clear All"
  ClaimPast -> "Capture"
  SaveTake -> "Save"
  ExportSet -> "Export"
  ClickToggle -> "Click"
  Reverse -> "Reverse"
  Pendulum -> "Pendulum"
  -- Not "Grab 4". The page already says Grab on the device's own header, and
  -- "Grab 4" sitting a switch away from "Loop 4" is two different fours side
  -- by side on a screen you read with your feet.
  Grab n -> show n <> " bars"
  LinkPlay on -> if on then "Play" else "Halt"
  OneShot -> "One Shot"
  LevelArm -> "Listen"
  StepChance -> "Chance"
  StepFade -> "Fade"
  StepDecay -> "Decay"
  Free -> "Free"
  Grid n -> show n <> (if n == 1 then " Bar" else " Bars")
  Rate r -> "x " <> rateWord r
  Place p -> placeWord p
  MultiplyLoop -> "Multiply"
  SetBars _ -> "Bars"
  Every _ -> "Every"
  PlaceAt _ -> "On"
  Launch _ -> "Launch"
  SpreadLoop _ -> "Spread"
  RotateLoop -> "Shift"
  DenseLoop -> "Dense"
  ForgetLength -> "Length"
  TakeTempo -> "Tempo"
  SetSource _ -> "Input"
  MonoToggle -> "Mono"
  Mono _ -> "Mono"
  MonitorToggle -> "Monitor"
  -- The value goes in `dutyName`, not here. Eight characters cannot hold
  -- "Chance 3 in 4", and these three never reach an MC6 switch anyway — they
  -- are what the knobs and the page send.
  Click _ -> "Click"
  Monitor _ -> "Monitor"
  OnGrid _ -> "Grid"
  GridToggle -> "Grid"
  RevoxToggle -> "Revox"
  Revox _ -> "Revox"
  Blank _ -> "Tape"
  Feedback _ -> "Feedback"
  Tone _ -> "Tone"
  Layers _ -> "Layers"
  ArmLevel _ -> "Listen at"
  Level _ -> "Level"
  Chance _ -> "Chance"
  Fade _ -> "Fade"
  Decay _ -> "Decay"
  NotYet l _ -> l
  Nothing_ -> ""
  LayerOn l on -> "Lyr " <> show l <> (if on then "+" else "-")
  WindowIn _ -> "In"
  WindowOut _ -> "Out"
  ClearWindow -> "Whole"
  ShiftStart _ -> "Shift"
  AskPeaks _ -> "Peaks"
  CopyLoop s -> "Copy L" <> show (s + 1)
  CopyLayer s k -> "Copy L" <> show (s + 1) <> "/" <> show k
  SoloLayer k -> "Solo " <> show k
  LayerWindow k _ _ -> "Win " <> show k
  ClearLayerWindow k -> "Whole " <> show k
  DupLayer k -> "Dup " <> show k

-- | Twenty-four characters, for the device's long name and for reporting a
-- | press the app did not expect as words rather than as a CC number.
dutyName :: Duty -> String
dutyName = case _ of
  SelectLoop i -> "Loop " <> show (i + 1)
  RecordLoop -> "Record, or close what is"
  OverdubLoop -> "One more pass over it"
  Transport -> "Stop it, or set it going"
  ArmLoop -> "Start on the next note"
  Enter ConfigBank -> "Set up this loop"
  Enter slot -> "Set " <> slotName slot
  Back (ToSlot slot) -> "Back to " <> slotName slot
  Back ToBoard -> "Leave the looper"
  StopAll -> "Stop every loop"
  StartAll -> "Every loop from the top, together"
  Undo -> "Undo the last layer"
  Redo -> "Put the layer back"
  ClearLoop -> "Clear the chosen loop"
  HalfSpeed -> "Half speed or back to 1"
  ClearAll -> "Clear every loop"
  ClaimPast -> "Claim what just happened"
  SaveTake -> "Save the take to disk"
  ExportSet -> "Render every loop to its own WAV"
  ClickToggle -> "Click on or off"
  Reverse -> "Play the loop backwards"
  Pendulum -> "Forward, then back"
  Grab n -> "Grab " <> show n <> " bars into it"
  LinkPlay on -> (if on then "Start" else "Stop") <> " the Link transport"
  OneShot -> "One pass, then silence"
  LevelArm -> "Start when you play"
  StepChance -> ladderLine chanceLadder
  StepFade -> ladderLine fadeLadder
  StepDecay -> ladderLine decayLadder
  Free -> "Free length and launch"
  Grid n -> "Round to " <> show n <> (if n == 1 then " bar" else " bars")
  Rate r -> rateWord r <> " speed"
  Place p -> placeWord p <> " in the field"
  MultiplyLoop -> "Extend by whole cycles"
  SetBars n -> show n <> (if n == 1 then " bar long" else " bars long")
  Every n -> if n == 1 then "Sounds every time round"
             else "Sounds once every " <> show n
  PlaceAt n -> "On slot " <> show n
  Launch n -> case n of
    -1 -> "Launch on the bar"
    0 -> "Launch straight away"
    b -> "Launch on " <> show b <> (if b == 1 then " beat" else " beats")
  SpreadLoop n -> "Sound one cycle in " <> show n
  RotateLoop -> "Move it one slot later"
  DenseLoop -> "Sound every cycle again"
  ForgetLength -> "Let go of the length"
  TakeTempo -> "Take the tempo from this loop"
  SetSource n -> "Record from input " <> show n
  MonoToggle -> "Fold to mono, or keep the sides"
  Mono on -> if on then "Folded to mono" else "Two channels"
  MonitorToggle -> "Input monitoring"
  Click on -> "Click " <> onOff on
  Monitor on -> "Monitoring " <> onOff on
  OnGrid on -> if on then "Waits for the grid" else "Free length and launch"
  GridToggle -> "Wait for the grid, or not"
  RevoxToggle -> "Tape mode, or layers"
  Revox on -> if on then "A tape: undo is gone" else "Record in layers again"
  Blank secs -> "Thread " <> show (Int.round secs) <> " s of tape"
  Feedback db -> "A pass leaves " <> levelWord db
  Tone hz -> if hz >= 20000.0 then "Every pass as bright"
             else "Keeps " <> show (Int.round (hz / 100.0) * 100) <> " Hz"
  Layers n -> "Keep " <> show n <> (if n == 1 then " layer" else " layers")
  ArmLevel db -> "Starts at " <> show (Int.round db) <> " dBFS"
  Level db -> "Plays at " <> levelWord db
  Chance p -> "Sounds " <> chanceWord p
  Fade ms -> "Wraps " <> fadeWord ms
  Decay db -> "Decays " <> decayWord db
  NotYet l _ -> l
  Nothing_ -> ""
  LayerOn l on -> "Layer " <> show l <> (if on then " in the mix" else " parked")
  WindowIn f -> "Window starts at frame " <> show f
  WindowOut f -> "Window ends at frame " <> show f
  ClearWindow -> "The whole loop again"
  ShiftStart k -> "Start shifted by " <> show k <> " frames"
  AskPeaks n -> "Waveform in " <> show n <> " buckets"
  CopyLoop s -> "Copy every layer of loop " <> show (s + 1) <> " here"
  CopyLayer s k -> "Copy layer " <> show k <> " of loop " <> show (s + 1) <> " here"
  SoloLayer k -> "Layer " <> show k <> " alone"
  LayerWindow k i o -> "Layer " <> show k <> " plays " <> show i <> " to " <> show o
  ClearLayerWindow k -> "Layer " <> show k <> " plays whole"
  DupLayer k -> "Duplicate layer " <> show k

-- | A level in words. The daemon's own vocabulary — "full" and "silent" rather
-- | than "0.0 dB" and "-60.0 dB", because those are things a meter says and not
-- | things a person does.
levelWord :: Number -> String
levelWord db
  | db >= 0.0 = "full"
  | db <= -60.0 = "silent"
  | otherwise = show (Int.round db) <> " dB"

-- | For the duties whose whole content is a boolean.
onOff :: Boolean -> String
onOff on = if on then "on" else "off"

-- | A value a switch can step to, and what to call it.
-- |
-- | **The word lives beside the value**, rather than in a second function keyed
-- | by it. Three things read a ladder: the step a press takes, what the screen
-- | says, and — through the step — what the engine is told. Keeping them in one
-- | table is the same move as `Duty` itself.
-- | `word` is what the screen says; `tick` is the same thing squeezed small
-- | enough that the whole ladder fits in the twenty-four characters the pedal
-- | flashes on a press.
type Rung = { value :: Number, word :: String, tick :: String }

-- | The whole ladder on one line, for the pedal's long name.
-- |
-- | **A stepper cannot say where it is, so it should at least say where it can
-- | go.** The MC6 has one static line per switch and no way to update it from
-- | the device, so a stepper's long name was a description — "How often it
-- | plays" — which is the one thing the player already knows from the label
-- | underfoot. Listing the rungs at least tells you what the presses will do
-- | and in what order, which is what you want when your hands are busy; where
-- | you *are* on it is the computer's job.
ladderLine :: Array Rung -> String
ladderLine = joinWith " " <<< map _.tick

-- | The next rung the switch walks to, wrapping at the end.
-- |
-- | **One rule for every ladder**, rather than a step function per parameter:
-- | *the rung after the one you are standing on, and back to the first if there
-- | is none.* A value that is on no rung — only reachable by typing at the
-- | daemon — also goes to the first, because guessing which rung a number
-- | nobody chose is nearest to is a guess the player would have to learn.
-- |
-- | The wrap matters more than it looks: a ladder you cannot get off is worse
-- | than one that takes five presses, and five presses on a switch you are
-- | already standing over is nothing.
nextRung :: Array Rung -> Number -> Number
nextRung rungs now = case Array.findIndex (\r -> onRung r.value now) rungs of
  Just i -> maybe first _.value (Array.index rungs (i + 1))
  Nothing -> first
  where
  first = maybe 0.0 _.value (Array.head rungs)

-- | This ladder's own word for a value, when it has one.
rungWord :: Array Rung -> Number -> Maybe String
rungWord rungs v = _.word <$> Array.find (\r -> onRung r.value v) rungs

-- | How close counts as being on a rung. Wide enough to survive a round trip
-- | through the wire as text, narrow enough that no two rungs could claim the
-- | same reading.
onRung :: Number -> Number -> Boolean
onRung a b = Number.abs (a - b) < 1.0e-4

-- | How often a pass sounds. Rarest last, so stepping makes it rarer.
-- |
-- | The engine takes any probability from zero to one and has no opinion about
-- | which are worth a press. That is a question about feet, and this is where
-- | feet are answered.
chanceLadder :: Array Rung
chanceLadder =
  [ { value: 1.0, word: "always", tick: "all" }
  , { value: 0.75, word: "3 in 4", tick: "3:4" }
  , { value: 0.5, word: "1 in 2", tick: "1:2" }
  , { value: 0.25, word: "1 in 4", tick: "1:4" }
  , { value: 0.125, word: "1 in 8", tick: "1:8" }
  ]

stepChance :: Number -> Number
stepChance = nextRung chanceLadder

chanceWord :: Number -> String
chanceWord p = fromMaybe (show (Int.round (p * 100.0)) <> "%") (rungWord chanceLadder p)

-- | How much of the wrap is crossfaded with what followed it, in milliseconds.
-- |
-- | **Off first, and off by default.** A fade changes the first few milliseconds
-- | of every cycle — it has to, that is what makes the join continuous — and a
-- | looper that quietly softened every downbeat would be doing something nobody
-- | asked for. Ten is under a drum transient; a hundred is a real dissolve.
fadeLadder :: Array Rung
fadeLadder =
  [ { value: 0.0, word: "hard", tick: "hard" }
  , { value: 10.0, word: "10 ms", tick: "10" }
  , { value: 25.0, word: "25 ms", tick: "25" }
  , { value: 50.0, word: "50 ms", tick: "50" }
  , { value: 100.0, word: "100 ms", tick: "100" }
  ]

stepFade :: Number -> Number
stepFade = nextRung fadeLadder

fadeWord :: Number -> String
fadeWord ms = fromMaybe (show (Int.round ms) <> " ms") (rungWord fadeLadder ms)

-- | How much a pass costs what is already there, in decibels.
-- |
-- | **Hold first, and hold by default**, because holding is what a looper has
-- | always done and every loop recorded before this existed was recorded
-- | expecting it.
-- |
-- | The rungs are chosen by how long they take rather than by round numbers: at
-- | one a pass a phrase is still there thirty passes later, which is Fripp's
-- | Soundscapes; at twelve it is gone in three, which is a slapback with
-- | delusions. Three and six are where most of the interesting ambience lives.
decayLadder :: Array Rung
decayLadder =
  [ { value: 0.0, word: "hold", tick: "hold" }
  , { value: -1.0, word: "-1 dB", tick: "1" }
  , { value: -3.0, word: "-3 dB", tick: "3" }
  , { value: -6.0, word: "-6 dB", tick: "6" }
  , { value: -12.0, word: "-12 dB", tick: "12" }
  ]

stepDecay :: Number -> Number
stepDecay = nextRung decayLadder

decayWord :: Number -> String
decayWord db = fromMaybe (show (Int.round db) <> " dB") (rungWord decayLadder db)

shortSlot :: BankSlot -> String
shortSlot = case _ of
  ConfigBank -> "Config"
  slot -> slotName slot

rateWord :: Number -> String
rateWord r
  | r == 0.25 = "1/4"
  | r == 0.5 = "1/2"
  | r == 1.5 = "1 1/2"
  | r == 2.0 = "2"
  | otherwise = "1"

placeWord :: Int -> String
placeWord p
  | p <= 10 = "Left"
  | p <= 52 = "L 50"
  | p <= 74 = "Centre"
  | p <= 116 = "R 50"
  | otherwise = "Right"
