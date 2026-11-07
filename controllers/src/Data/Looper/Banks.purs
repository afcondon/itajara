-- | The MC6 banks for the six-loop looper, and the namespace they speak in.
-- |
-- | This is the build-time artifact of `itajara-in-atlantis` §"The MC6 is a
-- | keyboard": one bank per `BankSlot`, generated here and uploaded once. There
-- | were seven, as many as the CC arithmetic below has room for; since
-- | 2026-11-07 there is one, the Loops bank (AC: "simplify everything by
-- | removing all the other looper banks too"), with room left for a second on
-- | the same channel. Nothing rewrites them live, because relabelling a switch costs a preset upload of well over
-- | a second and the state of a loop changes faster than that. What each loop
-- | is doing is shown on the computer; the MC6 supplies twelve labelled places
-- | to put a foot.
-- |
-- | ## Every press says where it came from
-- |
-- | A switch does not send "record" or "loop 3". It sends **which switch on
-- | which bank**, and the app decides what that means. The whole meaning table
-- | is then in one place that can be changed without touching the hardware, and
-- | — more importantly — the app never has to remember which bank the board is
-- | showing. It is told, on every press.
-- |
-- | That is worth the CC space. If a press only said "switch 3", the app would
-- | have to infer the bank from its own memory of the last bank change, and the
-- | first missed message would put the two out of step permanently, with every
-- | subsequent press doing something confidently wrong. Here a stale bank
-- | arrives as a CC that does not fit the app's expectation, which is a thing
-- | you can *notice*.
-- |
-- | The arithmetic is deliberately readable in a MIDI monitor:
-- |
-- | ```
-- | cc = 16 * (bank slot + 1) + switch index
-- | ```
-- |
-- | so bank slots start at 16, 32, 48, 64, 80, 96, 112 and a switch is the offset
-- | within the block — CC 51 is slot 2, switch D, at a glance and without a
-- | table. Four CCs per block go unused, which buys that legibility cheaply.
-- | CC 0-15 are left alone because CC 0 and 32 are bank select and the low
-- | numbers are where every other device's defaults live.
-- |
-- | ## A whole channel, nine by default
-- |
-- | The family speaks on one MIDI channel of its own, a setting
-- | (`SwitchChannel`) that defaults to nine. Why a whole channel, and why nine,
-- | is told at `defaultSwitchChannel`.
-- |
-- | Until 2026-11-07 PWYF also wrote a single-loop "transport" bank
-- | (`Data.Looper.looperBank` there, channel 13, through Itajara's pedal CCs).
-- | It was removed (AC: "we can absolutely remove that extra transport bank");
-- | this family is the looper's only MC6 bank.
-- |
-- | ## The device says which gesture, and the value says which
-- |
-- | **Tap, double tap and long press are recognised on the MC6, not here.**
-- | Measured on the device 2026-08-21 with the gesture probe in
-- | `Data.MC6.Diagnostics`, one CC per action, read off a MIDI listener:
-- |
-- | ```
-- | double tap   DoubleTap alone — no Release at all, three times out of three
-- | double tap   DoubleTapRelease alone — Release suppressed too
-- | long press   Press at press-down, then LongPress while held, and no Release
-- | press-only   Press at press-down, Release when the foot lifts, both instant
-- | window       under 414 ms: two presses that far apart read as two singles
-- | ```
-- |
-- | Which gives a **clean, mutually exclusive triple**: `Release` fires on a
-- | single tap and on nothing else, because a double takes `DoubleTapRelease`
-- | and a hold takes `LongPress`. Nothing has to be suppressed by hand and
-- | nothing overlaps.
-- |
-- | **The deferral is on the release, never on the press.** `Press` fires the
-- | instant the foot lands, on every switch, whatever else is bound to it. It is
-- | `Release` that has to be *decided* — is this a single tap's release, the
-- | first half of a double, or the end of a hold? — and so it is `Release` that
-- | waits, and that gets suppressed when the answer turns out to be something
-- | else. This is Morningstar's own advice to program the release on any switch
-- | that also carries a long press, arrived at from underneath. See
-- | `soleGesture` for the rule it implies and for what it buys.
-- |
-- | So the app stopped doing it. There was an `Data.Looper.Gestures`, a `Mealy`
-- | transducer timing the gap between a CC at 127 and the same CC at 0, and it
-- | was well made and it is gone. Its stated justification — the double-tap
-- | window should be a function of the grid, which the device cannot know — was
-- | real and never exercised: the window was a constant. What it cost was three
-- | failure modes the hardware does not have, all of which were met in one day:
-- | the **orphan release** (a bank jump on press means the release arrives from
-- | the bank you have already reached, so the pair never completes), the
-- | **phantom hold** (that orphan's timer firing a gesture nobody made), and the
-- | app and the board **disagreeing about a threshold** — 600 ms here against
-- | 700 there, so a press in the gap closed nothing and stranded a recording.
-- |
-- | ### The value carries the gesture
-- |
-- | The CC number still says which switch on which bank. The **value** says
-- | which of the three gestures it was:
-- |
-- | ```
-- | 127  tap        ActionRelease
-- |  64  double     ActionDoubleTapRelease
-- |   1  hold       ActionLongPress
-- | ```
-- |
-- | Three widely spaced values rather than three CC blocks, because the
-- | namespace is already spent — six banks of sixteen is ninety-six CCs, and
-- | tripling that does not fit. A monitor still reads it at a glance: CC 51
-- | value 64 is a double tap on slot 2, switch D.
-- |
-- | Decoding is exact rather than banded, which makes a board programmed by an
-- | older version *say so*: its release sends 0, which is on no gesture and gets
-- | logged instead of silently meaning something.
-- |
-- | ### A switch with no second meaning still answers a double
-- |
-- | This applies only to a switch that carries a *hold* but no double — one that
-- | is therefore on the release side. The device suppresses `Release` on a
-- | double tap whether or not anything is bound to it, so left alone a fumbled
-- | double would do **nothing at all**, which is a worse answer than doing it
-- | once.
-- |
-- | So `bindings` gives such a switch the *tap's* value on
-- | `ActionDoubleTapRelease`, and its jump with it. Two taps too close together
-- | come out as one tap, which is what the player meant. The table is untouched:
-- | `Duties.double` stays `Nothing`, the screen still says the switch has no
-- | second meaning, and the fallback lives in the one function that programs the
-- | device.
-- |
-- | A switch carrying **only** a tap needs none of this, because it is on
-- | `ActionPress` and two presses are simply two presses.
module Data.Looper.Banks
  ( SwitchChannel
  , defaultSwitchChannel
  , switchChannelFrom
  , switchChannelNumber
  , allSwitchChannels
  , switchStatus
  , switchesClaim
  , loopSwitches
  , switchCC
  , Gesture(..)
  , allGestures
  , gestureName
  , gestureAction
  , gestureValue
  , gestureFromValue
  , SwitchGesture
  , switchGesture
  , decodeSwitch
  , labelOf
  , mc6OwnSwitches
  , switchLetter
  , auxLegend
  , Duties
  , dutiesAt
  , dutyFor
  , soleGesture
  , firesAtPressDown
  , dutyAt
  , Face
  , face
  , faceSlot
  , faceLoopKey
  , boardRows
  , loopRows
  , switchLoops
  , grabLoops
  , grabSource
  , loopAtSwitch
  , switchForLoop
  , sendsTo
  , banks
  , module Data.Looper.Duty
  , module Data.Looper.Layout
  ) where

import Prelude

import Data.Array as Array
import Data.Looper.Duty
  ( BankSlot(..), allSlots, slotIndex, slotFromIndex, slotName, slotId
  , shortSlot, Jump(..), Duty(..), Subject(..), dutyLabel, dutyName, levelWord
  , onOff, Rung, ladderLine, nextRung, rungWord, onRung, chanceLadder, stepChance
  , chanceWord, fadeLadder, stepFade, fadeWord, decayLadder, stepDecay, decayWord
  , rateWord, placeWord
  )
import Data.Looper.Layout (nLoops)
import Helm.MC6.ControlBank (ControlBank, ControlBankSwitch, switchCount)
import Helm.MC6.Message as MC6Msg
import Helm.MC6.Types (MC6Action(..), MC6Message)
import Data.Maybe (Maybe(..), fromMaybe, maybe)

-- | The MIDI channel the MC6 uses to address the looper about its own
-- | switches: one setting, read by whoever writes the banks (PWYF) and by
-- | whoever answers them (the looper's process, PWYF's fallback), passed to
-- | both as a value. A newtype, and no bare constant, so nothing can address
-- | the family on a hard-coded nine while the setting says otherwise: the
-- | writer and the reader disagreeing is a board whose presses go nowhere.
newtype SwitchChannel = SwitchChannel Int

derive instance Eq SwitchChannel
derive instance Ord SwitchChannel

instance Show SwitchChannel where
  show (SwitchChannel n) = "(SwitchChannel " <> show n <> ")"

-- | **Nine, by default.** Taking a whole channel for "the MC6 talking to the
-- | looper about its own switches" keeps this namespace from ever colliding
-- | with a message meant for a pedal. It is a control plane, not a pedal.
-- |
-- | **Nine, because it was the only one left, and that was checked against the
-- | device rather than against a comment.** This first shipped on channel 16 on
-- | the strength of a note in PWYF's `Data.Looper` saying 9, 13 and 16 were
-- | free. Thirteen had since been taken by Itajara, and sixteen is
-- | **LoopyPro** — the device's own channel table says so, and its
-- | `sendToPort` is 2034 where every other channel is 2047, three ports masked
-- | off exactly as a channel routed to the iPad and nowhere else would be.
-- |
-- | The device's table, read from the August connect dump, is the authority:
-- | 1 MC6, 2 (Brothers), 3 MOOD, 4 Clean, 5 Hedra, 7 Flint, 8 Lex, 10 Iridium,
-- | 11 Riverside, 12 Mercury7, 14 Brig, 15 Habit, 16 LoopyPro — plus 6
-- | Lost + Found and 13 Itajara, which the app knows about and the device does
-- | not name. That leaves nine, alone.
-- |
-- | Note that the March backup has Habit and LoopyPro the other way round. They
-- | were swapped at some point between, which is precisely why a channel is not
-- | something to take on the word of a comment — and why, since 2026-11-07, it
-- | is a setting rather than a constant: a board laid out differently can move
-- | it without a rebuild. Changing it means writing the banks again and
-- | restarting the looper's process (`LOOPER_MC6_CHANNEL`).
defaultSwitchChannel :: SwitchChannel
defaultSwitchChannel = SwitchChannel 9

-- | A channel, if the number is one (1–16, as the MC6's editor and every MIDI
-- | monitor number them).
switchChannelFrom :: Int -> Maybe SwitchChannel
switchChannelFrom n
  | n >= 1 && n <= 16 = Just (SwitchChannel n)
  | otherwise = Nothing

switchChannelNumber :: SwitchChannel -> Int
switchChannelNumber (SwitchChannel n) = n

-- | All sixteen, in order, for a picker.
allSwitchChannels :: Array SwitchChannel
allSwitchChannels = map SwitchChannel (Array.range 1 16)

-- | The status byte of a Control Change on this channel, for matching raw MIDI.
switchStatus :: SwitchChannel -> Int
switchStatus (SwitchChannel n) = 0xB0 + n - 1

-- | The name on Helm's hub held by whoever is answering these switches.
-- |
-- | The looper's own process claims it while it runs; a page that could also
-- | answer them (PWYF) reads it and stands back, so a press is acted on once.
-- | Declared rather than inferred: the claim ends with the holder's
-- | connection, so a process that dies hands the switches back by itself.
switchesClaim :: String
switchesClaim = "itajara:switches"

-- | How far apart two banks' CC blocks sit. Sixteen rather than twelve so the
-- | block boundary falls on a round number and the switch index is the low
-- | nibble of the CC.
stride :: Int
stride = 16

-- | How many of the eight loops the **MC6** can reach.
-- |
-- | **This used to say "must equal `N_LOOPS` in the daemon", and it deliberately
-- | no longer does.** Six was the MC6's number — what it has underfoot without
-- | an FS3X — and `N_LOOPS` was set to match it. With the web page as the
-- | reference surface and the Twister as a second controller (`DESIGN-TWISTER`
-- | §1, §5) that reasoning inverts: the loop count comes from the instrument,
-- | and the foot reaches what it can.
-- |
-- | So this is a fact about the *device*, and `nLoops` is the fact about the
-- | *instrument*. Restoring the equality would give the app two ghost loops it
-- | could see and never address.
loopSwitches :: Int
loopSwitches = 6

-- | Which CC a given switch sends.
switchCC :: BankSlot -> Int -> Int
switchCC slot i = stride * (slotIndex slot + 1) + i

-- | The three gestures, told apart by the device rather than by the app.
-- |
-- | A closed set, and closed for a hardware reason: these are the three the MC6
-- | resolves without overlap. It has more actions than this — `LongDoubleTap`,
-- | the scroll pair, the release halves of each — and every one of them is
-- | another thing to remember while standing on an unmarked switch. Three is
-- | already more than most switches use.
data Gesture = Tap | Double | Hold

derive instance Eq Gesture
derive instance Ord Gesture

allGestures :: Array Gesture
allGestures = [ Tap, Double, Hold ]

gestureName :: Gesture -> String
gestureName = case _ of
  Tap -> "tap"
  Double -> "double tap"
  Hold -> "long press"

-- | Which of the device's actions carries each gesture.
-- |
-- | **The measured triple**, and the reason this module can stop timing
-- | anything. `Release` fires on a single tap and on nothing else; a double
-- | takes `DoubleTapRelease` and suppresses the release; a long press takes
-- | `LongPress` and sends no release at all.
-- |
-- | The release halves rather than `Press` and `DoubleTap`, because the device
-- | is already deferring — it cannot know a tap is a tap until the window
-- | expires — so the earlier action buys nothing and, on a hold, would fire
-- | before the gesture had happened.
gestureAction :: Gesture -> MC6Action
gestureAction = case _ of
  Tap -> ActionRelease
  Double -> ActionDoubleTapRelease
  Hold -> ActionLongPress

-- | The CC value that says which gesture it was. See the module header.
gestureValue :: Gesture -> Int
gestureValue = case _ of
  Tap -> 127
  Double -> 64
  Hold -> 1

-- | Exact, so that a board programmed by an older version is *noticed* rather
-- | than half-understood: its release sends 0, which is on no gesture.
gestureFromValue :: Int -> Maybe Gesture
gestureFromValue = case _ of
  127 -> Just Tap
  64 -> Just Double
  1 -> Just Hold
  _ -> Nothing

-- | A recognised gesture on a known switch, as the app reads it off the wire.
-- |
-- | One message, complete. There is no half of this: the device does not tell
-- | the app a switch went down, so there is nothing to hold onto between
-- | messages and nothing that can be left holding it.
type SwitchGesture =
  { slot :: BankSlot
  , switch :: Int
  , gesture :: Gesture
  }

switchGesture :: BankSlot -> Int -> Gesture -> SwitchGesture
switchGesture slot switch gesture = { slot, switch, gesture }

-- | Read a CC as a gesture on a switch, or refuse it: the family's channel
-- | (the setting), then the channel the CC arrived on (1–16), its number and
-- | its value.
-- |
-- | Total and cheap, so the router can offer it every incoming CC and let the
-- | `Nothing` mean "not ours" — which is what keeps this namespace from having
-- | to be checked for at the call site as well as here.
decodeSwitch :: SwitchChannel -> Int -> Int -> Int -> Maybe SwitchGesture
decodeSwitch (SwitchChannel channel) arrivedOn ccNum value =
  if arrivedOn /= channel || ccNum < stride then Nothing
  else do
    slot <- slotFromIndex (ccNum `div` stride - 1)
    let sw = ccNum `mod` stride
    -- The four CCs above each block are the gap that makes the arithmetic
    -- legible; nothing sends them, so nothing should accept them either.
    if sw >= switchCount then Nothing
      else do
        gesture <- gestureFromValue value
        Just { slot, switch: sw, gesture }

-- | What a switch is labelled, for logging a press the app was not expecting.
-- |
-- | Reads the same table the device was programmed from, so a surprise can be
-- | reported as "Loop 3, hold to set up" rather than as a CC number — and if
-- | the two ever disagree, the disagreement is between the device and this
-- | table rather than between two copies of the table.
labelOf :: BankSlot -> Int -> Maybe String
labelOf slot i = map dutyName (dutyAt slot i)

-- | How many switches the MC6 has of its own, and so where the aux ones start.
-- |
-- | Numerically the same as `loopSwitches`, and not the same fact: one is a
-- | property of the unit, the other a choice about how many loops to run. They
-- | are written separately so that changing one does not silently change the
-- | other.
mc6OwnSwitches :: Int
mc6OwnSwitches = 6

-- | The letter printed on the board for a switch index.
switchLetter :: Int -> Maybe String
switchLetter = Array.index
  [ "A", "B", "C", "D", "E", "F", "G", "H", "I", "J", "K", "L" ]

-- | The switches past the MC6's own six, as letter and label, for a given bank.
-- |
-- | **The display's only honest source for these.** The MC6's LCD names its own
-- | six switches and stops there; G to L are FS3X footswitches with no display
-- | and no markings, so the app has to say what they do — and it has to say it
-- | *for the bank the board is actually showing*.
-- |
-- | That last part was learned the hard way. The legend was a hand-written copy
-- | of the loop bank's six, shown unconditionally, so with the board on the
-- | config bank the screen said J was Clear while J was End Stop. Pressing it
-- | reported something about leaving-state, which reads exactly like a switch
-- | wired to the wrong place — and cost an hour looking for a reversed mapping
-- | that was never reversed.
-- |
-- | Derived from `layout` rather than restated, because a legend that can
-- | disagree with the table the device was programmed from is a legend that
-- | eventually will.
auxLegend :: BankSlot -> Array { key :: String, what :: String }
auxLegend = map (\e -> { key: e.key, what: e.what }) <<< auxLegendAt

auxLegendAt :: BankSlot -> Array { index :: Int, key :: String, what :: String }
auxLegendAt slot = Array.catMaybes (map entry (Array.range mc6OwnSwitches (switchCount - 1)))
  where
  entry i = do
    key <- switchLetter i
    d <- dutyAt slot i
    if d == Nothing_ then Nothing else Just { index: i, key, what: dutyLabel d }

-- | The MC6's own six switches, in the rows they physically occupy.
-- |
-- | **The device numbers from the bottom**: A B C is the near row, under your
-- | toes, and D E F the far one. A view that lays out six things in board order
-- | must use this rather than index order, and it lives here because it is a
-- | fact about the hardware, not about any one screen — the loop grid got it
-- | wrong on its own, and the next screen to draw six switches would have got
-- | it wrong again, differently.
boardRows :: Array (Array Int)
boardRows = [ [ 3, 4, 5 ], [ 0, 1, 2 ] ]

-- | **The loops, four across and two down, on every surface at once.**
-- |
-- | This is the harmonisation, decided 2026-08-25, and it is a reversal worth
-- | stating plainly: the loop order used to be the *MC6's*, because the MC6 was
-- | once the only way to reach a loop and its switches number from the bottom.
-- | So loop 1 was switch A, on the near row, and the screen drew A B C below
-- | D E F to match.
-- |
-- | With eight loops and a 4×4 controller that tail is wagging the dog. The
-- | Twister's top-left encoder is loop 1 because that is what a grid of eight
-- | things obviously means, the page draws the same two rows of four, and the
-- | **pedal is the surface that has to fit in** — it covers the left three
-- | columns of both rows and simply lacks the fourth.
-- |
-- | Which is the shape an MC8 would fill exactly: four across, two down, loops
-- | 4 and 8 in the column the MC6 does not have. The layout is already waiting
-- | for it.
loopRows :: Array (Array Int)
loopRows = [ [ 0, 1, 2, 3 ], [ 4, 5, 6, 7 ] ]

-- | Which loop each of the MC6's six switches selects, by switch index.
-- |
-- | **The device numbers from the bottom** — A B C is the near row, under your
-- | toes, and D E F the far one — so the far row holds the *top* row of
-- | `loopRows` and the near row holds the bottom. Read it as the grid with its
-- | right-hand column removed and its rows swapped, because that is exactly
-- | what it is.
-- |
-- | ```
-- |   the page and the Twister      the MC6
-- |     1  2  3  4                    D  E  F      (far row)
-- |     5  6  7  8                    A  B  C      (near row)
-- | ```
-- |
-- | One table, both directions, so a switch and the letter printed beside a
-- | loop on screen cannot come to disagree. **Changing this needs the MC6
-- | re-uploaded**: the labels are compiled from it.
switchLoops :: Array Int
switchLoops = [ 4, 5, 6, 0, 1, 2 ]

loopAtSwitch :: Int -> Maybe Int
loopAtSwitch = Array.index switchLoops

-- | The two loops the six loop switches cannot reach, in the pedal's row order.
-- |
-- | **The fourth column, derived rather than written down.** `switchLoops`
-- | covers the left three columns of both rows; this is what is left — and it
-- | is `Array.last` of each row rather than the literal `[7, 3]` so that a
-- | change to `loopRows` moves both halves together instead of leaving one of
-- | them right and the other stale.
-- |
-- | Reversed for the same reason `switchLoops` reads bottom row first: the MC6
-- | numbers its switches from the near edge, so A is loop 8 and D is loop 4.
-- |
-- | Being out of the feet's reach is what made them the loops you set up
-- | rather than stomp, and material that arrives from the iPad is set up
-- | rather than stomped: a `Grab` lands on the grid in these two. (The Grab
-- | bank that aimed at them went on 2026-11-07; the loops and the duty stay.)
grabLoops :: Array Int
grabLoops = Array.reverse (Array.mapMaybe Array.last loopRows)

-- | The input a grab loop records from, **by the name the daemon gives it**.
-- |
-- | Not a number. `src` is one-based over whatever `--source` flags itajara was
-- | launched with, so `src3` means the iPad only for as long as the flags stay
-- | in their present order — and those flags live in Bosun's registry, which
-- | this app never reads. A number here would be a coupling to a launch
-- | argument, silent when it broke, and wrong in the way that records four bars
-- | of the wrong room.
-- |
-- | A name is the durable half of the same fact. The daemon reports its sources
-- | with their names in every snapshot, so the lookup is live: if nothing is
-- | called this, nothing is sent and nothing is claimed.
grabSource :: String
grabSource = "ipad"

-- | Which switch selects a loop, or `Nothing` for the two the pedal cannot
-- | reach.
switchForLoop :: Int -> Maybe Int
switchForLoop l = Array.findIndex (_ == l) switchLoops

-- | One switch, as a view is allowed to see it.
-- |
-- | Opaque, and that is the whole point: **a view cannot make one up.** Three
-- | times in one day a screen restated something this module already knew — the
-- | six aux labels, the A-to-F letters, the physical row order — and each time
-- | the copy was right when written and wrong later. Types cannot stop somebody
-- | typing a word into a div, but they can stop a second *table* existing, and
-- | a second table is what actually rots.
-- | **The `Switch` view type was removed 2026-08-27**, with `switchKey`,
-- | `switchLabel`, `switchDouble`, `switchHold`, `faceAux` and `faceName`.
-- |
-- | They existed for one caller: the row under the loops that named G to L,
-- | because those are FS3X switches with no markings and nothing else could
-- | say. The board panel says it now, from `dutiesAt` and `dutyLabel` — the
-- | same facts by a shorter road, and pressable, which the legend never was.
-- |
-- | Left as a note rather than silently: `auxLegend` above is the surviving
-- | answer to the same question and is what four tests hold to.

-- | What the board is showing, as everything a view may say about it.
-- |
-- | `Nothing` is a real state and not a missing value: leaving for the board
-- | bank leaves the looper family entirely, and a face that kept naming the
-- | loop bank's switches there would be describing a board nobody is standing
-- | on.
newtype Face = Face (Maybe BankSlot)

face :: Maybe BankSlot -> Face
face = Face

-- | Which bank the face is, when it is one of ours.
-- |
-- | The one way out of the newtype, and deliberately so. A view that wants to
-- | draw the whole board needs the slot; a view that only wants to label a loop
-- | asks `faceLoopKey` and never opens it at all.
faceSlot :: Face -> Maybe BankSlot
faceSlot (Face m) = m

-- | What to print on a loop's slot: the switch that reaches it, or its number
-- | when the board is somewhere that cannot reach it.
-- |
-- | The letters are only true on the loop bank. Off it (on the board's own
-- | banks), A is something else — so labelling the first loop "A" there points
-- | a foot at the wrong thing, which is the same fault the legend had.
-- | What to print on a loop: the letter of the switch that selects it when the
-- | board is showing the loop bank, its own number otherwise.
-- |
-- | **Through `switchForLoop`, not through the loop's index.** Those were the
-- | same number until the surfaces were harmonised and are not any more: loop 1
-- | is switch D. Reading the letter straight off the index would print "A"
-- | beside loop 1 and send the foot to the wrong corner of the pedal — and the
-- | two would still agree perfectly on loop 5, which is the kind of half-right
-- | that survives testing.
-- |
-- | Loops with no switch have no letter; their number is the honest answer.
faceLoopKey :: Face -> Int -> String
faceLoopKey (Face m) i = case m of
  Just LoopBank -> fromMaybe (show (i + 1)) (switchForLoop i >>= switchLetter)
  Nothing -> show (i + 1)

-- | What one switch does, across the three gestures it can carry.
-- |
-- | `double` and `hold` are `Maybe` and mostly `Nothing`, which is a design
-- | position rather than an omission twice over.
-- |
-- | **It is a memory burden.** An unmarked switch is remembered as a position,
-- | and three meanings on every position is three times as much to hold while
-- | standing on it. Filling the surface because it is there is how a pedal
-- | becomes something you have to think about.
-- |
-- | **And it used to be a latency.** A tap cannot be known to be a tap until
-- | the double-tap window expires, so any switch that *might* be
-- | double-tapped answers a few hundred milliseconds late. Making it a `Maybe`
-- | lets the recogniser resolve instantly for the switches that have no
-- | double — which is most of them — instead of taxing every press for a
-- | gesture only a few carry.
type Duties =
  { tap :: Duty
  , double :: Maybe Duty
  , hold :: Maybe Duty
  }

only :: Duty -> Duties
only d = { tap: d, double: Nothing, hold: Nothing }

alsoDouble :: Duty -> Duties -> Duties
alsoDouble d s = s { double = Just d }

alsoHold :: Duty -> Duties -> Duties
alsoHold d s = s { hold = Just d }

-- | Where a press sends the board, from the duty rather than from a second
-- | list. A bank jump is a *consequence* of what the switch is for.
dutyTap :: Duty -> Maybe Jump
dutyTap = case _ of
  Back j -> Just j
  -- **Choosing a loop no longer opens anything**, changed 2026-08-30. It used
  -- to: choosing was opening its page, one act and one press, back when the
  -- page was where the verbs were. The verbs are on the toolbar now, on every
  -- bank, so the jump was taking you off the Loops page for nothing and the
  -- next loop you wanted was two presses away instead of one.
  --
  -- The working shape is to stand on Loops and stay there: press a loop, record
  -- it on `I`, undo with a double if it was not the one, press another loop.
  -- Its page is a hold away for the two things only it has.
  SelectLoop _ -> Nothing
  _ -> Nothing

-- | Whether this switch carries one meaning and nothing else.
-- |
-- | **The condition for using `ActionPress`, and it is exact.** Measured on the
-- | device 2026-08-21: the MC6 fires `Press` at press-down, unconditionally and
-- | for every switch — a press-only switch and a switch with all four actions
-- | bound both reported the instant the foot landed. So the deferral was never
-- | on the press. It is on the **release**, which is the message that has to be
-- | *decided*: is this a single tap's release, the first half of a double, or
-- | the end of a hold?
-- |
-- | Which gives the rule with a sharp edge: **a switch may use `ActionPress` if
-- | and only if it carries exactly one gesture.** Add a second and `Press`
-- | becomes a message that fires before the device knows what you meant — the
-- | tap runs, and then the hold runs on top of it. That is Morningstar's own
-- | advice to program the release on any switch that also has a long press,
-- | arrived at from underneath.
-- |
-- | The switches that qualify get their press for nothing: no double-tap window
-- | to wait out, no lateness to hand the daemon, and no fallback needed either
-- | — two presses in quick succession are simply two presses, which is already
-- | what a stepper wants and is harmless for everything else here.
soleGesture :: Duties -> Boolean
soleGesture s = s.double == Nothing && s.hold == Nothing

-- | Whether the device reported this the moment the foot landed.
-- |
-- | Read by the app to decide what to tell the daemon about lateness, and
-- | derived from the same table the device was programmed from rather than
-- | agreed by hand — which is the whole reason the app can know it at all.
firesAtPressDown :: BankSlot -> Int -> Gesture -> Boolean
firesAtPressDown slot i g =
  g == Tap && maybe false soleGesture (dutiesAt slot i)

-- | What a switch is for under a given gesture, when it is for anything.
-- |
-- | `Nothing` is a real answer twice over: most switches carry no hold, and a
-- | double on a switch with no second meaning is a fumble the device programming
-- | already turned into a tap (see the module header) — so a `Double` reaching
-- | here unbound means the board and this table have fallen out of step, which
-- | is worth saying rather than covering for.
dutyFor :: Gesture -> Duties -> Maybe Duty
dutyFor g s = case g of
  Tap -> Just s.tap
  Double -> s.double
  Hold -> s.hold

-- | Where a press sends the board, if anywhere.
-- |
-- | **Read from the same table the device was programmed from**, which is the
-- | only way the app can follow a bank change it did not command. Most of the
-- | navigation in this family is the MC6's own: J's hold leaves for the board
-- | without a word to anyone, because that jump is a message stored *on the
-- | device*. The app sees the loop switch's CC and
-- | nothing else — so if it waits to be told, it is one press behind for ever,
-- | and the legend describes a bank nobody is standing on.
-- |
-- | It does not have to wait. It wrote the jumps; it can read them back.
sendsTo :: BankSlot -> Int -> Gesture -> Maybe Jump
sendsTo slot i g = do
  s <- dutiesAt slot i
  d <- dutyFor g s
  dutyTap d

-- | What a given switch on a given bank is for. The whole surface, in one
-- | lookup that everything else goes through.
dutiesAt :: BankSlot -> Int -> Maybe Duties
dutiesAt slot = Array.index (layout slot)

dutyAt :: BankSlot -> Int -> Maybe Duty
dutyAt slot i = _.tap <$> dutiesAt slot i

-- | The twelve switches of each bank.
-- |
-- | Switches 0-5 are the MC6's own A-F, which the device labels on its screen;
-- | 6-11 are two FS3X units, which have no screen and no markings.
layout :: BankSlot -> Array Duties
layout slot =
  Array.take mc6OwnSwitches (own slot <> Array.replicate mc6OwnSwitches (only Nothing_))
    <> toolbar slot

-- | The six unmarked switches, and they are the same six everywhere.
-- |
-- | **This is a rule about feet, not about screen space.** G to L have no
-- | markings and no display; the only way to use them is to remember them, and
-- | memory of a footswitch is memory of a *position*. A switch that clears a
-- | loop on one page and sets an end-state on the next cannot be learned at all
-- | — you would have to know which bank you were on before you could know what
-- | your foot was about to do, which is exactly the thing a footswitch exists
-- | to avoid.
-- |
-- | So the family reserves them, and every bank spends its own choices on A to
-- | F, where the MC6 prints a label. That costs each sub-bank half its options,
-- | and the cost turned out to be nearly free: the reverse speeds were made
-- | redundant when direction became the sign of speed, and the rest of what was
-- | up here — the every-N counts, the leaving-states, momentary — is not
-- | implemented and had nowhere honest to sit anyway.
-- |
-- | `Back` is the one that changes destination: from the loops it leaves the
-- | looper entirely, and from anywhere else it goes home to the loops. Same
-- | role, same place, one press, from any depth.
toolbar :: BankSlot -> Array Duties
toolbar slot =
  -- **The six the feet are best at, and nothing that duplicates the Twister.**
  --
  -- This used to be the six most-wanted looper duties, from a time when the
  -- MC6 was the whole surface. It is not any more: the Twister is on the board
  -- permanently, and Undo, Clear, Clear All, Capture, Save and the click all
  -- have encoders of their own on its first two pages. Keeping them here as
  -- well was not redundancy for safety, it was two places to learn and one of
  -- them slower.
  --
  -- What is left is what a *foot* is better at than a hand: the gestures that
  -- happen mid-phrase, where looking away is the cost. Arm and Record start
  -- takes; Reverse and half speed are the two that transform a loop in one
  -- press; Overdub adds a pass; the set stops and starts.
  --
  -- **The order is ergonomic and was measured with feet, not reasoned.** These
  -- are the FS3X switches, which are sloped and reachable where the unit's own
  -- six are neither, and `I` and `J` are the easiest two of the six — so they
  -- carry Record and the set. Arm sits on `G`, the hardest, deliberately: it
  -- waits for your note, so unlike Record its own timing does not matter. The
  -- best switch goes to the gesture that is late if you are.
  --
  -- **G is Arm alone.** Its hold was the only way onto the Grab bank, and went
  -- with it on 2026-11-07, so G is a sole-gesture switch again and reports at
  -- press-down. An MC6 programmed before then still sends the hold (value 1,
  -- with a jump to where the Grab bank was); it decodes as a hold on G, which
  -- nothing is bound to, and is said rather than acted on until the bank is
  -- written again.
  [ only ArmLoop
  , only Reverse
  , only RecordLoop
  -- **The set, and the way out.** Tap stops everything, double starts it again
  -- from the top together — see `Machine.perform` on why that is one command
  -- and not eight.
  --
  -- The hold is navigation, and it is here because it had nowhere else to go.
  -- It was `G`'s tap, which this layout spends on Arm; the only switches left
  -- are the unit's own, which are the awkward ones. A hold on the easiest
  -- switch on the board is a better home than a tap on a switch you have to
  -- reach up for.
  --
  -- From the Loops bank it leaves the looper for the board. (While there were
  -- deeper pages it went home to the loops from them, a second hold leaving.)
  , alsoHold
      (case slot of
         LoopBank -> Back ToBoard)
      (alsoDouble StartAll (only StopAll))
  , only HalfSpeed
  , only OverdubLoop
  ]

-- | The MC6's own six, which the device labels on its screen and which each
-- | bank is therefore free to spend as it likes.
own :: BankSlot -> Array Duties
own = case _ of

  -- Six loops on six switches: a loop is *where you put your foot*, not a mode
  -- you enter. Which is also why the loop bank has no room for anything else.
  --
  -- **One gesture each, and that is the point.** These carried three — tap to
  -- act, double to overdub, hold for config — and so had to be programmed on
  -- the release side, where the device waits out its double-tap window before
  -- it can say which one you meant. Every loop press in the rig was a few
  -- hundred milliseconds late, on the one switch where a few hundred
  -- milliseconds is a take.
  --
  -- Carrying one meaning, they sit on `ActionPress` and report the instant a
  -- foot lands. The verbs they used to carry went to a page of their own
  -- (`LoopPage`, removed 2026-11-07) and are on the toolbar, the Twister and
  -- the screen.
  -- **Not switch order — grid order.** Switch A selects loop 5, because A is
  -- the bottom-left switch and loop 5 is the bottom-left loop. See
  -- `switchLoops`.
  --
  -- **And a double undoes that loop**, added 2026-08-30. It costs the
  -- press-down report above — a switch with two meanings waits out the
  -- device's double-tap window before it can say which you meant — and the
  -- paragraph above is wrong about how much that matters: it is *Record* that
  -- has to answer when your foot lands, and Record is on `I` with one gesture
  -- of its own. Choosing a loop can afford to wait.
  --
  -- Undo rather than Clear, which was the first idea. Clear zeroes `redo_to`
  -- as well as the loop, so it is the one destructive act in the engine that
  -- cannot be taken back — and a fumbled double while choosing loops is
  -- exactly how you would find that out. Undo leaves the audio in place and
  -- `Redo` brings it back.
  -- **Two gestures and no more.** Choose it, or double to undo the last thing
  -- on it. There is nowhere for a hold to go: what the tap used to open is
  -- gone, its verbs having turned out to be the toolbar's six over again.
  LoopBank -> map (\n -> alsoDouble Undo (only (SelectLoop n))) switchLoops

-- | The family, written from `base` on, its way home to `boardBank`, every
-- | switch speaking on `channel`.
banks :: { base :: Int, boardBank :: Int, channel :: SwitchChannel } -> Array ControlBank
banks cfg = map toBank allSlots
  where
  toBank :: BankSlot -> ControlBank
  toBank slot =
    { id: "itajara-" <> slotId slot
    , name: slotName slot
    , description: describe slot
    , mc6BankNumber: cfg.base + slotIndex slot
    -- Legacy, and read only by `Global.migrateReturns` on stored banks. Set to
    -- where the way back actually is, so it is not a lie if anything ever does
    -- look at it.
    , returnSwitchIndex: case slot of
        LoopBank -> 11
    , switches: Array.mapWithIndex (compile slot) (padTo (layout slot))
    }

  describe :: BankSlot -> String
  describe slot =
    "Itajara " <> slotName slot <> " — switch CCs "
      <> show (switchCC slot 0) <> "-" <> show (switchCC slot (switchCount - 1))
      <> " on channel " <> show (switchChannelNumber cfg.channel)

  padTo :: Array Duties -> Array Duties
  padTo ds = Array.take switchCount (ds <> Array.replicate switchCount (only Nothing_))

  compile :: BankSlot -> Int -> Duties -> ControlBankSwitch
  compile slot i sw =
    -- The device's own words come out of the duty, so a switch cannot be
    -- labelled one thing on the pedal and mean another in the app. The tap is
    -- what gets printed: the pedal has room for one name, and the gesture a
    -- player reaches for without thinking is the one it should say.
    { label: dutyLabel sw.tap
    , longName: dutyName sw.tap
    -- Never the MC6's native toggle. A latching switch keeps state on the
    -- device, and the device is the one thing here that cannot be told it is
    -- wrong — every piece of state lives in the app, which can see the engine.
    , toToggle: false
    -- Blank switches are written blank rather than left alone, so uploading
    -- over whatever the bank held before leaves no stragglers doing something
    -- from a previous life.
    , messages:
        if sw.tap == Nothing_ then []
        else
          let bs = bindings slot i sw
          in map _.cc bs <> Array.mapMaybe _.jump bs
    }

  -- | Every message this switch carries, one row per gesture the device can
  -- | tell apart.
  -- |
  -- | The gesture travels in the value and the bank jump rides on the same
  -- | action, so the app is told what happened by the same press that made it
  -- | happen. There is nothing left to infer from timing, from ordering, or from
  -- | a memory of the last bank change.
  -- |
  -- | **The double-tap fallback is here and nowhere else.** A switch with no
  -- | second meaning still binds `DoubleTapRelease` — to the tap's own value and
  -- | the tap's own jump — because the device suppresses `Release` on a double
  -- | whether or not anything is listening, and a fumbled double that does
  -- | nothing is worse than one that does the thing once.
  bindings
    :: BankSlot -> Int -> Duties
    -> Array { cc :: MC6Message, jump :: Maybe MC6Message }
  bindings slot i sw =
    let
      emit action value duty =
        { cc: MC6Msg.ccMessage (switchChannelNumber cfg.channel) (switchCC slot i) value action
        , jump: (\j -> MC6Msg.bankJumpMessage (target j) action) <$> dutyTap duty
        }

      row g = do
        duty <- case g of
          -- The fallback: no second meaning means a double is a tap said twice.
          Double -> Just (fromMaybe sw.tap sw.double)
          _ -> dutyFor g sw
        let value = case g of
              Double | sw.double == Nothing -> gestureValue Tap
              _ -> gestureValue g
        Just (emit (gestureAction g) value duty)
      -- | **Report on the press, navigate on the release.**
      -- |
      -- | A sole-gesture switch reports at press-down, which is the whole point
      -- | of it. But putting its bank jump on the same action put a CC and a
      -- | bank change in one message list again — and that configuration has
      -- | cost this project twice now. The first time it ate the *release*: the
      -- | board moved mid-press, so the second message was emitted from a bank
      -- | the app was not expecting. This time it ate the CC itself, and the
      -- | symptom was much worse for being quiet — selecting a loop did nothing
      -- | at all, so `focus` never moved off zero, and every verb on the page
      -- | went to loop 1 while the pedal said loop 2. It looked like the machine
      -- | was misrouting; nothing was reaching the machine.
      -- |
      -- | So the two are never on the same action any more. The CC goes at
      -- | press-down, the board moves when the foot lifts a few milliseconds
      -- | later, and the app is told *before* the thing it is being told about
      -- | happens. Ordering within one action was not enough; the device gets a
      -- | separate action.
      --
      -- Binding the release costs nothing: `Press` fires at press-down whatever
      -- else is bound (measured), so the report stays instant. The double is
      -- covered too, because the device suppresses `Release` on one and the
      -- board would otherwise stay put after two quick presses.
      pressSide duty =
        Array.cons
          (MC6Msg.ccMessage (switchChannelNumber cfg.channel) (switchCC slot i) (gestureValue Tap) ActionPress)
          (case dutyTap duty of
             Nothing -> []
             Just j ->
               [ MC6Msg.bankJumpMessage (target j) ActionRelease
               , MC6Msg.bankJumpMessage (target j) ActionDoubleTapRelease
               ])
    in
      -- One meaning, so there is nothing for the device to resolve and no
      -- reason for it to wait. See `soleGesture`.
      if soleGesture sw then map (\m -> { cc: m, jump: Nothing }) (pressSide sw.tap)
      else Array.mapMaybe row allGestures

  -- | **The CCs before the jumps.**
  -- |
  -- | A bank jump that goes out first means the message after it is emitted from
  -- | the bank the board has already reached — which is how the app came to see
  -- | a press on one bank and its release on another, forget the orphan, and
  -- | then fire a hold nobody made. The gesture is one message now, so this is
  -- | belt as well as braces; the ordering is kept because the reason it was
  -- | needed has not stopped being true of the device.
  target :: Jump -> Int
  target = case _ of
    ToSlot s -> cfg.base + slotIndex s
    ToBoard -> cfg.boardBank
