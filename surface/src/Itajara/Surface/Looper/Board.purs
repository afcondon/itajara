-- | The board, on screen, with every switch live.
-- |
-- | ## Why this exists
-- |
-- | **To take the MC6 out of the loop.** Debugging the looper surface has meant
-- | debugging two things at once: what the app does with a press, and whether
-- | the press arrived at all. Those failed together twice in one day and looked
-- | identical from a chair — a bank jump ate the CC beside it, so the board
-- | still navigated and every press *looked* like it worked while the app was
-- | never told which loop had been chosen. No amount of reasoning about the
-- | meaning table could have found that, because the meaning table was never
-- | reached.
-- |
-- | So: the same twelve switches, on the screen, clickable. If a press works
-- | here and not underfoot, the fault is in the wire or in what the device was
-- | programmed with. If it fails here too, it is ours. One bisection, and it
-- | costs a click instead of a session.
-- |
-- | ## It must be the same pipeline, not a similar one
-- |
-- | A button that called `Machine.act` directly would be a *second* input path,
-- | and a second input path agrees with the first right up until the moment it
-- | matters. So this emits the **MIDI bytes the MC6 would have sent** and hands
-- | them to the same handler the port feeds — through `decodeSwitch`, through
-- | `followBoard`, through the meaning table, out to the daemon. Everything
-- | between the wire and the sound is exercised; only the wire is skipped, which
-- | is exactly the thing being isolated.
-- |
-- | That also means this panel cannot drift into being a nicer interface than
-- | the pedal. It is a pedal simulator, and if a gesture is awkward here it is
-- | awkward there.
-- |
-- | ## Board order, not index order
-- |
-- | The MC6 numbers from the bottom: A B C is the near row, under your toes. A
-- | view that lays these out in index order is describing a board nobody is
-- | standing on, so the rows come from `Data.Looper.Banks.boardRows` rather than
-- | from a copy made here — the same fact, read from the same place as the
-- | device programming.
module Itajara.Surface.Looper.Board (render) where

import Prelude

import Data.Array as Array
import Data.Looper.Banks as LB
import Data.Maybe (Maybe(..), fromMaybe)
import Halogen.HTML as HH
import Halogen.HTML.Events as HE
import Halogen.HTML.Properties as HP

-- | The panel, given a way to turn a press into whatever the app calls an
-- | action.
-- |
-- | Taking the constructor as an argument rather than importing the app's action
-- | type keeps this a view: it knows what a press *is* and nothing about what
-- | happens next.
render
  :: forall w i
   . (LB.BankSlot -> Int -> LB.Gesture -> i)
  -> LB.Face
  -> HH.HTML w i
render press fc = case LB.faceSlot fc of
  -- The board has left the looper family. Naming its switches here would be
  -- inventing a board, which is the fault this module's own legend once had.
  Nothing ->
    HH.div [ HP.class_ (HH.ClassName "board-sim") ]
      [ header "the board is on another bank" ]
  Just slot ->
    HH.div [ HP.class_ (HH.ClassName "board-sim") ]
      [ header (LB.slotName slot)
      , HH.div [ HP.class_ (HH.ClassName "board-sim-rows") ]
          (map (row press slot) LB.boardRows
            <> [ HH.div [ HP.class_ (HH.ClassName "board-sim-fs3x") ]
                   (map (row press slot) fs3xBoxes) ])
      ]

header :: forall w i. String -> HH.HTML w i
header name =
  HH.div [ HP.class_ (HH.ClassName "board-sim-head") ]
    [ HH.span [ HP.class_ (HH.ClassName "board-sim-name") ] [ HH.text name ]
    , HH.span [ HP.class_ (HH.ClassName "board-sim-hint") ]
        [ HH.text "clicking sends what the MC6 sends" ]
    ]

-- | The two FS3X units, as they sit on the floor: side by side, G H I in the
-- | left box and J K L in the right, each box one row of three.
-- |
-- | Measured 2026-11-05 by tapping each switch with a listener on the hub (the
-- | left box sends G-I's CCs, the right J-L's). This used to stack them like
-- | the MC6's own two rows, J K L above G H I, on an assumption it marked as
-- | unchecked; AC read the top row as the left box.
fs3xBoxes :: Array (Array Int)
fs3xBoxes = map (map (_ + LB.mc6OwnSwitches)) [ [ 0, 1, 2 ], [ 3, 4, 5 ] ]

row :: forall w i. (LB.BankSlot -> Int -> LB.Gesture -> i) -> LB.BankSlot -> Array Int -> HH.HTML w i
row press slot ixs =
  HH.div [ HP.class_ (HH.ClassName "board-sim-row") ]
    (map (switch press slot) ixs)

switch :: forall w i. (LB.BankSlot -> Int -> LB.Gesture -> i) -> LB.BankSlot -> Int -> HH.HTML w i
switch press slot i = case LB.dutiesAt slot i of
  Just d | d.tap /= LB.Nothing_ ->
    HH.div [ HP.class_ (HH.ClassName "board-sim-key") ]
      [ HH.button
          [ HP.class_ (HH.ClassName "board-sim-tap")
          , HP.title (LB.dutyName d.tap <> " — CC " <> show (LB.switchCC slot i))
          , HE.onClick \_ -> press slot i LB.Tap
          ]
          [ HH.span [ HP.class_ (HH.ClassName "board-sim-letter") ]
              [ HH.text (fromMaybe (show i) (LB.switchLetter i)) ]
          , HH.span [ HP.class_ (HH.ClassName "board-sim-label") ]
              [ HH.text (LB.dutyLabel d.tap) ]
          ]
      -- The other two gestures get their own buttons rather than a modifier
      -- key, because a modifier is a thing to remember and the point of this
      -- panel is to remove things to remember while something is broken.
      , HH.div [ HP.class_ (HH.ClassName "board-sim-more") ]
          (Array.catMaybes
            [ extra press slot i LB.Double "\x00d7\x00d7" d.double
            , extra press slot i LB.Hold "hold" d.hold
            ])
      ]
  -- Blank switches are drawn rather than skipped, so the grid keeps the shape
  -- of the board and a letter always means the same position.
  _ ->
    HH.div [ HP.class_ (HH.ClassName "board-sim-key is-blank") ]
      [ HH.span [ HP.class_ (HH.ClassName "board-sim-letter") ]
          [ HH.text (fromMaybe (show i) (LB.switchLetter i)) ]
      ]

extra
  :: forall w i
   . (LB.BankSlot -> Int -> LB.Gesture -> i)
  -> LB.BankSlot -> Int -> LB.Gesture -> String -> Maybe LB.Duty
  -> Maybe (HH.HTML w i)
extra press slot i g word = case _ of
  Nothing -> Nothing
  Just d -> Just
    (HH.button
      [ HP.class_ (HH.ClassName "board-sim-extra")
      , HP.title (LB.gestureName g <> ": " <> LB.dutyName d)
      , HE.onClick \_ -> press slot i g
      ]
      [ HH.text (word <> " " <> LB.dutyLabel d) ])
