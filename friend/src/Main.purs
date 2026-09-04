module Main where

import Prelude

import Data.Array as Array
import Data.Maybe (Maybe(..))
import Data.String as String
import Effect (Effect)
import Effect.Class (liftEffect)
import Effect.Exception (throw)
import Friend.App as App
import Friend.Face as Face
import Halogen.Aff as HA
import Halogen.VDom.Driver (runUI)
import Web.DOM.ParentNode (QuerySelector(..))
import Web.HTML (window)
import Web.HTML.Location (search)
import Web.HTML.Window (location)

main :: Effect Unit
main = HA.runHalogenAff do
  HA.awaitLoad
  q <- liftEffect (window >>= location >>= search)
  let face = Face.faceFor (param "face" q)
  mEl <- HA.selectElement (QuerySelector "#app")
  case mEl of
    Nothing -> liftEffect $ throw "No #app element found"
    Just el -> void (runUI App.component face el)

-- | One query parameter, by name, from `?a=b&c=d`. Enough for a face id.
param :: String -> String -> Maybe String
param key q =
  let
    body = String.drop 1 q
    pairs = String.split (String.Pattern "&") body
    hit = Array.findMap (\p -> String.stripPrefix (String.Pattern (key <> "=")) p) pairs
  in
    hit
