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
//! `--max-secs` and `MAX_LAYERS` are. At the defaults — eight loops, eight
//! layers, thirty seconds, 48 kHz — the arena is 351 MiB.
//!
//! **This said 46 MB until 2026-08-25**, which was true of some earlier set of
//! defaults and of nothing since; the figure beside `N_LOOPS` was right and
//! this one had simply never been recomputed. Both are now derived from the
//! same arithmetic in the comment on that constant.

use cpal::traits::{DeviceTrait, StreamTrait};
use std::error::Error;
use std::io::BufRead;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU8, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use rand::rngs::SmallRng;
use rand::{Rng, SeedableRng};

use crate::measure::{choose_input, choose_output, signed_secs, Width};

/// How deep a loop can be stacked.
///
/// **Four, down from eight on 2026-08-29**, because the arena is
/// `loops × layers × frames × channels` and layers were the cheapest of those
/// to give back: eight were never used, and halving them buys twice the loop
/// length for the same footprint. Undo and redo still walk the whole stack, so
/// the ceiling is a ceiling and not a discipline — `t` and `r` both refuse at
/// it, and say so.
pub const MAX_LAYERS: usize = 4;

/// How many bars a loop may be declared, and how sparsely a layer may sound.
///
/// **Both are the encoder's limits rather than the engine's.** Nothing here
/// would struggle with 64 of either; a Midifighter encoder over 64 steps is two
/// units a step, and this hardware moves an encoder when you press it — which
/// is a measured fact about the device, not a guess. Thirty-two gives four
/// units a step and is already the tight end. The console can ask for more than
/// a knob can reach, the same way it can with decay.
pub const MAX_BARS: usize = 32;
pub const MAX_PERIOD: usize = 32;

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

/// **One thing you can record from**: a name, and the input channels it lives
/// on.
///
/// The rig grew past one input. A stereo pedalboard is a pair of jacks; a bare
/// DI on its way out to MIDI Guitar is a third; the iPad returning over USB is
/// a fourth. `--in-ch` could name exactly one of them, and the loop had no say.
///
/// **Named, not numbered.** "Input 3" means nothing with a guitar in your
/// hands, and the name is what the ack and the encoder say back.
///
/// A mono jack is a source whose two channels are the same index. That is not
/// a special case anywhere downstream: it records the same samples twice and a
/// loop set to `mono` folds them back to one, which is what it already does
/// for a stereo source with nothing different in it.
#[derive(Clone, Debug)]
pub struct Source {
    pub name: String,
    pub ch: [usize; CHANNELS],
}

impl Source {
    pub fn mono(name: &str, ch: usize) -> Self {
        Source { name: name.to_string(), ch: [ch, ch] }
    }
    pub fn is_mono(&self) -> bool { self.ch[0] == self.ch[1] }
    pub fn describe(&self) -> String {
        if self.is_mono() {
            format!("{} (in {})", self.name, self.ch[0] + 1)
        } else {
            format!("{} (in {}+{})", self.name, self.ch[0] + 1, self.ch[1] + 1)
        }
    }
}

pub struct Opts {
    pub device: String,
    pub in_ch: usize,
    /// What a loop can record from. Empty means "just `--in-ch`, as before",
    /// which is what an existing command line gets.
    pub sources: Vec<Source>,
    pub out_ch: usize,
    pub residual: f64,
    /// Whether `--residual` was actually given, as against left at its default.
    ///
    /// The default is not "no compensation", it is a number — so without this
    /// the engine cannot tell an operator who measured 252 from one who never
    /// looked, and cannot say which it is doing.
    pub residual_given: bool,
    /// The longest a single loop may become, and so the stride of every layer
    /// slot in the arena.
    ///
    /// **Five minutes, up from thirty seconds.** Thirty was 15 bars at 120, so
    /// the bars knob's top half was a refusal and "grab 16 bars" was not a
    /// gesture the engine could perform — the pre-roll remembered the audio and
    /// there was nowhere to put it. The arena is reserved rather than resident:
    /// measured 2026-08-29, a 703 MiB arena sat at 69 MiB RSS, because a page
    /// nobody has recorded into is never touched. So the cost of the ceiling is
    /// address space, and the cost of *using* it is paid a layer at a time by
    /// whoever uses it.
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
            sources: Vec::new(),
            out_ch: 0,
            residual: 252.0,
            residual_given: false,
            max_secs: 300.0,
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

/// A loop's level as the board says it. Silence is a word, not a number:
/// "-inf dB" is a thing a meter says, not a thing a person does.
fn vol_words(lp: &Loop) -> String {
    let g = f32::from_bits(lp.vol.load(Ordering::Relaxed));
    if g <= 0.0 {
        return "is turned all the way down".into();
    }
    if g >= 1.0 {
        return "plays at full level".into();
    }
    format!("plays {:.1} dB down", -20.0 * g.max(1e-9).log10())
}

/// The tape's bandwidth as the board says it.
fn tone_words(lp: &Loop) -> String {
    let hz = f32::from_bits(lp.tone.load(Ordering::Relaxed));
    if hz >= 20_000.0 {
        return "keeps every pass exactly as bright".into();
    }
    format!("loses everything above {:.1} kHz each pass", hz / 1000.0)
}

/// The Revox feedback as the board says it.
fn fb_words(lp: &Loop) -> String {
    let g = f32::from_bits(lp.fb.load(Ordering::Relaxed));
    if g <= 0.0 {
        return "nothing".into();
    }
    if g >= 1.0 {
        return "everything".into();
    }
    format!("{:.0} dB down", -20.0 * g.max(1e-9).log10())
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
/// **Eight since 2026-08-25, and it was six for the MC6's sake.** The original
/// reason was that the pedal has six main switches and the design rests on one
/// switch owning one loop. That reasoning inverted when the web page became the
/// reference surface and the Midifighter Twister a second controller: the loop
/// count comes from the instrument, and the foot reaches what it can. Eight
/// fills the top two rows of the Twister's 4×4, and loops 7 and 8 are simply
/// not on the pedal. See `docs/DESIGN-TWISTER.md` §5.
///
/// Nothing on the wire changed: `dispatch` picks the loop from a single leading
/// digit, so 0–7 still fits.
///
/// The cost is linear and paid at startup: the arena is
/// `N_LOOPS × MAX_LAYERS × max_secs × 4 bytes`, so eight loops of eight layers
/// at the default thirty seconds and 48 kHz is **351 MiB**, up from 263. It is
/// allocated once and never touched by the allocator again.
pub const N_LOOPS: usize = 8;

/// **Two, everywhere.** The arena, the pre-roll rings and every layer are
/// stereo as of 2026-08-29.
///
/// The engine was mono end to end: `--in-ch` named one channel and the others
/// were discarded — not summed, *dropped* — and `pan_gains` placed that one
/// signal in the field. Which is right for a guitar into a jack and wrong for
/// most of what this rig makes: a stereo pedalboard, a ping-pong delay, a wide
/// reverb, a drum machine. Half of each was never reaching the machine at all.
///
/// The cost is linear and paid at startup: the arena doubles, to 702 MiB at the
/// default eight loops, eight layers, thirty seconds and 48 kHz. `--max-secs`
/// is the dial if that is too much.
///
/// **Mono stopped being a storage decision and became a playback one.** A loop
/// whose channels carry nothing different can be folded at the mix — see
/// `Loop::mono` — which makes it instantly reversible, and means nothing is
/// thrown away by a choice you had to get right before the take.
pub const CHANNELS: usize = 2;

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
    /// **How many bars this loop is**, and the only place metre enters a loop.
    ///
    /// `loop_len` is frames and always has been; this says what those frames
    /// mean. `loop_len == cycles * bar` is the invariant everything else leans
    /// on, and it is what lets one loop be four bars while another is one
    /// without either of them being "the grid".
    ///
    /// One thing it does that nothing else can: on the **anchor**, with no
    /// clock, it divides the pulse. Record a phrase, say it was four bars, and
    /// the bar becomes a quarter of it — which is the only way a clockless
    /// session gets a loop shorter than its first take. See `Shared::loop_grid`.
    ///
    /// Zero means "not declared", which reads as one everywhere. Kept distinct
    /// from one so a loop that has never been told anything can be told
    /// something by the first thing that measures it.
    pub cycles: AtomicUsize,
    /// The output frame a running **first** recording should close itself at,
    /// or `i64::MIN` for "wait for a press".
    ///
    /// Set only when the loop already knew its length before recording began —
    /// which, once there is a clock or a declared bar count, is every recording
    /// after the very first one of a clockless session. That is the whole point
    /// of it: the second press exists because the engine did not know how long
    /// you meant, and as soon as it does, asking for it is ceremony.
    ///
    /// Read by `closer`, not by the callback, because closing a recording draws
    /// a layer and sleeps and neither belongs in an audio thread.
    pub close_at: AtomicI64,
    /// The length a running **first** recording was told to be, or zero for
    /// "whatever gets captured".
    ///
    /// **`commit` measures, and a declared loop must not be measured.** Its
    /// fallback is `reached` — the frames the input actually delivered — which
    /// trails the output by `K` and by the drain `commit` sleeps for, so a loop
    /// told it was one bar came back 26 ms short of one. Sonically nothing; on
    /// the grid it is a loop that walks away from every other loop a pass at a
    /// time, and the cause is invisible because the take sounds right.
    ///
    /// So the number that was asked for wins over the number that was counted.
    /// Only for a length that was declared *before* a note was played — a free
    /// take still gets exactly what it captured, because there nothing was
    /// asked for.
    pub rec_len: AtomicUsize,
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
    /// Per loop, which is what lets eight loops of different lengths run at once
    /// without any of them being the master. Whether they *should* be free of
    /// each other is a musical question, and the answer is a quantisation
    /// policy applied when a loop closes — not a shared origin, which would
    /// decide it here and for ever.
    pub origin: AtomicI64,
    /// Silenced, but still turning.
    ///
    /// **Phase-locked, deliberately.** The playhead keeps advancing while a loop
    /// is stopped, so bringing it back is not "start again" but "become audible
    /// again, where you would have been". With eight loops that is the only
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
    /// **Which input this loop records from.** An index into `Shared::sources`.
    ///
    /// Per loop rather than per rig, because `ClaimPast` decides *afterwards*
    /// which loop a moment belongs to — so the moment has to have been captured
    /// on every source, and the loop says which one it wants when it takes it.
    ///
    /// Survives a clear, like the other things that describe how you work
    /// rather than what is in the loop. Clearing a slot you had pointed at the
    /// drum machine and finding it back on the guitar would be a surprise in
    /// the middle of the one gesture that is supposed to be a fresh start.
    pub src: AtomicUsize,
    /// **Fold this loop's two channels together at playback.**
    ///
    /// A playback decision and deliberately not a capture one: the audio is
    /// always kept in stereo, so this is instantly reversible and costs nothing
    /// to try. On, the two channels are summed and `pan` is a true pan — which
    /// is what you want for a source with no meaningful stereo content and a
    /// place you want it to sit. Off, they pass through and `pan` is a balance.
    ///
    /// Andrew asked for this as a capture-time option; at playback it is
    /// strictly better, because nothing is thrown away by a choice made before
    /// the take.
    pub mono: AtomicBool,
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
    /// This loop's own level, as a linear gain. `1.0` is unity, which is where
    /// every loop starts.
    ///
    /// **Added 2026-08-25, and the engine went without it for a reason that
    /// stopped being true.** A looper whose loops are either in or out needs no
    /// faders: mute says everything. What changed is the Twister — eight loops
    /// with a knob each, and the first thing a hand does with a knob is set how
    /// loud something is. `chance` was standing in for it and is not a level;
    /// it is a gate on whole passes.
    ///
    /// A resolution at playback like speed, pan and decay: nothing is scaled in
    /// the arena, so turning a loop down and back up loses nothing.
    ///
    /// Multiplied into the pan gains once per buffer, so it costs nothing in
    /// the frame loop.
    pub vol: AtomicU32,
    /// The envelope of the recording **that is happening right now**, as
    /// `ENV_BUCKETS` bytes on the same absolute -60 dBFS scale as the committed
    /// ones.
    ///
    /// **Atomics rather than the `env` mutex**, because this is written from the
    /// audio callback and that one is not. `rebuild_env` runs on the command
    /// path at commit and can afford a lock; a live picture cannot, and a
    /// callback that blocks on a mutex is a callback that eventually misses a
    /// buffer.
    ///
    /// Empty of meaning while nothing is recording — cleared when a recording
    /// starts rather than when it ends, so what you see is always the take in
    /// hand and never the last one.
    pub rec_env: Vec<AtomicU8>,
    /// **Revox mode: the loop is a tape, and an overdub writes over it.**
    ///
    /// Everywhere else in this engine a pass is non-destructive — layers are
    /// kept whole and `decay` is a *resolution* applied at playback, which is
    /// why turning decay off brings a faded loop back. That is the right
    /// default and it is not what a tape does. Two Revoxes with the second one
    /// feeding back below unity erase as they record: what is under the head
    /// comes back quieter each time round, and there is no version of it that
    /// was not erased.
    ///
    /// So this is a mode you opt into, and the price is stated rather than
    /// hidden: **undo goes away**, because there is nothing kept to go back to.
    /// Entering flattens the loop to one layer — a tape has no layers — and
    /// leaving does not unflatten it.
    /// Whether this loop's one layer is a **threaded empty tape** — a length
    /// with nothing played onto it yet.
    ///
    /// The distinction `n_layers` cannot make. A threaded tape has one layer so
    /// that it *plays* (see `blank`), which makes it indistinguishable from a
    /// recorded loop by layer count alone — and that made the length knob a
    /// one-shot: the first turn threaded eight seconds and every turn after was
    /// refused as "there is something in it". There is not. Adjusting the length
    /// of a tape you have not played onto is exactly how you choose a length.
    ///
    /// Cleared the moment anything is recorded, which is the moment resizing
    /// would become a trim.
    pub threaded: AtomicBool,
    pub revox: AtomicBool,
    /// What a Revox pass leaves of what was under it, as a linear gain. `1.0`
    /// is a tape that never erases; `0.0` is one that replaces.
    ///
    /// **Its own value rather than `decay`'s**, deliberately. They are the same
    /// musical idea by two mechanisms — one destroys and one does not — and a
    /// single number meaning "resolution here, erase-head there" depending on a
    /// flag is the kind of overload this codebase spends whole comments
    /// regretting. `dec` still works in Revox mode and still does what it always
    /// did.
    pub fb: AtomicU32,
    /// How much top the tape keeps, as a corner frequency in hertz.
    ///
    /// **Tape loses the high end before it loses the level**, and losing only
    /// the level is what makes a digital feedback loop sound like a digital
    /// feedback loop: the last repeat is the first one, quieter, with every
    /// edge still on it. A pass over a real head comes back a little duller, and
    /// twenty passes come back as a wash.
    ///
    /// One pole, applied to what is already on the tape as the head goes over
    /// it. Not a simulation of anything — no head bump, no wow, no hiss — and
    /// that is the point: the whole of the effect is that each pass costs you a
    /// little of the top, and each pass costs it again.
    ///
    /// **In Revox only, and that is a fact about the design rather than a
    /// shortcut.** Outside it, `decay` is a *resolution* applied at playback
    /// with nothing in the arena touched, which is what lets a faded loop come
    /// back — a filter there would have to be a different filter per layer per
    /// pass count, cascaded as deep as the loop is old. Here the erasing has
    /// already happened, so darkening it is one multiply and it is permanent
    /// for the same reason everything else in this mode is.
    ///
    /// At or above 20 kHz it is bypassed rather than approximated, so "off" is
    /// off and not "very nearly".
    pub tone: AtomicU32,
    /// The one-pole's memory, carried across buffers and across the wrap —
    /// which is right, because the head does not stop at the splice.
    pub tape_lp: AtomicU32,
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
            cycles: AtomicUsize::new(0),
            close_at: AtomicI64::new(i64::MIN),
            rec_len: AtomicUsize::new(0),
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
            src: AtomicUsize::new(0),
            mono: AtomicBool::new(false),
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
            vol: AtomicU32::new(1.0f32.to_bits()),
            rec_env: (0..ENV_BUCKETS).map(|_| AtomicU8::new(0)).collect(),
            threaded: AtomicBool::new(false),
            revox: AtomicBool::new(false),
            fb: AtomicU32::new(10f32.powf(-3.0 / 20.0).to_bits()),
            tone: AtomicU32::new(6500.0f32.to_bits()),
            tape_lp: AtomicU32::new(0.0f32.to_bits()),
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

    /// Everything a clear forgets about one loop.
    ///
    /// Lifted out of the `c` arm of `dispatch` because it had grown to twenty
    /// lines in the middle of a very long match, and a list that long inside a
    /// match arm is a list nothing can test. It was missing `quant`: measured
    /// on the running daemon 2026-08-24, every other mode reset across a clear
    /// and `grid` stayed lit, so a cleared slot silently waited for the next
    /// bar before it began recording — a surprise you diagnose as a broken
    /// footswitch rather than as a setting.
    ///
    /// The rule this encodes: **a cleared slot has nobody's habits.** A loop
    /// that came back at half speed, backwards, hard left, firing once and
    /// waiting for a sound would be a haunting, and the switch that cleared it
    /// said nothing about any of that.
    ///
    /// Audio-side clearing — layer shapes, the envelope, the anchor — stays
    /// with the caller, which is the only thing holding `Shared`.
    fn cleared(&self) {
        self.state.set(IDLE);
        // An empty loop that is still silenced would refuse to record audibly
        // for a reason nothing on screen could explain.
        self.muted.store(false, Ordering::Relaxed);
        // And for the same reason, at full level. A cleared slot sitting at
        // -58 dB is silenced by a different mechanism and looks identical from
        // outside — which is exactly how it was found: a loop recorded happily
        // into a cleared slot and made no sound, and `Clear All` did not fix it
        // because clearing was the thing that had failed to reset it.
        self.vol.store(1.0f32.to_bits(), Ordering::Relaxed);
        // A cleared slot is not still a tape. The feedback amount survives,
        // like the other settings that describe how you work rather than what
        // is in the loop.
        self.revox.store(false, Ordering::Relaxed);
        self.threaded.store(false, Ordering::Relaxed);
        // The filter's memory is audio, not a setting: it goes with the audio.
        // `tone` and `fb` describe how you work and stay.
        self.tape_lp.store(0.0f32.to_bits(), Ordering::Relaxed);
        self.plainly();
        self.pan.store(64, Ordering::Relaxed);
        self.one_shot.store(false, Ordering::Relaxed);
        self.shot_end.store(i64::MIN, Ordering::Release);
        self.level_arm.store(false, Ordering::Relaxed);
        self.arm_from.store(i64::MIN, Ordering::Release);
        self.quant.store(false, Ordering::Relaxed);
        self.fade.store(0, Ordering::Relaxed);
        self.decay.store(1.0f32.to_bits(), Ordering::Relaxed);
        self.chance.store(1.0f32.to_bits(), Ordering::Relaxed);
        self.chance_pass.store(i64::MIN, Ordering::Relaxed);
        self.chance_sounds.store(true, Ordering::Relaxed);
        self.n_layers.store(0, Ordering::Release);
        self.redo_to.store(0, Ordering::Release);
        self.loop_len.store(0, Ordering::Release);
        // **Everything that says how long this loop is, together.**
        //
        // `loop_len` went to zero here from the beginning and `cycles` did not,
        // which was harmless while a bar count could only come from a recording
        // — the two were made and destroyed at the same moment. `len<n>` broke
        // that: it sizes an *empty* loop, so after a clear this slot said "no
        // length" and "four bars" at the same time.
        //
        // What that cost is worth writing down, because it looked like an
        // engine fault and was not. The Twister's ring is drawn from `cycles`,
        // so a cleared loop still showed four bars — and the app writes ring
        // positions back to the device, so the encoder physically sat at four.
        // Turning it "to 4" was then impossible: it was already there, no CC
        // moved, no `len4` was sent, and the next take recorded open-ended. The
        // second run of a recipe failed while the first one worked, which is the
        // signature of state that outlives the thing it describes.
        //
        // The same argument reaches `close_at` and `rec_len`: both describe a
        // recording that is no longer going to happen, and a stale `close_at`
        // is a timer pointed at a take nobody has played yet.
        self.cycles.store(0, Ordering::Release);
        self.close_at.store(i64::MIN, Ordering::Release);
        self.rec_len.store(0, Ordering::Release);
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
        let p = self.pan_position();
        let theta = p * std::f32::consts::FRAC_PI_2;
        (theta.cos(), theta.sin())
    }

    /// The same knob, read as a **balance** — for a loop that is already two
    /// channels and is not being folded.
    ///
    /// Equal-power panning is for *placing a signal*. Applied to a stereo pair
    /// it does two wrong things at once: at centre it takes 3 dB off both sides
    /// for no reason, and turning it collapses a field that was recorded rather
    /// than inventing one. What the knob should mean there is what it means on
    /// a mixer: leave one side alone and take the other down.
    ///
    /// So: unity both sides at centre, and one side falling linearly to silence
    /// at the end of the travel. Attenuating only — no side is ever boosted, so
    /// a balanced loop can never be louder than the loop that was recorded, and
    /// there is no headroom to lose.
    pub fn balance_gains(&self) -> (f32, f32) {
        let p = self.pan_position();
        ((2.0 * (1.0 - p)).min(1.0), (2.0 * p).min(1.0))
    }

    /// The knob's travel as a fraction, with **the detent at exactly a half**.
    ///
    /// It was `v / 127.0`, which cannot put centre in the middle: 127 is odd,
    /// so 64 lands on 0.5039 and a centred loop came out 0.07 dB down on the
    /// left. Inaudible, and it stayed unnoticed for exactly that reason — but
    /// export writes these gains *into the file*, and a stereo take whose
    /// centre is not centred is the sort of tilt that gets chased later in
    /// somebody else's mixer.
    ///
    /// So the two halves of the travel are scaled separately, which costs a
    /// slope change of one part in 128 at the detent and buys an exact middle,
    /// an exact hard left and an exact hard right.
    fn pan_position(&self) -> f32 {
        let v = self.pan.load(Ordering::Relaxed).min(127) as f32;
        if v <= 64.0 {
            v / 128.0
        } else {
            0.5 + (v - 64.0) / 126.0
        }
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
    /// Forget the live picture. Called when a recording *starts*.
    pub fn clear_rec_env(&self) {
        for b in self.rec_env.iter() {
            b.store(0, Ordering::Relaxed);
        }
    }

    /// The live picture, or empty when nothing is being recorded — which the
    /// caller decides, because only it knows the state.
    pub fn rec_env_bytes(&self) -> Vec<u8> {
        self.rec_env.iter().map(|b| b.load(Ordering::Relaxed)).collect()
    }

    /// Raise one bucket to a peak. `fetch_max` rather than a store: a bucket
    /// spans hundreds of frames and the loudest of them is the one worth
    /// drawing, which is the same thing `rebuild_env` does with a `max` over a
    /// range.
    pub fn mark_rec_env(&self, bucket: usize, peak: f32) {
        if let Some(b) = self.rec_env.get(bucket) {
            b.fetch_max(to_byte(peak), Ordering::Relaxed);
        }
    }

    pub fn layer_env(&self, layer: usize) -> Vec<u8> {
        self.env
            .lock()
            .map(|e| e[layer].clone())
            .unwrap_or_default()
    }
    pub fn layer_gain(&self, layer: usize) -> f32 {
        f32::from_bits(self.l_gain[layer].load(Ordering::Relaxed))
    }

    /// The pass this layer was laid on. Reported so a quiet layer can say why
    /// it is quiet: `gain` alone shows that it has receded and not how far back
    /// it started, and the difference between "born three passes ago" and "born
    /// with the loop" is the whole of what per-layer decay means.
    pub fn layer_born(&self, layer: usize) -> i64 {
        self.l_born[layer].load(Ordering::Relaxed)
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
    /// One ring for every loop, because there is one input. Which loop a
    /// retroactive take lands in is a decision made when `t` is pressed, not
    /// something the capture has to anticipate.
    /// **One ring per source, and every one of them always filling.**
    ///
    /// It was a single ring, because there was a single input. Now there is a
    /// source per thing you can record from, and each keeps its own last
    /// `ring_secs` — which is what `ClaimPast` needs to stay honest. The ring
    /// exists so that you need not decide in advance; a *global* input selector
    /// would put that decision straight back in front of you, and the one time
    /// it mattered would be the time you were on the wrong input.
    ///
    /// Indexed `(src * ring_len + frame % ring_len) * CHANNELS + ch`. The cost
    /// is 11.5 MB a source a channel at the default sixty seconds, against an
    /// arena of hundreds — which is why "all of them, always" is affordable.
    ring: Vec<AtomicU32>,
    ring_len: usize,
    /// What each source is called and which input channels it reads.
    pub sources: Vec<Source>,
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
    /// The loudest thing each source has heard since the last poll. Per source,
    /// because the arm threshold is answered from the loop's *own* input — a
    /// drum loop should wait for a drum and a guitar loop for a guitar, and one
    /// shared peak would have each of them starting on the other.
    pub in_peak: Vec<AtomicU32>,
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
    /// **The join, done.** A bar's length in frames, and an output frame on
    /// which some bar began.
    ///
    /// Derived in `link.rs` at the moment an anchor lands, because that is the
    /// only place all four halves are in scope at once: the beat position, the
    /// frame counter, the tempo and the sample rate. `grid` reads these and
    /// nothing else, which is what lets it stay a method on `Shared` with no
    /// arguments.
    ///
    /// `link_bar_origin` may be in the past or, briefly, in the future — it is
    /// a phase, not an event, and every bar line is `origin + n * frames` for
    /// any integer `n`. Zero frames means no usable clock.
    ///
    /// **How accurate this is, honestly.** The anchor's beat position belongs
    /// to the moment link-spike sent it; the frame belongs to the moment we
    /// received it. Between them is a UDP hop on the loopback and one trip
    /// through the OSC decoder — well under a millisecond, and small against a
    /// bar. It is not sample-accurate and does not claim to be. What it is
    /// accurate enough for is deciding which side of a bar line a foot landed.
    pub link_bar_frames: AtomicUsize,
    pub link_bar_origin: AtomicI64,
    /// **What a launch waits for**, in beats. Rig-wide, the way Ableton's is.
    ///
    /// `-1` is a bar and is the default, because that is what the grid has
    /// always meant here and a looper with no opinion should behave the way it
    /// did yesterday. `0` is none — nothing waits, whatever a loop's own `g`
    /// says. Anything above zero is that many beats, so a quarter of a bar and
    /// eight bars are the same setting at different values and neither is a
    /// special case.
    ///
    /// **Separate from the bar on purpose.** The bar is what a *length* is
    /// counted in; this is what a *start* waits for. A DAW keeps them apart and
    /// so does this, because "close on a whole bar" and "start on the next
    /// beat" are both things you want at once — and collapsing them would take
    /// away free-length takes over a quantised rig.
    pub launch_q: AtomicI64,
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

/// Which output frame the bar containing an anchor began on.
///
/// The other half of the join `bar_frames` could not make on its own. `beat` is
/// Link's beat position at the moment the anchor was taken and `at` is the
/// output frame we were on when it landed; a bar is `quantum` beats, so the bar
/// this anchor sits in began `beat mod quantum` beats ago.
///
/// Signed, and may be negative: an anchor arriving in the first bar of a
/// session names a frame before the stream started, which is correct — it is a
/// phase, not an event, and every bar line is this plus a multiple of the bar.
pub fn bar_origin(beat: f64, quantum: f64, tempo_bpm: f64, at: usize, sr: u32) -> i64 {
    let per_beat = 60.0 / tempo_bpm * sr as f64;
    let into_bar = beat.rem_euclid(quantum) * per_beat;
    at as i64 - into_bar.round() as i64
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

    /// **The bar.** Link's when there is a clock, the first loop's cycle when
    /// there is not.
    ///
    /// This used to be only the second half, and said so: *tempo alone gives a
    /// bar's length but not where the bar falls, so until the frame-to-wall-
    /// clock join lands the grid the engine can honestly offer is another
    /// loop's cycle.* The join landed — see `link_bar_origin` — so the honest
    /// answer is now the better one.
    ///
    /// The order matters and it is not arbitrary. A looper with no clock has
    /// always worked the other way round: the thing you played first is the
    /// thing everything else fits around. But that makes the pulse and the
    /// first loop's *length* the same number, and then **no loop can ever be
    /// shorter than the first one** — you cannot put a one-bar kick under a
    /// four-bar phrase, because four bars is what "one cycle" means. With a
    /// clock the bar is a fact about the rig rather than about loop one, and
    /// length becomes a count of bars, which is the thing a musician was
    /// counting anyway.
    ///
    /// The fallback is not a lesser mode, it is the same model with the bar
    /// taken from the only other thing that knows one. And a first loop can be
    /// *told* it was four bars after the fact (`len`), which divides the pulse
    /// and gets the short loop back without a clock.
    pub fn grid(&self) -> Option<(i64, usize)> {
        let bar = self.link_bar_frames.load(Ordering::Relaxed);
        let played = self.loop_grid();
        if bar > 0 {
            // **Length from the clock, phase from the music.** Link knows how
            // long a bar is far better than a looper can; where the *downbeat*
            // falls it knows only as well as a UDP hop allows, and the moment
            // anything has been recorded there is a better answer in the room.
            //
            // This is the priority the old comment on this function already
            // stated — *the loops agreeing with each other is the point, and
            // agreeing with Ableton is a bonus* — and it is what makes
            // arm-record define the downbeat: play the first loop free and the
            // note you played becomes bar one, with Link still supplying the
            // tempo. Record it on the grid instead and its origin is a Link bar
            // line already, so nothing moves.
            let origin = match played {
                Some((o, _)) => o,
                None => self.link_bar_origin.load(Ordering::Relaxed),
            };
            return Some((origin, bar));
        }
        played
    }

    /// The grid a *launch* aligns to: the bar, subdivided or multiplied by
    /// whatever `launch_q` asks for, and `None` when nothing should wait.
    ///
    /// Beats rather than fractions of a bar, so the setting means the same
    /// thing in 3/4 as in 4/4 — a quantum of three does not make "one beat"
    /// into a third of a bar, it stays a beat.
    fn launch_grid(&self) -> Option<(i64, usize)> {
        let (origin, bar) = self.grid()?;
        match self.launch_q.load(Ordering::Relaxed) {
            0 => None,
            n if n < 0 => Some((origin, bar)),
            n => {
                let quantum = f64::from_bits(self.link_quantum.load(Ordering::Relaxed));
                // With no clock there is no beat, only the bar — so a beat count
                // is honoured as a fraction of the bar in four, which is the
                // metre everything here assumes when nothing tells it otherwise.
                let beats = if quantum >= 1.0 { quantum } else { 4.0 };
                let step = ((bar as f64 / beats) * n as f64).round() as usize;
                Some((origin, step.max(1)))
            }
        }
    }

    /// The grid the anchor loop offers: its origin and its **bar**, which is
    /// its cycle divided by however many bars it has been declared to be.
    ///
    /// One is the ordinary case and divides by nothing. Anything else is a loop
    /// that has been told what it was — see the `len` verb — and is how a
    /// clockless session gets a pulse shorter than its first take.
    fn loop_grid(&self) -> Option<(i64, usize)> {
        let a = self.anchor.load(Ordering::Acquire);
        if a >= N_LOOPS {
            return None;
        }
        let lp = self.lp(a);
        let len = lp.loop_len.load(Ordering::Acquire);
        if len == 0 {
            return None;
        }
        let bars = lp.cycles.load(Ordering::Acquire).max(1);
        Some((lp.origin.load(Ordering::Acquire), (len / bars).max(1)))
    }

    /// The first output frame at or after `from` that a launch may happen on.
    ///
    /// The bar, unless `launch_q` says otherwise — see it for why the two are
    /// different questions. `None` means nothing to wait for, and every caller
    /// already treats that as "go now".
    pub fn next_boundary(&self, from: i64) -> Option<i64> {
        let (origin, len) = self.launch_grid()?;
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
    /// **Interleaved, not planar.** The two channels of a frame sit next to
    /// each other because that is how the mix reads them — one `loop_at` wants
    /// both — so a stereo frame is one cache line's work rather than two walks
    /// a `max_frames` apart.
    fn cell(&self, li: usize, layer: usize, pos: usize, ch: usize) -> &AtomicU32 {
        &self.arena[((li * MAX_LAYERS + layer) * self.max_frames + pos) * CHANNELS + ch]
    }
    fn read(&self, li: usize, layer: usize, pos: usize, ch: usize) -> f32 {
        f32::from_bits(self.cell(li, layer, pos, ch).load(Ordering::Relaxed))
    }
    fn write(&self, li: usize, layer: usize, pos: usize, ch: usize, v: f32) {
        self.cell(li, layer, pos, ch).store(v.to_bits(), Ordering::Relaxed)
    }
    fn add(&self, li: usize, layer: usize, pos: usize, ch: usize, v: f32) {
        let c = self.cell(li, layer, pos, ch);
        let cur = f32::from_bits(c.load(Ordering::Relaxed));
        c.store((cur + v).to_bits(), Ordering::Relaxed)
    }
    /// The captured sample for an input frame, if the ring still holds it.
    fn ring_at(&self, src: usize, in_frame: i64, ch: usize) -> Option<f32> {
        if in_frame < 0 || src >= self.sources.len() {
            return None;
        }
        let newest = self.in_frames.load(Ordering::Acquire) as i64;
        // Leave a buffer's grace at the trailing edge: the input callback is
        // still writing, and a frame about to be overwritten is not a frame.
        let oldest = newest - self.ring_len as i64 + self.buffer_frames.load(Ordering::Relaxed) as i64;
        if in_frame < oldest || in_frame >= newest {
            return None;
        }
        let i = (src * self.ring_len + (in_frame as usize) % self.ring_len) * CHANNELS + ch;
        Some(f32::from_bits(self.ring[i].load(Ordering::Relaxed)))
    }

    /// Which source a loop records from, clamped to one that exists.
    ///
    /// Clamped rather than trusted: a `src<n>` for a source nobody configured
    /// would otherwise index a ring that is not there, and silently recording
    /// nothing is the failure this engine spends most of its comments avoiding.
    pub fn src_of(&self, li: usize) -> usize {
        let n = self.sources.len().max(1);
        self.lp(li).src.load(Ordering::Relaxed).min(n - 1)
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
    fn sample_at(&self, li: usize, layer: usize, pos: usize, ch: usize) -> f32 {
        let lp = self.lp(li);
        let Some(p) = lp.layer_pos(layer, pos) else {
            return 0.0;
        };
        let v = self.read(li, layer, p, ch);
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
        wrap_mix(v, self.read(li, layer, len + p, ch), p, n)
    }

    fn zero_layer(&self, li: usize, layer: usize) {
        for i in 0..self.max_frames {
            for ch in 0..CHANNELS {
                self.cell(li, layer, i, ch).store(0, Ordering::Relaxed);
            }
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
                    // **One picture for both channels**, and the louder of the
                    // two. A waveform is read to answer "is there anything
                    // there and where does it stop", and two overlaid traces
                    // answer it worse than one.
                    for ch in 0..CHANNELS {
                        peak = peak.max(self.read(li, layer, p, ch).abs());
                    }
                }
                out.push(to_byte(peak));
            }
        }
        if let Ok(mut e) = lp.env.lock() {
            e[layer] = out;
        }
    }

    /// Fold every layer into one, at the gains they are being heard at.
    ///
    /// **A tape has no layers**, which is the whole of why entering Revox does
    /// this. Leaving them stacked and writing over layer zero would erase one
    /// voice of several and leave the rest untouched — a mode that only half
    /// applies, which is worse than either.
    ///
    /// Folded at each layer's *current* decay gain, so what you hear the instant
    /// before is what you hear the instant after. That does mean decay stops
    /// being undoable for the material folded in: it has been resolved into the
    /// tape. Said out loud on the verb, because it is the moment a loop stops
    /// being recoverable.
    fn flatten(&self, li: usize, at: i64) {
        let lp = self.lp(li);
        let n = lp.n_layers.load(Ordering::Acquire);
        let len = lp.loop_len.load(Ordering::Acquire);
        if len == 0 || n == 0 {
            return;
        }
        if n > 1 {
            for p in 0..len {
                for ch in 0..CHANNELS {
                    let mut v = 0.0f32;
                    for l in 0..n {
                        v += self.read(li, l, p, ch) * lp.layer_gain(l);
                    }
                    self.write(li, 0, p, ch, v);
                }
            }
            for l in 1..n {
                self.zero_layer(li, l);
                lp.set_layer_shape(l, Shape { len: 0, tail: 0, born: 0 });
            }
        }
        // Born now: the tape is one age, and the age it is starts here.
        lp.set_layer_shape(0, Shape { len, tail: 0, born: lp.pass_index(at, len) });
        lp.n_layers.store(1, Ordering::Release);
        self.rebuild_env(li, 0);
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
    ///
    /// ## `live`, and the line it draws
    ///
    /// Three of the things below do not shape the audio, they decide whether
    /// you hear it *this time round*: chance rolls per pass, a one-shot is
    /// silent between fires, and mute is a hand on the fader. Everything else
    /// here — layer gain, decay, speed, direction, the pendulum, where a sparse
    /// layer lands — is the sound itself.
    ///
    /// The output callback wants both and passes `true`. **Export wants only
    /// the first kind and passes `false`**, because a rendered file that had
    /// baked in one roll of the dice would be a performance rather than a loop,
    /// and every receiver that file is going to — Ableton, Loopy, Morphagene,
    /// Lubadh — can do chance and one-shot itself. What we do not render, we
    /// record in the manifest instead.
    fn loop_at(&self, li: usize, out_frame: i64, rng: &mut SmallRng, live: bool) -> [f32; CHANNELS] {
        let lp = self.lp(li);
        let len = lp.loop_len.load(Ordering::Acquire);
        if len == 0 {
            return [0.0; CHANNELS];
        }
        // Silenced but not stopped: `pos` below is still computed from `origin`
        // on every frame, so nothing drifts while a loop is quiet.
        if live && lp.muted.load(Ordering::Relaxed) {
            return [0.0; CHANNELS];
        }
        // A one-shot sounds only inside a pass. Before the first fire `shot_end`
        // is `i64::MIN`, so turning the mode on silences the loop at once — which
        // is right, and is why the ack says so: a one-shot that kept playing
        // until its next fire would be a loop in two minds.
        if live && lp.one_shot.load(Ordering::Relaxed) && !lp.firing(out_frame) {
            return [0.0; CHANNELS];
        }
        let n = lp.n_layers.load(Ordering::Acquire);
        if n == 0 {
            return [0.0; CHANNELS];
        }
        // Chance: one roll per pass, held for the whole pass.
        //
        // The roll has to happen here, because this is the only place that knows
        // the frame and so the only place that can turn a loop on and off *at* a
        // cycle boundary rather than within a buffer of one. Remembering which
        // pass it was for is what keeps a one-in-four loop from flickering at
        // sample rate.
        if live && lp.chance_applies() {
            let p = lp.chance_of();
            let pass = lp.pass_index(out_frame, len);
            if lp.chance_pass.load(Ordering::Relaxed) != pass {
                lp.chance_pass.store(pass, Ordering::Relaxed);
                lp.chance_sounds.store(rng.gen::<f32>() < p, Ordering::Relaxed);
            }
            if !lp.chance_sounds.load(Ordering::Relaxed) {
                return [0.0; CHANNELS];
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
        let a = self.mix_at(li, n, p0);
        let b = self.mix_at(li, n, p1);
        let mut out = [0.0f32; CHANNELS];
        for ch in 0..CHANNELS {
            out[ch] = a[ch] * (1.0 - f) + b[ch] * f;
        }
        out
    }

    /// Every layer of one loop, summed at one integer loop position.
    ///
    /// Split out because interpolation needs the same question asked at two
    /// neighbouring positions, and summing the layers first is the same number
    /// as interpolating each layer and summing after — for half the reads.
    fn mix_at(&self, li: usize, n: usize, pos: usize) -> [f32; CHANNELS] {
        let lp = self.lp(li);
        let mut v = [0.0f32; CHANNELS];
        for l in 0..n {
            let g = lp.layer_gain(l);
            // Eighty decibels down is not quiet, it is gone — and skipping it
            // saves the arena read and the wrap fade's second read with it. The
            // audio is still there; only the reading of it stops.
            if g > 1.0e-4 {
                for ch in 0..CHANNELS {
                    v[ch] += self.sample_at(li, l, pos, ch) * g;
                }
            }
        }
        v
    }

    /// One loop, rendered offline exactly as it sounds — layers flattened,
    /// placed and levelled — and nothing else.
    ///
    /// ## Why the engine has to be the one to do this
    ///
    /// `save_take` writes the arena raw, which is right for a session and wrong
    /// for everybody else: a layer file carries no gain, no decay, no speed, no
    /// slot and no placement, so nothing downstream can reconstruct what was
    /// played without reimplementing this file. Rendering is not a mixing-desk
    /// job that could live in another tool — it is the one question only the
    /// engine can answer.
    ///
    /// And it is nearly free, because the renderer already exists and runs
    /// forty-eight thousand times a second. This is the same call in a plain
    /// loop with the clock taken away.
    ///
    /// ## How long a rendered loop is
    ///
    /// **One cycle**, and the sparse layers are already inside it: `layer_pos`
    /// finds a slot with `(pos / layer_len) % period`, so a bar that sounds on
    /// the third of every four *is* a four-bar loop holding a one-bar layer,
    /// and those four bars are `loop_len`. There is no longer period hiding
    /// behind the loop's own, which is worth stating because there obviously
    /// could have been and the arithmetic to find one was written before this
    /// was read properly.
    ///
    /// Speed and the pendulum do change it, because they change how many
    /// *output* frames one trip through the audio takes: half speed is twice
    /// the file, and a pendulum is there and back.
    pub fn render_loop(&self, li: usize) -> Option<Vec<f32>> {
        let lp = self.lp(li);
        let len = lp.loop_len.load(Ordering::Acquire);
        if len == 0 || lp.n_layers.load(Ordering::Acquire) == 0 {
            return None;
        }
        let rate = lp.speed();
        if !rate.is_finite() || rate == 0.0 {
            return None;
        }
        let span = if lp.pendulum.load(Ordering::Relaxed) { 2 * len } else { len } as f64;
        let frames = (span / rate.abs()).round() as usize;
        if frames == 0 || frames > crate::wav::MAX_FRAMES {
            return None;
        }
        // Where the playhead reads zero. `raw_pos` is `warp + (f - origin) *
        // rate`, so this is that solved for `f` — and with the ordinary warp of
        // nothing it is `origin` exactly. Starting anywhere else would export a
        // loop that begins halfway through itself, which loops perfectly well
        // and is not the take.
        let warp = f64::from_bits(lp.warp.load(Ordering::Relaxed));
        let f0 = lp.origin.load(Ordering::Acquire) - (warp / rate).round() as i64;

        let fold = lp.mono.load(Ordering::Relaxed);
        let (gl, gr) = if fold { lp.pan_gains() } else { lp.balance_gains() };
        let v = f32::from_bits(lp.vol.load(Ordering::Relaxed));

        // Never consulted — `live` is false, so nothing below rolls — but
        // `loop_at` takes one, and a seeded one says so.
        let mut rng = SmallRng::seed_from_u64(0);
        let mut out = Vec::with_capacity(frames * CHANNELS);
        for f in 0..frames {
            let s = self.loop_at(li, f0 + f as i64, &mut rng, false);
            // The same two branches as the output callback, and they have to be:
            // a fold is an average through an equal-power pan, everything else
            // is two channels through a balance. See `balance_gains`.
            if fold {
                let m = (s[0] + s[1]) * 0.5;
                out.push(m * gl * v);
                out.push(m * gr * v);
            } else {
                out.push(s[0] * gl * v);
                out.push(s[1] * gr * v);
            }
        }
        Some(out)
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

    // **`--in-ch` becomes a source when nobody named any**, so an existing
    // command line keeps working and gets one mono source called "in".
    let sources: Vec<Source> = if opts.sources.is_empty() {
        vec![Source::mono("in", opts.in_ch)]
    } else {
        opts.sources.clone()
    };

    // A source naming a channel the device does not have would record silence
    // and say nothing, which is the shape of failure this engine exists to
    // refuse. Said at startup, where it can still be fixed.
    for s in &sources {
        for c in s.ch {
            if c >= in_channels {
                return Err(format!(
                    "source `{}` wants input channel {}, and {} has {}.",
                    s.name, c + 1, candidate.name, in_channels
                )
                .into());
            }
        }
    }

    println!("Device: {}", candidate.name);
    println!(
        "Playing output {}, at {} Hz. Sources: {}",
        opts.out_ch,
        sr,
        sources.iter().map(|s| s.describe()).collect::<Vec<_>>().join(", ")
    );
    println!(
        "Arena: {} loops x {} layers x {:.0} s x {} ch = {} MB.   \
         Pre-roll: {:.0} s x {} src x {} ch = {} MB.\n",
        N_LOOPS,
        MAX_LAYERS,
        opts.max_secs,
        CHANNELS,
        N_LOOPS * MAX_LAYERS * max_frames * CHANNELS * 4 / 1_048_576,
        opts.ring_secs,
        sources.len(),
        CHANNELS,
        ring_len * sources.len() * CHANNELS * 4 / 1_048_576
    );

    let sh = Arc::new(Shared {
        arena: (0..N_LOOPS * MAX_LAYERS * max_frames * CHANNELS)
            .map(|_| AtomicU32::new(0))
            .collect(),
        max_frames,
        ring: (0..ring_len * sources.len() * CHANNELS)
            .map(|_| AtomicU32::new(0))
            .collect(),
        ring_len,
        in_peak: (0..sources.len()).map(|_| AtomicU32::new(0)).collect(),
        sources,
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
        link_bar_frames: AtomicUsize::new(0),
        link_bar_origin: AtomicI64::new(0),
        launch_q: AtomicI64::new(-1),
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
                                // **Layers, not length.** This asked whether the
                                // loop had a length, which was the same question
                                // while the only way to have one was to have
                                // recorded one. A loop can now be *sized and
                                // empty* — told how many bars it is before
                                // anything is played into it — and that is a
                                // first recording with a length, not an overdub
                                // of nothing.
                                if n == 0 {
                                    // Only the first recording lays down the grid.
                                    // Re-stamping origin on every arm would drag the
                                    // whole loop to position zero the instant you
                                    // hit record — playback reads origin too. The
                                    // self-test cannot catch that, because both
                                    // sides move together.
                                    //
                                    // Safe on a sized-and-empty loop for the same
                                    // reason it is unsafe elsewhere: there is no
                                    // audio, so there is nothing for zero to move
                                    // away from.
                                    lp.origin.store(stamp, Ordering::Release);
                                    lp.rec_from.store(stamp, Ordering::Release);
                                    lp.clear_rec_env();
                                    lp.threaded.store(false, Ordering::Relaxed);
                                    lp.state.set(FIRST);
                                    // If the length was known before a note was
                                    // played, the close is known too. Arm it here
                                    // and let `closer` do the work — an audio
                                    // callback must not be the thing that draws a
                                    // layer.
                                    let want = lp.loop_len.load(Ordering::Acquire);
                                    lp.rec_len.store(want, Ordering::Release);
                                    lp.close_at.store(
                                        if want > 0 { stamp + want as i64 } else { i64::MIN },
                                        Ordering::Release,
                                    );
                                } else {
                                    // An overdub is modular against the existing
                                    // grid, so it records from the same reference
                                    // the loop plays from.
                                    lp.rec_from
                                        .store(lp.origin.load(Ordering::Acquire), Ordering::Release);
                                    lp.clear_rec_env();
                                    lp.threaded.store(false, Ordering::Relaxed);
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

                // **The click follows the grid, and ticks beats.**
                //
                // It followed the selected loop's cycle, one blip a time round,
                // and the note beside it said what to do about that: *when bar
                // quantisation lands, the click should follow Link instead —
                // that will be a grid rather than a guess.* It has landed.
                //
                // Two things were wrong with the old one and both bit the same
                // workflow. It needed a recorded loop to exist — `click_len > 0`
                // — so there was **no click before the first take**, which is
                // the one moment you need to count yourself in. And one blip a
                // cycle is not a count-in: four are, with the first one louder.
                //
                // Falls back to the selected loop when there is no grid at all,
                // which is a rig with no clock and nothing recorded — where the
                // old behaviour was the only answer available and still is.
                let (click_origin, click_len, click_beats) = match sh.grid() {
                    Some((o, bar)) => {
                        let q = f64::from_bits(sh.link_quantum.load(Ordering::Relaxed));
                        (o, bar, if q >= 1.0 { q.round() as usize } else { 4 })
                    }
                    None => {
                        let li = sh.sel();
                        (
                            sh.lp(li).origin.load(Ordering::Acquire),
                            sh.lp(li).loop_len.load(Ordering::Acquire),
                            1,
                        )
                    }
                };
                let click_beat = (click_len / click_beats.max(1)).max(1);

                // Monitoring reads the freshest frames the pre-roll holds. One
                // buffer behind the converters, so the interface's own direct
                // monitoring beats it — this is for headphones with nothing
                // else in the room.
                let monitor = sh.monitor.load(Ordering::Relaxed);
                let mon_from = sh.in_frames.load(Ordering::Acquire) as i64 - frames as i64;
                // **Monitor whatever is about to be written to.** Monitoring
                // exists to hear yourself while you play, and what you are
                // playing into is the loop that is armed or recording. Falls
                // back to the first source when nothing is, which is what a rig
                // with one source has always done.
                let mon_src = (0..N_LOOPS)
                    .find(|&li| {
                        let lp = sh.lp(li);
                        lp.is_armed() || lp.is_recording()
                    })
                    .map(|li| sh.src_of(li))
                    .unwrap_or(0);

                let mut peak = 0.0f32;
                // Once per buffer, not once per frame: six loops times two
                // trig calls is free here and wasteful inside the frame loop.
                let mut gains = [(0.0f32, 0.0f32); N_LOOPS];
                let mut folds = [false; N_LOOPS];
                for li in 0..N_LOOPS {
                    let lp = sh.lp(li);
                    // Level folded into the placement gains rather than applied
                    // in the frame loop: it is a per-buffer constant like the
                    // pan itself, and one multiply here is eight thousand fewer
                    // down there.
                    let v = f32::from_bits(lp.vol.load(Ordering::Relaxed));
                    // **Two different controls wearing one knob.** A loop that
                    // is folded to mono is a single signal being *placed*, so
                    // the equal-power pan is right. A loop that is not is two
                    // signals already in a field, and panning them would
                    // collapse it — what that knob means there is *balance*,
                    // which attenuates one side and leaves the other alone.
                    // See `Loop::mono` and `balance_gains`.
                    let fold = lp.mono.load(Ordering::Relaxed);
                    let (l, r) = if fold { lp.pan_gains() } else { lp.balance_gains() };
                    folds[li] = fold;
                    gains[li] = (l * v, r * v);
                }

                for f in 0..frames {
                    let out_frame = (base + f) as i64;
                    let mut vl = 0.0f32;
                    let mut vr = 0.0f32;

                    for li in 0..N_LOOPS {
                        let s = sh.loop_at(li, out_frame, &mut rng, true);
                        if folds[li] {
                            // Averaged rather than summed: two channels of the
                            // same performance are correlated, so adding them
                            // would be 6 dB louder than either and a fold would
                            // change the level as well as the width.
                            let m = (s[0] + s[1]) * 0.5;
                            vl += m * gains[li].0;
                            vr += m * gains[li].1;
                        } else {
                            vl += s[0] * gains[li].0;
                            vr += s[1] * gains[li].1;
                        }
                    }
                    // The click sits in the middle. It is a reference, not
                    // material, and a reference that moves is not one.
                    let mut v = 0.0f32;
                    if click_len > 0 && sh.click.load(Ordering::Relaxed) {
                        let pos = (out_frame - click_origin).rem_euclid(click_len as i64) as usize;
                        // The downbeat is louder, which is the whole of what
                        // makes four blips a count-in rather than a rattle.
                        if pos < 16 {
                            v += 0.5;
                        } else if pos % click_beat < 16 {
                            v += 0.22;
                        }
                    }
                    vl += v;
                    vr += v;

                    // **The monitor keeps its sides**, where the click does not:
                    // it is the thing you are about to record, so hearing it
                    // collapsed would be hearing something other than what
                    // lands in the loop.
                    if monitor {
                        if let Some(m) = sh.ring_at(mon_src, mon_from + f as i64, 0) {
                            vl += m;
                        }
                        if let Some(m) = sh.ring_at(mon_src, mon_from + f as i64, 1) {
                            vr += m;
                        }
                    }
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

                // **Every source, always, regardless of transport state.**
                // This is what makes the past claimable — and it is why the
                // source is a per-loop choice rather than a rig-wide one. A
                // moment you did not know you wanted has to have been captured
                // on whichever input it happened on.
                for (si, src) in sh.sources.iter().enumerate() {
                    let mut peak = 0.0f32;
                    for f in 0..frames {
                        let i = (si * sh.ring_len + (base + f) % sh.ring_len) * CHANNELS;
                        for ch in 0..CHANNELS {
                            let v = data[f * in_channels + src.ch[ch]];
                            peak = peak.max(v.abs());
                            sh.ring[i + ch].store(v.to_bits(), Ordering::Relaxed);
                        }
                    }
                    sh.in_peak[si].fetch_max(peak.to_bits(), Ordering::Relaxed);
                }

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
                    // **The armed loop's own input, not the rig's.** Arm a drum
                    // loop and it should wait for a drum; arm a guitar loop and
                    // it should wait for a guitar. One shared peak would have
                    // each of them starting on the other, which is the sort of
                    // thing you would blame on the threshold for an hour.
                    let asrc = &sh.sources[sh.src_of(li)];
                    let apeak = f32::from_bits(sh.in_peak[sh.src_of(li)].load(Ordering::Relaxed));
                    if apeak >= thresh {
                        if let Some(f) = (0..frames).find(|&f| {
                            (0..CHANNELS)
                                .any(|c| data[f * in_channels + asrc.ch[c]].abs() >= thresh)
                        })
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
                let revox = lp.revox.load(Ordering::Relaxed);
                // Whether the playhead is anywhere other than where a linear
                // write would put it. Once a buffer, because it is a property of
                // the loop and not of a frame — and `plain` is the same question
                // every writer in this file has always asked, so the two cannot
                // come to disagree about what unity means.
                let moving = !lp.plain();
                let fb = f32::from_bits(lp.fb.load(Ordering::Relaxed));
                // The one-pole's coefficient, worked out once a buffer rather
                // than once a frame — it only changes when the knob does.
                let tone = f32::from_bits(lp.tone.load(Ordering::Relaxed));
                let tone_a = if tone >= 20_000.0 {
                    1.0
                } else {
                    1.0 - (-2.0 * std::f32::consts::PI * tone / sr as f32).exp()
                };
                // The recording loop's own input, and its own tape memory per
                // channel. Revox's one-pole runs along the tape, so the two
                // sides need separate memories or the filter would cross-feed
                // them and the stereo would collapse a little on every pass.
                let rec_src = &sh.sources[sh.src_of(li)];
                let mut lp_mem = [f32::from_bits(lp.tape_lp.load(Ordering::Relaxed)); CHANNELS];
                if layer >= MAX_LAYERS && !revox {
                    sh.in_frames.store(base + frames, Ordering::Release);
                    return;
                }

                for f in 0..frames {
                    let out_frame = (base + f) as i64 + k;
                    let rel = out_frame - origin;
                    if rel < 0 {
                        continue;
                    }
                    if state == FIRST || state == MULTIPLY {
                        // Linear. Its length becomes the cycle, so it must not
                        // wrap — and it stops rather than overwriting.
                        let pos = rel as usize;
                        if pos >= sh.max_frames {
                            lp.overflowed.store(true, Ordering::Relaxed);
                            continue;
                        }
                        let mut loudest = 0.0f32;
                        for ch in 0..CHANNELS {
                            let v = data[f * in_channels + rec_src.ch[ch]];
                            sh.write(li, layer, pos, ch, v);
                            loudest = loudest.max(v.abs());
                        }
                        lp.reached.fetch_max(pos + 1, Ordering::Relaxed);
                        lp.rec_reached.fetch_max(out_frame + 1, Ordering::Relaxed);
                        // **A first take has no length yet**, so its picture
                        // cannot be laid out against one. It is drawn against
                        // the arena instead and rescales itself when the loop
                        // closes — which is what a tape counter does, and it
                        // means the bar fills left to right as you play rather
                        // than sitting empty until you stop.
                        lp.mark_rec_env(pos * ENV_BUCKETS / sh.max_frames, loudest);
                    } else {
                        // Modular: an overdub may go round as many times as it
                        // likes, summing into the same cycle.
                        if loop_len == 0 {
                            continue;
                        }
                        // **The write head follows the PLAY head.**
                        //
                        // At unity the two are the same ramp, and this is the
                        // fast branch that has always been here: one input
                        // frame, one slot. At any other rate they are not, and
                        // a linear write would put what you played somewhere
                        // you never heard it — which is why recording into a
                        // loop at speed used to be refused outright rather than
                        // done wrongly.
                        //
                        // The moving branch below spans instead of picking. One
                        // input frame covers an interval of the loop, and it is
                        // added to every slot that interval touches, weighted by
                        // how much of that slot it covers. That one rule gives
                        // all three cases without a case for any of them:
                        //
                        //   - **backwards** walks the interval down, one slot to
                        //     one frame, exactly and with no resampling at all;
                        //   - **half speed** has two input frames sharing a slot
                        //     at half weight each, which is their average — so
                        //     what comes back is what you played, not twice as
                        //     loud;
                        //   - **double speed** has one input frame filling two
                        //     slots at full weight, a zero-order hold.
                        //
                        // And at unity the interval is exactly one slot at
                        // weight one, which is the branch above — so the common
                        // path keeps its single write and this cannot change
                        // what it does.
                        if moving {
                            let a = lp.raw_pos(out_frame);
                            let b = lp.raw_pos(out_frame + 1);
                            let (lo, hi) = if a <= b { (a, b) } else { (b, a) };
                            let mut slot = lo.floor() as i64;
                            let mut first = true;
                            // A stopped loop has `hi == lo` and writes nothing,
                            // which is right: there is nowhere for it to go.
                            while (slot as f64) < hi {
                                let cover = (((slot + 1) as f64).min(hi)
                                    - (slot as f64).max(lo))
                                    .max(0.0) as f32;
                                if cover > 0.0 {
                                    let p = slot.rem_euclid(loop_len as i64) as usize;
                                    for ch in 0..CHANNELS {
                                        let v = data[f * in_channels + rec_src.ch[ch]];
                                        sh.add(li, layer, p, ch, v * cover);
                                    }
                                    if first {
                                        let loudest = (0..CHANNELS)
                                            .map(|ch| {
                                                data[f * in_channels + rec_src.ch[ch]].abs()
                                            })
                                            .fold(0.0f32, f32::max);
                                        lp.mark_rec_env(
                                            p * ENV_BUCKETS / loop_len,
                                            loudest,
                                        );
                                        first = false;
                                    }
                                }
                                slot += 1;
                            }
                            lp.reached.fetch_max(loop_len, Ordering::Relaxed);
                            lp.rec_reached.fetch_max(out_frame + 1, Ordering::Relaxed);
                            continue;
                        }
                        let pos = (rel % loop_len as i64) as usize;
                        // **Revox writes over the tape; everything else writes
                        // beside it.** In Revox mode there is one layer by
                        // construction and the overdub goes into *that*, not
                        // into a new one — which is why `layer` is zero here and
                        // why the loop does not grow a layer per pass.
                        for ch in 0..CHANNELS {
                            let v = data[f * in_channels + rec_src.ch[ch]];
                            if revox {
                                // What is on the tape, dulled, quieter, with the
                                // new sound on top of it. The filter runs along
                                // the tape rather than along time, which is the
                                // same thing while the head is moving and the
                                // reason the memory survives the wrap.
                                let cur = sh.read(li, 0, pos, ch);
                                lp_mem[ch] += tone_a * (cur - lp_mem[ch]);
                                sh.write(li, 0, pos, ch, lp_mem[ch] * fb + v);
                            } else {
                                sh.add(li, layer, pos, ch, v);
                            }
                        }
                        lp.reached.fetch_max(loop_len, Ordering::Relaxed);
                        lp.rec_reached.fetch_max(out_frame + 1, Ordering::Relaxed);
                        // An overdub already knows the cycle it is going round,
                        // so its picture is laid out against that and fills in
                        // wherever the playhead is — including on the second and
                        // third time round, which is why this is a peak and not
                        // a store.
                        let loudest = (0..CHANNELS)
                            .map(|ch| data[f * in_channels + rec_src.ch[ch]].abs())
                            .fold(0.0f32, f32::max);
                        lp.mark_rec_env(pos * ENV_BUCKETS / loop_len, loudest);
                    }
                }
                // One stored memory for two, which is a small lie that costs
                // nothing: it is the seed for the next buffer's filter and the
                // two sides are within a sample of each other by construction.
                lp.tape_lp.store(lp_mem[0].to_bits(), Ordering::Relaxed);
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

    spawn_closer(sh.clone(), sr);

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
/// **The second press, made unnecessary.**
///
/// One thread for the whole rig, polling every five milliseconds for a first
/// recording that has reached the length it was told to be. Five is the same
/// interval `multiply_end` already waits at and is a fortieth of the shortest
/// bar anyone will use; the close it produces is quantised to the loop's own
/// length by construction, so the poll's own jitter never reaches the audio —
/// `commit` is handed the target frame, not the frame it woke up on.
///
/// A thread rather than the callback because closing a recording draws a layer
/// and sleeps. A poll rather than a scheduled wake because there are six loops
/// and one of them might be re-armed while another is closing, and a timer per
/// recording is a timer to cancel.
///
/// **It re-checks before it acts, and that is the cancellation.** A foot that
/// closes the take early leaves the state at `PLAYING`; a clear leaves the
/// length at zero; a new recording moves `rec_from`. Any of those and this
/// finds the world it was told about is gone, and does nothing. There is no
/// flag to forget to clear.
fn spawn_closer(sh: Arc<Shared>, sr: u32) {
    std::thread::spawn(move || loop {
        std::thread::sleep(Duration::from_millis(5));
        let now = sh.out_frames.load(Ordering::Acquire) as i64;
        for li in 0..N_LOOPS {
            let lp = sh.lp(li);
            let at = lp.close_at.load(Ordering::Acquire);
            if at == i64::MIN || now < at {
                continue;
            }
            // Taken before the check, so two ticks cannot both close one take.
            if lp
                .close_at
                .compare_exchange(at, i64::MIN, Ordering::AcqRel, Ordering::Relaxed)
                .is_err()
            {
                continue;
            }
            if lp.state.get() != FIRST {
                continue;
            }
            // `late` is how far past the target we woke, so `commit` closes the
            // loop at the length it was asked for rather than at the length the
            // poll happened to notice.
            let msg = commit(&sh, li, sr, now - at);
            println!("  {}", msg);
            sh.note_ack(&msg);
        }
    });
}

fn commit(sh: &Shared, li: usize, sr: u32, late: i64) -> String {
    let lp = sh.lp(li);
    let state = lp.state.get();
    if state != FIRST && state != OVERDUB {
        // The callers only reach here from FIRST or OVERDUB, so this is a guard
        // rather than a path — but it answers anyway. Returning nothing is how
        // fourteen verbs became invisible, and a guard is exactly the sort of
        // thing that stops being unreachable without anyone noticing.
        return format!("loop {} is not recording.", li);
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
        // **Asked for beats counted.** Taken rather than read, so a take that is
        // closed by a foot instead of by `closer` cannot leave it armed for the
        // next one.
        let declared = lp.rec_len.swap(0, Ordering::AcqRel);
        let mut len = quantised_len.or(if declared > 0 { Some(declared) } else { None })
        .unwrap_or_else(|| {
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
        if declared > 0 && reached < declared {
            // Should not happen — `commit` sleeps a drain before reading
            // `reached` precisely so the input can catch up — but if it ever
            // does, the last few frames of the loop are silence rather than
            // audio. Said out loud rather than quietly shortening the loop,
            // because shortening it is the failure this is here to prevent.
            println!(
                "  input was {} frames behind the declared length; the tail is silent.",
                declared - reached
            );
        }
        if len == 0 {
            return format!("loop {} recorded nothing.", li);
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
                for ch in 0..CHANNELS {
                    let v = sh.read(li, layer, pos, ch);
                    sh.write(li, layer, pos + pre, ch, v);
                }
            }
            for pos in 0..pre {
                for ch in 0..CHANNELS {
                    sh.write(li, layer, pos, ch, 0.0);
                }
            }
            let got = fill_from_ring(sh, li, layer, new_origin, pre, 0, false);
            lp.origin.store(new_origin, Ordering::Release);
            // **A declared length is a promise about length; the pre-roll is
            // about where the loop starts.**
            //
            // Growing `len` here is right for a take whose length came from
            // what was played — you keep everything, plus the attack that was
            // clipped off the front. It is wrong for a take that was *told* how
            // long to be. Recipe 2 asks for four bars, arms, and plays: the
            // close fires at exactly four bars, and then this line added the
            // hundred milliseconds of recovered attack on top, so an 8.000 s
            // loop committed at 8.1 and sat beside the 8.000 s loop it was
            // supposed to match. Andrew saw it as two slots reading 8.0 and 8.1.
            //
            // Not by closing earlier, which was the other candidate: that would
            // spend the pre-roll out of the take instead of off the end, and
            // leave nothing past the loop point for the wrap crossfade to reach
            // into. Keeping the length shifts the last `pre` frames past the
            // end, where `tail` picks them up and `sample_at` uses them — the
            // material is not discarded, it becomes the continuation.
            if declared == 0 {
                len += pre;
            }
            println!(
                "  pre-roll: {:.0} ms recovered from before the tap ({} of {} frames).",
                pre as f64 / sr as f64 * 1000.0,
                got,
                pre
            );
        }
        lp.loop_len.store(len, Ordering::Release);
        // **And how many bars that is**, where anything knows what a bar is.
        //
        // `commit` set a length and never a bar count, which was invisible
        // while the count only mattered to loops that had been *told* one:
        // `cycles` is zero for a freely recorded loop and zero reads as one
        // everywhere. So an eight-second take showed "1 bar" on the encoder,
        // and `bpm` on it would have offered a tempo four times too slow.
        //
        // Rounded to the nearest, and at least one. A take aimed at four bars
        // misses in both directions and the nearest is what was meant; a take
        // shorter than a bar is one bar, because zero is the value that means
        // "nobody has said" and this is somebody saying.
        //
        // Only with a clock. Without one the first loop *is* the pulse and its
        // whole length is one cycle — the clockless behaviour `loop_grid`
        // depends on — so writing a count here would be inventing a metre from
        // nothing.
        let bar = sh.link_bar_frames.load(Ordering::Relaxed);
        if bar > 0 && len > 0 {
            let bars = ((len as f64 / bar as f64).round() as usize).max(1);
            lp.cycles.store(bars, Ordering::Release);
        }
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
        let src = sh.src_of(li);
        if len > 0 {
            for f in closed_at..last {
                let Some(v0) = sh.ring_at(src, f - k, 0) else { continue };
                let pos = (f - rec_from).rem_euclid(len as i64) as usize;
                let at = len + (f - closed_at) as usize;
                for ch in 0..CHANNELS {
                    let v = if ch == 0 {
                        v0
                    } else {
                        sh.ring_at(src, f - k, ch).unwrap_or(0.0)
                    };
                    sh.add(li, layer, pos, ch, -v);
                    if at < sh.max_frames {
                        sh.write(li, layer, at, ch, v);
                    }
                }
                undone += 1;
                if at < sh.max_frames {
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

    // **A Revox pass makes no layer.** It went over the tape, so what changed
    // is layer zero and there is nothing new to shape or to count. The picture
    // has to be redrawn because the audio under it moved, which is the one
    // thing this branch still owes.
    if lp.revox.load(Ordering::Relaxed) {
        lp.state.set(PLAYING);
        sh.rebuild_env(li, 0);
        return format!(
            "loop {} over the tape: {:.3} s, one layer.",
            li,
            len as f64 / sr as f64
        );
    }

    // Born on the pass it was committed on, which is when it starts existing as
    // something to be heard — and so when it starts getting older.
    lp.set_layer_shape(layer, Shape { len, tail, born: lp.pass_index(closed_at, len) });
    sh.rebuild_env(li, layer);
    lp.add_layer();
    if len > 0 {
        draw_layer(sh, li, layer, len, sr);
    }
    // The length belongs in the ack even though the snapshot also carries it:
    // this is the sentence that appears when the press lands, and "committed"
    // on its own cannot be told apart from the previous "committed". The
    // detail prints above stay on the console — they are several lines each and
    // diagnostic rather than an outcome.
    format!(
        "loop {} committed: {:.3} s, {} layer{} playing.",
        li,
        len as f64 / sr as f64,
        layer + 1,
        if layer == 0 { "" } else { "s" }
    )
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
        let v = (0..CHANNELS)
            .map(|ch| sh.read(li, layer, i, ch).abs())
            .fold(0.0f32, f32::max);
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
    // The loop's own source. A retroactive take lands wherever the press said,
    // and it takes the past of the input that loop is pointed at — which is the
    // whole reason every source keeps a ring of its own.
    let src = sh.src_of(li);
    let mut got = 0;
    for pos in 0..len {
        let Some(v0) = sh.ring_at(src, from_out + pos as i64 - k, 0) else {
            continue;
        };
        for ch in 0..CHANNELS {
            let v = if ch == 0 { v0 } else {
                sh.ring_at(src, from_out + pos as i64 - k, ch).unwrap_or(0.0)
            };
            if additive {
                sh.add(li, layer, at + pos, ch, v);
            } else {
                sh.write(li, layer, at + pos, ch, v);
            }
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
fn take(sh: &Shared, li: usize, sr: u32, secs: f64, late: i64) -> String {
    let lp = sh.lp(li);
    if !sh.k_set.load(Ordering::Acquire) {
        return "no input has arrived yet.".to_string();
    }
    let layer = lp.n_layers.load(Ordering::Acquire);
    if layer >= MAX_LAYERS {
        return format!(
            "loop {} is at {} layers, the ceiling; undo one first.",
            li, MAX_LAYERS
        );
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
            return format!("loop {}: not one complete cycle has gone by yet.", li);
        }
        (origin + (done - 1) * loop_len as i64, loop_len, "layer")
    };

    if from_out < 0 {
        return "that reaches back before the engine started.".to_string();
    }

    sh.zero_layer(li, layer);
    let got = fill_from_ring(sh, li, layer, from_out, len, 0, false);
    if got == 0 {
        return "the pre-roll does not reach back that far.".to_string();
    }
    // A short take is not a failed one: it succeeded with less than was asked
    // for. So this is a PREFIX to whatever the outcome turns out to be, not a
    // branch of its own — the app has to be told both that it worked and that
    // it is shorter than you meant, in one sentence.
    let shortfall = if got < len {
        format!(
            "only {:.2} s of the {:.2} s asked for was still in the pre-roll — ",
            got as f64 / sr as f64,
            len as f64 / sr as f64
        )
    } else {
        String::new()
    };

    let headline;
    if loop_len == 0 {
        lp.loop_len.store(len, Ordering::Release);
        // The first loop to acquire a length becomes the grid the rest
        // can align to — first rather than chosen, because that is how a
        // looper has always worked: what you played first is what the
        // rest fits around. A compare-exchange, so later calls are no-ops.
        sh.claim_anchor(li);
        lp.origin.store(from_out, Ordering::Release);
        lp.state.set(PLAYING);
        headline = format!(
            "loop {} took the last {:.3} s as the {}: {} frames, {:.1} bpm if that is one bar of 4/4",
            li,
            len as f64 / sr as f64,
            what,
            len,
            240.0 / (len as f64 / sr as f64)
        );
    } else {
        headline = format!("loop {} took the last complete cycle as a new {}.", li, what);
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
    format!(
        "{}{} — {} layer{} playing.",
        shortfall,
        headline.trim_end_matches('.'),
        taken + 1,
        if taken == 0 { "" } else { "s" }
    )
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
fn multiply_start(sh: &Shared, li: usize, sr: u32) -> String {
    let lp = sh.lp(li);
    let loop_len = lp.loop_len.load(Ordering::Acquire);
    if loop_len == 0 {
        return format!("loop {} has nothing to multiply — record a loop first.", li);
    }
    if lp.n_layers.load(Ordering::Acquire) >= MAX_LAYERS {
        return format!(
            "loop {} is at {} layers, the ceiling; undo one first.",
            li, MAX_LAYERS
        );
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
    // One sentence, not three. At a console three lines read as a paragraph; in
    // a single-line display the last one wins and the other two never existed.
    // The instruction ("x again to end it") is the part worth keeping, because
    // a multiply is the one gesture that is not finished when you let go.
    if behind > 0 {
        let got = fill_from_ring(sh, li, layer, from, behind, 0, false);
        lp.reached.fetch_max(got, Ordering::Relaxed);
        format!(
            "loop {} multiplying from the start of this cycle ({:.2} s recovered from \
             the pre-roll) — play across as many cycles as you want, then x again.",
            li,
            got as f64 / sr as f64
        )
    } else {
        format!(
            "loop {} multiplying from this cycle's start — play across as many cycles \
             as you want, then x again.",
            li
        )
    }
}

/// End a multiply: round to whole cycles and grow the loop to fit.
///
/// Rounding rather than truncating, because at nine tenths of the way through
/// the fourth cycle you meant four. Which means sometimes waiting for the
/// boundary to arrive rather than cutting the loop short at the press.
fn multiply_end(sh: &Shared, li: usize, sr: u32) -> String {
    let lp = sh.lp(li);
    let loop_len = lp.loop_len.load(Ordering::Acquire);
    let from = lp.rec_from.load(Ordering::Acquire);
    let cur = sh.out_frames.load(Ordering::Acquire) as i64;
    let elapsed = (cur - from).max(0) as f64;

    let n = ((elapsed / loop_len as f64).round() as usize).max(1);
    let new_len = n * loop_len;
    if new_len > sh.max_frames {
        lp.state.set(PLAYING);
        return format!(
            "loop {}: {} cycles would be {:.1} s, past the --max-secs ceiling of {:.1} s. \
             Stopping at the old length.",
            li,
            n,
            new_len as f64 / sr as f64,
            sh.max_frames as f64 / sr as f64
        );
    }

    // If the rounding went up, the last cycle has not finished yet. Wait for it
    // rather than hand back a loop that is short by however late the press was.
    let target = from + new_len as i64;
    // Said in the ACK rather than only here, and after the fact rather than
    // before it: this call blocks until the boundary arrives, so a message sent
    // now could not reach the app before the outcome does anyway. It matters
    // because a press that appears to do nothing for half a cycle is exactly
    // the kind of pause that gets pressed again.
    let rounded = if target > cur {
        format!(
            " (rounded up, waited {:.2} s for the boundary)",
            (target - cur) as f64 / sr as f64
        )
    } else {
        String::new()
    };
    if target > cur {
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

    let layer = lp.n_layers.load(Ordering::Acquire);
    // A multiplied layer ends where the multiply ended; nothing follows it. Born
    // at zero because a multiply redefines the cycle, so every pass count on
    // this loop starts again from here.
    lp.set_layer_shape(layer, Shape { len: new_len, tail: 0, born: 0 });
    sh.rebuild_env(li, layer);
    lp.add_layer();
    draw_layer(sh, li, layer, new_len, sr);
    format!(
        "loop {} x{}: now {:.3} s ({} cycles of {:.3} s){} — {} layers playing.",
        li,
        n,
        new_len as f64 / sr as f64,
        n,
        loop_len as f64 / sr as f64,
        rounded,
        layer + 1
    )
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
/// How often the newest layer sounds, **absolutely**, and nothing else.
///
/// Two things changed here on 2026-08-27, both because the surface grew a knob
/// for this and a knob holds a value rather than repeating a gesture.
///
/// **Absolute, not multiplicative.** `s4` used to mean *sound four times less
/// often than you already do*, so pressing it twice gave one in eight and there
/// was no way back except `d`. That is the right shape for a footswitch and the
/// wrong one for a knob: a knob asks "what should this be", and a control whose
/// meaning depends on where it has been cannot be read off the engine.
///
/// **And it no longer changes the loop's length.** It used to do both — set the
/// period *and* grow the loop by the same factor — which meant "how often does
/// this sound" and "how long is this loop" were one gesture and could not be
/// set independently. They are two knobs now: `len` says how many bars, this
/// says how often the material lands in them. A four-bar loop whose phrase
/// sounds every bar and a four-bar loop whose phrase sounds once are the same
/// length and different music, and neither was reachable before.
///
/// The way back is `d`, which is this with `n = 1`.
fn sparse(sh: &Shared, li: usize, _sr: u32, n: usize) -> String {
    let lp = sh.lp(li);
    let layers = lp.n_layers.load(Ordering::Acquire);
    if layers == 0 {
        return "nothing to spread — record a loop first.".into();
    }
    if n < 1 || n > MAX_PERIOD {
        return format!("`every` wants 1 to {}, not {}.", MAX_PERIOD, n);
    }
    let l = layers - 1;
    let (len, _, phase) = lp.layer_shape(l);
    if len == 0 {
        return "that layer has no length.".into();
    }
    lp.l_period[l].store(n, Ordering::Release);
    // A phase that is now past the end would silence the layer outright, which
    // is not what asking for a different spacing means.
    lp.l_phase[l].store(phase % n, Ordering::Release);
    if n == 1 {
        format!("layer {} sounds every time round.", l + 1)
    } else {
        format!(
            "layer {} sounds once every {}, on slot {}.",
            l + 1,
            n,
            (phase % n) + 1
        )
    }
}

/// Which slot of its period the newest layer lands on — the absolute form of
/// `o`, for the same reason `s` became absolute.
///
/// **Wraps rather than refusing.** The range depends on the period, and the app
/// deliberately does not make one knob's range depend on another's value: that
/// would make the pure position-to-value function need the snapshot. So any
/// slot is legal here and lands somewhere sensible, and turning past the end
/// comes round to the start, which is what a placement control should do
/// anyway.
fn place_at(sh: &Shared, li: usize, n: usize) -> String {
    let lp = sh.lp(li);
    let layers = lp.n_layers.load(Ordering::Acquire);
    if layers == 0 {
        return "nothing to place.".into();
    }
    let l = layers - 1;
    let (_, period, _) = lp.layer_shape(l);
    let slot = n % period.max(1);
    lp.l_phase[l].store(slot, Ordering::Release);
    if period <= 1 {
        format!(
            "layer {} sounds every time round, so there is only one slot; \
             `{}s<n>` first to make room.",
            l + 1,
            li
        )
    } else {
        format!("layer {} is on slot {} of {}.", l + 1, slot + 1, period)
    }
}

/// **How many bars this loop is.** One verb, and which of its three jobs it is
/// doing depends on what the loop already is — said out loud in the ack every
/// time, because the difference is the whole of it.
///
/// * **Empty** — sizes it. The loop gets a length and no audio, and the next
///   recording closes itself there instead of waiting for a second press.
/// * **The anchor, with no clock** — *declares* it. The audio is untouched and
///   the pulse becomes a fraction of it, which is the only way a clockless
///   session gets a loop shorter than its first take. Resizing the thing that
///   defines the pulse would move everything that follows it, so it doesn't.
/// * **Anything else with material in it** — resizes it. The layers keep their
///   own lengths and wrap inside the new one, which is what `multiply_end` has
///   always done at the end of a multiply.
fn set_bars(sh: &Shared, li: usize, sr: u32, n: usize) -> String {
    let lp = sh.lp(li);
    if n < 1 || n > MAX_BARS {
        return format!("a loop wants 1 to {} bars, not {}.", MAX_BARS, n);
    }
    if lp.is_recording() {
        return format!("loop {} is recording; finish that first.", li);
    }
    let layers = lp.n_layers.load(Ordering::Acquire);
    let anchor = sh.anchor.load(Ordering::Acquire);
    let clocked = sh.link_bar_frames.load(Ordering::Relaxed) > 0;

    // Declaring: the audio stays exactly as it is and the number beside it
    // changes, which divides the pulse for everything that follows.
    if layers > 0 && li == anchor && !clocked {
        let len = lp.loop_len.load(Ordering::Acquire);
        lp.cycles.store(n, Ordering::Release);
        return format!(
            "loop {} is {} bar{} — the bar is now {:.3} s. Nothing was moved.",
            li,
            n,
            if n == 1 { "" } else { "s" },
            (len / n.max(1)) as f64 / sr as f64
        );
    }

    let Some((origin, bar)) = sh.grid() else {
        return format!(
            "no bar yet: there is no clock and no loop has a length. \
             Record something first, or start Link."
        );
    };
    let want = n * bar;
    if want > sh.max_frames {
        return format!(
            "{} bars would be {:.1} s, past the ceiling of {:.1} s.",
            n,
            want as f64 / sr as f64,
            sh.max_frames as f64 / sr as f64
        );
    }

    if layers == 0 {
        // Sized and empty: a length with nothing in it, which is a state the
        // engine did not have. A threaded tape is the neighbouring idea and is
        // not this one — that carries a silent layer so it can *play*, and it
        // would make the next recording an overdub. This stays at zero layers
        // so the next recording is a first recording, and closes itself.
        let now = sh.out_frames.load(Ordering::Acquire) as i64;
        let start = if lp.quant.load(Ordering::Relaxed) {
            let elapsed = now - origin;
            origin + (elapsed.div_euclid(bar as i64) + 1) * bar as i64
        } else {
            now
        };
        lp.origin.store(start, Ordering::Release);
        lp.loop_len.store(want, Ordering::Release);
        lp.cycles.store(n, Ordering::Release);
        sh.claim_anchor(li);
        return format!(
            "loop {} is set to {} bar{} ({:.3} s); record and it closes itself.",
            li,
            n,
            if n == 1 { "" } else { "s" },
            want as f64 / sr as f64
        );
    }

    // Resizing something with material in it. Growing is always safe; shrinking
    // below the longest layer would cut audio, and a length control that
    // silently trims is a length control you cannot use in a hurry.
    let longest = (0..layers).map(|l| lp.layer_shape(l).0).max().unwrap_or(0);
    if want < longest {
        return format!(
            "loop {} has a {:.3} s layer in it; {} bar{} would be {:.3} s. \
             Undo it or clear the loop first.",
            li,
            longest as f64 / sr as f64,
            n,
            if n == 1 { "" } else { "s" },
            want as f64 / sr as f64
        );
    }
    lp.loop_len.store(want, Ordering::Release);
    lp.cycles.store(n, Ordering::Release);
    format!(
        "loop {} is {} bar{} ({:.3} s); its layers keep their own lengths.",
        li,
        n,
        if n == 1 { "" } else { "s" },
        want as f64 / sr as f64
    )
}

/// **Take the session tempo from this loop.**
///
/// The other half of `set_bars`. That verb has three jobs — size an empty loop,
/// declare the bar count of a clockless anchor, resize something with material
/// in it — and *declaring* was reachable only with no clock, because with one
/// there was nothing to tell. There is now: link-spike answers
/// `/link/set-tempo`, and a tempo sent there reaches every peer on the session.
///
/// So this is declaring, with a clock. The loop says "I am `cycles` bars long
/// and `loop_len` frames", and those two numbers are a tempo.
///
/// ## Why this is not warping
///
/// **No audio moves.** `loop_len` is frames; loops play at frame rate and stay
/// phase-locked to each other whatever the bar is. What a bar length reaches is
/// the click, quantised launches and closes, `set_bars` arithmetic — and the
/// rest of the Link session. The principle is the one the whole bar model runs
/// on, at rig scale: *move the grid to the audio, never the audio to the grid.*
///
/// It also takes the tempo from the loop's **average** over its bars, not from
/// the timing within them. Play four bars a little long and the click comes to
/// you; play them unevenly and they stay uneven. That is the floor-looper
/// behaviour and it is the point.
///
/// ## What it costs when other loops exist
///
/// Nothing to them — they are frames and do not move, and they stay in
/// relation to each other. What moves is the click and everything downstream of
/// Link, so loops recorded against the old click are now out with the click and
/// still in with each other. Sometimes that is exactly the intent and sometimes
/// it is a disaster, so the ack counts them and says so rather than deciding.
fn take_tempo(sh: &Shared, li: usize, sr: u32) -> String {
    let lp = sh.lp(li);
    if lp.is_recording() || lp.is_armed() {
        return format!("loop {} is still being written; finish that first.", li);
    }
    let len = lp.loop_len.load(Ordering::Acquire);
    if len == 0 {
        return format!(
            "loop {} has no length, so there is no tempo in it. Record it, or \
             `{}len<n>` first.",
            li, li
        );
    }
    // **A loop nobody has counted may not set the tempo**, and this guard was
    // bought at the cost of putting the whole rig on 29.56 bpm.
    //
    // `cycles` is zero for "nobody has said" and reads as one everywhere else,
    // which is harmless where a wrong count means a wrong ring. Here it meant an
    // eight-second take offering 29.56 bpm — inside Link's 20..999, so the
    // range check passed, so it went out to Ableton and the modular. A
    // plausible wrong answer is the failure mode this whole rig is built to
    // avoid, and the range check cannot catch it: for a four-bar take at any
    // ordinary tempo, one quarter of the truth is still an ordinary tempo.
    //
    // With a clock `commit` now counts the bars of every take, so this only
    // ever refuses a loop that genuinely has no count — which is the clockless
    // case, where there is no session to tell anyway.
    let bars = lp.cycles.load(Ordering::Acquire);
    if bars == 0 {
        return format!(
            "nobody has said how many bars loop {} is, and a tempo taken from a \
             guess would be wrong by exactly that guess. `{}len<n>` first.",
            li, li
        );
    }
    let secs = len as f64 / sr as f64;
    let quantum = f64::from_bits(sh.link_quantum.load(Ordering::Relaxed));
    let bpm = tempo_of(len, bars, sr, quantum);

    // **Refused rather than clamped.** link-spike clamps to Link's documented
    // 20..999, and a clamp here would be a lie: a tempo outside that range does
    // not mean the loop is strange, it means the bar count is wrong — four bars
    // read as one, or a two-second loop declared as thirty-two. Saying which
    // number to look at is worth more than a tempo nobody asked for.
    if !(20.0..=999.0).contains(&bpm) {
        return format!(
            "loop {} is {:.3} s over {} bar{}, which is {:.1} bpm — outside 20 to 999. \
             The bar count is the number to look at.",
            li,
            secs,
            bars,
            if bars == 1 { "" } else { "s" },
            bpm
        );
    }

    if let Err(e) = crate::link::set_tempo(bpm, crate::link::DEFAULT_TEMPO_PORT) {
        return format!("could not set the tempo: {}", e);
    }

    // Everything that would now disagree with the click, counted. Not a
    // refusal — re-deciding the tempo around the loop that came out well is a
    // real move — but it is never something to discover afterwards.
    let others = (0..N_LOOPS)
        .filter(|&o| o != li && sh.lp(o).n_layers.load(Ordering::Acquire) > 0)
        .count();
    let heard = sh.link_anchors.load(Ordering::Relaxed) > 0;

    format!(
        "tempo taken from loop {}: {:.3} s over {} bar{} is {:.2} bpm.{}{}",
        li,
        secs,
        bars,
        if bars == 1 { "" } else { "s" },
        bpm,
        if others > 0 {
            format!(
                " {} other loop{} keep their audio but no longer agree with the click.",
                others,
                if others == 1 { " does and it" } else { "s do and they" }
            )
        } else {
            String::new()
        },
        if heard {
            ""
        } else {
            " No anchor has ever arrived, so nothing here can confirm link-spike took it."
        }
    )
}

/// The tempo a loop implies: its bars over its seconds, in beats.
///
/// Split out so it can be tested without a socket or a `Shared` — the rest of
/// `take_tempo` is guards and a UDP send, and this is the only part that can be
/// arithmetically wrong.
///
/// Beats to the bar come from Link where it is known and are four where it is
/// not, which is the same assumption `launch_grid` makes: a quantum of zero
/// means "nobody has said", not "no beats".
fn tempo_of(len: usize, bars: usize, sr: u32, quantum: f64) -> f64 {
    let secs = len as f64 / sr as f64;
    let beats_per_bar = if quantum >= 1.0 { quantum } else { 4.0 };
    60.0 * beats_per_bar * bars.max(1) as f64 / secs
}

/// Every loop that holds something, from the top, together.
///
/// **Not eight unmutes.** `h1` restores audibility and leaves each loop wherever
/// its own phase had got to, so a set of a four-bar, a three-bar and a one-bar
/// loop came back in whatever relationship they happened to be in — and since
/// the lengths differ, "where they happened to be" is not a musical fact about
/// anything. Starting the set means *starting* it, which means one origin for
/// all of them.
///
/// It reuses the request the fire switch sends, because `FIRE` already is this:
/// stamp the origin, put the playhead at the top (at the end, going backwards),
/// unmute. The only thing it adds is a `shot_end`, and that is read only
/// through `firing()`, which tests `one_shot` first — so on a loop that is not
/// a one-shot it is a number nothing consults.
///
/// **One deadline for all of them, computed once here.** That is the whole
/// point: eight loops each asking `next_boundary` at eight slightly different
/// moments is eight answers, and the set would land ragged in exactly the way
/// this exists to prevent. It is `next_boundary` rather than the bar outright,
/// so Start All lands on whatever `launch quantise` is already set to and does
/// not become a second opinion about when a launch happens.
fn start_all(sh: &Shared, sr: u32) -> String {
    let now = sh.out_frames.load(Ordering::Acquire) as i64;
    let at = sh.next_boundary(now);
    let mut n = 0usize;
    let mut busy = 0usize;
    for li in 0..N_LOOPS {
        let lp = sh.lp(li);
        if lp.loop_len.load(Ordering::Acquire) == 0 {
            continue;
        }
        // A take in progress is not part of the set yet, and restarting the loop
        // being written into would move the origin out from under the recording.
        if lp.is_recording() || lp.is_armed() {
            busy += 1;
            continue;
        }
        lp.request_at.store(at.unwrap_or(i64::MIN), Ordering::Release);
        lp.request.set(FIRE);
        n += 1;
    }
    if n == 0 {
        return if busy > 0 {
            "nothing to start — everything with audio in it is still recording.".into()
        } else {
            "nothing to start — no loop has anything in it.".into()
        };
    }
    format!(
        "{} loop{} start from the top together{}.{}",
        n,
        if n == 1 { "" } else { "s" },
        match at {
            Some(t) => format!(" on the grid in {:.2} s", (t - now).max(0) as f64 / sr as f64),
            None => String::new(),
        },
        if busy > 0 {
            format!(" {} still recording, left alone.", busy)
        } else {
            String::new()
        }
    )
}

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
                MULTIPLY => return multiply_end(sh, li, sr),
                FIRST | OVERDUB => return format!("loop {} is still recording — finish that first.", li),
                _ => {
                    if let Some(other) = busy_elsewhere(sh, li) {
                        return other;
                    }
                    if let Some(no) = not_plain(lp, li) {
                        return no;
                    }
                    return multiply_start(sh, li, sr);
                }
            },
            "r" => match lp.state.get() {
                MULTIPLY => return multiply_end(sh, li, sr),
                FIRST | OVERDUB => return commit(sh, li, sr, late),
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
                    if let Some(no) = not_writable(lp, li) {
                        return no;
                    }
                    let layer = lp.n_layers.load(Ordering::Acquire);
                    if layer >= MAX_LAYERS {
                        return format!(
                            "loop {} is at {} layers, the ceiling; undo one first.",
                            li, MAX_LAYERS
                        );
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
                        //
                        // **Layers, not length** — the same correction as in the
                        // callback, and it had the same cause: "has a length"
                        // meant "has material" while the only way to get one was
                        // to record one. A loop that has been *told* how many
                        // bars it is has a length and nothing in it, and it is
                        // exactly the loop that most needs to start on the
                        // boundary: it will be four bars long either way, and
                        // four bars starting off the grid is four bars wrong.
                        let boundary = if lp.quant.load(Ordering::Relaxed)
                            && lp.n_layers.load(Ordering::Acquire) == 0
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
                                // The press you make most often, and until now
                                // the one that said least. Which layer matters:
                                // "recording" on an empty loop and "recording"
                                // onto layer 5 are different enough that a
                                // display showing neither is the reason nobody
                                // could tell an overdub had started.
                                return if lp.revox.load(Ordering::Relaxed) {
                                    // **Not "onto layer 2".** In Revox there is
                                    // one layer and the head is going over it;
                                    // naming a layer that will never exist is
                                    // how a mode gets blamed for making the
                                    // thing it was told not to make.
                                    format!("loop {} over the tape.", li)
                                } else if layer == 0 {
                                    format!("loop {} recording.", li)
                                } else {
                                    format!("loop {} overdubbing onto layer {}.", li, layer + 1)
                                };
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
            // **Before the take guard, and that is load-bearing.** `t` is
            // matched as `starts_with('t')` — a char, not a word — so every
            // command beginning with a t reaches it first. `tone3000` was
            // silently being read as "claim the last 3000 seconds", which
            // answered with a refusal about cycles and left the tone unchanged:
            // a verb that has an arm, is spelled right, and never arrives.
            //
            // `tools/check-verbs.py` cannot catch this. It asks whether every
            // verb *has* an arm, not whether it reaches the one it meant, and
            // both were true here.
            // How much top the tape keeps, in hertz. Twenty thousand and up is
            // off outright rather than very nearly off.
            _ if rest.starts_with("tone") => {
                let arg = rest[4..].trim();
                if arg.is_empty() {
                    return format!("loop {} {}.", li, tone_words(lp));
                }
                match arg.parse::<f32>() {
                    Ok(hz) if (200.0..=20_000.0).contains(&hz) => {
                        lp.tone.store(hz.to_bits(), Ordering::Relaxed);
                        return format!("loop {} {}.", li, tone_words(lp));
                    }
                    Ok(hz) => return format!("tape tone wants 200 to 20000 Hz, not {}.", hz),
                    _ => return format!("tape tone wants hertz, not `{}`.", arg),
                }
            }
            l if l.starts_with('t') => {
                let secs = l[1..].trim().parse::<f64>().unwrap_or(8.0);
                return take(sh, li, sr, secs, late);
            }
            // **Above `s`, which prefix-matches**, and above the `t` guard for
            // the same reason `tone` is: both are char-matched and would eat a
            // longer verb whole. This file has been bitten twice that way.
            _ if rest.starts_with("src") => {
                let arg = rest[3..].trim();
                if arg.is_empty() {
                    let i = sh.src_of(li);
                    return format!("loop {} records from {}.", li, sh.sources[i].describe());
                }
                match arg.parse::<usize>() {
                    Ok(n) if n >= 1 && n <= sh.sources.len() => {
                        if lp.is_recording() || lp.is_armed() {
                            return format!(
                                "loop {} is listening or writing; changing its input \
                                 mid-take would splice two different rooms together.",
                                li
                            );
                        }
                        lp.src.store(n - 1, Ordering::Release);
                        return format!("loop {} records from {}.", li, sh.sources[n - 1].describe());
                    }
                    Ok(n) => {
                        return format!(
                            "there are {} sources ({}), not {}.",
                            sh.sources.len(),
                            sh.sources
                                .iter()
                                .enumerate()
                                .map(|(i, s)| format!("{} {}", i + 1, s.name))
                                .collect::<Vec<_>>()
                                .join(", "),
                            n
                        )
                    }
                    _ => return format!("`{}` is not a source number.", arg),
                }
            }

            // Fold to mono at playback. Not a capture decision — the audio
            // stays stereo — so this is free to try and free to undo.
            "mono" | "mono1" => {
                lp.mono.store(true, Ordering::Relaxed);
                return format!(
                    "loop {} folds to mono; pan places it rather than balancing it.",
                    li
                );
            }
            "mono0" => {
                lp.mono.store(false, Ordering::Relaxed);
                return format!("loop {} keeps its two channels; pan is a balance.", li);
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
                    Ok(n) => return sparse(sh, li, sr, n),
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
                    // **Where the bar came from, not who the anchor is.** This
                    // named `anchor` unconditionally, which was true while the
                    // grid was always a loop's cycle and prints "from loop 8" —
                    // one past the last loop, the sentinel for "nobody" — the
                    // moment Link is the one supplying it.
                    (true, Some((_, glen))) => {
                        let from = if sh.link_bar_frames.load(Ordering::Relaxed) > 0 {
                            "Link".to_string()
                        } else {
                            match sh.anchor.load(Ordering::Acquire) {
                                a if a < N_LOOPS => format!("loop {}", a),
                                _ => "nowhere".to_string(),
                            }
                        };
                        format!(
                            "loop {} follows the grid ({:.3} s, from {}).",
                            li,
                            glen as f64 / sr as f64,
                            from
                        )
                    }
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
            "o" => return rotate(sh, li),
            // Exact match, and it has to be: `b` would collide with `blank`,
            // and this file has been bitten twice by a prefix guard eating a
            // longer verb — see the note above `tone`.
            "bpm" => return take_tempo(sh, li, sr),
            // Rig-wide, and exact-matched for the reason `bpm` above it is:
            // `g` is already a verb (grid), so a prefix guard here would be a
            // collision rather than a convenience.
            "go" => return start_all(sh, sr),
            "d" => return dense(sh, li),
            "z" => return free_length(sh, li, sr),
            // **Ahead of every prefix guard below and behind every one above.**
            // `len` shares two letters with `lev`, which is matched exactly, and
            // `ph` shares one with `pan` and `pend`, which are matched by their
            // own longer prefixes — so neither can be swallowed. That is worth
            // stating rather than trusting: a verb defined after a looser guard
            // is a verb that silently never runs, which has happened here once
            // already.
            // Rig-wide, so the loop prefix is ignored — the same shape as `arm`,
            // `k` and `m`, and said in the ack so a `3lq4` does not look like it
            // set something on loop 3.
            _ if rest.starts_with("lq") => {
                return match rest[2..].trim().parse::<i64>() {
                    Ok(n) if n >= -1 && n <= 64 => {
                        sh.launch_q.store(n, Ordering::Relaxed);
                        match n {
                            -1 => "launches wait for the bar (rig-wide).".to_string(),
                            0 => "launches do not wait (rig-wide).".to_string(),
                            b => format!(
                                "launches wait for the next {} beat{} (rig-wide).",
                                b,
                                if b == 1 { "" } else { "s" }
                            ),
                        }
                    }
                    Ok(n) => format!("launch quantise wants -1 to 64 beats, not {}.", n),
                    Err(_) => format!("`{}` wants a number of beats.", rest),
                };
            }
            _ if rest.starts_with("len") => {
                return match rest[3..].trim().parse::<usize>() {
                    Ok(n) => set_bars(sh, li, sr, n),
                    Err(_) => format!("`{}` wants a number of bars.", rest),
                };
            }
            _ if rest.starts_with("ph") => {
                return match rest[2..].trim().parse::<usize>() {
                    Ok(n) => place_at(sh, li, n.saturating_sub(1)),
                    Err(_) => format!("`{}` wants a slot number.", rest),
                };
            }
            // Returned rather than printed. This is the one command whose whole
            // point is *where* it put something, and a path printed on the
            // daemon's stdout is a path the app cannot show anyone — so the
            // message goes back as the ack and both callers display it
            // themselves. Printing here as well got it shown twice.
            // Rig-wide, so the loop prefix is ignored — said in the ack for
            // the same reason `arm` and `m` say it. Matched on two characters
            // rather than on `e`, because this file has been bitten twice by a
            // one-character guard silently eating a longer verb defined below
            // it, and `ex` costs nothing to be careful with.
            l if l.starts_with("ex") => return export_set(sh, sr, &l[2..]),
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
            // **An empty tape of a stated length.**
            //
            // Every other way a loop gets its length is by *recording* one: a
            // first take defines the cycle, a multiply extends it. A tape does
            // not work that way — you thread a loop of a chosen length and
            // then play onto it — so Revox needs a way to say "eight seconds,
            // empty, going round" before anything has been played.
            //
            // **One silent layer, not none.** Playback sums `0..n_layers` and
            // the layer being recorded sits *at* `n_layers`, so a loop with no
            // layers is silent even while something is being written into it.
            // In Revox that would matter: the erasing write goes into layer
            // zero, which is exactly the layer that has to be playing for the
            // tape to come round under your hands.
            //
            // Refused rather than applied when the loop has anything in it. It
            // is a way of *starting*, and quietly resizing a loop with material
            // in it would be a trim — a thing this engine does not have and
            // should not grow by accident.
            _ if rest.starts_with("blank") => {
                let arg = rest[5..].trim();
                let secs = match arg.parse::<f64>() {
                    Ok(v) if v > 0.0 => v,
                    Ok(_) => return format!("a tape wants a length in seconds, not {}.", arg),
                    _ => return format!("a tape wants a length in seconds, not `{}`.", arg),
                };
                if lp.is_recording() {
                    return format!("loop {} is recording; finish that first.", li);
                }
                // **Threaded, not recorded** is the test — not the layer count,
                // which cannot tell them apart because a threaded tape has one
                // layer in order to play at all.
                if lp.n_layers.load(Ordering::Acquire) > 0
                    && !lp.threaded.load(Ordering::Relaxed)
                {
                    return format!(
                        "loop {} has something in it; clear it before threading a tape.",
                        li
                    );
                }
                let mut len = (secs * sr as f64).round() as usize;
                if len > sh.max_frames {
                    return format!(
                        "{:.1} s is past --max-secs; the longest tape here is {:.1} s.",
                        secs,
                        sh.max_frames as f64 / sr as f64
                    );
                }
                // The grid rounds it, because only the engine knows where the
                // grid is — and a tape that does not line up with the anchor
                // loop is a tape that drifts against everything else.
                let mut said = String::new();
                if lp.quant.load(Ordering::Relaxed) {
                    if let Some((_, glen)) = sh.grid() {
                        let n = ((len as f64 / glen as f64).round() as usize).max(1);
                        len = n * glen;
                        said = format!(" ({} grid cycle{})", n, if n == 1 { "" } else { "s" });
                    }
                }
                let now = sh.out_frames.load(Ordering::Acquire) as i64;
                sh.zero_layer(li, 0);
                lp.origin.store(now, Ordering::Release);
                lp.loop_len.store(len, Ordering::Release);
                lp.set_layer_shape(0, Shape { len, tail: 0, born: 0 });
                lp.n_layers.store(1, Ordering::Release);
                lp.threaded.store(true, Ordering::Relaxed);
                lp.state.set(PLAYING);
                sh.rebuild_env(li, 0);
                return format!(
                    "loop {} is an empty {:.3} s tape{}, going round.",
                    li,
                    len as f64 / sr as f64,
                    said
                );
            }
            // **Revox mode: the loop becomes a tape.**
            //
            // Entering flattens what is there to one layer, because a tape has
            // no layers and a mode that only half applied would be worse than
            // either. That is not reversible — `rvx0` stops the erasing but
            // does not unfold what was folded — and the ack says so, because a
            // player is entitled to know which of their presses was the one
            // that could not be taken back.
            "rvx" | "rvx1" | "rvx0" => {
                let on = match rest {
                    "rvx1" => true,
                    "rvx0" => false,
                    _ => !lp.revox.load(Ordering::Relaxed),
                };
                if lp.is_recording() {
                    return format!("loop {} is recording; finish that first.", li);
                }
                let was = lp.n_layers.load(Ordering::Acquire);
                lp.revox.store(on, Ordering::Relaxed);
                if on {
                    sh.flatten(li, sh.out_frames.load(Ordering::Acquire) as i64);
                    let now = lp.n_layers.load(Ordering::Acquire);
                    return format!(
                        "loop {} is a tape now, {} a pass{}. Undo is gone.",
                        li,
                        fb_words(lp),
                        if was > now {
                            format!(" ({} layers folded into one)", was)
                        } else {
                            String::new()
                        }
                    );
                }
                return format!("loop {} records in layers again; it is still one layer.", li);
            }
            // What a Revox pass leaves of what was under it, in decibels. Zero
            // is a tape that never erases and -60 is one that replaces.
            //
            // Its own number rather than `dec`'s: they are the same musical idea
            // by two mechanisms, one destroying and one not, and a single value
            // meaning "resolution here, erase head there" depending on a flag is
            // exactly the overload this engine keeps refusing.
            _ if rest.starts_with("fb") => {
                let arg = rest[2..].trim();
                if arg.is_empty() {
                    return format!("a Revox pass on loop {} leaves {} a pass.", li, fb_words(lp));
                }
                match arg.parse::<f32>() {
                    Ok(db) if db > 0.0 => {
                        return format!(
                            "feedback is a loss, so it wants zero or less; {} would run away.",
                            db
                        )
                    }
                    Ok(db) if db >= -60.0 => {
                        let g = if db <= -60.0 { 0.0 } else { 10f32.powf(db / 20.0) };
                        lp.fb.store(g.to_bits(), Ordering::Relaxed);
                        return format!("a Revox pass on loop {} leaves {} a pass.", li, fb_words(lp));
                    }
                    Ok(db) => return format!("feedback wants 0 to -60 dB, not {}.", db),
                    _ => return format!("feedback wants decibels, not `{}`.", arg),
                }
            }
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
            // This loop's level, in decibels, with **silence at the bottom
            // rather than a very quiet loop**. A fader that cannot reach zero
            // is a fader you do not trust, and -60 dB is inaudible anyway —
            // saying "silent" is more honest than reporting a number nobody can
            // hear.
            //
            // Above unity is refused rather than clamped, for the same reason
            // decay refuses positive: a level control that quietly declined to
            // do what it was told would be worse than one that says no.
            _ if rest.starts_with("vol") => {
                let arg = rest[3..].trim();
                if arg.is_empty() {
                    return format!("loop {} {}.", li, vol_words(lp));
                }
                match arg.parse::<f32>() {
                    Ok(db) if db > 0.0 => {
                        return format!("a loop plays at unity or below; {} dB would clip.", db)
                    }
                    Ok(db) if db >= -60.0 => {
                        let g = if db <= -60.0 { 0.0 } else { 10f32.powf(db / 20.0) };
                        lp.vol.store(g.to_bits(), Ordering::Relaxed);
                        return format!("loop {} {}.", li, vol_words(lp));
                    }
                    Ok(db) => return format!("level wants 0 to -60 dB, not {}.", db),
                    _ => return format!("level wants decibels, not `{}`.", arg),
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
                lp.cleared();
                for l in 0..MAX_LAYERS {
                    sh.zero_layer(li, l);
                    lp.set_layer_shape(l, Shape { len: 0, tail: 0, born: 0 });
                }
                sh.clear_env(li);
                sh.release_anchor(li);
                return format!("loop {} cleared.", li);
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
                let inp = sh
                    .in_peak
                    .iter()
                    .map(|p| f32::from_bits(p.swap(0, Ordering::Relaxed)))
                    .fold(0.0f32, f32::max);
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
        // **Interleaved stereo out.** The arena holds two channels and the WAV
        // takes them both; a take saved as its left half would be the mono bug
        // reappearing at the one point where it cannot be undone.
        let samples: Vec<f32> = (0..len)
            .flat_map(|p| (0..CHANNELS).map(move |ch| (p, ch)))
            .map(|(p, ch)| sh.read(li, l, p, ch))
            .collect();
        // Zero-padded because these become a SuperDirt sample bank, and its
        // loader sorts the folder lexicographically to assign `n` indices.
        // Unpadded, a tenth layer would sort between the first and the second
        // and every index past it would name the wrong audio — silently, since
        // nothing downstream can tell a misordered bank from an intended one.
        // `MAX_LAYERS` is 4 today, so this is insurance bought while it is free.
        let file = format!("layer-{:02}.wav", l);
        if let Err(e) = std::fs::write(dir.join(&file), crate::wav::wav_bytes(&samples, sr, CHANNELS as u16)) {
            return format!("could not write {}: {}", file, e);
        }
        entries.push(format!(
            r#"{{"file":"{}","len":{},"channels":{},"period":{},"phase":{}}}"#,
            file, len, CHANNELS, period, phase
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
/// Whether this loop can be written into *now*, which is a narrower question
/// than whether it is playing plainly.
///
/// **Speed and direction stopped being refusals on 2026-08-30.** The write head
/// follows the play head now (see the input callback), so a loop running
/// backwards or at half speed takes an overdub and gives back what you played.
/// That was the whole of the old refusal and it is gone.
///
/// What is left is two things the span-write cannot answer for:
///
///   - **A pendulum** reflects rather than wrapping, so `raw_pos` is not the
///     position — the fold happens after it. A write head reading raw would run
///     off the end and come back through the audio it just laid down.
///   - **A tape at speed.** Revox reads, filters and writes one slot per frame;
///     it is a physical model of a head passing over oxide, and a head that
///     covers two slots or half of one is a different machine. Threading a tape
///     is a deliberate act, so being told to put the speed back is fair.
///
/// And a *first* take still wants unity, because an empty loop has no play head
/// to follow: the linear write is all there is, and the speed it would be
/// played back at is not a thing the recording can compensate for.
fn not_writable(lp: &Loop, li: usize) -> Option<String> {
    if lp.pendulum.load(Ordering::Relaxed) {
        return Some(format!(
            "loop {} is swinging, and a write head cannot follow a playhead that \
             turns round mid-pass; `{}pend0` to record into it.",
            li, li
        ));
    }
    if lp.revox.load(Ordering::Relaxed) && !lp.plain() {
        return Some(format!(
            "loop {} is a tape running at x{}; a tape head passes over the oxide \
             once per frame, so put the speed back with `{}sp1` to record onto it.",
            li,
            lp.speed().abs(),
            li
        ));
    }
    if lp.loop_len.load(Ordering::Acquire) == 0 {
        return not_plain(lp, li);
    }
    None
}

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
/// Every loop that holds something, rendered and written as one WAV each.
///
/// ## Export is not save
///
/// `save_take` writes one loop's *layers*, raw. That is the session: itajara's
/// own format, lossless, engine-shaped, the thing you reload to keep
/// overdubbing tomorrow. This writes *loops*, flattened and rendered, which is
/// what everything outside this daemon means by the word. Two artefacts for two
/// readers, and neither is a better version of the other — which is why both
/// verbs exist and why neither replaced the other.
///
/// ## What is deliberately not in the audio
///
/// Chance, one-shot and mute — see `loop_at` for the line. They are written
/// into the manifest as numbers instead, so a receiver that wants them can have
/// them, and every receiver these files are going to can: Ableton follows a
/// clip, Loopy has one-shots, a Morphagene or a Lubadh does chance with a knob.
/// **What we do not render, we record.**
///
/// ## And what is deliberately not here at all
///
/// No reel, no splice markers, no module-shaped anything. `msm` already knows
/// what a Morphagene wants and what an Arbhar wants, and it should stay the one
/// place that does. What only this daemon can supply is honest audio with its
/// bar count attached, so that is all it supplies.
fn export_set(sh: &Shared, sr: u32, name: &str) -> String {
    // Checked across every loop before anything is written, rather than per
    // loop as it goes: a half-written folder that stopped at loop 5 because
    // loop 5 was recording is worse than one that never started.
    for li in 0..N_LOOPS {
        let lp = sh.lp(li);
        if lp.is_recording() || lp.is_armed() {
            return format!(
                "loop {} is still recording — finish it before exporting the set.",
                li
            );
        }
    }

    let name = safe_name(name);
    let dir = sh.takes_dir.join(&name);
    if let Err(e) = std::fs::create_dir_all(&dir) {
        return format!("could not make {}: {}", dir.display(), e);
    }

    let quantum = f64::from_bits(sh.link_quantum.load(Ordering::Relaxed));
    let beats_per_bar = if quantum >= 1.0 { quantum } else { 4.0 };

    let mut entries: Vec<String> = Vec::new();
    let mut wrote: Vec<String> = Vec::new();
    for li in 0..N_LOOPS {
        let lp = sh.lp(li);
        let Some(samples) = sh.render_loop(li) else {
            continue;
        };
        let frames = samples.len() / CHANNELS;
        let bars = lp.cycles.load(Ordering::Acquire);

        // **Numbered from one, unlike everything else on this wire.** The rule
        // in here is that the daemon counts from zero and the surfaces count
        // from one, and a filename is a surface: it is read by a person in
        // Finder, by Ableton's browser and by msm, and none of them have the
        // ack beside them to explain a `loop-0.wav`. The ack below says the
        // mapping out loud so the seam is visible where it happens.
        let file = format!("loop-{}.wav", li + 1);

        // Only when the loop is doing the plain thing. At half speed or on a
        // pendulum there is no whole number of beats to declare, and a wrong
        // `acid` chunk warps confidently to the wrong grid — which would look
        // like our bug in someone else's application.
        let acid = if bars > 0 && lp.plain() {
            Some(crate::wav::Acid {
                beats: (bars as f64 * beats_per_bar).round() as u32,
                tempo: tempo_of(len_of(lp), bars, sr, quantum) as f32,
                beats_per_bar: beats_per_bar.round() as u16,
            })
        } else {
            None
        };
        let tempo_field = match &acid {
            Some(a) => format!("{:.4}", a.tempo),
            None => "null".to_string(),
        };

        if let Err(e) = std::fs::write(
            dir.join(&file),
            crate::wav::wav_bytes_acid(&samples, sr, CHANNELS as u16, acid),
        ) {
            return format!("could not write {}: {}", file, e);
        }
        entries.push(format!(
            concat!(
                r#"{{"file":"{}","loop":{},"frames":{},"secs":{:.6},"bars":{},"tempo":{},"#,
                r#""chance":{:.4},"oneShot":{},"muted":{}}}"#
            ),
            file,
            li + 1,
            frames,
            frames as f64 / sr as f64,
            bars,
            tempo_field,
            lp.chance_of(),
            lp.one_shot.load(Ordering::Relaxed),
            lp.muted.load(Ordering::Relaxed),
        ));
        wrote.push(file);
    }

    if entries.is_empty() {
        return "nothing to export — no loop has anything in it.".into();
    }

    // No timestamp, for the same reason `save_take` has none: these are bound
    // for amphora, which keys an artefact by the hash of its content.
    let manifest = format!(
        concat!(
            "{{\n  \"version\": 1,\n  \"kind\": \"export\",\n  \"sampleRate\": {},\n",
            "  \"beatsPerBar\": {},\n  \"loops\": [\n    {}\n  ]\n}}\n"
        ),
        sr,
        beats_per_bar,
        entries.join(",\n    ")
    );
    if let Err(e) = std::fs::write(dir.join("export.json"), manifest) {
        return format!("wrote the audio but not the manifest: {}", e);
    }

    format!(
        "exported {} loop{} to {}: {} — numbered as the board labels them, so \
         loop 0 is loop-1.wav.",
        wrote.len(),
        if wrote.len() == 1 { "" } else { "s" },
        dir.display(),
        wrote.join(", ")
    )
}

/// A loop's own length, named so the `acid` arithmetic above reads as arithmetic.
fn len_of(lp: &Loop) -> usize {
    lp.loop_len.load(Ordering::Acquire)
}

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
            best = best.max(sh.sample_at(li, 0, (c * len + d) % new_len, 0).abs());
            if c * len + len > d {
                let back = (c * len + new_len - d - 1) % new_len;
                best = best.max(sh.sample_at(li, 0, back, 0).abs());
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
        let v = (0..CHANNELS).map(|c| sh.read(li, layer, i, c).abs()).fold(0.0f32, f32::max);
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
        if (0..CHANNELS).map(|c| sh.read(li, layer, prev, c).abs()).fold(0.0f32, f32::max) <= 0.01 {
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

    /// A `Shared` small enough to build in a test, so the renderer can be
    /// asked what it actually produces.
    ///
    /// **Duplicated from the literal in `start`, on purpose.** The alternative
    /// was a constructor taking thirty arguments so that one caller could pass
    /// zeros. Adding a field to `Shared` breaks this at compile time, which is
    /// the only kind of drift worth defending against here.
    fn rig(max_frames: usize) -> Shared {
        Shared {
            arena: (0..N_LOOPS * MAX_LAYERS * max_frames * CHANNELS)
                .map(|_| AtomicU32::new(0))
                .collect(),
            max_frames,
            ring: (0..CHANNELS).map(|_| AtomicU32::new(0)).collect(),
            ring_len: 1,
            in_peak: vec![AtomicU32::new(0)],
            sources: vec![Source::mono("test", 0)],
            loops: (0..N_LOOPS).map(|_| Loop::new()).collect(),
            selected: AtomicUsize::new(0),
            anchor: AtomicUsize::new(N_LOOPS),
            out_frames: AtomicUsize::new(0),
            in_frames: AtomicUsize::new(0),
            k: AtomicI64::new(0),
            k_set: AtomicBool::new(false),
            p0: Mutex::new(None),
            buffer_frames: AtomicU32::new(0),
            click: AtomicBool::new(false),
            preroll: AtomicUsize::new(0),
            arm_thresh: AtomicU32::new(0.01f32.to_bits()),
            arm_reach: AtomicUsize::new(0),
            max_fade: 0,
            monitor: AtomicBool::new(false),
            out_peak: AtomicU32::new(0),
            p0_needed: AtomicBool::new(false),
            p0_frame: AtomicUsize::new(0),
            device_lost: AtomicBool::new(false),
            reopens: AtomicUsize::new(0),
            takes_dir: std::env::temp_dir().join("itajara-test-takes"),
            ack: Mutex::new(String::new()),
            ack_seq: AtomicUsize::new(0),
            link_micros: AtomicI64::new(0),
            link_beat: AtomicU64::new(0),
            link_tempo: AtomicU64::new(0),
            link_quantum: AtomicU64::new(0),
            link_frame: AtomicUsize::new(0),
            link_bar_frames: AtomicUsize::new(0),
            link_bar_origin: AtomicI64::new(0),
            launch_q: AtomicI64::new(-1),
            link_anchors: AtomicUsize::new(0),
            link_rejected: AtomicUsize::new(0),
        }
    }

    /// Fill one layer with a constant, and declare its shape.
    fn lay(sh: &Shared, li: usize, layer: usize, len: usize, v: f32) {
        for p in 0..len {
            for ch in 0..CHANNELS {
                sh.cell(li, layer, p, ch).store(v.to_bits(), Ordering::Relaxed);
            }
        }
        let lp = sh.lp(li);
        lp.l_len[layer].store(len, Ordering::Release);
        lp.l_period[layer].store(1, Ordering::Release);
        lp.l_phase[layer].store(0, Ordering::Release);
        lp.l_tail[layer].store(0, Ordering::Release);
        lp.l_gain[layer].store(1.0f32.to_bits(), Ordering::Release);
        lp.n_layers.store(layer + 1, Ordering::Release);
    }

    /// A loop of `len` holding one layer, at the origin, ready to render.
    fn one_layer_loop(sh: &Shared, li: usize, len: usize, v: f32) {
        lay(sh, li, 0, len, v);
        let lp = sh.lp(li);
        lp.loop_len.store(len, Ordering::Release);
        lp.origin.store(0, Ordering::Relaxed);
    }

    /// **A rendered loop is one cycle, and the placement is inside it.**
    ///
    /// The bar-on-the-third-of-four case, which is the one that made me reach
    /// for an LCM that turns out not to exist: `layer_pos` slots by
    /// `(pos / layer_len) % period`, so the four bars are already `loop_len`
    /// and a cycle is the whole of it. If that ever stops being true this test
    /// goes quiet in exactly the wrong way — silent thirds and a fourth that
    /// sounds — so it asserts each quarter separately.
    #[test]
    fn a_sparse_layer_renders_where_it_lands() {
        let sh = rig(LEN);
        let bar = 100;
        one_layer_loop(&sh, 0, bar, 0.5);
        let lp = sh.lp(0);
        lp.loop_len.store(4 * bar, Ordering::Release);
        lp.l_period[0].store(4, Ordering::Release);
        lp.l_phase[0].store(2, Ordering::Release); // the third of four
        let out = sh.render_loop(0).expect("renders");
        assert_eq!(out.len(), 4 * bar * CHANNELS, "one cycle, not more");
        let quarter = |q: usize| out[q * bar * CHANNELS];
        assert_eq!(quarter(0), 0.0);
        assert_eq!(quarter(1), 0.0);
        assert_eq!(quarter(2), 0.5, "the third quarter is where it was placed");
        assert_eq!(quarter(3), 0.0);
    }

    /// **Chance, one-shot and mute are not baked in.**
    ///
    /// The rule the export rests on: those three decide whether you hear a loop
    /// this time round, and every receiver these files go to can decide that
    /// for itself. A render that honoured them would hand Ableton one roll of
    /// the dice and call it the loop — and worse, a muted loop would export as
    /// a folder of silence with nothing to say why.
    #[test]
    fn the_render_ignores_what_only_decides_whether_you_hear_it() {
        let sh = rig(LEN);
        one_layer_loop(&sh, 0, 100, 0.5);
        let lp = sh.lp(0);
        lp.chance.store(0.0f32.to_bits(), Ordering::Relaxed);
        lp.one_shot.store(true, Ordering::Relaxed);
        lp.muted.store(true, Ordering::Relaxed);
        let out = sh.render_loop(0).expect("renders anyway");
        assert!(out.iter().any(|&v| v != 0.0), "silence would be the bug");

        // And the live path still honours all three, which is the other half of
        // the claim: this is a second mode, not a change of behaviour.
        let mut rng = SmallRng::seed_from_u64(1);
        assert_eq!(sh.loop_at(0, 0, &mut rng, true), [0.0; CHANNELS]);
    }

    /// Speed is audio, so it *is* rendered — and it changes the file's length.
    #[test]
    fn half_speed_renders_twice_the_file() {
        let sh = rig(LEN);
        one_layer_loop(&sh, 0, 100, 0.5);
        sh.lp(0).speed.store(0.5f64.to_bits(), Ordering::Relaxed);
        let out = sh.render_loop(0).expect("renders");
        assert_eq!(out.len(), 200 * CHANNELS);
    }

    /// **The span write puts one input frame's worth into the loop, whatever
    /// the rate.**
    ///
    /// The law the overdub-at-speed branch rests on, checked as arithmetic
    /// rather than through the audio callback — which needs a device. For one
    /// input frame the head covers `[a, b)`, and the weights it hands out are
    /// each slot's share of that interval. Two properties matter and both are
    /// here: the weights sum to the span, so half speed averages its two frames
    /// into one slot instead of doubling them; and at unity there is exactly one
    /// slot at weight one, so the fast path and the moving path agree at the
    /// only rate where both run.
    fn spans(a: f64, b: f64) -> Vec<(i64, f32)> {
        let (lo, hi) = if a <= b { (a, b) } else { (b, a) };
        let mut out = Vec::new();
        let mut slot = lo.floor() as i64;
        while (slot as f64) < hi {
            let cover =
                (((slot + 1) as f64).min(hi) - (slot as f64).max(lo)).max(0.0) as f32;
            if cover > 0.0 {
                out.push((slot, cover));
            }
            slot += 1;
        }
        out
    }

    #[test]
    fn one_input_frame_lands_once_however_fast_the_head_is_moving() {
        // Unity: one slot, full weight. The same answer the linear branch gives,
        // which is why that branch can stay and be trusted.
        assert_eq!(spans(10.0, 11.0), vec![(10, 1.0)]);

        // Backwards at unity: one slot, full weight, walking down. No
        // resampling at all — this is the case that is exact.
        assert_eq!(spans(11.0, 10.0), vec![(10, 1.0)]);

        // Half speed: two consecutive input frames share a slot at half each,
        // which is their average and not their sum. Getting this wrong is a
        // loop that comes back 6 dB hot and only when it is slowed down.
        let first = spans(10.0, 10.5);
        let second = spans(10.5, 11.0);
        assert_eq!(first, vec![(10, 0.5)]);
        assert_eq!(second, vec![(10, 0.5)]);
        let total: f32 = first.iter().chain(second.iter()).map(|(_, w)| w).sum();
        assert!((total - 1.0).abs() < 1e-6, "two frames, one slot's worth");

        // Double speed: one frame fills two slots outright — a zero-order hold,
        // which is the honest thing to do with samples that were never taken.
        assert_eq!(spans(10.0, 12.0), vec![(10, 1.0), (11, 1.0)]);

        // A stopped loop writes nowhere. There is no position for it to go to,
        // and picking one would smear a note into a single slot for as long as
        // a foot stayed down.
        assert!(spans(10.0, 10.0).is_empty());

        // Every weight is a share of a slot, so none can exceed one — the
        // property that keeps a rate the arena has never seen from writing
        // something louder than was played.
        for (num, den) in [(1, 3), (2, 3), (7, 4), (13, 5)] {
            let step = num as f64 / den as f64;
            for (_, w) in spans(3.25, 3.25 + step) {
                assert!(w <= 1.0 + 1e-6, "weight {} over span {}", w, step);
            }
        }
    }

    /// An empty loop is skipped rather than exported as silence.
    #[test]
    fn nothing_recorded_renders_to_nothing() {
        let sh = rig(LEN);
        assert!(sh.render_loop(0).is_none());
    }

    /// A loop with its position zero at output frame zero.
    fn at_origin() -> Loop {
        let lp = Loop::new();
        lp.origin.store(0, Ordering::Relaxed);
        lp
    }

    /// **A cleared loop must not remember how long it was.**
    ///
    /// The failure this pins was invisible from inside the engine: a slot with
    /// `loop_len == 0` and `cycles == 4` behaves correctly at every call site —
    /// `loop_grid` checks the length first and bails — so nothing here went
    /// wrong. What went wrong was on the surface. The Twister draws the bars
    /// ring from `cycles` and writes ring positions *back* to the device, so
    /// the encoder physically sat at four bars on a loop that had none, and
    /// turning it to four moved nothing and sent nothing. The next take
    /// recorded open-ended, and it did so only on the second run of a recipe.
    ///
    /// So the assertion is not about behaviour, it is about **agreement**: two
    /// fields describe one fact and they have to be cleared together. That is
    /// the class of bug this project keeps finding — see `sized-but-empty`, the
    /// same pair read the other way round.
    /// The whole of what `bpm` computes, and the case it was asked for.
    #[test]
    fn a_loop_that_ran_long_gives_back_a_slower_tempo() {
        let sr = 48_000;
        // Four bars at 120 in four: 2 s a bar, 8 s the loop.
        assert!((tempo_of(8 * sr as usize, 4, sr, 4.0) - 120.0).abs() < 1e-9);

        // The case this exists for. You played four bars against a 120 click
        // and took 8.15 s over them; the click comes to you rather than the
        // audio being stretched to it.
        let long = (8.15 * sr as f64) as usize;
        let bpm = tempo_of(long, 4, sr, 4.0);
        assert!(bpm < 120.0, "running long must give a slower tempo, got {}", bpm);
        assert!((bpm - 117.79).abs() < 0.01, "got {}", bpm);

        // Metre comes from Link, so the same audio in three is a faster tempo —
        // three beats to fill the same bar. A hard-coded four would be right in
        // 4/4 and quietly wrong everywhere else.
        assert!((tempo_of(8 * sr as usize, 4, sr, 3.0) - 90.0).abs() < 1e-9);
        // A quantum nobody has sent reads as four rather than as none.
        assert_eq!(tempo_of(8 * sr as usize, 4, sr, 0.0), tempo_of(8 * sr as usize, 4, sr, 4.0));

        // Bars, not cycles: the same audio called one bar is a quarter of the
        // tempo of the same audio called four.
        assert!(
            (tempo_of(8 * sr as usize, 1, sr, 4.0) * 4.0 - tempo_of(8 * sr as usize, 4, sr, 4.0))
                .abs()
                < 1e-9
        );
    }

    /// **Balance is not pan, and the difference is what a centred loop sounds
    /// like.**
    ///
    /// The knob was equal-power throughout, which is right for placing one
    /// signal and wrong for two that are already in a field: at centre it takes
    /// 3 dB off both sides for nothing, and turning it collapses a width that
    /// was recorded rather than inventing one.
    #[test]
    fn a_stereo_loop_is_balanced_and_a_folded_one_is_panned() {
        let lp = Loop::new();

        // Centre. A balance leaves both sides alone; a pan is 3 dB down on each
        // because it is spending the difference on placing a mono signal.
        lp.pan.store(64, Ordering::Relaxed);
        let (bl, br) = lp.balance_gains();
        // **Exactly**, not nearly. See `pan_position`: dividing the whole
        // travel by 127 put centre at 0.5039 and left every centred loop 0.07 dB
        // down on one side, which export would now write into the file.
        assert_eq!((bl, br), (1.0, 1.0));
        let (pl, pr) = lp.pan_gains();
        assert!((pl - 0.707).abs() < 0.02 && (pr - 0.707).abs() < 0.02, "{} {}", pl, pr);

        // Hard over: silence on the far side, unity on the near one.
        lp.pan.store(0, Ordering::Relaxed);
        let (bl, br) = lp.balance_gains();
        assert!((bl - 1.0).abs() < 1e-6 && br.abs() < 1e-6);
        lp.pan.store(127, Ordering::Relaxed);
        let (bl, br) = lp.balance_gains();
        assert!(bl.abs() < 1e-6 && (br - 1.0).abs() < 1e-6);

        // **Attenuating only, at every position.** A balance that boosted would
        // make a loop louder than it was recorded and there is no headroom to
        // spend on that.
        for v in 0..=127u8 {
            lp.pan.store(v as usize, Ordering::Relaxed);
            let (l, r) = lp.balance_gains();
            assert!(l <= 1.0 + 1e-6 && r <= 1.0 + 1e-6, "at {}: {} {}", v, l, r);
            assert!(l >= 0.0 && r >= 0.0);
        }
    }

    /// A mono jack is a source whose two channels are the same input, and
    /// nothing downstream needs a special case for it.
    #[test]
    fn a_one_channel_source_reads_the_same_input_twice() {
        let s = Source::mono("di", 2);
        assert_eq!(s.ch, [2, 2]);
        assert!(s.is_mono());
        assert_eq!(s.describe(), "di (in 3)");

        let board = Source { name: "board".into(), ch: [0, 1] };
        assert!(!board.is_mono());
        assert_eq!(board.describe(), "board (in 1+2)");
    }

    #[test]
    fn clearing_forgets_the_length_and_the_bar_count_together() {
        let lp = Loop::new();
        // Sized and empty, as `len<n>` leaves it: four bars of a two-second bar.
        lp.loop_len.store(4 * 96_000, Ordering::Release);
        lp.cycles.store(4, Ordering::Release);
        lp.rec_len.store(4 * 96_000, Ordering::Release);
        lp.close_at.store(1_234_567, Ordering::Release);

        lp.cleared();

        assert_eq!(lp.loop_len.load(Ordering::Acquire), 0, "kept its length");
        assert_eq!(
            lp.cycles.load(Ordering::Acquire),
            0,
            "kept its bar count, so the encoder still reads four bars on an \
             empty loop and cannot be turned to four"
        );
        assert_eq!(lp.rec_len.load(Ordering::Acquire), 0, "kept an asked-for length");
        assert_eq!(
            lp.close_at.load(Ordering::Acquire),
            i64::MIN,
            "kept a timer pointed at a take nobody has played"
        );
        // And is indistinguishable from one that was never touched, on every
        // field that describes a length.
        let fresh = Loop::new();
        assert_eq!(
            lp.loop_len.load(Ordering::Acquire),
            fresh.loop_len.load(Ordering::Acquire)
        );
        assert_eq!(
            lp.cycles.load(Ordering::Acquire),
            fresh.cycles.load(Ordering::Acquire)
        );
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

    /// A cleared slot has nobody's habits.
    ///
    /// Written after `quant` was found surviving a clear on the running daemon
    /// (2026-08-24) — every other mode reset and `grid` stayed lit, so a cleared
    /// slot silently waited for the next bar before recording. The list is
    /// exhaustive on purpose: the previous version of this test checked three
    /// fields, and the field it did not check was the one that was wrong.
    #[test]
    fn a_cleared_slot_has_nobody_s_habits() {
        let lp = at_origin();

        // Turn on everything a player can turn on.
        lp.adopt(0, LEN, -0.5, true);
        lp.muted.store(true, Ordering::Relaxed);
        lp.pan.store(100, Ordering::Relaxed);
        lp.one_shot.store(true, Ordering::Relaxed);
        lp.level_arm.store(true, Ordering::Relaxed);
        lp.quant.store(true, Ordering::Relaxed);
        lp.fade.store(250, Ordering::Relaxed);
        lp.decay.store(0.5f32.to_bits(), Ordering::Relaxed);
        lp.chance.store(0.5f32.to_bits(), Ordering::Relaxed);
        lp.vol.store(0.001f32.to_bits(), Ordering::Relaxed);
        lp.n_layers.store(3, Ordering::Release);
        lp.loop_len.store(LEN, Ordering::Release);

        lp.cleared();

        assert_eq!(lp.speed(), 1.0, "speed");
        assert!(!lp.pendulum.load(Ordering::Relaxed), "pendulum");
        assert!(!lp.muted.load(Ordering::Relaxed), "muted");
        assert_eq!(lp.pan.load(Ordering::Relaxed), 64, "pan");
        assert!(!lp.one_shot.load(Ordering::Relaxed), "one shot");
        assert!(!lp.level_arm.load(Ordering::Relaxed), "level arm");
        assert!(!lp.quant.load(Ordering::Relaxed), "quantise");
        assert_eq!(lp.fade.load(Ordering::Relaxed), 0, "fade");
        assert_eq!(f32::from_bits(lp.decay.load(Ordering::Relaxed)), 1.0, "decay");
        assert_eq!(f32::from_bits(lp.chance.load(Ordering::Relaxed)), 1.0, "chance");
        assert_eq!(f32::from_bits(lp.vol.load(Ordering::Relaxed)), 1.0, "level");
        assert_eq!(lp.n_layers.load(Ordering::Acquire), 0, "layers");
        assert_eq!(lp.loop_len.load(Ordering::Acquire), 0, "length");
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
