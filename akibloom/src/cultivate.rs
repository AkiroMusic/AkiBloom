//! Copied verbatim from the sibling AkiGraft project (shared kernel, one-way
//! copy — see AkiBloom PLAN.md). Do not diverge without updating both.
//! The Cultivate walker (培育): feedback-guided evolution over the
//! variation-transform parameter space.
//!
//! A cultivate "step" proposes a neighboring [`VariateState`] (tag possibly
//! re-rolled weighted by keep history, amount perturbed by the walk size,
//! fresh seed). The producer auditions it and either 保留 (keep: it becomes
//! the new base and biases future tag rolls) or 丢弃 (discard: the engine
//! falls back to the base). Every step lands on the timbre map, and clicking
//! a kept step rolls the base back to it — the paper's snapshot-rollback
//! design, realized over the seed×amount plane.
//!
//! Everything is deterministic given the call sequence: same actions → same
//! history → same map.

use crate::graft::VariateState;

/// Number of variation directions (湿润 / 颗粒 / 呼吸感).
pub const NUM_TAGS: usize = 3;

/// One landed step on the timbre map.
#[derive(Debug, Clone, Copy)]
pub struct CultStep {
    pub state: VariateState,
    /// Whether the producer kept this step.
    pub kept: bool,
    /// Step number (stable id for the map / rollback).
    pub n: usize,
}

/// The walker state. GUI thread owns mutation; the audio thread only ever
/// reads [`Cultivate::active`] through the shared lock.
#[derive(Debug, Clone)]
pub struct Cultivate {
    pub enabled: bool,
    /// The last kept state — what the engine reverts to on discard.
    pub base: CultState,
    /// The proposal currently being auditioned, if any.
    pub candidate: Option<CultState>,
    pub history: Vec<CultStep>,
    /// Per-tag keep counts (normalized weights for future tag rolls).
    pub keep_bias: [f32; NUM_TAGS],
    /// Walk size in "amount" units, set from the UI knob.
    pub walk: f32,
    rng: u64,
    next_n: usize,
}

pub type CultState = VariateState;

/// History cap — old steps fall off the map (kept bases stay usable).
const MAX_HISTORY: usize = 512;

impl Default for Cultivate {
    fn default() -> Self {
        Self {
            enabled: false,
            base: CultState {
                tag: 0,
                amount: 0.15,
                seed: 1,
            },
            candidate: None,
            history: Vec::new(),
            keep_bias: [1.0; NUM_TAGS],
            walk: 0.3,
            rng: 0x853C_49E6_748F_EA9B,
            next_n: 1,
        }
    }
}

impl Cultivate {
    /// Seed the walker RNG from wall-clock entropy (called once from the GUI).
    pub fn reseed_from_clock(&mut self) {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.subsec_nanos() as u64 ^ d.as_secs())
            .unwrap_or(0x1234_5678);
        self.rng ^= nanos.wrapping_mul(0x9E37_79B9_7F4A_7C15);
        self.rng = self.rng.wrapping_add(0x632B_E599_66D0_24A5);
    }

    fn rand_u64(&mut self) -> u64 {
        // xorshift64*.
        let mut x = self.rng;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.rng = x;
        x.wrapping_mul(0x2685_8216_5A10_D6FD)
    }

    fn rand_unit(&mut self) -> f32 {
        (self.rand_u64() >> 40) as f32 / (1u64 << 24) as f32
    }

    /// Propose the next neighboring state and audition it.
    pub fn next_candidate(&mut self) -> CultState {
        // Tag roll: usually keep the current tag, sometimes switch —
        // weighted by which directions got kept so far.
        let cur_tag = self.candidate.map(|c| c.tag).unwrap_or(self.base.tag) as usize;
        let tag = if self.rand_unit() < 0.30 {
            let total: f32 = self.keep_bias.iter().sum();
            let mut r = self.rand_unit() * total;
            let mut picked = cur_tag;
            for (i, w) in self.keep_bias.iter().enumerate() {
                r -= w;
                if r <= 0.0 {
                    picked = i;
                    break;
                }
            }
            picked as u8
        } else {
            cur_tag as u8
        };

        let cur_amount = self.candidate.map(|c| c.amount).unwrap_or(self.base.amount);
        let amount = (cur_amount + self.walk * (self.rand_unit() * 2.0 - 1.0)).clamp(0.05, 1.0);

        let state = CultState {
            tag,
            amount,
            seed: self.rand_u64(),
        };
        self.candidate = Some(state);
        self.push_history(state, false);
        state
    }

    /// 保留: the auditioned proposal becomes the new base and biases the
    /// tag roll toward its direction.
    pub fn keep(&mut self) {
        if let Some(c) = self.candidate.take() {
            self.base = c;
            self.keep_bias[c.tag as usize] += 1.0;
            // Keep the bias scale bounded.
            let max = self.keep_bias.fold_max();
            if max > 32.0 {
                for w in &mut self.keep_bias {
                    *w *= 0.5;
                }
            }
            self.mark_last(true);
        }
    }

    /// 丢弃: fall back to the base (the step stays on the map as discarded).
    pub fn discard(&mut self) {
        self.candidate = None;
        self.mark_last(false);
    }

    /// Roll the base back to step `n` (map click). The step does not need to
    /// have been kept — any landing is a valid base to walk from.
    pub fn rollback(&mut self, n: usize) {
        if let Some(step) = self.history.iter().find(|s| s.n == n) {
            let state = step.state;
            self.base = state;
            self.candidate = None;
            // Reset the walk counter so numbering stays monotonic.
            self.next_n = self.history.last().map(|s| s.n + 1).unwrap_or(1);
            self.push_history(state, true);
        }
    }

    /// What the engine should use right now.
    pub fn active(&self) -> CultState {
        self.candidate.unwrap_or(self.base)
    }

    fn push_history(&mut self, state: VariateState, kept: bool) {
        self.history.push(CultStep {
            state,
            kept,
            n: self.next_n,
        });
        self.next_n += 1;
        if self.history.len() > MAX_HISTORY {
            self.history.remove(0);
        }
    }

    fn mark_last(&mut self, kept: bool) {
        if let Some(last) = self.history.last_mut() {
            last.kept = kept;
        }
    }
}

trait FoldMax {
    fn fold_max(&self) -> f32;
}
impl FoldMax for [f32; NUM_TAGS] {
    fn fold_max(&self) -> f32 {
        self.iter().copied().fold(f32::MIN, f32::max)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn same_action_sequence_is_deterministic() {
        let run = || {
            let mut c = Cultivate::default();
            c.next_candidate();
            c.keep();
            c.next_candidate();
            c.next_candidate();
            (c.active(), c.history.len())
        };
        let a = run();
        let b = Cultivate::default();
        let _ = b;
        // A fresh default walker repeats the exact same sequence.
        let c = run();
        assert_eq!(a.0.seed, c.0.seed);
        assert_eq!(a.1, c.1);
    }

    #[test]
    fn keep_becomes_base_and_discard_reverts() {
        let mut c = Cultivate::default();
        let base0 = c.base;
        let cand = c.next_candidate();
        assert_eq!(c.active(), cand);
        c.keep();
        assert_eq!(c.base, cand);
        assert!(c.candidate.is_none());
        assert_eq!(c.active(), cand);

        let cand2 = c.next_candidate();
        assert_ne!(cand2, base0);
        c.discard();
        assert_eq!(c.active(), cand);
        assert!(c.candidate.is_none());
    }

    #[test]
    fn keep_updates_bias() {
        let mut c = Cultivate::default();
        c.base.tag = 1;
        // Force several keeps in tag 1's neighborhood.
        for _ in 0..8 {
            c.next_candidate();
            c.keep();
        }
        assert!(c.keep_bias[1] > c.keep_bias[0]);
    }

    #[test]
    fn rollback_restores_base() {
        let mut c = Cultivate::default();
        c.next_candidate();
        c.keep(); // step 1 kept
        let kept_state = c.base;
        c.next_candidate(); // step 2
        c.next_candidate(); // step 3, auditioning
        assert_ne!(c.active(), kept_state);

        c.rollback(1);
        assert_eq!(c.base, kept_state);
        assert_eq!(c.active(), kept_state);
        assert!(c.candidate.is_none());
        // Rollback itself lands on the map.
        assert_eq!(c.history.last().unwrap().state, kept_state);
    }

    #[test]
    fn walk_zero_stays_near_base() {
        let mut c = Cultivate::default();
        c.walk = 0.0;
        c.base.amount = 0.5;
        let cand = c.next_candidate();
        assert!((cand.amount - 0.5).abs() < 1e-6);
    }

    #[test]
    fn history_is_capped() {
        let mut c = Cultivate::default();
        for _ in 0..(MAX_HISTORY + 50) {
            c.next_candidate();
            c.discard();
        }
        assert_eq!(c.history.len(), MAX_HISTORY);
    }
}
