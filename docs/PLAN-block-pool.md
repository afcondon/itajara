# Pay for what you are using: the arena as a block pool

*Design note, 2026-09-22. Nothing built. Written out of a memory diagnosis —
4.5 GB resident after six days up — whose immediate cause is fixed, and whose
root cause is the storage model.*

---

## What happened, briefly

The daemon was holding 4.46 GiB. Not a leak: the arena, committed. With
`--layers 6` and the defaults it was 8 loops x 6 layers x 300 s x 48 kHz x 2 ch
x 4 B = **5.53 GB reserved**, and about 37 of the 48 slots had become resident.

`run.rs` allocates with `alloc_zeroed` precisely so pages commit as loops fill,
and says so. But `zero_layer` ran `0..max_frames` — the whole 300-second slot —
so **clearing a layer that held four seconds committed 115 MB**, permanently.
Clear all 48 slots once and the entire arena is resident whatever you recorded.
`lane.rs` had the cost written down all along ("linear in the layer, or in
`--max-secs` for `zero_layer`"); nothing connected it to RSS.

Fixed the same day: `Layer::written` is a high-water mark raised in
`Shared::write`/`add`, and `zero_layer` clears `0..written`. Bounding by
`len + tail` instead would have been wrong — three callers zero a slot whose
shape describes something other than its contents (the next free slot in
`cycle.rs`, a slot holding an undone take in `dispatch.rs`, a take abandoned
mid-record in `run.rs::drop_takes`), and only a high-water mark knows about
audio whose shape has been forgotten. Three regression tests hold that.

**That changed the rate, not the direction.** RSS still only ratchets: it now
tracks the longest take ever recorded into each slot and never gives it back.
The question this note answers is the one that follows — *is there an
architecture that pays for what it is using right now?*

## The one fact that makes this cheap

```rust
// engine/shared.rs:394 — the ONLY place `.arena` is indexed, anywhere
&self.arena[((li * self.max_layers + layer) * self.max_frames + pos) * CHANNELS + ch]
```

Every read, write, add and clear in the engine goes through `cell`. Nobody
reached around it in the whole codebase. **The storage model is already a
sealed abstraction**, so replacing what is behind it is a contained change
rather than a rewrite — which is the entire reason this note is worth writing
rather than filing as a someday.

## The proposal

Stop indexing `(loop, layer, frame)` into a rectangular reservation. Make a
layer **a list of block indices** into a shared pool of fixed-size blocks.

One second a block is a reasonable first guess: 48000 x 2 x 4 = 384 KB, and a
power-of-two frame count makes the arithmetic shifts.

```
cell(li, layer, pos, ch):
    b = layers[li][layer].blocks[pos >> BLOCK_SHIFT]   // one extra load
    pool[((b << BLOCK_SHIFT) | (pos & BLOCK_MASK)) * CHANNELS + ch]
```

### What it buys

**Pay-for-use, exactly.** A 13-second take costs 13 blocks. Today it reserves
the full `--max-secs` whether or not a sample is written. The same rig content
— 48 slots holding 13 s each — is **240 MB instead of 2.2 GB**.

**GC becomes a free-list push.** Clearing a layer returns its blocks. No
`madvise`, no `mmap` remap, no page-alignment arithmetic, and above all no race
against the audio callback: a returned block is an index moving on a stack.
Every difficulty in the "can we hand memory back" question disappears, because
the memory was never handed to the kernel in the first place.

It is worth being explicit about why the obvious alternative is a trap. To
return pages you must tell the kernel, and on Darwin `madvise(MADV_FREE)` says
pages *may* be reclaimed — **and if they are not, the old contents remain**. A
cleared layer could read back its old audio, non-deterministically: the bleed
bug in a disguise. The guaranteed-zero technique is remapping the range
(`mmap` with `MAP_FIXED|MAP_ANON`), which is `MAP_FIXED` over memory the
real-time callback holds references into, with no lock excluding it. Both are
avoided entirely by never involving the kernel.

**`--max-secs` stops being a per-slot reservation.** A loop can be any length
until the *pool* is exhausted. The flag becomes "the pool is 1 GB, which is
about 45 minutes of stereo across the whole rig" — a far more natural thing to
size than "each of 48 slots may independently be two minutes". And nothing
punishes you for headroom you do not use, which was the actual complaint: today
raising `--max-secs` for the one long piece you might play costs 48 slots' worth
of ceiling.

Blocks are uniform, so there is no fragmentation to manage.

### The one genuinely hard part

**Recording happens in the callback**, so crossing a block boundary means the
callback needs a block — and the callback may not allocate.

The fix is standard but has to be built deliberately: the callback **pops** an
index from a pre-filled lock-free free list (a CAS, not an allocation) and the
control thread refills it. Get it wrong and the symptom is a dropout exactly at
a block boundary — periodic, tempo-independent, and thoroughly confusing to
diagnose from the outside. It is the part of this to write first and test
hardest.

A second, smaller subtlety: the block *list* for a layer must stay readable by
the callback while the control thread appends to it. A fixed-capacity array
sized by the pool, written before the count is published, covers it — the same
publish-after-write discipline `set_layer_shape` already uses.

### Why the conformance machinery makes this safe to attempt

`engine/conformance.rs` and `engine/layer_conformance` replay a table through
the engine. So this change can be *proved* behaviour-identical rather than
hoped to be — which is exactly the kind of change that discipline was built
for, and the reason to do it here rather than in a codebase without it.

## A separate, also-attractive idea: back the pool with a file

`mmap` the pool from a file on the Crucial4TB. Untouched pages cost nothing,
the kernel evicts cold clean pages on its own, and **a take survives a daemon
restart or a crash**.

That last part is not hypothetical. On 2026-09-22 the daemon was killed by a
stray `pkill -f "23028"` — the pattern matched `--ws-port 23028` in its own
command line — and every loop in memory went with it. A device unplug already
puts the daemon into `device_lost`; "the loops outlive the process" is a real
robustness win independent of memory.

The hazard is exact and manageable: **a page fault in the audio callback is a
disk read, which is a dropout.** So `mlock` the blocks belonging to loops that
are currently sounding and let the rest page. The engine knows precisely which
those are, which is what makes this tractable here and not in general.

Orthogonal to the block pool — it can be done to either storage model — but
much more natural with blocks, because "the set of pages to lock" is then
already an explicit list.

## Considered and rejected

**Erlang for the audio path.** BEAM has no real-time guarantee and its
per-process GC pauses land exactly where they cannot be tolerated. Erlang is
right for the *control* plane, which is what purerl-tidal already uses it for,
and that split is correct. Worth noting in passing that BEAM's refcounted
binaries are conceptually the same idea as the block pool — immutable shared
chunks, freed when nobody holds them — so the model is right; it is the runtime
that cannot come along.

**A daemon per loop, or per sample.** CoreAudio lets one process claim the
ES-9, so N loop processes need a mixing server and IPC on the real-time path:
JACK, rebuilt, with worse jitter. `continuo` gets away with process-per-plugin
because those are independent instruments rather than a sample-locked shared
mix; a looper is the other case.

**Narrower samples.** The arena is f32 holding raw input, pre-processing, so
i24 halves it for one line in `cell`. Real, but note that overdub *sums in
place*: with six layers the quantisation is applied repeatedly. i24 is
comfortably clear of that; i16 is not. A 2x win that does not change the shape
of the problem — worth taking eventually, not worth doing instead of this.

## Where this leaves the flags

Until the pool exists, **the ceiling is the lever**. `--max-secs` is the honest
knob and it now sits at 120 (registered in Bosun's `fleet.json`), which is a
2.2 GB ceiling and covers a 32-bar loop down to 64 bpm — `MAX_BARS` is 32, so
60 s would have refused a full-length loop at any tempo below 128. The ceiling
fails loudly everywhere it is reached (`overflowed`, and explicit refusals in
`commit.rs` and `cycle.rs` naming the ceiling in seconds), so a low value can
annoy but cannot silently truncate.

## One-line summary

The arena is one line of indexing, so replace it with a pool of one-second
blocks and a free list: a layer becomes a list of block indices, clearing
becomes pushing indices back, `--max-secs` stops reserving anything, and the
whole "how do we hand memory back to the kernel" question — with its Darwin
`MADV_FREE` trap and its `MAP_FIXED`-under-a-real-time-reader hazard — is
answered by never asking the kernel at all.
