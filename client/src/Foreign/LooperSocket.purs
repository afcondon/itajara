-- | Talking to the looper daemon.
-- |
-- | The daemon (`looper/` in this repo) owns the audio: buffers, the sample
-- | clock, latency compensation. This app owns the UX and the MIDI. So the
-- | traffic is one-way in each direction — commands out, state in — and the
-- | app never needs to model the engine, only display what it reports.
-- |
-- | The snapshot arrives thirty times a second and is *pulled* rather than
-- | pushed at Halogen, because a component that re-renders thirty times a
-- | second to move a position readout is a component that will be blamed for
-- | the app feeling slow. The FFI keeps the newest and drops the rest.
module Foreign.LooperSocket
  ( LooperState
  , SocketStatus
  , connect
  , send
  , latest
  , status
  , defaultUrl
  ) where

import Prelude

import Data.Maybe (Maybe)
import Data.Nullable (Nullable, toMaybe)
import Effect (Effect)

-- | What the daemon says about itself. Mirrors `snapshot` in `looper/src/ws.rs`
-- | field for field; if one changes the other must.
type LooperState =
  { state :: String
  , layers :: Int
  , maxLayers :: Int
  , loopFrames :: Int
  , loopSecs :: Number
  , pos :: Int
  , phase :: Number
  , sampleRate :: Int
  , inDb :: Number
  , outDb :: Number
  , click :: Boolean
  , monitor :: Boolean
  , armed :: Boolean
  , recording :: Boolean
  , calibrated :: Boolean
  , k :: Int
  }

type SocketStatus =
  { connected :: Boolean
  -- | Distinguishes "never found it" from "had it and lost it", which want
  -- | different words on screen: one is a daemon that was never started, the
  -- | other is one that died mid-session.
  , everConnected :: Boolean
  , lastError :: String
  , url :: String
  }

foreign import connectImpl :: String -> Effect Unit
foreign import sendImpl :: String -> Effect Boolean
foreign import latestImpl :: Effect (Nullable LooperState)
foreign import statusImpl :: Effect SocketStatus

-- | Idempotent: calling it again with the same URL leaves the connection
-- | alone, so it is safe to call on every initialise.
connect :: String -> Effect Unit
connect = connectImpl

-- | Returns false if nothing was listening. Worth surfacing rather than
-- | swallowing — a footswitch that silently does nothing is the failure this
-- | whole app exists to design against.
send :: String -> Effect Boolean
send = sendImpl

latest :: Effect (Maybe LooperState)
latest = toMaybe <$> latestImpl

status :: Effect SocketStatus
status = statusImpl

-- | The daemon binds loopback only, so this is not configurable by accident.
defaultUrl :: String
defaultUrl = "ws://127.0.0.1:3028"
