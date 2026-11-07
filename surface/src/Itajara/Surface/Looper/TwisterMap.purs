-- | The Twister's layout, printed in the Looper page's bindings panel.
-- |
-- | **Every word of it comes from `Data.Looper.Twister`.** Nothing here knows
-- | what any encoder does; it knows how to draw a four-by-four grid of things
-- | that describe themselves. Move a control and the card moves with it, which
-- | is the only way a printed layout stays true — a cheat sheet typed out beside
-- | the table it describes is right when written and wrong in the one place
-- | nobody thinks to check, because it is only documentation.
-- |
-- | The same lesson as `Data.Looper.Banks.auxLegend`, which exists because three
-- | screens in one day restated something that module already knew.
-- |
-- | ## The colours are a claim, not an observation
-- |
-- | The tones are what the app *asks* the device for, and no one has yet
-- | compared them with what it does (`DESIGN-TWISTER` §12). The card says so,
-- | because a legend that quietly presented an unverified intention as fact
-- | would be the thing to blame when a knob turns out to be the wrong colour.
module Itajara.Surface.Looper.TwisterMap (render, tabs) where

import Prelude

import Data.Array as Array
import Data.Looper.Twister as TW
import Data.Maybe (Maybe(..))
import Halogen.HTML as HH
import Halogen.HTML.Events as HE
import Halogen.HTML.Properties as HP

-- | The card. Takes whether a Twister output is selected, which page the app is
-- | showing, which block the device last spoke from, and what to do about
-- | wanting a different page.
-- |
-- | **One page at a time, since 2026-08-27.** It printed both side by side, in
-- | a panel 1180px wide that covered the loops it was describing. Two things
-- | made that the wrong shape. The first is that you open this card to answer
-- | "what does *this* knob do" — a question about the page you are on, so the
-- | other page is furniture. The second is that the pager works now: turning
-- | the bottom-right encoder moves `showing`, so the card follows the device
-- | without anything here doing the following. Printing both pages was a
-- | workaround for a page turn you could not perform.
-- |
-- | What is left is narrow enough to read beside the loops instead of on top of
-- | them, which is the whole gain.
render :: forall w i. Boolean -> Int -> Maybe Int -> (Int -> i) -> HH.HTML w i
render connected showing heard goTo =
  HH.div [ HP.class_ (HH.ClassName "twister-map") ]
    [ HH.div [ HP.class_ (HH.ClassName "twister-map-body") ]
        ( exception
            <> [ tabs showing goTo ]
            <> body
            <> [ HH.p [ HP.class_ (HH.ClassName "twister-map-caveat") ]
                   -- Kept, and kept short. It says the one thing about this
                   -- card that you cannot check by looking at it.
                   [ HH.text "The colours are what the app asks for, not what \
                             \anyone has seen the device do." ]
               ]
        )
    ]
  where
  -- Printed only when it has something to say. In the ordinary case the tab
  -- strip already says where we are, so a line repeating it is a line in the
  -- way.
  exception = case status connected heard of
    Nothing -> []
    Just msg ->
      [ HH.p [ HP.class_ (HH.ClassName "twister-map-status") ] [ HH.text msg ] ]

  -- The page the app is on, and nothing else. `Nothing` cannot happen —
  -- `twisterPage` is clamped to the pages that exist — so it draws nothing
  -- rather than inventing a fallback that would only ever be wrong.
  body = case Array.find (\p -> p.bank == showing) TW.pages of
    Nothing -> []
    Just p ->
      [ HH.p [ HP.class_ (HH.ClassName "twister-page-note") ] [ HH.text p.note ]
      , HH.div [ HP.class_ (HH.ClassName "twister-grid") ] (map cell p.cells)
      ]
        -- Only where it applies. The key is titled "on page 1" because that is
        -- the only page whose rings are loops, and carrying it onto page 2 was
        -- something the two-column layout did by accident.
        <> if p.bank == 0 then [ phases ] else []

-- | Anything worth interrupting the card to say. `Nothing` in the ordinary
-- | case, which is most of the time.
-- |
-- | **This no longer compares the page with the block, and that distinction is
-- | the point.** It used to warn whenever `twisterHeardBank` differed from the
-- | page being shown, back when a page *was* a device block. Since the device
-- | is pinned to `TW.pinnedLayer` and the app owns paging outright, those are
-- | two different kinds of fact and comparing them would fire the warning on
-- | every visit to page 2.
-- |
-- | What is still worth saying is drift: a device that has wandered off the
-- | pinned block and not been put back. The app (PWYF's
-- | `App.TwisterMidiReceived`) pins it again on the next message, so seeing
-- | this at all means the device is refusing —
-- | and then every encoder is being read against the wrong sixteen CCs, which
-- | is exactly the kind of silence worth breaking.
status :: Boolean -> Maybe Int -> Maybe String
status false _ = Just "No Twister output selected — nothing here is reaching a device."
status true heard = case heard of
  Just b | b /= TW.pinnedLayer ->
    Just ("The device is on its own block " <> show (b + 1)
            <> " and has not gone back to block 1, so the encoders are not \
               \where this card says they are.")
  _ -> Nothing

-- | The strip of page tabs, here and on the Looper view's live card.
tabs :: forall w i. Int -> (Int -> i) -> HH.HTML w i
tabs showing goTo =
  HH.div [ HP.class_ (HH.ClassName "twister-tabs") ] (map (tab showing goTo) TW.pages)

-- | The pages, as a strip. Two jobs in one row of chrome: it says which page is
-- | showing, and it is how you get to the other one without reaching for the
-- | knob.
-- |
-- | It also moves on its own. Turn the pager and `showing` changes underneath
-- | this, so the strip is a readout of the encoder as much as a control — which
-- | is why the current page is not a button. There is nothing to press.
tab :: forall w i. Int -> (Int -> i) -> TW.Page -> HH.HTML w i
tab showing goTo p =
  let
    label =
      [ HH.span [ HP.class_ (HH.ClassName "twister-tab-num") ] [ HH.text (show (p.bank + 1)) ]
      , HH.text p.name
      ]
  in
    if showing == p.bank then HH.span [ HP.class_ (HH.ClassName "twister-tab on") ] label
    else HH.button
      [ HP.class_ (HH.ClassName "twister-tab")
      , HE.onClick \_ -> goTo p.bank
      , HP.title "Turn to this page. The same thing the pager encoder does, and it carries the knob with it."
      ]
      label

-- | One encoder, drawn where it physically sits: four across, four down, in
-- | index order, because that is how the device is laid out and any other
-- | arrangement would be a second mapping to hold in your head while using the
-- | first one.
cell :: forall w i. TW.Cell -> HH.HTML w i
cell c
  | c.name == "" = HH.div [ HP.class_ (HH.ClassName "twister-cell is-empty") ] []
  | otherwise =
      HH.div [ HP.class_ (HH.ClassName "twister-cell") ]
        [ HH.div [ HP.class_ (HH.ClassName "twister-cell-head") ]
            [ HH.span [ HP.class_ (HH.ClassName "twister-cell-name") ] [ HH.text c.name ]
            , case c.tone of
                Nothing -> HH.text ""
                Just t -> HH.span
                  [ HP.class_ (HH.ClassName ("twister-swatch tone-" <> t))
                  , HP.title t
                  ] []
            ]
        , line "turn" c.turn
        , line "press" c.press
        , case c.shows of
            Nothing -> HH.text ""
            Just s -> HH.div [ HP.class_ (HH.ClassName "twister-cell-shows") ] [ HH.text s ]
        ]
  where
  line _ Nothing = HH.text ""
  line label (Just what) =
    HH.div [ HP.class_ (HH.ClassName "twister-cell-line") ]
      [ HH.span [ HP.class_ (HH.ClassName "twister-cell-verb") ] [ HH.text label ]
      , HH.text what
      ]

-- | What each phase colours a loop encoder.
-- |
-- | Enumerated from `LoopPhase` rather than listed, so a seventh state would
-- | show up here rather than being quietly absent from the only place anyone
-- | would look for it.
phases :: forall w i. HH.HTML w i
phases =
  HH.div [ HP.class_ (HH.ClassName "twister-phases") ]
    [ HH.h4_ [ HH.text "A loop's colour, on page 1" ]
    , HH.div [ HP.class_ (HH.ClassName "twister-phase-row") ]
        (map one TW.phaseKey <> [ heldKey, empty ])
    ]
  where
  one k =
    HH.span [ HP.class_ (HH.ClassName "twister-phase") ]
      [ HH.span [ HP.class_ (HH.ClassName ("twister-swatch tone-" <> k.tone)) ] []
      , HH.text k.phase
      ]
  -- Not a phase, so it cannot come from the enumeration — an empty slot is
  -- dark, and that is the state you will see most often.
  empty =
    HH.span [ HP.class_ (HH.ClassName "twister-phase") ]
      [ HH.span [ HP.class_ (HH.ClassName "twister-swatch tone-off") ] []
      , HH.text "empty"
      ]
  -- Also not a phase. A loop at speed zero is *playing* as far as `LoopPhase`
  -- is concerned — held is orthogonal to it, the same way muted is — but it is
  -- a colour you will see, and a key that omits a colour is worse than none.
  heldKey =
    HH.span [ HP.class_ (HH.ClassName "twister-phase") ]
      [ HH.span [ HP.class_ (HH.ClassName "twister-swatch tone-teal") ] []
      , HH.text "held at zero"
      ]
