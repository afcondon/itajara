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
  , LoopState
  , LoopPhase(..)
  , phaseOf
  , phaseName
  , allPhases
  , isWriting
  , LayerShape
  , SocketStatus
  , connect
  , send
  , latest
  , status
  , defaultUrl
  , snapshotAge
  ) where

import Prelude

import Data.Maybe (Maybe)
import Data.Nullable (Nullable, toMaybe)
import Effect (Effect)

-- | Which of the six things a loop can be doing.
-- |
-- | **The daemon has an enum; the wire has a string; we had neither.** The
-- | snapshot carries `state` as text, and until now every consumer matched it
-- | against bare literals — five modules, twenty-odd string comparisons, and no
-- | single place saying what the alternatives were. So a missed case was not a
-- | type error, it was a wrong colour on a slot, and that is precisely how a
-- | loop came to be *actively recording and drawn as empty* for a whole
-- | session: `Slots` kept its own list of writing states, `Machine` kept a
-- | different one, and nothing could have told them apart.
-- |
-- | The set is closed and it is closed *at the source*: `state_name` in
-- | `itajara/src/engine.rs` matches on the engine's own state constants and can
-- | return exactly these six words. This type mirrors that function, and if one
-- | changes the other must — the same contract `LooperState` already has with
-- | `snapshot` in `ws.rs`.
data LoopPhase
  -- | Waiting for a sound rather than for a foot. Empty by definition: this is
  -- | what a loop is while it waits to stop being empty.
  = Armed
  -- | The three ways the input is open. `RecordingFirst` is laying material
  -- | into an empty loop; `Overdubbing` is another pass over what is there;
  -- | `Multiplying` is extending the length as it goes.
  | RecordingFirst
  | Overdubbing
  | Multiplying
  -- | Turning. Note this says nothing about being *audible* — `muted`,
  -- | `skipping` and `chance` are all orthogonal flags, which is why the
  -- | display asks them separately.
  | Playing
  -- | Nothing doing. Also where an unrecognised word lands — see `phaseOf`.
  | Idle

derive instance Eq LoopPhase
derive instance Ord LoopPhase

instance Show LoopPhase where
  show = phaseName

-- | The daemon's own word for a phase. The inverse of `phaseOf` on the six.
-- |
-- | Kept so the round trip can be tested against the strings the daemon really
-- | sends, rather than against a second copy of this list written in the test.
phaseName :: LoopPhase -> String
phaseName = case _ of
  Armed -> "armed"
  RecordingFirst -> "recordingFirst"
  Overdubbing -> "overdubbing"
  Multiplying -> "multiplying"
  Playing -> "playing"
  Idle -> "idle"

-- | Every phase, for tests and for anything that needs to enumerate them.
allPhases :: Array LoopPhase
allPhases = [ Armed, RecordingFirst, Overdubbing, Multiplying, Playing, Idle ]

-- | The wire's word, as a phase.
-- |
-- | **Total, and unknown words become `Idle` rather than a seventh case.** That
-- | is not a shrug — it is what the daemon itself does. `state_name` ends in
-- | `_ => "idle"`, so an engine state this app has never heard of already
-- | arrives called "idle"; adding an `Unknown String` constructor here would
-- | re-open the very set that closing is the point of, and force every `case`
-- | to carry a branch that can only be reached by a version skew that the
-- | constructor would not fix anyway.
-- |
-- | The cost is that a genuinely new seventh state would be silently read as
-- | idle. Three things guard that: the round-trip test over `allPhases`, the
-- | note above telling whoever edits `state_name` to come here, and the
-- | diagnostics readout in `Component.App`, which prints `state` raw — the one
-- | place an unrecognised word is still visible after this function has run.
-- |
-- | Row-polymorphic because the snapshot carries `state` twice: once per loop,
-- | and once at the top level where the flat legacy fields still describe
-- | whichever loop is selected. Those flat fields are on their way out, and
-- | taking the row rather than the record means this does not have to care
-- | when they go.
phaseOf :: forall r. { state :: String | r } -> LoopPhase
phaseOf st = case st.state of
  "armed" -> Armed
  "recordingFirst" -> RecordingFirst
  "overdubbing" -> Overdubbing
  "multiplying" -> Multiplying
  "playing" -> Playing
  _ -> Idle

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
  -- | All six loops, and which one the flat fields above describe.
  -- |
  -- | The duplication is deliberate and meant to be temporary. Everything above
  -- | describes ONE loop, because there was one when this type was written;
  -- | those fields now report whichever loop is selected, so this page keeps
  -- | working untouched while the six-loop display is built against `loops`.
  -- | Two new things at once is how you end up debugging both and understanding
  -- | neither. When the new display lands, the flat fields go.
  , selected :: Int
  , nLoops :: Int
  , loops :: Array LoopState
  }

-- | One loop as the daemon sees it.
type LoopState =
  { index :: Int
  , state :: String
  , layers :: Int
  , loopFrames :: Int
  , loopSecs :: Number
  , pos :: Int
  , phase :: Number
  , armed :: Boolean
  , recording :: Boolean
  -- | Whether this loop waits for the grid — the first loop's cycle — before
  -- | starting, and rounds its length to a whole number of those cycles.
  -- | Off by default, so a loop is free unless asked otherwise.
  , quant :: Boolean
  -- | Silenced but still turning. A flag rather than a state, because stopping
  -- | is orthogonal to recording — and phase-locked, so bringing a loop back
  -- | puts it where it would have been rather than where it started.
  , muted :: Boolean
  -- | Played backwards, and where it sits in the stereo field (0-127, 64
  -- | centre). Both are *resolutions* applied at playback rather than edits, so
  -- | they cost nothing to change and nothing to undo.
  , reverse :: Boolean
  , pan :: Int
  -- | Loop frames per output frame, as a magnitude — the engine keeps direction
  -- | in the sign and reports it separately as `reverse`, because the display
  -- | asks which way round a loop is far more often than it asks how fast.
  , speed :: Number
  -- | Forward, then back. Doubles the cycle rather than fitting into it.
  , pendulum :: Boolean
  -- | One pass per trigger, rather than turning for ever. Reported because it
  -- | changes what a **tap** means — a tap on a one-shot fires it where a tap
  -- | on any other loop stops it — and the app has to know which before the
  -- | foot lands, not after.
  , oneShot :: Boolean
  -- | Wait for a sound rather than starting on the press. Also changes what a
  -- | press does, and `armed` above is what it looks like while it waits.
  , levelArm :: Boolean
  -- | Whether a one-shot is inside a pass right now.
  -- |
  -- | Needed because `pos` keeps moving between passes — the playhead cannot
  -- | hold still — so a position readout on its own shows a one-shot sweeping
  -- | along while it is silent. The engine is the only thing that knows which,
  -- | so the engine says.
  , firing :: Boolean
  -- | How often a pass sounds, as a probability; `1.0` is always. A gate on the
  -- | mix and nothing else — the playhead keeps turning and `origin` never
  -- | moves, exactly like `muted`.
  , chance :: Number
  -- | Whether chance is holding *this* pass back. Read from the mixer's own
  -- | decision rather than worked out here: the roll happens once per pass in
  -- | the audio callback, and a display that rolled its own would disagree with
  -- | what is coming out of the speakers.
  , skipping :: Boolean
  -- | How much of the wrap is crossfaded with the layer's continuation, in
  -- | milliseconds; zero is a hard join. In milliseconds rather than frames so
  -- | the display never needs the sample rate to say what a switch did.
  , fadeMs :: Number
  -- | How much a pass costs the material already there, in decibels; zero holds
  -- | for ever. The parameter that separates Frippertronics from song looping,
  -- | and the reason a loop can now have a shape it was not given.
  , decayDb :: Number
  -- | Frames until a scheduled transition fires, or -1 when nothing is
  -- | pending. What lets the display say "starts in 1.4 s" rather than leaving
  -- | a deliberate wait looking like a dead button.
  , pendingAt :: Int
  , shapes :: Array LayerShape
  }

-- | Whether the loop has the input open right now, in any of the three ways it
-- | can have it.
-- |
-- | **One predicate, because two copies of this disagreed and it cost a
-- | session.** The daemon reports three writing states and every consumer has to
-- | know the same three: the meaning table, to decide whether a press closes
-- | something; the display, to colour it. `Data.Looper.Machine` had its own
-- | list, `Component.Looper.Slots` had a different one, and the difference was
-- | exactly `overdubbing` — so a loop that had been undone to nothing and then
-- | recorded into again was **actively writing and drawn as empty**, holding the
-- | one converter the rig has with nothing on screen to say so.
-- |
-- | Note this is deliberately *not* `st.recording`, which the daemon also
-- | reports. Trusting a derived boolean over the state it was derived from is
-- | how the two got out of step in the first place; the state is the thing the
-- | engine actually switches on.
-- |
-- | Written as an exhaustive `case` rather than a chain of `||`, so that adding
-- | a seventh phase makes the compiler ask whether it writes.
isWriting :: LoopState -> Boolean
isWriting st = case phaseOf st of
  RecordingFirst -> true
  Overdubbing -> true
  Multiplying -> true
  Armed -> false
  Playing -> false
  Idle -> false

type LayerShape =
  { len :: Int
  , period :: Int
  , phase :: Int
  -- | Frames of *continuation* held past this layer's end: what was still
  -- | being played when the loop closed. Never sounded — playback is
  -- | `pos % len` — and the only material a seamless wrap could be made from,
  -- | which is why it is kept rather than trimmed.
  , tail :: Int
  -- | What this layer is currently worth, after however many passes it has
  -- | lived through. One for every layer of a loop that is not decaying — and
  -- | the only way the display can show a loop receding, since nothing in the
  -- | arena changes.
  , gain :: Number
  -- | The layer's shape, as peaks 0-255 across its own length.
  -- |
  -- | **Absolute and logarithmic, never normalised per layer.** The picture is
  -- | for telling one loop from another at a glance and for not firing the loud
  -- | one when you meant the quiet one — and the second of those is destroyed
  -- | the moment each layer is scaled to its own peak. Zero is silence, 255 is
  -- | full scale, and the floor is -60 dBFS.
  -- |
  -- | Small enough to ride in the ordinary snapshot, which is why there is no
  -- | second message type, no request to trigger one, and no way for the
  -- | picture to be of audio that has changed since.
  , env :: Array Int
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

-- | Milliseconds since the newest snapshot arrived, or negative if none has.
-- |
-- | **This one has to come from JavaScript, and only this one.** The arrival
-- | time is kept by the socket callbacks in this module's own JS state, which
-- | is what an FFI wrapper around `WebSocket` is for — there is no PureScript
-- | side to read it from. A timer, by contrast, did *not* need to come from
-- | here: that was a shortcut around a problem whose real fix was structural,
-- | and it is gone. See `Component.App`'s poll subscription.
-- |
-- | Note it only helps while something is still redrawing. A frozen renderer
-- | cannot announce that it is frozen.
foreign import snapshotAgeImpl :: Effect Number

snapshotAge :: Effect Number
snapshotAge = snapshotAgeImpl

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

-- | The daemon's REGISTERED address — the port the fleet knows it by, which is
-- | not necessarily the port it is listening on. `connect` resolves this
-- | through Bosun's `/where` before dialling (and re-resolves on every
-- | reconnect), so under `serveMode: broker` the socket goes straight to the
-- | daemon and the relay leaves the path without a line changing here. See the
-- | header of `LooperSocket.js`; `?looper=ws://…` overrides the lot.
-- |
-- | Loopback only, either way: the daemon binds nothing else.
defaultUrl :: String
defaultUrl = "ws://127.0.0.1:3028"
