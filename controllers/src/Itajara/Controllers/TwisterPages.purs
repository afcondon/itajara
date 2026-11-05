-- | **The looper's pages on the Twister**, run by the looper's own process.
-- |
-- | Moved from PWYF (its looper branch of `handleEncoderTurn`,
-- | `handleEncoderPress`, `nudgeTurn`, `sendLooperLEDs`, `showTwisterPage`
-- | and the drift pin) on 2026-11-05, plan `quadrat-the-sampler.md` phase 3,
-- | step 3. What each encoder means is `Data.Looper.Twister`, unchanged; this
-- | is only the part that lives in time: painting, the pager, the nudges.
-- |
-- | **It paints only while it holds the Twister** on Helm's hub. Who should
-- | hold it is `Data.Looper.TwisterHolder`: on the looper's MC6 banks this
-- | process asks for it, and it refuses to give it up there; on any other
-- | bank it hands it to whoever asks (PWYF, which decides by focus and
-- | scenes). With nobody else holding it (PWYF closed) it takes it, so the
-- | looper's pages work with no page open.
-- |
-- | Everyone hears the Twister's knobs (inputs are not claimed); only the
-- | holder acts on them.
module Itajara.Controllers.TwisterPages
  ( Pages
  , State
  , start
  , heardBank
  ) where

import Prelude

import Data.Array as Array
import Data.Foldable (for_, traverse_)
import Data.Int as Int
import Data.Looper.Banks as LoopBanks
import Data.Looper.Machine as Machine
import Data.Looper.Press as Press
import Data.Looper.Twister as LoopTwister
import Data.Looper.TwisterHolder as Holder
import Data.Map (Map)
import Data.Map as Map
import Data.Maybe (Maybe(..), fromMaybe, isJust, maybe)
import Data.Ord (abs)
import Data.String as Str
import Data.Tuple (Tuple(..))
import Effect (Effect)
import Effect.Ref (Ref)
import Effect.Ref as Ref
import Effect.Timer (setInterval, setTimeout)
import Foreign.LooperSocket as LooperSocket
import Helm.Twister as HelmTwister
import Helm.Twister.Guard as Guard
import Helm.WebMIDI as MIDI

type State =
  { port :: Maybe String
  , out :: Maybe MIDI.MIDIOutput
  , holding :: Boolean
  -- The MC6's bank (wire number), as last reported.
  , bank :: Maybe Int
  , page :: Int
  -- What the rings and colours on the device show, so a frame where nothing
  -- moved sends nothing.
  , lit :: Map Int { ring :: Int, hue :: Int }
  , fine :: Boolean
  -- What each nudge encoder last reported, and a counter so only the last
  -- click's settle puts the ring back to centre.
  , nudge :: Map Int Int
  , nudgeSeq :: Int
  -- The undo knob waits for the engine to catch up before another step.
  , layersAsked :: Map Int Int
  }

type Pages =
  { access :: MIDI.MIDIAccess
  , st :: Ref State
  -- The loop in hand, shared with the switches (`Main`).
  , focus :: Ref Int
  , adopt :: Effect Unit
  , perform :: Machine.Action -> Effect Unit
  , say :: String -> Effect Unit
  -- Unsubscribes the Twister input we are listening to.
  , detach :: Ref (Effect Unit)
  }

-- | Start: find the Twister, listen to it and to the hub's claims, and paint
-- | ten times a second while holding it.
start
  :: { access :: MIDI.MIDIAccess
     , focus :: Ref Int
     , adopt :: Effect Unit
     , perform :: Machine.Action -> Effect Unit
     , say :: String -> Effect Unit
     }
  -> Effect Pages
start cfg = do
  st <- Ref.new
    { port: Nothing, out: Nothing, holding: false, bank: Nothing, page: 0
    , lit: Map.empty, fine: false, nudge: Map.empty, nudgeSeq: 0, layersAsked: Map.empty
    }
  detach <- Ref.new (pure unit)
  let p = { access: cfg.access, st, focus: cfg.focus, adopt: cfg.adopt, perform: cfg.perform, say: cfg.say, detach }
  attach p
  void $ MIDI.onStateChange cfg.access \_ -> attach p
  void $ MIDI.onClaims cfg.access (claimsChanged p)
  void $ MIDI.onWanted cfg.access (wanted p)
  void $ MIDI.onRefused cfg.access \port by why ->
    cfg.say ("twister: " <> by <> " keeps " <> port <> ": " <> why)
  void $ setInterval 100 (tick p)
  claimsChanged p
  pure p

-- | Find the Twister's output and input again (the hub or the device may have
-- | come back).
attach :: Pages -> Effect Unit
attach p = do
  join (Ref.read p.detach)
  Ref.write (pure unit) p.detach
  ports <- MIDI.outputs p.access
  let found = Array.find (\o -> matches o.name) ports
  out <- maybe (pure Nothing) (\o -> MIDI.findOutput p.access (MIDI.Exactly o.id)) found
  Ref.modify_ (_ { port = map _.id found, out = out }) p.st
  mIn <- MIDI.findInput p.access HelmTwister.port
  for_ mIn \input -> do
    hear <- Guard.guarded Guard.timing (onMessage p)
    un <- MIDI.onMessage input \bytes -> do
      drift p bytes
      for_ (HelmTwister.parse bytes) hear
    Ref.write un p.detach
  where
  matches name = case HelmTwister.port of
    MIDI.NameContains part -> Str.contains (Str.Pattern part) name
    MIDI.Exactly id -> id == name

-- | The MC6 is on `bank` (wire number). On the looper's banks this process
-- | should hold the Twister, so it asks.
heardBank :: Pages -> Int -> Effect Unit
heardBank p bank = do
  before <- Ref.read p.st
  Ref.modify_ (_ { bank = Just bank }) p.st
  when (before.bank /= Just bank) do
    p.say ("twister: the MC6 is on bank " <> show (bank + 1)
      <> (if Holder.looperHoldsBank bank then ", the looper's" else ""))
    when (Holder.looperHoldsBank bank) (ask p)

ask :: Pages -> Effect Unit
ask p = do
  s <- Ref.read p.st
  unless s.holding $ for_ s.port (MIDI.wantName p.access)

-- | Who holds what changed. Holding now, paint everything; not holding,
-- | forget what is lit. Nobody holding, take it: there is no one else to
-- | show anything.
claimsChanged :: Pages -> Effect Unit
claimsChanged p = do
  s <- Ref.read p.st
  for_ s.port \port -> do
    now <- MIDI.holding p.access port
    holder <- MIDI.holderOf p.access port
    Ref.modify_ (_ { holding = now }) p.st
    when (now && not s.holding) do
      p.say "twister: holding it, painting the looper's pages"
      paintFresh p
    when (not now && s.holding) do
      p.say ("twister: handed to " <> fromMaybe "nobody" holder)
      Ref.modify_ (_ { lit = Map.empty }) p.st
    when (not now && holder == Nothing) (MIDI.wantName p.access port)

-- | Someone wants the Twister. On the looper's banks the table says it is
-- | ours; anywhere else, focus decides, which is the asker's business.
wanted :: Pages -> String -> String -> Effect Unit
wanted p port by = do
  s <- Ref.read p.st
  when (s.port == Just port) $
    case s.bank of
      Just b | Holder.looperHoldsBank b ->
        MIDI.refuseWant p.access port by
          ("the MC6 is on bank " <> show (b + 1) <> ", one of the looper's")
      _ -> MIDI.grantName p.access port by

-- | Ten times a second: adopt the daemon's selection, and repaint what moved.
tick :: Pages -> Effect Unit
tick p = do
  s <- Ref.read p.st
  when s.holding do
    p.adopt
    paintDiff p

-- | A message from a layer other than the pinned one is drift (a press of
-- | the device's own side buttons): put it back and repaint. The device says
-- | nothing when it moves, so this acts on the first message from there.
drift :: Pages -> Array Int -> Effect Unit
drift p bytes = for_ (HelmTwister.layerOf bytes) \layer ->
  when (layer /= pinned) do
    s <- Ref.read p.st
    when s.holding do
      send p (HelmTwister.layerSelect pinned)
      paintFresh p

-- | The layer the device is kept on. Paging is this process's, not the
-- | device's (`Data.Twister.deviceBank` in PWYF says why).
pinned :: Int
pinned = 0

send :: Pages -> Array Int -> Effect Unit
send p bytes = do
  s <- Ref.read p.st
  for_ s.out \out -> MIDI.send out bytes

rig :: Pages -> Effect Machine.Rig
rig p = do
  snap <- LooperSocket.latest
  focus <- Ref.read p.focus
  pure (Press.rigFrom snap focus)

-- | Dim everything, then paint the page from nothing.
paintFresh :: Pages -> Effect Unit
paintFresh p = do
  for_ (Array.range 0 (HelmTwister.layers - 1)) \layer ->
    for_ (Array.range 0 (HelmTwister.encodersPerLayer - 1)) \index -> do
      send p (HelmTwister.colour { layer, index } 0)
      send p (HelmTwister.ring { layer, index } 0)
  Ref.modify_ (_ { lit = Map.empty }) p.st
  send p (HelmTwister.layerSelect pinned)
  s <- Ref.read p.st
  send p (HelmTwister.ring { layer: pinned, index: LoopTwister.pagerIndex } (LoopTwister.pagerRing s.page))
  paintDiff p

-- | What the page should show, from the newest snapshot, less what it
-- | already shows.
paintDiff :: Pages -> Effect Unit
paintDiff p = do
  s <- Ref.read p.st
  r <- rig p
  let wanted' = LoopTwister.ledsWith { fine: s.fine } r s.page
      changed = Array.filter (\l -> Map.lookup l.index s.lit /= Just { ring: l.ring, hue: l.hue }) wanted'
  for_ changed \l -> do
    let knob = { layer: pinned, index: l.index }
    send p (HelmTwister.colour knob l.hue)
    -- The pager's ring is the device's: writing it from the diff would move
    -- the page selector whenever a colour on the page changed.
    unless l.ringHeld $ send p (HelmTwister.ring knob l.ring)
  unless (Array.null changed) $
    Ref.modify_ (\x -> x { lit = Array.foldl (\m l -> Map.insert l.index { ring: l.ring, hue: l.hue } m) x.lit changed }) p.st

-- | Turn to a page. `moveKnob` when the page moved without the pager (a
-- | press for home), so the knob is carried to the band that means it.
showPage :: Pages -> Boolean -> Int -> Effect Unit
showPage p moveKnob page = do
  Ref.modify_ (_ { page = clamp 0 (LoopTwister.pages' - 1) page, lit = Map.empty }) p.st
  s <- Ref.read p.st
  when moveKnob $
    send p (HelmTwister.ring { layer: pinned, index: LoopTwister.pagerIndex } (LoopTwister.pagerRing s.page))
  paintDiff p

onMessage :: Pages -> HelmTwister.TwisterMsg -> Effect Unit
onMessage p msg = do
  s <- Ref.read p.st
  when s.holding case msg of
    HelmTwister.Turn knob val -> turn p s knob val
    HelmTwister.Press knob -> press p s knob
    -- The release means nothing of its own; the guard has used it. The side
    -- buttons walk between pedals, which is PWYF's.
    _ -> pure unit

-- | Which control an encoder is, on the page this process is showing: the
-- | device's own layer is discarded on purpose.
here :: State -> HelmTwister.Knob -> HelmTwister.Knob
here s knob = { layer: s.page, index: knob.index }

turn :: Pages -> State -> HelmTwister.Knob -> Int -> Effect Unit
turn p s knob val = do
  let control = LoopTwister.controlAt (here s knob)
  if control.pager then
    when (LoopTwister.pageFor val /= s.page) (showPage p false (LoopTwister.pageFor val))
  else case control.nudge of
    Just nd -> nudgeTurn p s knob nd val
    Nothing -> do
      r <- rig p
      for_ (LoopTwister.turnedAt (LoopTwister.shapeOf r) (here s knob) val) \(Tuple subject duty) -> case duty of
        LoopBanks.Layers want -> do
          snap <- LooperSocket.latest
          let i = case subject of
                LoopBanks.OnLoop n -> n
                _ -> r.focus
              have = maybe 0 _.layers (snap >>= \top -> Array.index top.loops i)
          case Map.lookup i s.layersAsked of
            Just asked | asked /= have -> pure unit
            _ -> do
              when (want /= have) $ Ref.modify_ (\x -> x { layersAsked = Map.insert i want x.layersAsked }) p.st
              traverse_ p.perform (Machine.perform r subject duty)
        _ -> traverse_ p.perform (Machine.perform r subject duty)

-- | One click of a nudge encoder: a delta against what it last reported,
-- | times the step, as the Edit panel's slider would send; then the ring
-- | goes back to centre once the knob is still, so it never runs out.
nudgeTurn :: Pages -> State -> HelmTwister.Knob -> LoopTwister.Nudge -> Int -> Effect Unit
nudgeTurn p s knob nd val = do
  let cc = HelmTwister.knobCC knob
      last = fromMaybe 64 (Map.lookup cc s.nudge)
      -- Against whichever base is nearer: the last report, or centre if a
      -- settle has landed since (PWYF's `nudgeTurn` says why).
      base = if abs (val - last) <= abs (val - 64) then last else 64
      delta = val - base
  snap <- LooperSocket.latest
  r <- rig p
  for_ snap \top -> for_ (Array.index top.loops r.focus) \lp -> do
    let beat = if top.barFrames > 0 && top.linkQuantum > 0.0
          then max 1 (Int.round (Int.toNumber top.barFrames / top.linkQuantum))
          else 1
        coarse = if lp.quant then beat else max 1 (lp.loopFrames / 100)
        step = if s.fine then max 1 (top.sampleRate / 100) else coarse
        frames = delta * step
        len = lp.loopFrames
        winI = if lp.winOut == 0 then 0 else lp.winIn
        winO = if lp.winOut == 0 then len else lp.winOut
        duty = case nd of
          LoopTwister.NIn -> LoopBanks.WindowIn (clamp (negate len) (winO - 1) (winI + frames))
          LoopTwister.NOut -> LoopBanks.WindowOut (clamp (winI + 1) (2 * len) (winO + frames))
          LoopTwister.NStart -> LoopBanks.ShiftStart frames
    when (delta /= 0 && len > 0) $
      traverse_ p.perform (Machine.perform r LoopBanks.Focused duty)
  let seq = s.nudgeSeq + 1
  Ref.modify_ (\x -> x { nudge = Map.insert cc val x.nudge, nudgeSeq = seq }) p.st
  void $ setTimeout 250 do
    now <- Ref.read p.st
    when (now.nudgeSeq == seq && now.holding) $
      send p (HelmTwister.ring { layer: pinned, index: knob.index } 64)

press :: Pages -> State -> HelmTwister.Knob -> Effect Unit
press p s knob = do
  let control = LoopTwister.controlAt (here s knob)
  if control.opens == Just LoopTwister.OpensEdit then
    p.say "twister: the Edit panel is a page's, not available here"
  else if isJust control.nudge then do
    -- A press on any nudge encoder flips the step for all three, and the
    -- lights say which: orange coarse, blue fine.
    Ref.modify_ (\x -> x { fine = not x.fine, lit = Map.empty }) p.st
    now <- Ref.read p.st
    p.say (if now.fine then "twister: edit encoders fine, 10 ms a click" else "twister: edit encoders coarse, a beat a click")
    paintDiff p
  else if control.pager then
    -- Home, the page you want in a hurry.
    showPage p true 0
  else do
    r <- rig p
    -- `performPress`: the one path that may take focus from a deliberate
    -- press (`Machine.performPress`). The lights follow the snapshot, not
    -- the press.
    for_ (LoopTwister.pressedAt (here s knob)) \(Tuple subject duty) ->
      traverse_ p.perform (Machine.performPress r subject duty)
