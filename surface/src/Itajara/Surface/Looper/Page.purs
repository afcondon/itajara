-- | The Looper page, whole.
-- |
-- | ## Why this is its own module
-- |
-- | It was three hundred and fifty lines inside `Component.App`, which is a
-- | four-thousand-line component that also owns two MIDI ports, the MC6's SysEx
-- | protocol, twelve pedals, board presets and a folder backup. Nothing about
-- | the looper page needed to be in there — it reads a snapshot and emits
-- | actions — but while it was, "what does the looper page depend on?" had no
-- | answer short of reading the whole file.
-- |
-- | Now it has one, and it is the type signature below. `State` is a row, so the
-- | call site passes `AppState` unchanged and the compiler still holds this
-- | module to exactly the fields it names. `Handlers` is the other half of the
-- | boundary: every action this page can cause, listed in one record.
-- |
-- | ## What deliberately did NOT come with it
-- |
-- | **The pedal face arrives as an argument.** It is a `HH.slot` of
-- | `Component.Pedal.View` — the same component the board grid uses — so
-- | rendering it here would drag the app's whole slot row into a module whose
-- | only other business is a snapshot. Itajara's donut is a *pedal* view that
-- | this page happens to show; it belongs to whoever owns the pedals.
-- |
-- | **The connection fact arrives as a boolean.** This page needs to know
-- | whether there is a Twister to page, and nothing else about the MIDI layer.
-- | Passing `MidiConnections` would have imported the whole rig to answer one
-- | question.
-- |
-- | **What only one app has arrives as `Extras`.** The live Twister card is
-- | drawn from that app's own idea of who the Twister is for; the MC6 bank
-- | tables and the print sheet exist only where something can program an MC6
-- | or print a sheet. The caller builds them, and a page with no MC6 passes
-- | `Nothing` and gets no button for them.
-- |
-- | ## It holds nothing
-- |
-- | Unchanged from when it lived in `App`, and the reason the extraction was
-- | mechanical: this page renders the snapshot the daemon pushes and sends
-- | command strings back. Every decision about what a command *means* lives in
-- | one place, at the other end of the socket.
-- |
-- | Deliberately unfinished. The interface this wants is in DESIGN-LOOPER §12 —
-- | concentric rings sharing a phase pointer, and a column per layer — and the
-- | thesis is that the display should say what the *next press* will do, not
-- | merely what happened.
module Itajara.Surface.Looper.Page
  ( State
  , Handlers
  , Ports
  , Extras
  , Parts
  , render
  , parts
  ) where

import Prelude

import Itajara.Surface.Looper.Board (render) as BoardSim
import Itajara.Surface.Looper.Slots as Slots
import Itajara.Surface.Looper.TwisterMap as TwisterMap
import Data.Looper.Recipes as Recipes
import Data.Looper.Twister as TW
import Itajara.Surface.Edit as Edit
import Data.Array as Array
import Data.Int as Int
import Data.Looper.Banks as LoopBanks
import Data.Map (Map)
import Data.Maybe (Maybe(..), maybe)
import Data.Tuple (Tuple(..))
import Itajara.Surface.Looper.Statement (inputWords, launchWords, secs)
import Itajara.Surface.Looper.Types (LogLine, LooperPanel(..))
import Foreign.LooperSocket (LooperState, SocketStatus)
import Foreign.LooperSocket as LooperSocket
import Halogen.HTML as HH
import Halogen.HTML.Events as HE
import Halogen.HTML.Properties as HP

-- | Everything the page reads.
-- |
-- | Open, so `AppState` satisfies it without a projection at the call site —
-- | but named field by field, so adding a read here is a visible change to the
-- | boundary rather than a quiet reach into a bigger record.
type State r =
  ( looper :: Maybe LooperState
  , looperStatus :: SocketStatus
  , looperSnapshotAge :: Number
  , looperFocus :: Int
  , looperPanel :: Maybe LooperPanel
  , looperPeaks :: Maybe LooperSocket.Peaks
  , looperEditLocal :: Map String Int
  , looperBankShown :: Maybe LoopBanks.BankSlot
  , looperLog :: Array LogLine
  , twisterPage :: Int
  , twisterHeardBank :: Maybe Int
  -- | What is typed in the Save field; empty lets the looper name it.
  , looperSaveName :: String
  | r
  )

-- | Whether there is anything on the other end of the wire.
-- |
-- | A boolean rather than the connection record, because that is the only
-- | question this page asks about MIDI: is there a Twister for the layout card
-- | to describe. (Whether there is an MC6 to write a bank to is asked by
-- | whoever supplies `Extras.banks`.)
type Ports = { twister :: Boolean }

-- | What only the calling app can draw, or has at all.
-- |
-- | * `twisterCard` — the body of the live Twister card on the right, under
-- |   its head: what the card shows depends on the app's idea of who the
-- |   Twister is for, so the app builds it (page tabs included, when the card
-- |   is the looper's).
-- | * `banks` — the MC6 banks panel's contents. `Nothing` and the panel's
-- |   button is not drawn.
-- | * `printSheet` — open the printable sheet. `Nothing` and the button is not
-- |   drawn.
type Extras w i =
  { twisterCard :: Array (HH.HTML w i)
  , banks :: Maybe (Array (HH.HTML w i))
  , printSheet :: Maybe i
  }

-- | Every action this page can cause.
-- |
-- | **The interesting one is `gesture`.** It takes a CC number, not a command —
-- | because every button here goes through the Itajara pedal's CC exactly as a
-- | footswitch does, and there is deliberately no shortcut to the socket from
-- | the UI. One route in means one place to debug. Keeping it as `Int -> i`
-- | means this module cannot take that shortcut even by accident: it has no
-- | word for the socket.
type Handlers i =
  { -- | A loop box was clicked: take that loop in hand and open its Edit
    -- | panel (the daemon's loop index).
    editLoop :: Int -> i
  , simulate :: LoopBanks.BankSlot -> Int -> LoopBanks.Gesture -> i
  , showTwisterPage :: Int -> i
  , openPanel :: Maybe LooperPanel -> i
  , gesture :: Int -> i
  , setClick :: Boolean -> i
  , setArm :: String -> i
  , setFeedback :: String -> i
  , setTone :: String -> i
  -- | One layer of one loop in or out of the mix: the daemon's loop index,
  -- | the layer number the slot shows (from one), and the wanted state.
  , setLayer :: Int -> Int -> Boolean -> i
  -- | The Edit panel's five: window in and out (loop, frames), the whole
  -- | loop again, a shift of the start (loop, signed frames), and a fresh
  -- | waveform (loop).
  , windowIn :: Int -> Int -> i
  , windowOut :: Int -> Int -> i
  , clearWindow :: Int -> i
  , shiftStart :: Int -> Int -> i
  , askPeaks :: Int -> i
  -- | Both ends at once (loop, in, out) — the shared panel's fixed-window
  -- | slider. This page never shows that mode, but the handler record is
  -- | the panel's contract, so it is supplied.
  , windowTo :: Int -> Int -> Int -> i
  , layerWindowTo :: Int -> Int -> Int -> Int -> i
  , clearLayerWindow :: Int -> Int -> i
  -- | A slider was released: the snapshot owns its value again.
  , editDone :: String -> i
  -- | Dragging the window along the picture; the fixed panel's hand, which
  -- | this page never shows, so it offers none.
  , waveDrag :: Maybe (Edit.WaveDrag i)
  -- | The Save field: typing in it, and saving the loop in hand under what
  -- | it says (empty: the looper names it `loop<N>-<MMDD>-<HHMMSS>`).
  -- |
  -- | **The one button that does not go through `gesture`**, because a CC
  -- | carries no name. The app turns it into the looper's save for the loop in
  -- | hand and runs it the way it runs everything else.
  , setSaveName :: String -> i
  , save :: String -> i
  }

render
  :: forall w i r
   . Handlers i
  -> Ports
  -> Extras w i
  -> Record (State r)
  -> HH.HTML w i
render h ports extras state =
  let
    p = parts h ports extras state
  in
    HH.div [ HP.class_ (HH.ClassName "looper-view") ]
      -- **No heading, and no "connected" line in the ordinary case.** The nav
      -- above already says LOOPER, and a band across the page saying the socket
      -- is open is a band that is right all day and tells you nothing on the one
      -- day it matters. Both are gone; what stands there instead is the rig's
      -- state, which is the thing you look up.
      ( [ HH.div [ HP.class_ (HH.ClassName "looper-status") ] [ p.readings ] ]
          <> p.notices
          <>
            -- **The board and the log, in that order, and the order is the
            -- argument.** Press a switch here and the sentence it produced
            -- appears directly underneath it — cause above effect, in one
            -- column, without crossing the page.
            [ HH.div [ HP.class_ (HH.ClassName "looper-body") ]
                [ HH.div [ HP.class_ (HH.ClassName "looper-main") ] p.main
                , HH.div [ HP.class_ (HH.ClassName "looper-side") ] p.side
                ]
            , p.overlay
            ]
      )

-- | **The page in pieces, for an app that lays them out itself.** Quadrat puts
-- | its own statement and meter at the head of a main column and the side
-- | cards in a column of their own, as it does on every page; `render` is the
-- | same pieces in the looper's own arrangement.
-- |
-- | * `readings` — the rig's state as a row of labelled readings.
-- | * `notices` — the connection and audio lines, empty when all is well.
-- | * `main` — Save, and the loops; a loop box opens its Edit panel.
-- | * `side` — the Twister card, the MC6 minimap, and the log. The Twister
-- |   and the MC6 each have a loupe onto their full reference.
-- | * `overlay` — the open panel, if any.
type Parts w i =
  { readings :: HH.HTML w i
  , notices :: Array (HH.HTML w i)
  , main :: Array (HH.HTML w i)
  , side :: Array (HH.HTML w i)
  , overlay :: HH.HTML w i
  }

parts
  :: forall w i r
   . Handlers i
  -> Ports
  -> Extras w i
  -> Record (State r)
  -> Parts w i
parts h ports extras state =
  { readings: HH.div [ HP.class_ (HH.ClassName "looper-status-row") ] readings
  , notices: [ connectionLine, audioLine ]
  , main:
      [ saveRow
      , case state.looper of
          Just lp -> Slots.render { onLayer: h.setLayer, onEdit: h.editLoop } lp state.looperFocus (LoopBanks.face state.looperBankShown)
          Nothing -> HH.text ""
      ]
  , side: [ twisterCard, boardCard, logCard ]
  , overlay: panel
  }
  where
  st = state.looperStatus

  panel = case state.looperPanel of
    Nothing -> HH.text ""
    Just PanelTwister ->
      -- Narrow, because the card is one page now. It was as wide as the panel
      -- would go so that both pages could sit side by side, and that width was
      -- spent covering the loops the card describes.
      modal "is-narrow" "Midifighter Twister — what each encoder does"
        [ TwisterMap.render ports.twister state.twisterPage state.twisterHeardBank h.showTwisterPage ]
    Just PanelBoard ->
      modal "is-board" "The MC6 board"
        [ BoardSim.render h.simulate (LoopBanks.face state.looperBankShown) ]
    Just PanelBanks -> case extras.banks of
      Just body -> modal "" "MC6 banks" body
      Nothing -> HH.text ""
    Just PanelEdit ->
      modal "is-edit" ("Edit — loop " <> show (state.looperFocus + 1)) [ editPanel ]
    -- **In the app rather than on paper, and that is the whole reason it
    -- exists.** Following a written sequence means looking away from the
    -- browser, and Chrome throttles a background tab — the looper stops
    -- handling Twister messages and every control reads as broken. A recipe you
    -- can follow without leaving the page is not a convenience.
    Just PanelRecipes ->
      modal "" "Recipes — what to press, and what should happen"
        [ HH.div [ HP.class_ (HH.ClassName "recipes") ]
            ( [ HH.p [ HP.class_ (HH.ClassName "recipes-preamble") ]
                  [ HH.text Recipes.preamble ] ]
                <> map recipeCard Recipes.recipes
            )
        ]

  -- | One recipe: why you would want it, the moves in order, and what each
  -- | move should say back.
  recipeCard r =
    HH.div [ HP.class_ (HH.ClassName "recipe") ]
      ( [ HH.h4_ [ HH.text r.name ]
        , HH.p [ HP.class_ (HH.ClassName "recipe-why") ] [ HH.text r.why ]
        , HH.ol [ HP.class_ (HH.ClassName "recipe-steps") ] (map recipeStep r.steps)
        ]
          <> case r.note of
               Nothing -> []
               Just n -> [ HH.p [ HP.class_ (HH.ClassName "recipe-note") ] [ HH.text n ] ]
      )

  recipeStep s =
    HH.li_
      ( [ if s.at == "" then HH.text ""
          else HH.span [ HP.class_ (HH.ClassName "recipe-at") ] [ HH.text s.at ]
        , HH.span [ HP.class_ (HH.ClassName "recipe-act") ] [ HH.text s.act ]
        ]
          <> case s.expect of
               Nothing -> []
               -- Quoted from an ack the daemon really sends, which is what makes
               -- this a test script as well as a manual.
               Just e -> [ HH.span [ HP.class_ (HH.ClassName "recipe-expect") ] [ HH.text e ] ]
      )

  -- | The Twister's current page, permanently.
  -- |
  -- | **Not behind a button, and that is a reversal made by looking at it.** It
  -- | was a corner dock you opened; seeing it open showed that it is small
  -- | enough to keep, and that a control you have to summon is one you will not
  -- | summon with a guitar in your hands. It is also the only place G to L are
  -- | written down at all — those are FS3X switches with no markings and no
  -- | LCD — which used to be the job of a legend under the loops. That legend
  -- | said the same six things less usefully, because you could not press it.
  twisterCard =
    HH.div [ HP.class_ (HH.ClassName "looper-side-card") ]
      ([ cardHead "The Twister, live" (Just (Tuple PanelTwister "what each encoder does")) ]
         <> extras.twisterCard)

  -- | **The MC6, small enough for the side column, and pressable.** The board
  -- | was a button that opened a modal; as a minimap it is also the screen's
  -- | transport, since its switches are the looper's verbs (record, overdub,
  -- | arm, undo on a double press) and pressing one here is pressing it there.
  -- | The loupe opens the full-size board.
  boardCard =
    HH.div [ HP.class_ (HH.ClassName "looper-side-card is-board") ]
      [ cardHead "The MC6" (Just (Tuple PanelBoard "the board, full size"))
      , BoardSim.render h.simulate (LoopBanks.face state.looperBankShown)
      ]

  -- | What the presses did, newest first.
  -- |
  -- | A log rather than the single line it replaced, because the sentence you
  -- | want is almost never the newest one — it is the two or three before the
  -- | press that surprised you, and a line that overwrites itself has thrown
  -- | those away by the time you look up.
  logCard =
    HH.div [ HP.class_ (HH.ClassName "looper-side-card") ]
      [ cardHead "What happened" Nothing
      , if Array.null state.looperLog
          then HH.p [ HP.class_ (HH.ClassName "looper-log-empty") ]
                 [ HH.text "Nothing pressed yet." ]
          else HH.ol [ HP.class_ (HH.ClassName "looper-log") ]
                 (map logLine state.looperLog)
      ]

  -- A repeat carries its count rather than a second identical line: pressing
  -- the same dead switch twice is two refusals and has to say so, but it does
  -- not have to say so twice as loudly as everything else.
  logLine l =
    HH.li [ HP.class_ (HH.ClassName "looper-log-line") ]
      ( [ HH.span_ [ HH.text l.text ] ]
          <> if l.times > 1
               then [ HH.span [ HP.class_ (HH.ClassName "looper-log-times") ]
                        [ HH.text ("\x00d7" <> show l.times) ] ]
               else []
      )

  -- The side cards' head. They are the page, not something laid over it, so
  -- there is nothing to close. A loupe opens the card's full reference, where
  -- it has one: the button row that used to open those is gone.
  cardHead title loupe =
    HH.div [ HP.class_ (HH.ClassName "looper-panel-head") ]
      ( [ HH.span [ HP.class_ (HH.ClassName "looper-panel-title") ] [ HH.text title ] ]
          <> case loupe of
            Nothing -> []
            Just (Tuple which what) ->
              [ HH.button
                  [ HP.class_ (HH.ClassName "looper-loupe")
                  , HP.title what
                  , HP.attr (HH.AttrName "aria-label") what
                  , HE.onClick \_ -> h.openPanel (Just which)
                  ]
                  [ HH.text "\x2315" ]
              ]
      )

  -- The panel itself is `Itajara.Surface.Edit`, shared with the Friend so
  -- the two draw one picture; this page supplies the three things it reads.
  editPanel = Edit.editPanel h
    { focus: state.looperFocus, peaks: state.looperPeaks, local: state.looperEditLocal, fixedFrames: Nothing, layer: Nothing }
    state.looper

  modal klass title body =
    HH.div [ HP.class_ (HH.ClassName "looper-modal-overlay") ]
      [ HH.div
          [ HP.class_ (HH.ClassName "looper-modal-backdrop")
          , HE.onClick \_ -> h.openPanel Nothing
          ]
          []
      , HH.div
          [ HP.class_ (HH.ClassName ("looper-modal " <> klass))
          , HP.attr (HH.AttrName "role") "dialog"
          , HP.attr (HH.AttrName "aria-label") title
          ]
          [ HH.button
              [ HP.class_ (HH.ClassName "looper-modal-close")
              , HP.attr (HH.AttrName "aria-label") "Close"
              , HE.onClick \_ -> h.openPanel Nothing
              ]
              [ HH.text "\x00D7" ]
          , HH.div [ HP.class_ (HH.ClassName "looper-modal-body") ] body
          ]
      ]

  -- | Save the loop in hand: it is what you do after a take. The folder is the name, cleaned
  -- | by the looper (letters, digits, `-`, `_`); a name that is already a take
  -- | is refused rather than written over, and the log says which happened.
  saveRow = case state.looper >>= \l -> Array.index l.loops state.looperFocus of
    Nothing -> HH.text ""
    Just fl ->
      let
        empty = fl.layers == 0 || fl.loopFrames == 0
        label = "loop " <> show (state.looperFocus + 1)
      in
        HH.div [ HP.class_ (HH.ClassName "looper-save") ]
          [ HH.label_ [ HH.text ("Save " <> label <> " as") ]
          , HH.input
              [ HP.type_ HP.InputText
              , HP.value state.looperSaveName
              , HP.placeholder ("loop" <> show (state.looperFocus + 1) <> "-date-time")
              , HP.disabled (not st.connected)
              , HE.onValueInput h.setSaveName
              ]
          , HH.button
              [ HP.class_ (HH.ClassName "looper-btn small")
              , HP.disabled (not st.connected || empty)
              , HP.title (if empty then label <> " has nothing to save yet" else "write its layers to a take")
              , HE.onClick \_ -> h.save state.looperSaveName
              ]
              [ HH.text "Save" ]
          ]

  -- A connected socket says nothing about whether audio is running: the push
  -- thread reads shared atomics and will serve a confident snapshot from an
  -- engine whose device was unplugged. That failure cost an afternoon of
  -- hunting a MIDI fault, so it gets its own line and says what to do.
  audioLine = case state.looper of
    Just lp | not lp.audioAlive ->
      HH.p [ HP.class_ (HH.ClassName "looper-conn down") ]
        [ HH.text $
            if lp.deviceLost
              then "The daemon lost the audio device — reconnecting. Commands will not take effect until it is back."
              else "The daemon is connected but its audio has stopped. Commands will not take effect."
        ]
    Just lp | lp.reopens > 0 ->
      HH.p [ HP.class_ (HH.ClassName "looper-muted") ]
        [ HH.text $ "Audio device recovered "
            <> show lp.reopens
            <> (if lp.reopens == 1 then " time" else " times")
            <> " this session."
        ]
    _ -> HH.text ""

  -- **A picture of the past must never be presented as the present.**
  --
  -- "Connected" was true and useless: the socket was open, commands were
  -- landing, and the loops on screen were minutes old. So the line reports the
  -- age of the newest snapshot as well as the state of the socket — the daemon
  -- pushes thirty times a second, so anything approaching a second is already
  -- wrong and the player deserves to be told rather than to work it out from a
  -- playhead that has stopped moving.
  -- | **The one place to look.** Bold, and at the top, because the question it
  -- | answers is not "what is this loop doing" — the slots answer that — but
  -- | "what is the rig set to", which was previously only answerable by opening
  -- | two panels and reading three encoders.
  -- |
  -- | Some of it repeats what a slot says. That is the point: a value you have
  -- | to go and find is a value you check once and then assume, and every one
  -- | of these is a thing that being wrong about costs a take.
  readings = case state.looper of
    Nothing -> [ reading "rig" "no daemon" true ]
    Just lp ->
      let focused = Array.index lp.loops state.looperFocus
          bars = maybe 0 _.cycles focused
      in
        -- Tempo and metre first, because everything else is counted in them —
        -- and marked as an alarm when absent, since "no clock" is the one
        -- reading here that changes what every other one means.
        [ reading "tempo"
            (if lp.linkAnchors == 0 then "no clock"
             else show (Int.round lp.linkTempo) <> " bpm")
            (lp.linkAnchors == 0)
        , reading "metre"
            (if lp.linkAnchors == 0 then "—" else show (Int.round lp.linkQuantum) <> "/4")
            false
        , reading "bar"
            (if lp.barFrames == 0 then "none yet"
             else secs (Int.toNumber lp.barFrames / Int.toNumber lp.sampleRate)
                    <> (if lp.linkAnchors == 0 then " · from loop 1" else ""))
            (lp.barFrames == 0)
        -- **One-based, like every other surface.** The wire counts loops from
        -- zero and this does not; the two met in the log once and cost an hour.
        , reading "selected" ("loop " <> show (state.looperFocus + 1)) false
        -- **Which jack the selected loop is listening to.** It belongs beside
        -- `selected` because it is a fact about that loop and not about the
        -- rig: the source is per loop, a new loop starts on the first one, and
        -- there is nothing anywhere else on screen that says which. Getting it
        -- wrong costs a whole take and the take is silent, which is the most
        -- expensive way to find out.
        --
        -- Stereo or mono comes with the name, because that is the half of it
        -- this page can be wrong about without saying so: a mono source is one
        -- jack read twice, and a loop recorded from one will never have width
        -- however the balance knob is turned. The channel numbers stay in the
        -- ack, which names them every time the knob moves.
        , reading "input" (inputWords lp focused) false
        , reading "length" (if bars == 0 then "not set"
                            else show bars <> (if bars == 1 then " bar" else " bars")) false
        , reading "launch" (launchWords lp.launchQ) false
        , reading "grid" (if maybe false _.quant focused then "on" else "off") false
        , twisterReading
        ]

  -- | **Which page the Twister is on, on the screen as well as under the hand.**
  -- |
  -- | The controller says it twice — the pager's ring position and its colour —
  -- | and both are on the device, which is no help at all when the thing you
  -- | are looking at is the screen. The eight loop encoders are deliberately in
  -- | the same eight positions on the Loops page and the Set page so the map is
  -- | learned once, and the cost of that is a page you can be on without
  -- | noticing: press Loop 8 on the Set page and it stops or starts rather than
  -- | selecting. That produced a bug report, and this is half the answer to it
  -- | (the other half is that acting on a loop now takes it in hand).
  -- |
  -- | The name comes from `TW.pages`, so a renamed page renames itself here.
  -- | Not an alarm: being on the Set page is an ordinary place to be. It earns
  -- | its space by being the one fact on this strip that is about the *hand*
  -- | rather than the rig.
  twisterReading =
    let name = maybe "—" _.name
                 (Array.find (\pg -> pg.bank == state.twisterPage) TW.pages)
    in reading "twister"
         ("page " <> show (state.twisterPage + 1) <> " · " <> name) false

  reading label value alarm =
    HH.div [ HP.class_ (HH.ClassName ("looper-reading" <> if alarm then " is-alarm" else "")) ]
      [ HH.span [ HP.class_ (HH.ClassName "looper-reading-label") ] [ HH.text label ]
      , HH.span [ HP.class_ (HH.ClassName "looper-reading-value") ] [ HH.text value ]
      ]

  -- **Silent when it is right**, which is the same rule the Twister card's
  -- status line follows. A green band saying the socket is open is correct all
  -- day and worth nothing; the strip above carries a `no daemon` reading for
  -- the case that matters, and this speaks only to say what to do about it.
  connectionLine =
    let stale = state.looperSnapshotAge > 1000.0
    in if st.connected && not stale then HH.text "" else HH.p
      [ HP.class_ (HH.ClassName "looper-conn down") ]
      [ HH.text $
          if st.connected && stale then
            "Connected, but the picture is "
              <> show (Int.round (state.looperSnapshotAge / 100.0) / 10) <> " s old."
          else if st.connected then "Connected to the daemon."
          else if st.everConnected then "Lost the daemon — retrying."
          else "No daemon. Start it with:  itajara loop --device AUDIO4c --ws"
      ]

