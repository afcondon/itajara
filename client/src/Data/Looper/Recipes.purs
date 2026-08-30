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
  [ { name: "A beat off the iPad, four bars of it"
    , why: "The way in when the piece starts with a drum machine rather than \
           \with you. Get to the Grab bank with a hold on G — the same switch \
           \Arm is on — and have Patterning (or anything with Start/Stop Sync \
           \ticked) loaded and stopped. Choosing the loop sets it up: on the \
           \grid, listening to the iPad. There is no input to go and find."
    , steps:
        [ says "Grab bank" "press Loop 4, or Loop 8 for the other one"
               "loop 3 records from ipad (in 5+6)."
        , says "Grab bank" "press 4 bars"
               "loop 3 starts on the grid in 1.94 s."
        , says "" "the iPad comes in on the downbeat and four bars go by"
               "loop 3 committed: 8.000 s, 1 layer playing."
        , says "Grab bank" "press Halt — the iPad is still playing otherwise"
               "asked Link to stop — peers with start/stop sync follow."
        , step "Grab bank" "press < Loops, and make guitar loops against it"
        ]
    , note: Just "**One press does four things**, and it has to: the iPad will \
                 \not play until Link's transport starts, and the take has to \
                 \open on the same bar line the transport does. So `4 bars` \
                 \starts the session, sets the grid, declares the length and \
                 \records — and the recording waits for the downbeat rather \
                 \than for your foot, which is why nothing here needs counting \
                 \in. **Two machines go in the two loops, not in one loop \
                 \twice.** Both grab loops open on the grid always, so their \
                 \bar lines are the same bar lines and the two beats agree \
                 \about where one is. Layering a second grab into the *same* \
                 \loop is an overdub, and an overdub starts at the play head \
                 \rather than on a bar line — measured — so the iPad's own \
                 \downbeat lands wherever the loop happens to be, and a \
                 \pattern longer than a bar can come back rotated. Layer by \
                 \hand, into two loops by foot. A double on the loop switch \
                 \undoes the last one. And Halt stops the Link *session*, so \
                 \it stops Ableton too — that is what a session transport is, \
                 \not a leak."
    }
  , { name: "A four-bar first loop, in time"
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
  , { name: "The click comes to you, not the other way round"
    , why: "For a take that is right except that it ran a little long or a \
           \little short against the click. Nothing is stretched — the session \
           \tempo is re-derived from what you played, which is the floor-looper \
           \move rather than the DAW one."
    , steps:
        [ step "Page 1" "press Loop 1, leave bars/Grid unlit"
        , says "Page 1" "press Arm, then play four bars"
               "loop 0 committed: 8.129 s, 1 layer playing."
        , says "" "look at bars/Grid — it already reads 4"
               "a free take counts its own bars against the clock, so there is \
               \nothing to declare and nothing to trim"
        , says "Page 2" "press Tempo"
               "tempo taken from loop 0: 8.129 s over 4 bars is 118.09 bpm."
        ]
    , note: Just "It takes the **average** over the bars, not your timing \
                 \within them: play four bars slightly slow and the click comes \
                 \to you, play them unevenly and they stay uneven. That is the \
                 \point. It also moves Ableton and anything else on the Link \
                 \session — if other loops are already down they keep their \
                 \audio and stop agreeing with the click, and the ack counts \
                 \them so you find out now rather than later."
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
        -- **The precondition, stated.** This recipe used to begin at the
        -- Multiply press, which worked whenever the rig happened to have a loop
        -- left over from the recipe before it and refused outright when it did
        -- not — "loop 0 has nothing to multiply — record a loop first". A test
        -- script whose first step depends on the state the last one left is not
        -- a test script. Recipe four says its precondition; this one now does
        -- too.
        [ step "" "record a loop as in any recipe above — Multiply extends \
                  \something, it does not start one"
        , says "Page 3" "press Multiply"
               "loop 0 multiplying from the start of this cycle (0.25 s \
               \recovered from the pre-roll) — play across as many cycles as \
               \you want, then x again."
        , step "" "play across as many cycles as you like"
        , says "Page 3" "press Multiply again"
               "loop 0 x1: now 4.535 s (1 cycles of 4.535 s) — 2 layers playing."
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
