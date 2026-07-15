//! Acoustic echo cancellation reference signal buffer.
//!
//! The cpal output callback (host's audio, played on the client's
//! speakers) writes F32 PCM samples into this ring. The mic
//! pipeline pulls the same samples back out and feeds them to
//! WebRTC's `process_render_frame` so the AEC can subtract the
//! speaker signal from the mic capture.
//!
//! Capacity is intentionally small (≤ 40 ms) so the ring can't
//! accumulate drift between playback and capture. The cpal
//! callback pushes via `try_lock`-style non-blocking write; if
//! the ring is full it drops the oldest reference sample (the
//! AEC adapts in <100 ms so dropping is harmless).

use crate::ring::SpscRing;

#[derive(Debug, Clone)]
pub struct ReferenceAudioTap {
    ring: std::sync::Arc<SpscRing>,
}

impl ReferenceAudioTap {
    /// Build a new tap. `capacity_samples` is the maximum ring size
    /// in samples; 20 ms × 48 kHz = 960 samples is the ADR-008
    /// recommended default.
    pub fn new(capacity_samples: usize) -> Self {
        Self {
            ring: std::sync::Arc::new(SpscRing::new(capacity_samples)),
        }
    }

    /// Push samples from the cpal output callback. Non-blocking;
    /// drops oldest samples on overflow.
    pub fn push(&self, samples: &[f32]) {
        self.ring.push_slice(samples);
    }

    /// Pop up to `out.len()` samples into `out`. Returns the
    /// number of samples actually written.
    pub fn pop_into(&self, out: &mut [f32]) -> usize {
        self.ring.pop_into(out)
    }

    /// True if no reference samples are buffered.
    pub fn is_empty(&self) -> bool {
        self.ring.is_empty()
    }

    /// Number of samples currently buffered.
    pub fn len(&self) -> usize {
        self.ring.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reference_tap_drops_oldest_on_overflow() {
        let tap = ReferenceAudioTap::new(4);
        tap.push(&[1.0, 2.0, 3.0, 4.0, 5.0, 6.0]);
        let mut out = [0.0_f32; 4];
        let n = tap.pop_into(&mut out);
        assert_eq!(n, 4);
        assert_eq!(out, [3.0, 4.0, 5.0, 6.0]);
    }

    #[test]
    fn reference_tap_is_empty_after_drain() {
        let tap = ReferenceAudioTap::new(8);
        tap.push(&[0.5, 0.6]);
        assert!(!tap.is_empty());
        let mut out = [0.0_f32; 8];
        let _ = tap.pop_into(&mut out);
        assert!(tap.is_empty());
    }
}
