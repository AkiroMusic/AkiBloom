//! Copied verbatim from the sibling AkiGraft project (shared kernel, one-way
//! copy — see AkiBloom PLAN.md). Do not diverge without updating both.
//! Parent sample loading and profile analysis.
//!
//! A parent's "identity" is its time-averaged log-band energy profile
//! (dB per mel band). Profiles are computed in the Hz domain, so a profile
//! from a 44.1 kHz file is directly comparable to 48 kHz runtime frames.

use std::path::Path;
use std::sync::Arc;

use parking_lot::RwLock;

use crate::graft::{DEFAULT_FMAX_HZ, DEFAULT_FMIN_HZ, FFT_SIZE, NUM_BANDS, OVERLAP};
use crate::mel::MelBands;
use crate::stft::{SpectralConfig, SpectralEngine, WindowType};
use crate::wav;

/// A loaded parent sample: metadata plus its static timbre profile.
pub struct ParentProfile {
    pub name: String,
    pub sample_rate: u32,
    pub channels: usize,
    pub frames: usize,
    /// Time-averaged log-band energies in dB (length `NUM_BANDS`).
    pub band_db: Vec<f32>,
}

impl ParentProfile {
    pub fn info_line(&self) -> String {
        let secs = self.frames as f32 / self.sample_rate as f32;
        format!(
            "{} · {} Hz · {} ch · {:.1}s",
            self.name, self.sample_rate, self.channels, secs
        )
    }
}

/// Load a parent sample and compute its timbre profile.
pub fn load_parent(path: &Path) -> Result<ParentProfile, String> {
    let (chans, sample_rate) = wav::read_wav_channels(path)?;
    if sample_rate == 0 {
        return Err("无效的采样率".to_owned());
    }
    let frames = chans.first().map(|c| c.len()).unwrap_or(0);
    if frames == 0 {
        return Err("文件里没有音频数据".to_owned());
    }

    let mono = wav::to_mono(&chans);
    let band_db = analyze_profile(&mono, sample_rate);

    Ok(ParentProfile {
        name: path
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| "未命名".to_owned()),
        sample_rate,
        channels: chans.len(),
        frames,
        band_db,
    })
}

/// Time-averaged log-band energies of a mono buffer, in dB.
///
/// Runs the same STFT the runtime engine uses, accumulating per-band energy
/// linearly across all frames before converting to dB (energy averaging, not
/// dB averaging).
///
/// Made `pub` in AkiBloom: seed loading reuses this for its own profiles.
pub fn analyze_profile(samples: &[f32], sample_rate: u32) -> Vec<f32> {
    let num_bins = FFT_SIZE / 2;
    let fmax = DEFAULT_FMAX_HZ.min(sample_rate as f32 * 0.5);
    let mel = MelBands::new(NUM_BANDS, num_bins, sample_rate as f32, DEFAULT_FMIN_HZ, fmax);

    let mut engine = SpectralEngine::new(
        SpectralConfig {
            fft_size: FFT_SIZE,
            overlap_count: OVERLAP,
            window: WindowType::Hann,
        },
        1,
    );

    let mut acc_lin = vec![0.0f32; NUM_BANDS];
    let mut frame_count = 0usize;
    // Reusable scratch so the per-frame callback never allocates.
    let mut mags = vec![0.0f32; num_bins];
    let mut band_buf = Vec::new();
    let mut out_scratch = vec![0.0f32; 8192];

    for chunk in samples.chunks(8192) {
        let out = &mut out_scratch[..chunk.len()];
        engine.process(&[chunk], &mut [out], &mut |_bins, _chan, _ov, polar| {
            for (i, p) in polar.iter().enumerate() {
                mags[i] = p.magnitude;
            }
            mel.band_db(&mags, &mut band_buf);
            for (acc, db) in acc_lin.iter_mut().zip(band_buf.iter()) {
                // Energy-domain accumulation: 10^(dB/10).
                *acc += 10f32.powf(db / 10.0);
            }
            frame_count += 1;
        });
    }

    if frame_count == 0 {
        return vec![crate::mel::FLOOR_DB; NUM_BANDS];
    }
    acc_lin
        .iter()
        .map(|e| 10.0 * (e / frame_count as f32).log10())
        .collect()
}

/// Which parent slot a load targets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Slot {
    A,
    B,
}

/// The pair of loaded parents, shared between the GUI (writer) and the audio
/// thread (reader).
pub type SharedParents = Arc<RwLock<ParentPair>>;

#[derive(Default)]
pub struct ParentPair {
    pub a: Option<Arc<ParentProfile>>,
    pub b: Option<Arc<ParentProfile>>,
}

impl ParentPair {
    pub fn set(&mut self, slot: Slot, profile: Arc<ParentProfile>) {
        match slot {
            Slot::A => self.a = Some(profile),
            Slot::B => self.b = Some(profile),
        }
    }

    pub fn get(&self, slot: Slot) -> Option<&Arc<ParentProfile>> {
        match slot {
            Slot::A => self.a.as_ref(),
            Slot::B => self.b.as_ref(),
        }
    }

    pub fn any_loaded(&self) -> bool {
        self.a.is_some() || self.b.is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graft::GraftCore;

    #[test]
    fn profile_of_silence_is_floor() {
        let db = analyze_profile(&vec![0.0; 48_000], 48_000);
        assert_eq!(db.len(), NUM_BANDS);
        assert!(db.iter().all(|d| (*d - crate::mel::FLOOR_DB).abs() < 1.0));
    }

    #[test]
    fn low_tone_profile_peaks_low() {
        // A 200 Hz sine must light up low bands much more than high ones.
        let sr = 48_000.0f32;
        let samples: Vec<f32> = (0..96_000)
            .map(|i| (2.0 * std::f32::consts::PI * 200.0 * i as f32 / sr).sin() * 0.5)
            .collect();
        let db = analyze_profile(&samples, 48_000);
        let low: f32 = db[..6].iter().sum();
        let high: f32 = db[16..].iter().sum();
        assert!(low / 6.0 - high / 8.0 > 12.0, "low {} vs high {}", low, high);
    }

    #[test]
    fn parent_pair_slot_roundtrip() {
        let mut pair = ParentPair::default();
        assert!(!pair.any_loaded());
        let prof = Arc::new(ParentProfile {
            name: "x".into(),
            sample_rate: 48_000,
            channels: 1,
            frames: 1,
            band_db: vec![0.0; NUM_BANDS],
        });
        pair.set(Slot::B, prof);
        assert!(pair.b.is_some() && pair.a.is_none());
        assert!(pair.any_loaded());
    }

    // Silence a clippy "field never read" style warning by touching GraftCore
    // once: the test crate links the whole lib.
    #[test]
    fn graft_core_constructs() {
        let core = GraftCore::new(48_000.0);
        assert!(core.latency_samples() > 0);
    }
}
