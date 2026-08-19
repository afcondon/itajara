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

use crate::measure::{choose_input, choose_output, signed_secs, Width};

pub const MAX_LAYERS: usize = 8;

/// Transport states, as a `u8` because the audio thread reads it every buffer.
const IDLE: u8 = 0;
/// Waiting for the output callback to stamp the exact frame recording begins.
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

pub struct Opts {
    pub device: String,
    pub in_ch: usize,
    pub out_ch: usize,
    pub residual: f64,
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
}

impl Default for Opts {
    fn default() -> Self {
        Opts {
            device: String::new(),
            in_ch: 0,
            out_ch: 0,
            residual: 252.0,
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
        }
    }
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
    /// Played backwards.
    ///
    /// A *resolution*, like `period` and `phase` — the samples are untouched and
    /// `pos` is simply read from the other end. Length-preserving, so a reversed
    /// loop stays on whatever grid it closed on, and reversible at no cost
    /// because nothing was rewritten.
    pub reverse: AtomicBool,
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
    overflowed: AtomicBool,
    /// Output frame at which the layer being recorded has its position zero.
    /// Equal to `origin` for a first recording; for a multiply it is the cycle
    /// boundary the multiply started on, which is also where the *new* loop's
    /// position zero will end up.
    rec_from: AtomicI64,
}

impl Loop {
    fn new() -> Self {
        Loop {
            loop_len: AtomicUsize::new(0),
            n_layers: AtomicUsize::new(0),
            l_len: (0..MAX_LAYERS).map(|_| AtomicUsize::new(0)).collect(),
            l_period: (0..MAX_LAYERS).map(|_| AtomicUsize::new(1)).collect(),
            l_phase: (0..MAX_LAYERS).map(|_| AtomicUsize::new(0)).collect(),
            origin: AtomicI64::new(0),
            muted: AtomicBool::new(false),
            reverse: AtomicBool::new(false),
            pan: AtomicUsize::new(64),
            state: AtomicU8Wrapper::new(IDLE),
            request: AtomicU8Wrapper::new(0),
            request_at: AtomicI64::new(i64::MIN),
            quant: AtomicBool::new(false),
            reached: AtomicUsize::new(0),
            overflowed: AtomicBool::new(false),
            rec_from: AtomicI64::new(0),
        }
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
    pub fn layer_shape(&self, layer: usize) -> (usize, usize, usize) {
        (
            self.l_len[layer].load(Ordering::Relaxed),
            self.l_period[layer].load(Ordering::Relaxed).max(1),
            self.l_phase[layer].load(Ordering::Relaxed),
        )
    }
    /// A freshly committed layer: its own length, sounding every time round.
    ///
    /// Written *before* `n_layers` is incremented everywhere it is used. The
    /// output callback plays `0..n_layers`, so publishing the layer first and
    /// its length second leaves a window in which the mix reads a length of
    /// zero and drops it — a buffer of silence at the exact moment a take
    /// lands, which is the least forgivable place for one.
    fn set_layer_shape(&self, layer: usize, len: usize) {
        self.l_len[layer].store(len, Ordering::Release);
        self.l_period[layer].store(1, Ordering::Release);
        self.l_phase[layer].store(0, Ordering::Release);
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
    fn sample_at(&self, li: usize, layer: usize, pos: usize) -> f32 {
        match self.lp(li).layer_pos(layer, pos) {
            Some(p) => self.read(li, layer, p),
            None => 0.0,
        }
    }

    fn zero_layer(&self, li: usize, layer: usize) {
        for i in 0..self.max_frames {
            self.cell(li, layer, i).store(0, Ordering::Relaxed);
        }
    }

    /// Everything one loop contributes to the mix at one output frame.
    ///
    /// Pulled out of the callback because six loops made it a nested loop worth
    /// naming, and because the self-test now has to be able to ask the same
    /// question of a specific loop.
    fn loop_at(&self, li: usize, out_frame: i64) -> f32 {
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
        let mut pos =
            (out_frame - lp.origin.load(Ordering::Acquire)).rem_euclid(len as i64) as usize;
        // Read from the other end. Applied to the loop's position rather than
        // to each layer's, so layers keep their places relative to one another
        // and the whole cycle turns over — which is what reversing a loop
        // means, and not the same as reversing every layer separately.
        if lp.reverse.load(Ordering::Relaxed) {
            pos = len - 1 - pos;
        }
        let n = lp.n_layers.load(Ordering::Acquire);
        let mut v = 0.0f32;
        for l in 0..n {
            v += self.sample_at(li, l, pos);
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

pub fn run(opts: Opts) -> Result<(), Box<dyn Error>> {
    let candidate = crate::devices::find(&opts.device)?;
    let device = candidate.device;

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
        device.build_output_stream(
            &out_cfg,
            move |data: &mut [f32], info: &cpal::OutputCallbackInfo| {
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
                        let s = sh.loop_at(li, out_frame);
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
        let residual = opts.residual;
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
                    } else {
                        // Modular: an overdub may go round as many times as it
                        // likes, summing into the same cycle.
                        if loop_len == 0 {
                            continue;
                        }
                        let pos = (rel % loop_len as i64) as usize;
                        sh.add(li, layer, pos, v);
                        lp.reached.fetch_max(loop_len, Ordering::Relaxed);
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

fn commit(sh: &Shared, li: usize, sr: u32) {
    let lp = sh.lp(li);
    let state = lp.state.get();
    if state != FIRST && state != OVERDUB {
        return;
    }

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
            let cur = sh.out_frames.load(Ordering::Acquire) as i64;
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

    // Let the input drain: it trails the output by K, so the last frames of the
    // loop have not arrived yet. Without this the tail of every recording is
    // missing, which is exactly the kind of fault that sounds like "feel".
    lp.state.set(PLAYING);
    std::thread::sleep(Duration::from_millis(60));

    if state == FIRST {
        let mut len = quantised_len.unwrap_or_else(|| lp.reached.load(Ordering::Acquire));
        if len == 0 {
            println!("  nothing recorded.");
            return;
        }
        // Pre-roll: a tap is always a little late, so back-date the loop's start
        // and fill the front from the ring. The attack that would have been
        // clipped off is already captured; it just has to be claimed.
        // Never for a quantised loop: the pre-roll shifts `origin` backwards to
        // reclaim the attack, and moving origin is exactly what must not happen
        // to a loop that was started on a boundary. Alignment beats the last
        // few milliseconds of the attack, and a loop that drifts off the grid
        // by its pre-roll would be a bug nobody could see the cause of.
        let pre = if quantised_len.is_some() {
            0
        } else {
            sh.preroll.load(Ordering::Acquire)
        };
        let layer = lp.n_layers.load(Ordering::Acquire);
        let origin = lp.origin.load(Ordering::Acquire);
        let new_origin = origin - pre as i64;
        if pre > 0 && len + pre > sh.max_frames {
            // Shifting anyway would run off the end of this layer's slice and
            // into the next one's, which is silent corruption rather than an
            // error. Refuse instead.
            println!(
                "  pre-roll skipped: the loop plus pre-roll would exceed --max-secs."
            );
        } else if pre > 0 && new_origin >= 0 {
            // Shift what was recorded up by `pre`, backwards so the move does
            // not eat its own tail, then fill the vacated front from the ring.
            for pos in (0..len).rev() {
                let v = sh.read(li, layer, pos);
                sh.write(li, layer, pos + pre, v);
            }
            for pos in 0..pre {
                sh.write(li, layer, pos, 0.0);
            }
            let got = fill_from_ring(sh, li, layer, new_origin, pre, false);
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
    let layer = lp.n_layers.load(Ordering::Acquire);
    let len = lp.loop_len.load(Ordering::Acquire);
    lp.set_layer_shape(layer, len);
    lp.n_layers.fetch_add(1, Ordering::AcqRel);
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
fn fill_from_ring(sh: &Shared, li: usize, layer: usize, from_out: i64, len: usize, additive: bool) -> usize {
    let k = sh.k.load(Ordering::Acquire);
    let mut got = 0;
    for pos in 0..len {
        let Some(v) = sh.ring_at(from_out + pos as i64 - k) else {
            continue;
        };
        if additive {
            sh.add(li, layer, pos, v);
        } else {
            sh.write(li, layer, pos, v);
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
fn take(sh: &Shared, li: usize, sr: u32, secs: f64) {
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
    let cur = sh.out_frames.load(Ordering::Acquire) as i64;

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
    let got = fill_from_ring(sh, li, layer, from_out, len, false);
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
    lp.set_layer_shape(taken, lp.loop_len.load(Ordering::Acquire));
    lp.n_layers.fetch_add(1, Ordering::AcqRel);
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
        let got = fill_from_ring(sh, li, layer, from, behind, false);
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
    lp.set_layer_shape(layer, new_len);
    lp.n_layers.fetch_add(1, Ordering::AcqRel);
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
                    multiply_start(sh, li, sr)
                }
            },
            "r" => match lp.state.get() {
                MULTIPLY => multiply_end(sh, li, sr),
                FIRST | OVERDUB => commit(sh, li, sr),
                _ => {
                    if let Some(other) = busy_elsewhere(sh, li) {
                        return other;
                    }
                    let layer = lp.n_layers.load(Ordering::Acquire);
                    if layer >= MAX_LAYERS {
                        println!("  {} layers is the ceiling; undo one first.", MAX_LAYERS);
                    } else {
                        // An overdub sums into its layer, so anything left there
                        // from an undone take would bleed into the new one.
                        sh.zero_layer(li, layer);
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
            l if l.starts_with('t') => {
                let secs = l[1..].trim().parse::<f64>().unwrap_or(8.0);
                take(sh, li, sr, secs);
            }
            // The second multiply, and its two companions. Structural, so they
            // are instant and reversible — nothing here records anything.
            l if l.starts_with('s') => {
                let n = l[1..].trim().parse::<usize>().unwrap_or(2);
                println!("  {}", sparse(sh, li, sr, n));
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
            "z" => println!("  {}", free_length(sh, li, sr)),
            // Returned rather than printed. This is the one command whose whole
            // point is *where* it put something, and a path printed on the
            // daemon's stdout is a path the app cannot show anyone — so the
            // message goes back as the ack and both callers display it
            // themselves. Printing here as well got it shown twice.
            l if l.starts_with('w') => return save_take(sh, li, sr, &l[1..]),
            "u" => {
                let n = lp.n_layers.load(Ordering::Acquire);
                if n == 0 {
                    println!("  nothing to undo.");
                } else {
                    lp.n_layers.store(n - 1, Ordering::Release);
                    sh.zero_layer(li, n - 1);
                    if n == 1 {
                        // Say what is being kept, or it reads as a fault. The
                        // length surviving an undo is the point — the click goes
                        // on at the tempo you found, so the next attempt lands on
                        // the same grid — but a length with nothing in it looks
                        // exactly like a looper that has stopped listening.
                        let len = lp.loop_len.load(Ordering::Acquire);
                        println!(
                            "  layer 1 removed. Empty now, but still {:.3} s long, so the next \
                             take lands on the same grid — `z` to forget the length.",
                            len as f64 / sr as f64
                        );
                    } else {
                        println!("  layer {} removed, {} left.", n, n - 1);
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
                let want = match rest {
                    "rev1" => true,
                    "rev0" => false,
                    _ => !lp.reverse.load(Ordering::Relaxed),
                };
                lp.reverse.store(want, Ordering::Relaxed);
                return format!(
                    "loop {} plays {}.",
                    li,
                    if want { "backwards" } else { "forwards" }
                );
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
                // back reversed and hard left would be a haunting.
                lp.reverse.store(false, Ordering::Relaxed);
                lp.pan.store(64, Ordering::Relaxed);
                lp.n_layers.store(0, Ordering::Release);
                lp.loop_len.store(0, Ordering::Release);
                for l in 0..MAX_LAYERS {
                    sh.zero_layer(li, l);
                    lp.set_layer_shape(l, 0);
                }
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
                println!("  click {}.", if on { "on" } else { "off" });
            }
            "m" | "m1" | "m0" => {
                let on = match line.trim() {
                    "m1" => true,
                    "m0" => false,
                    _ => !sh.monitor.load(Ordering::Relaxed),
                };
                sh.monitor.store(on, Ordering::Relaxed);
                println!(
                    "  input monitoring {}.{}",
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
    commit(sh, li, sr);
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
    commit(sh, li, sr);
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
    take(sh, li, sr, 0.0);
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
