-- | **What the looper is set to, as sentences.**
-- |
-- | The same facts as the status strip, minus the Twister's page (which is
-- | about the hand, not the rig, and has its own card), said as prose so a
-- | page can carry them as its statement. Data, not HTML: each part is words
-- | or a reading, and the page draws the readings however its statements draw
-- | them. This module knows the looper's vocabulary; it knows nothing about how
-- | a statement looks.
-- |
-- | Two sentences: the rig (clock, bar, launch), then the loop in hand (which,
-- | its input, its length, its grid).
module Itajara.Surface.Looper.Statement
  ( Reading
  , Part(..)
  , sentences
  , inputWords
  , launchWords
  , secs
  ) where

import Prelude

import Data.Array as Array
import Data.Int as Int
import Data.Maybe (Maybe(..))
import Foreign.LooperSocket (LooperState, LoopState)

-- | A value the sentence reports. `label` is what the strip calls it, kept for
-- | the tooltip; `alarm` marks the reading that changes what the rest means.
type Reading = { label :: String, value :: String, alarm :: Boolean }

data Part = Said String | Read Reading

read :: String -> String -> Part
read label value = Read { label, value, alarm: false }

alarmed :: String -> String -> Part
alarmed label value = Read { label, value, alarm: true }

-- | The looper's statement, from the snapshot and the loop in hand (the
-- | daemon's index, from zero; the sentence counts from one, like every
-- | other surface).
sentences :: Maybe LooperState -> Int -> Array (Array Part)
sentences looper focus = case looper of
  Nothing ->
    [ [ alarmed "rig" "No daemon", Said ": nothing is looping." ] ]
  Just lp ->
    [ rig lp, inHand lp (Array.index lp.loops focus) ]
  where
  rig lp =
    clock lp <> [ Said ", and loops launch " ] <> launch lp.launchQ <> [ Said "." ]

  -- Tempo first, because everything else is counted in it; and "no clock" is
  -- the alarm, since without it the bar is borrowed from loop 1 or unknown.
  clock lp
    | lp.linkAnchors == 0 =
        [ Said "There is ", alarmed "tempo" "no clock", Said ", so " ] <> bar lp
    | otherwise =
        [ Said "At "
        , read "tempo" (show (Int.round lp.linkTempo) <> " bpm")
        , Said " in "
        , read "metre" (show (Int.round lp.linkQuantum) <> "/4")
        , Said ", "
        ]
          <> bar lp

  bar lp
    | lp.barFrames == 0 = [ Said "a bar is ", alarmed "bar" "not known yet" ]
    | otherwise =
        [ Said "a bar is "
        , read "bar" (secs (Int.toNumber lp.barFrames / Int.toNumber lp.sampleRate))
        ]
          <> (if lp.linkAnchors == 0 then [ Said ", taken from loop 1" ] else [])

  -- In beats, because that is what the setting is; a bar is spelled as a bar,
  -- since in 3/4 it is three.
  launch q = case q of
    -1 -> [ Said "on ", read "launch" "the bar" ]
    0 -> [ read "launch" "freely" ]
    1 -> [ Said "on ", read "launch" "the beat" ]
    n -> [ Said "every ", read "launch" (show n <> " beats") ]

  inHand lp = case _ of
    Nothing -> [ read "selected" ("Loop " <> show (focus + 1)), Said " is selected." ]
    Just fl ->
      [ read "selected" ("Loop " <> show (focus + 1))
      , Said " is selected, listening to "
      , read "input" (inputWords lp (Just fl))
      , Said "; "
      ]
        <> length fl.cycles
        <> [ Said ", and its grid is ", read "grid" (if fl.quant then "on" else "off"), Said "." ]

  length = case _ of
    0 -> [ Said "its length is ", read "length" "not set" ]
    1 -> [ Said "it is ", read "length" "1 bar", Said " long" ]
    n -> [ Said "it is ", read "length" (show n <> " bars"), Said " long" ]

-- | The loop's source, named as the daemon names it, with stereo or mono:
-- | the half a page can be wrong about without saying so.
-- |
-- | `src` is **one-based** and indexes `sources`, so the subtraction is the
-- | seam and is done here. A number with no entry is reported as the number
-- | rather than blanked: the daemon and the page would disagree about how
-- | many inputs exist, which is worth seeing.
inputWords :: LooperState -> Maybe LoopState -> String
inputWords lp = case _ of
  Nothing -> "\x2014"
  Just f -> case Array.index lp.sources (f.src - 1) of
    Nothing -> "source " <> show f.src
    Just src -> src.name <> (if src.mono then " (mono)" else " (stereo)")

-- | The launch quantum as the strip says it.
launchWords :: Int -> String
launchWords = case _ of
  -1 -> "the bar"
  0 -> "free"
  1 -> "1 beat"
  n -> show n <> " beats"

secs :: Number -> String
secs n = show (Int.toNumber (Int.round (n * 100.0)) / 100.0) <> " s"
