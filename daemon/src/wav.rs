//! Writing a layer out as a file everything else can read.
//!
//! **32-bit float, not 16-bit PCM.** The arena sums overdubs with `add` and
//! nothing limits, so a layer can legitimately sit above ±1.0 — it plays back
//! fine, because the mix is summed and peaked, not clipped per layer. Writing
//! integer PCM would clip exactly that material, which would make saving a take
//! the first place in the whole engine where a musical decision got baked in
//! without being asked for. The file is meant to be the take, not a rendering
//! of it.
//!
//! Hand-written rather than pulling in `hound`. The tree is cpal and tungstenite
//! and a WAV header is fifty bytes; a dependency to emit fifty known bytes is a
//! poor trade. The layout below is the strict non-PCM form — an 18-byte `fmt `
//! with `cbSize`, and the `fact` chunk that the spec requires for float data —
//! because the readers that matter here are libsndfile (SuperDirt, via
//! SuperCollider) and CoreAudio, and the strict form costs twenty bytes.

/// Bytes of a mono 32-bit-float WAV holding `samples`.
/// **`channels` is a parameter now, and interleaved samples are expected.**
///
/// It was a constant 1, because the engine was mono. A stereo take written with
/// a mono header would play at double speed with the channels alternating — a
/// failure that is obvious the first time and impossible to spot in a file
/// listing, which is why the count comes from the caller rather than from an
/// assumption here.
pub fn wav_bytes(samples: &[f32], sample_rate: u32, channels: u16) -> Vec<u8> {
    wav_bytes_acid(samples, sample_rate, channels, None)
}

/// What a receiver needs in order to know a file is a *loop* rather than a
/// sound: how many beats long it is, and at what tempo.
///
/// This is the `acid` chunk, and it is the difference between a take that drops
/// into Ableton already warped and one that has to be told its own length by
/// hand, eight times. Loopy Pro and most samplers read it too.
pub struct Acid {
    /// Beats in the whole file. Not bars — the chunk counts beats.
    pub beats: u32,
    pub tempo: f32,
    /// The numerator: beats to a bar, which here is Link's quantum.
    pub beats_per_bar: u16,
}

/// As `wav_bytes`, and optionally saying what loop this is.
///
/// **`None` is not a lesser file, it is an honest one.** A take played back at
/// half speed, or swinging on a pendulum, has no whole number of beats to
/// declare; a wrong `acid` chunk would make Ableton warp it confidently to the
/// wrong grid, which is worse than making it ask. So the caller passes `Some`
/// only when the loop is doing the plain thing at a known bar count.
pub fn wav_bytes_acid(
    samples: &[f32],
    sample_rate: u32,
    channels: u16,
    acid: Option<Acid>,
) -> Vec<u8> {
    const FMT_LEN: u32 = 18;
    const FLOAT_FORMAT: u16 = 3;
    const BYTES_PER_SAMPLE: u16 = 4;
    // Four flags, two shorts, a float, a long, two shorts and a float.
    const ACID_LEN: u32 = 24;
    let channels = channels.max(1);

    let data_len = (samples.len() * BYTES_PER_SAMPLE as usize) as u32;
    let acid_len = if acid.is_some() { 8 + ACID_LEN } else { 0 };
    // "WAVE", then fmt, fact, maybe acid, and data, each with its 8-byte header.
    let riff_len = 4 + (8 + FMT_LEN) + (8 + 4) + acid_len + 8 + data_len;

    let mut b = Vec::with_capacity(riff_len as usize + 8);
    b.extend_from_slice(b"RIFF");
    b.extend_from_slice(&riff_len.to_le_bytes());
    b.extend_from_slice(b"WAVE");

    b.extend_from_slice(b"fmt ");
    b.extend_from_slice(&FMT_LEN.to_le_bytes());
    b.extend_from_slice(&FLOAT_FORMAT.to_le_bytes());
    b.extend_from_slice(&channels.to_le_bytes());
    b.extend_from_slice(&sample_rate.to_le_bytes());
    // Byte rate and block align both carry the channel count. Getting either
    // wrong makes a file that opens and plays at the wrong speed.
    b.extend_from_slice(
        &(sample_rate * BYTES_PER_SAMPLE as u32 * channels as u32).to_le_bytes(),
    );
    b.extend_from_slice(&(BYTES_PER_SAMPLE * channels).to_le_bytes());
    b.extend_from_slice(&32u16.to_le_bytes());
    b.extend_from_slice(&0u16.to_le_bytes());

    // Frame count, which for float data a reader is entitled to expect rather
    // than derive from the data length.
    b.extend_from_slice(b"fact");
    b.extend_from_slice(&4u32.to_le_bytes());
    // **Frames, not samples.** `fact` counts sample *frames* per channel, and
    // a stereo file that declared twice as many would tell a reader the take is
    // twice as long as it is.
    b.extend_from_slice(&((samples.len() / channels as usize) as u32).to_le_bytes());

    // Between `fact` and `data`, which is where every writer puts it and where
    // a reader that does not know the chunk will skip it by its declared size.
    if let Some(a) = acid {
        b.extend_from_slice(b"acid");
        b.extend_from_slice(&ACID_LEN.to_le_bytes());
        // Flags. Bit 0 set would mean *one shot*, which is the one thing this
        // must not say — everything written here is a loop. No root note is
        // declared either: these are not pitched samples for a keyboard.
        b.extend_from_slice(&0u32.to_le_bytes());
        b.extend_from_slice(&60u16.to_le_bytes()); // root note, unread with the flag clear
        b.extend_from_slice(&0x8000u16.to_le_bytes()); // the constant every writer emits
        b.extend_from_slice(&0.0f32.to_le_bytes());
        b.extend_from_slice(&a.beats.to_le_bytes());
        // Denominator first. The order is the wrong way round and is the
        // format's, not a slip here.
        b.extend_from_slice(&4u16.to_le_bytes());
        b.extend_from_slice(&a.beats_per_bar.max(1).to_le_bytes());
        b.extend_from_slice(&a.tempo.to_le_bytes());
    }

    b.extend_from_slice(b"data");
    b.extend_from_slice(&data_len.to_le_bytes());
    for s in samples {
        b.extend_from_slice(&s.to_le_bytes());
    }
    b
}

/// Longest take this can address, in frames.
///
/// RIFF sizes are unsigned 32-bit, so a file cannot exceed 4 GB however much
/// arena there is. At 48 kHz float that is six hours, which no `--max-secs`
/// will ever reach — but the ceiling is stated rather than assumed, because a
/// silently truncated length field would produce a file that opens and is
/// wrong, which is the worst of the available failures.
pub const MAX_FRAMES: usize = (u32::MAX as usize - 64) / 4;

#[cfg(test)]
mod tests {
    use super::*;

    fn u32_at(b: &[u8], i: usize) -> u32 {
        u32::from_le_bytes([b[i], b[i + 1], b[i + 2], b[i + 3]])
    }

    #[test]
    fn header_declares_the_lengths_it_actually_wrote() {
        let samples = vec![0.0f32, 1.0, -1.0, 2.5];
        let b = wav_bytes(&samples, 48_000, 1);

        assert_eq!(&b[0..4], b"RIFF");
        assert_eq!(&b[8..12], b"WAVE");
        // Every RIFF length is "bytes after this field", so the declared size
        // and the real one must agree exactly or a reader walks off the end.
        assert_eq!(u32_at(&b, 4) as usize, b.len() - 8);
        assert_eq!(b.len(), 12 + 26 + 12 + 8 + samples.len() * 4);
    }

    #[test]
    fn samples_survive_the_round_trip_including_above_unity() {
        let samples = vec![0.0f32, 0.5, -0.5, 2.5, -3.25];
        let b = wav_bytes(&samples, 48_000, 1);
        let data = &b[b.len() - samples.len() * 4..];
        let got: Vec<f32> = data
            .chunks_exact(4)
            .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
            .collect();
        // 2.5 and -3.25 are the point: integer PCM could not carry them, and a
        // layer that has been overdubbed into several times will hold values
        // like these.
        assert_eq!(got, samples);
    }

    #[test]
    fn an_acid_chunk_leaves_the_riff_length_honest() {
        let samples = vec![0.0f32; 8];
        let plain = wav_bytes(&samples, 48_000, 2);
        let looped = wav_bytes_acid(
            &samples,
            48_000,
            2,
            Some(Acid { beats: 16, tempo: 120.0, beats_per_bar: 4 }),
        );
        // The whole risk of adding a chunk is getting the size field wrong, so
        // this is the assertion that matters: both files must still declare
        // exactly the bytes they contain, and the loop one must be 32 longer.
        assert_eq!(u32_at(&plain, 4) as usize, plain.len() - 8);
        assert_eq!(u32_at(&looped, 4) as usize, looped.len() - 8);
        assert_eq!(looped.len(), plain.len() + 32);

        let at = looped.windows(4).position(|w| w == b"acid").expect("acid chunk");
        assert_eq!(u32_at(&looped, at + 4), 24);
        // Bit zero clear: this is a loop, not a one-shot. A file that claimed
        // otherwise would land in Ableton unwarped and look like our bug.
        assert_eq!(u32_at(&looped, at + 8) & 1, 0);
        assert_eq!(u32_at(&looped, at + 8 + 12), 16);
        // And the audio still follows it.
        assert_eq!(&looped[looped.len() - 40..looped.len() - 36], b"data");
    }

    /// Writes a real file so it can be checked against a decoder that is not
    /// this one — `afinfo` on macOS. Asserting my writer against my own reader
    /// would only prove they agree with each other.
    #[test]
    fn writes_a_file_a_system_decoder_can_read() {
        let sr = 48_000u32;
        let samples: Vec<f32> = (0..sr)
            .map(|i| (i as f32 / sr as f32 * 440.0 * std::f32::consts::TAU).sin() * 0.5)
            .collect();
        let path = std::env::temp_dir().join("itajara-wav-selftest.wav");
        std::fs::write(&path, wav_bytes(&samples, sr, 1)).expect("write");
        eprintln!("wrote {} — check with: afinfo {}", path.display(), path.display());
    }
}
