//! Takes on disk: one loop's layers, every loop's, and the rendered set.
//!
//! Split out of `engine.rs` on 2026-09-06 (REVIEW-daemon-debt step 1).

use std::sync::atomic::Ordering;

use super::{Ack, CHANNELS};
use super::cycle::tempo_of;
use super::loop_state::Loop;
use super::shared::Shared;

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
///
/// The guards and the name are decided now; the writing is a `Job` for the
/// slow thread (step 7), which reads the arena — nothing writes a loop that
/// is not recording, and recording was refused above.
pub(crate) fn save_take(sh: &Shared, li: usize, sr: u32, name: &str) -> Ack {
    let lp = sh.lp(li);
    if lp.is_recording() || lp.is_armed() {
        return Ack::Now("finish the recording first — a layer still being written is half a thing.".into());
    }
    if lp.n_layers.load(Ordering::Acquire) == 0 || lp.loop_len.load(Ordering::Acquire) == 0 {
        return Ack::Now("nothing to save yet.".into());
    }

    let name = safe_name(name);
    let dir = sh.takes_dir.join(&name);
    Ack::Later(Box::new(move |sh: &Shared| {
        let (written, loop_len) = match write_take(sh, li, sr, &dir) {
            Ok(w) => w,
            Err(e) => return e,
        };
        format!(
            "saved {} layer{} ({:.3} s) to {}",
            written,
            if written == 1 { "" } else { "s" },
            loop_len as f64 / sr as f64,
            dir.display()
        )
    }))
}

/// One layer as it will be written: the file it goes to and what the
/// manifests say about it. Filled by `write_layers`, read by both formats.
struct LayerFile {
    file: String,
    len: usize,
    period: usize,
    phase: usize,
    gain: f32,
    born: i64,
    on: bool,
    window: Option<(i64, i64)>,
}

/// One loop's layers, raw, into `dir`: the audio and the version-1
/// `take.json` beside it. What `w` has always written, so a folder written
/// by `exl` is a take that reloads like any other. Returns the count written
/// and the loop's length.
fn write_take(sh: &Shared, li: usize, sr: u32, dir: &std::path::Path) -> Result<(usize, usize), String> {
    let layers = write_layers(sh, li, sr, dir)?;
    let loop_len = sh.lp(li).loop_len.load(Ordering::Acquire);
    let entries: Vec<String> = layers
        .iter()
        .map(|l| {
            format!(
                r#"{{"file":"{}","len":{},"channels":{},"period":{},"phase":{}}}"#,
                l.file, l.len, CHANNELS, l.period, l.phase
            )
        })
        .collect();
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
        return Err(format!("wrote the audio but not the manifest: {}", e));
    }
    Ok((layers.len(), loop_len))
}

/// The audio of one loop's layers, one WAV each, into `dir`. No manifest:
/// the two callers write different ones.
fn write_layers(sh: &Shared, li: usize, sr: u32, dir: &std::path::Path) -> Result<Vec<LayerFile>, String> {
    let lp = sh.lp(li);
    if let Err(e) = std::fs::create_dir_all(dir) {
        return Err(format!("could not make {}: {}", dir.display(), e));
    }
    let n = lp.n_layers.load(Ordering::Acquire);
    let mut out = Vec::new();
    for l in 0..n {
        let (len, period, phase) = lp.layer_shape(l);
        if len == 0 {
            continue;
        }
        if len > crate::wav::MAX_FRAMES {
            return Err(format!("layer {} is longer than a WAV can address.", l));
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
        // `--layers` is 4 by default, so this is insurance bought while it is free.
        let file = format!("layer-{:02}.wav", l);
        if let Err(e) = std::fs::write(dir.join(&file), crate::wav::wav_bytes(&samples, sr, CHANNELS as u16)) {
            return Err(format!("could not write {}: {}", file, e));
        }
        out.push(LayerFile {
            file,
            len,
            period,
            phase,
            gain: lp.layer_gain(l),
            born: lp.layer_born(l),
            on: lp.layer_on(l),
            window: lp.layer_window(l),
        });
    }
    Ok(out)
}

/// Every loop that holds something, as a take each: `<name>/loop-<n>/` with
/// the layers raw and a `take.json`, and one `export.json` for the set.
///
/// ## The third render
///
/// `w` is one loop's layers; `ex` is every loop flat. This is every loop's
/// layers — the shape a sample-playing module wants when a loop is going to
/// become a *scene* (Arbhar), a *reel* (Morphagene) or a *voice* (Rample):
/// the six takes that were played against each other, kept apart so the
/// module can scan across them, but grouped so they arrive together. One
/// verb rather than a fan-out of `w`, so one ack says where it all went.
///
/// ## Raw, with the edit recorded beside it
///
/// The layers are written as they lie in the arena — the whole layer, not
/// the window — and the window and rotation go in the manifest instead.
/// Store everything, flatten late: the harvest that shapes these for a
/// module applies the window itself, and needs the whole layer to do it
/// (an Arbhar layer wants the loop's own wrap as its tail, which is audio
/// *outside* the window). What we do not render, we record.
///
/// ## Version 2
///
/// The set manifest carries what the shaping side asks for: per loop its
/// window, bars, tempo, source and the three things the render leaves out;
/// per layer its gain, birth and whether it is in the mix. The per-loop
/// `take.json` stays at version 1 so each folder reloads as a plain take.
pub(crate) fn export_layers(sh: &Shared, sr: u32, name: &str) -> Ack {
    // Checked across every loop before anything is written, as `ex` does.
    for li in 0..sh.n_loops {
        let lp = sh.lp(li);
        if lp.is_recording() || lp.is_armed() {
            return Ack::Now(format!(
                "loop {} is still recording — finish it before exporting the layers.",
                li
            ));
        }
    }

    let name = safe_name(name);
    let dir = sh.takes_dir.join(&name);
    if let Err(e) = std::fs::create_dir_all(&dir) {
        return Ack::Now(format!("could not make {}: {}", dir.display(), e));
    }
    // The rest is reads of the arena and writes to disk: the slow thread's.
    Ack::Later(Box::new(move |sh: &Shared| write_layer_set(sh, sr, dir)))
}

/// The writing half of `exl`: every loop that holds something, as a take
/// each, and the set manifest.
fn write_layer_set(sh: &Shared, sr: u32, dir: std::path::PathBuf) -> String {
    let quantum = f64::from_bits(sh.link_quantum.load(Ordering::Relaxed));
    let beats_per_bar = if quantum >= 1.0 { quantum } else { 4.0 };

    let mut entries: Vec<String> = Vec::new();
    let mut wrote: Vec<String> = Vec::new();
    for li in 0..sh.n_loops {
        let lp = sh.lp(li);
        let loop_len = lp.loop_len.load(Ordering::Acquire);
        if lp.n_layers.load(Ordering::Acquire) == 0 || loop_len == 0 {
            continue;
        }
        // Numbered from one, like `ex`: a folder name is a surface.
        let sub = format!("loop-{}", li + 1);
        let loop_dir = dir.join(&sub);
        let layers = match write_layers(sh, li, sr, &loop_dir) {
            Ok(l) => l,
            Err(e) => return e,
        };
        if let Err(e) = write_take(sh, li, sr, &loop_dir) {
            return e;
        }
        let bars = lp.cycles.load(Ordering::Acquire);
        let tempo = if bars > 0 && lp.plain() {
            format!("{:.4}", tempo_of(loop_len, bars, sr, quantum))
        } else {
            "null".to_string()
        };
        let window = match lp.window() {
            Some((i, o)) => format!(r#"{{"in":{},"out":{}}}"#, i, o),
            None => "null".to_string(),
        };
        let source = sh.sources.get(sh.src_of(li)).map(|s| s.name.as_str()).unwrap_or("");
        let layer_entries: Vec<String> = layers
            .iter()
            .map(|l| {
                format!(
                    r#"{{"file":"{}","len":{},"channels":{},"period":{},"phase":{},"gain":{:.5},"born":{},"on":{},"window":{}}}"#,
                    l.file, l.len, CHANNELS, l.period, l.phase, l.gain, l.born, l.on,
                    match l.window {
                        Some((i, o)) => format!(r#"{{"in":{},"out":{}}}"#, i, o),
                        None => "null".to_string(),
                    }
                )
            })
            .collect();
        entries.push(format!(
            concat!(
                "{{\"dir\":\"{}\",\"loop\":{},\"frames\":{},\"secs\":{:.6},\"bars\":{},\"tempo\":{},",
                "\"quant\":{},\"window\":{},\"rot\":{},\"source\":\"{}\",",
                "\"chance\":{:.4},\"oneShot\":{},\"muted\":{},\n      \"layers\":[\n        {}\n      ]}}"
            ),
            sub,
            li + 1,
            loop_len,
            loop_len as f64 / sr as f64,
            bars,
            tempo,
            lp.quant.load(Ordering::Relaxed),
            window,
            lp.rot.load(Ordering::Relaxed),
            source,
            lp.chance_of(),
            lp.one_shot.load(Ordering::Relaxed),
            lp.muted.load(Ordering::Relaxed),
            layer_entries.join(",\n        "),
        ));
        wrote.push(sub);
    }

    if entries.is_empty() {
        return "nothing to export — no loop has anything in it.".into();
    }

    // No timestamp, for the same reason the others have none.
    let manifest = format!(
        concat!(
            "{{\n  \"version\": 2,\n  \"kind\": \"layers\",\n  \"sampleRate\": {},\n",
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
        "exported the layers of {} loop{} to {}: {} — numbered as the board labels them, so \
         loop 0 is loop-1/.",
        wrote.len(),
        if wrote.len() == 1 { "" } else { "s" },
        dir.display(),
        wrote.join(", ")
    )
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
pub(crate) fn export_set(sh: &Shared, sr: u32, name: &str) -> Ack {
    // Checked across every loop before anything is written, rather than per
    // loop as it goes: a half-written folder that stopped at loop 5 because
    // loop 5 was recording is worse than one that never started.
    for li in 0..sh.n_loops {
        let lp = sh.lp(li);
        if lp.is_recording() || lp.is_armed() {
            return Ack::Now(format!(
                "loop {} is still recording — finish it before exporting the set.",
                li
            ));
        }
    }

    let name = safe_name(name);
    let dir = sh.takes_dir.join(&name);
    if let Err(e) = std::fs::create_dir_all(&dir) {
        return Ack::Now(format!("could not make {}: {}", dir.display(), e));
    }
    // The renders and the writes are the slow thread's.
    Ack::Later(Box::new(move |sh: &Shared| write_set(sh, sr, dir)))
}

/// The rendering-and-writing half of `ex`.
fn write_set(sh: &Shared, sr: u32, dir: std::path::PathBuf) -> String {
    let quantum = f64::from_bits(sh.link_quantum.load(Ordering::Relaxed));
    let beats_per_bar = if quantum >= 1.0 { quantum } else { 4.0 };

    let mut entries: Vec<String> = Vec::new();
    let mut wrote: Vec<String> = Vec::new();
    for li in 0..sh.n_loops {
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
