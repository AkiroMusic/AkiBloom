//! Copied verbatim from the sibling AkiGraft project (shared kernel, one-way
//! copy — see AkiBloom PLAN.md). Do not diverge without updating both.
//! Mel-band spectral analysis for the graft engine.
//!
//! Bands are defined purely in Hz over a shared range (60 Hz – 18 kHz by
//! default), so a parent profile computed from a 44.1 kHz file is directly
//! comparable to frames analysed at 48 kHz — no resampling needed.

/// Hz → mel (`2595 · log10(1 + hz/700)`), the standard Sant'Anna mapping.
fn hz_to_mel(hz: f32) -> f32 {
    2595.0 * (1.0 + hz / 700.0).log10()
}

/// mel → Hz.
fn mel_to_hz(m: f32) -> f32 {
    700.0 * (10.0f32.powf(m / 2595.0) - 1.0)
}

/// Sparse mel-band mapping over FFT bins.
///
/// `bin_map[bin] = (lo, hi, t)` — the bin's value interpolates linearly
/// between band `lo` and band `hi` by weight `t` (bands are indices into the
/// `num_bands` array). The same mapping is used in both directions: band
/// energies accumulate from bins with the `(1-t) / t` split, and per-bin
/// gains interpolate back from per-band gains.
pub struct MelBands {
    num_bands: usize,
    bin_map: Vec<(u16, u16, f32)>,
}

pub const DEFAULT_FMIN_HZ: f32 = 60.0;
pub const DEFAULT_FMAX_HZ: f32 = 18_000.0;

impl MelBands {
    pub fn new(
        num_bands: usize,
        num_bins: usize,
        sample_rate: f32,
        fmin_hz: f32,
        fmax_hz: f32,
    ) -> Self {
        assert!(num_bands >= 2);
        assert!(fmax_hz > fmin_hz);
        // Nyquist ceiling: never analyse above half the sample rate.
        let fmax = fmax_hz.min(sample_rate * 0.5);
        assert!(fmax > fmin_hz);

        // Band centers are uniformly spaced in mel space.
        let m_min = hz_to_mel(fmin_hz);
        let m_max = hz_to_mel(fmax);
        let centers: Vec<f32> = (0..num_bands)
            .map(|b| {
                let t = b as f32 / (num_bands - 1) as f32;
                mel_to_hz(m_min + t * (m_max - m_min))
            })
            .collect();

        let mut bin_map = Vec::with_capacity(num_bins);
        for bin in 0..num_bins {
            let freq = bin as f32 * sample_rate / (num_bins as f32 * 2.0);
            if freq <= centers[0] {
                bin_map.push((0, 0, 0.0));
                continue;
            }
            if freq >= centers[num_bands - 1] {
                let last = (num_bands - 1) as u16;
                bin_map.push((last, last, 0.0));
                continue;
            }
            // Find the segment between two adjacent centers.
            let mut seg = 0;
            while seg < num_bands - 2 && freq >= centers[seg + 1] {
                seg += 1;
            }
            let span = centers[seg + 1] - centers[seg];
            let t = if span > 0.0 {
                ((freq - centers[seg]) / span).clamp(0.0, 1.0)
            } else {
                0.0
            };
            bin_map.push((seg as u16, (seg + 1) as u16, t));
        }

        Self { num_bands, bin_map }
    }

    #[inline]
    pub fn num_bands(&self) -> usize {
        self.num_bands
    }

    /// The `(lo, hi, t)` interpolation endpoints for one bin.
    #[inline]
    pub fn bin_map(&self, bin: usize) -> (u16, u16, f32) {
        self.bin_map[bin]
    }

    /// Log-band energies (dB) from a magnitude spectrum.
    ///
    /// `out` is resized to `num_bands` and filled with
    /// `10·log10(energy)`, floored at `FLOOR_DB`.
    pub fn band_db(&self, magnitudes: &[f32], out: &mut Vec<f32>) {
        let nb = self.num_bands;
        out.clear();
        out.resize(nb, 0.0);

        let bins = self.bin_map.len().min(magnitudes.len());
        for bin in 0..bins {
            let m2 = magnitudes[bin] * magnitudes[bin];
            let (lo, hi, t) = self.bin_map[bin];
            out[lo as usize] += (1.0 - t) * m2;
            if hi != lo {
                out[hi as usize] += t * m2;
            }
        }

        for e in out.iter_mut() {
            *e = 10.0 * ((*e + 1e-12).log10());
            if *e < FLOOR_DB {
                *e = FLOOR_DB;
            }
        }
    }
}

/// Floor for band energies in dB — anything below counts as silence.
pub const FLOOR_DB: f32 = -90.0;

#[cfg(test)]
mod tests {
    use super::*;

    fn bands() -> MelBands {
        MelBands::new(24, 1024, 48_000.0, DEFAULT_FMIN_HZ, DEFAULT_FMAX_HZ)
    }

    #[test]
    fn bin_map_covers_all_bands() {
        let mel = bands();
        let mut seen = vec![false; 24];
        for bin in 0..1024 {
            let (lo, hi, t) = mel.bin_map(bin);
            assert!((lo as usize) < 24 && (hi as usize) < 24);
            assert!((0.0..=1.0).contains(&t));
            if t < 1.0 {
                seen[lo as usize] = true;
            }
            if t > 0.0 && hi != lo {
                seen[hi as usize] = true;
            }
        }
        // Every band must be reachable by some bin (60 Hz – 18 kHz at 48 kHz
        // with 1024 bins spans bins 1..=384, comfortably covering 24 bands).
        assert!(seen.iter().all(|s| *s), "band unreachable: {seen:?}");
    }

    #[test]
    fn silence_floors_at_minus_90_db() {
        let mel = bands();
        let mut out = Vec::new();
        mel.band_db(&vec![0.0; 1024], &mut out);
        assert_eq!(out.len(), 24);
        assert!(out.iter().all(|db| (*db - FLOOR_DB).abs() < 1e-4));
    }

    #[test]
    fn energy_concentrates_in_expected_band() {
        let mel = bands();
        // A pure 1 kHz sine's magnitude is concentrated near bin 21.3 at
        // 48 kHz with 2048-point FFT (num_bins = 1024).
        let mut mags = vec![0.0; 1024];
        mags[21] = 1.0;
        mags[22] = 0.5;
        let mut out = Vec::new();
        mel.band_db(&mags, &mut out);
        // Bands far from 1 kHz must sit at the floor.
        let (lo, hi, _) = mel.bin_map(21);
        let peak = out[lo as usize].max(out[hi as usize]);
        let far = out[0].min(out[23]);
        assert!(peak - far > 30.0, "peak {peak} dB vs far {far} dB");
    }
}
