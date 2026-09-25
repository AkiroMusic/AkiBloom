//! Polyphonic sample playback for AkiBloom.
//!
//! Eight voices read the rendered [`PlayableVariant`] with linear
//! interpolation, pitch-shifted by `2^((note − root) / 12)` relative to the
//! variant's native sample rate. The variant loops over its whole length
//! (its loop point is pre-crossfaded at render time). Voice stealing takes
//! the oldest voice; released voices fade out over ~10 ms to avoid clicks.

use std::sync::Arc;

use nih_plug::params::smoothing::{Smoother, SmoothingStyle};

use crate::render::PlayableVariant;

/// Hard polyphony cap from the plan (CPU safety valve).
pub const NUM_VOICES: usize = 8;
/// Release fade length in samples (~10 ms @ 48 kHz; scaled by context SR).
pub const RELEASE_SECS: f32 = 0.010;
/// Attack fade length in samples (~1 ms) — kills note-on clicks.
pub const ATTACK_SECS: f32 = 0.001;

struct Voice {
    note: u8,
    velocity: f32,
    /// Playback position in variant samples; fractional (linear interp).
    pos: f64,
    /// Pitch shift `2^((note − root) / 12)`; the context/variant sample-rate
    /// ratio is folded in per block (see `render_block`).
    rate: f64,
    /// Insertion order, for oldest-first stealing.
    age: u64,
    /// Samples since note-on (attack fade).
    held: u32,
    /// Samples left in the release fade; `None` = sustained.
    release: Option<u32>,
}

pub struct Voices {
    voices: [Option<Voice>; NUM_VOICES],
    counter: u64,
    gain: Smoother<f32>,
}

impl Voices {
    pub fn new() -> Self {
        Self {
            voices: std::array::from_fn(|_| None),
            counter: 0,
            gain: Smoother::new(SmoothingStyle::Linear(50.0)),
        }
    }

    pub fn reset(&mut self) {
        for v in self.voices.iter_mut() {
            *v = None;
        }
    }

    /// Pitch shift for a note relative to the root (12-TET plus the fine
    /// tune fraction), pinned by test.
    pub fn pitch_ratio(note: u8, root: f32) -> f64 {
        2.0f64.powf((note as f32 - root) as f64 / 12.0)
    }

    pub fn note_on(&mut self, note: u8, velocity: f32, root: f32) {
        // Re-triggering a held note replaces its voice (no duplicate stacks).
        self.kill_note(note);
        let voice = Voice {
            note,
            velocity: velocity.clamp(0.0, 1.0),
            pos: 0.0,
            rate: Self::pitch_ratio(note, root),
            age: self.counter,
            held: 0,
            release: None,
        };
        self.counter = self.counter.wrapping_add(1);
        match self.voices.iter_mut().find(|v| v.is_none()) {
            Some(slot) => *slot = Some(voice),
            None => {
                // Steal the oldest voice when full.
                let oldest = self
                    .voices
                    .iter_mut()
                    .filter_map(|v| v.as_mut())
                    .min_by_key(|v| v.age)
                    .unwrap();
                *oldest = voice;
            }
        }
    }

    /// Begin the release fade for all voices on `note`.
    pub fn note_off(&mut self, note: u8) {
        for v in self.voices.iter_mut().flatten() {
            if v.note == note && v.release.is_none() {
                v.release = Some(0);
            }
        }
    }

    /// Remove voices on `note` immediately (re-trigger path only).
    fn kill_note(&mut self, note: u8) {
        for v in self.voices.iter_mut() {
            if let Some(voice) = v {
                if voice.note == note {
                    *v = None;
                }
            }
        }
    }

    /// True when no voice is sounding (the caller can skip mixing entirely).
    pub fn idle(&self) -> bool {
        self.voices.iter().all(|v| v.is_none())
    }

    /// Mix all voices into `out` (1 or 2 channels). `out` must arrive zeroed.
    /// `variant_rate_ratio` = context SR / variant SR.
    pub fn render_block(
        &mut self,
        out: &mut [&mut [f32]],
        variant: Option<&Arc<PlayableVariant>>,
        variant_rate_ratio: f64,
        gain: f32,
        sample_rate: f32,
    ) {
        self.gain.set_target(sample_rate, gain);
        let Some(variant) = variant else {
            return;
        };
        let len = variant.len();
        if len < 2 || out.is_empty() {
            return;
        }
        let chans = variant.channels.len().min(2);
        let loop_len = len as f64;
        let attack_step = 1.0 / (ATTACK_SECS * sample_rate).max(1.0);
        let release_len = (RELEASE_SECS * sample_rate) as u32;
        let release_step = 1.0 / release_len.max(1) as f32;

        let n = out[0].len();
        for i in 0..n {
            let g = self.gain.next();
            // Stereo accumulator for this sample.
            let mut acc = [0.0f32; 2];
            for voice in self.voices.iter_mut().flatten() {
                // Envelope: linear attack × linear release. Only the release
                // phase can retire a voice (attack starts at 0 by design).
                let attack = (voice.held as f32 * attack_step).min(1.0);
                voice.held = voice.held.saturating_add(1);
                let release = match voice.release.as_mut() {
                    None => 1.0,
                    Some(done) => {
                        *done = done.saturating_add(1);
                        1.0 - *done as f32 * release_step
                    }
                };
                if release <= 0.0 {
                    voice_dead(voice);
                    continue;
                }
                let env = attack * release;

                // Advance and read (linear interpolation, whole-buffer loop).
                voice.pos = (voice.pos + voice.rate * variant_rate_ratio).rem_euclid(loop_len);
                let idx = voice.pos as usize;
                let frac = (voice.pos - idx as f64) as f32;
                let v = voice.velocity * env;
                acc[0] += read_lerp(&variant.channels[0], idx, frac, len) * v;
                if chans == 2 {
                    acc[1] += read_lerp(&variant.channels[1], idx, frac, len) * v;
                }
            }

            for (c, chan_out) in out.iter_mut().enumerate().take(chans) {
                chan_out[i] += acc[c] * g;
            }
        }

        // Retire finished voices.
        for v in self.voices.iter_mut() {
            if let Some(voice) = v {
                if voice.release == Some(u32::MAX) {
                    *v = None;
                }
            }
        }
    }
}

/// Mark a voice for retirement (checked after the block loop).
fn voice_dead(voice: &mut Voice) {
    voice.release = Some(u32::MAX);
}

#[inline]
fn read_lerp(data: &[f32], idx: usize, frac: f32, len: usize) -> f32 {
    let a = data[idx];
    let b = data[(idx + 1) % len];
    a + (b - a) * frac
}

#[cfg(test)]
mod tests {
    use super::*;

    fn variant(len: usize, level: f32) -> Arc<PlayableVariant> {
        Arc::new(PlayableVariant {
            channels: vec![vec![level; len], vec![level; len]],
            sample_rate: 48_000,
        })
    }

    #[test]
    fn pitch_ratio_is_twelve_tet() {
        assert!((Voices::pitch_ratio(60, 60.0) - 1.0).abs() < 1e-9);
        assert!((Voices::pitch_ratio(72, 60.0) - 2.0).abs() < 1e-9);
        assert!((Voices::pitch_ratio(48, 60.0) - 0.5).abs() < 1e-9);
        // Fine tune: symmetric, and half a semitone is the exact midpoint.
        // +1 semitone up and −1 down are reciprocals.
        assert!((Voices::pitch_ratio(61, 60.0) * Voices::pitch_ratio(60, 61.0) - 1.0).abs() < 1e-12);
        assert!((Voices::pitch_ratio(60, 60.5) - 2.0f64.powf(-0.5 / 12.0)).abs() < 1e-12);
    }

    #[test]
    fn voices_are_stolen_oldest_first() {
        let mut voices = Voices::new();
        for note in 60..60 + NUM_VOICES as u8 + 1 {
            voices.note_on(note, 1.0, 60.0);
        }
        let held: Vec<u8> = voices.voices.iter().flatten().map(|v| v.note).collect();
        assert_eq!(held.len(), NUM_VOICES);
        assert!(!held.contains(&60), "oldest voice must be stolen");
        assert!(held.contains(&(60 + NUM_VOICES as u8)));
    }

    #[test]
    fn retrigger_replaces_same_note() {
        let mut voices = Voices::new();
        voices.note_on(64, 1.0, 60.0);
        voices.note_on(64, 0.5, 60.0);
        let count = voices
            .voices
            .iter()
            .flatten()
            .filter(|v| v.note == 64)
            .count();
        assert_eq!(count, 1);
    }

    #[test]
    fn constant_buffer_is_audible_and_finite() {
        let mut voices = Voices::new();
        let var = variant(4096, 0.25);
        voices.note_on(60, 1.0, 60.0);
        // The attack fade (~1 ms) and the gain smoother's 50 ms ramp are both
        // by design; after ~60 ms of blocks the level must be near full.
        for _ in 0..12 {
            let mut l = vec![0.0f32; 256];
            let mut r = vec![0.0f32; 256];
            voices.render_block(&mut [&mut l, &mut r], Some(&var), 1.0, 1.0, 48_000.0);
            assert!(l.iter().all(|v| v.is_finite()));
            assert!(r.iter().all(|v| v.is_finite()));
        }
        let mut l = vec![0.0f32; 256];
        let mut r = vec![0.0f32; 256];
        voices.render_block(&mut [&mut l, &mut r], Some(&var), 1.0, 1.0, 48_000.0);
        assert!(l.iter().all(|v| *v > 0.05), "settled level must be audible");
        assert!(r.iter().all(|v| *v > 0.05));
    }

    #[test]
    fn note_off_fades_to_silence() {
        let mut voices = Voices::new();
        let var = variant(4096, 0.25);
        voices.note_on(60, 1.0, 60.0);
        let mut l = vec![0.0f32; 256];
        let mut r = vec![0.0f32; 256];
        voices.render_block(&mut [&mut l, &mut r], Some(&var), 1.0, 1.0, 48_000.0);

        voices.note_off(60);
        // Fade through the release window, then the *next* block must be
        // completely silent with the voice retired.
        let blocks = (RELEASE_SECS * 48_000.0 / 256.0).ceil() as usize;
        for _ in 0..blocks {
            let mut l2 = vec![0.0f32; 256];
            let mut r2 = vec![0.0f32; 256];
            voices.render_block(&mut [&mut l2, &mut r2], Some(&var), 1.0, 1.0, 48_000.0);
        }
        let mut l3 = vec![0.0f32; 256];
        let mut r3 = vec![0.0f32; 256];
        voices.render_block(&mut [&mut l3, &mut r3], Some(&var), 1.0, 1.0, 48_000.0);
        assert!(l3.iter().all(|v| v.abs() < 1e-6), "tail must be silent");
        assert!(voices.idle());
    }

    #[test]
    fn no_variant_mixes_silence() {
        let mut voices = Voices::new();
        voices.note_on(60, 1.0, 60.0);
        let mut l = vec![0.0f32; 64];
        let mut r = vec![0.0f32; 64];
        voices.render_block(&mut [&mut l, &mut r], None, 1.0, 1.0, 48_000.0);
        assert!(l.iter().all(|v| *v == 0.0) && r.iter().all(|v| *v == 0.0));
    }
}
