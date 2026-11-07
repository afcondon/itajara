-- | The Itajara pedal's CCs: which duty a CC on the looper's channel names.
-- |
-- | ## Why this is here
-- |
-- | The looper is a pedal in PWYF's registry (`DESIGN-LOOPER` §2), addressed
-- | by CC on channel 13 like any other pedal, and every button on the Looper
-- | page speaks that way too: `Itajara.Surface.Looper.Page`'s `gesture` takes
-- | a CC number, not a command, so a screen button and a footswitch are the
-- | same message by the time anything acts on them.
-- |
-- | That page moved into `itajara-surface` so Quadrat could draw it, and a
-- | page whose buttons speak in CCs is no use to an app that cannot read them.
-- | PWYF's own table (`Data.Looper.command`) is typed over its `CC` and
-- | `MidiValue` and sits beside the MC6 bank builder, so it could not come
-- | along; this is the same table over plain integers, which is all the page
-- | hands out. PWYF can call this one and drop its own (it has not yet).
-- |
-- | ## An addressing table, not a meaning one
-- |
-- | It says which `Duty` a CC names and nothing about what that duty does:
-- | `Data.Looper.Machine.perform` is the only place that decides. That is the
-- | lesson of 2026-08-25, when this table still named verbs and went to the
-- | socket itself, and so was a second meaning table that had drifted from the
-- | first.
module Data.Looper.PedalCC
  ( Dispatch(..)
  , channel
  , dispatch
  , isMomentary
  ) where

import Prelude

import Data.Looper.Duty (Duty(..), Subject(..))

-- | The looper's MIDI channel, from one. Free on the rig's board: the pedals
-- | hold 2-8, 10-12, 14 and 15, and channel 1 is PWYF's board-recall relay.
channel :: Int
channel = 13

-- | What a CC change should do.
data Dispatch
  -- | Ask the machine for this, about this loop.
  = Do Subject Duty
  -- | In the surface, not yet in the engine. Carries its own name so the log
  -- | says which feature is missing rather than just refusing.
  | NotYetImplemented String
  -- | The release half of a momentary, or a value the daemon does not need.
  | Ignore

derive instance Eq Dispatch

-- | Which duty a CC and its value name.
-- |
-- | **Everything here addresses `Focused`.** The pedal face, the MC6 and the
-- | page's buttons all speak about the loop in hand; the only surface that
-- | names a loop while acting on it is the Twister, which does not come
-- | through this table.
-- |
-- | Momentary controls act on a value above 63 and ignore the rest, so a
-- | footswitch's release costs nothing. Click and monitor are **set from the
-- | value, never flipped**: they are what an MC6 native toggle lands on, 127
-- | and 0 on alternate presses, so the value is the instruction.
dispatch :: Int -> Int -> Dispatch
dispatch cc value = case cc of
  1 -> onPress (Do Focused RecordLoop)
  2 -> onPress (Do Focused MultiplyLoop)
  3 -> onPress (Do Focused Undo)
  4 -> onPress (Do Focused Redo)
  5 -> onPress (Do Focused ClaimPast)
  6 -> onPress (Do Focused ClearLoop)
  -- CC 7 is the pedal's `SingleEngage`, which PWYF sends by itself whenever a
  -- board preset is recalled; a stop or start riding on it would be the
  -- machine doing something nobody asked for. The duty is reachable
  -- elsewhere; it is this CC that is spoken for.
  7 -> NotYetImplemented "play/stop on CC 7 — that CC is the pedal's engage"
  8 -> NotYetImplemented "global reverse"
  9 -> NotYetImplemented "global half speed"
  10 -> onPress (Do Focused (SpreadLoop 2))
  11 -> onPress (Do Focused RotateLoop)
  12 -> onPress (Do Focused DenseLoop)
  13 -> onPress (Do Focused ForgetLength)
  20 -> NotYetImplemented "record source select"
  21 -> NotYetImplemented "record width"
  22 -> NotYetImplemented "monitor source select"
  23 -> NotYetImplemented "monitor level"
  24 -> NotYetImplemented "send destination"
  25 -> NotYetImplemented "send source"
  60 -> NotYetImplemented "layer select"
  61 -> onPress (NotYetImplemented "layer next")
  62 -> onPress (NotYetImplemented "layer previous")
  63 -> NotYetImplemented "solo"
  64 -> NotYetImplemented "layer source"
  65 -> NotYetImplemented "layer pan"
  66 -> NotYetImplemented "layer width"
  67 -> NotYetImplemented "layer reverse"
  68 -> NotYetImplemented "layer half speed"
  70 -> NotYetImplemented "loop select"
  71 -> onPress (NotYetImplemented "loop next")
  72 -> onPress (NotYetImplemented "loop previous")
  73 -> onPress (Do Focused SaveTake)
  74 -> onPress (NotYetImplemented "loop load")
  80 -> NotYetImplemented "loop level"
  81 -> Do Focused (Click on)
  82 -> NotYetImplemented "click level"
  83 -> Do Focused (Monitor on)
  n | n >= 40 && n <= 47 -> NotYetImplemented ("layer " <> show (n - 39) <> " mute")
    | n >= 48 && n <= 55 -> NotYetImplemented ("layer " <> show (n - 47) <> " level")
    | otherwise -> Ignore
  where
  on = value > 63
  onPress d = if on then d else Ignore

-- | Momentary controls are gestures, not state: a surface that stores a CC's
-- | value should put these back to zero after acting, or a record press
-- | stays lit and a recalled preset records.
isMomentary :: Int -> Boolean
isMomentary cc
  | cc >= 1 && cc <= 6 = true
  | cc >= 10 && cc <= 13 = true
  | cc == 61 || cc == 62 = true
  | cc >= 71 && cc <= 74 = true
  | otherwise = false
