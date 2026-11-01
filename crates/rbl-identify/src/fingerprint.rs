//! Decoding the start of a file and computing its Chromaprint fingerprint,
//! encoded the way AcoustID's lookup takes it.

use std::path::Path;

use base64::Engine as _;
use rusty_chromaprint::{Configuration, FingerprintCompressor, Fingerprinter};

use crate::error::{IdentifyError, Result};

/// How much of a file is fingerprinted. AcoustID matches on the start of a
/// track; two minutes is what `fpcalc` uses by default.
pub const FINGERPRINT_SECS: f64 = 120.0;

/// Below this the fingerprint is too short for AcoustID to match.
const MIN_SECS: u32 = 10;

/// A fingerprint ready for `acoustid::AcoustIdClient::lookup`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Fingerprint {
    /// The compressed fingerprint, unpadded URL-safe base64, as `fpcalc`
    /// prints it.
    pub encoded: String,
    /// Sub-fingerprints before compression; a debugging aid.
    pub items: usize,
}

/// Fingerprints the first [`FINGERPRINT_SECS`] of a file.
pub fn fingerprint_file(path: &Path) -> Result<Fingerprint> {
    let audio = rbl_audio::decode_mono(path, Some(FINGERPRINT_SECS))?;
    fingerprint_samples(&audio.samples, audio.sample_rate)
}

/// Fingerprints mono `f32` samples at `sample_rate`.
///
/// Chromaprint's algorithm 2 (`preset_test2`), which is `fpcalc`'s default
/// and what AcoustID's database is built from. The samples are converted to
/// 16-bit, the input Chromaprint is defined on; it resamples internally.
pub fn fingerprint_samples(samples: &[f32], sample_rate: u32) -> Result<Fingerprint> {
    let min_samples = usize::try_from(u64::from(MIN_SECS) * u64::from(sample_rate)).unwrap_or(usize::MAX);
    if sample_rate == 0 || samples.len() < min_samples {
        return Err(IdentifyError::Fingerprint(format!("audio shorter than {MIN_SECS} s")));
    }
    let config = Configuration::preset_test2();
    let mut printer = Fingerprinter::new(&config);
    printer.start(sample_rate, 1).map_err(|e| IdentifyError::Fingerprint(format!("{e:?}")))?;
    let mut block = Vec::with_capacity(4096);
    for chunk in samples.chunks(4096) {
        block.clear();
        block.extend(chunk.iter().map(|&s| to_i16(s)));
        printer.consume(&block);
    }
    printer.finish();
    let raw = printer.fingerprint();
    if raw.is_empty() {
        return Err(IdentifyError::Fingerprint("no fingerprint produced".to_owned()));
    }
    let compressed = FingerprintCompressor::from(&config).compress(raw);
    Ok(Fingerprint { encoded: base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(compressed), items: raw.len() })
}

#[allow(clippy::cast_possible_truncation, reason = "clamped to the i16 range first")]
fn to_i16(sample: f32) -> i16 {
    (sample.clamp(-1.0, 1.0) * f32::from(i16::MAX)).round() as i16
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::cast_precision_loss, clippy::cast_possible_truncation, clippy::cast_sign_loss)]
mod tests {
    use super::*;

    /// A few seconds of chords that change every half second: enough
    /// spectral movement for Chromaprint to produce sub-fingerprints.
    fn music(secs: f64, rate: u32) -> Vec<f32> {
        let notes = [261.63_f32, 329.63, 392.0, 440.0, 349.23, 293.66];
        let total = (secs * f64::from(rate)) as usize;
        (0..total)
            .map(|i| {
                let t = i as f32 / rate as f32;
                let step = (t * 2.0) as usize;
                let a = notes[step % notes.len()];
                let b = notes[(step + 2) % notes.len()];
                0.3 * (2.0 * std::f32::consts::PI * a * t).sin() + 0.2 * (2.0 * std::f32::consts::PI * b * t).sin()
            })
            .collect()
    }

    #[test]
    fn the_same_audio_gives_the_same_fingerprint() {
        let samples = music(30.0, 44_100);
        let a = fingerprint_samples(&samples, 44_100).unwrap();
        let b = fingerprint_samples(&samples, 44_100).unwrap();
        assert_eq!(a, b);
        assert!(a.items > 100, "{}", a.items);
        assert_ne!(a.encoded, "");
        assert!(a.encoded.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'_'), "URL-safe, unpadded");
    }

    #[test]
    fn different_audio_gives_a_different_fingerprint() {
        let a = fingerprint_samples(&music(30.0, 44_100), 44_100).unwrap();
        let other: Vec<f32> = music(30.0, 44_100).iter().rev().copied().collect();
        let b = fingerprint_samples(&other, 44_100).unwrap();
        assert_ne!(a.encoded, b.encoded);
    }

    #[test]
    fn too_short_audio_is_refused() {
        let error = fingerprint_samples(&music(5.0, 44_100), 44_100).unwrap_err();
        assert!(error.to_string().contains("shorter"), "{error}");
        assert!(fingerprint_samples(&[], 0).is_err());
    }

    #[test]
    fn samples_are_clamped_into_16_bits() {
        assert_eq!(to_i16(1.5), i16::MAX);
        assert_eq!(to_i16(-1.5), -i16::MAX);
        assert_eq!(to_i16(0.0), 0);
    }
}
