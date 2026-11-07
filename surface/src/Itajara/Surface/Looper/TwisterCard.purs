-- | The Twister's current page as a small card: the looper's pages only.
-- |
-- | ## Why this is here and PWYF's card is not
-- |
-- | PWYF's `Component.Looper.TwisterCard` draws three surfaces with one
-- | renderer — a scene, a pedal's own page, and the looper's four — because
-- | PWYF can hand the Twister to any of them. Two of those three read PWYF's
-- | pedal registry, so that card cannot leave PWYF.
-- |
-- | A page that only ever shows the looper (Quadrat's Loop page) needs the
-- | third case and nothing else, and every word of it already comes from
-- | `Data.Looper.Twister`. So this is that case alone, under the same class
-- | names (`twister-card*`, in `surface/looper.css`), so the two cards look
-- | the same.
module Itajara.Surface.Looper.TwisterCard (render) where

import Prelude

import Data.Array as Array
import Data.Looper.Twister as LoopTwister
import Data.Maybe (Maybe(..), maybe)
import Halogen.HTML as HH
import Halogen.HTML.Properties as HP

-- | The looper's page `page` (from 0, as `Data.Looper.Twister` counts them):
-- | its name, then its sixteen cells four across.
-- |
-- | A cell's colour rides its left edge, from the page table's fixed tone; a
-- | loop encoder has no fixed tone (it takes the loop's phase on the device),
-- | so it is drawn without one rather than with a guess.
render :: forall w i. Int -> HH.HTML w i
render page = case Array.find (\candidate -> candidate.bank == page) LoopTwister.pages of
  Nothing -> HH.text ""
  Just shown ->
    HH.div [ HP.class_ (HH.ClassName "twister-card") ]
      [ HH.div [ HP.class_ (HH.ClassName "twister-card-title") ] [ HH.text shown.name ]
      , HH.div [ HP.class_ (HH.ClassName "twister-card-grid") ] (map cell shown.cells)
      ]
  where
  cell c =
    HH.div
      [ HP.class_ (HH.ClassName ("twister-card-cell" <> if c.name == "" then " is-dark" else ""))
      , HP.style (maybe "" (\ink -> "border-left-color:" <> ink) (c.tone >>= LoopTwister.swatchNamed))
      ]
      [ HH.text c.name ]
