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
import Control.Promise (toAffE)
import Data.Array as Array
import Data.Either (Either(..))
import Data.Foldable (for_, traverse_)
import Data.Int as Int
import Data.Looper.Duty (Duty, Subject(..))
import Data.Looper.Duty as Duty
import Data.Looper.Machine as Machine
import Data.Looper.Verb as Verb
import Data.Map (Map)
import Data.Map as Map
import Data.Maybe (Maybe(..), fromMaybe, maybe)
import Data.Number as Number
import Data.String as String
import Effect.Aff (Milliseconds(..), attempt, delay)
import Effect.Exception (message)
import Effect.Aff as Aff
import Effect.Aff.Class (class MonadAff)
import Effect.Class (liftEffect)
import Effect.Console as Console
import Foreign.LooperSocket (LoopState, LooperState, Peaks, SocketStatus)
import Foreign.LooperSocket as Socket
import Friend.Face (Face)
import Friend.Face as Face
import Friend.Http (Notes, LoopNote)
import Friend.Http as Http
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
  , panel :: Panel
  , ackSeq :: Int
  , log :: Array String
  -- | The name the next save goes under.
  , take :: String
  -- | Takes the server can see on disk: a name in here has been saved.
  , saved :: Array String
  -- | The player's notes for the take in hand, and which take they were
  -- | loaded for, so switching takes does not carry one's notes to another.
  , notes :: Notes
  , notesFor :: String
  , notesStatus :: String
  -- | The harvest form.
  , sticks :: Array String
  , stick :: String
  , bank :: String
  , scene :: String
  , overwrite :: Boolean
  , allLayers :: Boolean
  , harvestOut :: String
  , harvestBusy :: Boolean
  }

-- | One modal at a time: the loop in hand's edit, the take's notes, or the
-- | harvest. The Edit panel is the shared one; the other two are this page's.
data Panel = NoPanel | EditPanel | NotesPanel | HarvestPanel

derive instance Eq Panel

-- | A field of the take's notes, so one action sets any of them.
data NoteField = NTitle | NKey | NBpm | NTimbre | NUses | NNotes | NTags

-- | A field of one loop's notes.
data LoopField = LTitle | LKey | LTimbre | LUses | LNotes

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
  | OpenPanel Panel
  | RefreshTakes
  | SetNote NoteField String
  | SetLoopNote Int LoopField String
  | SaveNotes
  | RefreshSticks
  | SetStick String
  | SetBank String
  | SetScene String
  | SetOverwrite Boolean
  | SetAllLayers Boolean
  | RunHarvest Boolean

component :: forall q o m. MonadAff m => H.Component q Face o m
component =
  H.mkComponent
    { initialState: \face ->
        { face, looper: Nothing, status: Nothing, age: 0.0, focus: 0, peaks: Nothing
        , peaksKey: "", local: Map.empty, panel: NoPanel, ackSeq: 0, log: [], take: "take"
        , saved: [], notes: Http.emptyNotes, notesFor: "", notesStatus: ""
        , sticks: [], stick: "", bank: "1", scene: "1_1", overwrite: false, allLayers: false
        , harvestOut: "", harvestBusy: false }
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
    handleAction RefreshTakes
    handleAction RefreshSticks

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
      when (cur.panel == EditPanel) $
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
    let opening = not (st.panel == EditPanel && st.focus == i)
    H.modify_ _ { focus = i, panel = if opening then EditPanel else NoPanel, peaksKey = "" }
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
    else do
      runAction (Machine.Command (Verb.render (Verb.ExportLayers (safeName st.take))))
      -- The daemon writes on its own thread and the ack lands in the
      -- snapshot; the folder is there a moment later. Ask the server then.
      void $ H.fork do
        H.liftAff (delay (Milliseconds 1500.0))
        handleAction RefreshTakes

  OpenPanel p -> do
    H.modify_ _ { panel = p }
    st <- H.get
    when (p == NotesPanel && st.notesFor /= safeName st.take) do
      r <- H.liftAff (attempt (toAffE (Http.loadNotes (safeName st.take))))
      case r of
        Right n -> H.modify_ _ { notes = n, notesFor = safeName st.take, notesStatus = "" }
        Left e -> H.modify_ _ { notes = Http.emptyNotes, notesFor = safeName st.take, notesStatus = "could not load notes: " <> message e }
    when (p == HarvestPanel) $ handleAction RefreshSticks
  RefreshTakes -> do
    r <- H.liftAff (attempt (toAffE Http.listTakes))
    case r of
      Right ts -> H.modify_ _ { saved = ts }
      -- No server (the page is being served statically): nothing to list,
      -- and nothing to say every second about it.
      Left _ -> pure unit
  SetNote f v -> H.modify_ \s -> s { notes = setNote f v s.notes }
  SetLoopNote i f v -> H.modify_ \s -> s { notes = s.notes { loops = setLoopNote i f v s.notes.loops } }
  SaveNotes -> do
    st <- H.get
    r <- H.liftAff (attempt (toAffE (Http.saveNotes (safeName st.take) st.notes)))
    H.modify_ _ { notesStatus = case r of
      Right _ -> "saved to ~/.itajara/takes/" <> safeName st.take <> "/notes.json"
      Left e -> "could not save: " <> message e }
  RefreshSticks -> do
    r <- H.liftAff (attempt (toAffE Http.listSticks))
    case r of
      Right ss -> H.modify_ \s -> s { sticks = ss, stick = if s.stick == "" then fromMaybe "" (Array.head ss) else s.stick }
      Left _ -> pure unit
  SetStick v -> H.modify_ _ { stick = v }
  SetBank v -> H.modify_ _ { bank = v }
  SetScene v -> H.modify_ _ { scene = v }
  SetOverwrite v -> H.modify_ _ { overwrite = v }
  SetAllLayers v -> H.modify_ _ { allLayers = v }
  RunHarvest dryRun -> do
    st <- H.get
    H.modify_ _ { harvestBusy = true, harvestOut = if dryRun then "dry run…" else "harvesting…" }
    r <- H.liftAff (attempt (toAffE (Http.harvest
      { take: safeName st.take, module: st.face.id, stick: st.stick, bank: st.bank, scene: st.scene
      , overwrite: st.overwrite, allLayers: st.allLayers, dryRun })))
    H.modify_ _ { harvestBusy = false, harvestOut = case r of
      Right res -> res.output <> (if res.ok then "" else "\n(msm reported a failure)")
      Left e -> "could not reach the server: " <> message e }
    handleAction RefreshTakes
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

setNote :: NoteField -> String -> Notes -> Notes
setNote f v n = case f of
  NTitle -> n { title = v }
  NKey -> n { key = v }
  NBpm -> n { bpm = v }
  NTimbre -> n { timbre = v }
  NUses -> n { uses = v }
  NNotes -> n { notes = v }
  NTags -> n { tags = v }

-- | The loop's row, made if it has none yet. Loops are numbered from one
-- | here, as the datasheet and the folders are.
setLoopNote :: Int -> LoopField -> String -> Array LoopNote -> Array LoopNote
setLoopNote i f v rows =
  case Array.findIndex (\r -> r.loop == i) rows of
    Just k -> fromMaybe rows (Array.modifyAt k (set) rows)
    Nothing -> Array.snoc rows (set { loop: i, title: "", key: "", timbre: "", uses: "", notes: "" })
  where
  set r = case f of
    LTitle -> r { title = v }
    LKey -> r { key = v }
    LTimbre -> r { timbre = v }
    LUses -> r { uses = v }
    LNotes -> r { notes = v }

loopNote :: Int -> Array LoopNote -> LoopNote
loopNote i rows = fromMaybe { loop: i, title: "", key: "", timbre: "", uses: "", notes: "" } (Array.find (\r -> r.loop == i) rows)

render :: forall m. State -> H.ComponentHTML Action () m
render st =
  HH.div [ HP.class_ (HH.ClassName "friend") ]
    ( [ header ]
        <> (case st.looper of
              Nothing -> [ noDaemon ]
              Just top -> [ shape top, loops top, controls top ])
        <> [ logView ]
        <> (case st.panel of
              NoPanel -> []
              EditPanel -> [ editModal ]
              NotesPanel -> [ notesModal ]
              HarvestPanel -> [ harvestModal ])
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
          , btn "Edit" (ToggleEdit i) (st.panel == EditPanel && st.focus == i)
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
      , btn "Save take" SaveAll false
      , HH.span [ HP.class_ (HH.ClassName "friend-saved") ]
          [ HH.text (if Array.elem (safeName st.take) st.saved then "saved ✓" else "") ]
      , btn "Notes" (OpenPanel NotesPanel) (st.panel == NotesPanel)
      , if f.harvest
          then btn ("Harvest to " <> f.module_) (OpenPanel HarvestPanel) (st.panel == HarvestPanel)
          else HH.text ""
      , HH.span [ HP.class_ (HH.ClassName "friend-note") ]
          [ HH.text
              ("Save writes every loop's layers to ~/.itajara/takes/<take>/loop-<n>/, raw, with one manifest. "
                <> (if f.harvest
                      then "Harvest shapes that onto the stick: a loop is a library bank and a scene, and a datasheet goes with it."
                      else "The " <> f.module_ <> " layout is the next step."))
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

  modal klass title body =
    HH.div [ HP.class_ (HH.ClassName "looper-modal-overlay") ]
      [ HH.div [ HP.class_ (HH.ClassName "looper-modal-backdrop"), HE.onClick \_ -> OpenPanel NoPanel ] []
      , HH.div [ HP.class_ (HH.ClassName ("looper-modal " <> klass)), HP.attr (HH.AttrName "role") "dialog" ]
          [ HH.button [ HP.class_ (HH.ClassName "looper-modal-close"), HE.onClick \_ -> OpenPanel NoPanel ] [ HH.text "×" ]
          , HH.div [ HP.class_ (HH.ClassName "looper-modal-body") ] ([ HH.h2_ [ HH.text title ] ] <> body)
          ]
      ]

  field label value onV =
    HH.label [ HP.class_ (HH.ClassName "friend-field") ]
      [ HH.span_ [ HH.text label ]
      , HH.input [ HP.type_ HP.InputText, HP.value value, HE.onValueInput onV ]
      ]

  -- **What only the player knows.** The daemon's facts — length, bars,
  -- tempo, source — go on the datasheet by themselves; these are the rest.
  notesModal =
    modal "is-notes" ("Notes — " <> safeName st.take)
      [ HH.div [ HP.class_ (HH.ClassName "friend-fields") ]
          [ field "Title" st.notes.title (SetNote NTitle)
          , field "Key" st.notes.key (SetNote NKey)
          , field "BPM" st.notes.bpm (SetNote NBpm)
          , field "Timbre" st.notes.timbre (SetNote NTimbre)
          , field "Intended use" st.notes.uses (SetNote NUses)
          , field "Tags" st.notes.tags (SetNote NTags)
          ]
      , HH.label [ HP.class_ (HH.ClassName "friend-field is-wide") ]
          [ HH.span_ [ HH.text "Notes" ]
          , HH.textarea [ HP.value st.notes.notes, HP.rows 3, HE.onValueInput (SetNote NNotes) ]
          ]
      , HH.table [ HP.class_ (HH.ClassName "friend-loop-notes") ]
          [ HH.thead_ [ HH.tr_ [ HH.th_ [ HH.text "loop" ], HH.th_ [ HH.text "title" ], HH.th_ [ HH.text "key" ], HH.th_ [ HH.text "timbre" ], HH.th_ [ HH.text "use / notes" ] ] ]
          , HH.tbody_ (map loopNoteRow (Array.range 1 (maybe 0 (Array.length <<< _.loops) st.looper)))
          ]
      , HH.div [ HP.class_ (HH.ClassName "looper-edit-actions") ]
          [ btn "Save notes" SaveNotes false
          , HH.span [ HP.class_ (HH.ClassName "looper-edit-note") ] [ HH.text st.notesStatus ]
          ]
      ]

  loopNoteRow i =
    let r = loopNote i st.notes.loops
        cell fld v = HH.td_ [ HH.input [ HP.type_ HP.InputText, HP.value v, HE.onValueInput (SetLoopNote i fld) ] ]
    in HH.tr_
      [ HH.td_ [ HH.text (show i <> (if hasMaterial i then "" else " (empty)")) ]
      , cell LTitle r.title
      , cell LKey r.key
      , cell LTimbre r.timbre
      , cell LNotes r.notes
      ]

  hasMaterial i = maybe false (\top -> maybe false (\lp -> lp.layers > 0) (Array.index top.loops (i - 1))) st.looper

  -- **The stick, and where on it.** A loop is a library bank and a scene;
  -- the form says which bank and scene the first loop takes and the rest
  -- follow. Dry run first is cheap and says exactly what would land where.
  harvestModal =
    modal "is-harvest" ("Harvest " <> safeName st.take <> " to the " <> f.module_)
      ( (if Array.elem (safeName st.take) st.saved then []
          else [ HH.p [ HP.class_ (HH.ClassName "friend-warn") ] [ HH.text "This take has not been saved yet — Save take first." ] ])
      <> [ HH.div [ HP.class_ (HH.ClassName "friend-fields") ]
            [ HH.label [ HP.class_ (HH.ClassName "friend-field") ]
                [ HH.span_ [ HH.text "Stick" ]
                , if Array.null st.sticks
                    then HH.span [ HP.class_ (HH.ClassName "friend-warn") ] [ HH.text "no mounted volume has an _arbhar_library folder" ]
                    else HH.select [ HE.onValueChange SetStick ]
                      (map (\p -> HH.option [ HP.value p, HP.selected (p == st.stick) ] [ HH.text p ]) st.sticks)
                ]
            , field "First library bank (1–6)" st.bank SetBank
            , field "First scene (1_1 … 6_6)" st.scene SetScene
            , HH.label [ HP.class_ (HH.ClassName "friend-field is-check") ]
                [ HH.input [ HP.type_ HP.InputCheckbox, HP.checked st.overwrite, HE.onChecked SetOverwrite ], HH.span_ [ HH.text "Overwrite slots already holding audio" ] ]
            , HH.label [ HP.class_ (HH.ClassName "friend-field is-check") ]
                [ HH.input [ HP.type_ HP.InputCheckbox, HP.checked st.allLayers, HE.onChecked SetAllLayers ], HH.span_ [ HH.text "Include layers switched off" ] ]
            ]
        , HH.div [ HP.class_ (HH.ClassName "looper-edit-actions") ]
            [ btn "Refresh sticks" RefreshSticks false
            , btn "Dry run" (RunHarvest true) false
            , btn ("Harvest to " <> f.module_) (RunHarvest false) st.harvestBusy
            , HH.span [ HP.class_ (HH.ClassName "looper-edit-note") ]
                [ HH.text "Each loop takes one bank and one scene, ten seconds plus the three that follow, 24-bit at 48 kHz. The datasheet lands in the take and in _harvest/ on the stick." ]
            ]
        , HH.pre [ HP.class_ (HH.ClassName "friend-harvest-out") ] [ HH.text st.harvestOut ]
        ]
      )

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
