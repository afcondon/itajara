-- | A face is what the Friend knows about one destination.
-- |
-- | The differences between Arbhar's Friend, Morphagene's Friend and Rample's
-- | Friend are a table — how many layers the module holds, how long one is,
-- | what a loop and a layer *become* on the stick — not code. So they are a
-- | table, and the page reads it: the loop cards say "→ scene 3", the header
-- | says whether the daemon was started with enough layers, and the save
-- | button says where the take will go. Chosen by `?face=<id>`; unknown or
-- | absent is the first one.
-- |
-- | **The face is not the skin.** A face is a configuration; how the page
-- | *looks* is the stylesheet, and the two vary independently — the same
-- | Arbhar face can be drawn plainly or in any house style.
module Friend.Face
  ( Face
  , faces
  , faceFor
  , arbhar
  , shapeNote
  ) where

import Prelude

import Data.Array as Array
import Data.Maybe (Maybe(..), fromMaybe)
import Foreign.LooperSocket (LooperState)

type Face =
  { id :: String
  , name :: String
  , module_ :: String
  , maker :: String
  -- | Layers the module holds in one unit; the daemon wants at least this
  -- | many per loop (`--layers`).
  , layers :: Int
  -- | What one layer holds, in seconds, and what the module reads past that.
  , layerSecs :: Number
  , tailSecs :: Number
  -- | What a loop becomes on the stick, and what a layer becomes.
  , unit :: String
  , layerWord :: String
  -- | The stick's capacity, as one sentence.
  , holds :: String
  -- | How to start the daemon to suit this module. `<device>` is the user's.
  , daemon :: String
  -- | Whether the shaping step (msm `harvest`) exists for this module yet.
  -- | Until it does, Save writes the daemon's own take format and the
  -- | Harvest button is not offered.
  , harvest :: Boolean
  , notes :: Array String
  }

-- | Instruo's Arbhar, firmware 2.0. Six layers of ten seconds, plus three
-- | the module reads as a tail; a loop's layers load together as a *scene*,
-- | which is why a loop maps to one — six takes that were played against
-- | each other, and a layer-scan across them.
arbhar :: Face
arbhar =
  { id: "arbhar"
  , name: "Arbhar's Friend"
  , module_: "Arbhar"
  , maker: "Instruo"
  , layers: 6
  , layerSecs: 10.0
  , tailSecs: 3.0
  , unit: "scene"
  , layerWord: "layer"
  , holds: "36 scenes of 6 layers, and 36 single-layer library slots, per stick"
  , daemon: "itajara loop --device <device> --layers 6 --yes"
  , harvest: true
  , notes:
      [ "A layer holds 10 s; the first 3 s of the loop follow it as the tail the module reads past the end."
      , "Layers load in name order, 1_ to 6_; a scene's preset.txt with Load Layers loads audio without touching the panel."
      , "Link off and free lengths: nothing here needs a bar."
      ]
  }

-- | Make Noise Morphagene: a loop flat is one splice of a reel, or a loop's
-- | layers are a reel of splices. Both are one flag on the harvest.
morphagene :: Face
morphagene =
  { id: "morphagene"
  , name: "Morphagene's Friend"
  , module_: "Morphagene"
  , maker: "Make Noise"
  , layers: 8
  , layerSecs: 174.0
  , tailSecs: 0.0
  , unit: "reel"
  , layerWord: "splice"
  , holds: "32 reels of up to 300 splices, 174 s per reel"
  , daemon: "itajara loop --device <device> --layers 8 --yes"
  , harvest: false
  , notes: [ "A loop as a reel: each layer solo is one splice. Or the set as a reel: each loop flat is one splice." ]
  }

-- | Squarp Rample: a voice with a stack of samples, which is a loop with a
-- | stack of layers — the most exact fit of the four.
rample :: Face
rample =
  { id: "rample"
  , name: "Rample's Friend"
  , module_: "Rample"
  , maker: "Squarp"
  , layers: 12
  , layerSecs: 60.0
  , tailSecs: 0.0
  , unit: "voice"
  , layerWord: "sample"
  , holds: "kits A0 to Z99, four voices each, up to 12 samples per voice"
  , daemon: "itajara loop --device <device> --loops 4 --layers 12 --yes"
  , harvest: false
  , notes: [ "Four loops make one kit; a layer is one sample in the voice's stack." ]
  }

-- | vpme.de Quad Drum: short mono windows, a folder of samples per voice.
qd :: Face
qd =
  { id: "qd"
  , name: "QD's Friend"
  , module_: "Quad Drum"
  , maker: "vpme.de"
  , layers: 8
  , layerSecs: 4.0
  , tailSecs: 0.0
  , unit: "sample set"
  , layerWord: "sample"
  , holds: "128 samples per voice, mono"
  , daemon: "itajara loop --device <device> --loops 4 --layers 8 --yes"
  , harvest: false
  , notes: [ "Mono, and short: window each layer to the hit." ]
  }

faces :: Array Face
faces = [ arbhar, morphagene, rample, qd ]

-- | By id; the first face when the id is unknown or absent, so a bare URL is
-- | Arbhar's Friend.
faceFor :: Maybe String -> Face
faceFor mid = fromMaybe arbhar (mid >>= \i -> Array.find (\f -> f.id == i) faces)

-- | Whether the daemon, as it reports itself, suits this face. `Nothing` is
-- | "yes"; a sentence is what to change and how.
shapeNote :: Face -> LooperState -> Maybe String
shapeNote f top
  | top.maxLayers < f.layers =
      Just ("The daemon holds " <> show top.maxLayers <> " layers per loop; a "
        <> f.module_ <> " " <> f.unit <> " holds " <> show f.layers
        <> ". Start it with --layers " <> show f.layers <> " to fill one.")
  | otherwise = Nothing
