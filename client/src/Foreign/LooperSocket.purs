-- | Talking to the looper daemon.
-- |
-- | The daemon (`itajara/` in this repo) owns the audio: buffers, the sample
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
  , LayerShape
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

-- | What the daemon says about itself. Mirrors `snapshot` in `itajara/src/ws.rs`
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
  -- | Whether the audio callbacks are actually running. A connected socket says
  -- | nothing about this: the push thread only reads shared atomics, so it will
  -- | serve confident snapshots from an engine whose device was unplugged.
  , audioAlive :: Boolean
  , deviceLost :: Boolean
  , reopens :: Int
  -- | Each layer's own length and where it sounds. The daemon has sent these
  -- | since layers stopped being tiled into the cycle; this type went on
  -- | claiming to mirror the snapshot without them, which is how a field the
  -- | display most needs stayed invisible.
  -- |
  -- | `period` and `phase` are the whole reason a take is stored rather than
  -- | flattened: two layers of the same length look identical until you can see
  -- | that one of them sounds one cycle in four.
  , shapes :: Array LayerShape
  -- | What the last command had to say, and a counter that moves when it
  -- | changes. Carried in every snapshot rather than sent once, so a reload
  -- | still sees it — and so a client can tell a fresh ack from the same one
  -- | still on screen.
  , ack :: String
  , ackSeq :: Int
  -- | What the rig's clock says, from link-spike's `/link/anchor`. Zero
  -- | throughout when no anchor has arrived — `linkAnchors` is what
  -- | distinguishes "no clock" from "a clock reading zero", and
  -- | `linkRejected` counts anchors that arrived in a shape we would not
  -- | believe, so a changed message cannot be adopted in silence.
  -- |
  -- | `linkBarFrames` is the one number quantisation needs and the looper
  -- | cannot derive: it measures cycles, and nothing in it has an opinion
  -- | about metre.
  , linkTempo :: Number
  , linkQuantum :: Number
  , linkBarFrames :: Int
  , linkAnchors :: Int
  , linkRejected :: Int
  }

type LayerShape =
  { len :: Int
  , period :: Int
  , phase :: Int
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
