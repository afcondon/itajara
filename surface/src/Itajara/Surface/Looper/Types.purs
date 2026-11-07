-- | The two small types the Looper page shares with whichever app holds its
-- | state: which reference panel is open, and a line of the press log. Here
-- | rather than in `Itajara.Surface.Looper.Page` so an app's state record can
-- | name them without importing a view.
module Itajara.Surface.Looper.Types
  ( LooperPanel(..)
  , LogLine
  , pushLog
  ) where

import Prelude

import Data.Array as Array
import Data.Maybe (Maybe(..))

-- | Which of the Looper page's reference panels is open, if any.
-- |
-- | **They are panels rather than page furniture because they are read, not
-- | used.** The bindings card, the MC6 bank tables and the board simulator are
-- | each consulted once and then in the way — and the page they were crowding
-- | is the one surface that has to be legible while both hands are busy.
-- |
-- | **The board is not one of these.** It was, for an afternoon, as a corner
-- | dock — and seeing it there settled the question the other way: it is small
-- | enough to keep beside the loops permanently, and a control you have to open
-- | is a control you do not use while both hands are busy. It lives in the
-- | page's right column now, with the log under it.
data LooperPanel = PanelTwister | PanelBanks | PanelRecipes | PanelBoard | PanelEdit

derive instance Eq LooperPanel

-- | One line of the looper log.
type LogLine = { text :: String, times :: Int }

-- | Add a line to the log, newest first, collapsing an immediate repeat into
-- | a count and keeping the newest sixty.
-- |
-- | **Here, beside the type, so every app with a Looper page collapses the
-- | same way.** A repeat is two presses and has to say so, but not twice as
-- | loudly; and the rule for what counts as "the same press" is one a second
-- | copy would come to disagree with. (PWYF's `Engine.pushLooperLog` is the
-- | rule this was copied from, and could call this.)
pushLog :: String -> Array LogLine -> Array LogLine
pushLog text lines = case Array.uncons lines of
  Just { head, tail } | head.text == text ->
    Array.cons (head { times = head.times + 1 }) tail
  _ -> Array.take 60 (Array.cons { text, times: 1 } lines)
