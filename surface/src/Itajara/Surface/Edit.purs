-- | The Edit panel: one loop's window, where a pass starts inside it, and the
-- | rendered waveform the two are set against.
-- |
-- | **Non-destructive, and audible while you do it.** The window's two ends
-- | and the start are the daemon's `in`/`out`/`rot`, applied by the engine at
-- | the next restart — so what you hear while you drag is what `Export` will
-- | write; nothing here is a preview of a different renderer. Sliders rather
-- | than handles on the waveform: they are exact, keyboard-nudgeable, and
-- | need no drag machinery. Steps are a beat when the loop is on the grid, a
-- | frame when it is not.
-- |
-- | A pure render over the client's types and a handler record. The app
-- | owns the three pieces of state the panel reads — which loop is in focus,
-- | the last `peaks` answer, and the slider a hand is currently on — and
-- | passes them in as `View`; it owns the socket too, which is why every
-- | handler here is `… -> i` and the panel has no word for it. Moved here
-- | from producing-with-your-feet's `Component.Looper.Page` on 2026-09-04.
module Itajara.Surface.Edit
  ( Handlers
  , View
  , editPanel
  ) where

import Prelude

import Data.Array as Array
import Data.Int as Int
import Data.Map (Map)
import Data.Map as Map
import Data.Maybe (Maybe(..), fromMaybe, maybe)
import Data.String (joinWith)
import Foreign.LooperSocket (LoopState, LooperState, Peaks)
import Halogen.HTML as HH
import Halogen.HTML.Events as HE
import Halogen.HTML.Properties as HP
import Itajara.Surface.Wave (sAttr, svgEl)

-- | The panel's five, plus the release. Window in and out take (loop,
-- | frames); the whole loop again takes the loop; a shift of the start takes
-- | (loop, signed frames); a fresh waveform takes the loop. `editDone` says a
-- | slider was released, so the snapshot owns its value again. An open row,
-- | so an app's larger handler record passes as it is.
type Handlers i r =
  { windowIn :: Int -> Int -> i
  , windowOut :: Int -> Int -> i
  , clearWindow :: Int -> i
  , shiftStart :: Int -> Int -> i
  , askPeaks :: Int -> i
  -- | Both ends at once (loop, in, out): the fixed window's one slider.
  , windowTo :: Int -> Int -> Int -> i
  , editDone :: String -> i
  | r
  }

-- | What the app holds for the panel.
type View =
  { focus :: Int
  , peaks :: Maybe Peaks
  -- | The slider a hand is on, by key (`in`, `out`, `rot`), showing the
  -- | hand's value until release. Without it the snapshot — which lags the
  -- | verb by a restart — snatches the thumb back under the pointer.
  , local :: Map String Int
  -- | A window of one fixed length, in frames, when the destination holds
  -- | exactly that much — an Arbhar layer's thirteen seconds. The panel is
  -- | then one slider: where the window sits. `Nothing` is the free panel.
  , fixedFrames :: Maybe Int
  }

editPanel :: forall w i r. Handlers i r -> View -> Maybe LooperState -> HH.HTML w i
editPanel h v = case _ of
  Nothing -> HH.div_ [ HH.text "No daemon to edit on." ]
  Just top -> case Array.index top.loops v.focus of
    Nothing -> HH.text ""
    Just lp
      | lp.loopFrames <= 0 -> HH.div_ [ HH.text "This loop has no length yet; record something first." ]
      | otherwise -> case v.fixedFrames of
          Just n -> fixedBody h v top lp n
          Nothing -> editBody h v top lp

editBody :: forall w i r. Handlers i r -> View -> LooperState -> LoopState -> HH.HTML w i
editBody h v top lp =
  HH.div [ HP.class_ (HH.ClassName "looper-edit") ]
    [ waveform
    -- **Past the ends is silence, and allowed.** In can go a whole loop
    -- before zero and Out a whole loop past the length; what that adds is
    -- rest, and it is how a loop grows room without a crop or a re-take.
    -- **The sliders run the length of the picture**, so where the thumb is
    -- is where the line is. The picture is the loop plus whatever silence
    -- the window has reached into, with an eighth of a loop of room past
    -- each end so it can always be pushed further; it grows as it goes.
    , sliderRow "in" "In" picFrom (winO - 1) winI (\x -> h.windowIn li x) (timeWord winI <> (if winI < 0 then " (rest before)" else ""))
    , sliderRow "out" "Out" (winI + 1) picTo winO (\x -> h.windowOut li x) (timeWord winO <> (if winO > len then " (rest after)" else ""))
    , sliderRow "rot" "Start" 0 (max 0 (span - 1)) lp.rot (\x -> h.shiftStart li (x - lp.rot))
        (timeWord (winI + lp.rot) <> " into the loop")
    , HH.div [ HP.class_ (HH.ClassName "looper-edit-actions") ]
        [ HH.button [ HP.class_ (HH.ClassName "looper-help-btn"), HE.onClick \_ -> h.shiftStart li (negate step) ] [ HH.text ("⟵ " <> stepWord) ]
        , HH.button [ HP.class_ (HH.ClassName "looper-help-btn"), HE.onClick \_ -> h.shiftStart li step ] [ HH.text (stepWord <> " ⟶") ]
        , HH.button [ HP.class_ (HH.ClassName "looper-help-btn"), HE.onClick \_ -> h.clearWindow li ] [ HH.text "Whole loop" ]
        , HH.button [ HP.class_ (HH.ClassName "looper-help-btn"), HE.onClick \_ -> h.askPeaks li ] [ HH.text "Redraw" ]
        , HH.span [ HP.class_ (HH.ClassName "looper-edit-note") ]
            [ HH.text (if windowed then "Windowed: the daemon will not record into this loop until it plays whole again." else "The whole loop.") ]
        ]
    ]
  where
  li = v.focus
  len = lp.loopFrames
  windowed = lp.winOut > 0
  winI = if windowed then lp.winIn else 0
  winO = if windowed then lp.winOut else len
  span = max 1 (winO - winI)
  picFrom = max (negate len) (min 0 winI - len / 8)
  picTo = min (2 * len) (max len winO + len / 8)
  -- A beat, from the bar the rig reports, when the loop is on the grid;
  -- otherwise a frame, which is the exact thing.
  beat = if top.barFrames > 0 && top.linkQuantum > 0.0
    then max 1 (Int.round (Int.toNumber top.barFrames / top.linkQuantum))
    else 1
  step = if lp.quant then beat else 1
  stepWord = if step == 1 then "1 frame" else "1 beat"
  timeWord f = secsOf f <> " s"
  secsOf f = show (Int.toNumber (Int.round (Int.toNumber f / Int.toNumber top.sampleRate * 1000.0)) / 1000.0)

  -- The slider shows the hand's value while the hand is on it (see `local`)
  -- and the snapshot's otherwise.
  sliderRow key label lo hi val onV word =
    HH.div [ HP.class_ (HH.ClassName "looper-edit-row") ]
      [ HH.span [ HP.class_ (HH.ClassName "looper-edit-label") ] [ HH.text label ]
      , HH.input
          [ HP.type_ HP.InputRange
          , HP.class_ (HH.ClassName "looper-edit-slider")
          , HP.min (Int.toNumber lo)
          , HP.max (Int.toNumber hi)
          , HP.step (HP.Step (Int.toNumber step))
          , HP.value (show (fromMaybe val (Map.lookup key v.local)))
          , HE.onValueInput \raw -> onV (maybe val identity (Int.fromString raw))
          , HE.onValueChange \_ -> h.editDone key
          ]
      , HH.span [ HP.class_ (HH.ClassName "looper-edit-word") ] [ HH.text word ]
      ]

  waveform = picture v lp { len, winI, winO, picFrom, picTo, showStart: true }

-- | The picture: the whole loop, always, with the window shaded, the
-- | start marked, and the playhead where the daemon says it is. Drawn from
-- | the last `peaks` answer; a stale one for another loop is not drawn.
picture :: forall w i. View -> LoopState -> { len :: Int, winI :: Int, winO :: Int, picFrom :: Int, picTo :: Int, showStart :: Boolean } -> HH.HTML w i
picture v lp g = case v.peaks of
  Just pk | pk.loop == v.focus && pk.buckets > 0 ->
    let
      w = Int.toNumber pk.buckets
      -- The picture spans `picFrom..picTo`, the same range the sliders
      -- run, so a thumb and a line agree. The peaks were drawn over
      -- their own range and are placed onto this one bucket by bucket.
      x f = Int.toNumber (f - g.picFrom) / Int.toNumber (max 1 (g.picTo - g.picFrom)) * w
      y val = 100.0 - Int.toNumber val / 10.0
      pt i val = show (x (pk.from + i * (pk.to - pk.from) / max 1 pk.buckets)) <> "," <> show (y val)
      top' = joinWith " " (Array.mapWithIndex pt pk.hi)
      bot = joinWith " " (Array.reverse (Array.mapWithIndex pt pk.lo))
    in
      svgEl "svg"
        [ sAttr "viewBox" ("0 0 " <> show pk.buckets <> " 200")
        , sAttr "preserveAspectRatio" "none"
        , sAttr "class" "looper-wave"
        ]
        -- Drawn in this order so the feedback is unmissable: the loop's
        -- extent in white, the audio, then everything *outside* the
        -- window dimmed hard on top of it, the two ends as lines, the
        -- start in red, and the playhead — which only ever moves inside
        -- the window, which is the other half of the feedback.
        [ svgEl "rect" [ sAttr "x" (show (x 0)), sAttr "y" "0", sAttr "width" (show (x g.len - x 0)), sAttr "height" "200", sAttr "class" "looper-wave-loop" ] []
        , svgEl "polygon" [ sAttr "points" (top' <> " " <> bot), sAttr "class" "looper-wave-body" ] []
        , svgEl "rect" [ sAttr "x" "0", sAttr "y" "0", sAttr "width" (show (x g.winI)), sAttr "height" "200", sAttr "class" "looper-wave-outside" ] []
        , svgEl "rect" [ sAttr "x" (show (x g.winO)), sAttr "y" "0", sAttr "width" (show (w - x g.winO)), sAttr "height" "200", sAttr "class" "looper-wave-outside" ] []
        , svgEl "line" [ sAttr "x1" (show (x g.winI)), sAttr "x2" (show (x g.winI)), sAttr "y1" "0", sAttr "y2" "200", sAttr "class" "looper-wave-edge" ] []
        , svgEl "line" [ sAttr "x1" (show (x g.winO)), sAttr "x2" (show (x g.winO)), sAttr "y1" "0", sAttr "y2" "200", sAttr "class" "looper-wave-edge" ] []
        , svgEl "line" [ sAttr "x1" (show (x (g.winI + lp.rot))), sAttr "x2" (show (x (g.winI + lp.rot))), sAttr "y1" "0", sAttr "y2" "200", sAttr "class" "looper-wave-start" ] []
        , svgEl "line" [ sAttr "x1" (show (x lp.pos)), sAttr "x2" (show (x lp.pos)), sAttr "y1" "0", sAttr "y2" "200", sAttr "class" "looper-wave-head" ] []
        ]
  _ -> HH.div [ HP.class_ (HH.ClassName "looper-wave-missing") ] [ HH.text "Waveform on its way…" ]

-- | One slider: where a window of a fixed length sits on the loop. The
-- | destination holds exactly this much, so the two ends move together and
-- | rotation has no meaning — position zero of a pass is the window's start.
-- | A loop shorter than the window plays whole and is not windowed: the
-- | harvest fills the module's length by letting it come round again, which
-- | is what a loop does.
fixedBody :: forall w i r. Handlers i r -> View -> LooperState -> LoopState -> Int -> HH.HTML w i
fixedBody h v top lp n =
  HH.div [ HP.class_ (HH.ClassName "looper-edit") ]
    ( [ picture v lp { len, winI, winO, picFrom: 0, picTo: len, showStart: false } ]
        <> (if len > n
              then
                [ HH.div [ HP.class_ (HH.ClassName "looper-edit-row") ]
                    [ HH.span [ HP.class_ (HH.ClassName "looper-edit-label") ] [ HH.text "Window" ]
                    , HH.input
                        [ HP.type_ HP.InputRange
                        , HP.class_ (HH.ClassName "looper-edit-slider")
                        , HP.min 0.0
                        , HP.max (Int.toNumber (len - n))
                        , HP.step (HP.Step (Int.toNumber step))
                        , HP.value (show (fromMaybe winI (Map.lookup "in" v.local)))
                        , HE.onValueInput \raw -> let s = maybe winI identity (Int.fromString raw) in h.windowTo li s (s + n)
                        , HE.onValueChange \_ -> h.editDone "in"
                        ]
                    , HH.span [ HP.class_ (HH.ClassName "looper-edit-word") ]
                        [ HH.text (secs winI <> " – " <> secs winO <> " s of " <> secs len) ]
                    ]
                ]
              else
                [ HH.div [ HP.class_ (HH.ClassName "looper-edit-row") ]
                    [ HH.span [ HP.class_ (HH.ClassName "looper-edit-label") ] [ HH.text "Whole" ]
                    , HH.span [ HP.class_ (HH.ClassName "looper-edit-word") ]
                        [ HH.text (secs len <> " s, shorter than the " <> secs n <> " s the module holds: it plays whole, and the harvest fills the rest by letting it come round again.") ]
                    ]
                ])
        <> [ HH.div [ HP.class_ (HH.ClassName "looper-edit-actions") ]
              [ HH.button [ HP.class_ (HH.ClassName "looper-help-btn"), HE.onClick \_ -> h.windowTo li (max 0 (winI - step)) (max 0 (winI - step) + n) ] [ HH.text ("⟵ " <> stepWord) ]
              , HH.button [ HP.class_ (HH.ClassName "looper-help-btn"), HE.onClick \_ -> h.windowTo li (min (len - n) (winI + step)) (min (len - n) (winI + step) + n) ] [ HH.text (stepWord <> " ⟶") ]
              , HH.button [ HP.class_ (HH.ClassName "looper-help-btn"), HE.onClick \_ -> h.clearWindow li ] [ HH.text "From the top" ]
              , HH.button [ HP.class_ (HH.ClassName "looper-help-btn"), HE.onClick \_ -> h.askPeaks li ] [ HH.text "Redraw" ]
              , HH.span [ HP.class_ (HH.ClassName "looper-edit-note") ]
                  [ HH.text (if windowed then "What sounds is what the harvest writes." else "The first " <> secs (min n len) <> " s.") ]
              ]
           ]
    )
  where
  li = v.focus
  len = lp.loopFrames
  windowed = lp.winOut > 0
  winI = if windowed then lp.winIn else 0
  winO = if windowed then lp.winOut else min len n
  beat = if top.barFrames > 0 && top.linkQuantum > 0.0
    then max 1 (Int.round (Int.toNumber top.barFrames / top.linkQuantum))
    else max 1 (top.sampleRate / 10)
  step = if lp.quant then beat else max 1 (top.sampleRate / 10)
  stepWord = if lp.quant then "1 beat" else "0.1 s"
  secs f = show (Int.toNumber (Int.round (Int.toNumber f / Int.toNumber top.sampleRate * 100.0)) / 100.0)
