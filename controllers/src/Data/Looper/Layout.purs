-- | What the looper's controllers are laid out for.
-- |
-- | The vocabulary (`Data.Looper.Duty`) and the machine (`Data.Looper.Machine`)
-- | live with the daemon now, in the itajara client package, and neither has
-- | a number of loops in it: the machine reads the count from the snapshot.
-- | This app does have a number, because its bank tables and the Twister's
-- | rows are drawn for one, and that number is here — the one fact about the
-- | looper that belongs to the pedalboard and not to the engine.
module Data.Looper.Layout
  ( nLoops
  ) where

-- | How many loops this surface is laid out for. The daemon's `--loops` is
-- | what there is; `Component.App` says once, in the log, when the two differ.
-- |
-- | Eight to match the Twister's 4×4: the top two rows of its first bank are
-- | the loops, one encoder each. Seven and eight are reachable from the page
-- | and the Twister and not from the MC6, which is not a deficiency — they are
-- | the ones you *set up* rather than stomp.
nLoops :: Int
nLoops = 8
