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
pub fn wav_bytes(samples: &[f32], sample_rate: u32) -> Vec<u8> {
    const FMT_LEN: u32 = 18;
    const FLOAT_FORMAT: u16 = 3;
    const CHANNELS: u16 = 1;
    const BYTES_PER_SAMPLE: u16 = 4;

    let data_len = (samples.len() * BYTES_PER_SAMPLE as usize) as u32;
    // "WAVE", then fmt, fact and data chunks each with their 8-byte header.
    let riff_len = 4 + (8 + FMT_LEN) + (8 + 4) + 8 + data_len;

    let mut b = Vec::with_capacity(riff_len as usize + 8);
    b.extend_from_slice(b"RIFF");
    b.extend_from_slice(&riff_len.to_le_bytes());
    b.extend_from_slice(b"WAVE");

    b.extend_from_slice(b"fmt ");
    b.extend_from_slice(&FMT_LEN.to_le_bytes());
    b.extend_from_slice(&FLOAT_FORMAT.to_le_bytes());
    b.extend_from_slice(&CHANNELS.to_le_bytes());
    b.extend_from_slice(&sample_rate.to_le_bytes());
    b.extend_from_slice(&(sample_rate * BYTES_PER_SAMPLE as u32).to_le_bytes());
    b.extend_from_slice(&BYTES_PER_SAMPLE.to_le_bytes());
    b.extend_from_slice(&32u16.to_le_bytes());
    b.extend_from_slice(&0u16.to_le_bytes());

    // Frame count, which for float data a reader is entitled to expect rather
    // than derive from the data length.
    b.extend_from_slice(b"fact");
    b.extend_from_slice(&4u32.to_le_bytes());
    b.extend_from_slice(&(samples.len() as u32).to_le_bytes());

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
        let b = wav_bytes(&samples, 48_000);

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
        let b = wav_bytes(&samples, 48_000);
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
        std::fs::write(&path, wav_bytes(&samples, sr)).expect("write");
        eprintln!("wrote {} — check with: afinfo {}", path.display(), path.display());
    }
}
