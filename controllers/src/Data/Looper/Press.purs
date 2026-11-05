-- | What every answerer of a footswitch needs besides the meaning: the rig
-- | the meaning is read against, and how late a press already is.
-- |
-- | Pure, and shared by the two things that answer the looper's switches: the
-- | looper's own process (`Itajara.Controllers.Main`) and, while it is not
-- | running, PWYF's page. Moved out of PWYF 2026-11-05 so they cannot drift
-- | (plan `docs/kb/plans/quadrat-the-sampler.md`, phase 3, step 3).
module Data.Looper.Press
  ( Deferral
  , defaultDeferral
  , deferralOf
  , rigFrom
  ) where

import Prelude

import Data.Looper.Banks as LoopBanks
import Data.Looper.Machine (Rig)
import Data.Maybe (Maybe, maybe)
import Foreign.LooperSocket (LooperState)

-- | How long the MC6 waits before it reports a gesture, by gesture: its
-- | double-tap window for a tap, its hold time for a hold.
type Deferral = { tapMs :: Number, holdMs :: Number }

-- | The MC6's own settings, as this rig has them.
defaultDeferral :: Deferral
defaultDeferral = { tapMs: 250.0, holdMs: 700.0 }

-- | How late this command already is, before it has gone anywhere.
-- |
-- | The daemon spends it where a frame matters (`@ms` in its dispatch) and
-- | strips it everywhere else, so everything can be stamped without the
-- | answerer having to know which commands care.
-- |
-- | **It is the device's own threshold rather than a measurement**, and that
-- | is what moving gesture recognition onto the MC6 cost: it reports one
-- | message once it knows, and how long it waited is a setting.
-- |
-- | **Except when there was no wait at all.** A switch carrying one meaning is
-- | programmed on `ActionPress`, which fires at press-down, so its tap is not
-- | late; saying it was would have the daemon reach back into the pre-roll
-- | ring for time that has not passed. Read from the table that programmed it.
deferralOf :: Deferral -> LoopBanks.SwitchGesture -> Number
deferralOf d g
  | LoopBanks.firesAtPressDown g.slot g.switch g.gesture = 0.0
  | otherwise = case g.gesture of
      LoopBanks.Tap -> d.tapMs
      -- Measured from the second press; not a gesture anything
      -- sample-critical hangs on.
      LoopBanks.Double -> d.tapMs
      LoopBanks.Hold -> d.holdMs

-- | The rig a press is read against: the daemon's snapshot, and the loop in
-- | hand.
rigFrom :: Maybe LooperState -> Int -> Rig
rigFrom snap focus =
  { loops: maybe [] _.loops snap
  , focus
  -- Four until the daemon says otherwise, which is the daemon's own default,
  -- and inert before it speaks: a rig with no loops has no layers to take
  -- back.
  , maxLayers: maybe 4 _.maxLayers snap
  , click: maybe false _.click snap
  , monitor: maybe false _.monitor snap
  , armDb: maybe (-36.0) _.armDb snap
  -- The daemon's boot default, so a rig that has not spoken reads as "on the
  -- bar" rather than "none", the one value that would silently change
  -- behaviour if it were wrong.
  , launchQ: maybe (-1) _.launchQ snap
  -- Names only: what a source is called is what the machine asks by.
  , sources: maybe [] (map _.name <<< _.sources) snap
  , grab: LoopBanks.grabLoops
  , grabSource: LoopBanks.grabSource
  }
