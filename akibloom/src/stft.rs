//! Copied verbatim from the sibling AkiGraft project (shared kernel, one-way
//! copy — see AkiBloom PLAN.md). Do not diverge without updating both.
//! STFT engine for AkiGraft.
//!
//! Sliding-window overlap-add design: per channel, incoming samples queue in
//! a FIFO; every `hop_size` new samples the oldest `fft_size` samples form a
//! frame, which is windowed, transformed, spectrally modified,
//! inverse-transformed, re-windowed, and accumulated into an overlap tail.
//! Arbitrary host block sizes are handled naturally — nothing is dropped,
//! and an untouched spectral callback reconstructs the input exactly (up to
//! float error) at a delay of exactly `fft_size` samples.
//!
//! The one-sided real reconstruction scales DC and Nyquist by 1/N and the
//! inner bins by 2/N; doubling DC or dropping Nyquist (a classic STFT bug)
//! would corrupt low and high frequencies respectively.

use std::collections::VecDeque;
use std::sync::Arc;

use rustfft::num_complex::Complex;
use rustfft::{Fft, FftPlanner};

// ── Polar ───────────────────────────────────────────────────────────────

/// A magnitude/phase pair — the currency type between the STFT engine and
/// AkiGraft's spectral callback.
#[derive(Debug, Clone, Copy, Default)]
pub struct Polar {
    pub magnitude: f32,
    pub phase: f32,
}

impl Polar {
    #[inline]
    pub fn new(magnitude: f32, phase: f32) -> Self {
        Self { magnitude, phase }
    }

    #[inline]
    pub fn from_complex(c: Complex<f32>) -> Self {
        Self {
            magnitude: c.norm(),
            phase: c.arg(),
        }
    }

    #[inline]
    pub fn to_complex(self) -> Complex<f32> {
        Complex::from_polar(self.magnitude, self.phase)
    }
}

// ── Window ──────────────────────────────────────────────────────────────

/// FFT window functions (coefficients matching SpectralSuite's convention).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WindowType {
    Rectangular,
    Hann,
    Hamming,
    Blackman,
    BlackmanHarris,
}

/// `6.28318531`, matching the C++ `TWOPI` constant.
const TWOPI: f32 = 6.28318531;

impl WindowType {
    pub fn generate(&self, len: usize) -> Vec<f32> {
        match self {
            Self::Rectangular => vec![1.0; len],
            Self::Hann => Self::gen_hann(len),
            Self::Hamming => Self::gen_hamming(len),
            Self::Blackman => Self::gen_blackman(len),
            Self::BlackmanHarris => Self::gen_blackman_harris(len),
        }
    }

    fn gen_hann(len: usize) -> Vec<f32> {
        let max = (len as f32) - 1.0;
        (0..len)
            .map(|n| {
                let w = TWOPI * (n as f32 / max);
                0.5 * (1.0 - w.cos())
            })
            .collect()
    }

    fn gen_hamming(len: usize) -> Vec<f32> {
        let max = (len as f32) + 1.0;
        (0..len)
            .map(|n| {
                let w = TWOPI * (n as f32 / max);
                0.53836 - 0.46164 * w.cos()
            })
            .collect()
    }

    fn gen_blackman(len: usize) -> Vec<f32> {
        let max = (len as f32) - 1.0;
        (0..len)
            .map(|n| {
                let ratio = n as f32 / max;
                let w1 = TWOPI * ratio;
                let w2 = TWOPI * 2.0 * ratio;
                0.42 - (0.5 * w1.cos() + 0.08 * w2.cos())
            })
            .collect()
    }

    fn gen_blackman_harris(len: usize) -> Vec<f32> {
        let max = (len as f32) - 1.0;
        (0..len)
            .map(|n| {
                let inc = n as f32 / max;
                let w1 = TWOPI * inc;
                let w2 = TWOPI * 2.0 * inc;
                let w3 = TWOPI * 6.0 * inc;
                0.35875 - 0.48829 * w1.cos() + 0.14128 * w2.cos() - 0.01168 * w3.cos()
            })
            .collect()
    }
}

// ── Engine ──────────────────────────────────────────────────────────────

/// Configuration for [`SpectralEngine`].
#[derive(Debug, Clone)]
pub struct SpectralConfig {
    pub fft_size: usize,
    pub overlap_count: usize,
    pub window: WindowType,
}

/// Per-channel streaming state.
struct ChannelState {
    /// Pending input samples, oldest first.
    in_fifo: VecDeque<f32>,
    /// Accumulated synthesis output awaiting emission, oldest first.
    out_fifo: VecDeque<f32>,
    /// Overlap tail: contributions from frames not yet fully emitted.
    tail: Vec<f32>,
    /// Total samples pushed / emitted — the emission gate that keeps the
    /// algorithmic delay at exactly `fft_size` regardless of host block
    /// size (frames may compute ahead within a block, but output for
    /// position `p` is only released once `p + fft_size` samples have been
    /// received).
    total_in: usize,
    total_out: usize,
}

pub struct SpectralEngine {
    fft_size: usize,
    half_size: usize,
    hop_size: usize,
    window: Vec<f32>,
    num_channels: usize,
    fft_fwd: Arc<dyn Fft<f32>>,
    fft_inv: Arc<dyn Fft<f32>>,
    /// `1 / Σ w²` — overlap-add normalization, computed as the worst-case
    /// sum across positions so unity is never exceeded.
    output_norm: f32,
    channels: Vec<ChannelState>,
    frame_seq: usize,
    /// Scratch reused across frames (fft_size complex).
    fft_buf: Vec<Complex<f32>>,
    ifft_buf: Vec<Complex<f32>>,
    polar_buf: Vec<Polar>,
}

impl SpectralEngine {
    pub fn new(config: SpectralConfig, num_channels: usize) -> Self {
        assert!(config.fft_size.is_power_of_two());
        assert!(config.fft_size >= 128 && config.fft_size <= 16384);
        assert!(config.overlap_count >= 1 && config.overlap_count <= 8);
        assert!(config.fft_size % config.overlap_count == 0);

        let fft_size = config.fft_size;
        let half_size = fft_size / 2;
        let hop_size = fft_size / config.overlap_count;
        let window = config.window.generate(fft_size);

        let mut planner = FftPlanner::<f32>::new();
        let fft_fwd = planner.plan_fft_forward(fft_size);
        let fft_inv = planner.plan_fft_inverse(fft_size);

        let mut w2 = vec![0.0f32; fft_size];
        for (i, w) in window.iter().enumerate() {
            w2[i] = w * w;
        }
        let mut cola_min = f32::INFINITY;
        for n in 0..fft_size {
            let sum: f32 = (0..config.overlap_count)
                .map(|k| w2[(n + k * hop_size) % fft_size])
                .sum();
            cola_min = cola_min.min(sum);
        }

        let channels = (0..num_channels)
            .map(|_| ChannelState {
                in_fifo: VecDeque::with_capacity(fft_size * 2),
                out_fifo: VecDeque::with_capacity(fft_size * 2),
                tail: vec![0.0; fft_size],
                total_in: 0,
                total_out: 0,
            })
            .collect();

        Self {
            fft_size,
            half_size,
            hop_size,
            window,
            num_channels,
            fft_fwd,
            fft_inv,
            output_norm: 1.0 / cola_min,
            channels,
            frame_seq: 0,
            fft_buf: vec![Complex::default(); fft_size],
            ifft_buf: vec![Complex::default(); fft_size],
            polar_buf: vec![Polar::default(); half_size],
        }
    }

    /// True algorithmic delay: one full frame.
    #[inline]
    pub fn latency_samples(&self) -> usize {
        self.fft_size
    }

    #[inline]
    pub fn num_bins(&self) -> usize {
        self.half_size
    }

    pub fn reset(&mut self) {
        for ch in &mut self.channels {
            ch.in_fifo.clear();
            ch.out_fifo.clear();
            ch.tail.iter_mut().for_each(|s| *s = 0.0);
            ch.total_in = 0;
            ch.total_out = 0;
        }
    }

    /// Process a block of audio for all channels. The callback is invoked
    /// once per FFT frame per channel with
    /// `(num_bins, channel_idx, frame_seq, polar)`.
    ///
    /// `input` and `output` must not alias.
    pub fn process<F>(&mut self, input: &[&[f32]], output: &mut [&mut [f32]], callback: &mut F)
    where
        F: FnMut(usize, usize, usize, &mut [Polar]),
    {
        let n = input[0].len();
        let Self {
            fft_fwd,
            fft_inv,
            window,
            output_norm,
            fft_size,
            half_size,
            hop_size,
            frame_seq,
            channels,
            fft_buf,
            ifft_buf,
            polar_buf,
            ..
        } = self;
        let (fft_size, half_size, hop_size) = (*fft_size, *half_size, *hop_size);
        let (output_norm, frame_seq_v) = (*output_norm, *frame_seq);
        let mut total_frames = 0usize;

        for (chan, ch) in channels.iter_mut().enumerate() {
            for s in output[chan].iter_mut() {
                *s = 0.0;
            }
            let t0 = ch.total_in; // stream position of this block's first sample
            let block = input[chan].len();
            ch.total_in += block;
            ch.in_fifo.extend(input[chan].iter().copied());

            // Frames within the block get consecutive sequence numbers so
            // L/R frame pairs share one sequence value (the variation
            // transform's time buckets stay stereo-aligned).
            let mut local_frame = 0usize;
            while ch.in_fifo.len() >= fft_size {
                Self::process_frame(
                    &**fft_fwd,
                    &**fft_inv,
                    window,
                    output_norm,
                    fft_size,
                    half_size,
                    hop_size,
                    frame_seq_v + local_frame,
                    chan,
                    ch,
                    fft_buf,
                    ifft_buf,
                    polar_buf,
                    callback,
                );
                local_frame += 1;
            }
            total_frames = local_frame;

            // Constant-delay emission: host slot p - t0 + fft_size carries
            // the reconstruction of stream position p, so the host sees a
            // flat `fft_size`-sample delay no matter how far ahead frames
            // computed within this block.
            let first_p = t0.saturating_sub(fft_size);
            let last_p = (t0 + block).saturating_sub(fft_size);
            let lo = first_p.max(ch.total_out);
            let hi = last_p.min(ch.total_out + ch.out_fifo.len());
            if hi > lo {
                let slot0 = first_p + fft_size - t0;
                for p in lo..hi {
                    output[chan][slot0 + (p - first_p)] = ch.out_fifo[p - ch.total_out];
                }
                ch.out_fifo.drain(..hi - ch.total_out);
                ch.total_out = hi;
            }
        }
        // Advance the global frame sequence once per frame — not per channel
        // (L/R frame pairs share sequence values on purpose, keeping the
        // variation transform's time buckets stereo-aligned).
        *frame_seq = frame_seq.wrapping_add(total_frames);
    }

    /// Transform, spectrally modify, and accumulate one frame; emit the
    /// `hop_size` output samples this frame finalizes.
    #[allow(clippy::too_many_arguments)]
    fn process_frame<F>(
        fft_fwd: &dyn Fft<f32>,
        fft_inv: &dyn Fft<f32>,
        window: &[f32],
        output_norm: f32,
        fft_size: usize,
        half_size: usize,
        hop_size: usize,
        frame_seq: usize,
        chan: usize,
        ch: &mut ChannelState,
        fft_buf: &mut Vec<Complex<f32>>,
        ifft_buf: &mut Vec<Complex<f32>>,
        polar_buf: &mut Vec<Polar>,
        callback: &mut F,
    ) where
        F: FnMut(usize, usize, usize, &mut [Polar]),
    {
        // Copy the oldest fft_size samples in, applying the analysis
        // window. (The FIFO may hold more than one frame's worth when a
        // block arrives larger than the hop size — only the front frame is
        // taken.)
        let (first, second) = ch.in_fifo.as_slices();
        let n1 = first.len().min(fft_size);
        for (i, s) in first.iter().enumerate().take(n1) {
            fft_buf[i] = Complex::new(s * window[i], 0.0);
        }
        for (j, s) in second.iter().enumerate().take(fft_size - n1) {
            let i = n1 + j;
            fft_buf[i] = Complex::new(s * window[i], 0.0);
        }

        fft_fwd.process(fft_buf);

        // One-sided real normalization: DC ×1/N, bins 1..N/2 ×2/N, and
        // Nyquist ×1/N captured aside so it survives the polar round trip.
        let inv_n = 1.0 / fft_size as f32;
        let two_inv_n = 2.0 / fft_size as f32;
        fft_buf[0] *= inv_n;
        for c in fft_buf[1..half_size].iter_mut() {
            c.re *= two_inv_n;
            c.im *= two_inv_n;
        }
        let nyquist = Complex::new(fft_buf[half_size].re * inv_n, 0.0);

        for (i, c) in fft_buf[..half_size].iter().enumerate() {
            polar_buf[i] = Polar::from_complex(*c);
        }
        callback(half_size, chan, frame_seq, polar_buf);

        for (i, p) in polar_buf.iter().enumerate() {
            ifft_buf[i] = p.to_complex();
        }
        ifft_buf[half_size] = nyquist;
        for i in (half_size + 1)..fft_size {
            ifft_buf[i] = Complex::default();
        }
        fft_inv.process(ifft_buf);

        // Synthesis window + COLA normalization into the tail; the oldest
        // hop_size samples are now complete → emit.
        for (j, v) in ifft_buf.iter().enumerate() {
            ch.tail[j] += v.re * window[j] * output_norm;
        }
        for j in 0..hop_size {
            ch.out_fifo.push_back(ch.tail[j]);
        }
        ch.tail.copy_within(hop_size.., 0);
        ch.tail[fft_size - hop_size..].iter_mut().for_each(|s| *s = 0.0);

        ch.in_fifo.drain(..hop_size);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identity_reconstruction_after_latency() {
        let mut engine = SpectralEngine::new(
            SpectralConfig {
                fft_size: 512,
                overlap_count: 4,
                window: WindowType::Hann,
            },
            1,
        );

        // A slowly varying sine exercises DC/Nyquist much less than a
        // constant, but the tolerance is tight all the same.
        let n = 8192;
        let input: Vec<f32> = (0..n)
            .map(|i| 0.25 * (i as f32 * 0.037).sin())
            .collect();
        let mut output = vec![0.0f32; n];
        let input_ref = input.clone();

        engine.process(
            &[&input_ref],
            &mut [&mut output],
            &mut |_bins, _chan, _ov, _polar| {},
        );

        let latency = engine.latency_samples();
        let mut max_err = 0.0f32;
        for i in (latency + 512)..n {
            max_err = max_err.max((output[i] - input[i - latency]).abs());
        }
        assert!(max_err < 1e-3, "max identity error {max_err}");
    }

    #[test]
    fn arbitrary_block_sizes_give_identical_results() {
        let run = |block_size: usize| {
            let mut engine = SpectralEngine::new(
                SpectralConfig {
                    fft_size: 1024,
                    overlap_count: 4,
                    window: WindowType::Hann,
                },
                1,
            );
            let n = 16_384;
            let input: Vec<f32> = (0..n).map(|i| 0.3 * (i as f32 * 0.011).sin()).collect();
            let mut output = vec![0.0f32; n];
            let mut i = 0;
            while i < n {
                let end = (i + block_size).min(n);
                engine.process(
                    &[&input[i..end]],
                    &mut [&mut output[i..end]],
                    &mut |_bins, _chan, _seq, _polar| {},
                );
                i = end;
            }
            output
        };
        let a = run(64);
        let b = run(777);
        let lat = 1024;
        for i in (lat + 512)..n_len() {
            assert!(
                (a[i] - b[i]).abs() < 1e-5,
                "block-size dependence at {i}: {} vs {}",
                a[i],
                b[i]
            );
        }
    }

    fn n_len() -> usize {
        16_384
    }
}
