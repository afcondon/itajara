-- | The little the process needs from Node: its settings, a clock for the
-- | log, and a way to hear that Bosun is stopping it.
module Itajara.Controllers.Env
  ( env
  , envOr
  , stamp
  , onStop
  ) where

import Prelude

import Data.Maybe (Maybe, fromMaybe)
import Data.Nullable (Nullable, toMaybe)
import Effect (Effect)

foreign import envImpl :: String -> Effect (Nullable String)
foreign import stampImpl :: Effect String
foreign import onSignalImpl :: Effect Unit -> Effect Unit

env :: String -> Effect (Maybe String)
env name = toMaybe <$> envImpl name

envOr :: String -> String -> Effect String
envOr name fallback = fromMaybe fallback <$> env name

-- | The time of day to the millisecond, for the log.
stamp :: Effect String
stamp = stampImpl

-- | Run this on SIGTERM or SIGINT, then exit (the bosun-daemon contract:
-- | drain on signal).
onStop :: Effect Unit -> Effect Unit
onStop = onSignalImpl
