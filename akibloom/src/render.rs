//! Offline variant rendering for AkiBloom.
//!
//! The instrument's "grow" step: one seed's audio is pushed toward the other
//! seed's timbre profile by the shared GraftCore engine (the same engine and
//! the same ±12 dB band-shift clamp as AkiGraft — the instrument simply runs
//! it over a whole buffer instead of a live stream), then latency-trimmed,
//! loop-crossfaded, and packaged as a [`PlayableVariant`] the voices can
//! read.
//!
//! Rendering happens on a worker thread; the audio thread only ever receives
//! a new `Arc<PlayableVariant>` through the shared slot. Identity holds at
//! α = 0 with no variation: the rendered buffer equals the source seed (the
//! engine's verified bit-level identity, minus the trimmed latency).

use std::sync::Arc;

use crate::graft::{GraftControl, GraftCore};
use crate::seeds::SeedSample;
use crate::wav;

/// Length of the baked loop crossfade (samples per channel). A short fade
/// between buffer start and end removes the click at the loop wrap.
const LOOP_XFADE: usize = 512;
/// Output safety ceiling — the rendered variant is peak-normalized down to
/// this when the graft pushed it over (gain-up is never applied).
const PEAK_CEILING: f32 = 0.98;

/// A rendered, playable timbre: stereo at `sample_rate`, seamless-looping.
pub struct PlayableVariant {
    pub channels: Vec<Vec<f32>>,
    pub sample_rate: u32,
}

impl PlayableVariant {
    pub fn len(&self) -> usize {
        self.channels.first().map(|c| c.len()).unwrap_or(0)
    }

    pub fn info_line(&self) -> String {
        format!(
            "{:.1} s · {} Hz",
            self.len() as f32 / self.sample_rate as f32,
            self.sample_rate
        )
    }
}

/// Everything a render needs, snapshotted by the GUI before spawning the
/// worker (no locks cross the thread boundary).
pub struct RenderJob<'a> {
    pub source: &'a SeedSample,
    /// The color donor; `None` keeps the variant identical to the source.
    pub donor: Option<&'a SeedSample>,
    /// How far the source is pulled toward the donor, `0..1` (0 = identity).
    pub alpha: f32,
    /// Optional variation transform (杂交); `None` for M0 graft-only renders.
    pub variate: Option<crate::graft::VariateState>,
}

/// Render a playable variant from the source seed toward the donor's color.
pub fn render_variant(job: RenderJob) -> PlayableVariant {
    let sr = job.source.profile.sample_rate;
    let mut chans = job.source.channels.clone();
    let n = chans.first().map(|c| c.len()).unwrap_or(0);

    // The GraftCore pulls the *input* toward the blended profile of its
    // `parent_a`/`parent_b` arguments: here the source is parent A, the donor
    // is parent B, and blend selects the donor fully when it exists.
    let source_profile = &job.source.profile;
    let donor_profile = job.donor.map(|d| &d.profile);
    let blend = if donor_profile.is_some() { 1.0 } else { 0.0 };

    let mut core = GraftCore::new(sr as f32);
    let ctl = GraftControl {
        alpha: job.alpha,
        blend,
        mix: 1.0,
        out_gain: 1.0,
        variate: job.variate,
    };

    let chunk = 8192;
    let mut processed = 0usize;
    while processed < n {
        let end = (processed + chunk).min(n);
        let mut slices: Vec<&mut [f32]> =
            chans.iter_mut().map(|c| &mut c[processed..end]).collect();
        core.process_block(
            &mut slices,
            Some(source_profile),
            donor_profile,
            ctl,
        );
        processed = end;
    }

    // Trim the engine's algorithmic delay so the variant is sample-aligned
    // with the seed (voices loop from sample 0).
    let latency = core.latency_samples();
    if latency > 0 && latency < n {
        for c in chans.iter_mut() {
            c.drain(0..latency);
        }
    }

    bake_loop_crossfade(&mut chans, LOOP_XFADE);
    apply_peak_ceiling(&mut chans, PEAK_CEILING);

    PlayableVariant {
        channels: chans,
        sample_rate: sr,
    }
}

/// Crossfade the tail into the head and drop the tail, so looping the whole
/// buffer is click-free. Buffers shorter than 4× the fade are left alone.
fn bake_loop_crossfade(chans: &mut [Vec<f32>], xfade: usize) {
    let len = chans.first().map(|c| c.len()).unwrap_or(0);
    if len < xfade * 4 {
        return;
    }
    for i in 0..xfade {
        let t = i as f32 / (xfade - 1) as f32;
        for c in chans.iter_mut() {
            let head = c[i];
            let tail = c[len - xfade + i];
            c[i] = head * t + tail * (1.0 - t);
        }
    }
    for c in chans.iter_mut() {
        c.truncate(len - xfade);
    }
}

/// Never boost; only scale down when the render peaked above the ceiling.
fn apply_peak_ceiling(chans: &mut [Vec<f32>], ceiling: f32) {
    let peak = chans
        .iter()
        .flat_map(|c| c.iter())
        .fold(0.0f32, |m, v| m.max(v.abs()));
    if peak > ceiling {
        let g = ceiling / peak;
        for c in chans.iter_mut() {
            for v in c.iter_mut() {
                *v *= g;
            }
        }
    }
}

/// Write a rendered variant to a 32-bit float WAV (used by the demo CLI and
/// the export button later on).
pub fn write_variant(path: &std::path::Path, variant: &PlayableVariant) -> Result<(), String> {
    wav::write_wav_32f(path, &variant.channels, variant.sample_rate)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graft::{NUM_BANDS, FFT_SIZE};
    use crate::parents::ParentProfile;

    fn seed_with(name: &str, level_db: f32, sr: u32) -> SeedSample {
        // Deterministic LCG noise, scaled to roughly the requested RMS.
        let mut x: u32 = 0x1234_5678;
        let frames = (sr as usize) / 2; // 0.5 s — enough frames for the engine
        let mut chan = Vec::with_capacity(frames);
        for _ in 0..frames {
            x ^= x << 13;
            x ^= x >> 17;
            x ^= x << 5;
            chan.push((x as f32 / u32::MAX as f32) * 2.0 - 1.0);
        }
        let scale = 10f32.powf(level_db / 20.0);
        for v in chan.iter_mut() {
            *v *= scale;
        }
        SeedSample {
            profile: ParentProfile {
                name: name.into(),
                sample_rate: sr,
                channels: 2,
                frames,
                band_db: vec![level_db; NUM_BANDS],
            },
            channels: vec![chan.clone(), chan],
        }
    }

    /// α = 0 with a donor: the variant equals the source after the engine's
    /// latency is trimmed (the locked "α=0 原声" identity, offline edition).
    #[test]
    fn alpha_zero_variant_equals_source() {
        let a = seed_with("a", -12.0, 48_000);
        let b = seed_with("b", -40.0, 48_000);
        let variant = render_variant(RenderJob {
            source: &a,
            donor: Some(&b),
            alpha: 0.0,
            variate: None,
        });
        assert_eq!(variant.sample_rate, 48_000);
        let src = &a.channels[0];
        let out = &variant.channels[0];
        // Both the engine latency (trimmed) and the loop crossfade (baked in)
        // shorten the variant relative to the seed.
        assert_eq!(out.len(), src.len() - FFT_SIZE - LOOP_XFADE);
        // Skip the crossfaded head and a settling margin at the end.
        let mut max_err = 0.0f32;
        for i in (LOOP_XFADE + 4096)..out.len() - 4096 {
            max_err = max_err.max((out[i] - src[i]).abs());
        }
        assert!(max_err < 2e-2, "max identity error {max_err}");
    }

    /// α = 1 toward a much quieter donor: the variant's level drops clearly.
    #[test]
    fn alpha_one_pulls_level_toward_donor() {
        let a = seed_with("a", -6.0, 48_000);
        let b = seed_with("b", -36.0, 48_000);
        let variant = render_variant(RenderJob {
            source: &a,
            donor: Some(&b),
            alpha: 1.0,
            variate: None,
        });
        let rms = |x: &[f32]| {
            (x.iter().map(|v| v * v).sum::<f32>() / x.len() as f32).sqrt()
        };
        let db_drop =
            20.0 * (rms(&variant.channels[0]) / rms(&a.channels[0])).log10();
        assert!(
            (-14.0..=-2.0).contains(&db_drop),
            "expected a clear drop, got {db_drop:.2} dB"
        );
    }

    /// The loop crossfade must actually shorten the buffer and keep it finite.
    #[test]
    fn crossfade_shortens_and_stays_finite() {
        let a = seed_with("a", -12.0, 48_000);
        let variant = render_variant(RenderJob {
            source: &a,
            donor: None,
            alpha: 0.0,
            variate: None,
        });
        assert_eq!(
            variant.len(),
            a.channels[0].len() - FFT_SIZE - LOOP_XFADE
        );
        assert!(variant.channels.iter().flat_map(|c| c.iter()).all(|v| v.is_finite()));
    }

    /// The demo export path round-trips through the WAV writer.
    #[test]
    fn variant_writes_to_wav() {
        let a = seed_with("a", -12.0, 48_000);
        let variant = render_variant(RenderJob {
            source: &a,
            donor: None,
            alpha: 0.0,
            variate: None,
        });
        let tmp = std::env::temp_dir().join("akibloom_test_variant.wav");
        write_variant(&tmp, &variant).unwrap();
        let (chans, sr) = wav::read_wav_channels(&tmp).unwrap();
        assert_eq!(sr, 48_000);
        assert_eq!(chans.len(), 2);
        assert_eq!(chans[0].len(), variant.len());
        let _ = std::fs::remove_file(&tmp);
    }
}
