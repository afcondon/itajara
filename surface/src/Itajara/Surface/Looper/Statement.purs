-- | **What the looper is set to, as sentences.**
-- |
-- | The same facts as the status strip, minus the Twister's page (which is
-- | about the hand, not the rig, and has its own card), said as prose so a
-- | page can carry them as its statement. Data, not HTML: each part is words
-- | or a reading, and the page draws the readings however its statements draw
-- | them. This module knows the looper's vocabulary; it knows nothing about how
-- | a statement looks.
-- |
-- | Two sentences: the rig (clock, bar, launch, the click, the arm level), then
-- | the loop in hand (which, its input, its length, its grid, and on tape what
-- | a pass leaves). The settings in them are parts you change in place: the
-- | statement replaced the Transport face, so what the face set, the sentence
-- | sets. Its verbs (record, multiply, take, undo) are not settings and are
-- | not here; they are on the MC6 and the Twister, both drawn beside it.
module Itajara.Surface.Looper.Statement
  ( Reading
  , Part(..)
  , Option
  , Settings
  , sentences
  , warnings
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

-- | A part of a sentence: words, a reading, or a setting changed in place.
-- |
-- | * `Toggle` — a word that flips when pressed (the click: on, off).
-- | * `Field` — a number typed or stepped, in `unit`, from `min` to `max`;
-- |   `intent` takes the text, as the page's handlers do, and the daemon
-- |   clamps.
data Part i
  = Said String
  | Read Reading
  | Toggle { label :: String, value :: String, intent :: i }
  | Field
      { label :: String
      , value :: String
      , unit :: String
      , min :: Number
      , max :: Number
      , step :: Number
      , intent :: String -> i
      }
  | Choice
      { label :: String
      , value :: String
      , options :: Array Option
      , intent :: String -> i
      }

-- | One of a `Choice`'s options: what is sent (`value`), what is read
-- | (`text`), and whether it can be chosen (`off`: a source not connected,
-- | or "not set", which describes a loop but is not something you can ask for).
type Option = { value :: String, text :: String, off :: Boolean }

-- | What the settings in the sentence ask for: the page's own handlers. The
-- | strings are the option values and field text, parsed by the page.
type Settings i =
  { setClick :: Boolean -> i
  , setArm :: String -> i
  , setFeedback :: String -> i
  , setTone :: String -> i
  , setLaunch :: String -> i
  , setSource :: String -> i
  , setGrid :: Boolean -> i
  , setBars :: String -> i
  }

read :: forall i. String -> String -> Part i
read label value = Read { label, value, alarm: false }

alarmed :: forall i. String -> String -> Part i
alarmed label value = Read { label, value, alarm: true }

-- | The looper's statement, from the snapshot and the loop in hand (the
-- | daemon's index, from zero; the sentence counts from one, like every
-- | other surface).
sentences :: forall i. Settings i -> Maybe LooperState -> Int -> Array (Array (Part i))
sentences set looper focus = case looper of
  Nothing ->
    [ [ alarmed "rig" "No daemon", Said ": nothing is looping." ] ]
  Just lp ->
    [ rig lp, inHand lp (Array.index lp.loops focus) ]
  where
  rig lp =
    clock lp <> [ Said ", and loops launch ", launch lp.launchQ, Said ". " ]
      <> clickAndArm lp

  -- | **Rig-wide, set once a session**: the click, and how loud a sound has to
  -- | be to start a level-armed loop (a calibration of the room and the
  -- | instrument, which is why it is a number here and not a knob).
  clickAndArm lp =
    [ Said "The click is "
    , Toggle
        { label: "the click: press to turn it " <> if lp.click then "off" else "on"
        , value: if lp.click then "on" else "off"
        , intent: set.setClick (not lp.click)
        }
    , Said ", and an armed loop starts when the input passes "
    , Field
        { label: "how loud a sound has to be to start a level-armed loop"
        , value: show (Int.round lp.armDb)
        , unit: " dBFS"
        , min: -80.0
        , max: 0.0
        , step: 1.0
        , intent: set.setArm
        }
    , Said "."
    ]

  -- Tempo first, because everything else is counted in it; and "no clock" is
  -- the alarm, since without it the bar is borrowed from loop 1 or unknown.
  clock lp
    | lp.linkAnchors == 0 =
        [ Said "There is ", alarmed "tempo" "no clock", Said ", so " ] <> bar lp
    | otherwise =
        [ Said "At "
        , read "tempo" (show (Int.round lp.linkTempo) <> " bpm")
        , Said " in "
        -- The metre is Link's, set where the tempo is (Ableton, or any Link
        -- peer): the looper has no verb for it, so it is read, not chosen.
        , read "metre, from Link" (show (Int.round lp.linkQuantum) <> "/4")
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
  -- In beats, because that is what the setting is: a bar is spelled as a
  -- bar, since in 3/4 it is three. A quantum the menu does not offer is shown
  -- as itself rather than snapped to one it does.
  launch q =
    Choice
      { label: "what a launch waits for (rig-wide)"
      , value: show q
      , options:
          map (\n -> { value: show n, text: launchPhrase n, off: false })
            (if Array.elem q launches then launches else Array.snoc launches q)
      , intent: set.setLaunch
      }

  launches = [ -1, 0, 1, 2, 3, 4 ]

  launchPhrase = case _ of
    -1 -> "on the bar"
    0 -> "freely"
    1 -> "on the beat"
    n -> "every " <> show n <> " beats"

  inHand lp = case _ of
    Nothing -> [ read "selected" ("Loop " <> show (focus + 1)), Said " is selected." ]
    Just fl ->
      [ read "selected" ("Loop " <> show (focus + 1))
      , Said " is selected, listening to "
      , Choice
          { label: "the input this loop records from"
          , value: show fl.src
          , options:
              Array.mapWithIndex
                (\i src ->
                  { value: show (i + 1)
                  , text: src.name <> (if src.mono then " (mono)" else " (stereo)")
                      <> (if src.available then "" else " \x2014 off")
                  , off: not src.available
                  })
                lp.sources
          , intent: set.setSource
          }
      , Said "; it is "
      , length fl.cycles
      , Said ", and its grid is "
      , Toggle
          { label: "whether this loop keeps to the grid: press to turn it " <> if fl.quant then "off" else "on"
          , value: if fl.quant then "on" else "off"
          , intent: set.setGrid (not fl.quant)
          }
      , Said "."
      ]
        <> tape fl

  -- | **Only on tape.** What a Revox pass leaves of what was under it, and how
  -- | much of the top it keeps: riding these is how the mode is played, so they
  -- | are said when the loop is a tape and not otherwise.
  tape fl
    | not fl.revox = []
    | otherwise =
        [ Said " On tape, each pass leaves "
        , Field
            { label: "what a pass leaves of what was under it"
            , value: show fl.fbDb
            , unit: " dB"
            , min: -24.0
            , max: 0.0
            , step: 0.5
            , intent: set.setFeedback
            }
        , Said " of what was under it, keeping up to "
        , Field
            { label: "how much of the top a pass keeps (20000: all of it)"
            , value: show (Int.round fl.toneHz)
            , unit: " Hz"
            , min: 1000.0
            , max: 20000.0
            , step: 250.0
            , intent: set.setTone
            }
        , Said "."
        ]

  -- | How many bars. "Not set" describes an empty loop and cannot be asked
  -- | for, so it is offered only as the reading and never as a choice; a length
  -- | the menu does not list is shown as itself.
  length n =
    Choice
      { label: "how many bars this loop is (on an empty loop: the length the next take will have)"
      , value: show n
      , options:
          (if n == 0 then [ { value: "0", text: "of no set length", off: true } ] else [])
            <> map (\b -> { value: show b, text: barsPhrase b, off: false })
                 (if n == 0 || Array.elem n lengths then lengths else Array.sort (Array.snoc lengths n))
      , intent: set.setBars
      }

  lengths = [ 1, 2, 3, 4, 6, 8, 12, 16, 24, 32 ]

  barsPhrase = case _ of
    1 -> "1 bar long"
    b -> show b <> " bars long"

-- | **Only the exception**: the one readout of the old Transport face that is
-- | worth saying, and only while it is wrong. Alignment locks on the first
-- | input buffer; until then a recording lands late by an unknown amount.
warnings :: Maybe LooperState -> Array String
warnings = case _ of
  Just lp | not lp.calibrated ->
    [ "Alignment is not locked yet: the looper is waiting for its first input \
      \buffer, and until then a recording lands late by an unknown amount." ]
  _ -> []

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
