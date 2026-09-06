//! `--selftest`: one cycle of the engine's own click through a loopback.
//!
//! Split out of `engine.rs` on 2026-09-06 (REVIEW-daemon-debt step 1).

use std::error::Error;
use std::time::Duration;
use std::sync::atomic::Ordering;

use super::{ARMED, CHANNELS};
use super::commit::{commit, take};
use super::cycle::{multiply_end, multiply_start};
use super::shared::Shared;

/// Record one cycle of the engine's own click through a loopback cable and ask
/// where it ended up. Same question `align` asks, but through the real transport
/// and the real layer storage — so it tests what will actually run.
pub(crate) fn selftest(sh: &Shared, sr: u32, secs: f64) -> Result<(), Box<dyn Error>> {
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
