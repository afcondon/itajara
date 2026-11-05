-- | **Who holds the Twister**: the table both programs read, so neither has to
-- | ask the other.
-- |
-- | The rule (AC, 2026-11-05): an MC6 bank can name who holds the Twister, and
-- | when it does, that holder has it. On the looper's banks the looper's own
-- | process holds it (its pages); on a bank with a scene, PWYF does (the
-- | scene). On every other bank, focus decides, and focus is PWYF's.
-- |
-- | Each program reads the same bank from the same MC6 frame and looks it up
-- | here, so the process and PWYF agree on the looper's banks without a word
-- | between them. Scenes are PWYF's data (a control bank's page), so PWYF adds
-- | its scene banks to this; the process needs to know only its own.
-- |
-- | Holding is on Helm's hub: the holder claims the Twister's output, and
-- | whoever should have it asks (`Helm.WebMIDI.wantName`) and the holder
-- | grants it or says why not. The claim is the record; nobody paints a
-- | Twister they do not hold.
-- |
-- | Not a scene: a scene is one flat page borrowed from pedals, and the
-- | looper is pages of its own with a pager and live colours, painted by
-- | another process. Both are **holders**; that is the shared idea.
module Data.Looper.TwisterHolder
  ( loopBankBase
  , looperHoldsBank
  , looperProcess
  ) where

import Prelude

import Data.Array as Array
import Data.Looper.Duty (allSlots)

-- | The wire number of the first of the looper's MC6 banks (the device shows
-- | it as one more: bank 3). The looper's banks follow it, one per
-- | `BankSlot`, as `Data.Looper.Banks.banks` lays them out.
loopBankBase :: Int
loopBankBase = 2

-- | Whether a bank (wire number) is one of the looper's, and so names the
-- | looper as the Twister's holder.
looperHoldsBank :: Int -> Boolean
looperHoldsBank bank = bank >= loopBankBase && bank < loopBankBase + Array.length allSlots

-- | The looper's process, by the name it gives Helm's hub: who PWYF hands the
-- | Twister to when the looper should hold it.
looperProcess :: String
looperProcess = "itajara-controllers"
