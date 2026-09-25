//! Copied verbatim from the sibling AkiGraft project (shared kernel, one-way
//! copy — see AkiBloom PLAN.md). Do not diverge without updating both.
//! The graft engine: spectral-envelope morphing toward parent timbres,
//! plus the variation (杂交/变异) transform shared by manual Hybridize and
//! the Cultivate walker.
//!
//! Per STFT frame, the input's mel-band log energies are measured and each
//! band is pushed toward the (blended) parent profile by `alpha`, clamped to
//! `±MAX_BAND_SHIFT_DB`. Band gains are interpolated back to per-bin gains
//! and applied to the magnitudes; phases stay untouched, so pitch, rhythm,
//! and stereo image of the input survive — only the timbral "skin" changes.
//!
//! The variation transform then perturbs band magnitudes in a tag-specific
//! direction (湿润 / 颗粒 / 呼吸感) from a deterministic (seed, band,
//! time-bucket) hash — same seed reproduces the same variation bit-for-bit.
//!
//! `alpha = 0` and `amount = 0` are strict identities (原声不动), matching
//! the locked product decision that untouched controls leave the signal
//! clean.

use crate::mel::MelBands;
use crate::parents::ParentProfile;
use crate::stft::{Polar, SpectralConfig, SpectralEngine, WindowType};
use nih_plug::params::smoothing::{Smoother, SmoothingStyle};

/// 2048-point FFT at 75% overlap — ~43 ms window at 48 kHz, fine detail
/// while staying cheap.
pub const FFT_SIZE: usize = 2048;
pub const OVERLAP: usize = 4;
/// Mel band count shared by runtime frames and parent profiles.
pub const NUM_BANDS: usize = 24;
pub use crate::mel::{DEFAULT_FMAX_HZ, DEFAULT_FMIN_HZ};
/// The spectral analogue of the paper's 3σ clip: per-band envelope pushes
/// are clamped so a silent input band can never be boosted into the parent's
/// level (and a loud band never ducked to silence).
pub const MAX_BAND_SHIFT_DB: f32 = 12.0;

// ── Control structures ──────────────────────────────────────────────────

/// One variation state: which direction (tag), how strong, and the
/// deterministic seed. Both manual Hybridize and the Cultivate walker drive
/// the engine through this.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct VariateState {
    /// 0 = 湿润 wet, 1 = 颗粒 grainy, 2 = 呼吸感 breathing.
    pub tag: u8,
    /// `[0, 1]` — overall depth of the variation.
    pub amount: f32,
    pub seed: u64,
}

/// Everything the engine needs per block, resolved by the plugin layer.
#[derive(Debug, Clone, Copy, Default)]
pub struct GraftControl {
    pub alpha: f32,
    pub blend: f32,
    pub mix: f32,
    pub out_gain: f32,
    /// `None` = variation transform off (identity).
    pub variate: Option<VariateState>,
}

// ── Deterministic pseudo-randomness ─────────────────────────────────────

/// SplitMix64-style hash → `[-1, 1]`. Deterministic across runs and
/// platforms: the same `(seed, band, bucket)` always yields the same value,
/// which is what makes Cultivate's rollback faithful.
fn hash_f32(seed: u64, band: u32, bucket: u64) -> f32 {
    let mut x = seed
        ^ (band as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15)
        ^ bucket.wrapping_mul(0xBF58_476D_1CE4_E5B9);
    x ^= x >> 30;
    x = x.wrapping_mul(0xBF58_476D_1CE4_E5B9);
    x ^= x >> 27;
    x = x.wrapping_mul(0x94D0_49BB_1331_11EB);
    x ^= x >> 31;
    ((x >> 40) as f32 / (1u64 << 24) as f32) * 2.0 - 1.0
}

/// Logistic helper for smooth band transitions.
fn sigmoid(x: f32) -> f32 {
    1.0 / (1.0 + (-x).exp())
}

/// Per-tag static spectral shape in dB across the `NUM_BANDS` bands
/// (60 Hz – 18 kHz, 24 bands ≈ each band ~0.19 octaves).
///
/// * 湿润 wet (0): body in the low mids (150–900 Hz), softened highs —
///   "liquid" also gets slow, smooth jitter (8-frame buckets).
/// * 颗粒 grainy (1): bright tilt (2–8 kHz) plus a comb-like ripple across
///   bands, jittering every frame — granular sparkle.
/// * 呼吸感 breathing (2): gentle high shelf, minimal jitter, and a slow
///   amplitude LFO handled separately.
fn tag_shape_db(tag: u8, band: usize) -> f32 {
    let b = band as f32;
    match tag {
        // Wet: bump around band 3 (~250 Hz), roll off above band 13.
        0 => 4.0 * (-((b - 3.0).powi(2)) / 6.0).exp() - 3.0 * sigmoid((b - 13.0) / 2.5),
        // Grainy: bright tilt above band 12 (~2 kHz) + comb ripple.
        1 => 3.0 * sigmoid((b - 12.0) / 3.0) + 2.5 * (b * 2.1).cos(),
        // Breathing: gentle shelf above band 12.
        2 => 2.5 * sigmoid((b - 12.0) / 2.5),
        _ => 0.0,
    }
}

/// How many frames one jitter bucket lasts per tag. Wet moves slowly
/// (smooth, liquid), grainy flickers per frame (sparkle).
fn jitter_bucket_frames(tag: u8) -> u64 {
    match tag {
        0 => 8,
        1 => 1,
        _ => 32,
    }
}

/// Jitter depth (dB at amount = 1) per tag.
fn jitter_depth_db(tag: u8) -> f32 {
    match tag {
        0 => 2.5,
        1 => 4.0,
        _ => 1.0,
    }
}

#[cfg(test)]
mod shape_tests {
    use super::*;

    #[test]
    fn wet_shape_peaks_in_low_mids() {
        let peak = (0..NUM_BANDS)
            .map(|b| tag_shape_db(0, b))
            .fold(f32::MIN, f32::max);
        let peak_band = (0..NUM_BANDS).max_by_key(|b| (tag_shape_db(0, *b) * 100.0) as i32);
        assert!(peak > 2.0 && peak_band.unwrap() < 8);
        assert!(tag_shape_db(0, 20) < tag_shape_db(0, 3));
    }

    #[test]
    fn grainy_shape_is_bright_and_rippled() {
        assert!(tag_shape_db(1, 16) > tag_shape_db(1, 2));
        // Ripple: some adjacent band pair must differ by > 1.5 dB.
        let max_diff = (1..NUM_BANDS)
            .map(|b| (tag_shape_db(1, b) - tag_shape_db(1, b - 1)).abs())
            .fold(0.0f32, f32::max);
        assert!(max_diff > 1.5);
    }

    #[test]
    fn breath_shape_is_mild_shelf() {
        assert!(tag_shape_db(2, 20) > tag_shape_db(2, 2));
        assert!(tag_shape_db(2, 20) < 4.0);
    }

    #[test]
    fn hash_is_deterministic_and_spread() {
        let a = hash_f32(42, 3, 7);
        assert_eq!(a, hash_f32(42, 3, 7));
        assert_ne!(a, hash_f32(43, 3, 7));
        assert_ne!(a, hash_f32(42, 4, 7));
        assert_ne!(a, hash_f32(42, 3, 8));
    }
}

// ── Engine ──────────────────────────────────────────────────────────────

pub struct GraftCore {
    engine: SpectralEngine,
    mel: MelBands,
    sample_rate: f32,
    /// Pre-allocated dry copies (engine input + mix source).
    dry: Vec<Vec<f32>>,
    /// Pre-allocated wet buffers (engine output).
    wet: Vec<Vec<f32>>,
    /// Reusable per-frame scratch (audio thread only, never allocated inside
    /// the frame callback).
    mags: Vec<f32>,
    frame_db: Vec<f32>,
    gains_lin: Vec<f32>,
    var_gains_db: Vec<f32>,
    blended_db: Vec<f32>,
    /// Frame-level parameter smoothing (kills zipper on fast automation).
    alpha_s: Smoother<f32>,
    blend_s: Smoother<f32>,
    mix_s: Smoother<f32>,
    gain_s: Smoother<f32>,
}

impl GraftCore {
    pub fn new(sample_rate: f32) -> Self {
        let engine = SpectralEngine::new(
            SpectralConfig {
                fft_size: FFT_SIZE,
                overlap_count: OVERLAP,
                window: WindowType::Hann,
            },
            2,
        );
        let mel = MelBands::new(
            NUM_BANDS,
            FFT_SIZE / 2,
            sample_rate,
            DEFAULT_FMIN_HZ,
            DEFAULT_FMAX_HZ,
        );
        Self {
            engine,
            mel,
            sample_rate,
            dry: vec![Vec::new(), Vec::new()],
            wet: vec![Vec::new(), Vec::new()],
            mags: vec![0.0; FFT_SIZE / 2],
            frame_db: Vec::with_capacity(NUM_BANDS),
            gains_lin: vec![1.0; NUM_BANDS],
            var_gains_db: vec![0.0; NUM_BANDS],
            blended_db: Vec::with_capacity(NUM_BANDS),
            alpha_s: Smoother::new(SmoothingStyle::Linear(80.0)),
            blend_s: Smoother::new(SmoothingStyle::Linear(80.0)),
            mix_s: Smoother::new(SmoothingStyle::Linear(60.0)),
            gain_s: Smoother::new(SmoothingStyle::Linear(50.0)),
        }
    }

    pub fn latency_samples(&self) -> usize {
        self.engine.latency_samples()
    }

    pub fn reset(&mut self) {
        self.engine.reset();
    }

    /// Process one block in place. `io` holds 1 (mono) or 2 (stereo) channel
    /// slices; mono input is duplicated internally and written back mono.
    pub fn process_block(
        &mut self,
        io: &mut [&mut [f32]],
        parent_a: Option<&ParentProfile>,
        parent_b: Option<&ParentProfile>,
        ctl: GraftControl,
    ) {
        let n = io[0].len();
        let in_channels = io.len().min(2);

        // Per-block smoothing targets; values are advanced per frame/sample
        // below.
        self.alpha_s.set_target(self.sample_rate, ctl.alpha);
        self.blend_s.set_target(self.sample_rate, ctl.blend);
        self.mix_s.set_target(self.sample_rate, ctl.mix);
        self.gain_s.set_target(self.sample_rate, ctl.out_gain);

        // 1. Dry copies (engine input + mix source). Mono input feeds both
        //    engine channels.
        for c in 0..2 {
            if self.dry[c].len() < n {
                self.dry[c].resize(n, 0.0);
            }
            let src = &*io[c.min(in_channels - 1)];
            self.dry[c][..n].copy_from_slice(&src[..n]);
        }

        // 2. Blend the parent profile using the block-end blend value
        //    (advance the blend smoother across the whole block).
        let mut blend = self.blend_s.next();
        for _ in 1..n {
            blend = self.blend_s.next();
        }
        if !self.blend_profiles(parent_a, parent_b, blend) {
            // Still advance the per-sample smoothers so re-activation stays
            // smooth when a parent comes back.
            for _ in 0..n {
                let _ = self.mix_s.next();
                let _ = self.gain_s.next();
            }
            return;
        }

        // 3. Run the engine: dry in → wet out.
        for c in 0..2 {
            if self.wet[c].len() < n {
                self.wet[c].resize(n, 0.0);
            }
        }

        let num_bins = FFT_SIZE / 2;
        let hop = FFT_SIZE / OVERLAP;
        // Split borrows so the per-frame callback can touch scratch fields
        // while `engine` is mutably borrowed.
        let Self {
            engine,
            mel,
            sample_rate,
            dry,
            wet,
            mags,
            frame_db,
            gains_lin,
            var_gains_db,
            blended_db,
            alpha_s,
            blend_s: _,
            mix_s,
            gain_s,
        } = self;
        let mel = &*mel;
        let sample_rate = *sample_rate;
        let mags = &mut *mags;
        let frame_db = &mut *frame_db;
        let gains_lin = &mut *gains_lin;
        let var_gains_db = &mut *var_gains_db;
        let blended_db = &*blended_db;
        let mut callback = |_bins: usize, _chan: usize, seq: usize, polar: &mut [Polar]| {
            // Advance the frame-level smoother to this frame's position:
            // consume hop values, keep the last.
            let mut alpha = alpha_s.next();
            for _ in 1..hop {
                alpha = alpha_s.next();
            }

            // ── Graft: push bands toward the blended parent profile ──
            for (i, p) in polar.iter().enumerate().take(num_bins) {
                mags[i] = p.magnitude;
            }
            mel.band_db(mags, frame_db);
            for b in 0..NUM_BANDS {
                let delta = alpha
                    * (blended_db[b] - frame_db[b]).clamp(-MAX_BAND_SHIFT_DB, MAX_BAND_SHIFT_DB);
                gains_lin[b] = 10f32.powf(delta / 20.0);
            }

            // ── Variation transform (杂交/变异) — same bucket for L/R ──
            let amount = match ctl.variate {
                Some(v) if v.amount > 0.0005 => v.amount,
                _ => 0.0,
            };
            if amount > 0.0 {
                let v = ctl.variate.unwrap();
                let tag = v.tag.min(2);
                let bucket = (seq as u64) / jitter_bucket_frames(tag);
                for b in 0..NUM_BANDS {
                    let jitter = hash_f32(v.seed, b as u32, bucket) * jitter_depth_db(tag);
                    let mut db = amount * (tag_shape_db(tag, b) + jitter);
                    // Breathing: slow amplitude LFO on top (~0.35 Hz).
                    if tag == 2 {
                        let t = seq as f32 * hop as f32 / sample_rate;
                        db += amount * 2.0 * (std::f32::consts::TAU * 0.35 * t).sin();
                    }
                    var_gains_db[b] = db.clamp(-MAX_BAND_SHIFT_DB, MAX_BAND_SHIFT_DB);
                }
                for g in var_gains_db.iter_mut() {
                    *g = 10f32.powf(*g / 20.0);
                }
            }

            // ── Apply combined per-bin gains ──
            for (i, p) in polar.iter_mut().enumerate().take(num_bins) {
                let (lo, hi, t) = mel.bin_map(i);
                let mut g = (1.0 - t) * gains_lin[lo as usize] + t * gains_lin[hi as usize];
                if amount > 0.0 {
                    let gv = (1.0 - t) * var_gains_db[lo as usize] + t * var_gains_db[hi as usize];
                    g *= gv;
                }
                p.magnitude *= g;
            }
        };

        // Split the wet pair so the two elements can be borrowed mutably at
        // the same time.
        let (wet0, wet1) = wet.split_at_mut(1);
        engine.process(
            &[&dry[0][..n], &dry[1][..n]],
            &mut [&mut wet0[0][..n], &mut wet1[0][..n]],
            &mut callback,
        );

        // 4. Dry/wet crossfade + output gain, written back in place with
        //    per-sample smoothing.
        for c in 0..in_channels {
            for i in 0..n {
                io[c][i] =
                    ((1.0 - mix_s.next()) * dry[c][i] + mix_s.next() * wet[c][i]) * gain_s.next();
            }
        }
    }

    /// Fill `blended_db` from the loaded parents (dB-domain lerp by `blend`).
    /// Returns false when no parent is loaded (caller then bypasses).
    fn blend_profiles(
        &mut self,
        parent_a: Option<&ParentProfile>,
        parent_b: Option<&ParentProfile>,
        blend: f32,
    ) -> bool {
        self.blended_db.clear();
        match (parent_a, parent_b) {
            (Some(a), Some(b)) => {
                for b_idx in 0..NUM_BANDS {
                    self.blended_db
                        .push(a.band_db[b_idx] * (1.0 - blend) + b.band_db[b_idx] * blend);
                }
                true
            }
            (Some(a), None) => {
                self.blended_db.extend_from_slice(&a.band_db);
                true
            }
            (None, Some(b)) => {
                self.blended_db.extend_from_slice(&b.band_db);
                true
            }
            (None, None) => false,
        }
    }
}

    /// Absolute time in seconds for the breathing LFO, from the frame index.
    #[cfg(test)]
    fn self_var_time(seq: usize, hop: usize, sample_rate: f32) -> f32 {
        seq as f32 * hop as f32 / sample_rate
    }

#[cfg(test)]
mod tests {
    use super::*;

    fn parent_with(db: f32) -> ParentProfile {
        ParentProfile {
            name: "test".into(),
            sample_rate: 48_000,
            channels: 1,
            frames: 1,
            band_db: vec![db; NUM_BANDS],
        }
    }

    /// Deterministic LCG noise so assertions are stable across runs.
    struct Noise(u32);
    impl Noise {
        fn next(&mut self) -> f32 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 17;
            self.0 ^= self.0 << 5;
            (self.0 as f32 / u32::MAX as f32) * 2.0 - 1.0
        }
    }

    #[test]
    fn alpha_zero_is_identity_after_latency() {
        let mut core = GraftCore::new(48_000.0);
        let parent = parent_with(-20.0);
        let latency = core.latency_samples();

        let n = 65_536;
        let mut rng = Noise(0x1234_5678);
        let input: Vec<f32> = (0..n).map(|_| rng.next()).collect();
        let mut l = input.clone();
        let mut r = input.clone();

        core.process_block(
            &mut [&mut l, &mut r],
            Some(&parent),
            None,
            GraftControl {
                alpha: 0.0,
                blend: 0.0,
                mix: 1.0,
                out_gain: 1.0,
                variate: None,
            },
        );

        for i in (latency + FFT_SIZE)..n {
            let err = (l[i] - input[i - latency]).abs();
            assert!(err < 1e-2, "sample {i}: {} vs {}", l[i], input[i - latency]);
        }
    }

    #[test]
    fn alpha_one_pulls_level_toward_parent() {
        let mut core = GraftCore::new(48_000.0);
        let parent = parent_with(-40.0);

        let n = 65_536;
        let mut rng = Noise(0xdead_beef);
        let input: Vec<f32> = (0..n).map(|_| rng.next()).collect();
        let mut l = input.clone();
        let mut r = input.clone();

        core.process_block(
            &mut [&mut l, &mut r],
            Some(&parent),
            None,
            GraftControl {
                alpha: 1.0,
                blend: 0.0,
                mix: 1.0,
                out_gain: 1.0,
                variate: None,
            },
        );

        let tail = n - 4 * FFT_SIZE;
        let rms = |x: &[f32]| (x.iter().map(|v| v * v).sum::<f32>() / tail as f32).sqrt();
        let db_drop = 20.0 * (rms(&l[4 * FFT_SIZE..]) / rms(&input[4 * FFT_SIZE..])).log10();
        assert!(
            (-15.0..=-2.0).contains(&db_drop),
            "expected a clear drop, got {db_drop:.2} dB"
        );
    }

    #[test]
    fn no_parents_leaves_buffer_untouched() {
        let mut core = GraftCore::new(48_000.0);
        let mut l: Vec<f32> = (0..1024).map(|i| (i as f32 * 0.01).sin()).collect();
        let original = l.clone();
        let mut r = vec![0.0; 1024];
        core.process_block(
            &mut [&mut l, &mut r],
            None,
            None,
            GraftControl {
                alpha: 1.0,
                blend: 0.0,
                mix: 1.0,
                out_gain: 1.0,
                variate: None,
            },
        );
        assert_eq!(l, original);
    }

    #[test]
    fn blend_profiles_two_parents_lerps() {
        let mut core = GraftCore::new(48_000.0);
        let a = parent_with(-10.0);
        let b = parent_with(-30.0);
        assert!(core.blend_profiles(Some(&a), Some(&b), 0.5));
        assert!((core.blended_db[0] - (-20.0)).abs() < 1e-4);
        assert!(core.blend_profiles(Some(&a), None, 0.7));
        assert!((core.blended_db[0] - (-10.0)).abs() < 1e-4);
        assert!(!core.blend_profiles(None, None, 0.5));
        assert!(core.blended_db.is_empty());
    }

    #[test]
    fn mono_input_written_back_mono() {
        let mut core = GraftCore::new(48_000.0);
        let parent = parent_with(-20.0);
        let mut rng = Noise(7);
        let mut mono: Vec<f32> = (0..16_384).map(|_| rng.next()).collect();
        core.process_block(
            &mut [&mut mono],
            Some(&parent),
            None,
            GraftControl {
                alpha: 0.5,
                blend: 0.0,
                mix: 1.0,
                out_gain: 1.0,
                variate: None,
            },
        );
        assert!(mono.iter().all(|v| v.is_finite()));
    }

    #[test]
    fn amount_zero_variation_is_identity() {
        let mut core = GraftCore::new(48_000.0);
        let parent = parent_with(-20.0);
        let latency = core.latency_samples();

        let n = 65_536;
        let mut rng = Noise(99);
        let input: Vec<f32> = (0..n).map(|_| rng.next()).collect();
        let mut l = input.clone();
        let mut r = input.clone();

        core.process_block(
            &mut [&mut l, &mut r],
            Some(&parent),
            None,
            GraftControl {
                alpha: 0.0,
                blend: 0.0,
                mix: 1.0,
                out_gain: 1.0,
                variate: Some(VariateState {
                    tag: 1,
                    amount: 0.0,
                    seed: 7,
                }),
            },
        );
        for i in (latency + FFT_SIZE)..n {
            assert!((l[i] - input[i - latency]).abs() < 1e-2);
        }
    }

    /// Same seed → identical output; different seed → different output.
    #[test]
    fn variation_is_seed_deterministic() {
        let run = |seed: u64| {
            let mut core = GraftCore::new(48_000.0);
            let parent = parent_with(-20.0);
            let mut rng = Noise(5);
            let input: Vec<f32> = (0..32_768).map(|_| rng.next()).collect();
            let mut l = input.clone();
            let mut r = input.clone();
            core.process_block(
                &mut [&mut l, &mut r],
                Some(&parent),
                None,
                GraftControl {
                    alpha: 0.0,
                    blend: 0.0,
                    mix: 1.0,
                    out_gain: 1.0,
                    variate: Some(VariateState {
                        tag: 0,
                        amount: 1.0,
                        seed,
                    }),
                },
            );
            l
        };
        let a1 = run(123);
        let a2 = run(123);
        let b = run(456);
        let tail = 8 * FFT_SIZE..32_768 - FFT_SIZE;
        let d_same = a1[tail.clone()]
            .iter()
            .zip(a2[tail.clone()].iter())
            .map(|(x, y)| (x - y).abs())
            .fold(0.0f32, f32::max);
        let d_diff = a1[tail.clone()]
            .iter()
            .zip(b[tail.clone()].iter())
            .map(|(x, y)| (x - y).abs())
            .fold(0.0f32, f32::max);
        assert!(d_same < 1e-5, "same seed must reproduce ({d_same})");
        assert!(d_diff > 1e-3, "different seed must differ ({d_diff})");
    }
}
