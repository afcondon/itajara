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
//! `--max-secs`, `--loops` and `--layers` are. At the defaults — eight loops, eight
//! layers, thirty seconds, 48 kHz — the arena is 351 MiB.
//!
//! **This said 46 MB until 2026-08-25**, which was true of some earlier set of
//! defaults and of nothing since; the figure beside `DEFAULT_LOOPS` was right and
//! this one had simply never been recomputed. Both are now derived from the
//! same arithmetic in the comment on that constant.

use std::path::PathBuf;
use std::sync::atomic::Ordering;

mod callbacks;
mod commit;
mod control;
mod copy;
mod cycle;
mod dispatch;
mod edit;
mod export;
mod guards;
mod layer;
mod loop_state;
mod next_take;
mod phase;
mod run;
mod selftest;
mod shared;
mod verb;

pub use dispatch::dispatch;
pub use loop_state::Loop;
pub(crate) use layer::Layer;
pub(crate) use next_take::NextTake;
pub(crate) use phase::Phase;
pub use run::run;
pub use shared::Shared;
pub(crate) use run::resolve_residual;

/// The playhead arithmetic, which is the one part of speed that can be checked
/// without a cable.
///
/// `align` proves where recorded audio *lands*; these prove where the playhead
/// *is*, which is a different claim and the one this change actually makes. The
/// property that matters most is the last one: a speed change must not move the
/// audio, and that is a statement about two calls to `play_pos` either side of
/// an `adopt` rather than about anything anyone can hear.
#[cfg(test)]
pub(crate) mod tests;

/// The phase machine held to the Glassbox artifact: the mapping from the
/// engine's byte-and-facts to the artifact's states, and the replay of its
/// conformance vectors.
#[cfg(test)]
mod conformance;

/// How deep a loop can be stacked.
///
/// **Four, down from eight on 2026-08-29**, because the arena is
/// `loops × layers × frames × channels` and layers were the cheapest of those
/// to give back: eight were never used, and halving them buys twice the loop
/// length for the same footprint. Undo and redo still walk the whole stack, so
/// the ceiling is a ceiling and not a discipline — `t` and `r` both refuse at
/// it, and say so.
///
/// **A default since 2026-09-04**, not a constant: `--layers` sets it, and
/// everything that used to read the constant reads `Shared::max_layers`.
pub const DEFAULT_LAYERS: usize = 4;

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

/// Transport states are `Phase` (`phase.rs`), stored as its byte because the
/// audio thread reads it every buffer. The two below are the byte as a
/// *request* — the value `NextTake` carries until the output callback turns
/// it into a phase — where `FIRE` also lives, which is a request and never a
/// state. Same values as the phases they name.
const ARMED: u8 = Phase::Armed as u8;

const PLAYING: u8 = Phase::Playing as u8;

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
pub(crate) struct Shape {
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
/// **192, up from 48 on 2026-09-04.** Forty-eight drew a loop as a fat band;
/// the Edit panel's six hundred showed what the slot was hiding, and the
/// slot is where a loop is watched. Four times the bytes on the wire —
/// eight loops of four layers is six kilobytes a snapshot, thirty a second,
/// on localhost — for a picture that says which loop this is.
pub(crate) const ENV_BUCKETS: usize = 192;

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
    /// Whether `--max-secs` was given, so `--fixed-secs` can stand in for it.
    pub max_secs_given: bool,
    /// How many loops and how many layers each; the arena is their product.
    pub loops: usize,
    pub layers: usize,
    /// Every loop threaded as an empty tape of this length at startup and
    /// after a clear, so a first recording closes itself there. For rigs
    /// whose destination has a fixed length — an Arbhar layer is ten seconds.
    pub fixed_secs: Option<f64>,
    /// `--yes`: take the memory footprint as read instead of asking.
    pub yes: bool,
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
            max_secs_given: false,
            loops: DEFAULT_LOOPS,
            layers: DEFAULT_LAYERS,
            fixed_secs: None,
            yes: false,
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
/// `loops × layers × max_secs × 4 bytes`, so eight loops of eight layers
/// at the default thirty seconds and 48 kHz is **351 MiB**, up from 263. It is
/// allocated once and never touched by the allocator again.
///
/// **A default since 2026-09-04**, not a constant: `--loops` sets it, with
/// no ceiling but memory, and everything that used to read the constant
/// reads `Shared::n_loops`. The arena line above is now `loops × layers × …`.
pub const DEFAULT_LOOPS: usize = 8;

/// What `anchor` holds when no loop owns the grid. Any loop index is below
/// it, however many loops `--loops` asked for.
pub const NO_ANCHOR: usize = usize::MAX;

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
pub(crate) struct AtomicU8Wrapper(std::sync::atomic::AtomicU8);

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
}
