-- | A layer's envelope, drawn.
-- |
-- | The daemon sends a 48-bucket envelope per layer in every snapshot, in
-- | *arena* positions. What a slot shows is the *cycle* — the loop as it now
-- | plays, through its window and from its rotated start — so `viewOf` maps
-- | one to the other and `wave` draws the result as one SVG path. Moved here
-- | from producing-with-your-feet's `Component.Looper.Slots` on 2026-09-04 so
-- | that the Friend draws the same picture from the same numbers.
module Itajara.Surface.Wave
  ( viewOf
  , wave
  , waveEdge
  , svgEl
  , sAttr
  ) where

import Prelude

import Data.Array as Array
import Data.Int (toNumber)
import Data.Maybe (fromMaybe)
import Data.Number (pow)
import Data.String (joinWith)
import Foreign.LooperSocket (LayerShape, LoopState)
import Halogen (AttrName(..), ElemName(..), Namespace(..))
import Halogen.HTML as HH
import Halogen.HTML.Properties as HP

-- | The layer's envelope as the loop now plays it: through the window and
-- | from the rotated start, with the silence a window adds drawn as silence.
-- | The envelope arrives in arena positions; the slot shows the cycle.
-- |
-- | Only for a layer the length of the loop — a sparse layer tiles by its
-- | own length and its blocks already say where it sounds.
viewOf :: LoopState -> LayerShape -> Array Int
viewOf st sh
  | (st.winOut == 0 && st.rot == 0) || sh.len /= st.loopFrames || Array.null sh.env = sh.env
  | otherwise =
      let
        n = Array.length sh.env
        len = st.loopFrames
        start = if st.winOut == 0 then 0 else st.winIn
        span = max 1 (if st.winOut == 0 then len else st.winOut - st.winIn)
        -- Cycle position `c` of `n` -> arena position, as the daemon places it.
        arena c = start + ((c * span / n + st.rot) `mod` span)
        bucket a
          | a < 0 || a >= len = 0
          | otherwise = fromMaybe 0 (Array.index sh.env (a * n / len))
      in
        map (bucket <<< arena) (Array.range 0 (n - 1))

-- | The layer's shape, mirrored about the middle the way a waveform is read.
-- |
-- | `preserveAspectRatio="none"` so one path stretches to whatever width the
-- | block happens to be — the blocks are laid out by the tiling and their width
-- | is not this module's to know.
-- |
-- | Nothing here rescales the peaks. They arrive absolute, on a decibel curve
-- | with a -60 dBFS floor, and drawing them any other way would throw away the
-- | one thing the picture is insurance against.
wave :: forall w i. Array Int -> Array (HH.HTML w i)
wave env
  | Array.null env = []
  | otherwise =
      [ svgEl "svg"
          [ sAttr "viewBox" ("0 0 " <> show (Array.length env - 1) <> " 2")
          , sAttr "preserveAspectRatio" "none"
          -- **`sAttr`, not `HP.class_`.** See `svgEl` below: on an SVG element
          -- the property form silently does nothing, and the symptom is a
          -- picture that renders perfectly at the wrong size.
          , sAttr "class" "loop-wave"
          ]
          [ svgEl "path" [ sAttr "d" path ] [] ]
      ]
  where
  -- Out along the top and back along the bottom, so the fill is the envelope
  -- rather than an outline of half of it.
  path =
    joinWith " "
      ( [ "M0," <> show (top 0) ]
          <> Array.mapWithIndex (\i v -> "L" <> show i <> "," <> show (edge v)) env
          <> Array.reverse
              (Array.mapWithIndex (\i v -> "L" <> show i <> "," <> show (2.0 - edge v)) env)
          <> [ "Z" ]
      )
  top i = waveEdge (fromMaybe 0 (Array.index env i))
  edge = waveEdge

-- | A peak byte to the top edge of the mark, in a viewBox two units tall.
-- |
-- | **Loud is more ink.** The first version filled the block and drew the
-- | envelope in the background colour, so a loud layer was *less* mark than a
-- | quiet one — inverted, and instantly wrong to look at once it was on screen.
-- |
-- | **Linear, from a byte that is logarithmic.** The daemon sends a layer's
-- | peaks on a decibel scale with the floor at -60 dBFS, which keeps quiet
-- | material from vanishing into one byte — and drawn as it arrives, that
-- | made every loop a fat band, because -30 dB sat halfway up the block. The
-- | Edit panel draws linear and looked like the audio; so does this. A small
-- | floor keeps a silent layer visible as a line: a mark you cannot see reads
-- | as one that is not there, which is the opposite of what this is for.
waveEdge :: Int -> Number
waveEdge v = 1.0 - max 0.06 amplitude
  where
  db = -60.0 + toNumber v / 255.0 * 60.0
  amplitude = if v <= 0 then 0.0 else pow 10.0 (db / 20.0)

-- | Just enough SVG to draw one shape: Halogen ships the namespace-aware
-- | constructor and this needs two elements and a few attributes.
-- |
-- | **Classes on SVG go through `sAttr`, never `HP.class_`.** `HP.class_` sets
-- | the DOM *property*, and `SVGElement.className` is a read-only
-- | `SVGAnimatedString` — so the assignment does nothing, quietly, and every
-- | rule keyed on that class simply never applies. The symptom is not a missing
-- | element: the shape renders correctly and at completely the wrong size,
-- | because with no CSS the browser falls back to sizing the SVG from its
-- | viewBox aspect ratio.
svgEl :: forall r w i. String -> Array (HH.IProp r i) -> Array (HH.HTML w i) -> HH.HTML w i
svgEl name = HH.elementNS (Namespace "http://www.w3.org/2000/svg") (ElemName name)

sAttr :: forall r i. String -> String -> HH.IProp r i
sAttr k v = HP.attr (AttrName k) v
