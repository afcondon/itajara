-- | The MC6's way into the machine: a switch press becomes duties, and the
-- | duties become actions.
-- |
-- | Split out of `Data.Looper.Machine` on 2026-09-04 so that the machine no
-- | longer imports `Data.Looper.Banks` — the one thing standing between the
-- | looper's meaning and a controller-free build of it (`DESIGN-HARVEST.md`
-- | §6). Nothing here changed but the address.
module Data.Looper.Switchboard
  ( act
  ) where

import Prelude

import Data.Looper.Banks (SwitchGesture)
import Data.Looper.Banks as LB
import Data.Looper.Duty (BankSlot, Duty(..), Subject(..))
import Data.Looper.Machine (Action(..), Rig, perform, performPress, slotWord)
import Data.Maybe (Maybe(..))

-- | The whole meaning table.
-- |
-- | **Keyed by what the switch is for, never by which switch it is.** This used
-- | to be a set of per-bank tables indexed by a switch number, running in
-- | parallel with the labels in `Data.Looper.Banks` and joined to them by
-- | nothing but that number. The layout said switch 9 was "Clear"; this said
-- | switch 9 sent `c`; moving Clear would have left a switch labelled one thing
-- | and doing another, and nothing would have failed to compile.
-- |
-- | Now a press is resolved to a `Duty` first and the meaning is a total
-- | function of that. The label on the pedal, the words on screen and the
-- | command on the wire are three renderings of one value, so they cannot
-- | disagree — and a new duty is a compile error here until it is given a
-- | meaning, rather than a switch that silently does nothing.
-- | **One decoder of three.** `perform` is the meaning; this is the MC6's way
-- | in, and the page and the Twister have their own. See `DESIGN-TWISTER` §4.
act :: Rig -> SwitchGesture -> Array Action
act rig p = case LB.dutiesAt p.slot p.switch of
  Nothing -> [ missing p.slot p.switch ]
  Just s -> case p.gesture, LB.dutyFor p.gesture s of

    -- **All three go the same way now**, which they did not when a loop switch
    -- carried a different verb on each. A double no longer means overdub and a
    -- hold no longer opens the config bank, because both have a switch of their
    -- own on `LoopPage` with their name printed on it — so the gesture chooses
    -- which duty the switch is showing, and the duty decides everything else.
    -- The MC6 always speaks about the focused loop: six switches cannot name
    -- eight loops as well as saying what to do to one.
    -- **A loop switch names its loop, so a gesture on it acts on that loop.**
    --
    -- The line above this used to be the whole story and said why: six switches
    -- cannot name eight loops *as well as* saying what to do to one. That is
    -- still true of every other switch in the family — they act on whatever is
    -- in hand — but a loop switch is the exception it was always going to be,
    -- because naming the loop is the only thing it does.
    --
    -- It matters because of how the device reports a double: the tap is
    -- suppressed, so nothing ever says you touched loop 3, and `Focused` would
    -- have undone whichever loop you happened to have chosen before. Through
    -- `performPress` the subject prepends its own focus, so a double takes the
    -- loop in hand *and* undoes it, which is what a foot on that switch means.
    g, Just d -> case s.tap of
      SelectLoop n | g /= LB.Tap -> performPress rig (OnLoop n) d
      _ -> perform rig Focused d

    -- **The board and this table have fallen out of step.** A gesture only
    -- arrives because the device was programmed to send it, and the device is
    -- programmed from this table — so an unbound one means the board is running
    -- an older upload. Said out loud rather than swallowed: that is exactly the
    -- disagreement a press should be able to report.
    g, Nothing ->
      [ Unavailable
          ( "the board sent a " <> LB.gestureName g <> " on "
              <> LB.dutyLabel s.tap
              <> ", which this bank does not carry — reprogram the MC6"
          )
      ]

missing :: BankSlot -> Int -> Action
missing slot i = Unavailable (slotWord slot <> " switch " <> show i <> " has nothing on it")
