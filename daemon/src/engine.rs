//! The looper proper: transport, layers, record, overdub, undo.
//!
//! Built on the arithmetic `align` proves. Two rules carry over and neither is
//! negotiable:
//!
//! - **Loop position is a device frame count**, never a host-clock instant. The
//!   two clocks differ by ~15.6 ppm here (DESIGN-LOOPER §10) and anything
//!   derived from the host clock walks away from the audio at 0.75 samples a
//!   second.
//! - **`out_frame = in_frame + K`**, with `K` established once at the first
//!   input callback and never recomputed. After that it is integer addition.
//!
//! ## Layers, not mixdown
//!
//! Every overdub is its own buffer, summed at playback (§4). Undo is then free,
//! and so is muting, reversing or re-rendering one layer while the rest play.
//!
//! ## How memory is handled, and why it looks odd
//!
//! Audio callbacks must not allocate, and two callbacks need access to the same
//! layer storage — the input side writes the layer being recorded while the
//! output side reads the ones already committed. The usual answers are unsafe
//! aliasing or a lock-free handoff of buffer ownership.
//!
//! Instead every sample is an `AtomicU32` holding f32 bits, accessed `Relaxed`.
//! On any machine this runs on that compiles to exactly the same load and store
//! as a plain `f32` — the atomics buy the absence of undefined behaviour, not
//! synchronisation, and cost nothing. The whole arena is allocated once at
//! startup, so no callback ever touches the allocator.
//!
//! The price is a fixed ceiling on loop length and layer count, which is what
//! `--max-secs` and `MAX_LAYERS` are. At the defaults the arena is 46 MB.

use cpal::traits::{DeviceTrait, StreamTrait};
use std::error::Error;
use std::io::BufRead;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use rand::rngs::SmallRng;
use rand::{Rng, SeedableRng};

use crate::measure::{choose_input, choose_output, signed_secs, Width};

pub const MAX_LAYERS: usize = 8;

/// Transport states, as a `u8` because the audio thread reads it every buffer.
const IDLE: u8 = 0;
/// Waiting for the output callback to stamp the exact frame recording begins.
///
/// Also, and for a long time only nominally, the state a **level-armed** loop
/// sits in while it listens. `ARMED` was written as a request value and never
/// once set as a state — `is_armed()` could not return true, and the `armed`
/// field has been going out in every snapshot reading `false` since the socket
/// existed. Level-arm is what it was always describing: the loop has claimed
/// the input and is not yet writing to it.
const ARMED: u8 = 1;
/// Recording the first loop: linear, and its length becomes the cycle.
const FIRST: u8 = 2;
/// Recording an overdub: modular, into a buffer one cycle long.
const OVERDUB: u8 = 3;
/// Playing, not recording.
const PLAYING: u8 = 4;
/// Recording across several cycles, to make the loop an integer multiple longer
/// with what is already there repeating underneath. The EDP's `Multiply`.
const MULTIPLY: u8 = 5;
/// A request only, never a state: play one pass from the top and stop.
const FIRE: u8 = 6;

/// How far before the threshold crossing a level-armed recording begins.
///
/// **The crossing is not the start of the sound, it is the middle of the
/// attack.** A threshold low enough to catch the very front of a pluck is a
/// threshold that fires on the room; a threshold high enough not to fire on the
/// room is one that arrives some milliseconds into the note. Reaching backwards
/// dissolves the trade: the ring already holds those milliseconds, and level-arm
/// can pick a threshold that will not misfire and then take the attack anyway.
///
/// Fifty is comfortably past the front of anything with a pick or a stick on it,
/// and comfortably short of catching the previous bar.
const ARM_REACH_MS: f64 = 50.0;

/// Everything that makes a layer what it is.
///
/// A value rather than three arguments, because three integers in a row is
/// exactly where a transposition hides — and because it says out loud that a
/// layer is described in one place. `tail` used to be left alone and went stale;
/// `born` would do the same, and a layer born at the wrong pass is a layer that
/// arrives already faded.
struct Shape {
    len: usize,
    /// Frames of continuation past the end, for the wrap fade.
    tail: usize,
    /// The pass the layer was laid on: where its decay counts from.
    born: i64,
}

/// How many buckets a layer's envelope is drawn with.
///
/// **Deliberately coarse.** The job the picture does is telling one loop from
/// another at a glance and not firing the loud one when you meant the quiet one
/// — both of which are questions about *shape*, and a shape is legible long
/// before it is detailed. Forty-eight is about a bucket every two millimetres
/// at the size a slot actually is, and it makes the whole envelope for six
/// loops small enough to ride in the ordinary snapshot rather than needing a
/// message of its own.
const ENV_BUCKETS: usize = 48;

/// The quietest thing the envelope draws, in dBFS.
///
/// **Absolute, and on a decibel curve, which is the whole point.** Scaling each
/// layer to its own peak is what a waveform editor does and it would destroy the
/// one thing this is for: a quiet loop would draw exactly as tall as a loud one.
/// Linear against full scale is honest and useless — a loop peaking at -20 dBFS
/// would be a tenth of the height and one at -40 would be invisible. Sixty
/// decibels of range on a log curve is what every meter does, for the same
/// reason.
const ENV_FLOOR_DB: f32 = -60.0;

/// The longest wrap crossfade, and so the most continuation worth keeping past
/// a layer's end.
///
/// Half a second is already far longer than a join wants; past that it stops
/// being a join and becomes a different effect, which should be asked for by
/// its own name rather than by winding this one up.
const MAX_FADE_MS: f64 = 500.0;

pub struct Opts {
    pub device: String,
    pub in_ch: usize,
    pub out_ch: usize,
    pub residual: f64,
    /// Whether `--residual` was actually given, as against left at its default.
    ///
    /// The default is not "no compensation", it is a number — so without this
    /// the engine cannot tell an operator who measured 252 from one who never
    /// looked, and cannot say which it is doing.
    pub residual_given: bool,
    pub max_secs: f64,
    pub sample_rate: u32,
    pub buffer: Option<u32>,
    pub click: bool,
    pub selftest: Option<f64>,
    pub ring_secs: f64,
    /// How far before the press the first recording actually begins, pulled
    /// from the ring. A tap is always a little late; this makes that harmless
    /// instead of clipping the attack off the front of the loop.
    pub preroll_ms: f64,
    /// Send the mix to `out_ch` and `out_ch + 1` rather than one channel. On by
    /// default: monitors are a pair, and a loop in one ear is not a loop you can
    /// judge.
    pub dual: bool,
    /// Pass the live input through to the output. Off by default because the
    /// interface's own direct monitoring is strictly better — it costs no
    /// latency, where this costs the round trip plus a buffer. Useful on
    /// headphones with nothing else in the room.
    pub monitor: bool,
    /// TCP port for the app to connect on. None keeps the daemon console-only.
    pub ws_port: Option<u16>,
    /// Where `w` writes takes. Under `$HOME` by convention, beside `~/.es9` and
    /// `~/.fh2`.
    pub takes_dir: PathBuf,
    /// UDP port to hear `/link/anchor` on. `None` runs the looper without a
    /// bar, which is the right default for using it alone.
    pub link_port: Option<u16>,
    /// dBFS a sound has to reach to start a level-armed recording. Changeable
    /// while running with `arm<db>`; this is only where it starts.
    pub arm_db: f64,
}

impl Default for Opts {
    fn default() -> Self {
        Opts {
            device: String::new(),
            in_ch: 0,
            out_ch: 0,
            residual: 252.0,
            residual_given: false,
            max_secs: 30.0,
            sample_rate: 48_000,
            buffer: None,
            click: false,
            selftest: None,
            ring_secs: 60.0,
            preroll_ms: 0.0,
            dual: true,
            monitor: false,
            ws_port: None,
            takes_dir: default_takes_dir(),
            link_port: None,
            arm_db: -36.0,
        }
    }
}

/// The arm threshold as the player would say it, for every ack that mentions it.
///
/// One function rather than the conversion written out at each call site: three
/// acks quote this number, and three copies of a `log10` is three chances for
/// the daemon to describe a threshold it is not using.
fn thresh_words(sh: &Shared) -> String {
    let mag = f32::from_bits(sh.arm_thresh.load(Ordering::Relaxed));
    format!("{:.0} dBFS", 20.0 * (mag.max(1e-9) as f64).log10())
}

/// One frame of the wrap fade: the head arrived at through the continuation.
///
/// At `p = 0` this is almost entirely the continuation — the frame that truly
/// followed the last one played — and by `p = n` it is the recording again.
/// Split out from `sample_at` so the property it exists for can be asserted
/// without standing up an arena.
fn wrap_mix(head: f32, tail: f32, p: usize, n: usize) -> f32 {
    let t = (p + 1) as f32 / (n + 1) as f32;
    tail * (1.0 - t) + head * t
}

/// Decay as the board says it, in the unit it was asked for.
fn decay_words(lp: &Loop) -> String {
    let d = lp.decay_of();
    if d >= 1.0 {
        return "holds every layer for ever".into();
    }
    format!("loses {:.0} dB a pass", -20.0 * d.max(1e-9).log10())
}

/// The wrap fade as the board says it.
fn fade_words(lp: &Loop, sr: u32) -> String {
    match lp.fade.load(Ordering::Relaxed) {
        0 => "a hard join".into(),
        f => format!("{:.0} ms of crossfade", f as f64 / sr as f64 * 1000.0),
    }
}

/// A peak as a byte on the envelope's decibel scale.
///
/// Zero is silence and 255 is full scale, with `ENV_FLOOR_DB` at the bottom.
/// Absolute, never per layer: a loop twelve decibels quieter than its neighbour
/// has to *look* twelve decibels quieter, or the picture cannot do the one job
/// it is here for.
fn to_byte(peak: f32) -> u8 {
    if peak <= 0.0 {
        return 0;
    }
    let db = 20.0 * peak.log10();
    let t = 1.0 + db / -ENV_FLOOR_DB;
    (t.clamp(0.0, 1.0) * 255.0).round() as u8
}

/// A probability as the board says it, for the acks.
///
/// The named rungs are the ones the app's ladder offers, so a press and its ack
/// use the same words; anything else set by hand gets a percentage rather than
/// being rounded to the nearest rung it is not on.
fn odds_words(p: f32) -> String {
    match p {
        _ if p >= 1.0 => "every pass".into(),
        _ if p <= 0.0 => "never".into(),
        _ if (p - 0.75).abs() < 1e-4 => "3 passes in 4".into(),
        _ if (p - 0.5).abs() < 1e-4 => "1 pass in 2".into(),
        _ if (p - 0.25).abs() < 1e-4 => "1 pass in 4".into(),
        _ if (p - 0.125).abs() < 1e-4 => "1 pass in 8".into(),
        _ => format!("{:.0}% of passes", p * 100.0),
    }
}

/// dBFS to a magnitude, floored at silence rather than at minus infinity.
///
/// A threshold of exactly zero would fire on the first denormal the converter
/// produced, so "off" is not expressible here and is not meant to be — a
/// level-arm with no threshold is a level-arm that starts immediately, which is
/// what plain record already does.
fn db_to_mag(db: f64) -> f32 {
    (10f64.powf(db / 20.0)).clamp(1e-6, 1.0) as f32
}

/// `~/.itajara/takes`, or a relative path if there is no home — which happens
/// under some launchers, and is better than refusing to save at all.
pub fn default_takes_dir() -> PathBuf {
    match std::env::var_os("HOME") {
        Some(home) => PathBuf::from(home).join(".itajara").join("takes"),
        None => PathBuf::from("itajara-takes"),
    }
}

/// Everything both callbacks and the control thread touch.
/// How many loops the engine holds.
///
/// Six, because the MC6 has six main switches and the whole design rests on one
/// switch owning one loop. The cost is linear and paid at startup: the arena is
/// `N_LOOPS × MAX_LAYERS × max_secs`, so six loops of eight layers at the
/// default thirty seconds is 259 MB, allocated once and never touched by the
/// allocator again.
pub const N_LOOPS: usize = 6;

/// One loop: its layers, its cycle, and where it stands in it.
///
/// Split out of `Shared` when the engine went from one loop to six. What lives
/// here is what a loop can have an opinion of its own about; what stays on
/// `Shared` is what belongs to the rig — the single input's pre-roll, the frame
/// counters, the latency calibration, the clock. The division is not stylistic:
/// there is one audio device, so there is one K and one ring no matter how many
/// loops there are, and duplicating those per loop would be six chances to
/// disagree about what time it is.
pub struct Loop {
    pub loop_len: AtomicUsize,
    pub n_layers: AtomicUsize,
    /// Each layer's own length, and where in the cycle it sounds.
    ///
    /// A layer is **not** stretched to fill the loop. It keeps the length it was
    /// recorded at, sounds once every `period` of its own lengths, and sits at
    /// slot `phase` within that period. Playback resolves all three.
    ///
    /// This is what makes two kinds of multiply one mechanism. `period = 1` is
    /// an ordinary layer, repeating every time round — which is what the old
    /// code achieved by copying the audio n times into the longer cycle. Set
    /// `period = 4, phase = 3` and the same bar sounds once in four: `~ ~ ~ B`.
    /// Since nothing was flattened, it can go back, or move, or alternate,
    /// afterwards. Tiling could not: it destroyed the fact that there was a
    /// one-bar thing there at all, which is the same reason a `MidiClip` in
    /// Triggerfish stores every note and bakes in no tempo.
    l_len: Vec<AtomicUsize>,
    /// How many frames of *continuation* sit past each layer's end.
    ///
    /// The audio that was played after the loop closed. It is not spare and it
    /// is not rubbish: it is the only material that can make the wrap seamless,
    /// because a crossfade at the loop point needs to know what would have come
    /// next — and what would have come next is exactly what the player kept
    /// playing while the gesture was still being worked out.
    ///
    /// Never sounded. Playback is `pos % l_len`, so anything past the end does
    /// not exist until something asks for it. *Store everything, flatten late*,
    /// which is the same rule `MidiClip` follows in Triggerfish for the same
    /// reason: the lossy step belongs at the end, where it can be undone.
    l_tail: Vec<AtomicUsize>,
    /// The pass this layer was laid on, which is where its decay counts from.
    ///
    /// Per layer rather than per loop, and that is the whole of what makes decay
    /// sound like tape rather than like a fader: new material enters at full
    /// while everything underneath goes on receding from its own beginning. It
    /// is also what a single feedback gain cannot do, because a feedback gain
    /// destroys as it goes and has no idea how old anything is.
    l_born: Vec<AtomicI64>,
    /// Each layer's envelope, as `ENV_BUCKETS` bytes on the scale
    /// `ENV_FLOOR_DB` describes.
    ///
    /// A mutex rather than atomics because nothing real-time goes near it: the
    /// control thread writes it when a layer's content changes, and the socket
    /// thread reads it to build a snapshot. The audio thread has no business
    /// here at all.
    env: Mutex<Vec<Vec<u8>>>,
    /// This layer's decay gain right now, recomputed once per buffer.
    ///
    /// Cached because it only changes at a pass boundary and the mixer runs per
    /// frame. Six loops times eight layers of `powi` once a buffer is nothing;
    /// the same arithmetic per frame would be real.
    l_gain: Vec<AtomicU32>,
    l_period: Vec<AtomicUsize>,
    l_phase: Vec<AtomicUsize>,
    /// The output frame at which this loop's position zero sits.
    ///
    /// Per loop, which is what lets six loops of different lengths run at once
    /// without any of them being the master. Whether they *should* be free of
    /// each other is a musical question, and the answer is a quantisation
    /// policy applied when a loop closes — not a shared origin, which would
    /// decide it here and for ever.
    pub origin: AtomicI64,
    /// Silenced, but still turning.
    ///
    /// **Phase-locked, deliberately.** The playhead keeps advancing while a loop
    /// is stopped, so bringing it back is not "start again" but "become audible
    /// again, where you would have been". With six loops that is the only
    /// behaviour worth having: a loop that restarted from its own zero would
    /// come back out of phase with everything it was recorded against.
    ///
    /// It is also why this is a flag rather than a state. Stopping is
    /// orthogonal to the record machine — a loop can be stopped while playing
    /// or while overdubbing — and folding it into `state` would make the
    /// machine describe two things at once. `Data.Loopy`, removed from the app
    /// long before this existed, had already reached the same conclusion and
    /// called it `PhaseMuted`.
    ///
    /// The alternative — moving `origin` — is the one thing that must never
    /// happen to a loop that closed on a grid boundary.
    pub muted: AtomicBool,
    /// Loop frames travelled per output frame. Negative plays backwards.
    ///
    /// A *resolution*, like `period` and `phase`: the samples are untouched and
    /// the playhead is simply asked to move at a different rate, so speed costs
    /// nothing to change and nothing to change back.
    ///
    /// **Direction is the sign, not a separate flag.** It was a flag for a day,
    /// and a flag is a second source of truth about which way the playhead is
    /// going — SuperDirt has always spelt backwards as a negative `speed`, and
    /// splitting them here would invent a distinction the rest of the rig does
    /// not make. Folding it in also removed a click: mirroring `pos` to
    /// `len - 1 - pos` jumps the playhead across the loop at the instant you
    /// press it, where a sign change simply turns round where it stands.
    ///
    /// `f64` in an `AtomicU64` because the position it drives is an absolute
    /// frame count, and `f32` runs out of mantissa at 16.7 M — about six
    /// minutes at 48 k, which is well inside what a long take can reach.
    pub speed: AtomicU64,
    /// Forward, then back: the playhead reflects at each end instead of
    /// wrapping, so a cycle takes twice as long and the loop is heard both ways
    /// round.
    ///
    /// Free, given speed. A pendulum is a triangle where a plain loop is a
    /// sawtooth, and the fold is two lines in the same place the wrap already
    /// happens — which is why it is here rather than on the list of things
    /// waiting for engine work.
    pub pendulum: AtomicBool,
    /// Where the playhead sits at `origin`, in loop frames.
    ///
    /// Zero until something changes speed. Playback is `warp + (frame -
    /// origin) * speed`, so at `warp = 0, speed = 1` it is exactly the
    /// subtraction it has always been, down to the bit — which is what keeps
    /// the alignment self-test a regression test rather than a new claim.
    ///
    /// It exists because a speed change must not move the audio. Rescaling the
    /// whole history would jump the playhead by however far it had already
    /// come; instead the callback records where the loop *is* and rescales only
    /// what happens next.
    warp: AtomicU64,
    /// A pending speed and pendulum, consumed by the output callback.
    ///
    /// The same argument as `request_at`: only the callback knows the frame, and
    /// re-anchoring `warp` against a frame the control thread guessed would be
    /// out by up to a buffer — 21 ms of jump at 1024 frames, which is a click.
    cfg_speed: AtomicU64,
    cfg_pend: AtomicBool,
    cfg_armed: AtomicBool,
    /// Stereo placement, 0 hard left to 127 hard right, 64 centre.
    ///
    /// Equal-power, and the gains are computed once per buffer rather than once
    /// per frame — six loops times two `cos` calls is nothing at buffer rate and
    /// wasteful at sample rate.
    pub pan: AtomicUsize,
    state: AtomicU8Wrapper,
    /// Set by the control thread, consumed by the output callback, which is the
    /// only place a transition can be stamped to an exact frame.
    request: AtomicU8Wrapper,
    /// The output frame the pending request should take effect on, or
    /// `i64::MIN` for "the next buffer", which is what every request used to be.
    ///
    /// This is what makes a loop start *on* a boundary rather than within a
    /// buffer of one. Sleeping on the control thread until the boundary and
    /// then setting the request would still land at the start of whichever
    /// buffer came next — up to a full buffer late, and a buffer is 21 ms at
    /// 1024 frames, which is an audible flam against a loop already playing.
    /// The callback is the only thread that knows the frame, so the frame is
    /// what it is told.
    request_at: AtomicI64,
    /// Whether this loop's transitions wait for the grid.
    ///
    /// Off by default, so a rig that never asks for it behaves exactly as it
    /// did — which is also what keeps the self-test a regression test rather
    /// than a description of new behaviour.
    quant: AtomicBool,
    /// Highest position the first recording reached, so a loop can be closed at
    /// the right length even though the input trails the output.
    reached: AtomicUsize,
    /// Highest output frame the input callback actually wrote for this
    /// recording, one past the end.
    ///
    /// Asked rather than inferred. Undoing an overdub's wrapped tail means
    /// subtracting exactly the samples that were added, and "how far did the
    /// input get" cannot be worked out from a clock afterwards: the flip to
    /// PLAYING stops the writes, but frames keep arriving and the drain sleep
    /// lets an in-flight callback finish. Reading `in_frames` afterwards
    /// therefore names frames that were never recorded, and subtracting those
    /// would gouge real audio out of the loop head — a ghost where there had
    /// been a doubling.
    rec_reached: AtomicI64,
    /// How far back up the layer stack still holds audio, so undo can be
    /// taken back.
    ///
    /// Undo no longer zeroes what it removes, so an undone layer is still
    /// there and can simply be counted back in. This is the highest layer
    /// index that is still recoverable; recording into a layer moves it,
    /// because a take that has been recorded over is not recoverable and
    /// offering to redo it would be a lie.
    redo_to: AtomicUsize,
    overflowed: AtomicBool,
    /// How late the press that started this recording was, in frames.
    ///
    /// The app knows when the MIDI arrived and the daemon does not, so lateness
    /// travels on the command (`0r@312`) and is kept here until the recording
    /// closes — which is the only moment it can be spent, because the pre-roll
    /// shift happens at commit.
    ///
    /// Zero means "no measurement", and the compiled `--preroll-ms` is used
    /// instead. That is deliberately not the same as a measured zero: a rig
    /// that cannot time its own presses should still be able to say "always
    /// reach back 40 ms" by hand.
    started_late: AtomicI64,
    /// Output frame at which the layer being recorded has its position zero.
    /// Equal to `origin` for a first recording; for a multiply it is the cycle
    /// boundary the multiply started on, which is also where the *new* loop's
    /// position zero will end up.
    rec_from: AtomicI64,
    /// Play one pass and stop, rather than turning for ever.
    ///
    /// A mode rather than a state, like `muted` and for the same reason: it is
    /// orthogonal to the record machine. A one-shot can be recorded into, undone
    /// and overdubbed exactly as any other loop; the only thing it changes is
    /// what happens between fires, which is silence.
    pub one_shot: AtomicBool,
    /// The output frame the current pass ends at, or `i64::MIN` for "not
    /// sounding".
    ///
    /// `i64::MIN` rather than a separate flag so that switching the mode on puts
    /// a loop straight into the silence it will spend most of its life in — one
    /// comparison in the mixer, no second thing to keep in step.
    shot_end: AtomicI64,
    /// Wait for a sound rather than starting on the press.
    ///
    /// The other half of *"we can't go back in time, but we're monitoring
    /// continuously"*: with the ring running, arming costs nothing and the
    /// recording can begin before the command that caused it.
    pub level_arm: AtomicBool,
    /// The output frame a pending recording should be back-dated to, or
    /// `i64::MIN` for none.
    ///
    /// Written by the input callback at the threshold crossing, read by the
    /// output callback when it stamps the recording. The two cannot be the same
    /// frame — the crossing is found on the input thread and the transition is
    /// stamped on the output one — so the difference is handed to `started_late`
    /// and spent as pre-roll, which is the machinery a late footswitch already
    /// built.
    arm_from: AtomicI64,
    /// How many frames of the wrap are crossfaded with the layer's continuation.
    /// Zero is off, which is the default.
    ///
    /// **A resolution applied at playback, not an edit.** The samples are never
    /// touched: the mixer reads two of them near a wrap instead of one. So the
    /// length can be changed while the loop plays, turned off, and undone by
    /// turning it off — the same standing as speed, pan and direction, and the
    /// same reason. *Store everything, flatten late.*
    pub fade: AtomicUsize,
    /// How much of itself a layer keeps from one pass to the next. `1.0` holds
    /// for ever, which is the default and what a looper has always done.
    ///
    /// **The parameter that separates Frippertronics from song looping.** Two
    /// Revoxes with the second one feeding back below unity is this number, and
    /// so is what a tape echo does to its repeats. Without it every layer plays
    /// at full for ever and the only shape a loop can have is the one it was
    /// given.
    ///
    /// A resolution at playback like speed, pan and the wrap fade — nothing is
    /// scaled in the arena — so a loop that has faded to nothing is still all
    /// there, and turning decay off brings it back.
    pub decay: AtomicU32,
    /// How often a pass sounds, as a probability. `1.0` is always.
    ///
    /// A gate on the mix and nothing else — the playhead keeps turning, `origin`
    /// never moves, and the pass count keeps counting. Exactly the shape of
    /// `muted`, and phase-locked for the same reason: a loop that plays one
    /// cycle in four has to come back on the cycle it would have been on, or it
    /// is not one cycle in four of anything.
    pub chance: AtomicU32,
    /// Which pass the last roll was for, and what it came up.
    ///
    /// The roll happens in the mixer, which runs per frame — so it has to be
    /// remembered, or a one-in-four loop would flicker at sample rate instead of
    /// dropping cycles. One roll per pass, held for the whole pass.
    chance_pass: AtomicI64,
    chance_sounds: AtomicBool,
}

impl Loop {
    fn new() -> Self {
        Loop {
            loop_len: AtomicUsize::new(0),
            n_layers: AtomicUsize::new(0),
            l_len: (0..MAX_LAYERS).map(|_| AtomicUsize::new(0)).collect(),
            l_tail: (0..MAX_LAYERS).map(|_| AtomicUsize::new(0)).collect(),
            l_born: (0..MAX_LAYERS).map(|_| AtomicI64::new(0)).collect(),
            l_gain: (0..MAX_LAYERS).map(|_| AtomicU32::new(1.0f32.to_bits())).collect(),
            env: Mutex::new((0..MAX_LAYERS).map(|_| Vec::new()).collect()),
            l_period: (0..MAX_LAYERS).map(|_| AtomicUsize::new(1)).collect(),
            l_phase: (0..MAX_LAYERS).map(|_| AtomicUsize::new(0)).collect(),
            origin: AtomicI64::new(0),
            muted: AtomicBool::new(false),
            speed: AtomicU64::new(1.0f64.to_bits()),
            pendulum: AtomicBool::new(false),
            warp: AtomicU64::new(0.0f64.to_bits()),
            cfg_speed: AtomicU64::new(1.0f64.to_bits()),
            cfg_pend: AtomicBool::new(false),
            cfg_armed: AtomicBool::new(false),
            pan: AtomicUsize::new(64),
            state: AtomicU8Wrapper::new(IDLE),
            request: AtomicU8Wrapper::new(0),
            request_at: AtomicI64::new(i64::MIN),
            quant: AtomicBool::new(false),
            reached: AtomicUsize::new(0),
            rec_reached: AtomicI64::new(0),
            redo_to: AtomicUsize::new(0),
            overflowed: AtomicBool::new(false),
            rec_from: AtomicI64::new(0),
            started_late: AtomicI64::new(0),
            one_shot: AtomicBool::new(false),
            shot_end: AtomicI64::new(i64::MIN),
            level_arm: AtomicBool::new(false),
            arm_from: AtomicI64::new(i64::MIN),
            fade: AtomicUsize::new(0),
            decay: AtomicU32::new(1.0f32.to_bits()),
            chance: AtomicU32::new(1.0f32.to_bits()),
            chance_pass: AtomicI64::new(i64::MIN),
            chance_sounds: AtomicBool::new(true),
        }
    }

    pub fn speed(&self) -> f64 {
        f64::from_bits(self.speed.load(Ordering::Relaxed))
    }

    /// Whether the playhead is doing the plain thing: forward, at rate one, from
    /// `origin`.
    ///
    /// Everything that *writes* asks this first. Recording at a speed is a
    /// different instrument — the input arrives at rate one and would have to be
    /// resampled into a buffer whose grid is moving — and the honest answer for
    /// now is to refuse and say so, rather than record something nobody asked
    /// for. Playback is where speed belongs, and playback is where it is.
    pub fn plain(&self) -> bool {
        self.speed() == 1.0
            && !self.pendulum.load(Ordering::Relaxed)
            && f64::from_bits(self.warp.load(Ordering::Relaxed)) == 0.0
    }

    /// Where the playhead is, in loop frames, at an output frame.
    ///
    /// Fractional, which is the whole of what speed costs: at any rate but one
    /// the playhead lands between samples, and the mix has to interpolate.
    ///
    /// The pendulum fold happens here rather than in the caller because it is a
    /// property of *where the playhead is*, not of what is read there — and
    /// keeping it here means the display and the audio cannot disagree about
    /// which way round a loop currently is.
    /// Where the playhead is *before* it is folded back into the loop: how far
    /// it has travelled since `origin`, in loop frames, without wrapping.
    ///
    /// Both the position and the pass count come out of this one expression, so
    /// "where in the cycle" and "which cycle" cannot come to disagree — which
    /// they would the first time speed or a pendulum was involved and only one
    /// of them was taught about it.
    fn raw_pos(&self, out_frame: i64) -> f64 {
        let warp = f64::from_bits(self.warp.load(Ordering::Relaxed));
        let origin = self.origin.load(Ordering::Acquire);
        warp + (out_frame - origin) as f64 * self.speed()
    }

    /// How many complete trips through the loop have gone by, counting from
    /// `origin`. Negative before it, which is honest rather than clamped.
    ///
    /// One *pass* is what chance rolls for, and a pendulum's pass is there and
    /// back — the same span `pass_frames` measures, so a swinging loop that
    /// plays one cycle in four drops a whole there-and-back rather than half of
    /// one.
    pub fn pass_index(&self, out_frame: i64, len: usize) -> i64 {
        if len == 0 {
            return 0;
        }
        let span = if self.pendulum.load(Ordering::Relaxed) { 2 * len } else { len } as f64;
        (self.raw_pos(out_frame) / span).floor() as i64
    }

    pub fn play_pos(&self, out_frame: i64, len: usize) -> f64 {
        if len == 0 {
            return 0.0;
        }
        let raw = self.raw_pos(out_frame);
        let lenf = len as f64;
        if self.pendulum.load(Ordering::Relaxed) {
            // A triangle where a plain loop is a sawtooth. `2 * len` is one
            // there-and-back, and the second half is read as the reflection of
            // the first — so the turn happens at the ends of the audio rather
            // than at an arbitrary point, which is what makes it sound like a
            // tape reversing rather than a jump.
            let q = raw.rem_euclid(2.0 * lenf);
            if q < lenf {
                q
            } else {
                (2.0 * lenf - q).min(lenf - 1.0).max(0.0)
            }
        } else {
            raw.rem_euclid(lenf)
        }
    }

    /// Adopt a new speed and pendulum without moving the audio.
    ///
    /// Called only from the output callback, at a frame it knows exactly. The
    /// playhead is read under the old settings and `warp` is chosen so the new
    /// ones put it in the same place — after which everything downstream is
    /// arithmetic and nothing is stored about how it got there.
    fn adopt(&self, out_frame: i64, len: usize, speed: f64, pend: bool) {
        let here = self.play_pos(out_frame, len);
        self.speed.store(speed.to_bits(), Ordering::Relaxed);
        self.pendulum.store(pend, Ordering::Relaxed);
        if len == 0 {
            // Nothing to hold in place. An empty loop has no position to
            // preserve and its `origin` has not been stamped yet, so anchoring
            // against it would store a number about a frame that means nothing.
            self.warp.store(0.0f64.to_bits(), Ordering::Relaxed);
            return;
        }
        let origin = self.origin.load(Ordering::Acquire);
        let warp = here - (out_frame - origin) as f64 * speed;
        if speed == 1.0 && !pend {
            // Coming back to rate one, the offset is a whole-frame shift of
            // where position zero sits — so put it there and have done, rather
            // than carry it as a fraction for ever. That restores the exact
            // integer arithmetic (and with it the no-interpolation path), and
            // it makes `origin` tell the truth again: a loop that spent a while
            // at half speed really has drifted off the grid it closed on, and
            // this is where it says so.
            //
            // Rounding loses at most half a sample of position, once, at a
            // moment the player asked for a change anyway.
            self.origin
                .store(origin - warp.round() as i64, Ordering::Release);
            self.warp.store(0.0f64.to_bits(), Ordering::Relaxed);
        } else {
            self.warp.store(warp.to_bits(), Ordering::Relaxed);
        }
    }

    /// How many output frames one trip through this loop takes, at whatever
    /// speed and direction it is currently set to.
    ///
    /// Only a one-shot needs it — everything else wraps and never asks how long
    /// a pass was — but it is the arithmetic most likely to be quietly wrong, so
    /// it is a function with tests rather than three lines inside a callback.
    fn pass_frames(&self, len: usize) -> i64 {
        // A pendulum goes there and back before it has been round once.
        let span = if self.pendulum.load(Ordering::Relaxed) { 2 * len } else { len };
        // Direction does not change how long a pass takes, only which end it
        // starts at — so the rate is the magnitude.
        let rate = self.speed().abs().max(1e-6);
        (span as f64 / rate).round() as i64
    }

    /// Back to forward, rate one, no offset — what a cleared loop plays at.
    fn plainly(&self) {
        self.speed.store(1.0f64.to_bits(), Ordering::Relaxed);
        self.pendulum.store(false, Ordering::Relaxed);
        self.warp.store(0.0f64.to_bits(), Ordering::Relaxed);
        self.cfg_speed.store(1.0f64.to_bits(), Ordering::Relaxed);
        self.cfg_pend.store(false, Ordering::Relaxed);
        self.cfg_armed.store(false, Ordering::Relaxed);
    }

    /// Ask for a speed and pendulum. Applied by the callback, at its own frame.
    fn want(&self, speed: f64, pend: bool) {
        self.cfg_speed.store(speed.to_bits(), Ordering::Relaxed);
        self.cfg_pend.store(pend, Ordering::Relaxed);
        self.cfg_armed.store(true, Ordering::Release);
    }

    /// Left and right gain for this loop's pan setting, equal-power.
    ///
    /// At centre both are `1/sqrt(2)`, so a centred loop is the same loudness
    /// as a hard-panned one — which linear panning would not give, and which
    /// matters when six loops are being placed against each other.
    pub fn pan_gains(&self) -> (f32, f32) {
        let p = self.pan.load(Ordering::Relaxed).min(127) as f32 / 127.0;
        let theta = p * std::f32::consts::FRAC_PI_2;
        (theta.cos(), theta.sin())
    }

    pub fn state_name(&self) -> &'static str {
        match self.state.get() {
            ARMED => "armed",
            FIRST => "recordingFirst",
            OVERDUB => "overdubbing",
            MULTIPLY => "multiplying",
            PLAYING => "playing",
            _ => "idle",
        }
    }
    pub fn is_armed(&self) -> bool {
        self.state.get() == ARMED
    }
    pub fn is_recording(&self) -> bool {
        matches!(self.state.get(), FIRST | OVERDUB | MULTIPLY)
    }
    /// True when this loop wants the input — armed counts, because arming is a
    /// claim on the one converter the rig has.
    pub fn wants_input(&self) -> bool {
        self.is_armed() || self.is_recording()
    }
    pub fn quantised(&self) -> bool {
        self.quant.load(Ordering::Relaxed)
    }
    /// Whether a one-shot is inside a pass at this frame.
    ///
    /// Reported as well as mixed with, because the playhead does not stop
    /// between passes — it cannot, the arithmetic has no way to hold still —
    /// and a display reading `pos` alone shows a one-shot sweeping merrily
    /// along while it is silent. That is the same shape of lie the legend told
    /// about a bank nobody was standing on.
    pub fn firing(&self, out_frame: i64) -> bool {
        self.one_shot.load(Ordering::Relaxed) && out_frame < self.shot_end.load(Ordering::Acquire)
    }
    pub fn decay_of(&self) -> f32 {
        f32::from_bits(self.decay.load(Ordering::Relaxed))
    }
    /// What this layer is currently worth, after however many passes it has
    /// lived through. One for every layer of a loop that is not decaying.
    /// This layer's envelope, or empty when it has none yet.
    pub fn layer_env(&self, layer: usize) -> Vec<u8> {
        self.env
            .lock()
            .map(|e| e[layer].clone())
            .unwrap_or_default()
    }
    pub fn layer_gain(&self, layer: usize) -> f32 {
        f32::from_bits(self.l_gain[layer].load(Ordering::Relaxed))
    }
    /// Recompute every layer's decay gain for the buffer starting at
    /// `out_frame`. Called once a buffer from the output callback, which is the
    /// only thread that knows the frame.
    fn age(&self, out_frame: i64) {
        let d = self.decay_of();
        let now = self.pass_index(out_frame, self.loop_len.load(Ordering::Acquire));
        for l in 0..MAX_LAYERS {
            let g = if d >= 1.0 {
                1.0
            } else {
                // Clamped because nothing is louder than silence twice, and an
                // exponent from a loop that has been running all afternoon
                // should not be asked of `powi`.
                let age = (now - self.l_born[l].load(Ordering::Relaxed)).clamp(0, 4096);
                d.powi(age as i32)
            };
            self.l_gain[l].store(g.to_bits(), Ordering::Relaxed);
        }
    }
    pub fn chance_of(&self) -> f32 {
        f32::from_bits(self.chance.load(Ordering::Relaxed))
    }
    /// Whether chance has any say over this loop at the moment.
    ///
    /// One function because two things ask: the mixer, which rolls, and the
    /// snapshot, which reports. Written twice they would drift, and the way they
    /// would drift is the quiet one — the display saying a loop is sitting a
    /// pass out while it is audibly overdubbing.
    ///
    /// Never while recording. Overdubbing onto something you cannot hear is a
    /// way to record a mistake twice, which is the same argument that un-stops a
    /// loop before an overdub.
    fn chance_applies(&self) -> bool {
        self.chance_of() < 1.0 && !self.is_recording()
    }
    /// Whether chance is holding this pass back.
    ///
    /// **Reads the decision, never makes it.** The snapshot thread calls this
    /// thirty times a second; rolling here would consume randomness the mixer
    /// was going to use and, worse, would decide passes on whether anybody
    /// happened to be looking. The mixer owns the roll, this only reports it.
    pub fn skipping(&self, out_frame: i64, len: usize) -> bool {
        self.chance_applies()
            && self.chance_pass.load(Ordering::Relaxed) == self.pass_index(out_frame, len)
            && !self.chance_sounds.load(Ordering::Relaxed)
    }
    /// Frames until a scheduled transition fires, or `-1` when nothing is
    /// pending or it has no deadline.
    pub fn pending_in(&self, now: i64) -> i64 {
        if self.request.get() == 0 {
            return -1;
        }
        match self.request_at.load(Ordering::Acquire) {
            i64::MIN => -1,
            at => (at - now).max(0),
        }
    }
    /// How much continuation this layer holds past its end, for a crossfade.
    pub fn layer_tail(&self, layer: usize) -> usize {
        self.l_tail[layer].load(Ordering::Acquire)
    }
    pub fn layer_shape(&self, layer: usize) -> (usize, usize, usize) {
        (
            self.l_len[layer].load(Ordering::Relaxed),
            self.l_period[layer].load(Ordering::Relaxed).max(1),
            self.l_phase[layer].load(Ordering::Relaxed),
        )
    }
    /// One more layer playing, and the redo ceiling raised to match.
    ///
    /// Together, always: `redo_to` is how far back up the stack still holds
    /// audio, and every path that lands a layer — commit, a retroactive take,
    /// the end of a multiply — is a path where it has just moved. Beside each
    /// increment they would drift, and the failure would be a redo that raised
    /// a layer nobody recorded.
    fn add_layer(&self) {
        let n = self.n_layers.fetch_add(1, Ordering::AcqRel);
        self.redo_to.store(n + 1, Ordering::Release);
    }
    /// A freshly committed layer: its own length, sounding every time round.
    ///
    /// Written *before* `n_layers` is incremented everywhere it is used. The
    /// output callback plays `0..n_layers`, so publishing the layer first and
    /// its length second leaves a window in which the mix reads a length of
    /// zero and drops it — a buffer of silence at the exact moment a take
    /// lands, which is the least forgivable place for one.
    /// Declare what a layer is: its length, its continuation, and when it was
    /// born.
    ///
    /// **The tail is a parameter rather than something left alone**, because it
    /// is now read at playback and a stale one is audible. `take` and the
    /// multiply family write a layer without a continuation and used to leave
    /// whatever the slot held before; the samples there had been zeroed, so the
    /// wrap would have crossfaded into silence — a loop fading in from nothing
    /// every cycle, for a reason nothing on screen could explain.
    fn set_layer_shape(&self, layer: usize, s: Shape) {
        self.l_len[layer].store(s.len, Ordering::Release);
        self.l_period[layer].store(1, Ordering::Release);
        self.l_phase[layer].store(0, Ordering::Release);
        self.l_tail[layer].store(s.tail, Ordering::Release);
        self.l_born[layer].store(s.born, Ordering::Release);
        self.l_gain[layer].store(1.0f32.to_bits(), Ordering::Release);
    }
    /// Where in a layer's own buffer the loop position `pos` falls — or `None`
    /// when the layer is silent there.
    ///
    /// Called once per layer per frame in the output callback, so the dense case
    /// skips the division: a layer at `period = 1` sounds everywhere, and asking
    /// which slot it is in has no answer worth computing.
    fn layer_pos(&self, layer: usize, pos: usize) -> Option<usize> {
        let len = self.l_len[layer].load(Ordering::Relaxed);
        if len == 0 {
            return None;
        }
        let period = self.l_period[layer].load(Ordering::Relaxed).max(1);
        if period > 1 {
            let slot = (pos / len) % period;
            if slot != self.l_phase[layer].load(Ordering::Relaxed) % period {
                return None;
            }
        }
        Some(pos % len)
    }
}

pub struct Shared {
    arena: Vec<AtomicU32>,
    max_frames: usize,
    /// The pre-roll. The input callback writes every frame it ever receives here
    /// whether anything is recording or not, indexed by input frame modulo its
    /// length — so the last `ring_secs` of playing are always retrievable.
    ///
    /// This is the thing a pedal cannot do. Sixty seconds is 11 MB; a 720 has
    /// no such memory to spare and so must be told to record *before* the good
    /// bit happens. Here the good bit can be claimed afterwards.
    ///
    /// One ring for all six loops, because there is one input. Which loop a
    /// retroactive take lands in is a decision made when `t` is pressed, not
    /// something the capture has to anticipate.
    ring: Vec<AtomicU32>,
    ring_len: usize,
    pub loops: Vec<Loop>,
    /// Which loop bare commands address.
    ///
    /// A convenience for the console and for the app's single-loop view; the
    /// footswitch path does not rely on it, because every command accepts an
    /// explicit loop prefix (`3r`). Selection that only *some* callers depend on
    /// is a mode, and a mode that a footswitch could fall out of step with is
    /// the thing this design is trying not to have.
    pub selected: AtomicUsize,
    /// Which loop's cycle is the grid, or `N_LOOPS` for none yet. Set by the
    /// first loop to acquire a length; see `grid`.
    pub anchor: AtomicUsize,
    pub out_frames: AtomicUsize,
    in_frames: AtomicUsize,
    pub k: AtomicI64,
    pub k_set: AtomicBool,
    pub p0: Mutex<Option<cpal::StreamInstant>>,
    buffer_frames: AtomicU32,
    pub click: AtomicBool,
    preroll: AtomicUsize,
    /// The level a sound has to reach to start a level-armed recording, as an
    /// `f32` magnitude in the bits of a `u32`.
    ///
    /// Rig-wide rather than per loop, and settable while the daemon runs, because
    /// it is a fact about the room and the instrument rather than about any one
    /// loop — and because a threshold you cannot tune where you are standing is a
    /// threshold that will be wrong.
    pub arm_thresh: AtomicU32,
    /// `ARM_REACH_MS` in frames, resolved once at startup.
    arm_reach: AtomicUsize,
    /// `MAX_FADE_MS` in frames: the most continuation worth keeping past a
    /// layer's end, since nothing longer can ever be crossfaded into the wrap.
    max_fade: usize,
    pub monitor: AtomicBool,
    pub out_peak: AtomicU32,
    pub in_peak: AtomicU32,
    /// Latched by cpal's stream error callback. Unplugging the USB bus kills
    /// both streams, and until this existed the daemon carried on serving a
    /// confident socket from a dead engine: `r` set the request, no output
    /// callback ever consumed it, and the state sat at `idle` for ever. The
    /// only tell was two meters reading digital zero.
    /// Ask the output callback to stamp a fresh `p0`. Set at startup and again
    /// after every reopen — `p0` used to be taken only when `out_frames` was
    /// zero, which meant that after a recovery it could never be retaken, `K`
    /// could never be recomputed, and every subsequent recording silently wrote
    /// nothing at all.
    pub p0_needed: AtomicBool,
    /// The output frame `p0` was stamped at. Zero at startup, which is why the
    /// original arithmetic could get away without it; not zero after a reopen.
    pub p0_frame: AtomicUsize,
    pub device_lost: AtomicBool,
    /// How many times the device has been reopened. Worth surfacing rather
    /// than hiding — a rig that silently recovers six times in a session is
    /// telling you something about the cable.
    pub reopens: AtomicUsize,
    /// Where saved takes go.
    pub takes_dir: PathBuf,
    /// The last thing a command had to say, and a counter that moves whenever
    /// it changes.
    ///
    /// `dispatch` has always returned a sentence and the socket has always
    /// thrown it away — printing it to the daemon's stdout, where no app can
    /// see it. So a command either worked or did not and the display could not
    /// tell which, which is the same silence this project keeps finding.
    ///
    /// It rides in the snapshot rather than as its own message because the app
    /// keeps only the newest message it received: a separate ack would be
    /// overwritten within a frame, or worse, handed to a decoder expecting
    /// state. The sequence number is what lets a client tell a fresh ack from
    /// the same one still being shown — and if two commands land inside one
    /// tick the counter jumps by two, so the loss is visible instead of silent.
    pub ack: Mutex<String>,
    pub ack_seq: AtomicUsize,
    /// The newest `/link/anchor`, as sent: microseconds, beat, tempo, quantum.
    /// Doubles are held as bits because there is no `AtomicF64`.
    ///
    /// This is the only thing in the engine that knows what a bar is. Everything
    /// else measures cycles, which is why a looper alone cannot answer "one bar"
    /// and why quantisation waits on this rather than on a tap tempo.
    pub link_micros: AtomicI64,
    pub link_beat: AtomicU64,
    pub link_tempo: AtomicU64,
    pub link_quantum: AtomicU64,
    /// The output frame the newest anchor arrived on — the half of the
    /// wall-clock-to-frame join that can only be taken at the moment it lands.
    pub link_frame: AtomicUsize,
    /// How many anchors have been accepted, and how many were refused for
    /// having the wrong shape or an impossible value. A silent listener and an
    /// absent clock look identical from the app unless both are counted.
    pub link_anchors: AtomicUsize,
    pub link_rejected: AtomicUsize,
}

/// How many frames a bar lasts, or `None` when there is no usable tempo.
///
/// The whole of what tempo buys us on its own: a bar's *length*, which is
/// enough to round a recording to a whole number of bars. Where we are within
/// the bar is a different question and needs the frame counter tied to wall
/// clock — see `link.rs`.
pub fn bar_frames(tempo_bpm: f64, quantum: f64, sr: u32) -> Option<usize> {
    if !(tempo_bpm > 0.0) || !(quantum > 0.0) {
        return None;
    }
    let secs = 60.0 / tempo_bpm * quantum;
    let frames = (secs * sr as f64).round();
    if frames >= 1.0 { Some(frames as usize) } else { None }
}

/// `AtomicU8` under a name that makes the intent obvious at the use sites.
struct AtomicU8Wrapper(std::sync::atomic::AtomicU8);
impl AtomicU8Wrapper {
    fn new(v: u8) -> Self {
        AtomicU8Wrapper(std::sync::atomic::AtomicU8::new(v))
    }
    fn get(&self) -> u8 {
        self.0.load(Ordering::Acquire)
    }
    fn set(&self, v: u8) {
        self.0.store(v, Ordering::Release)
    }
    fn take(&self) -> u8 {
        self.0.swap(0, Ordering::AcqRel)
    }
}

impl Shared {
    /// One loop, by index, clamped rather than panicking: an out-of-range index
    /// can only come from a command string, and a bad command should be refused
    /// where commands are parsed, not by killing the audio thread.
    pub fn lp(&self, li: usize) -> &Loop {
        &self.loops[li.min(N_LOOPS - 1)]
    }
    pub fn sel(&self) -> usize {
        self.selected.load(Ordering::Relaxed).min(N_LOOPS - 1)
    }
    /// Which loop currently owns the input, if any.
    ///
    /// There is one converter, so at most one loop can be recording. Rather than
    /// keep a separate "who is recording" field that could disagree with the
    /// loops' own states, the input callback asks. Six relaxed loads per buffer
    /// is nothing, and a derived answer cannot go stale.
    pub fn recording_loop(&self) -> Option<usize> {
        (0..N_LOOPS).find(|&i| self.loops[i].is_recording())
    }
    /// Whether any loop is claiming the input, including one merely armed.
    pub fn input_claimed(&self) -> Option<usize> {
        (0..N_LOOPS).find(|&i| self.loops[i].wants_input())
    }
    /// The loop waiting for a sound, if one is. Asked by the input callback on
    /// every buffer, and derived for the same reason `recording_loop` is.
    ///
    /// A loop whose crossing has already been found still reads `ARMED` — the
    /// state does not change until the output callback stamps the transition,
    /// which may be a buffer or two later. Excluding it here is what stops the
    /// next buffer finding a second crossing and back-dating the recording to
    /// *that* one instead.
    pub fn armed_loop(&self) -> Option<usize> {
        (0..N_LOOPS).find(|&i| self.loops[i].is_armed() && self.loops[i].request.get() == 0)
    }

    /// The grid quantised loops align to: the anchor's origin and cycle length.
    ///
    /// The anchor is the first loop to acquire a length, which is how a looper
    /// has always worked — the thing you played first is the thing everything
    /// else fits around. It is *derived* on every call rather than cached,
    /// because a cached anchor survives the loop being cleared and would hand
    /// out a grid belonging to audio that no longer exists.
    ///
    /// Deliberately not Link. Tempo alone gives a bar's *length* but not where
    /// the bar falls, and aligning to a boundary needs both — so until the
    /// frame-to-wall-clock join lands, the grid the engine can honestly offer
    /// is another loop's cycle. That is also the grid that matters most here:
    /// six loops agreeing with each other is the point, and agreeing with
    /// Ableton is a bonus.
    pub fn grid(&self) -> Option<(i64, usize)> {
        let a = self.anchor.load(Ordering::Acquire);
        if a >= N_LOOPS {
            return None;
        }
        let lp = self.lp(a);
        let len = lp.loop_len.load(Ordering::Acquire);
        if len == 0 {
            return None;
        }
        Some((lp.origin.load(Ordering::Acquire), len))
    }

    /// The first output frame at or after `from` that lands on the grid.
    pub fn next_boundary(&self, from: i64) -> Option<i64> {
        let (origin, len) = self.grid()?;
        let elapsed = from - origin;
        let cycles = elapsed.div_euclid(len as i64) + if elapsed.rem_euclid(len as i64) == 0 { 0 } else { 1 };
        Some(origin + cycles * len as i64)
    }

    /// Remember which loop laid down the grid, the first time one does.
    fn claim_anchor(&self, li: usize) {
        let _ = self.anchor.compare_exchange(
            N_LOOPS,
            li,
            Ordering::AcqRel,
            Ordering::Relaxed,
        );
    }

    /// Give up the grid when the loop that set it loses its length.
    ///
    /// `grid` already refuses to serve a boundary from an empty anchor, so the
    /// audio is safe without this — but the index would stay pointed at the
    /// cleared loop and `claim_anchor` only succeeds from "none", so the next
    /// loop recorded could never become the grid. The rig would quietly have no
    /// grid for the rest of the session.
    fn release_anchor(&self, li: usize) {
        let _ = self.anchor.compare_exchange(
            li,
            N_LOOPS,
            Ordering::AcqRel,
            Ordering::Relaxed,
        );
    }

    /// Record what a command had to say, for the snapshot to carry.
    pub fn note_ack(&self, msg: &str) {
        if let Ok(mut g) = self.ack.lock() {
            *g = msg.to_string();
        }
        self.ack_seq.fetch_add(1, Ordering::Release);
    }

    /// One sample of one layer of one loop.
    ///
    /// The arena stays a single allocation with the loop as the outermost index,
    /// rather than six Vecs: it keeps the "allocated once, never touched by the
    /// allocator again" property that lets the callbacks be allocation-free, and
    /// a loop's layers stay contiguous, which is the order the mix walks them in.
    fn cell(&self, li: usize, layer: usize, pos: usize) -> &AtomicU32 {
        &self.arena[(li * MAX_LAYERS + layer) * self.max_frames + pos]
    }
    fn read(&self, li: usize, layer: usize, pos: usize) -> f32 {
        f32::from_bits(self.cell(li, layer, pos).load(Ordering::Relaxed))
    }
    fn write(&self, li: usize, layer: usize, pos: usize, v: f32) {
        self.cell(li, layer, pos).store(v.to_bits(), Ordering::Relaxed)
    }
    fn add(&self, li: usize, layer: usize, pos: usize, v: f32) {
        let c = self.cell(li, layer, pos);
        let cur = f32::from_bits(c.load(Ordering::Relaxed));
        c.store((cur + v).to_bits(), Ordering::Relaxed)
    }
    /// The captured sample for an input frame, if the ring still holds it.
    fn ring_at(&self, in_frame: i64) -> Option<f32> {
        if in_frame < 0 {
            return None;
        }
        let newest = self.in_frames.load(Ordering::Acquire) as i64;
        // Leave a buffer's grace at the trailing edge: the input callback is
        // still writing, and a frame about to be overwritten is not a frame.
        let oldest = newest - self.ring_len as i64 + self.buffer_frames.load(Ordering::Relaxed) as i64;
        if in_frame < oldest || in_frame >= newest {
            return None;
        }
        let i = (in_frame as usize) % self.ring_len;
        Some(f32::from_bits(self.ring[i].load(Ordering::Relaxed)))
    }


    /// What the mix takes from a layer at a loop position: the sample, or zero
    /// where the layer is silent.
    ///
    /// The output callback and the self-test both go through here on purpose.
    /// The test used to read the arena directly, which made it an assertion about
    /// *storage* — and it duly failed the moment repetition stopped being a copy
    /// and became a calculation, while the audio was correct. A test that can
    /// disagree with the audio path about what is audible is testing the wrong
    /// thing.
    /// One layer's contribution at one loop position, with the wrap made
    /// continuous if it has been asked for.
    ///
    /// ## What is actually wrong at a loop point
    ///
    /// A first recording is written linearly and then cut: frame `len - 1` is
    /// followed at playback by frame `0`, but the frame that *truly* followed it
    /// when it was played is the first of the continuation. So the join is a
    /// step in the waveform — a click — and whatever was sustaining is chopped.
    ///
    /// The fix is to arrive at the head through the continuation. Over the first
    /// `n` frames of the layer, fade from the tail into the head: at `p = 0` you
    /// hear almost exactly what followed `len - 1`, and by `p = n` you are back
    /// on the recording. Continuous by construction, because the two are the
    /// same performance either side of the same instant.
    ///
    /// **Linear, and deliberately.** Equal-power is for crossfading *unrelated*
    /// sources; these two are one performance a cycle apart and are correlated
    /// at the join, where a linear pair sums to unity and equal-power would add
    /// three decibels. Where they are uncorrelated — a different drum hit at
    /// each end — linear dips, but only by a few decibels over a few
    /// milliseconds, which is the cheaper failure.
    ///
    /// ## Only a layer that was cut needs it
    ///
    /// An **overdub** is recorded modularly, into `pos % len`, so the sample at
    /// position zero genuinely is the one that followed position `len - 1` — it
    /// was played that way. Nothing to fix. Its tail exists for a different
    /// reason (unwrapping the frames recorded after the press), and using it
    /// here costs nothing and does no harm.
    ///
    /// A **tiled** layer is skipped outright. Its blocks are separated by
    /// silence, so there is no step to smooth, and blending the continuation in
    /// there would insert audio at a moment nothing was playing.
    fn sample_at(&self, li: usize, layer: usize, pos: usize) -> f32 {
        let lp = self.lp(li);
        let Some(p) = lp.layer_pos(layer, pos) else {
            return 0.0;
        };
        let v = self.read(li, layer, p);
        let xf = lp.fade.load(Ordering::Relaxed);
        // The ordinary case, and the first test is the cheap one: away from a
        // wrap this is the single read it has always been.
        if xf == 0 || p >= xf || lp.l_period[layer].load(Ordering::Relaxed) > 1 {
            return v;
        }
        let len = lp.l_len[layer].load(Ordering::Relaxed);
        // Bounded by what the layer actually kept, and by where its slice of the
        // arena ends — reading past that would read the next layer's audio,
        // which is silent corruption rather than an error.
        let n = xf
            .min(lp.l_tail[layer].load(Ordering::Acquire))
            .min(self.max_frames.saturating_sub(len));
        if p >= n {
            return v;
        }
        wrap_mix(v, self.read(li, layer, len + p), p, n)
    }

    fn zero_layer(&self, li: usize, layer: usize) {
        for i in 0..self.max_frames {
            self.cell(li, layer, i).store(0, Ordering::Relaxed);
        }
    }

    /// Redraw a layer's envelope from what is actually in the arena.
    ///
    /// Called from the control thread whenever a layer's *content* changes —
    /// a shorter list than it looks: recording, claiming and multiplying. Undo
    /// and redo move a layer count; sparse and rotate move period and phase;
    /// speed, pan, decay and the wrap fade are resolutions applied at playback.
    /// None of them touch a sample, which is exactly why a picture of the stored
    /// audio can be cached and still be true.
    ///
    /// Linear in the layer's length — about a millisecond for a thirty-second
    /// take — which is why it is cached rather than computed per snapshot.
    fn rebuild_env(&self, li: usize, layer: usize) {
        let lp = self.lp(li);
        let len = lp.l_len[layer].load(Ordering::Acquire);
        let mut out = Vec::new();
        if len > 0 {
            out.reserve(ENV_BUCKETS);
            for b in 0..ENV_BUCKETS {
                let from = b * len / ENV_BUCKETS;
                let to = (((b + 1) * len) / ENV_BUCKETS).max(from + 1).min(len);
                let mut peak = 0.0f32;
                for p in from..to {
                    peak = peak.max(self.read(li, layer, p).abs());
                }
                out.push(to_byte(peak));
            }
        }
        if let Ok(mut e) = lp.env.lock() {
            e[layer] = out;
        }
    }

    /// Forget every envelope on a loop, for when its audio goes.
    fn clear_env(&self, li: usize) {
        if let Ok(mut e) = self.lp(li).env.lock() {
            for v in e.iter_mut() {
                v.clear();
            }
        }
    }

    /// Everything one loop contributes to the mix at one output frame.
    ///
    /// Pulled out of the callback because six loops made it a nested loop worth
    /// naming, and because the self-test now has to be able to ask the same
    /// question of a specific loop.
    fn loop_at(&self, li: usize, out_frame: i64, rng: &mut SmallRng) -> f32 {
        let lp = self.lp(li);
        let len = lp.loop_len.load(Ordering::Acquire);
        if len == 0 {
            return 0.0;
        }
        // Silenced but not stopped: `pos` below is still computed from `origin`
        // on every frame, so nothing drifts while a loop is quiet.
        if lp.muted.load(Ordering::Relaxed) {
            return 0.0;
        }
        // A one-shot sounds only inside a pass. Before the first fire `shot_end`
        // is `i64::MIN`, so turning the mode on silences the loop at once — which
        // is right, and is why the ack says so: a one-shot that kept playing
        // until its next fire would be a loop in two minds.
        if lp.one_shot.load(Ordering::Relaxed) && !lp.firing(out_frame) {
            return 0.0;
        }
        let n = lp.n_layers.load(Ordering::Acquire);
        if n == 0 {
            return 0.0;
        }
        // Chance: one roll per pass, held for the whole pass.
        //
        // The roll has to happen here, because this is the only place that knows
        // the frame and so the only place that can turn a loop on and off *at* a
        // cycle boundary rather than within a buffer of one. Remembering which
        // pass it was for is what keeps a one-in-four loop from flickering at
        // sample rate.
        if lp.chance_applies() {
            let p = lp.chance_of();
            let pass = lp.pass_index(out_frame, len);
            if lp.chance_pass.load(Ordering::Relaxed) != pass {
                lp.chance_pass.store(pass, Ordering::Relaxed);
                lp.chance_sounds.store(rng.gen::<f32>() < p, Ordering::Relaxed);
            }
            if !lp.chance_sounds.load(Ordering::Relaxed) {
                return 0.0;
            }
        }
        // Speed is applied to the *loop's* position rather than to each layer's,
        // so the layers keep their places relative to one another and the whole
        // cycle turns over together — which is what playing a loop at a speed
        // means, and not the same as playing every layer at one.
        let pf = lp.play_pos(out_frame, len);
        let p0 = (pf as usize).min(len - 1);
        let frac = pf - p0 as f64;
        // At rate one going forwards the fraction is exactly zero — the
        // arithmetic is `warp + (frame - origin) * 1.0` on integers — so the
        // ordinary case reads one sample per layer, as it always did, and the
        // second read is bought only by the loops that asked for it.
        if frac == 0.0 {
            return self.mix_at(li, n, p0);
        }
        let p1 = (p0 + 1) % len;
        let f = frac as f32;
        self.mix_at(li, n, p0) * (1.0 - f) + self.mix_at(li, n, p1) * f
    }

    /// Every layer of one loop, summed at one integer loop position.
    ///
    /// Split out because interpolation needs the same question asked at two
    /// neighbouring positions, and summing the layers first is the same number
    /// as interpolating each layer and summing after — for half the reads.
    fn mix_at(&self, li: usize, n: usize, pos: usize) -> f32 {
        let lp = self.lp(li);
        let mut v = 0.0f32;
        for l in 0..n {
            let g = lp.layer_gain(l);
            // Eighty decibels down is not quiet, it is gone — and skipping it
            // saves the arena read and the wrap fade's second read with it. The
            // audio is still there; only the reading of it stops.
            if g > 1.0e-4 {
                v += self.sample_at(li, l, pos) * g;
            }
        }
        v
    }
}

/// The stream error callback, latching device loss so the supervisor can act.
///
/// One per stream because cpal takes ownership of each.
fn err_cb(sh: Arc<Shared>) -> impl FnMut(cpal::StreamError) + Send + 'static {
    move |e| {
        eprintln!("stream error: {}", e);
        sh.device_lost.store(true, Ordering::Release);
    }
}

/// The residual in force, and where it came from.
///
/// The second half is not decoration. The residual is a *measurement*, it moves
/// when another client opens the device, and the failure mode is that nobody
/// notices — so the engine says which of the three sources it used every time it
/// starts, and admits when it is guessing.
pub(crate) struct Residual {
    pub samples: f64,
    pub source: String,
    /// What had the device open when the number was measured, if it is stored.
    /// Kept so the operator can compare it with what is running now; the
    /// comparison is `deepstar latency check`'s job, not the audio daemon's.
    pub clients: Option<String>,
}

/// Where DeepStar leaves the calibration it curates.
///
/// The canonical artefact is in Amphora, content-addressed, alongside the VCO
/// tables — this is its projection onto the filesystem, so the audio daemon
/// needs no HTTP client and starts with no dependency on a store being up. Same
/// division as everywhere else in the rig: the store holds the truth, and what
/// the realiser reads is compiled output.
///
/// Deliberately not JSON. It is a handful of scalars that a person reads exactly
/// once — at the moment they suspect it — and `residual_samples = 275` is more
/// use then than a brace.
pub(crate) fn calibration_path() -> Option<PathBuf> {
    std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".itajara").join("calibration.conf"))
}

pub(crate) fn resolve_residual(default: f64, given: bool, device: &str) -> Residual {
    // Given explicitly: the operator has measured for the configuration in
    // force and knows better than anything stored.
    if given {
        return Residual {
            samples: default,
            source: "--residual".into(),
            clients: None,
        };
    }
    if let Some(path) = calibration_path() {
        if let Ok(text) = std::fs::read_to_string(&path) {
            let mut fields = std::collections::HashMap::new();
            for line in text.lines() {
                let line = line.trim();
                if line.starts_with('#') || line.is_empty() {
                    continue;
                }
                if let Some((k, v)) = line.split_once('=') {
                    fields.insert(k.trim().to_string(), v.trim().to_string());
                }
            }
            // Keyed by device, because the residual is a property of the
            // interface and this rig has more than one. A calibration for
            // something else is not a calibration for this.
            let stored_device = fields.get("device").cloned().unwrap_or_default();
            let matches = stored_device.is_empty()
                || device.to_lowercase().contains(&stored_device.to_lowercase());
            if let (true, Some(v)) = (matches, fields.get("residual_samples")) {
                if let Ok(n) = v.parse::<f64>() {
                    return Residual {
                        samples: n,
                        source: format!(
                            "{} (measured {})",
                            path.display(),
                            fields
                                .get("measured_at")
                                .cloned()
                                .unwrap_or_else(|| "at an unrecorded time".into())
                        ),
                        clients: fields.get("clients").cloned(),
                    };
                }
            }
            if !matches {
                eprintln!(
                    "  calibration at {} is for {:?}, not {:?} — ignoring it.",
                    path.display(),
                    stored_device,
                    device
                );
            }
        }
    }
    Residual {
        samples: default,
        source: "the compiled default, which is an assumption".into(),
        clients: None,
    }
}

pub fn run(opts: Opts) -> Result<(), Box<dyn Error>> {
    let candidate = crate::devices::find(&opts.device)?;
    let device = candidate.device;

    // Said out loud at every start, because the whole failure mode here is a
    // number that quietly stopped being true. On 2026-08-19 the default was 23
    // samples short and nothing in the sound said so.
    let residual = resolve_residual(opts.residual, opts.residual_given, &candidate.name);
    println!(
        "Residual {:.0} samples, from {}.",
        residual.samples, residual.source
    );
    if let Some(clients) = &residual.clients {
        println!(
            "  measured with these also on the device: {}. \
             `deepstar latency check` compares that with what is running now.",
            clients
        );
    }

    let mut in_cfg = choose_input(&device, opts.in_ch, opts.sample_rate, Width::Widest)
        .ok_or_else(|| format!("{} has no f32 input config", candidate.name))?;
    let mut out_cfg = choose_output(&device, opts.out_ch, opts.sample_rate, Width::Narrowest)
        .ok_or_else(|| format!("{} has no f32 output config", candidate.name))?;
    if let Some(n) = opts.buffer {
        in_cfg.buffer_size = cpal::BufferSize::Fixed(n);
        out_cfg.buffer_size = cpal::BufferSize::Fixed(n);
    }

    let sr = in_cfg.sample_rate.0;
    let sr_f = sr as f64;
    let in_channels = in_cfg.channels as usize;
    let out_channels = out_cfg.channels as usize;
    let max_frames = (opts.max_secs * sr_f).round() as usize;
    let ring_len = (opts.ring_secs * sr_f).round() as usize;

    println!("Device: {}", candidate.name);
    println!(
        "Recording input {}, playing output {}, at {} Hz.",
        opts.in_ch, opts.out_ch, sr
    );
    println!(
        "Arena: {} loops x {} layers x {:.0} s = {} MB.   Pre-roll: {:.0} s = {} MB.\n",
        N_LOOPS,
        MAX_LAYERS,
        opts.max_secs,
        N_LOOPS * MAX_LAYERS * max_frames * 4 / 1_048_576,
        opts.ring_secs,
        ring_len * 4 / 1_048_576
    );

    let sh = Arc::new(Shared {
        arena: (0..N_LOOPS * MAX_LAYERS * max_frames)
            .map(|_| AtomicU32::new(0))
            .collect(),
        max_frames,
        ring: (0..ring_len).map(|_| AtomicU32::new(0)).collect(),
        ring_len,
        loops: (0..N_LOOPS).map(|_| Loop::new()).collect(),
        selected: AtomicUsize::new(0),
        anchor: AtomicUsize::new(N_LOOPS),
        out_frames: AtomicUsize::new(0),
        in_frames: AtomicUsize::new(0),
        k: AtomicI64::new(0),
        k_set: AtomicBool::new(false),
        p0: Mutex::new(None),
        buffer_frames: AtomicU32::new(0),
        click: AtomicBool::new(opts.click || opts.selftest.is_some()),
        preroll: AtomicUsize::new(
            (opts.preroll_ms / 1000.0 * sr_f).round().max(0.0) as usize,
        ),
        arm_thresh: AtomicU32::new(db_to_mag(opts.arm_db).to_bits()),
        arm_reach: AtomicUsize::new((ARM_REACH_MS / 1000.0 * sr_f).round() as usize),
        max_fade: (MAX_FADE_MS / 1000.0 * sr_f).round() as usize,
        monitor: AtomicBool::new(opts.monitor),
        out_peak: AtomicU32::new(0),
        in_peak: AtomicU32::new(0),
        p0_needed: AtomicBool::new(true),
        p0_frame: AtomicUsize::new(0),
        device_lost: AtomicBool::new(false),
        reopens: AtomicUsize::new(0),
        takes_dir: opts.takes_dir.clone(),
        ack: Mutex::new(String::new()),
        ack_seq: AtomicUsize::new(0),
        link_micros: AtomicI64::new(0),
        link_beat: AtomicU64::new(0),
        link_tempo: AtomicU64::new(0),
        link_quantum: AtomicU64::new(0),
        link_frame: AtomicUsize::new(0),
        link_anchors: AtomicUsize::new(0),
        link_rejected: AtomicUsize::new(0),
    });

    // Both streams are rebuilt on recovery, so building them lives in a closure
    // rather than inline. Everything it captures is either `Arc` or `Copy`.
    let build_streams = |device: &cpal::Device|
     -> Result<(cpal::Stream, cpal::Stream), Box<dyn Error>> {

    let out_stream = {
        // Cloned before the shadowing below moves `sh` into the callback.
        let err_sh = sh.clone();
        let sh = sh.clone();
        let ch = opts.out_ch;
        let dual = opts.dual;
        // Seeded here rather than inside: `from_entropy` asks the operating
        // system, which is exactly the thing the callback may not do — and this
        // runs on the control thread, at stream build, where it costs nothing.
        // A fixed seed would make every session drop the same cycles, which is
        // the opposite of what anybody switches chance on for.
        let mut rng = SmallRng::from_entropy();
        device.build_output_stream(
            &out_cfg,
            move |data: &mut [f32], info: &cpal::OutputCallbackInfo| {
                // Chance's generator, owned outright by the thread that rolls
                // it. No atomic and no sharing, because there is no sharing: the
                // mixer is the only thing that rolls, and it runs here.
                //
                // `SmallRng` is xoshiro256++ — pure arithmetic over its own
                // state, so it is as safe here as the `cos` next door. What must
                // never appear in a callback is `thread_rng()`, which reseeds
                // from the operating system every 64 KiB and so hides a
                // `getrandom` syscall at a moment nobody chose.
                for s in data.iter_mut() {
                    *s = 0.0;
                }
                let frames = data.len() / out_channels;
                sh.buffer_frames.store(frames as u32, Ordering::Relaxed);

                let base = sh.out_frames.load(Ordering::Acquire);
                if sh.p0_needed.load(Ordering::Relaxed) {
                    // `try_lock` because this is the audio thread; if the lock
                    // is contended the next buffer will do just as well.
                    if let Ok(mut g) = sh.p0.try_lock() {
                        *g = Some(info.timestamp().playback);
                        sh.p0_frame.store(base, Ordering::Release);
                        sh.p0_needed.store(false, Ordering::Release);
                    }
                }

                // Transitions are stamped here because this is the only thread
                // that knows the exact frame, and a loop boundary a buffer out
                // is a loop boundary that is audibly wrong.
                // Every loop's pending transition, stamped to this frame. Six
                // `take`s a buffer, and each is a swap on an uncontended atomic.
                for li in 0..N_LOOPS {
                    let lp = sh.lp(li);
                    // Speed first, and at the buffer start, because adopting it
                    // reads the playhead and everything below may move `origin`.
                    if lp.cfg_armed.swap(false, Ordering::Acquire) {
                        lp.adopt(
                            base as i64,
                            lp.loop_len.load(Ordering::Acquire),
                            f64::from_bits(lp.cfg_speed.load(Ordering::Relaxed)),
                            lp.cfg_pend.load(Ordering::Relaxed),
                        );
                    }
                    // Decay, at the buffer start and for the same reason: it
                    // only changes at a pass boundary, so a `powi` per layer per
                    // buffer is free where per frame would not be.
                    lp.age(base as i64);
                    // Peek, not take: a request with a deadline in the future
                    // has to survive this buffer and be reconsidered on the
                    // next. Consuming first and re-arming would lose it if the
                    // control thread never looked again.
                    let pending = lp.request.get();
                    if pending == 0 {
                        continue;
                    }
                    let at = lp.request_at.load(Ordering::Acquire);
                    // Due if it has no deadline, or its deadline falls inside
                    // this buffer, or has already gone by — a deadline in the
                    // past means the control thread was late, and being late is
                    // not a reason to wait a whole cycle more.
                    if at != i64::MIN && at >= (base + frames) as i64 {
                        continue;
                    }
                    // The frame the transition belongs to. `origin` and
                    // `rec_from` are stamped with this rather than with the
                    // buffer start, which is what makes the alignment exact:
                    // the flag flips at buffer granularity, but everything
                    // downstream reads the frame.
                    let stamp = if at == i64::MIN { base as i64 } else { at.max(base as i64) };
                    lp.request.set(0);
                    lp.request_at.store(i64::MIN, Ordering::Release);
                    match pending {
                        ARMED => {
                            lp.reached.store(0, Ordering::Release);
                            lp.rec_reached.store(0, Ordering::Release);
                            // A level-armed recording knows the frame the sound
                            // crossed the threshold, and that frame is earlier
                            // than the one this request can be stamped at — the
                            // crossing is found on the input thread. Hand the
                            // difference to `started_late`, which is the same
                            // road a late footswitch already travels: `commit`
                            // shifts `origin` back by it and fills the front
                            // from the ring.
                            match lp.arm_from.swap(i64::MIN, Ordering::AcqRel) {
                                i64::MIN => {}
                                want => lp
                                    .started_late
                                    .store((stamp - want).max(0), Ordering::Release),
                            }
                            let n = lp.n_layers.load(Ordering::Acquire);
                            if n < MAX_LAYERS {
                                if lp.loop_len.load(Ordering::Acquire) == 0 {
                                    // Only the first recording lays down the grid.
                                    // Re-stamping origin on every arm would drag the
                                    // whole loop to position zero the instant you
                                    // hit record — playback reads origin too. The
                                    // self-test cannot catch that, because both
                                    // sides move together.
                                    lp.origin.store(stamp, Ordering::Release);
                                    lp.rec_from.store(stamp, Ordering::Release);
                                    lp.state.set(FIRST);
                                } else {
                                    // An overdub is modular against the existing
                                    // grid, so it records from the same reference
                                    // the loop plays from.
                                    lp.rec_from
                                        .store(lp.origin.load(Ordering::Acquire), Ordering::Release);
                                    lp.state.set(OVERDUB);
                                }
                            }
                        }
                        PLAYING => lp.state.set(PLAYING),
                        // **The one place `origin` moves.**
                        //
                        // Everywhere else in this engine a loop's zero is fixed
                        // at the moment it was recorded and stays there. That is
                        // what phase-locking means and it is why stopping a loop
                        // and starting it again puts it back where it would have
                        // been rather than where it began — the alternative,
                        // moving `origin`, is called out on `muted` as "the one
                        // thing that must never happen to a loop that closed on a
                        // grid boundary".
                        //
                        // A one-shot is the documented exception, and has to be:
                        // the entire gesture is *play this, from the top, now*.
                        // Which is also why the mode is a mode — a loop that can
                        // be fired is a loop that has given up its place in the
                        // phase-locked set, and that should be a thing you turn
                        // on rather than a thing a footswitch does to you.
                        FIRE => {
                            let len = lp.loop_len.load(Ordering::Acquire);
                            if len > 0 {
                                lp.origin.store(stamp, Ordering::Release);
                                // Backwards, the top of the pass is the *end*.
                                // Starting at zero and stepping negative wraps
                                // there anyway, one sample later and audibly.
                                let from = if lp.speed() < 0.0 { (len - 1) as f64 } else { 0.0 };
                                lp.warp.store(from.to_bits(), Ordering::Relaxed);
                                lp.shot_end
                                    .store(stamp + lp.pass_frames(len), Ordering::Release);
                                // A fired loop is audible by definition. Leaving
                                // `muted` set would make the switch do nothing
                                // for a reason nothing on screen could explain.
                                lp.muted.store(false, Ordering::Relaxed);
                            }
                        }
                        IDLE => {}
                        _ => {}
                    }
                }

                // The click follows the SELECTED loop, not loop 0 and not a
                // rig-wide grid. With six independent cycles there is no one
                // right answer, and "the loop you are working on" is the only
                // one that stays predictable as loops come and go. When bar
                // quantisation lands, the click should follow Link instead —
                // that will be a grid rather than a guess.
                let click_li = sh.sel();
                let click_len = sh.lp(click_li).loop_len.load(Ordering::Acquire);
                let click_origin = sh.lp(click_li).origin.load(Ordering::Acquire);

                // Monitoring reads the freshest frames the pre-roll holds. One
                // buffer behind the converters, so the interface's own direct
                // monitoring beats it — this is for headphones with nothing
                // else in the room.
                let monitor = sh.monitor.load(Ordering::Relaxed);
                let mon_from = sh.in_frames.load(Ordering::Acquire) as i64 - frames as i64;

                let mut peak = 0.0f32;
                // Once per buffer, not once per frame: six loops times two
                // trig calls is free here and wasteful inside the frame loop.
                let mut gains = [(0.0f32, 0.0f32); N_LOOPS];
                for li in 0..N_LOOPS {
                    gains[li] = sh.lp(li).pan_gains();
                }

                for f in 0..frames {
                    let out_frame = (base + f) as i64;
                    let mut vl = 0.0f32;
                    let mut vr = 0.0f32;

                    for li in 0..N_LOOPS {
                        let s = sh.loop_at(li, out_frame, &mut rng);
                        vl += s * gains[li].0;
                        vr += s * gains[li].1;
                    }
                    // The click and the input monitor sit in the middle. They
                    // are references, not material, and a reference that moves
                    // is not one.
                    let mut v = 0.0f32;
                    if click_len > 0 && sh.click.load(Ordering::Relaxed) {
                        let pos = (out_frame - click_origin).rem_euclid(click_len as i64) as usize;
                        if pos < 16 {
                            v += 0.4;
                        }
                    }
                    if monitor {
                        if let Some(m) = sh.ring_at(mon_from + f as i64) {
                            v += m;
                        }
                    }

                    vl += v;
                    vr += v;
                    peak = peak.max(vl.abs()).max(vr.abs());
                    data[f * out_channels + ch] = vl;
                    if dual && ch + 1 < out_channels {
                        data[f * out_channels + ch + 1] = vr;
                    }
                }
                sh.out_peak.fetch_max(peak.to_bits(), Ordering::Relaxed);
                sh.out_frames.store(base + frames, Ordering::Release);
            },
            err_cb(err_sh),
            None,
        )?
    };

    let in_stream = {
        // Cloned before the shadowing below moves `sh` into the callback.
        let err_sh = sh.clone();
        let sh = sh.clone();
        let ch = opts.in_ch;
        let residual = residual.samples;
        device.build_input_stream(
            &in_cfg,
            move |data: &[f32], info: &cpal::InputCallbackInfo| {
                let frames = data.len() / in_channels;
                let base = sh.in_frames.load(Ordering::Acquire);

                if !sh.k_set.load(Ordering::Acquire) {
                    // The one consultation of the host clock in the whole engine.
                    let Ok(g) = sh.p0.try_lock() else {
                        sh.in_frames.store(base + frames, Ordering::Release);
                        return;
                    };
                    let Some(p0) = g.as_ref() else {
                        sh.in_frames.store(base + frames, Ordering::Release);
                        return;
                    };
                    let buffer = sh.buffer_frames.load(Ordering::Relaxed) as f64;
                    let offset = residual - 2.0 * buffer;
                    let c0 = signed_secs(p0, &info.timestamp().capture) * sr_f;
                    // `p0_frame` is zero at startup, so this is the same
                    // arithmetic as before for the case that always worked.
                    let p0_frame = sh.p0_frame.load(Ordering::Acquire) as f64;
                    sh.k.store(
                        (p0_frame + c0 - base as f64 - offset).round() as i64,
                        Ordering::Release,
                    );
                    sh.k_set.store(true, Ordering::Release);
                }

                // Always, regardless of transport state. This is what makes
                // the past claimable.
                let mut peak = 0.0f32;
                for f in 0..frames {
                    let v = data[f * in_channels + ch];
                    peak = peak.max(v.abs());
                    let i = (base + f) % sh.ring_len;
                    sh.ring[i].store(v.to_bits(), Ordering::Relaxed);
                }
                sh.in_peak.fetch_max(peak.to_bits(), Ordering::Relaxed);

                // A level-armed loop is *listening*, not recording — it is not
                // `recording_loop()` and nothing below will write for it. What it
                // needs is the frame the sound crossed the threshold, found here
                // because this is the only place that sees individual input
                // frames. Per-buffer would do at 21 ms granularity, but the
                // frames are already in hand and the crossing is the one number
                // the whole mode turns on.
                //
                // The crossing is not the start of the note, so the recording is
                // dated `ARM_REACH_MS` before it. That costs nothing: the ring
                // has been running since the daemon started.
                if let Some(li) = sh.armed_loop() {
                    let thresh = f32::from_bits(sh.arm_thresh.load(Ordering::Relaxed));
                    if peak >= thresh {
                        if let Some(f) = (0..frames)
                            .find(|&f| data[f * in_channels + ch].abs() >= thresh)
                        {
                            let lp = sh.lp(li);
                            let k = sh.k.load(Ordering::Acquire);
                            let at = (base + f) as i64 + k;
                            // Quantised wins, as it does for a footswitch: a loop
                            // told to start on the grid starts on the grid,
                            // whatever asked for it. There is no back-dating then
                            // — the boundary is ahead, not behind.
                            match if lp.quant.load(Ordering::Relaxed) {
                                sh.next_boundary(at)
                            } else {
                                None
                            } {
                                Some(t) => {
                                    lp.arm_from.store(i64::MIN, Ordering::Release);
                                    lp.request_at.store(t, Ordering::Release);
                                }
                                None => {
                                    let reach = sh.arm_reach.load(Ordering::Relaxed) as i64;
                                    lp.arm_from.store(at - reach, Ordering::Release);
                                    lp.request_at.store(i64::MIN, Ordering::Release);
                                }
                            }
                            lp.request.set(ARMED);
                        }
                    }
                }

                // Which loop the input belongs to, asked rather than remembered.
                // There is one converter, so at most one loop can be recording;
                // a separate "who has the input" field would be a second source
                // of truth able to disagree with the loops' own states.
                let Some(li) = sh.recording_loop() else {
                    sh.in_frames.store(base + frames, Ordering::Release);
                    return;
                };
                let lp = sh.lp(li);
                let state = lp.state.get();

                let k = sh.k.load(Ordering::Acquire);
                let origin = lp.rec_from.load(Ordering::Acquire);
                let loop_len = lp.loop_len.load(Ordering::Acquire);
                let layer = lp.n_layers.load(Ordering::Acquire);
                if layer >= MAX_LAYERS {
                    sh.in_frames.store(base + frames, Ordering::Release);
                    return;
                }

                for f in 0..frames {
                    let out_frame = (base + f) as i64 + k;
                    let rel = out_frame - origin;
                    if rel < 0 {
                        continue;
                    }
                    let v = data[f * in_channels + ch];

                    if state == FIRST || state == MULTIPLY {
                        // Linear. Its length becomes the cycle, so it must not
                        // wrap — and it stops rather than overwriting.
                        let pos = rel as usize;
                        if pos >= sh.max_frames {
                            lp.overflowed.store(true, Ordering::Relaxed);
                            continue;
                        }
                        sh.write(li, layer, pos, v);
                        lp.reached.fetch_max(pos + 1, Ordering::Relaxed);
                        lp.rec_reached.fetch_max(out_frame + 1, Ordering::Relaxed);
                    } else {
                        // Modular: an overdub may go round as many times as it
                        // likes, summing into the same cycle.
                        if loop_len == 0 {
                            continue;
                        }
                        let pos = (rel % loop_len as i64) as usize;
                        sh.add(li, layer, pos, v);
                        lp.reached.fetch_max(loop_len, Ordering::Relaxed);
                        lp.rec_reached.fetch_max(out_frame + 1, Ordering::Relaxed);
                    }
                }
                sh.in_frames.store(base + frames, Ordering::Release);
            },
            err_cb(err_sh),
            None,
        )?
    };

        Ok((out_stream, in_stream))
    };

    let (mut out_stream, mut in_stream) = build_streams(&device)?;
    out_stream.play()?;
    in_stream.play()?;
    std::thread::sleep(Duration::from_millis(300));

    if let Some(port) = opts.link_port {
        crate::link::spawn_listener(sh.clone(), sr, port);
    }

    if let Some(port) = opts.ws_port {
        crate::ws::serve(sh.clone(), sr, port);
    }

    if let Some(secs) = opts.selftest {
        let r = selftest(&sh, sr, secs);
        drop(in_stream);
        drop(out_stream);
        return r;
    }

    // With a socket open, the console moves to its own thread so the main one
    // can watch the device. cpal's streams are not `Send` on this platform, so
    // whichever thread built them is the only one that may replace them — and
    // supervision behind a blocking read of stdin would only begin once the
    // console closed, which is precisely backwards.
    if opts.ws_port.is_some() {
        let sh_console = sh.clone();
        std::thread::spawn(move || {
            // `q` means stop the daemon, not stop this thread. EOF means the
            // console was never there, and the socket carries on regardless.
            if control_loop(&sh_console, sr) {
                std::process::exit(0);
            }
            println!("(console closed; still serving the socket and watching the device)");
        });
    } else {
        let _ = control_loop(&sh, sr);
    }

    // stdin closing is not a reason to stop.
    //
    // Run headless — from a launcher, or with output redirected — and
    // `lines()` returns immediately at EOF. Exiting there would take the audio
    // engine and the socket down with it the instant the daemon stopped being
    // attached to a terminal, which is exactly when it is meant to be working.
    // With a socket open there is still a client to serve, so park instead.
    if opts.ws_port.is_some() {
        supervise(&sh, &opts.device, &build_streams, &mut out_stream, &mut in_stream);
    }

    drop(in_stream);
    drop(out_stream);
    Ok(())
}

/// Watch the device, and put it back when it goes.
///
/// Two detectors, because they catch different faults. cpal *reports* an
/// unplugged interface through the error callback — that is the loud case. But
/// a stream can also simply stop being called with no error at all, and that is
/// the one that cost an afternoon: the socket kept serving plausible snapshots
/// while both meters read digital zero and every command vanished into a
/// request nothing would ever consume. So the frame counter is watched too, and
/// a transport that claims to be running while its frames stand still is
/// treated as lost whether or not anyone said so.
///
/// Never returns. Ctrl-C or a kill stops the daemon.
fn supervise<F>(
    sh: &Arc<Shared>,
    device_name: &str,
    build: &F,
    out_stream: &mut cpal::Stream,
    in_stream: &mut cpal::Stream,
) where
    F: Fn(&cpal::Device) -> Result<(cpal::Stream, cpal::Stream), Box<dyn Error>>,
{
    const TICK_MS: u64 = 250;
    /// Ticks of a motionless frame counter before we stop giving it the benefit
    /// of the doubt. Comfortably longer than any buffer, short enough that the
    /// app says so before you have finished wondering.
    const STALL_TICKS: u32 = 8;

    let mut last_frames = sh.out_frames.load(Ordering::Acquire);
    let mut still = 0u32;

    loop {
        std::thread::sleep(Duration::from_millis(TICK_MS));

        let frames = sh.out_frames.load(Ordering::Acquire);
        if frames == last_frames {
            still += 1;
        } else {
            still = 0;
            last_frames = frames;
        }

        let reported = sh.device_lost.load(Ordering::Acquire);
        let stalled = still >= STALL_TICKS;
        if !reported && !stalled {
            continue;
        }

        eprintln!(
            "device {} — reopening {}",
            if reported { "reported lost" } else { "stopped answering" },
            device_name
        );

        // A recording that spans an outage has a hole in it, and a hole in a
        // layer is worse than no layer: it will be discovered later, in the
        // mix, with no way to tell what went wrong. So abandon whatever was
        // being captured and keep only what was already committed.
        // Whichever loop held the input, and every loop's pending request: an
        // outage invalidates all of them, not just the one that was recording.
        if let Some(li) = sh.recording_loop() {
            let lp = sh.lp(li);
            let n = lp.n_layers.load(Ordering::Acquire);
            sh.zero_layer(li, n);
            lp.state.set(if lp.loop_len.load(Ordering::Acquire) > 0 {
                PLAYING
            } else {
                IDLE
            });
            eprintln!("  the recording in progress on loop {} was dropped — it would have had a gap", li);
        }
        for li in 0..N_LOOPS {
            sh.lp(li).request.take();
        }

        // Both streams restart independently, so the input↔output pairing has
        // to be established again from scratch. Everything downstream reads
        // `k_set`, so clearing it is enough to make them wait for a fresh K
        // rather than trust a stale one.
        sh.k_set.store(false, Ordering::Release);
        if let Ok(mut g) = sh.p0.lock() {
            *g = None;
        }
        sh.p0_needed.store(true, Ordering::Release);

        // Reopen. The device has to be looked up again — after a USB cycle the
        // old handle refers to something that no longer exists.
        loop {
            std::thread::sleep(Duration::from_millis(750));
            let found = match crate::devices::find(device_name) {
                Ok(c) => c,
                Err(_) => continue,
            };
            match build(&found.device) {
                Ok((new_out, new_in)) => {
                    let played = new_out.play().and_then(|_| new_in.play());
                    if played.is_err() {
                        continue;
                    }
                    *out_stream = new_out;
                    *in_stream = new_in;
                    sh.device_lost.store(false, Ordering::Release);
                    sh.reopens.fetch_add(1, Ordering::Release);
                    last_frames = sh.out_frames.load(Ordering::Acquire);
                    still = 0;
                    eprintln!("  {} is back.", found.name);
                    break;
                }
                Err(_) => continue,
            }
        }
    }
}

/// Close a recording, as of the moment the foot went down rather than the
/// moment the command arrived.
///
/// `late` is how many frames ago the closing press happened. It is not a
/// nicety: a switch that may be double-tapped cannot resolve until the
/// double-tap window expires, so every close arrives a fixed few hundred
/// milliseconds after the press, and a free loop was coming out that much
/// longer than it was played. Nothing in the sound says so — overdubs are
/// modular against whatever length the loop ended up with, so everything still
/// stacks perfectly against a cycle nobody chose.
///
/// The fix is not to hurry the gesture but to un-do the delay: the audio for
/// those milliseconds is already in the arena, and the loop simply ends
/// earlier than the last frame recorded. Which is also why adding a double-tap
/// to a switch stopped costing anything recorded.
fn commit(sh: &Shared, li: usize, sr: u32, late: i64) {
    let lp = sh.lp(li);
    let state = lp.state.get();
    if state != FIRST && state != OVERDUB {
        return;
    }

    // The frame the foot went down on. Taken before anything below sleeps —
    // the quantised path waits for a boundary, which would move it.
    let closed_at = sh.out_frames.load(Ordering::Acquire) as i64 - late.max(0);

    // A quantised first recording gets a length that is a whole number of grid
    // cycles, decided here rather than taken from what happened to be captured.
    // Rounding to nearest means a press slightly late loses the overhang and a
    // press slightly early waits — which is right, because the intent was a
    // whole number of cycles either way, and a human aiming at a boundary
    // misses it in both directions.
    //
    // The wait happens BEFORE the state flips, so the loop keeps recording up
    // to the boundary. Flipping first and waiting after would hand back a loop
    // whose last fraction of a cycle is silence.
    let quantised_len = if state == FIRST && lp.quant.load(Ordering::Relaxed) {
        sh.grid().and_then(|(_, glen)| {
            let from = lp.origin.load(Ordering::Acquire);
            let cur = closed_at;
            let elapsed = (cur - from).max(0) as f64;
            let n = ((elapsed / glen as f64).round() as usize).max(1);
            let len = n * glen;
            if len > sh.max_frames {
                println!("  {} grid cycles would exceed --max-secs; closing free.", n);
                return None;
            }
            let target = from + len as i64;
            if target > cur {
                println!(
                    "  waiting {:.2} s for the grid boundary ({} cycle{}).",
                    (target - cur) as f64 / sr as f64,
                    n,
                    if n == 1 { "" } else { "s" }
                );
                while (sh.out_frames.load(Ordering::Acquire) as i64) < target {
                    std::thread::sleep(Duration::from_millis(5));
                }
            }
            Some(len)
        })
    } else {
        None
    };

    // Frames of continuation past this layer's end, filled in by whichever
    // branch below runs and handed to `set_layer_shape` at the bottom — one
    // place that decides a layer's shape, rather than two that each remember to
    // set part of it.
    let mut tail = 0usize;

    // Let the input drain: it trails the output by K, so the last frames of the
    // loop have not arrived yet. Without this the tail of every recording is
    // missing, which is exactly the kind of fault that sounds like "feel".
    lp.state.set(PLAYING);
    std::thread::sleep(Duration::from_millis(60));

    if state == FIRST {
        let reached = lp.reached.load(Ordering::Acquire);
        let mut len = quantised_len.unwrap_or_else(|| {
            if late <= 0 {
                return reached;
            }
            // What was played, rather than what was captured. The frames after
            // the press stay in the arena and are simply never read: playback
            // is `pos % len`, so anything past the end does not exist.
            let origin = lp.origin.load(Ordering::Acquire);
            let want = (closed_at - origin).max(0) as usize;
            // Only ever shorter. If the input has not caught up to the press —
            // it trails the output by K — then `reached` is the honest answer
            // and claiming further would claim silence.
            want.min(reached)
        });
        if len == 0 {
            println!("  nothing recorded.");
            return;
        }
        if late > 0 && quantised_len.is_none() && len < reached {
            println!(
                "  closed as of the press, {:.0} ms before the command: {} frames dropped.",
                late as f64 / sr as f64 * 1000.0,
                reached - len
            );
        }
        // Pre-roll: a tap is always a little late, so back-date the loop's start
        // and fill the front from the ring. The attack that would have been
        // clipped off is already captured; it just has to be claimed.
        // Never for a quantised loop: the pre-roll shifts `origin` backwards to
        // reclaim the attack, and moving origin is exactly what must not happen
        // to a loop that was started on a boundary. Alignment beats the last
        // few milliseconds of the attack, and a loop that drifts off the grid
        // by its pre-roll would be a bug nobody could see the cause of.
        // Measured beats configured: `started_late` is how late the press that
        // began this recording actually was, where `--preroll-ms` is a guess
        // applied to every take alike. Falls back to the guess when nothing
        // measured it, so a rig that cannot time its presses still works.
        let pre = if quantised_len.is_some() {
            0
        } else {
            let measured = lp.started_late.load(Ordering::Acquire);
            if measured > 0 {
                measured as usize
            } else {
                sh.preroll.load(Ordering::Acquire)
            }
        };
        let layer = lp.n_layers.load(Ordering::Acquire);
        let origin = lp.origin.load(Ordering::Acquire);
        let new_origin = origin - pre as i64;
        if pre > 0 && reached.max(len) + pre > sh.max_frames {
            // Shifting anyway would run off the end of this layer's slice and
            // into the next one's, which is silent corruption rather than an
            // error. Refuse instead.
            println!(
                "  pre-roll skipped: the loop plus pre-roll would exceed --max-secs."
            );
        } else if pre > 0 && new_origin >= 0 {
            // Shift what was recorded up by `pre`, backwards so the move does
            // not eat its own tail, then fill the vacated front from the ring.
            //
            // **Everything recorded, not just the loop.** The frames past `len`
            // are the continuation — what the player kept playing while the
            // gesture was still being worked out — and shifting only the loop
            // would leave them a `pre` behind and overlapped by the shifted
            // material. They are never sounded, so nothing would have said so.
            let moved = reached.max(len).min(sh.max_frames - pre);
            for pos in (0..moved).rev() {
                let v = sh.read(li, layer, pos);
                sh.write(li, layer, pos + pre, v);
            }
            for pos in 0..pre {
                sh.write(li, layer, pos, 0.0);
            }
            let got = fill_from_ring(sh, li, layer, new_origin, pre, 0, false);
            lp.origin.store(new_origin, Ordering::Release);
            len += pre;
            println!(
                "  pre-roll: {:.0} ms recovered from before the tap ({} of {} frames).",
                pre as f64 / sr as f64 * 1000.0,
                got,
                pre
            );
        }
        lp.loop_len.store(len, Ordering::Release);
        // What was recorded past the end, kept rather than trimmed. A first
        // recording writes linearly, so the continuation is already sitting
        // there and costs nothing to keep — it only had to not be thrown away.
        tail = (reached.max(len) + if quantised_len.is_some() { 0 } else { pre })
            .saturating_sub(len);
        // The first loop to acquire a length becomes the grid the rest
        // can align to — first rather than chosen, because that is how a
        // looper has always worked: what you played first is what the
        // rest fits around. A compare-exchange, so later calls are no-ops.
        sh.claim_anchor(li);
        println!(
            "  loop set: {} frames ({:.3} s), {:.1} bpm if that is one bar of 4/4",
            len,
            len as f64 / sr as f64,
            240.0 / (len as f64 / sr as f64)
        );
    }
    // An overdub is modular, so the frames recorded after the press did not
    // land past the end — they wrapped and SUMMED onto the head of their own
    // layer. That is a doubled transient at the loop point, not a length error,
    // and it is why an overdub needs undoing where a first recording only
    // needed measuring.
    //
    // Undone exactly, because the ring holds the very samples that were added:
    // subtract them where they landed, and write them where they belong — past
    // the end, as the continuation, the same place a first recording keeps it.
    // The material is not discarded, because it is the thing a seamless loop is
    // made of.
    if state == OVERDUB && late > 0 {
        let layer = lp.n_layers.load(Ordering::Acquire);
        let len = lp.loop_len.load(Ordering::Acquire);
        let k = sh.k.load(Ordering::Acquire);
        let rec_from = lp.rec_from.load(Ordering::Acquire);
        // The furthest output frame the input actually reached. From the
        // callback, not from a clock: `in_frames` keeps advancing after the
        // flip to PLAYING, so it names frames that were never recorded, and
        // subtracting those would gouge real audio out of the loop head.
        let last = lp.rec_reached.load(Ordering::Acquire);
        let mut undone = 0usize;
        let mut kept = 0usize;
        if len > 0 {
            for f in closed_at..last {
                let Some(v) = sh.ring_at(f - k) else { continue };
                let pos = (f - rec_from).rem_euclid(len as i64) as usize;
                sh.add(li, layer, pos, -v);
                undone += 1;
                let at = len + (f - closed_at) as usize;
                if at < sh.max_frames {
                    sh.write(li, layer, at, v);
                    kept += 1;
                }
            }
        }
        tail = kept;
        if undone > 0 {
            println!(
                "  {:.0} ms recorded after the press unwrapped from the loop head, \
                 kept as continuation ({} frames).",
                undone as f64 / sr as f64 * 1000.0,
                kept
            );
        }
    }

    let layer = lp.n_layers.load(Ordering::Acquire);
    let len = lp.loop_len.load(Ordering::Acquire);
    // Born on the pass it was committed on, which is when it starts existing as
    // something to be heard — and so when it starts getting older.
    lp.set_layer_shape(layer, Shape { len, tail, born: lp.pass_index(closed_at, len) });
    sh.rebuild_env(li, layer);
    lp.add_layer();
    if len > 0 {
        draw_layer(sh, li, layer, len, sr);
    }
    println!(
        "  committed. {} layer{} playing.",
        layer + 1,
        if layer == 0 { "" } else { "s" }
    );
}

/// What a layer actually contains, drawn.
///
/// "How do I know what has been recorded?" is a fair question to ask of a
/// machine whose entire state is invisible, and it is the question this whole
/// project exists to answer better than a single LED does. Hearing it is the
/// real answer; this is the one available the instant a pass ends, and it
/// distinguishes silence from quiet, a full loop from a half-empty one, and a
/// clipped take from a clean one at a glance.
fn draw_layer(sh: &Shared, li: usize, layer: usize, len: usize, sr: u32) {
    const COLS: usize = 56;
    const RAMP: [char; 8] = [' ', '.', ':', '-', '=', '+', '*', '#'];

    let mut peak = 0.0f32;
    let mut sum = 0.0f64;
    let mut bins = [0.0f32; COLS];
    for i in 0..len {
        let v = sh.read(li, layer, i).abs();
        peak = peak.max(v);
        sum += (v * v) as f64;
        let b = i * COLS / len;
        bins[b] = bins[b].max(v);
    }
    let rms = (sum / len.max(1) as f64).sqrt() as f32;

    if peak < 1e-6 {
        println!("  layer {}: silent.", layer);
        return;
    }

    let bar: String = bins
        .iter()
        .map(|&v| {
            // Against the layer's own peak, so a quiet take still shows its
            // shape rather than a flat line.
            let f = (v / peak).clamp(0.0, 1.0);
            RAMP[((f.sqrt() * 7.0).round() as usize).min(7)]
        })
        .collect();

    println!("  |{}|", bar);
    println!(
        "  layer {}   {:.2} s   peak {:.1} dBFS   rms {:.1} dBFS{}",
        layer,
        len as f64 / sr as f64,
        20.0 * (peak.max(1e-9) as f64).log10(),
        20.0 * (rms.max(1e-9) as f64).log10(),
        if peak >= 0.999 { "   CLIPPED" } else { "" }
    );
}

/// Fill a stretch of a layer from the pre-roll, addressing it in *output* frames
/// so it lands on the same grid live recording uses.
///
/// Returns how many frames were actually available. A short answer is not an
/// error — it means the request reached back further than the ring holds, and
/// the caller should say so rather than silently hand over a loop with a
/// truncated front.
fn fill_from_ring(
    sh: &Shared,
    li: usize,
    layer: usize,
    from_out: i64,
    len: usize,
    at: usize,
    additive: bool,
) -> usize {
    let k = sh.k.load(Ordering::Acquire);
    let mut got = 0;
    for pos in 0..len {
        let Some(v) = sh.ring_at(from_out + pos as i64 - k) else {
            continue;
        };
        if additive {
            sh.add(li, layer, at + pos, v);
        } else {
            sh.write(li, layer, at + pos, v);
        }
        got += 1;
    }
    got
}

/// Claim the recent past as a loop or a layer.
///
/// The feature no pedal can offer, and the one most likely to change how the
/// thing gets used: you played something good and did not hit record, so hit it
/// afterwards. With no loop yet, `secs` of the past becomes the loop and sets
/// the cycle. With a loop running, the last complete cycle becomes a new layer,
/// landing on the existing grid because the fill is addressed in output frames.
fn take(sh: &Shared, li: usize, sr: u32, secs: f64, late: i64) {
    let lp = sh.lp(li);
    if !sh.k_set.load(Ordering::Acquire) {
        println!("  no input has arrived yet.");
        return;
    }
    let layer = lp.n_layers.load(Ordering::Acquire);
    if layer >= MAX_LAYERS {
        println!("  {} layers is the ceiling; undo one first.", MAX_LAYERS);
        return;
    }

    let loop_len = lp.loop_len.load(Ordering::Acquire);
    // As of the press, not as of the command. Claiming the past is the one
    // gesture where the boundary is the whole point — you press because the
    // good bit has just finished — so the few hundred milliseconds a footswitch
    // takes to resolve would otherwise be claimed as part of it.
    let cur = sh.out_frames.load(Ordering::Acquire) as i64 - late.max(0);

    let (from_out, len, what) = if loop_len == 0 {
        let len = ((secs * sr as f64).round() as usize).min(sh.max_frames);
        (cur - len as i64, len, "loop")
    } else {
        // The last cycle that has actually finished. Anything else would be a
        // partial pass presented as a whole one.
        let origin = lp.origin.load(Ordering::Acquire);
        let done = (cur - origin).div_euclid(loop_len as i64);
        if done < 1 {
            println!("  not one complete cycle has gone by yet.");
            return;
        }
        (origin + (done - 1) * loop_len as i64, loop_len, "layer")
    };

    if from_out < 0 {
        println!("  that reaches back before the engine started.");
        return;
    }

    sh.zero_layer(li, layer);
    let got = fill_from_ring(sh, li, layer, from_out, len, 0, false);
    if got == 0 {
        println!("  the pre-roll does not reach back that far.");
        return;
    }
    if got < len {
        println!(
            "  only {:.2} s of the {:.2} s asked for was still in the pre-roll.",
            got as f64 / sr as f64,
            len as f64 / sr as f64
        );
    }

    if loop_len == 0 {
        lp.loop_len.store(len, Ordering::Release);
        // The first loop to acquire a length becomes the grid the rest
        // can align to — first rather than chosen, because that is how a
        // looper has always worked: what you played first is what the
        // rest fits around. A compare-exchange, so later calls are no-ops.
        sh.claim_anchor(li);
        lp.origin.store(from_out, Ordering::Release);
        lp.state.set(PLAYING);
        println!(
            "  took the last {:.3} s as the {}: {} frames, {:.1} bpm if that is one bar of 4/4",
            len as f64 / sr as f64,
            what,
            len,
            240.0 / (len as f64 / sr as f64)
        );
    } else {
        println!("  took the last complete cycle as a new {}.", what);
    }
    let taken = lp.n_layers.load(Ordering::Acquire);
    // The continuation comes from the ring too, so a claimed layer wraps as
    // seamlessly as a recorded one.
    //
    // It is free where it is available and empty where it is not, and the ring
    // says which: claiming the last complete *cycle* means what followed it has
    // already gone by, but claiming the last few seconds as the loop itself ends
    // at now, and nothing has followed now. `ring_at` refuses a frame it does
    // not hold, so the second case simply keeps nothing rather than reading a
    // minute-old slot as though it were the future.
    let taken_len = lp.loop_len.load(Ordering::Acquire);
    let want = sh.max_fade.min(sh.max_frames.saturating_sub(taken_len));
    let tail = fill_from_ring(sh, li, taken, from_out + taken_len as i64, want, taken_len, false);
    // A claimed layer is born now, not when the audio in it was played. It
    // starts being heard at this instant, and decay is about what you can hear.
    lp.set_layer_shape(taken, Shape { len: taken_len, tail, born: lp.pass_index(cur, taken_len) });
    sh.rebuild_env(li, taken);
    lp.add_layer();
    draw_layer(sh, li, taken, lp.loop_len.load(Ordering::Acquire), sr);
    println!(
        "  {} layer{} playing.",
        taken + 1,
        if taken == 0 { "" } else { "s" }
    );
}

/// Begin a multiply: keep the loop playing and start recording across it.
///
/// The EDP's gesture, and the one this whole thing was asked for — two bars
/// down, a couple of taps, and you are recording eight with the two repeating
/// underneath.
///
/// **It starts at the beginning of the cycle you are in, not when you pressed.**
/// The pre-roll holds that cycle already, so the part you have played of it is
/// recovered rather than lost, and the multiply lands on the grid instead of
/// wherever your foot happened to be. Pressing late is free.
fn multiply_start(sh: &Shared, li: usize, sr: u32) {
    let lp = sh.lp(li);
    let loop_len = lp.loop_len.load(Ordering::Acquire);
    if loop_len == 0 {
        println!("  nothing to multiply — record a loop first.");
        return;
    }
    if lp.n_layers.load(Ordering::Acquire) >= MAX_LAYERS {
        println!("  {} layers is the ceiling; undo one first.", MAX_LAYERS);
        return;
    }

    let origin = lp.origin.load(Ordering::Acquire);
    let cur = sh.out_frames.load(Ordering::Acquire) as i64;
    let cyc = (cur - origin).div_euclid(loop_len as i64);
    let from = origin + cyc * loop_len as i64;

    let layer = lp.n_layers.load(Ordering::Acquire);
    sh.zero_layer(li, layer);
    lp.rec_from.store(from, Ordering::Release);
    lp.reached.store(0, Ordering::Release);
    lp.state.set(MULTIPLY);

    // The part of this cycle already played is in the pre-roll; claim it, so
    // the multiply really does begin on the boundary.
    let behind = (cur - from) as usize;
    if behind > 0 {
        let got = fill_from_ring(sh, li, layer, from, behind, 0, false);
        lp.reached.fetch_max(got, Ordering::Relaxed);
        println!(
            "  multiplying from the start of this cycle — {:.2} s of it recovered \
             from the pre-roll.",
            got as f64 / sr as f64
        );
    } else {
        println!("  multiplying from this cycle's start.");
    }
    println!("  play across as many cycles as you want, then x again.");
}

/// End a multiply: round to whole cycles and grow the loop to fit.
///
/// Rounding rather than truncating, because at nine tenths of the way through
/// the fourth cycle you meant four. Which means sometimes waiting for the
/// boundary to arrive rather than cutting the loop short at the press.
fn multiply_end(sh: &Shared, li: usize, sr: u32) {
    let lp = sh.lp(li);
    let loop_len = lp.loop_len.load(Ordering::Acquire);
    let from = lp.rec_from.load(Ordering::Acquire);
    let cur = sh.out_frames.load(Ordering::Acquire) as i64;
    let elapsed = (cur - from).max(0) as f64;

    let n = ((elapsed / loop_len as f64).round() as usize).max(1);
    let new_len = n * loop_len;
    if new_len > sh.max_frames {
        println!(
            "  {} cycles would be {:.1} s, past the --max-secs ceiling of {:.1} s. \
             Stopping at the old length.",
            n,
            new_len as f64 / sr as f64,
            sh.max_frames as f64 / sr as f64
        );
        lp.state.set(PLAYING);
        return;
    }

    // If the rounding went up, the last cycle has not finished yet. Wait for it
    // rather than hand back a loop that is short by however late the press was.
    let target = from + new_len as i64;
    if target > cur {
        println!(
            "  rounding up to {} cycles; waiting {:.2} s for the boundary.",
            n,
            (target - cur) as f64 / sr as f64
        );
        while (sh.out_frames.load(Ordering::Acquire) as i64) < target {
            std::thread::sleep(Duration::from_millis(5));
        }
    }
    // And let the input drain past it, since it trails by K.
    lp.state.set(PLAYING);
    std::thread::sleep(Duration::from_millis(60));

    // "With the original repeating underneath" now costs nothing. Every existing
    // layer keeps its own length at `period = 1`, and the mix wraps it inside
    // the longer cycle by itself. This used to copy the audio n times, which
    // worked and threw away the structure: afterwards there was no one-bar thing
    // to make sparse, alternate or move, because it had been smeared across four
    // bars of buffer. The multiply began on a cycle boundary, so each layer's
    // position zero still lands where it did.

    // The new loop's position zero is where the multiply began.
    lp.origin.store(from, Ordering::Release);
    lp.loop_len.store(new_len, Ordering::Release);

    println!(
        "  x{}: loop is now {:.3} s ({} cycles of {:.3} s).",
        n,
        new_len as f64 / sr as f64,
        n,
        loop_len as f64 / sr as f64
    );
    let layer = lp.n_layers.load(Ordering::Acquire);
    // A multiplied layer ends where the multiply ended; nothing follows it. Born
    // at zero because a multiply redefines the cycle, so every pass count on
    // this loop starts again from here.
    lp.set_layer_shape(layer, Shape { len: new_len, tail: 0, born: 0 });
    sh.rebuild_env(li, layer);
    lp.add_layer();
    draw_layer(sh, li, layer, new_len, sr);
    println!("  committed. {} layers playing.", layer + 1);
}

/// The other multiply: keep the layer one bar long and give it room.
///
/// Ordinary multiply asks "how many bars of this?" and answers by repeating it.
/// This asks "how *often*?" and answers by leaving the rest silent. `s 2` on a
/// one-bar layer gives `B ~`; again gives `B ~ ~ ~`; again `B ~ ~ ~ ~ ~ ~ ~`.
/// Everything else keeps repeating underneath, so the loop grows without the
/// newest thing in it getting busier — which is the opposite of what a looper
/// usually does to you.
///
/// It takes no time. Ordinary multiply costs you n cycles of playing, because it
/// is recording; this is structural, so it lands on the next boundary and you
/// have not committed to anything you cannot take back with `d`.
///
/// **Growth is in whole multiples of the current cycle**, which is not an
/// arbitrary restriction: every layer's length divides the cycle, so a cycle
/// that is a multiple of the old one still divides evenly by all of them. Grow
/// by anything else and some other layer gets cut off mid-phrase at the wrap.
fn sparse(sh: &Shared, li: usize, sr: u32, n: usize) -> String {
    let lp = sh.lp(li);
    let layers = lp.n_layers.load(Ordering::Acquire);
    if layers == 0 {
        return "nothing to spread — record a loop first.".into();
    }
    let n = n.max(2);
    let l = layers - 1;
    let (len, period, phase) = lp.layer_shape(l);
    if len == 0 {
        return "that layer has no length.".into();
    }
    let loop_len = lp.loop_len.load(Ordering::Acquire);
    let new_len = n * loop_len;
    if new_len > sh.max_frames {
        return format!(
            "{} cycles would be {:.1} s, past the ceiling of {:.1} s.",
            n,
            new_len as f64 / sr as f64,
            sh.max_frames as f64 / sr as f64
        );
    }

    // Measured in the layer's own lengths, not in cycles, and multiplicative
    // rather than absolute: "spread by n" means *sound n times less often*. So
    // pressing it twice gives one in four, and a layer that already repeats four
    // times inside the cycle halves its density rather than jumping to once.
    let new_period = (period * n).max(1);
    lp.l_period[l].store(new_period, Ordering::Release);
    lp.l_phase[l].store(phase, Ordering::Release);
    lp.loop_len.store(new_len, Ordering::Release);

    format!(
        "layer {} sounds once every {} of its own lengths; loop is {:.3} s.",
        l + 1,
        new_period,
        new_len as f64 / sr as f64
    )
}

/// Move the newest layer one slot later in the cycle.
///
/// `B ~ ~ ~` → `~ B ~ ~` → `~ ~ B ~`. One button, and it is the cheapest way to
/// make a loop stop announcing where its bar line is.
///
/// It takes effect immediately rather than at the next boundary, which can chop
/// the layer mid-phrase if you press while it is sounding. The fix is the same
/// pending-at-the-wrap mechanism scenes will need, so it is worth building once,
/// for both, rather than here.
fn rotate(sh: &Shared, li: usize) -> String {
    let lp = sh.lp(li);
    let layers = lp.n_layers.load(Ordering::Acquire);
    if layers == 0 {
        return "nothing to move.".into();
    }
    let l = layers - 1;
    let (_, period, phase) = lp.layer_shape(l);
    if period <= 1 {
        return "that layer sounds every time round — spread it first.".into();
    }
    let next = (phase + 1) % period;
    lp.l_phase[l].store(next, Ordering::Release);
    format!("layer {} moved to slot {} of {}.", l + 1, next + 1, period)
}

/// Put the newest layer back to sounding every time round.
///
/// The loop keeps the length it grew to, because that length is now shared with
/// everything else that was recorded against it. Shrinking it would be a
/// different and much less reversible operation.
fn dense(sh: &Shared, li: usize) -> String {
    let lp = sh.lp(li);
    let layers = lp.n_layers.load(Ordering::Acquire);
    if layers == 0 {
        return "nothing to fill.".into();
    }
    let l = layers - 1;
    lp.l_period[l].store(1, Ordering::Release);
    lp.l_phase[l].store(0, Ordering::Release);
    format!("layer {} sounds every time round again.", l + 1)
}

/// Forget the loop's length, so the next recording lays down a new grid.
///
/// Undo removes a layer and deliberately keeps the length: erasing a first take
/// while holding onto the tempo you found is worth having, and the click goes on
/// running at it so the next attempt lands on the same grid. But without a way to
/// let go of it, undoing everything left the engine "stuck" at a length with
/// nothing in it — the transport still running, the record button still offering
/// an overdub, and no route back to an open-ended first recording short of `c`.
///
/// So the three erasures are distinct, and worth keeping distinct: `u` drops a
/// layer, this drops the grid, `c` drops both.
///
/// Refused while layers exist, because the length is what they are addressed by.
/// Clearing it under them would leave a mix reading positions in a cycle that no
/// longer has a size.
fn free_length(sh: &Shared, li: usize, sr: u32) -> String {
    let lp = sh.lp(li);
    let n = lp.n_layers.load(Ordering::Acquire);
    if n > 0 {
        return format!(
            "{} layer{} still playing — undo or clear them first; the length is what they sit in.",
            n,
            if n == 1 { "" } else { "s" }
        );
    }
    let was = lp.loop_len.load(Ordering::Acquire);
    if was == 0 {
        return "no length set — the next recording will set one.".into();
    }
    lp.loop_len.store(0, Ordering::Release);
    lp.reached.store(0, Ordering::Release);
    lp.state.set(IDLE);
    sh.release_anchor(li);
    format!(
        "length forgotten (was {:.3} s). The next recording sets a new one.",
        was as f64 / sr as f64
    )
}

/// Returns true only if the user actually asked to quit.
///
/// EOF is not a quit. Run headless — from a launcher, or with output
/// redirected — and `lines()` returns immediately, which must not be allowed to
/// take the audio engine and the socket down with it.
fn control_loop(sh: &Shared, sr: u32) -> bool {
    println!("Commands:  r = record/overdub toggle   x = multiply   t [secs] = take");
    println!("           s [n] = spread one in n   o = move it one slot   d = dense again");
    println!("           u = undo a layer   z = forget the length   c = both");
    println!("           w [name] = save the take (one file per layer + manifest)");
    println!("           g = follow the grid (the first loop's cycle) / free");
    println!(
        "           a leading digit picks the loop: 3r records loop 3, 3s2 spreads it,\n\
         \x20          a bare 3 selects it. {} loops, 0 to {}.",
        N_LOOPS,
        N_LOOPS - 1
    );
    println!("           k = click   m = input monitoring");
    println!("           l = levels   p = status + waveforms   q = quit\n");

    let stdin = std::io::stdin();
    for line in stdin.lock().lines() {
        let Ok(line) = line else { return false };
        if line.trim() == "q" {
            return true;
        }
        let ack = dispatch(sh, sr, &line);
        if !ack.is_empty() {
            println!("  {}", ack);
        }
    }
    false
}

/// One command, from wherever it came.
///
/// Both the console and the socket land here, so a footswitch, a browser and a
/// terminal cannot drift into meaning different things by the same name. The
/// detail still goes to stdout — waveforms and level readings are for the
/// person sitting at the daemon — and what comes back is the short
/// acknowledgement a remote caller needs. Remote clients render from the state
/// snapshot rather than from these strings.
pub fn dispatch(sh: &Shared, sr: u32, line: &str) -> String {
    // A leading digit picks the loop: `3r` records loop 3 whatever is selected,
    // `3s2` spreads it. Every command can therefore address a loop explicitly,
    // which is what the footswitch path needs — the MC6 sends one fixed message
    // per switch and must not depend on a selection it cannot see. A switch
    // that means different things according to hidden state is precisely the
    // failure this design exists to avoid.
    //
    // A bare digit selects, which is a convenience for the console and for the
    // single-loop view, and nothing depends on it.
    // `@<ms>` on the end says how long ago the press actually happened.
    //
    // **The app knows and the daemon cannot.** A switch that may be
    // double-tapped cannot resolve until the window expires, so every command
    // from a footswitch arrives a fixed few hundred milliseconds after the
    // foot moved — and a looper that believes the arrival time records a loop
    // that much longer than it was played. Nothing in the sound says so, which
    // is the worst kind of wrong.
    //
    // Carried on the command rather than inferred, because only the sender was
    // there. Stripped for every command and spent only by the ones for which a
    // frame matters, so a client can stamp everything it sends without having
    // to know which those are.
    let (line, late_ms) = match line.rsplit_once('@') {
        Some((cmd, ms)) => match ms.trim().parse::<f64>() {
            Ok(v) if v >= 0.0 && v < 5000.0 => (cmd, v),
            // Out of range or unparseable: refuse rather than silently treating
            // it as on time, because a client that thinks it is compensating
            // and is not would be worse off than one that never tried.
            _ => return format!("`@{}` is not a lateness in milliseconds.", ms.trim()),
        },
        None => (line, 0.0),
    };
    let late = (late_ms / 1000.0 * sr as f64).round() as i64;

    let trimmed = line.trim();
    let (li, rest) = match trimmed.chars().next() {
        Some(c) if c.is_ascii_digit() => {
            let n = c.to_digit(10).unwrap() as usize;
            if n >= N_LOOPS {
                return format!("there are {} loops, numbered 0 to {}.", N_LOOPS, N_LOOPS - 1);
            }
            (n, trimmed[1..].trim())
        }
        _ => (sh.sel(), trimmed),
    };
    if !trimmed.is_empty() && rest.is_empty() {
        sh.selected.store(li, Ordering::Relaxed);
        return format!("loop {} selected.", li);
    }
    let lp = sh.lp(li);
    {
        match rest {
            "x" => match lp.state.get() {
                MULTIPLY => multiply_end(sh, li, sr),
                FIRST | OVERDUB => println!("  finish this recording first."),
                _ => {
                    if let Some(other) = busy_elsewhere(sh, li) {
                        return other;
                    }
                    if let Some(no) = not_plain(lp, li) {
                        return no;
                    }
                    multiply_start(sh, li, sr)
                }
            },
            "r" => match lp.state.get() {
                MULTIPLY => multiply_end(sh, li, sr),
                FIRST | OVERDUB => commit(sh, li, sr, late),
                // A second press while the loop is waiting for a sound takes the
                // arm back. There has to be a way out: the sound may never come,
                // and a loop holding the input for a recording that will never
                // begin locks out all five others. Asked before the claim checks
                // below, because it is this loop's own claim being released.
                ARMED => {
                    lp.state.set(IDLE);
                    lp.arm_from.store(i64::MIN, Ordering::Release);
                    return format!("loop {} has stopped listening.", li);
                }
                _ => {
                    if let Some(other) = busy_elsewhere(sh, li) {
                        return other;
                    }
                    if let Some(no) = not_plain(lp, li) {
                        return no;
                    }
                    let layer = lp.n_layers.load(Ordering::Acquire);
                    if layer >= MAX_LAYERS {
                        println!("  {} layers is the ceiling; undo one first.", MAX_LAYERS);
                    } else {
                        // An overdub sums into its layer, so anything left there
                        // from an undone take would bleed into the new one.
                        sh.zero_layer(li, layer);
                        // And the picture of it, which is now of audio that no
                        // longer exists. Redrawn at commit; blank until then,
                        // which reads as "being recorded" rather than as a lie.
                        sh.rebuild_env(li, layer);
                        // Anything above this layer has just been made
                        // unrecoverable, so redo must not offer it.
                        lp.redo_to.store(layer, Ordering::Release);
                        // Kept until the recording closes, because the pre-roll
                        // shift that spends it happens at commit.
                        lp.started_late.store(late, Ordering::Release);
                        // Level-armed: wait for a sound rather than starting on
                        // the press. Nothing else happens here — the input
                        // callback finds the crossing and sets the same request
                        // this would have, so there is one road into `FIRST` and
                        // not two.
                        //
                        // The press's own lateness is dropped, deliberately. It
                        // measures how late the *foot* was, and the foot is no
                        // longer what starts this recording; carrying it would
                        // back-date the loop past the note that began it.
                        if lp.level_arm.load(Ordering::Relaxed) {
                            lp.started_late.store(0, Ordering::Release);
                            lp.arm_from.store(i64::MIN, Ordering::Release);
                            lp.request_at.store(i64::MIN, Ordering::Release);
                            lp.state.set(ARMED);
                            return format!(
                                "loop {} is listening — it starts when something goes over {}.",
                                li,
                                thresh_words(sh)
                            );
                        }
                        // Only a FIRST recording needs a deadline. An overdub
                        // records from `origin`, so it is already on whatever
                        // grid its loop sits on and cannot be nudged off it.
                        let boundary = if lp.quant.load(Ordering::Relaxed)
                            && lp.loop_len.load(Ordering::Acquire) == 0
                        {
                            sh.next_boundary(sh.out_frames.load(Ordering::Acquire) as i64)
                        } else {
                            None
                        };
                        match boundary {
                            Some(t) => {
                                lp.request_at.store(t, Ordering::Release);
                                lp.request.set(ARMED);
                                let wait =
                                    (t - sh.out_frames.load(Ordering::Acquire) as i64).max(0);
                                return format!(
                                    "loop {} starts on the grid in {:.2} s.",
                                    li,
                                    wait as f64 / sr as f64
                                );
                            }
                            None => {
                                lp.request_at.store(i64::MIN, Ordering::Release);
                                lp.request.set(ARMED);
                                println!("  recording...");
                            }
                        }
                    }
                }
            },
            // Fire a one-shot: one pass from the top, now.
            //
            // **Lateness is not spent here, and that is a choice.** Every other
            // time-critical command in this daemon subtracts it, because they all
            // describe something that has already been captured and can be
            // re-dated. A fire describes something about to be *played*, and no
            // speaker can emit a frame that should have gone out 300 ms ago. The
            // alternative — starting the pass that far in, so it lands where the
            // foot meant it to — buys grid alignment with the attack, and the
            // attack is the reason anybody fires a one-shot. So it starts at the
            // top and is late; `g1` is how you ask for it to be on the grid.
            "f" => {
                let len = lp.loop_len.load(Ordering::Acquire);
                if len == 0 {
                    return format!("loop {} is empty; there is nothing to fire.", li);
                }
                if !lp.one_shot.load(Ordering::Relaxed) {
                    return format!(
                        "loop {} is not a one-shot; `{}one1` first, or it would just \
                         jump to the top and carry on.",
                        li, li
                    );
                }
                let now = sh.out_frames.load(Ordering::Acquire) as i64;
                match if lp.quant.load(Ordering::Relaxed) {
                    sh.next_boundary(now)
                } else {
                    None
                } {
                    Some(t) => {
                        lp.request_at.store(t, Ordering::Release);
                        lp.request.set(FIRE);
                        return format!(
                            "loop {} fires on the grid in {:.2} s.",
                            li,
                            (t - now).max(0) as f64 / sr as f64
                        );
                    }
                    None => {
                        lp.request_at.store(i64::MIN, Ordering::Release);
                        lp.request.set(FIRE);
                        return format!("loop {} fires.", li);
                    }
                }
            }
            l if l.starts_with('t') => {
                let secs = l[1..].trim().parse::<f64>().unwrap_or(8.0);
                take(sh, li, sr, secs, late);
            }
            // **Above `s`, which prefix-matches.** `s` is sparse-multiply and
            // takes anything beginning with an s, so `sp0.5` read as "sparse,
            // could not parse the count, use 2" and quietly did a multiply. It
            // cost half an hour and would have cost a take: the command was
            // acked by nothing and did something else entirely. Ordering fixes
            // it here; `s` itself was tightened to refuse a count it cannot
            // read, rather than inventing one.
            _ if rest.starts_with("sp") => {
                let arg = &rest[2..];
                match arg.parse::<f64>() {
                    // An eighth to four times. Below that a loop is a drone and
                    // linear interpolation is audibly a filter; above it, the
                    // aliasing this does nothing about becomes the loudest thing
                    // in the sound.
                    Ok(v) if v.abs() >= 0.125 && v.abs() <= 4.0 => {
                        if lp.is_recording() {
                            return format!("loop {} is recording; speed would move the grid under it.", li);
                        }
                        lp.want(v, lp.pendulum.load(Ordering::Relaxed));
                        return format!(
                            "loop {} plays at x{} {}.",
                            li,
                            v.abs(),
                            if v < 0.0 { "backwards" } else { "forwards" }
                        );
                    }
                    Ok(v) => {
                        return format!("speed wants 0.125 to 4, either sign, not {}.", v)
                    }
                    _ => return format!("speed wants a number, not `{}`.", arg),
                }
            }
            // The second multiply, and its two companions. Structural, so they
            // are instant and reversible — nothing here records anything.
            l if l.starts_with('s') => {
                // Bare `s` means two, which is the common case. Anything else
                // has to be a number: `unwrap_or(2)` here turned every typo
                // beginning with an s into a multiply nobody asked for.
                let arg = l[1..].trim();
                match if arg.is_empty() { Ok(2) } else { arg.parse::<usize>() } {
                    Ok(n) => println!("  {}", sparse(sh, li, sr, n)),
                    Err(_) => return format!("`{}` is not a command; `s` wants a count.", l),
                }
            }
            // Grid sync for this loop. Explicit forms alongside the toggle for
            // the same reason `k` and `m` have them: a client that flips rather
            // than sets drifts out of step the first time a message is dropped
            // and never recovers.
            "g" | "g1" | "g0" => {
                let on = match rest {
                    "g1" => true,
                    "g0" => false,
                    _ => !lp.quant.load(Ordering::Relaxed),
                };
                lp.quant.store(on, Ordering::Relaxed);
                return match (on, sh.grid()) {
                    (false, _) => format!("loop {} is free.", li),
                    (true, Some((_, glen))) => format!(
                        "loop {} follows the grid ({:.3} s from loop {}).",
                        li,
                        glen as f64 / sr as f64,
                        sh.anchor.load(Ordering::Acquire)
                    ),
                    // Worth saying plainly rather than reporting success: the
                    // setting took, but with nothing to align to it does
                    // nothing, and a loop that starts free when you asked for
                    // the grid is the kind of surprise that gets blamed on the
                    // engine much later.
                    (true, None) => format!(
                        "loop {} will follow the grid — but no loop has a length yet, \
                         so there is no grid. The first recording makes one.",
                        li
                    ),
                };
            }
            "o" => println!("  {}", rotate(sh, li)),
            "d" => println!("  {}", dense(sh, li)),
            "z" => return free_length(sh, li, sr),
            // Returned rather than printed. This is the one command whose whole
            // point is *where* it put something, and a path printed on the
            // daemon's stdout is a path the app cannot show anyone — so the
            // message goes back as the ack and both callers display it
            // themselves. Printing here as well got it shown twice.
            l if l.starts_with('w') => return save_take(sh, li, sr, &l[1..]),
            // Take back an undo. Free, now that undo does not destroy what it
            // removes: the layer is still there with its shape intact, so this
            // is one number going back up.
            "y" => {
                let n = lp.n_layers.load(Ordering::Acquire);
                let ceiling = lp.redo_to.load(Ordering::Acquire);
                if n >= ceiling {
                    return if ceiling == 0 {
                        format!("loop {} has nothing to redo.", li)
                    } else {
                        format!("loop {} is already at its last take.", li)
                    };
                }
                lp.n_layers.store(n + 1, Ordering::Release);
                return format!("loop {} redone: {} layers playing.", li, n + 1);
            }
            "u" => {
                let n = lp.n_layers.load(Ordering::Acquire);
                if n == 0 {
                    return format!("loop {} has nothing to undo.", li);
                } else {
                    lp.n_layers.store(n - 1, Ordering::Release);
                    // **Not zeroed.** Undo used to destroy the audio as well as
                    // remove the layer, which made redo impossible — and the
                    // destruction was redundant: recording zeroes its layer
                    // before it starts, precisely so nothing left from an
                    // undone take can bleed into a new one. The belt was doing
                    // the braces' job and costing the only thing it prevented.
                    //
                    // `redo_to` is how far back up the layer stack still holds
                    // audio. Recording into a layer moves it, because a take
                    // that has been recorded over is not recoverable and
                    // offering to redo it would be a lie.
                    if n == 1 {
                        // Say what is being kept, or it reads as a fault. The
                        // length surviving an undo is the point — the click goes
                        // on at the tempo you found, so the next attempt lands on
                        // the same grid — but a length with nothing in it looks
                        // exactly like a looper that has stopped listening.
                        let len = lp.loop_len.load(Ordering::Acquire);
                        return format!(
                            "loop {} layer 1 removed. Empty now, but still {:.3} s long, so the \
                             next take lands on the same grid — `{}z` to forget the length.",
                            li,
                            len as f64 / sr as f64,
                            li
                        );
                    } else {
                        return format!("loop {} layer {} removed, {} left.", li, n, n - 1);
                    }
                }
            }
            // Deliberate device-loss injection, so the recovery path can be
            // proved rather than hoped for. It is the same argument as the
            // alignment self-test: this is a part of a looper that can be
            // verified, so it should be.
            "!lose" => {
                sh.device_lost.store(true, Ordering::Release);
                println!("  simulating device loss.");
            }
            // Silence a loop, or bring it back, without touching its origin.
            //
            // Explicit `h1`/`h0` alongside the flipping `h`, the same as the
            // click and the monitor: a dropped command must not leave the app
            // and the engine disagreeing about something the player cannot see
            // — and a stopped loop is invisible by definition.
            // Multi-letter from here on. Single letters were running out and a
            // config surface should read like what it does — `0rev1` and
            // `0pan32` say themselves in a log where `0v1` and `0n32` would
            // need the source open.
            "rev" | "rev1" | "rev0" => {
                let now = lp.speed();
                let back = match rest {
                    "rev1" => true,
                    "rev0" => false,
                    _ => now > 0.0,
                };
                // Direction changes the sign and keeps the rate, so reversing a
                // half-speed loop leaves it at half speed — the two are one
                // parameter and this is the arithmetic that says so.
                let want = now.abs() * if back { -1.0 } else { 1.0 };
                lp.want(want, lp.pendulum.load(Ordering::Relaxed));
                return format!(
                    "loop {} plays {} at x{}.",
                    li,
                    if back { "backwards" } else { "forwards" },
                    want.abs()
                );
            }
            // Forward, then back. Doubles the cycle, which is the point: a
            // pendulum that fitted into one cycle would be a different effect
            // wearing the name.
            "pend" | "pend1" | "pend0" => {
                let want = match rest {
                    "pend1" => true,
                    "pend0" => false,
                    _ => !lp.pendulum.load(Ordering::Relaxed),
                };
                lp.want(lp.speed(), want);
                return format!(
                    "loop {} {}.",
                    li,
                    if want {
                        "swings forward then back"
                    } else {
                        "runs one way"
                    }
                );
            }
            // One pass per trigger, rather than turning for ever.
            //
            // A mode, not a gesture, because it costs a loop its place in the
            // phase-locked set: firing moves `origin`, which is the one thing
            // this engine otherwise never does. Making it something you switch on
            // means a loop cannot lose its grid by accident.
            "one" | "one1" | "one0" => {
                let on = match rest {
                    "one1" => true,
                    "one0" => false,
                    _ => !lp.one_shot.load(Ordering::Relaxed),
                };
                lp.one_shot.store(on, Ordering::Relaxed);
                if !on {
                    // Back to a loop, from wherever the last pass left it. Its
                    // `origin` has moved and stays moved — that is what firing
                    // did, and pretending otherwise would put the audio somewhere
                    // nobody chose.
                    lp.shot_end.store(i64::MIN, Ordering::Release);
                }
                return if on {
                    format!(
                        "loop {} is a one-shot: silent now, one pass each time it fires.",
                        li
                    )
                } else {
                    format!("loop {} turns for ever again.", li)
                };
            }
            // Wait for a sound instead of starting on the press.
            "lev" | "lev1" | "lev0" => {
                let on = match rest {
                    "lev1" => true,
                    "lev0" => false,
                    _ => !lp.level_arm.load(Ordering::Relaxed),
                };
                lp.level_arm.store(on, Ordering::Relaxed);
                // Turning it off under a loop that is already waiting has to end
                // the wait, or the loop keeps the input for a recording that can
                // no longer begin.
                if !on && lp.is_armed() {
                    lp.state.set(IDLE);
                    lp.arm_from.store(i64::MIN, Ordering::Release);
                }
                return if on {
                    format!(
                        "loop {} waits for a sound over {} and reaches {:.0} ms back past it.",
                        li,
                        thresh_words(sh),
                        ARM_REACH_MS
                    )
                } else {
                    format!("loop {} records on the press again.", li)
                };
            }
            // How much a pass costs the material already there, in decibels.
            //
            // Decibels rather than a gain, because that is the unit the effect
            // is actually thought in — "three down a pass" is a musical
            // statement where "point seven oh eight" is a number — and because
            // it makes the ladder on the pedal readable.
            _ if rest.starts_with("dec") => {
                let arg = rest[3..].trim();
                if arg.is_empty() {
                    return format!("loop {} {}.", li, decay_words(lp));
                }
                match arg.parse::<f32>() {
                    // Positive would be feedback above unity, which is not a
                    // longer decay, it is a loop that gets louder until it
                    // clips. Refused by name rather than clamped.
                    Ok(db) if db > 0.0 => {
                        return format!(
                            "decay is a loss, so it wants zero or less; {} per pass would \
                             run away.",
                            db
                        )
                    }
                    Ok(db) if db >= -60.0 => {
                        lp.decay
                            .store(10f32.powf(db / 20.0).to_bits(), Ordering::Relaxed);
                        return format!("loop {} {}.", li, decay_words(lp));
                    }
                    Ok(db) => return format!("decay wants 0 to -60 dB a pass, not {}.", db),
                    _ => return format!("decay wants decibels a pass, not `{}`.", arg),
                }
            }
            // Crossfade the wrap, in milliseconds. Zero is off.
            //
            // Says when it will do nothing. A loop whose layers kept no
            // continuation has nothing to fade *from*, so the setting takes and
            // is inaudible — which is the exact shape of failure this surface
            // exists to prevent, and costs one sentence to rule out.
            _ if rest.starts_with("xf") => {
                let arg = rest[2..].trim();
                if arg.is_empty() {
                    return format!("loop {} wraps with {}.", li, fade_words(lp, sr));
                }
                match arg.parse::<f64>() {
                    // Half a second is already far longer than a wrap wants; past
                    // that it is not a join, it is a different effect.
                    Ok(ms) if (0.0..=MAX_FADE_MS).contains(&ms) => {
                        lp.fade
                            .store((ms / 1000.0 * sr as f64).round() as usize, Ordering::Relaxed);
                        // Which layers can actually use it, said in numbers.
                        //
                        // All-or-nothing was the first version and it was the
                        // usual half-truth: a loop where two layers of three
                        // kept a continuation wraps two-thirds seamlessly and
                        // reported nothing at all, so the one hard join left
                        // would have been a click with no explanation anywhere.
                        let n = lp.n_layers.load(Ordering::Acquire);
                        let kept = (0..n).filter(|&l| lp.layer_tail(l) > 0).count();
                        return format!(
                            "loop {} wraps with {}.{}",
                            li,
                            fade_words(lp, sr),
                            match (ms > 0.0, kept, n) {
                                (false, _, _) | (_, _, 0) => String::new(),
                                (_, 0, _) => "  Nothing here kept a continuation, though, so \
                                              there is nothing to fade from."
                                    .into(),
                                (_, k, n) if k == n => String::new(),
                                (_, k, n) => format!(
                                    "  {} of {} layers kept a continuation; the rest still \
                                     join hard.",
                                    k, n
                                ),
                            }
                        );
                    }
                    Ok(ms) => return format!("the wrap fade wants 0 to {:.0} ms, not {}.", MAX_FADE_MS, ms),
                    _ => return format!("the wrap fade wants milliseconds, not `{}`.", arg),
                }
            }
            // How often a pass sounds. A probability rather than a ratio,
            // because the ladder the board offers (always, 3 in 4, 1 in 2, 1 in
            // 4, 1 in 8) is a choice the *app* makes about which values are
            // worth a press, and the engine should not have opinions about that
            // — the same reason speed takes a number and not a gear.
            _ if rest.starts_with("ch") => {
                let arg = rest[2..].trim();
                if arg.is_empty() {
                    return format!("loop {} sounds {}.", li, odds_words(lp.chance_of()));
                }
                match arg.parse::<f32>() {
                    Ok(p) if (0.0..=1.0).contains(&p) => {
                        lp.chance.store(p.to_bits(), Ordering::Relaxed);
                        // Forget the pass the last roll covered, or a loop set to
                        // always would stay silent until the cycle turned over —
                        // the switch would look like it had not worked, for up to
                        // a whole cycle, which is exactly long enough to press it
                        // again and undo what you just did.
                        lp.chance_pass.store(i64::MIN, Ordering::Relaxed);
                        lp.chance_sounds.store(true, Ordering::Relaxed);
                        return format!("loop {} sounds {}.", li, odds_words(p));
                    }
                    Ok(p) => return format!("chance wants 0 to 1, not {}.", p),
                    _ => return format!("chance wants a probability, not `{}`.", arg),
                }
            }
            // The level a sound has to reach. Rig-wide, like the click — it
            // describes the room and the instrument, not any one loop.
            _ if rest.starts_with("arm") => {
                let arg = rest[3..].trim();
                if arg.is_empty() {
                    return format!("a level-armed loop starts at {}.", thresh_words(sh));
                }
                match arg.parse::<f64>() {
                    // Full scale to the noise floor. Above zero can never be
                    // reached and below -80 is the converter's own hiss, so both
                    // are refused rather than accepted into a mode that would
                    // then never fire, or fire immediately.
                    Ok(db) if db <= 0.0 && db >= -80.0 => {
                        sh.arm_thresh.store(db_to_mag(db).to_bits(), Ordering::Relaxed);
                        return format!("a level-armed loop now starts at {}.", thresh_words(sh));
                    }
                    Ok(db) => return format!("the arm level wants 0 to -80 dBFS, not {}.", db),
                    _ => return format!("the arm level wants a number of dBFS, not `{}`.", arg),
                }
            }
            _ if rest.starts_with("pan") => {
                match rest[3..].parse::<usize>() {
                    Ok(v) if v <= 127 => {
                        lp.pan.store(v, Ordering::Relaxed);
                        let (l, r) = lp.pan_gains();
                        return format!(
                            "loop {} panned {} (L {:.2}, R {:.2}).",
                            li,
                            match v {
                                0..=10 => "hard left",
                                11..=52 => "left",
                                53..=74 => "centre",
                                75..=116 => "right",
                                _ => "hard right",
                            },
                            l,
                            r
                        );
                    }
                    // Says what was wrong rather than ignoring it. A config
                    // command that silently does nothing is the failure this
                    // whole surface is built against.
                    _ => return format!("pan wants 0-127, not `{}`.", &rest[3..]),
                }
            }
            "h" | "h1" | "h0" => {
                let want = match rest {
                    "h1" => false,
                    "h0" => true,
                    _ => !lp.muted.load(Ordering::Relaxed),
                };
                lp.muted.store(want, Ordering::Relaxed);
                return format!(
                    "loop {} {}.",
                    li,
                    if want { "stopped, still turning" } else { "playing" }
                );
            }
            "c" => {
                lp.state.set(IDLE);
                // A cleared loop is an empty loop, and an empty loop that is
                // still silenced would refuse to record audibly for a reason
                // nothing on screen could explain.
                lp.muted.store(false, Ordering::Relaxed);
                // The resolutions go with the audio. A cleared loop that came
                // back at half speed backwards and hard left would be a
                // haunting.
                lp.plainly();
                lp.pan.store(64, Ordering::Relaxed);
                // The modes go too. A cleared slot that still fired once and
                // waited for a sound would be a loop with someone else's habits,
                // and the switch that cleared it said nothing about either.
                lp.one_shot.store(false, Ordering::Relaxed);
                lp.shot_end.store(i64::MIN, Ordering::Release);
                lp.level_arm.store(false, Ordering::Relaxed);
                lp.arm_from.store(i64::MIN, Ordering::Release);
                lp.fade.store(0, Ordering::Relaxed);
                lp.decay.store(1.0f32.to_bits(), Ordering::Relaxed);
                lp.chance.store(1.0f32.to_bits(), Ordering::Relaxed);
                lp.chance_pass.store(i64::MIN, Ordering::Relaxed);
                lp.chance_sounds.store(true, Ordering::Relaxed);
                lp.n_layers.store(0, Ordering::Release);
                lp.redo_to.store(0, Ordering::Release);
                lp.loop_len.store(0, Ordering::Release);
                for l in 0..MAX_LAYERS {
                    sh.zero_layer(li, l);
                    lp.set_layer_shape(l, Shape { len: 0, tail: 0, born: 0 });
                }
                sh.clear_env(li);
                sh.release_anchor(li);
                println!("  cleared.");
            }
            // `k` and `m` flip, which is right at a console and wrong over a
            // wire: a client that sets rather than flips drifts out of step the
            // first time a command is dropped, and never recovers. So the
            // explicit forms exist alongside, and the app uses those.
            "k" | "k1" | "k0" => {
                let on = match line.trim() {
                    "k1" => true,
                    "k0" => false,
                    _ => !sh.click.load(Ordering::Relaxed),
                };
                sh.click.store(on, Ordering::Relaxed);
                return format!("click {}.", if on { "on" } else { "off" });
            }
            "m" | "m1" | "m0" => {
                let on = match line.trim() {
                    "m1" => true,
                    "m0" => false,
                    _ => !sh.monitor.load(Ordering::Relaxed),
                };
                sh.monitor.store(on, Ordering::Relaxed);
                return format!(
                    "input monitoring {}.{}",
                    if on { "on" } else { "off" },
                    if on {
                        "  (the interface's own direct monitoring is lower latency)"
                    } else {
                        ""
                    }
                );
            }
            "l" => {
                let inp = f32::from_bits(sh.in_peak.swap(0, Ordering::Relaxed));
                let out = f32::from_bits(sh.out_peak.swap(0, Ordering::Relaxed));
                println!(
                    "  in {:>7.1} dBFS   out {:>7.1} dBFS   (peak since last check)",
                    20.0 * (inp.max(1e-9) as f64).log10(),
                    20.0 * (out.max(1e-9) as f64).log10()
                );
                if inp < 1e-6 {
                    println!("    nothing at all is arriving on input {}.", "the chosen channel");
                }
            }
            "p" => {
                let len = lp.loop_len.load(Ordering::Acquire);
                for l in 0..lp.n_layers.load(Ordering::Acquire) {
                    if len > 0 {
                        draw_layer(sh, li, l, len, sr);
                    }
                }
                println!(
                    "  {} layers, loop {} frames ({:.3} s), state {}, K {:+}{}",
                    lp.n_layers.load(Ordering::Acquire),
                    len,
                    len as f64 / sr as f64,
                    match lp.state.get() {
                        FIRST => "recording first",
                        OVERDUB => "overdubbing",
                        MULTIPLY => "multiplying",
                        PLAYING => "playing",
                        _ => "idle",
                    },
                    sh.k.load(Ordering::Acquire),
                    if lp.overflowed.load(Ordering::Relaxed) {
                        "   (a recording hit the arena ceiling)"
                    } else {
                        ""
                    }
                );
            }
            "" => {}
            other => return format!("unknown command {:?}", other),
        }
    }
    String::new()
}

/// Write the loop out as a take: one file per layer, plus a manifest.
///
/// **Not a bounce.** A take is the layers at the lengths they were recorded,
/// with their `period` and `phase` recorded beside them — so a take reloads as
/// the thing that was played, and `s`/`o`/`d` still mean something afterwards.
/// The resolved mix is a *view* of this and can be rendered whenever it is
/// wanted; the reverse is not true, because flattening destroys the fact that
/// there were layers at all. Same argument as the engine's refusal to tile a
/// layer into a longer cycle, and as `MidiClip` storing every note.
///
/// The manifest carries no timestamp on purpose. Two takes of identical audio
/// should produce identical bytes, because the destination for these is
/// amphora, which keys an artefact by the hash of its content — a clock reading
/// baked into the payload would make every save a different artefact and throw
/// that away. When it was written is the filesystem's business.
fn save_take(sh: &Shared, li: usize, sr: u32, name: &str) -> String {
    let lp = sh.lp(li);
    if lp.is_recording() || lp.is_armed() {
        return "finish the recording first — a layer still being written is half a thing.".into();
    }
    let n = lp.n_layers.load(Ordering::Acquire);
    let loop_len = lp.loop_len.load(Ordering::Acquire);
    if n == 0 || loop_len == 0 {
        return "nothing to save yet.".into();
    }

    let name = safe_name(name);
    let dir = sh.takes_dir.join(&name);
    if let Err(e) = std::fs::create_dir_all(&dir) {
        return format!("could not make {}: {}", dir.display(), e);
    }

    let mut entries: Vec<String> = Vec::new();
    let mut written = 0usize;
    for l in 0..n {
        let (len, period, phase) = lp.layer_shape(l);
        if len == 0 {
            continue;
        }
        if len > crate::wav::MAX_FRAMES {
            return format!("layer {} is longer than a WAV can address.", l);
        }
        // Nothing is writing the arena here — saving is refused while
        // recording — so a plain read is a consistent read.
        let samples: Vec<f32> = (0..len).map(|p| sh.read(li, l, p)).collect();
        // Zero-padded because these become a SuperDirt sample bank, and its
        // loader sorts the folder lexicographically to assign `n` indices.
        // Unpadded, a tenth layer would sort between the first and the second
        // and every index past it would name the wrong audio — silently, since
        // nothing downstream can tell a misordered bank from an intended one.
        // `MAX_LAYERS` is 8 today, so this is insurance bought while it is free.
        let file = format!("layer-{:02}.wav", l);
        if let Err(e) = std::fs::write(dir.join(&file), crate::wav::wav_bytes(&samples, sr)) {
            return format!("could not write {}: {}", file, e);
        }
        entries.push(format!(
            r#"{{"file":"{}","len":{},"period":{},"phase":{}}}"#,
            file, len, period, phase
        ));
        written += 1;
    }

    // Hand-rolled for the same reason `snapshot` is: the shape is fixed and
    // small, and every value in it is a number or a name this function chose,
    // so there is nothing here that could need escaping.
    let manifest = format!(
        concat!(
            "{{\n  \"version\": 1,\n  \"sampleRate\": {},\n",
            "  \"loopFrames\": {},\n  \"loopSecs\": {:.6},\n  \"layers\": [\n    {}\n  ]\n}}\n"
        ),
        sr,
        loop_len,
        loop_len as f64 / sr as f64,
        entries.join(",\n    ")
    );
    if let Err(e) = std::fs::write(dir.join("take.json"), manifest) {
        return format!("wrote the audio but not the manifest: {}", e);
    }

    format!(
        "saved {} layer{} ({:.3} s) to {}",
        written,
        if written == 1 { "" } else { "s" },
        loop_len as f64 / sr as f64,
        dir.display()
    )
}

/// Refuse a claim on the input when another loop already has it.
///
/// There is one converter, so only one loop can record at a time. Without this
/// the second loop would go to `FIRST` quite happily and then capture nothing,
/// because the input callback asks `recording_loop()` and gets the first match —
/// a loop that says it is recording, shows as recording, and is writing to no
/// buffer. Refusing out loud is the whole difference between a rule and a bug.
fn busy_elsewhere(sh: &Shared, li: usize) -> Option<String> {
    match sh.input_claimed() {
        Some(other) if other != li => Some(format!(
            "loop {} has the input ({}). One converter, one recording — finish that first.",
            other,
            sh.lp(other).state_name()
        )),
        _ => None,
    }
}

/// Why a loop at a speed cannot be recorded into.
///
/// Named rather than worked around. The input arrives at rate one and the loop's
/// grid is moving under it, so there is no honest place to put the samples —
/// and the answer that would look like it worked (resample the input, or quietly
/// snap back to rate one) is the answer this project keeps refusing.
fn not_plain(lp: &Loop, li: usize) -> Option<String> {
    if lp.plain() {
        return None;
    }
    Some(format!(
        "loop {} is playing at x{}{}; `{}sp1` to record into it.",
        li,
        lp.speed().abs(),
        if lp.pendulum.load(Ordering::Relaxed) {
            ", swinging"
        } else if lp.speed() < 0.0 {
            ", backwards"
        } else {
            ""
        },
        li
    ))
}

/// A take name that cannot leave the takes directory.
///
/// Everything outside a small safe set becomes a dash rather than being
/// rejected, so a name typed with a slash in it still saves somewhere sensible
/// instead of failing at the one moment the user is trying not to lose a take.
fn safe_name(raw: &str) -> String {
    let cleaned: String = raw
        .trim()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '-' })
        .collect();
    let cleaned = cleaned.trim_matches('-').to_string();
    if cleaned.is_empty() {
        format!(
            "take-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0)
        )
    } else {
        cleaned
    }
}

/// Record one cycle of the engine's own click through a loopback cable and ask
/// where it ended up. Same question `align` asks, but through the real transport
/// and the real layer storage — so it tests what will actually run.
fn selftest(sh: &Shared, sr: u32, secs: f64) -> Result<(), Box<dyn Error>> {
    // Loop 0 throughout. The properties under test — that a recording lands
    // where it was heard, that overdubs stack, that a claimed cycle is the one
    // that played, that multiply and spread are exactly reversible — are about
    // one loop's storage and transport, and are the same for all six.
    let li = 0usize;
    let lp = sh.lp(li);
    let len = (secs * sr as f64).round() as usize;
    println!("Self-test: {} frame loop ({:.2} s), recording one cycle.", len, secs);

    lp.loop_len.store(len, Ordering::Release);
    lp.request.set(ARMED);
    std::thread::sleep(Duration::from_secs_f64(secs * 2.0 + 0.3));
    commit(sh, li, sr, 0);
    std::thread::sleep(Duration::from_millis(200));

    let (e0, p0) = onset_of(sh, li, 0, len)
        .ok_or("nothing recorded — is the loopback cable patched from the output \
                jack to the input jack named by --out-ch / --in-ch?")?;
    println!(
        "  layer 0: click played at 0, recorded at {:+} samples ({:+.3} ms), peak {:.1} dBFS",
        e0,
        e0 as f64 / sr as f64 * 1000.0,
        20.0 * (p0.max(1e-9) as f64).log10()
    );

    // Now the property a looper actually stands on: that an overdub recorded
    // while listening to an existing layer lands on top of it. The click is
    // switched off, so the only thing going down the cable is layer 0 playing
    // back. If it returns to the same position, layers stack — and if it did
    // not, every overdub would sit a little further out than the last.
    println!("\nOverdub pass: click off, recording layer 0's own playback.");
    sh.click.store(false, Ordering::Relaxed);
    lp.request.set(ARMED);
    std::thread::sleep(Duration::from_secs_f64(secs * 2.0 + 0.3));
    commit(sh, li, sr, 0);
    std::thread::sleep(Duration::from_millis(200));

    let (e1, p1) = onset_of(sh, li, 1, len)
        .ok_or("the overdub recorded nothing, though the first pass worked")?;
    println!(
        "  layer 1: layer 0's click returned at {:+} samples ({:+.3} ms), peak {:.1} dBFS",
        e1,
        e1 as f64 / sr as f64 * 1000.0,
        20.0 * (p1.max(1e-9) as f64).log10()
    );

    // Third: claim a cycle that was never recorded. Both existing layers carry
    // the click, so playback has one at position zero; a retroactive take of
    // the last complete cycle must land it there too. This is the pre-roll path
    // rather than the live-record path, and it uses different code to reach the
    // same grid — so it deserves its own check.
    println!("\nRetroactive take: claiming the last complete cycle from the pre-roll.");
    std::thread::sleep(Duration::from_secs_f64(secs * 1.5));
    take(sh, li, sr, 0.0, 0);
    std::thread::sleep(Duration::from_millis(100));

    let e2 = match onset_of(sh, li, 2, len) {
        Some((e, p)) => {
            println!(
                "  layer 2: taken from the past, click at {:+} samples ({:+.3} ms), peak {:.1} dBFS",
                e,
                e as f64 / sr as f64 * 1000.0,
                20.0 * (p.max(1e-9) as f64).log10()
            );
            e
        }
        None => return Err("the retroactive take captured nothing".into()),
    };

    // Fourth: grow the loop while it plays. The claim being tested is not the
    // arithmetic but the bookkeeping — that everything already recorded repeats
    // into the new length, which is what "with the original playing underneath"
    // means and is the whole point of the gesture.
    println!("\nMultiply: growing the loop while it plays.");
    multiply_start(sh, li, sr);
    std::thread::sleep(Duration::from_secs_f64(secs * 2.2));
    multiply_end(sh, li, sr);
    std::thread::sleep(Duration::from_millis(100));

    let new_len = lp.loop_len.load(Ordering::Acquire);
    if new_len % len != 0 {
        return Err(format!(
            "the multiplied loop is {} frames, not a whole multiple of {}",
            new_len, len
        )
        .into());
    }
    let n = new_len / len;

    // Layer 0 carried the click at position zero. After a multiply it should be
    // *audible* at every cycle boundary — which is a question about the mix, not
    // about where the bytes are, so it is asked through `sample_at`.
    let click_at = |c: usize| -> f32 {
        let mut best = 0f32;
        for d in 0..64usize {
            best = best.max(sh.sample_at(li, 0, (c * len + d) % new_len).abs());
            if c * len + len > d {
                let back = (c * len + new_len - d - 1) % new_len;
                best = best.max(sh.sample_at(li, 0, back).abs());
            }
        }
        best
    };
    let mut missing = Vec::new();
    for c in 0..n {
        if click_at(c) < 0.01 {
            missing.push(c);
        }
    }
    println!(
        "  loop is now x{} ({:.2} s), and layer 0 repeats at {} of {} cycle boundaries.",
        n,
        new_len as f64 / sr as f64,
        n - missing.len(),
        n
    );
    if !missing.is_empty() {
        return Err(format!(
            "the original does not repeat underneath — it is missing at cycle(s) {:?}. \
             A multiply that drops what it was multiplying is worse than no multiply",
            missing
        )
        .into());
    }

    // The other multiply, checked on the same click. Spreading layer 0 one-in-n
    // must silence it at every boundary but one, and moving it must move which
    // one — and both must be exactly reversible, since the whole claim of doing
    // this at playback rather than by copying is that nothing was destroyed.
    if n >= 2 {
        println!("\n  Spread: the same layer, sounding once instead of {} times.", n);
        let before: Vec<f32> = (0..n).map(click_at).collect();
        lp.l_period[0].store(n, Ordering::Release);
        lp.l_phase[0].store(0, Ordering::Release);
        let sounding: Vec<usize> = (0..n).filter(|&c| click_at(c) >= 0.01).collect();
        if sounding != vec![0] {
            return Err(format!(
                "spread one-in-{} should sound at cycle 0 alone; it sounds at {:?}",
                n, sounding
            )
            .into());
        }
        lp.l_phase[0].store(n - 1, Ordering::Release);
        let moved: Vec<usize> = (0..n).filter(|&c| click_at(c) >= 0.01).collect();
        if moved != vec![n - 1] {
            return Err(format!(
                "moved to the last slot it should sound at cycle {} alone; it sounds at {:?}",
                n - 1,
                moved
            )
            .into());
        }
        println!("    sounds at cycle 0 alone, then at cycle {} alone.", n - 1);

        lp.l_period[0].store(1, Ordering::Release);
        lp.l_phase[0].store(0, Ordering::Release);
        let after: Vec<f32> = (0..n).map(click_at).collect();
        if before != after {
            return Err("dense again did not restore what spreading hid — the audio \
                        was altered by an operation that is supposed to be a view of it"
                .into());
        }
        println!("    and dense again is identical to before, sample for sample.");
    }

    let slip = e1 - e0;
    println!("\n  Layer-to-layer slip: {:+} samples.", slip);
    if e2.abs() > 2 {
        return Err(format!(
            "live recording aligns but the retroactive take is {} samples out — the \
             pre-roll is being addressed on a different grid than the live path",
            e2.abs()
        )
        .into());
    }

    if e0.abs() <= 2 && slip.abs() <= 2 {
        println!(
            "\n  Aligned through the real transport and the real layer storage, and\n  \
             overdubs stack on top of what they were recorded against. Eight layers\n  \
             deep will be as tight as one."
        );
        Ok(())
    } else if e0.abs() > 2 {
        Err(format!(
            "layer 0 is off by {} samples through the engine, though `align` passes — \
             the fault is in the transport, not the calibration",
            e0.abs()
        )
        .into())
    } else {
        Err(format!(
            "layer 0 lands correctly but the overdub slips {} samples against it. That \
             compounds: eight layers would end up {} samples apart",
            slip.abs(),
            slip.abs() * 8
        )
        .into())
    }
}

/// Onset position of the loudest thing in a layer, as a signed offset from loop
/// position zero, with its peak. Wrapping, because something landing slightly
/// early sits at the end of the loop rather than the start.
fn onset_of(sh: &Shared, li: usize, layer: usize, len: usize) -> Option<(i64, f32)> {
    let mut peak = 0f32;
    let mut peak_at = 0usize;
    for i in 0..len {
        let v = sh.read(li, layer, i).abs();
        if v > peak {
            peak = v;
            peak_at = i;
        }
    }
    if peak < 0.01 {
        return None;
    }
    let mut onset = peak_at;
    for _ in 0..len {
        let prev = (onset + len - 1) % len;
        if sh.read(li, layer, prev).abs() <= 0.01 {
            break;
        }
        onset = prev;
    }
    let e = if onset > len / 2 { onset as i64 - len as i64 } else { onset as i64 };
    Some((e, peak))
}

/// The playhead arithmetic, which is the one part of speed that can be checked
/// without a cable.
///
/// `align` proves where recorded audio *lands*; these prove where the playhead
/// *is*, which is a different claim and the one this change actually makes. The
/// property that matters most is the last one: a speed change must not move the
/// audio, and that is a statement about two calls to `play_pos` either side of
/// an `adopt` rather than about anything anyone can hear.
#[cfg(test)]
mod tests {
    use super::*;

    const LEN: usize = 1000;

    /// A loop with its position zero at output frame zero.
    fn at_origin() -> Loop {
        let lp = Loop::new();
        lp.origin.store(0, Ordering::Relaxed);
        lp
    }

    #[test]
    fn rate_one_is_the_subtraction_it_always_was() {
        let lp = at_origin();
        // Exactly integral, which is what lets the mix skip interpolation and
        // read one sample per layer in the ordinary case.
        for f in [0i64, 1, 999, 1000, 1001, 48_000_000] {
            let p = lp.play_pos(f, LEN);
            assert_eq!(p, p.floor(), "frame {} landed between samples", f);
            assert_eq!(p as i64, f.rem_euclid(LEN as i64));
        }
        assert!(lp.plain());
    }

    #[test]
    fn half_speed_travels_half_as_far() {
        let lp = at_origin();
        lp.adopt(0, LEN, 0.5, false);
        assert_eq!(lp.play_pos(0, LEN), 0.0);
        assert_eq!(lp.play_pos(400, LEN), 200.0);
        // And wraps after two thousand output frames rather than one.
        assert_eq!(lp.play_pos(1999, LEN), 999.5);
        assert_eq!(lp.play_pos(2000, LEN), 0.0);
        // Recording into it is refused, because the grid is moving.
        assert!(!lp.plain());
    }

    #[test]
    fn a_negative_rate_walks_backwards_and_reappears_at_the_far_end() {
        let lp = at_origin();
        lp.adopt(0, LEN, -1.0, false);
        assert_eq!(lp.play_pos(0, LEN), 0.0);
        assert_eq!(lp.play_pos(1, LEN), 999.0);
        assert_eq!(lp.play_pos(400, LEN), 600.0);
    }

    #[test]
    fn a_pendulum_reflects_rather_than_wrapping() {
        let lp = at_origin();
        lp.adopt(0, LEN, 1.0, true);
        assert_eq!(lp.play_pos(250, LEN), 250.0);
        // Turns at the end of the audio, not at an arbitrary point...
        assert_eq!(lp.play_pos(1200, LEN), 800.0);
        // ...and so takes two lengths to come back to where it started.
        assert_eq!(lp.play_pos(2000, LEN), 0.0);
        // Never off the end, which a naive `2 * len - q` would be at the turn.
        for f in 0..4000i64 {
            let p = lp.play_pos(f, LEN);
            assert!(p >= 0.0 && p < LEN as f64, "frame {} gave {}", f, p);
        }
    }

    /// The property the whole `warp` field exists for.
    #[test]
    fn changing_speed_does_not_move_the_playhead() {
        for &(from, to) in &[
            (1.0, 0.5),
            (1.0, 2.0),
            (1.0, -1.0),
            (0.5, -2.0),
            (-1.5, 0.25),
            (2.0, 1.0),
        ] {
            for &at in &[1i64, 777, 123_456, 9_999_999] {
                let lp = at_origin();
                lp.adopt(0, LEN, from, false);
                let before = lp.play_pos(at, LEN);
                lp.adopt(at, LEN, to, false);
                let after = lp.play_pos(at, LEN);
                // Half a sample, and only when returning to rate one, where the
                // offset is folded into `origin` as a whole number of frames.
                assert!(
                    (before - after).abs() <= 0.5,
                    "x{} -> x{} at {} moved the playhead from {} to {}",
                    from, to, at, before, after
                );
            }
        }
    }

    /// Coming back to rate one has to restore the exact arithmetic, or a loop
    /// that had once been at a speed could never be recorded into again.
    #[test]
    fn returning_to_rate_one_makes_a_loop_recordable_again() {
        let lp = at_origin();
        lp.adopt(0, LEN, 0.5, false);
        lp.adopt(4321, LEN, 1.0, false);
        assert!(lp.plain(), "still carrying an offset after returning to x1");
        let p = lp.play_pos(9999, LEN);
        assert_eq!(p, p.floor());
    }

    #[test]
    fn clearing_forgets_every_resolution() {
        let lp = at_origin();
        lp.adopt(0, LEN, -0.25, true);
        lp.plainly();
        assert!(lp.plain());
        assert_eq!(lp.speed(), 1.0);
        assert!(!lp.pendulum.load(Ordering::Relaxed));
    }

    /// How long one pass lasts, which is the only number a one-shot needs and
    /// the only place in the engine that has to know a cycle can be finite.
    #[test]
    fn a_pass_lasts_as_long_as_the_speed_makes_it() {
        let lp = at_origin();
        assert_eq!(lp.pass_frames(LEN), LEN as i64);
        lp.adopt(0, LEN, 0.5, false);
        assert_eq!(lp.pass_frames(LEN), 2 * LEN as i64, "half speed, twice as long");
        lp.adopt(0, LEN, 2.0, false);
        assert_eq!(lp.pass_frames(LEN), LEN as i64 / 2);
    }

    /// Direction is not duration. Backwards takes exactly as long as forwards,
    /// which is easy to get wrong when direction lives in the sign of the number
    /// being divided by.
    #[test]
    fn backwards_takes_just_as_long_and_a_pendulum_takes_twice() {
        let lp = at_origin();
        lp.adopt(0, LEN, -1.0, false);
        assert_eq!(lp.pass_frames(LEN), LEN as i64);
        lp.adopt(0, LEN, -0.5, true);
        assert_eq!(
            lp.pass_frames(LEN),
            4 * LEN as i64,
            "there and back at half speed"
        );
    }

    /// Which pass we are on, which is what chance rolls for. Worth stating as a
    /// property because it has to keep step with `play_pos` through speed,
    /// direction and the pendulum — the two come out of one expression exactly
    /// so this cannot drift, and this is what says so.
    #[test]
    fn a_pass_is_one_trip_through_the_loop_however_long_that_takes() {
        let lp = at_origin();
        assert_eq!(lp.pass_index(0, LEN), 0);
        assert_eq!(lp.pass_index(LEN as i64 - 1, LEN), 0);
        assert_eq!(lp.pass_index(LEN as i64, LEN), 1);
        // Before `origin` is behind the loop's own beginning, and says so
        // rather than clamping to zero and claiming a pass that never ran.
        assert_eq!(lp.pass_index(-1, LEN), -1);

        // Half speed: a pass takes twice as many output frames.
        lp.adopt(0, LEN, 0.5, false);
        assert_eq!(lp.pass_index(2 * LEN as i64 - 1, LEN), 0);
        assert_eq!(lp.pass_index(2 * LEN as i64, LEN), 1);

        // A pendulum's pass is there and back, so a swinging loop set to one
        // cycle in four drops a whole round trip rather than half of one.
        let sw = at_origin();
        sw.adopt(0, LEN, 1.0, true);
        assert_eq!(sw.pass_index(2 * LEN as i64 - 1, LEN), 0);
        assert_eq!(sw.pass_index(2 * LEN as i64, LEN), 1);
    }

    /// The gate the mixer applies — `gen::<f32>() < p` — comes out at the rate
    /// the label promises.
    ///
    /// The generator itself is `rand`'s and needs no test from us; what is worth
    /// asserting is that the *gate* opens as often as the rung says, because
    /// every rung on the ladder except the first lives in the tail and a
    /// comparison written the wrong way round would still look plausible.
    #[test]
    fn a_pass_sounds_as_often_as_the_rung_says() {
        let mut rng = SmallRng::seed_from_u64(0xDEAD_BEEF_CAFE_F00D);
        const N: usize = 40_000;
        for p in [1.0f32, 0.75, 0.5, 0.25, 0.125] {
            let hits = (0..N).filter(|_| rng.gen::<f32>() < p).count() as f64 / N as f64;
            assert!(
                (hits - p as f64).abs() < 0.01,
                "at {} the gate opened {:.4} of the time",
                p,
                hits
            );
        }
    }

    /// The whole point of keeping the tail: the loop point stops being a step in
    /// the waveform.
    ///
    /// A first recording is cut, so frame `len - 1` is followed at playback by
    /// frame `0` — which is not what followed it when it was played. Here the
    /// performance is a sine whose period does not divide the loop length, so
    /// the naked splice is a large step; the fade should bring it down to
    /// roughly what one sample of the signal moves anyway.
    #[test]
    fn the_wrap_stops_being_a_step_in_the_waveform() {
        const LEN: usize = 997;
        const N: usize = 64;
        // One continuous performance, sampled past the loop's end. `head` is what
        // was kept as the loop; `tail` is what carried on.
        let x = |i: usize| (i as f32 * 0.021_37).sin();
        let head = |i: usize| x(i);
        let tail = |j: usize| x(LEN + j);

        // How much the signal moves in one sample, at its steepest. Anything
        // near this is not a discontinuity, it is the waveform.
        let natural = (1..LEN).map(|i| (x(i) - x(i - 1)).abs()).fold(0.0f32, f32::max);

        let naked = (head(0) - head(LEN - 1)).abs();
        assert!(
            naked > natural * 20.0,
            "the test signal does not actually have a bad splice: {} vs {}",
            naked,
            natural
        );

        // Now walk the wrap with the fade on, and measure the biggest step
        // anywhere across it — including back out of the fade at `p = n`.
        let faded = |p: usize| wrap_mix(head(p), tail(p), p, N);
        let mut worst = (faded(0) - head(LEN - 1)).abs();
        for p in 1..N {
            worst = worst.max((faded(p) - faded(p - 1)).abs());
        }
        worst = worst.max((head(N) - faded(N - 1)).abs());
        assert!(
            worst < natural * 2.0,
            "the crossfaded wrap still steps by {} where the signal itself moves {}",
            worst,
            natural
        );
    }

    /// And that it arrives where it should at both ends, which is what makes the
    /// continuity above hold rather than being an accident of one signal.
    #[test]
    fn a_wrap_fade_starts_on_the_continuation_and_ends_on_the_recording() {
        const N: usize = 100;
        // Head and tail held at opposite constants, so the blend is readable.
        assert!(wrap_mix(1.0, 0.0, 0, N) < 0.02, "does not start on the continuation");
        assert!(wrap_mix(1.0, 0.0, N - 1, N) > 0.98, "does not end on the recording");
        // Correlated material — the usual case, since the two ends are one
        // performance a cycle apart — keeps its level all the way through. This
        // is what linear buys and equal-power would not.
        for p in 0..N {
            assert!(
                (wrap_mix(0.7, 0.7, p, N) - 0.7).abs() < 1e-5,
                "the level moved at {}",
                p
            );
        }
    }

    /// Decay is per layer, counted from its own birth — which is the whole of
    /// what makes it sound like tape rather than like a fader. New material
    /// enters at full while everything underneath goes on receding.
    #[test]
    fn every_layer_recedes_from_its_own_beginning() {
        let lp = at_origin();
        lp.loop_len.store(LEN, Ordering::Release);
        // Six decibels a pass: a half each time round.
        lp.decay.store(10f32.powf(-6.0206 / 20.0).to_bits(), Ordering::Relaxed);
        // Layer 0 laid at the start; layer 1 laid three passes later.
        lp.set_layer_shape(0, Shape { len: LEN, tail: 0, born: 0 });
        lp.set_layer_shape(1, Shape { len: LEN, tail: 0, born: 3 });

        lp.age(3 * LEN as i64);
        assert!(
            (lp.layer_gain(0) - 0.125).abs() < 0.01,
            "three passes old should be an eighth, got {}",
            lp.layer_gain(0)
        );
        assert!(
            (lp.layer_gain(1) - 1.0).abs() < 0.01,
            "a layer laid this pass enters at full, got {}",
            lp.layer_gain(1)
        );

        // Three passes further on they have both lost the same amount, which is
        // what "from its own beginning" means.
        lp.age(6 * LEN as i64);
        assert!((lp.layer_gain(1) - 0.125).abs() < 0.01);
        assert!(lp.layer_gain(0) < lp.layer_gain(1));
    }

    /// And that turning it off brings everything back, because nothing was
    /// scaled in the arena — the whole reason it is a resolution and not an edit.
    #[test]
    fn decay_is_a_resolution_and_undoes_by_being_turned_off() {
        let lp = at_origin();
        lp.loop_len.store(LEN, Ordering::Release);
        lp.set_layer_shape(0, Shape { len: LEN, tail: 0, born: 0 });
        lp.decay.store(0.5f32.to_bits(), Ordering::Relaxed);
        lp.age(8 * LEN as i64);
        assert!(lp.layer_gain(0) < 0.01, "should have faded away by now");
        lp.decay.store(1.0f32.to_bits(), Ordering::Relaxed);
        lp.age(8 * LEN as i64);
        assert_eq!(lp.layer_gain(0), 1.0, "turning decay off must bring it back");
    }

    /// The envelope's scale is absolute and logarithmic, which is the whole of
    /// what makes the picture useful.
    ///
    /// Per-layer normalisation is what a waveform editor does, and it would
    /// destroy the one job this has: a quiet loop must not draw as tall as a
    /// loud one. Linear against full scale would be honest and useless — a take
    /// peaking at -20 dBFS is a tenth of the height and one at -40 is invisible.
    #[test]
    fn a_quieter_loop_draws_shorter_and_stays_visible() {
        assert_eq!(to_byte(0.0), 0, "silence is nothing");
        assert_eq!(to_byte(1.0), 255, "full scale is everything");
        // Twelve decibels down should be visibly shorter and still plainly
        // there. On a linear scale it would be a quarter; here it is four
        // fifths, which is what keeps forty decibels of range legible.
        let loud = to_byte(1.0) as i32;
        let quiet = to_byte(0.251) as i32; // -12 dBFS
        assert!(quiet < loud - 30, "-12 dB did not read as quieter: {}", quiet);
        assert!(quiet > loud / 2, "-12 dB fell off the picture: {}", quiet);
        // The floor is the floor, and below it there is nothing to draw.
        assert_eq!(to_byte(0.0001), 0, "-80 dBFS is under the floor");
        // Monotone, or two loudnesses could draw the same height.
        let mut last = 0u8;
        for i in 1..=100 {
            let b = to_byte(i as f32 / 100.0);
            assert!(b >= last, "not monotone at {}", i);
            last = b;
        }
    }

    /// A one-shot is silent until it is fired, and silent again after one pass.
    /// The whole mode is this comparison, so it is worth stating as a property
    /// rather than trusting to a mixer branch nobody reads twice.
    #[test]
    fn a_one_shot_sounds_only_inside_its_pass() {
        let lp = at_origin();
        lp.one_shot.store(true, Ordering::Relaxed);
        assert!(!lp.firing(0), "silent before it has ever been fired");
        // Fired at 500: audible for one pass and not a frame more.
        lp.shot_end.store(500 + lp.pass_frames(LEN), Ordering::Release);
        assert!(lp.firing(500));
        assert!(lp.firing(500 + LEN as i64 - 1));
        assert!(!lp.firing(500 + LEN as i64));
        // And a loop that is not a one-shot is never "firing", whatever is left
        // in `shot_end` from before the mode was switched off.
        lp.one_shot.store(false, Ordering::Relaxed);
        assert!(!lp.firing(500));
    }
}
