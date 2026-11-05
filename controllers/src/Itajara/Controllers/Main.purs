-- | **The looper's own process**: it answers the looper's MC6 switches beside
-- | Helm's hub, whether or not any page is open, or in front.
-- |
-- | Why a process: a browser tab you are not looking at has its timers slowed
-- | to about once a second, so a looper answered by a background tab is late
-- | on every press (plan `docs/kb/plans/quadrat-the-sampler.md`, phase 3).
-- | This is the same answer PWYF's page gave, from the same code
-- | (`Data.Looper.Switchboard`, `Data.Looper.Machine`, `Data.Looper.Press`),
-- | run where nothing throttles it.
-- |
-- | **It says that it is answering.** It claims `itajara:switches` on the hub
-- | (`Data.Looper.Banks.switchesClaim`), and PWYF stands back while that is
-- | held, so a press is acted on once. The claim ends with this process's
-- | connection, so if it dies PWYF answers again by itself.
-- |
-- | **The loop in hand is the daemon's.** A `Focus` is told to the daemon (a
-- | bare loop number), and a change in the snapshot's `selected` is adopted,
-- | so this process, PWYF and Quadrat agree on it without talking to each
-- | other.
-- |
-- | **And the looper's pages on the Twister** (`TwisterPages`), held by the
-- | table in `Data.Looper.TwisterHolder`: the MC6's bank, read from the same
-- | frames PWYF reads (`Helm.MC6.Read`), says when they are the looper's.
-- |
-- | Not yet: `ShowBank` (the config page on close), which needs the MC6
-- | session PWYF holds; it is logged as not available here.
-- |
-- | Settings, from the environment: `HUB_URL` (ws://127.0.0.1:3039),
-- | `LOOPER_URL` (Itajara's registered address), `MC6_INPUT` (a part of the
-- | MC6's input name, "Morningstar").
module Itajara.Controllers.Main (main) where

import Prelude

import Data.Array as Array
import Data.Either (Either(..))
import Data.Foldable (for_, traverse_)
import Data.Int as Int
import Data.Looper.Banks as LoopBanks
import Data.Looper.Machine as Machine
import Data.Looper.Press as Press
import Data.Looper.Switchboard as Switchboard
import Data.Looper.TwisterHolder as Holder
import Helm.MC6.Read as Read
import Itajara.Controllers.TwisterPages as TwisterPages
import Data.Maybe (Maybe(..), fromMaybe)
import Effect (Effect)
import Effect.Aff (launchAff_, attempt)
import Effect.Class (liftEffect)
import Effect.Console as Console
import Effect.Exception (message)
import Effect.Ref (Ref)
import Effect.Ref as Ref
import Foreign.LooperSocket as LooperSocket
import Helm.WebMIDI as MIDI
import Itajara.Controllers.Env (envOr, onStop, stamp)

clientName :: String
clientName = Holder.looperProcess

type Process =
  { access :: MIDI.MIDIAccess
  , focus :: Ref Int
  -- The daemon's `selected` as last seen, so only a change in it moves
  -- `focus` (see PWYF's `looperSelectedSeen`).
  , selectedSeen :: Ref (Maybe Int)
  -- Unsubscribes the MC6 input we are listening to, if any.
  , detach :: Ref (Effect Unit)
  , mc6Match :: String
  , pages :: Ref (Maybe TwisterPages.Pages)
  }

say :: String -> Effect Unit
say msg = do
  t <- stamp
  Console.log (t <> " " <> msg)

main :: Effect Unit
main = launchAff_ do
  hubUrl <- liftEffect $ envOr "HUB_URL" "ws://127.0.0.1:3039"
  looperUrl <- liftEffect $ envOr "LOOPER_URL" LooperSocket.defaultUrl
  mc6Match <- liftEffect $ envOr "MC6_INPUT" "Morningstar"
  got <- attempt $ MIDI.requestHubAccessAff { url: hubUrl, client: clientName, waitMs: 15000.0 }
  liftEffect case got of
    Left err -> do
      say ("no hub: " <> message err <> "; exiting so Bosun restarts me")
      exit1
    Right access -> do
      LooperSocket.connect looperUrl
      p <- { access, focus: _, selectedSeen: _, detach: _, mc6Match, pages: _ }
        <$> Ref.new 0 <*> Ref.new Nothing <*> Ref.new (pure unit) <*> Ref.new Nothing
      MIDI.claimName access LoopBanks.switchesClaim
      say ("up: hub " <> hubUrl <> ", looper " <> looperUrl <> "; claimed " <> LoopBanks.switchesClaim)
      attach p
      pages <- TwisterPages.start
        { access, focus: p.focus, adopt: adopt p, perform: run p 0.0, say }
      Ref.write (Just pages) p.pages
      -- The hub going and coming back, or the MC6 being replugged, arrives as
      -- port changes: listen again to whatever now matches.
      void $ MIDI.onStateChange access \_ -> attach p
      onStop do
        MIDI.releaseName access LoopBanks.switchesClaim
        say "stopping: released the switches"
  where
  exit1 = exitImpl 1

foreign import exitImpl :: Int -> Effect Unit

-- | Listen to the MC6, replacing whatever we were listening to.
attach :: Process -> Effect Unit
attach p = do
  join (Ref.read p.detach)
  Ref.write (pure unit) p.detach
  found <- MIDI.findInput p.access (MIDI.NameContains p.mc6Match)
  case found of
    Nothing -> pure unit
    Just input -> do
      un <- MIDI.onMessage input (onMidi p)
      Ref.write un p.detach

onMidi :: Process -> Array Int -> Effect Unit
onMidi p = case _ of
  [ status, ccNum, val ] | status == 0xB0 + LoopBanks.switchChannel - 1 ->
    for_ (LoopBanks.decodeSwitch LoopBanks.switchChannel ccNum val) (answer p)
  -- Which bank the MC6 is on, from the frames it volunteers while PWYF holds
  -- its session: the same frames and the same reading as PWYF's, so the two
  -- agree on the looper's banks.
  bytes | Array.head bytes == Just 0xF0 ->
    for_ (Read.decodeReply bytes >>= Read.replyBank) \bank -> do
      pages <- Ref.read p.pages
      for_ pages \pg -> TwisterPages.heardBank pg bank
  _ -> pure unit

-- | Take the daemon's selection, if it changed since we last looked, so this
-- | process, PWYF and Quadrat agree on the loop in hand.
adopt :: Process -> Effect Unit
adopt p = do
  snap <- LooperSocket.latest
  for_ snap \s -> do
    seen <- Ref.read p.selectedSeen
    when (seen /= Just s.selected) do
      Ref.write s.selected p.focus
      Ref.write (Just s.selected) p.selectedSeen

-- | One press, answered.
answer :: Process -> LoopBanks.SwitchGesture -> Effect Unit
answer p press = do
  adopt p
  snap <- LooperSocket.latest
  focus <- Ref.read p.focus
  let late = Press.deferralOf Press.defaultDeferral press
  say ("switch: " <> LoopBanks.slotName press.slot <> " "
    <> fromMaybe (show press.switch) (LoopBanks.switchLetter press.switch)
    <> " " <> LoopBanks.gestureName press.gesture)
  traverse_ (run p late) (Switchboard.act (Press.rigFrom snap focus) press)

run :: Process -> Number -> Machine.Action -> Effect Unit
run p late a = case a of
  Machine.Command c -> do
    ok <- LooperSocket.send (c <> "@" <> show (Int.round late))
    say ((if ok then "  " else "  no daemon, went nowhere: ") <> Machine.describe a
      <> (if late >= 1.0 then " (" <> show (Int.round late) <> " ms late)" else ""))
  Machine.Focus i -> do
    Ref.write i p.focus
    void $ LooperSocket.send (show i)
    say ("  " <> Machine.describe a)
  Machine.ShowBank _ -> say ("  not available here: " <> Machine.describe a <> " (needs the MC6 session)")
  Machine.Unavailable why -> say ("  " <> why)
  Machine.Handled what -> say ("  " <> what)
