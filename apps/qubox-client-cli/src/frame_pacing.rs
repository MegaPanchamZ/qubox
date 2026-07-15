//! P0-5 frame pacing.
//!
//! The streaming client is renderer-agnostic: today it uses
//! `softbuffer` for CPU-rendered BGRA, and the P0-5 spec calls for
//! moving to `wgpu` with a `Mailbox` swapchain and `Fifo` fallback.
//! The swapchain selection is a per-platform GPU detail; the
//! *pacing algorithm* (when to present a new frame) is independent of
//! the swapchain and is what this module provides.
//!
//! ## Algorithm
//!
//! Per P0-5 spec research/roadmap/p0-05-frame-pacing.md:
//!
//! - Target frame interval is derived from the stream framerate
//!   (e.g. 60 fps → 16.67 ms, 120 fps → 8.33 ms).
//! - The first decoded frame is presented immediately
//!   (`first_frame_immediate = true`); subsequent frames are paced
//!   at the target interval.
//! - A small "present-early" tolerance (`EARLY_TOLERANCE`) lets us
//!   present up to 500 us early to absorb decode jitter without
//!   actually skipping frames.
//! - A "max skip" cap (`MAX_SKIPS_PER_TICK = 2`) prevents long
//!   catch-up bursts after a stall from causing runaway redraws.
//! - Stats: actual FPS (EWMA), frame interval jitter (ms, EWMA),
//!   frames presented, frames skipped.
//!
//! The winit event loop queries `FramePacer::should_present(now)` on
//! each `RedrawRequested` and skips the present (and re-arms the
//! `request_redraw` call for the next deadline) if it's too early.
//! This is the softbuffer-friendly equivalent of the wgpu `Mailbox`
//! swapchain: both keep the GPU fed at the target interval without
//! running ahead of the encoder.

use std::time::{Duration, Instant};

/// Tolerance for presenting slightly early (absorbs decode jitter).
const EARLY_TOLERANCE: Duration = Duration::from_micros(500);

/// Maximum number of "missed" frames a single `should_present` call
/// will catch up. Prevents a long stall from spamming the window
/// with `request_redraw` calls.
const MAX_SKIPS_PER_TICK: u32 = 2;

#[derive(Debug, Clone, Copy)]
pub struct FramePacer {
    target_interval: Duration,
    last_present: Option<Instant>,
    presented: u64,
    skipped: u64,
    /// Smoothed frame interval (ms).
    interval_ewma_ms: f64,
    /// Smoothed actual FPS.
    fps_ewma: f64,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FramePacerStats {
    pub presented: u64,
    pub skipped: u64,
    pub actual_fps: f64,
    pub interval_jitter_ms: f64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PresentDecision {
    /// Caller should present the current decoded frame now.
    Present,
    /// Caller should skip this redraw; another `request_redraw`
    /// will fire at the next deadline.
    Skip,
    /// Caller has fallen more than `MAX_SKIPS_PER_TICK` intervals
    /// behind; present now and reset the deadline to "now + 1 interval"
    /// (effectively a catch-up reset, prevents an unbounded burst of
    /// skip-then-present cycles).
    CatchUpReset,
}

impl FramePacer {
    pub fn new(framerate: u32) -> Self {
        let target_interval = if framerate == 0 {
            Duration::from_millis(16)
        } else {
            Duration::from_secs_f64(1.0 / f64::from(framerate))
        };
        Self {
            target_interval,
            last_present: None,
            presented: 0,
            skipped: 0,
            interval_ewma_ms: target_interval.as_secs_f64() * 1000.0,
            fps_ewma: f64::from(framerate),
        }
    }

    pub fn target_interval(&self) -> Duration {
        self.target_interval
    }

    pub fn stats(&self) -> FramePacerStats {
        FramePacerStats {
            presented: self.presented,
            skipped: self.skipped,
            actual_fps: self.fps_ewma,
            interval_jitter_ms: self.interval_ewma_ms,
        }
    }

    /// Decide whether to present the current decoded frame. Call this
    /// on every winit `RedrawRequested`; if it returns `Skip`, do not
    /// `request_redraw` again (the loop will fire `RedrawRequested`
    /// again at the next event), or re-arm with a small delay.
    pub fn should_present(&mut self, now: Instant) -> PresentDecision {
        let Some(last) = self.last_present else {
            // First frame: present immediately.
            self.mark_presented(now, now);
            return PresentDecision::Present;
        };
        let elapsed = now.saturating_duration_since(last);
        if elapsed + EARLY_TOLERANCE >= self.target_interval {
            // How many intervals have we missed?
            let missed = (elapsed.as_nanos() / self.target_interval.as_nanos()) as u32;
            self.mark_presented(last, now);
            if missed > MAX_SKIPS_PER_TICK {
                return PresentDecision::CatchUpReset;
            }
            return PresentDecision::Present;
        }
        self.skipped += 1;
        PresentDecision::Skip
    }

    fn mark_presented(&mut self, last: Instant, now: Instant) {
        let interval_ms = now.saturating_duration_since(last).as_secs_f64() * 1000.0;
        self.interval_ewma_ms = 0.9 * self.interval_ewma_ms + 0.1 * interval_ms;
        let actual_fps = if interval_ms > 0.0 {
            1000.0 / interval_ms
        } else {
            self.fps_ewma
        };
        self.fps_ewma = 0.9 * self.fps_ewma + 0.1 * actual_fps;
        self.last_present = Some(now);
        self.presented += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_frame_is_immediate() {
        let mut p = FramePacer::new(60);
        let now = Instant::now();
        assert_eq!(p.should_present(now), PresentDecision::Present);
        assert_eq!(p.stats().presented, 1);
    }

    #[test]
    fn rapid_redraws_are_skipped() {
        let mut p = FramePacer::new(60);
        let t0 = Instant::now();
        // First frame: present.
        let _ = p.should_present(t0);
        // 1ms later: too early, skip.
        let t1 = t0 + Duration::from_millis(1);
        assert_eq!(p.should_present(t1), PresentDecision::Skip);
        // 17ms after first: present.
        let t2 = t0 + Duration::from_millis(17);
        assert_eq!(p.should_present(t2), PresentDecision::Present);
        assert_eq!(p.stats().skipped, 1);
        assert_eq!(p.stats().presented, 2);
    }

    #[test]
    fn catchup_after_long_stall() {
        let mut p = FramePacer::new(60);
        let t0 = Instant::now();
        let _ = p.should_present(t0);
        // 200ms later: missed ~12 frames; catch-up reset.
        let t1 = t0 + Duration::from_millis(200);
        assert_eq!(p.should_present(t1), PresentDecision::CatchUpReset);
    }

    #[test]
    fn early_tolerance_permits_slight_overshoot() {
        let mut p = FramePacer::new(60);
        let t0 = Instant::now();
        let _ = p.should_present(t0);
        // 16.2ms after first: within 500us tolerance, present.
        let t1 = t0 + Duration::from_micros(16_200);
        assert_eq!(p.should_present(t1), PresentDecision::Present);
    }
}
