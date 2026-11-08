-- | Eight loops, four across and two down — the same grid on every surface.
-- |
-- | The MC6 has no per-switch LEDs, only an LCD, so **all real feedback about
-- | the looper lives here** (`itajara-in-atlantis` §"The display"). The board
-- | supplies twelve labelled places to stand; this says what is happening.
-- |
-- | ## The grid mirrors the pedal
-- |
-- | Four across and two down, because that is what eight loops obviously look
-- | like and because it is the Twister's own top half: encoder 1 is loop 1, and
-- | the slot in the top-left of the screen is the encoder in the top-left of the
-- | controller. Nothing has to be looked up.
-- |
-- | **This used to be the MC6's arrangement and deliberately is not any more.**
-- | Three across and two down, drawn D E F above A B C, because the pedal
-- | numbers its switches from the bottom and was once the only way to reach a
-- | loop. With eight loops and a 4×4 controller that was the tail wagging the
-- | dog, so the pedal is now the surface that fits in: it covers the left three
-- | columns of both rows and lacks the fourth. Each slot prints the letter of
-- | the switch that selects it — D for loop 1 — so the foot still has an
-- | answer, and loops 4 and 8 print their number because no foot can reach
-- | them. An MC8 would fill that column exactly.
-- |
-- | `LB.loopRows` owns the arrangement and `LB.switchForLoop` owns the letters;
-- | this module only draws them.
-- |
-- | Which took a correction to actually be true. **The MC6 numbers from the
-- | bottom**: A B C is the near row and D E F the far one, so drawing the loops
-- | in index order put loop 1 at the top of the screen and under the far edge
-- | of the pedal. See `boardOrder`.
-- |
-- | ## What a slot draws, and why not a waveform yet
-- |
-- | Each slot shows the loop's **structure**: one track representing one cycle,
-- | divided into the slots a layer's own length cuts it into, with the ones
-- | that actually sound filled in. That is the thing the plan says matters most
-- | and the thing a waveform would not show — "two takes of the same length
-- | look identical when one of them plays one bar in four".
-- |
-- | The envelope goes *inside* those blocks later, when the daemon grows a
-- | peaks message. Drawing the structure first is not a placeholder: it needs
-- | no change to the engine, and it is the half that carries the meaning.
-- |
-- | ## The geometry is `layer_pos`, not an interpretation of it
-- |
-- | From `engine.rs`:
-- |
-- | ```rust
-- | let slot = (pos / len) % period;
-- | if slot != phase % period { return None }
-- | Some(pos % len)
-- | ```
-- |
-- | So a layer *tiles* across the cycle in blocks of its own `len`, and it
-- | sounds in block `s` exactly when `s % period == phase % period`. `period`
-- | counts blocks within the cycle, not cycles of the loop — which is why
-- | spreading a layer grows the loop around it and leaves room, rather than
-- | making it skip whole cycles.
-- |
-- | This module reproduces that expression rather than paraphrasing it. If the
-- | engine's rule changes, the drawing is wrong in the same way the sound is,
-- | which is the only kind of wrong worth having.
-- |
-- | ## The playhead, and the way that went wrong first
-- |
-- | Halogen re-renders at 10 Hz, which would step a playhead in visible jumps,
-- | so something has to interpolate between snapshots.
-- |
-- | The first attempt was a CSS animation of duration `loopSecs` with a
-- | **negative `animation-delay`** of the current position, restated on every
-- | snapshot. It ran at exactly double speed, and measuring it is the only
-- | reason that was noticed: a 6.47 s loop swept in 3.23 s. Changing
-- | `animation-delay` on a *running* animation shifts its start time without
-- | resetting elapsed time, so the position is `(elapsed - delay) / duration`
-- | — and both `elapsed` and `-delay` advance in real time. The animation and
-- | the correction were counting the same seconds twice.
-- |
-- | What it does instead is the plain thing: `left` is the phase, stated
-- | outright, with a short linear `transition` doing the interpolation. The
-- | position is then a pure function of the snapshot and cannot accumulate
-- | error, because nothing is accumulating.
-- |
-- | The one wrinkle a transition brings is the cycle boundary, where phase goes
-- | from nearly one to nearly zero and the transition would sweep the playhead
-- | *backwards* across the whole track. So the transition is suppressed for the
-- | first fraction of a cycle, which is computable from the snapshot alone and
-- | costs a jump nobody can see at the one moment a jump is correct.
-- | `stateClass` and `stateWord` are exported for the tests and for no other
-- | reason. They are the two places where a fault is invisible from inside the
-- | program — a wrong colour and a wrong word are still a rendered slot — so
-- | they are the two that most need enumerating, and a function nothing can
-- | reach is a function nothing can check.
module Itajara.Surface.Looper.Slots (render, stateClass, stateWord) where

import Prelude

import Data.Array as Array
import Data.Int (round, toNumber)
import Data.Maybe (Maybe(..))
import Data.Looper.Banks as LB
import Data.Looper.Twister as TW
import Foreign.LooperSocket (LoopState, LayerShape, LooperState)
import Foreign.LooperSocket as Looper
import Itajara.Surface.Wave (viewOf, wave)
import Halogen.HTML as HH
import Halogen.HTML.Events as HE
import Halogen.HTML.Properties as HP
import Web.DOM.Element as Element
import Web.Event.Event (Event, EventType(..))
import Web.Event.Event as Event

-- | The six slots, in board order.
-- |
-- | Takes the whole snapshot rather than just the loops, because a slot needs
-- | the sample rate to turn frames into seconds and the selection to say which
-- | loop the flat controls are pointed at.
-- | `shown` is the bank the MC6 is actually displaying, which the app learns
-- | from the presses themselves — every switch says which bank it came from.
-- | The legend has to follow it or it describes a board nobody is standing on.
-- | The six slots, the toolbar legend, and which loop the controls follow.
-- |
-- | **`focus` is passed in, and it must be the app's own.** It used to read the
-- | daemon's `selected` — a field left over from when there was one loop, which
-- | the six-loop surface never sets and which is therefore stuck on zero for
-- | ever. So the screen said the controls followed loop 1 while every press was
-- | going to loop 2, and there was no way to tell from the outside that the
-- | *display* was the thing that was wrong rather than the routing.
-- |
-- | The app's `looperFocus` is what `Data.Looper.Machine` actually acts on, so
-- | it is the only honest answer to "which loop does this bank talk about".
-- | What a slot can *do*. `onLayer loop layer on` is the layer checkboxes
-- | (loop is the daemon's index; layer is the number the slot shows, from
-- | one); `onEdit loop` is a click anywhere else in the box, which opens the
-- | loop's Edit panel.
type Intents i = { onLayer :: Int -> Int -> Boolean -> i, onEdit :: Int -> i }

render :: forall w i. Intents i -> LooperState -> Int -> LB.Face -> HH.HTML w i
render intents lp focus fc =
  HH.div [ HP.class_ (HH.ClassName "loops") ]
    [ HH.div [ HP.class_ (HH.ClassName "loops-grid") ]
        (Array.mapMaybe cell (join LB.loopRows))
    -- **No aux legend here since 2026-08-27.** There used to be a row naming
    -- G to L, because nothing else could — those are FS3X switches with no
    -- markings and no LCD. The board panel beside this one names them now, and
    -- names them better: it is the same table read the same way, and you can
    -- press it. A second copy of the six would only be a second thing to keep
    -- true.
    , legend lp focus
    ]
  where
  cell i = slot intents lp focus fc i <$> Array.index lp.loops i

-- | One loop. **The whole box opens its Edit panel**, except the layer
-- | checkboxes, which are controls of their own: a click whose target is an
-- | `INPUT` is the checkbox's, and the box lets it pass.
slot :: forall w i. Intents i -> LooperState -> Int -> LB.Face -> Int -> LoopState -> HH.HTML w i
slot intents top focus fc idx st =
  HH.div
    [ HP.class_ (HH.ClassName ("loop-slot " <> stateClass st
        <> (if focus == idx then " is-selected" else "")
        -- **The fourth column, marked rather than hidden.** These were the two
        -- loops no foot could reach and were drawn with a dashed edge to say
        -- so; the Grab bank (2026-08-30 to 2026-11-07) gave them switches, and what
        -- makes them different is no longer absence but *kind* — they hold
        -- what the iPad plays rather than what the guitar does. So the mark is
        -- a colour now and not a dash, which is a statement rather than an
        -- apology.
        <> (if Array.elem idx LB.grabLoops then " is-grab" else "")))
    , HP.title "open this loop's Edit panel"
    , HE.handler' (EventType "click") \ev ->
        if targetIsInput ev then Nothing else Just (intents.onEdit idx)
    ]
    [ HH.div [ HP.class_ (HH.ClassName "loop-head") ]
        [ HH.span [ HP.class_ (HH.ClassName "loop-letter") ]
            [ HH.text (LB.faceLoopKey fc idx) ]
        , HH.span [ HP.class_ (HH.ClassName "loop-state") ] [ HH.text (stateWord st) ]
        , HH.span [ HP.class_ (HH.ClassName "loop-layers") ]
            [ HH.text (if st.layers == 0 then "" else show st.layers <> plural st.layers " layer") ]
        ]
    , track (intents.onLayer idx) top st
    , mix top st
    , HH.div [ HP.class_ (HH.ClassName "loop-foot") ]
        [ HH.span_ [ HH.text (lengthWord top st) ]
        -- The resolutions, shown only when they are not the default. A row of
        -- "forward · centre · free" on six slots is noise; a lone "REV" is
        -- information, and these resolutions are otherwise invisible from here.
        , HH.span [ HP.class_ (HH.ClassName "loop-marks") ]
            (map mark (marks st))
        ]
    ]

-- | **Level and pan, on every slot, always.**
-- |
-- | `marks` shows both already and only when they are off their defaults, which
-- | is the right rule for a *flag* — six slots each announcing "forward, centre,
-- | free" is a row of noise that hides the one loop somebody reversed. It is
-- | the wrong rule for the two continuous values you mix with. Those you want
-- | to compare *across* loops, which a mark that disappears at unity cannot do:
-- | the question is never "is this one turned down", it is "which of these is
-- | loudest", and a row where the answer is sometimes blank does not answer it.
-- |
-- | So they are drawn rather than written. A bar for the level and a tick on a
-- | centre-marked track for the pan — the same two things the Twister's first
-- | two pages are, in the same order, and readable in one sweep down the row.
-- |
-- | **The bar is `TW.toKnob`, not a percentage of the decibels.** That is the
-- | one function that says where a value sits on a 0-127 travel, and it is what
-- | the encoder ring is drawn from — so the bar on the screen and the ring under
-- | the hand cannot disagree about the same loop. A second conversion here
-- | would be a second chance to bend the fader law differently.
targetIsInput :: Event -> Boolean
targetIsInput ev =
  case Event.target ev >>= Element.fromEventTarget of
    Just el -> Element.tagName el == "INPUT"
    Nothing -> false

mix :: forall w i. LooperState -> LoopState -> HH.HTML w i
mix lp st =
  HH.div [ HP.class_ (HH.ClassName "loop-mix") ]
    -- The words survive as the hover, which is where an exact number belongs on
    -- a control you read by shape: the picture answers "which is loudest" and
    -- the tooltip answers "how loud, exactly".
    [ HH.div
        [ HP.class_ (HH.ClassName "loop-level")
        , HP.title ("level " <> LB.levelWord st.volDb)
        ]
        [ HH.div
            [ HP.class_ (HH.ClassName "loop-level-fill")
            , HP.style ("width:" <> pc (TW.toKnob { maxLayers: lp.maxLayers } TW.PLevel st) <> "%")
            ] []
        ]
    , HH.div
        [ HP.class_ (HH.ClassName "loop-pan")
        , HP.title ("pan " <> panWord st.pan)
        ]
        [ HH.div [ HP.class_ (HH.ClassName "loop-pan-centre") ] []
        , HH.div
            [ HP.class_ (HH.ClassName "loop-pan-tick")
            , HP.style ("left:" <> pc st.pan <> "%")
            ] []
        ]
    ]
  where
  pc v = show (toNumber (round (toNumber v / 127.0 * 1000.0)) / 10.0)

mark :: forall w i. Mark -> HH.HTML w i
mark m =
  HH.span [ HP.class_ (HH.ClassName ("loop-mark " <> markClass m)) ]
    [ HH.text (markText m) ]

-- | The cycle, its layers, and the playhead.
-- |
-- | An empty loop still draws the track. Six slots that vanish when empty would
-- | make the board's shape change under your feet, and the whole point of the
-- | grid is that it does not.
track :: forall w i. (Int -> Boolean -> i) -> LooperState -> LoopState -> HH.HTML w i
track onLayer top st =
  HH.div [ HP.class_ (HH.ClassName "loop-track") ]
    (Array.mapWithIndex (layerRow onLayer st) st.shapes <> liveRow st <> playhead top st)

-- | The take being recorded **right now**, drawn as it is played.
-- |
-- | **Nothing showed here at all until 2026-08-25**, and it read as broken
-- | because it was: you pressed record, the slot went the colour of recording,
-- | and nothing else happened until you closed the take. Everything the display
-- | knew about a loop was a *committed* layer, so the one moment you most want
-- | to see — is it hearing me, am I loud enough, how far round am I — was the
-- | one moment it had nothing to say.
-- |
-- | It matters more with Revox than with anything else. A destructive pass has
-- | no undo, so watching it happen is the only feedback there is, and a mode
-- | that erases while showing you nothing is not a mode anybody should be asked
-- | to use.
-- |
-- | Drawn as a row of its own beneath the layers rather than as one of them: it
-- | is not a layer yet, it may never become one, and it is the row your eye
-- | should go to.
liveRow :: forall w i. LoopState -> Array (HH.HTML w i)
liveRow st
  -- The daemon sends this empty whenever nothing is recording, so there is one
  -- test here rather than a second copy of "what counts as recording".
  | Array.null st.recEnv = []
  | otherwise =
      [ HH.div
          [ HP.class_ (HH.ClassName ("loop-layer loop-live"
              <> (if st.revox then " is-tape" else ""))) ]
          [ HH.div
              [ HP.class_ (HH.ClassName "loop-block sounds has-wave")
              , HP.style "width:100%"
              ]
              (wave st.recEnv)
          ]
      ]

-- | One layer, as the blocks in which it sounds.
-- | A row per layer: its checkbox, then the blocks of the cycle it sounds in.
-- |
-- | **The checkbox is the layer's, not the block's.** A layer that plays one
-- | bar in four is still one layer, and taking it out of the mix is one act.
-- | Off dims the blocks rather than hiding them: where the layer *would* sound
-- | is still a fact about the loop, and the reason you might want it back.
layerRow :: forall w i. (Int -> Boolean -> i) -> LoopState -> Int -> LayerShape -> HH.HTML w i
layerRow onLayer st i sh =
  HH.div [ HP.class_ (HH.ClassName ("loop-layer" <> (if sh.on then "" else " is-off"))) ]
    ( [ HH.input
          [ HP.type_ HP.InputCheckbox
          , HP.class_ (HH.ClassName "loop-layer-on")
          , HP.checked sh.on
          , HP.title ("layer " <> show (i + 1) <> (if sh.on then " is in the mix" else " is parked"))
          , HE.onChecked (onLayer (i + 1))
          ]
      ] <> map block (Array.range 0 (blocks - 1))
    )
  where
  -- How many of this layer's own lengths fit in the cycle. At least one, so a
  -- layer as long as the loop is one block rather than none.
  blocks = max 1 (if sh.len > 0 then st.loopFrames / sh.len else 1)

  period = max 1 sh.period

  -- `engine.rs`: `slot != phase % period` is silence. Same expression.
  sounds s = s `mod` period == sh.phase `mod` period

  widthPc = 100.0 / toNumber blocks

  -- Whether there is a picture to draw. A layer being recorded has none yet,
  -- and one recorded before the daemon knew how to draw them never will — both
  -- keep the solid block they always had rather than going pale and empty.
  drawn = not (Array.null sh.env)

  block s =
    HH.div
      [ HP.class_ (HH.ClassName ("loop-block"
          <> (if sounds s then " sounds" else " rest")
          <> (if drawn then " has-wave" else "")))
      -- Decay is invisible in the arena — nothing is scaled there — so the only
      -- way a receding loop can be seen is if the display asks the engine what
      -- each layer is currently worth. A floor of a tenth, because a layer on
      -- its way out is still a fact about the loop and a block you cannot see
      -- reads as a block that is not there.
      , HP.style $ "width:" <> show widthPc <> "%; --layer:" <> show i
          <> (if sh.gain >= 1.0 then ""
              else ";opacity:" <> show (max 0.1 sh.gain))
      ]
      -- **Inside the block, not instead of it.** The block says *where in the
      -- cycle* this layer sounds, which is the thing a waveform cannot show and
      -- the reason the chart was drawn this way in the first place. The
      -- envelope goes in it and answers a different question: which loop is
      -- this, and how loud.
      (if sounds s && drawn then wave (viewOf st sh) else [])

-- | Present only when there is a cycle to sweep, and positioned outright from
-- | `phase` rather than from anything that keeps its own time.
playhead :: forall w i. LooperState -> LoopState -> Array (HH.HTML w i)
playhead _ st
  | st.loopFrames <= 0 = []
  -- A one-shot between passes has a playhead and nothing to show with it. The
  -- arithmetic never stops — it cannot hold still — so `phase` goes on sweeping
  -- while the loop is silent, and a bar moving under a loop nobody can hear is
  -- the display telling a story about audio that is not happening.
  | st.oneShot && not st.firing = []
  | otherwise =
      [ HH.div
          [ HP.class_ (HH.ClassName "loop-playhead")
          , HP.style $
              "left:" <> show (st.phase * 100.0) <> "%;"
                -- Just wrapped, or standing still: go straight there. Gliding
                -- would run the whole track the wrong way in the first case and
                -- pretend to motion in the second.
                -- **Both ends**, because a loop can wrap at either. A reversed
                -- loop crosses zero going the other way and reappears at the
                -- far edge, which guarding only the near edge would animate as
                -- a full sweep across the slot every time round.
                -- Note that a *stopped* loop keeps its moving playhead. That
                -- is not an oversight: stopping is phase-locked, the loop is
                -- still turning, and showing it frozen would promise it comes
                -- back at the start when it does not.
                <> ( if st.phase < wrapGuard
                       || st.phase > 1.0 - wrapGuard
                       || (Looper.phaseOf st == Looper.Idle && st.layers == 0) then "transition:none;"
                     else "transition:left 110ms linear;"
                   )
          ]
          []
      ]

-- | How much of a cycle counts as "just wrapped".
-- |
-- | Wide enough to cover more than one 100 ms tick on any loop long enough to
-- | play — at 6.5 s this is 130 ms, or between one and two snapshots — and
-- | narrow enough that the un-interpolated stretch is over before an eye
-- | settles on it.
wrapGuard :: Number
wrapGuard = 0.02

-- | What the flat controls are pointed at, and what the rig's clock says.
-- |
-- | Small, because it is reference rather than performance information — but
-- | present, because "which loop does the pedal face drive" is otherwise
-- | invisible, and because a Link tempo of zero is the difference between no
-- | clock and a clock reading zero.
legend :: forall w i. LooperState -> Int -> HH.HTML w i
legend lp focus =
  HH.div [ HP.class_ (HH.ClassName "loops-legend") ]
    [ HH.span_ [ HH.text ("controls follow " <> letter focus) ]
    -- **Tempo and metre are a readout, not a control**, and that is the whole
    -- answer to where their controls should live. They arrive from link-spike,
    -- which gets them from Ableton; a knob here would be a second place the
    -- rig's tempo is decided, which is the one thing this app is careful never
    -- to be. So: show them, and never offer to set them.
    , HH.span_
        [ HH.text $
            if lp.linkAnchors == 0 then "no clock"
            else show (round lp.linkTempo) <> " bpm · "
                   <> show (round lp.linkQuantum) <> "/bar"
        ]
    -- **The bar the engine is actually counting in**, which is not always
    -- Link's. Without a clock it is the first loop's cycle over however many
    -- bars that loop has been declared to be — so this is the number lengths
    -- mean, and it is worth showing beside the one the clock reports because
    -- they can differ and the difference is never obvious.
    , if lp.barFrames == 0 then HH.text ""
      else HH.span_
             [ HH.text ("bar " <> secs (toNumber lp.barFrames / toNumber lp.sampleRate)
                          <> (if lp.linkAnchors == 0 then " (from loop 1)" else "")) ]
    , HH.span_ [ HH.text ("launch " <> launchWord lp.launchQ) ]
    , if lp.linkRejected == 0 then HH.text ""
      else HH.span [ HP.class_ (HH.ClassName "loops-warn") ]
             [ HH.text (show lp.linkRejected <> " clock messages refused") ]
    ]

-- | What a launch waits for, in the words the encoder uses.
-- |
-- | `-1` is a bar rather than four beats, because a bar is what the metre says
-- | it is — three beats in 3/4 — and a setting spelled as a beat count would be
-- | right in one time signature and quietly wrong in every other.
launchWord :: Int -> String
launchWord q = case q of
  -1 -> "on the bar"
  0 -> "straight away"
  1 -> "on the beat"
  n -> "every " <> show n <> " beats"

-- | A B C on the top row, D E F below — the board's own letters, so the screen
-- | and the pedal agree without anybody translating.
-- | Which switch a loop sits on, from the same table the aux legend uses.
letter :: Int -> String
letter i = case LB.switchLetter i of
  Just l -> l
  Nothing -> "?"

-- | The daemon's own words, softened for reading at a distance. `RecordingFirst`
-- | is a state name; "recording" is what you need to know while playing.
-- |
-- | **Two levels, and they are kept apart deliberately.** The phase is one
-- | thing; `muted`, `oneShot`, `skipping` and the layer count are orthogonal
-- | flags that override it on screen. Flattening the two into one `case` is
-- | what made the ordering here so delicate — and it also meant the phase
-- | match ended in a catch-all, so a phase nobody had handled fell through
-- | silently. The guards come first, then the phase is matched exhaustively.
stateWord :: LoopState -> String
stateWord st
  -- Above the emptiness guard, and it has to be: a level-armed loop is empty
  -- by definition — that is what it is waiting to stop being — so reading the
  -- layer count first made the one state the player most needs to see the one
  -- state that could never be shown.
  | Looper.phaseOf st == Looper.Armed =
      if st.pendingAt >= 0 then "waiting" else "listening"
  -- Layers next, because a loop undone to nothing keeps its length and its
  -- phase: the engine still calls it `Playing` and there is nothing to play.
  -- The footer still shows the length, which is the useful half — that is what
  -- the next take will land on. Sized is the daemon's word for it since
  -- 2026-09-07, so it is read rather than derived.
  | st.sized = "empty"
  | st.layers == 0 = ""
  | st.muted = "stopped"
  -- Loaded and waiting for a foot, which is a different thing from stopped: a
  -- stopped loop is still turning and comes back where it would have been, and
  -- a one-shot comes back at the top.
  | st.oneShot = if st.firing then "firing" else "ready"
  -- Sitting this one out. Distinct from stopped, which is a decision you made
  -- and which stays made — this one comes back by itself.
  | st.skipping = "sitting out"
  | otherwise = case Looper.phaseOf st of
      Looper.RecordingFirst -> "recording"
      Looper.Overdubbing -> "overdub"
      Looper.Multiplying -> "multiply"
      Looper.Playing -> "playing"
      Looper.Idle -> if st.layers > 0 then "idle" else ""
      -- Answered by the first guard; listed so the compiler can see the set is
      -- covered rather than being told to assume it by a catch-all.
      Looper.Armed -> ""

stateClass :: LoopState -> String
stateClass st
  -- **Writing beats everything, including emptiness.** This used to ask about
  -- emptiness first and exempt only `recordingFirst` and `armed` — so a loop
  -- that had been undone to nothing and then recorded into again was
  -- `overdubbing` with `layers == 0`, matched the empty branch, and was drawn
  -- as an empty slot while it held the one converter the rig has. The word
  -- underneath said "overdub"; nobody reads the word, they read the colour.
  --
  -- Asked through `isWriting` rather than by listing phases here, because
  -- listing them here is precisely what let this drift out of step with the
  -- meaning table.
  | Looper.isWriting st = "is-recording"
  -- Stopped beats the rest: a loop being recorded into while silenced is a
  -- thing the player most needs told.
  | st.layers == 0 && Looper.phaseOf st /= Looper.Armed = "is-empty"
  | st.muted = "is-stopped"
  | st.oneShot = if st.firing then "is-playing" else "is-stopped"
  | otherwise = case Looper.phaseOf st of
      Looper.Armed -> "is-armed"
      Looper.Playing -> "is-playing"
      Looper.Idle -> if st.layers > 0 then "is-stopped" else "is-empty"
      -- All three answered by `isWriting` in the first guard.
      Looper.RecordingFirst -> "is-recording"
      Looper.Overdubbing -> "is-recording"
      Looper.Multiplying -> "is-recording"

-- | Length, or the countdown to one.
-- |
-- | A scheduled transition that shows nothing is the failure this display
-- | exists to prevent: a deliberate wait and a dead footswitch look identical
-- | until one of them says how long it intends to wait.
lengthWord :: LooperState -> LoopState -> String
lengthWord top st
  | st.pendingAt >= 0 =
      "in " <> secs (toNumber st.pendingAt / toNumber (max 1 top.sampleRate))
  | st.loopFrames <= 0 = "empty"
  | otherwise = secs st.loopSecs

-- | What a mark is *for*, which is why they are not one string any more.
-- |
-- | They were: a row of words joined by dots, all the same weight and all the
-- | same colour, so `1 SHOT` — which changes what your foot does — read exactly
-- | like `L 50`, which is a fact about the stereo field. Six slots of that is a
-- | wall of small grey text with the one thing you needed in the middle of it.
-- |
-- | Three kinds, and the distinction is about *when they matter* rather than
-- | what they configure:
-- |
-- | - **`Foot`** changes what the next press does. It is about your next action,
-- |   not about the sound, which is why it is the loudest thing here.
-- | - **`Live`** is doing something on its own, right now, without you — a loop
-- |   that is dropping cycles or receding is changing while you look at it.
-- | - **`Set`** is simply true until you change it.
data Mark
  = Foot String
  | Live String
  | Set String

markText :: Mark -> String
markText = case _ of
  Foot t -> t
  Live t -> t
  Set t -> t

markClass :: Mark -> String
markClass = case _ of
  Foot _ -> "mark-foot"
  Live _ -> "mark-live"
  Set _ -> "mark-set"

-- | What has been done to a loop that is not the default.
-- |
-- | Silence when nothing has: six slots each announcing "forward, centre, free"
-- | is a row of noise that hides the one loop somebody reversed.
marks :: LoopState -> Array Mark
marks st = Array.catMaybes
  -- Your feet first, because they are about what happens next rather than about
  -- what is happening. A one-shot fires where any other loop stops.
  [ if st.oneShot then Just (Foot "1 shot") else Nothing
  , if st.levelArm then Just (Foot "listen") else Nothing
  -- **Anything below full, said out loud.** A loop turned down is silent for a
  -- reason no other mark would show, and that is not hypothetical: a knob left
  -- one at -58 dB, `Clear All` did not restore it because clearing was the
  -- thing that had failed to, and the slot showed a perfectly ordinary loop
  -- making no sound. Shown as `Live` rather than `Set` because it is the loudest
  -- fact about a loop that is not doing what you expect.
  -- **A tape, and it says so first.** It changes what every other control
  -- means — an overdub writes over what is there and undo is gone — so it
  -- outranks every other mark on the slot.
  , if st.revox
      then Just (Foot ("tape " <> LB.levelWord st.fbDb
             <> (if st.toneHz >= 20000.0 then "" else " · " <> show (round (st.toneHz / 100.0) * 100) <> " Hz")))
      else Nothing
  -- Level and pan came off this list on 2026-08-27: they are drawn on every
  -- slot now, by `mix`, and a word saying what the bar directly above it says
  -- is a word in the way.
  -- Then the two that move on their own. A loop at 1 in 4 or losing 3 dB a pass
  -- is not where you left it, and nothing else on screen would say so.
  , if st.chance >= 1.0 then Nothing else Just (Live (LB.chanceWord st.chance))
  , if st.decayDb >= 0.0 then Nothing else Just (Live (LB.decayWord st.decayDb))
  -- And then what is merely true.
  , if st.pendulum then Just (Set "swing") else if st.reverse then Just (Set "rev") else Nothing
  , if st.speed == 1.0 then Nothing else Just (Set (speedWord st.speed))
  , if st.quant then Just (Set "grid") else Nothing
  , if st.fadeMs <= 0.0 then Nothing else Just (Set ("~" <> LB.fadeWord st.fadeMs))
  ]

-- | Speed as the multiplier the switch was labelled with, not a decimal.
-- |
-- | The board says "x 1/2"; so does this. A display that answers a press with
-- | different words from the switch that caused it makes the player do the
-- | translation, which is the one job a display is for.
-- | Every rung of `TW.rateLadder` has a name here, because the ladder is made
-- | of just ratios and a decimal is the one way to write a just ratio that
-- | tells you nothing: ×0.67 is a fifth down and reads like a rounding error.
speedWord :: Number -> String
speedWord s
  | s == 0.125 = "×⅛"
  | s == 0.167 = "×⅙"
  | s == 0.25 = "×¼"
  | s == 0.333 = "×⅓"
  | s == 0.5 = "×½"
  | s == 0.667 = "×⅔"
  | s == 1.5 = "×1½"
  | s == 2.0 = "×2"
  | s == 3.0 = "×3"
  | s == 4.0 = "×4"
  | s == 0.0 = "held"
  | otherwise = "×" <> show (toNumber (round (s * 100.0)) / 100.0)

-- | Pan as a word rather than a number: 0-127 is the wire's business.
panWord :: Int -> String
panWord p
  | p <= 10 = "L"
  | p <= 52 = "l"
  | p <= 74 = "C"
  | p <= 116 = "r"
  | otherwise = "R"

secs :: Number -> String
secs s = show (toNumber (round (s * 10.0)) / 10.0) <> " s"

plural :: Int -> String -> String
plural n word = if n == 1 then word else word <> "s"
