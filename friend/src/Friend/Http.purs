-- | The page's own server, as four calls. Everything here is a `Promise`
-- | behind an `Effect`, run with `toAffE`; the JS side owns the wire shape
-- | so that this side can hold what a form holds — strings in boxes.
module Friend.Http
  ( Notes
  , LoopNote
  , emptyNotes
  , HarvestRequest
  , HarvestResult
  , loadNotes
  , saveNotes
  , listSticks
  , listTakes
  , harvest
  ) where

import Prelude

import Control.Promise (Promise)
import Effect (Effect)

-- | What the player knows. `bpm` and `tags` are text here and numbers and a
-- | list on the wire; the JS bridge does the shaping.
type Notes =
  { title :: String
  , key :: String
  , bpm :: String
  , timbre :: String
  , uses :: String
  , notes :: String
  , tags :: String
  , loops :: Array LoopNote
  }

type LoopNote =
  { loop :: Int
  , title :: String
  , key :: String
  , timbre :: String
  , uses :: String
  , notes :: String
  }

emptyNotes :: Notes
emptyNotes = { title: "", key: "", bpm: "", timbre: "", uses: "", notes: "", tags: "", loops: [] }

type HarvestRequest =
  { take :: String
  , module :: String
  , stick :: String
  , bank :: String
  , scene :: String
  , overwrite :: Boolean
  , allLayers :: Boolean
  , dryRun :: Boolean
  }

type HarvestResult = { ok :: Boolean, output :: String }

foreign import loadNotesImpl :: String -> Effect (Promise Notes)
foreign import saveNotesImpl :: String -> Notes -> Effect (Promise Unit)
foreign import listSticksImpl :: Effect (Promise (Array String))
foreign import listTakesImpl :: Effect (Promise (Array String))
foreign import harvestImpl :: HarvestRequest -> Effect (Promise HarvestResult)

loadNotes :: String -> Effect (Promise Notes)
loadNotes = loadNotesImpl

saveNotes :: String -> Notes -> Effect (Promise Unit)
saveNotes = saveNotesImpl

listSticks :: Effect (Promise (Array String))
listSticks = listSticksImpl

listTakes :: Effect (Promise (Array String))
listTakes = listTakesImpl

harvest :: HarvestRequest -> Effect (Promise HarvestResult)
harvest = harvestImpl
