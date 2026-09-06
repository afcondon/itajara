//! Everything both callbacks and the control thread touch: `Shared`.
//!
//! Split out of `engine.rs` on 2026-09-06 (REVIEW-daemon-debt step 1).

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::sync::Mutex;

use rand::rngs::SmallRng;
use rand::{Rng, SeedableRng};

use super::{CHANNELS, ENV_BUCKETS, NO_ANCHOR, Shape, Source, to_byte, wrap_mix};
use super::loop_state::Loop;

pub struct Shared {
    pub(crate) arena: Vec<AtomicU32>,
    pub max_frames: usize,
    /// `--loops` and `--layers`: the arena's shape, fixed at startup.
    pub n_loops: usize,
    pub max_layers: usize,
    /// `--fixed-secs` in frames, or zero for none.
    pub fixed_frames: usize,
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
    pub(crate) ring: Vec<AtomicU32>,
    pub ring_len: usize,
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
    /// Which loop's cycle is the grid, or `NO_ANCHOR` for none yet. Set by the
    /// first loop to acquire a length; see `grid`.
    pub anchor: AtomicUsize,
    pub out_frames: AtomicUsize,
    pub(crate) in_frames: AtomicUsize,
    pub k: AtomicI64,
    pub k_set: AtomicBool,
    pub p0: Mutex<Option<cpal::StreamInstant>>,
    pub(crate) buffer_frames: AtomicU32,
    pub click: AtomicBool,
    pub(crate) preroll: AtomicUsize,
    /// The level a sound has to reach to start a level-armed recording, as an
    /// `f32` magnitude in the bits of a `u32`.
    ///
    /// Rig-wide rather than per loop, and settable while the daemon runs, because
    /// it is a fact about the room and the instrument rather than about any one
    /// loop — and because a threshold you cannot tune where you are standing is a
    /// threshold that will be wrong.
    pub arm_thresh: AtomicU32,
    /// `ARM_REACH_MS` in frames, resolved once at startup.
    pub(crate) arm_reach: AtomicUsize,
    /// `MAX_FADE_MS` in frames: the most continuation worth keeping past a
    /// layer's end, since nothing longer can ever be crossfaded into the wrap.
    pub(crate) max_fade: usize,
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
    /// The last `pk` answer, as the JSON object the socket forwards once, and
    /// a sequence number so each connection can tell a new one from the one
    /// it has sent. Off the snapshot on purpose: a waveform is asked for once
    /// and changes when a layer does, not thirty times a second.
    pub peaks: Mutex<String>,
    pub peaks_seq: AtomicUsize,
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

impl Shared {
    /// One loop, by index, clamped rather than panicking: an out-of-range index
    /// can only come from a command string, and a bad command should be refused
    /// where commands are parsed, not by killing the audio thread.
    pub fn lp(&self, li: usize) -> &Loop {
        &self.loops[li.min(self.n_loops - 1)]
    }
    pub fn sel(&self) -> usize {
        self.selected.load(Ordering::Relaxed).min(self.n_loops - 1)
    }
    /// Which loop currently owns the input, if any.
    ///
    /// There is one converter, so at most one loop can be recording. Rather than
    /// keep a separate "who is recording" field that could disagree with the
    /// loops' own states, the input callback asks. Six relaxed loads per buffer
    /// is nothing, and a derived answer cannot go stale.
    pub fn recording_loop(&self) -> Option<usize> {
        (0..self.n_loops).find(|&i| self.loops[i].is_recording())
    }
    /// Whether any loop is claiming the input, including one merely armed.
    pub fn input_claimed(&self) -> Option<usize> {
        (0..self.n_loops).find(|&i| self.loops[i].wants_input())
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
        (0..self.n_loops).find(|&i| self.loops[i].is_armed() && self.loops[i].request.get() == 0)
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
        if a >= self.n_loops {
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
    pub(crate) fn claim_anchor(&self, li: usize) {
        let _ = self.anchor.compare_exchange(
            NO_ANCHOR,
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
    pub(crate) fn release_anchor(&self, li: usize) {
        let _ = self.anchor.compare_exchange(
            li,
            NO_ANCHOR,
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
    pub(crate) fn cell(&self, li: usize, layer: usize, pos: usize, ch: usize) -> &AtomicU32 {
        &self.arena[((li * self.max_layers + layer) * self.max_frames + pos) * CHANNELS + ch]
    }
    pub(crate) fn read(&self, li: usize, layer: usize, pos: usize, ch: usize) -> f32 {
        f32::from_bits(self.cell(li, layer, pos, ch).load(Ordering::Relaxed))
    }
    pub(crate) fn write(&self, li: usize, layer: usize, pos: usize, ch: usize, v: f32) {
        self.cell(li, layer, pos, ch).store(v.to_bits(), Ordering::Relaxed)
    }
    pub(crate) fn add(&self, li: usize, layer: usize, pos: usize, ch: usize, v: f32) {
        let c = self.cell(li, layer, pos, ch);
        let cur = f32::from_bits(c.load(Ordering::Relaxed));
        c.store((cur + v).to_bits(), Ordering::Relaxed)
    }
    /// The captured sample for an input frame, if the ring still holds it.
    pub(crate) fn ring_at(&self, src: usize, in_frame: i64, ch: usize) -> Option<f32> {
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
    pub(crate) fn sample_at(&self, li: usize, layer: usize, pos: usize, ch: usize) -> f32 {
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

    pub(crate) fn zero_layer(&self, li: usize, layer: usize) {
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
    pub(crate) fn rebuild_env(&self, li: usize, layer: usize) {
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
    pub(crate) fn flatten(&self, li: usize, at: i64) {
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
                        // A parked layer is not carried into the tape: what
                        // flattens is what you were hearing.
                        if lp.layer_on(l) {
                            v += self.read(li, l, p, ch) * lp.layer_gain(l);
                        }
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
    pub(crate) fn clear_env(&self, li: usize) {
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
    pub(crate) fn loop_at(&self, li: usize, out_frame: i64, rng: &mut SmallRng, live: bool) -> [f32; CHANNELS] {
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
        let p0 = pf.floor() as i64;
        let frac = pf - p0 as f64;
        // Outside the arena is the silence a window added, not a read.
        let at = |pos: i64| -> [f32; CHANNELS] {
            if pos >= 0 && (pos as usize) < len {
                self.mix_at(li, n, pos as usize, true)
            } else {
                [0.0; CHANNELS]
            }
        };
        // At rate one going forwards the fraction is exactly zero — the
        // arithmetic is `warp + (frame - origin) * 1.0` on integers — so the
        // ordinary case reads one sample per layer, as it always did, and the
        // second read is bought only by the loops that asked for it.
        let (start, span) = lp.cycle(len);
        let end = start + span as i64;
        let mut out = if frac == 0.0 {
            at(p0)
        } else {
            // The next sample is the next arena position, unless this is the
            // last of the cycle — then it is the first, which with a window
            // is `start` and not zero.
            let p1 = if p0 + 1 >= end { start } else { p0 + 1 };
            let f = frac as f32;
            let a = at(p0);
            let b = at(p1);
            let mut o = [0.0f32; CHANNELS];
            for ch in 0..CHANNELS {
                o[ch] = a[ch] * (1.0 - f) + b[ch] * f;
            }
            o
        };
        // **The window's seam gets the crossfade too.** A window ends in the
        // middle of audio, so `out -> in` is a cut where the whole-loop wrap
        // was a join, and it needs `xf` more than the join did: for the first
        // `xf` positions after `in`, what would have followed `out` fades out
        // under what follows `in`. Inside a whole loop the per-layer wrap in
        // `sample_at` still does the job it always did.
        if start != 0 || span != len {
            let xf = lp.fade.load(Ordering::Relaxed).min(span / 2);
            let k = (p0 - start).max(0) as usize;
            if xf > 0 && k < xf {
                // What would have followed `out`: the loop's own next audio
                // when the window ends inside it, wrapping as the loop does;
                // silence when the window already reaches past the end.
                let after = end + k as i64;
                let tail = if after >= len as i64 && end <= len as i64 {
                    at(after % len as i64)
                } else {
                    at(after)
                };
                for ch in 0..CHANNELS {
                    out[ch] = wrap_mix(out[ch], tail[ch], k, xf);
                }
            }
        }
        out
    }

    /// Every layer of one loop, summed at one integer loop position.
    ///
    /// Split out because interpolation needs the same question asked at two
    /// neighbouring positions, and summing the layers first is the same number
    /// as interpolating each layer and summing after — for half the reads.
    /// `played` applies each layer's own window, as the output does; the
    /// picture passes `false` and sees the arena as stored, so a window can
    /// be drawn over it rather than baked into it.
    pub fn mix_at(&self, li: usize, n: usize, pos: usize, played: bool) -> [f32; CHANNELS] {
        let lp = self.lp(li);
        let mut v = [0.0f32; CHANNELS];
        for l in 0..n {
            let g = lp.layer_gain(l);
            // Eighty decibels down is not quiet, it is gone — and skipping it
            // saves the arena read and the wrap fade's second read with it. The
            // audio is still there; only the reading of it stops.
            if g > 1.0e-4 && lp.layer_on(l) {
                match if played { lp.windowed_pos(l, pos) } else { None } {
                    // A windowed layer: a plain read at its own place, no
                    // placement and no wrap fade — the window's seam is a cut
                    // the player chose.
                    Some(Some(p)) => {
                        for ch in 0..CHANNELS {
                            v[ch] += self.read(li, l, p, ch) * g;
                        }
                    }
                    Some(None) => {}
                    None => {
                        for ch in 0..CHANNELS {
                            v[ch] += self.sample_at(li, l, pos, ch) * g;
                        }
                    }
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
        let (_, cyc) = lp.cycle(len);
        let span = if lp.pendulum.load(Ordering::Relaxed) { 2 * cyc } else { cyc } as f64;
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
