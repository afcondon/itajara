//! Finding an audio interface and seeing what it will actually give you.
//!
//! CoreAudio device names are not stable identifiers and the useful ones are
//! long ("iConnectivity Audio4c"), so matching is case-insensitive substring
//! rather than exact. Aggregate devices make this worse: a Mac accumulates
//! them, several will contain the word you searched for, and picking the wrong
//! one silently measures the wrong path. So every lookup reports what it
//! matched and refuses to guess between equals.

use cpal::traits::{DeviceTrait, HostTrait};
use std::error::Error;

/// A device plus the channel counts it is prepared to offer.
pub struct Candidate {
    pub device: cpal::Device,
    pub name: String,
    pub max_in: u16,
    pub max_out: u16,
}

fn probe(device: cpal::Device) -> Option<Candidate> {
    let name = device.name().ok()?;
    let max_in = device
        .supported_input_configs()
        .map(|cs| cs.map(|c| c.channels()).max().unwrap_or(0))
        .unwrap_or(0);
    let max_out = device
        .supported_output_configs()
        .map(|cs| cs.map(|c| c.channels()).max().unwrap_or(0))
        .unwrap_or(0);
    Some(Candidate { device, name, max_in, max_out })
}

pub fn all() -> Vec<Candidate> {
    let host = cpal::default_host();
    match host.devices() {
        Ok(ds) => ds.filter_map(probe).collect(),
        Err(_) => Vec::new(),
    }
}

/// Case-insensitive substring match, with exact matches winning outright.
///
/// The exact-match precedence matters on a Mac with aggregates: "BlackHole 2ch"
/// is a substring of nothing, but "Audio4c" is a substring of every aggregate
/// built on top of it, and the bare device is nearly always the one meant.
pub fn find(needle: &str) -> Result<Candidate, Box<dyn Error>> {
    let lower = needle.to_lowercase();
    let mut hits: Vec<Candidate> = all()
        .into_iter()
        .filter(|c| c.name.to_lowercase().contains(&lower))
        .collect();

    if let Some(i) = hits.iter().position(|c| c.name.to_lowercase() == lower) {
        return Ok(hits.swap_remove(i));
    }

    match hits.len() {
        0 => Err(format!(
            "no audio device matching {:?}. Run `itajara devices` to see what is here.",
            needle
        )
        .into()),
        1 => Ok(hits.remove(0)),
        _ => {
            let names: Vec<&str> = hits.iter().map(|c| c.name.as_str()).collect();
            Err(format!(
                "{:?} is ambiguous — it matches {}. Be more specific.",
                needle,
                names.join(", ")
            )
            .into())
        }
    }
}

/// `itajara devices` — what CoreAudio is offering, with the detail that
/// decides whether a rig is wired the way you think it is.
pub fn list() {
    let host = cpal::default_host();
    let default_in = host.default_input_device().and_then(|d| d.name().ok());
    let default_out = host.default_output_device().and_then(|d| d.name().ok());

    let devices = all();
    if devices.is_empty() {
        println!("No audio devices visible.");
        return;
    }

    for c in &devices {
        let mut marks = Vec::new();
        if Some(&c.name) == default_in.as_ref() {
            marks.push("default in");
        }
        if Some(&c.name) == default_out.as_ref() {
            marks.push("default out");
        }
        let suffix = if marks.is_empty() {
            String::new()
        } else {
            format!("   [{}]", marks.join(", "))
        };

        println!("\n{}{}", c.name, suffix);
        println!("  in {:>2} ch   out {:>2} ch", c.max_in, c.max_out);

        // Sample-rate ranges are the thing that bites later: a device that will
        // not do 48k, or will only do it on some channel counts, produces a
        // resampled loop whose length drifts against the cycle.
        if let Ok(cs) = c.device.supported_input_configs() {
            for cfg in cs {
                println!(
                    "  in   {:>2} ch  {:>6}–{:<6} Hz  {:?}",
                    cfg.channels(),
                    cfg.min_sample_rate().0,
                    cfg.max_sample_rate().0,
                    cfg.sample_format()
                );
            }
        }
        if let Ok(cs) = c.device.supported_output_configs() {
            for cfg in cs {
                println!(
                    "  out  {:>2} ch  {:>6}–{:<6} Hz  {:?}",
                    cfg.channels(),
                    cfg.min_sample_rate().0,
                    cfg.max_sample_rate().0,
                    cfg.sample_format()
                );
            }
        }
    }
    println!();
}
