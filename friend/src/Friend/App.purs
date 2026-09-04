-- | The page. One component, because the Friend is one page: the loops as
-- | cards, each layer drawn, and every control a button that goes through the
-- | same machine a footswitch would — `Data.Looper.Machine.perform` against the
-- | daemon's own snapshot, so a button here and a switch on a pedalboard
-- | cannot come to mean different things by the same name.
-- |
-- | Two things are copied from producing-with-your-feet deliberately rather
-- | than shared, because each is a fact about how a Halogen app has to live
-- | beside this daemon and is worth reading in place: the poll is a
-- | subscription that only emits (a forked loop that *calls* the handler dies
-- | with the first throw and freezes the picture for ever), and the daemon's
-- | acks are read by sequence, not text (two identical refusals are two).
module Friend.App (component) where

import Prelude

import Control.Monad.Rec.Class (forever)
import Data.Array as Array
import Data.Foldable (for_, traverse_)
import Data.Int as Int
import Data.Looper.Duty (Duty, Subject(..))
import Data.Looper.Duty as Duty
import Data.Looper.Machine as Machine
import Data.Looper.Verb as Verb
import Data.Map (Map)
import Data.Map as Map
import Data.Maybe (Maybe(..), maybe)
import Data.Number as Number
import Data.String as String
import Effect.Aff (Milliseconds(..), delay)
import Effect.Aff as Aff
import Effect.Aff.Class (class MonadAff)
import Effect.Class (liftEffect)
import Effect.Console as Console
import Foreign.LooperSocket (LoopState, LooperState, Peaks, SocketStatus)
import Foreign.LooperSocket as Socket
import Friend.Face (Face)
import Friend.Face as Face
import Halogen as H
import Halogen.HTML as HH
import Halogen.HTML.Events as HE
import Halogen.HTML.Properties as HP
import Halogen.Subscription as HS
import Itajara.Surface.Edit as Edit
import Itajara.Surface.Wave (viewOf, wave)

type State =
  { face :: Face
  , looper :: Maybe LooperState
  , status :: Maybe SocketStatus
  , age :: Number
  -- | The loop in hand: what the Edit panel edits, what a bare duty acts on.
  , focus :: Int
  , peaks :: Maybe Peaks
  , peaksKey :: String
  -- | The slider a hand is on; see `Itajara.Surface.Edit.View`.
  , local :: Map String Int
  , editing :: Boolean
  , ackSeq :: Int
  , log :: Array String
  -- | The name the next save goes under.
  , take :: String
  }

data Action
  = Initialize
  | Poll
  | Do Subject Duty
  | Focus Int
  | ToggleEdit Int
  | SetLayer Int Int Boolean
  | WindowIn Int Int
  | WindowOut Int Int
  | ClearWindow Int
  | ShiftStart Int Int
  | AskPeaks Int
  | EditDone String
  | SetTake String
  | SaveAll

component :: forall q o m. MonadAff m => H.Component q Face o m
component =
  H.mkComponent
    { initialState: \face ->
        { face, looper: Nothing, status: Nothing, age: 0.0, focus: 0, peaks: Nothing
        , peaksKey: "", local: Map.empty, editing: false, ackSeq: 0, log: [], take: "take" }
    , render
    , eval: H.mkEval H.defaultEval { handleAction = handleAction, initialize = Just Initialize }
    }

-- | Everything the machine is allowed to know, from the newest snapshot.
-- | No grab loops and no grab source: those are facts about a pedalboard's
-- | reach, and this page reaches everything.
rigOf :: State -> Machine.Rig
rigOf st =
  { loops: maybe [] _.loops st.looper
  , focus: st.focus
  , click: maybe false _.click st.looper
  , monitor: maybe false _.monitor st.looper
  , armDb: maybe (-36.0) _.armDb st.looper
  , launchQ: maybe (-1) _.launchQ st.looper
  , sources: maybe [] (map _.name <<< _.sources) st.looper
  , grab: []
  , grabSource: ""
  }

handleAction :: forall o m. MonadAff m => Action -> H.HalogenM State Action () o m Unit
handleAction = case _ of
  Initialize -> do
    liftEffect $ Socket.connect Socket.defaultUrl
    void $ H.subscribe $ HS.makeEmitter \emit -> do
      fiber <- Aff.launchAff $ forever do
        delay (Milliseconds 100.0)
        liftEffect (emit Poll)
      pure (Aff.launchAff_ (Aff.killFiber (Aff.error "poll stopped") fiber))

  Poll -> do
    status <- liftEffect Socket.status
    age <- liftEffect Socket.snapshotAge
    snap <- liftEffect Socket.latest
    pk <- liftEffect Socket.latestPeaks
    cur <- H.get
    when (cur.peaks /= pk) $ H.modify_ _ { peaks = pk }
    -- Rounded, or a number that changes every tick redraws the page ten
    -- times a second for ever.
    let age' = Number.floor (age / 500.0) * 500.0
    when (cur.looper /= snap || cur.status /= Just status || cur.age /= age') do
      H.modify_ _ { looper = snap, status = Just status, age = age' }
      -- The Edit panel asks for its picture only when the picture would
      -- differ: the loop in focus, its layer count, its newest layer's birth.
      when cur.editing $
        for_ snap \s -> for_ (Array.index s.loops cur.focus) \lp -> do
          let key = if lp.layers == 0 then ""
                    else show cur.focus <> ":" <> show lp.layers <> ":"
                      <> show (maybe 0 _.born (Array.last lp.shapes))
          when (key /= "" && key /= cur.peaksKey) do
            H.modify_ _ { peaksKey = key }
            duty cur.focus (Duty.AskPeaks 600)
      -- What the daemon had to say. By sequence, so two identical refusals
      -- in a row are two lines.
      for_ snap \s ->
        when (s.ackSeq /= cur.ackSeq && s.ack /= "") do
          H.modify_ (note s.ack <<< _ { ackSeq = s.ackSeq })

  Do subject d -> do
    st <- H.get
    traverse_ runAction (Machine.perform (rigOf st) subject d)
  Focus i -> H.modify_ _ { focus = i }
  ToggleEdit i -> do
    st <- H.get
    let opening = not (st.editing && st.focus == i)
    H.modify_ _ { focus = i, editing = opening, peaksKey = "" }
  SetLayer loop layer on -> duty loop (Duty.LayerOn layer on)
  WindowIn loop f -> do
    H.modify_ \s -> s { local = Map.insert "in" f s.local }
    duty loop (Duty.WindowIn f)
  WindowOut loop f -> do
    H.modify_ \s -> s { local = Map.insert "out" f s.local }
    duty loop (Duty.WindowOut f)
  ClearWindow loop -> duty loop Duty.ClearWindow
  ShiftStart loop k -> do
    st <- H.get
    let rotNow = maybe 0 _.rot (st.looper >>= \s -> Array.index s.loops loop)
    H.modify_ \s -> s { local = Map.insert "rot" (rotNow + k) s.local }
    duty loop (Duty.ShiftStart k)
  AskPeaks loop -> duty loop (Duty.AskPeaks 600)
  EditDone key -> H.modify_ \s -> s { local = Map.delete key s.local }
  SetTake t -> H.modify_ _ { take = t }
  -- **One verb, one ack.** `exl<name>` writes every loop that holds
  -- something as a take of its own — `<name>/loop-<n>/`, the layers raw —
  -- and one manifest for the set, which is exactly the material a scene is
  -- made of. The shaping into the module's own folder is the harvest step,
  -- which the face says whether it has yet. The one thing here that does
  -- not go through `perform`: no switch can carry a name, so the vocabulary
  -- has no slot for one.
  SaveAll -> do
    st <- H.get
    let loops = maybe [] _.loops st.looper
    if Array.all (\lp -> lp.layers == 0) loops then H.modify_ (note "nothing to save: no loop has a layer")
    else runAction (Machine.Command (Verb.render (Verb.ExportLayers (safeName st.take))))
  where
  duty loop d = do
    st <- H.get
    traverse_ runAction (Machine.perform (rigOf st) (OnLoop loop) d)

runAction :: forall o m. MonadAff m => Machine.Action -> H.HalogenM State Action () o m Unit
runAction a = do
  liftEffect $ Console.log ("looper: " <> Machine.describe a)
  case a of
    Machine.Command c -> do
      ok <- liftEffect $ Socket.send (c <> "@0")
      H.modify_ (note (if ok then Machine.describe a else "no daemon — " <> c <> " went nowhere"))
    Machine.Focus i -> H.modify_ _ { focus = i }
    -- No pedalboard to show a bank on. Not an error: the machine asks as a
    -- courtesy, and here there is nobody to ask.
    Machine.ShowBank _ -> pure unit
    Machine.Unavailable why -> H.modify_ (note why)
    Machine.Handled what -> H.modify_ (note what)

note :: String -> State -> State
note msg s = s { log = Array.takeEnd 12 (Array.snoc s.log msg) }

-- | A take name the filesystem and the module both accept: letters, digits,
-- | dash and underscore; anything else becomes an underscore.
safeName :: String -> String
safeName s =
  let
    ok c = c == "-" || c == "_" || (c >= "a" && c <= "z") || (c >= "A" && c <= "Z") || (c >= "0" && c <= "9")
    cleaned = String.joinWith "" (map (\c -> if ok c then c else "_") (String.split (String.Pattern "") s))
  in
    if cleaned == "" then "take" else cleaned

render :: forall m. State -> H.ComponentHTML Action () m
render st =
  HH.div [ HP.class_ (HH.ClassName "friend") ]
    ( [ header ]
        <> (case st.looper of
              Nothing -> [ noDaemon ]
              Just top -> [ shape top, loops top, controls top ])
        <> [ logView ]
        <> (if st.editing then [ editModal ] else [])
    )
  where
  f = st.face

  header =
    HH.header [ HP.class_ (HH.ClassName "friend-head") ]
      [ HH.h1_ [ HH.text f.name ]
      , HH.p [ HP.class_ (HH.ClassName "friend-sub") ]
          [ HH.text ("A looper that writes " <> f.unit <> "s for the " <> f.maker <> " " <> f.module_ <> ". ") ]
      , HH.p [ HP.class_ (HH.ClassName ("friend-conn " <> connClass)) ] [ HH.text connWord ]
      ]

  connClass = case st.status of
    Just s | s.connected -> "is-on"
    _ -> "is-off"
  connWord = case st.status of
    Nothing -> "Looking for the daemon…"
    Just s
      | s.connected && st.age > 2000.0 -> "Connected, but the daemon has said nothing for " <> secs (st.age / 1000.0) <> " s."
      | s.connected -> "Daemon at " <> s.url
      | s.everConnected -> "Lost the daemon at " <> s.url <> " — it was there and is not now."
      | otherwise -> "No daemon at " <> s.url <> ". Start one:"

  noDaemon =
    HH.section [ HP.class_ (HH.ClassName "friend-start") ]
      [ HH.pre_ [ HH.code_ [ HH.text f.daemon ] ]
      , HH.p_ [ HH.text ("<device> is your audio interface's name; itajara devices lists them. This page connects by itself once the daemon is up.") ]
      , HH.ul_ (map (\n -> HH.li_ [ HH.text n ]) f.notes)
      ]

  shape top =
    HH.section [ HP.class_ (HH.ClassName "friend-shape") ]
      ( [ HH.span_
            [ HH.text (show top.nLoops <> " loops × " <> show top.maxLayers <> " layers × "
                <> secs top.maxSecs <> " s at " <> show top.sampleRate <> " Hz") ]
        , HH.span_ [ HH.text ("A " <> f.unit <> " holds " <> show f.layers <> " " <> f.layerWord <> "s of " <> secs f.layerSecs <> " s; " <> f.holds <> ".") ]
        ]
          <> maybe [] (\n -> [ HH.span [ HP.class_ (HH.ClassName "friend-warn") ] [ HH.text n ] ]) (Face.shapeNote f top)
      )

  loops top =
    HH.section [ HP.class_ (HH.ClassName "friend-loops") ]
      (Array.mapWithIndex (card top) top.loops)

  card top i lp =
    HH.article
      [ HP.class_ (HH.ClassName ("friend-loop " <> phaseClass lp <> (if i == st.focus then " is-focus" else "")))
      , HE.onClick \_ -> Focus i
      ]
      [ HH.div [ HP.class_ (HH.ClassName "friend-loop-head") ]
          [ HH.span [ HP.class_ (HH.ClassName "friend-loop-name") ] [ HH.text ("Loop " <> show (i + 1)) ]
          , HH.span [ HP.class_ (HH.ClassName "friend-loop-state") ] [ HH.text (stateWord lp) ]
          , HH.span [ HP.class_ (HH.ClassName "friend-loop-len") ] [ HH.text (lengthWord top lp) ]
          , HH.span [ HP.class_ (HH.ClassName "friend-loop-dest") ] [ HH.text ("→ " <> f.unit <> " " <> show (i + 1)) ]
          ]
      , HH.div [ HP.class_ (HH.ClassName "friend-layers") ]
          (if Array.null lp.shapes
            then [ HH.div [ HP.class_ (HH.ClassName "friend-layer is-empty") ] [ HH.text "empty" ] ]
            else Array.mapWithIndex (layerRow i lp) lp.shapes)
      , HH.div [ HP.class_ (HH.ClassName "friend-loop-buttons") ]
          [ btn (recordWord lp) (Do (OnLoop i) Duty.RecordLoop) (Socket.isWriting lp)
          , btn "Overdub" (Do (OnLoop i) Duty.OverdubLoop) false
          , btn (if lp.state == "playing" then "Stop" else "Play") (Do (OnLoop i) Duty.Transport) false
          , btn "Undo" (Do (OnLoop i) Duty.Undo) false
          , btn "Clear" (Do (OnLoop i) Duty.ClearLoop) false
          , btn "Edit" (ToggleEdit i) (st.editing && st.focus == i)
          ]
      ]

  layerRow i lp k sh =
    HH.div [ HP.class_ (HH.ClassName ("friend-layer" <> (if sh.on then "" else " is-off"))) ]
      [ HH.input
          [ HP.type_ HP.InputCheckbox
          , HP.class_ (HH.ClassName "loop-layer-on")
          , HP.checked sh.on
          , HP.title (f.layerWord <> " " <> show (k + 1) <> (if sh.on then ", in the mix" else ", out of the mix"))
          , HE.onChecked (SetLayer i (k + 1))
          ]
      , HH.span [ HP.class_ (HH.ClassName "friend-layer-n") ] [ HH.text (show (k + 1)) ]
      , HH.div [ HP.class_ (HH.ClassName "friend-layer-wave") ] (wave (viewOf lp sh))
      ]

  btn label act on =
    HH.button
      [ HP.class_ (HH.ClassName ("friend-btn" <> (if on then " on" else "")))
      , HE.onClick \_ -> act
      ]
      [ HH.text label ]

  controls top =
    HH.section [ HP.class_ (HH.ClassName "friend-controls") ]
      [ btn (if top.click then "Click on" else "Click off") (Do Focused Duty.ClickToggle) top.click
      , btn "Stop all" (Do Focused Duty.StopAll) false
      , btn "Clear all" (Do Focused Duty.ClearAll) false
      , HH.span [ HP.class_ (HH.ClassName "friend-gap") ] []
      , HH.label_ [ HH.text "Take " ]
      , HH.input
          [ HP.type_ HP.InputText
          , HP.class_ (HH.ClassName "friend-take")
          , HP.value st.take
          , HE.onValueInput SetTake
          ]
      , btn ("Save for " <> f.module_) SaveAll false
      , HH.span [ HP.class_ (HH.ClassName "friend-note") ]
          [ HH.text
              (if f.harvest
                then "Writes " <> f.unit <> "s in the module's own layout."
                else "Writes every loop's layers to ~/.itajara/takes/<take>/loop-<n>/, raw, with one manifest. The " <> f.module_ <> " layout is the next step.")
          ]
      ]

  logView =
    HH.section [ HP.class_ (HH.ClassName "friend-log") ]
      (map (\l -> HH.div_ [ HH.text l ]) (Array.reverse st.log))

  editModal =
    HH.div [ HP.class_ (HH.ClassName "looper-modal-overlay") ]
      [ HH.div [ HP.class_ (HH.ClassName "looper-modal-backdrop"), HE.onClick \_ -> ToggleEdit st.focus ] []
      , HH.div [ HP.class_ (HH.ClassName "looper-modal is-edit"), HP.attr (HH.AttrName "role") "dialog" ]
          [ HH.button [ HP.class_ (HH.ClassName "looper-modal-close"), HE.onClick \_ -> ToggleEdit st.focus ] [ HH.text "×" ]
          , HH.div [ HP.class_ (HH.ClassName "looper-modal-body") ]
              [ HH.h2_ [ HH.text ("Edit — loop " <> show (st.focus + 1)) ]
              , Edit.editPanel editHandlers { focus: st.focus, peaks: st.peaks, local: st.local } st.looper
              ]
          ]
      ]

  editHandlers =
    { windowIn: WindowIn
    , windowOut: WindowOut
    , clearWindow: ClearWindow
    , shiftStart: ShiftStart
    , askPeaks: AskPeaks
    , editDone: EditDone
    }

-- | The record button says what the next press does, because `r` is one
-- | verb that opens, closes, overdubs or cancels depending on the loop.
recordWord :: LoopState -> String
recordWord lp = case Socket.phaseOf lp of
  Socket.Armed -> "Cancel arm"
  Socket.RecordingFirst -> "Close"
  Socket.Overdubbing -> "End overdub"
  Socket.Multiplying -> "End multiply"
  Socket.Playing -> "Overdub"
  Socket.Idle -> if lp.layers > 0 then "Overdub" else "Record"

stateWord :: LoopState -> String
stateWord lp = case Socket.phaseOf lp of
  Socket.Armed -> "armed"
  Socket.RecordingFirst -> "recording"
  Socket.Overdubbing -> "overdubbing"
  Socket.Multiplying -> "multiplying"
  Socket.Playing -> if lp.muted then "muted" else "playing"
  Socket.Idle -> if lp.layers > 0 then "stopped" else "empty"

phaseClass :: LoopState -> String
phaseClass lp = case Socket.phaseOf lp of
  Socket.Armed -> "is-armed"
  Socket.RecordingFirst -> "is-recording"
  Socket.Overdubbing -> "is-recording"
  Socket.Multiplying -> "is-recording"
  Socket.Playing -> "is-playing"
  Socket.Idle -> if lp.layers > 0 then "is-stopped" else "is-empty"

lengthWord :: LooperState -> LoopState -> String
lengthWord top lp
  | lp.loopFrames <= 0 = ""
  | top.barFrames > 0 && lp.quant =
      let bars = Int.toNumber lp.loopFrames / Int.toNumber top.barFrames
      in secs lp.loopSecs <> " s · " <> secs bars <> " bars"
  | otherwise = secs lp.loopSecs <> " s"

secs :: Number -> String
secs n = show (Int.toNumber (Int.round (n * 10.0)) / 10.0)
