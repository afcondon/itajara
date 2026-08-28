-- | The recipes: what to press, in order, and what should come back.
-- |
-- | **Data rather than prose, for the same reason the Twister card is.** There
-- | was a written list of these and it was already drifting inside an hour —
-- | a sequence that names controls is wrong the moment a control moves, and it
-- | is wrong in the one place nobody thinks to check, because it is only
-- | documentation. Here the modal renders it, the suite prints it, and
-- | `docs/RECIPES.md` is a copy of that printout rather than a second author.
-- |
-- | ## Why these are in the app at all
-- |
-- | Following a written sequence means looking away from the browser, and
-- | **Chrome throttles a background tab** — the looper stops handling Twister
-- | messages and every control reads as broken. A recipe you can follow without
-- | leaving the page is not a convenience, it is the difference between the
-- | surface working and appearing not to.
-- |
-- | ## What an `expect` is for
-- |
-- | Every step that can be wrong says what right looks like, quoted from an ack
-- | the daemon really sends. That is what makes this a test script as well as a
-- | manual: a run that disagrees with the line beside it has found something,
-- | and which of the two is wrong is then a short conversation rather than an
-- | afternoon.
module Data.Looper.Recipes
  ( Step
  , Recipe
  , preamble
  , recipes
  ) where

import Data.Maybe (Maybe(..))

-- | One move: where the control is, what to do to it, and what should come
-- | back. `expect` is `Nothing` for a step that cannot report — pressing a page
-- | button says nothing, and pretending it does would be worse than silence.
type Step = { at :: String, act :: String, expect :: Maybe String }

type Recipe =
  { name :: String
  , why :: String
  , steps :: Array Step
  , note :: Maybe String
  }

-- | The one thing you have to know before reading an expected line.
-- |
-- | **The daemon counts loops from zero and every surface here counts from
-- | one**, so the ack for Loop 1 begins "loop 0". These quotes are the
-- | daemon's own words rather than a translation of them, because a test
-- | script that says something other than what will appear on screen is a test
-- | script that sends you hunting a bug you do not have — which is precisely
-- | what the numbering did on 2026-08-27 before it was written down.
-- |
-- | The app's own log lines *are* translated (`Machine.describe`), so a step
-- | produces one line counting from one and one counting from zero. Driving
-- | that out is MVP work; saying so is today's.
preamble :: String
preamble =
  "The daemon counts loops from zero, so the ack for Loop 1 reads \"loop 0\". \
  \The lines quoted below are its own words, untranslated — the app's own log \
  \line above each of them counts from one."

step :: String -> String -> Step
step at act = { at, act, expect: Nothing }

says :: String -> String -> String -> Step
says at act expect = { at, act, expect: Just expect }

recipes :: Array Recipe
recipes =
  [ { name: "A four-bar first loop, in time"
    , why: "The ordinary way in when Link is running and you want to sit on \
           \Ableton's grid. Check the legend reads a tempo before you start."
    , steps:
        [ step "Page 1" "press Loop 1"
        , step "Page 2" "press Click"
        , says "" "it ticks four to the bar, downbeat louder"
               "a click before anything is recorded — that is the point of it"
        , step "Page 1" "press bars/Grid so it lights"
        , says "Page 1" "turn bars/Grid to 4"
               "loop 0 is set to 4 bars (8.000 s); record and it closes itself."
        , says "Page 1" "press Record, count yourself in"
               "loop 0 starts on the grid in 0.88 s"
        , says "" "play four bars and touch nothing"
               "loop 0 committed: 8.000 s, 1 layer playing."
        ]
    , note: Just "**Arm is a trap here with Grid on.** It waits for a sound and \
                 \then for the next bar line, so playing just after a line costs \
                 \almost a whole bar and the attack with it. Record and count in \
                 \— that is what the click is for. **Running it a second time?** \
                 \Clear forgets the grid flag AND the bar count, so both of \
                 \those steps are needed every time round, not only the first."
    }
  , { name: "A four-bar first loop, where your note is the downbeat"
    , why: "The same length, started by playing rather than by counting. Use it \
           \when Link is giving you a tempo rather than a performance."
    , steps:
        [ step "Page 1" "press Loop 1"
        , step "Page 1" "leave bars/Grid unlit"
        , says "Page 1" "turn bars/Grid to 4"
               "loop 0 is set to 4 bars (8.000 s); record and it closes itself."
        , step "Page 1" "press Arm"
        , says "" "play — the take starts on your note"
               "loop 0 committed: 8.000 s, 1 layer playing."
        ]
    , note: Just "That note becomes **bar one for the whole rig**: the bar's \
                 \length still comes from Link, its downbeat now comes from you. \
                 \Nothing is aligned to Ableton after this and everything is \
                 \aligned to what you played."
    }
  , { name: "A one-bar second loop against it"
    , why: "The kick after the song. A loop SHORTER than the first one, which \
           \is the thing the old model could not express at all — the pulse was \
           \loop one's length, so one cycle meant four bars."
    , steps:
        [ step "Page 1" "press Loop 2"
        , step "Page 1" "press bars/Grid — the grid is per loop, so loop 2 needs its own"
        , says "Page 1" "turn bars/Grid to 1"
               "loop 1 is set to 1 bar (2.000 s); record and it closes itself."
        , says "Page 1" "press Record"
               "loop 1 committed: 2.000 s, 1 layer playing."
        ]
    , note: Nothing
    }
  , { name: "A bar, spread over four, landing on the third"
    , why: "One phrase placed in a longer loop rather than repeated through it. \
           \The layer keeps its own length throughout — only where it lands moves."
    , steps:
        [ step "" "record a one-bar loop as above"
        , says "Page 1" "turn bars/Grid to 4"
               "loop 1 is 4 bars (8.000 s); its layers keep their own lengths."
        , says "Page 3" "turn every to 4"
               "layer 1 sounds once every 4, on slot 1."
        , says "Page 3" "turn slot to 3"
               "layer 1 is on slot 3 of 4."
        ]
    , note: Just "Watch the waveform rather than the words: the bar moves to the \
                 \third of four empty ones. That picture is why this is three \
                 \knobs and not a sentence about how often something happens."
    }
  , { name: "Multiply — playing the length instead of naming it"
    , why: "For when you do not know how many bars yet. You count with bars and \
           \you play with this; both are worth having."
    , steps:
        [ step "Page 3" "press Multiply"
        , says "" "the write head opens"
               "loop 0 multiplying from the start of this cycle — play across as \
               \many cycles as you want, then x again."
        , step "" "play across as many cycles as you like"
        , says "Page 3" "press Multiply again"
               "rounds to whole cycles, waiting for the boundary if it rounded up"
        ]
    , note: Just "It feels like an overdub because it **is** one — an overdub \
                 \that also lengthens the loop. Refused unless the loop is \
                 \playing forwards at unity."
    }
  , { name: "Declaring a length, with no clock"
    , why: "Only reachable with Link off. With a clock, bars resizes instead — \
           \and the ack always says which of the two happened."
    , steps:
        [ says "Page 1" "press Record, play, press Record again"
               "the one place a closing press still survives"
        , says "Page 1" "turn bars/Grid to 4"
               "loop 0 is 4 bars — the bar is now 2.000 s. Nothing was moved."
        ]
    , note: Just "No audio changed. The pulse is a quarter of what you played, \
                 \so a one-bar loop 2 is now possible."
    }
  ]
