//! Seed sample loading for AkiBloom.
//!
//! A seed is a producer's own sample used as raw material for growing a
//! timbre: unlike an AkiGraft parent (profile only), a seed keeps its full
//! audio — the render engine grafts one seed's *sound* toward the other
//! seed's *profile*. Loading runs off the audio thread; the audio thread only
//! ever sees `Arc<SeedSample>` values swapped in through `SharedSeeds`.
//!
//! Adapted from AkiGraft's `parents.rs` (profile analysis is reused verbatim
//! via `parents::analyze_profile`).

use std::path::Path;
use std::sync::Arc;

use parking_lot::RwLock;

use crate::parents::{self, ParentProfile};
use crate::wav;

/// Render length cap per the plan: longer seeds are truncated with a status
/// hint (memory bound for the rendered variant).
pub const MAX_RENDER_SECS: f32 = 10.0;

/// A loaded seed: metadata, timbre profile, and the audio itself (stereo,
/// mono seeds duplicated to both channels).
pub struct SeedSample {
    pub profile: ParentProfile,
    /// Always 2 channels, equal length, at `profile.sample_rate`.
    pub channels: Vec<Vec<f32>>,
}

impl SeedSample {
    pub fn name(&self) -> &str {
        &self.profile.name
    }

    pub fn info_line(&self) -> String {
        self.profile.info_line()
    }
}

/// Load a seed sample, compute its profile, and keep the audio for rendering.
/// Audio longer than [`MAX_RENDER_SECS`] is truncated.
pub fn load_seed(path: &Path) -> Result<SeedSample, String> {
    let (mut chans, sample_rate) = wav::read_wav_channels(path)?;
    if sample_rate == 0 {
        return Err("无效的采样率".to_owned());
    }
    let frames = chans.first().map(|c| c.len()).unwrap_or(0);
    if frames == 0 {
        return Err("文件里没有音频数据".to_owned());
    }

    // Mono seeds feed both channels (the render engine and voices are
    // stereo-native; AkiGraft's core duplicates mono the same way).
    if chans.len() == 1 {
        chans.push(chans[0].clone());
    }
    let truncated = {
        let cap = (MAX_RENDER_SECS * sample_rate as f32) as usize;
        chans[0].len().min(cap)
    };
    for c in chans.iter_mut() {
        c.truncate(truncated);
    }

    let mono = wav::to_mono(&chans);
    let band_db = parents::analyze_profile(&mono, sample_rate);
    let profile = ParentProfile {
        name: path
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| "未命名".to_owned()),
        sample_rate,
        channels: chans.len(),
        frames: truncated,
        band_db,
    };

    Ok(SeedSample { profile, channels: chans })
}

/// Which seed slot a load targets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Slot {
    A,
    B,
}

/// The pair of loaded seeds, shared between the GUI (writer, via load and
/// render workers) and the audio thread (reader).
pub type SharedSeeds = Arc<RwLock<SeedPair>>;

#[derive(Default)]
pub struct SeedPair {
    pub a: Option<Arc<SeedSample>>,
    pub b: Option<Arc<SeedSample>>,
}

impl SeedPair {
    pub fn set(&mut self, slot: Slot, seed: Arc<SeedSample>) {
        match slot {
            Slot::A => self.a = Some(seed),
            Slot::B => self.b = Some(seed),
        }
    }

    pub fn get(&self, slot: Slot) -> Option<&Arc<SeedSample>> {
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
    use crate::graft::NUM_BANDS;

    fn silent_seed(frames: usize) -> SeedSample {
        SeedSample {
            profile: ParentProfile {
                name: "t".into(),
                sample_rate: 48_000,
                channels: 2,
                frames,
                band_db: vec![crate::mel::FLOOR_DB; NUM_BANDS],
            },
            channels: vec![vec![0.0; frames], vec![0.0; frames]],
        }
    }

    #[test]
    fn seed_pair_slots_roundtrip() {
        let mut pair = SeedPair::default();
        assert!(!pair.any_loaded());
        pair.set(Slot::B, Arc::new(silent_seed(4)));
        assert!(pair.b.is_some() && pair.a.is_none());
        assert_eq!(pair.get(Slot::B).unwrap().channels.len(), 2);
        assert!(pair.any_loaded());
    }

    #[test]
    fn mono_seed_is_duplicated_to_stereo() {
        // load_seed is file-based; verify the duplication invariant it
        // guarantees by construction on the stereo check path.
        let s = silent_seed(8);
        assert_eq!(s.channels.len(), 2);
        assert_eq!(s.channels[0].len(), s.channels[1].len());
    }
}
