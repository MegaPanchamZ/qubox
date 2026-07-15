//! The sender-side frame-aware pacer. ADR-013 §2.

use std::time::{Duration, Instant};

use qubox_proto::FramePacingSchedule;
use tracing::{debug, instrument, trace, warn};

use super::schedule::{bytes_in_slot, deadline_for, slots_per_frame, SendBase};

/// Configuration knobs the host can override at construction.
#[derive(Debug, Clone, Copy)]
pub struct FrameAwarePacerConfig {
    /// If the pacer is more than this far behind `Instant::now()`, drop
    /// the frame (advisory; the caller decides). Default 50 ms.
    pub drop_threshold: Duration,
    /// Maximum number of slot emissions to coalesce per call to
    /// `next_send_window`. Default 1.
    pub max_emissions_per_call: u32,
    /// Switch to "burst-shrink" mode if observed jitter exceeds this.
    /// Default 5 ms (RFC 8289 jitter budget headroom).
    pub jitter_shrink_threshold: Duration,
}

impl Default for FrameAwarePacerConfig {
    fn default() -> Self {
        Self {
            drop_threshold: Duration::from_millis(50),
            max_emissions_per_call: 1,
            jitter_shrink_threshold: Duration::from_millis(5),
        }
    }
}

/// What the caller (encoder loop in `host-agent/src/main.rs:1009`)
/// should do with the next chunk of bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PacingDecision {
    /// Wait until `wake_at`, then emit `bytes_for_slot` bytes.
    WaitThenSend { wake_at: Instant, bytes_for_slot: u32 },
    /// Frame is too far behind — drop and let the codec re-encode.
    Drop,
    /// Frame's byte budget is fully consumed; move to the next frame.
    FrameComplete,
}

/// The sender-side frame-aware pacer. One per media stream.
pub struct FrameAwarePacer {
    schedule: FramePacingSchedule,
    send_base: SendBase,
    config: FrameAwarePacerConfig,

    current_frame: u32,
    next_slot: u32,
    total_slots: u32,

    last_tick: Option<Instant>,
    jitter_ewma_us: f64,

    pub stats: FramePacerStats,
}

#[derive(Debug, Default, Clone, Copy)]
pub struct FramePacerStats {
    pub frames_sent: u64,
    pub frames_dropped: u64,
    pub slots_emitted: u64,
    pub bytes_emitted: u64,
    pub wake_late_count: u64,
    pub wake_late_total_us: u64,
}

impl FrameAwarePacer {
    /// Build a pacer with the default config.
    pub fn new(schedule: FramePacingSchedule, send_base: SendBase) -> Self {
        Self::with_config(schedule, send_base, FrameAwarePacerConfig::default())
    }

    /// Build a pacer with an explicit config.
    pub fn with_config(
        schedule: FramePacingSchedule,
        send_base: SendBase,
        config: FrameAwarePacerConfig,
    ) -> Self {
        let total_slots = slots_per_frame(&schedule);
        Self {
            schedule,
            send_base,
            config,
            current_frame: send_base.frame_index,
            next_slot: 0,
            total_slots,
            last_tick: None,
            jitter_ewma_us: 0.0,
            stats: FramePacerStats::default(),
        }
    }

    /// Begin pacing the next frame. Returns the deadline the encoder
    /// should aim for.
    #[instrument(skip(self), fields(frame = n))]
    pub fn begin_frame(&mut self, n: u32, now: Instant) -> Instant {
        if n != self.current_frame && n != self.current_frame + 1 {
            warn!(
                "FrameAwarePacer::begin_frame called with non-sequential \
                 frame {n} (current {})", self.current_frame
            );
        }
        self.current_frame = n;
        self.next_slot = 0;
        self.last_tick = Some(now);
        match deadline_for(n, self.send_base, &self.schedule) {
            Some(us) => Instant::now() + Duration::from_micros(us),
            None => {
                warn!("begin_frame: deadline_for overflowed; using now+interval");
                Instant::now() + Duration::from_micros(u64::from(self.schedule.target_interval_us))
            }
        }
    }

    /// Tell the pacer the actual byte count handed to the transport.
    pub fn end_frame(&mut self, byte_count: u32, now: Instant) {
        let bytes_in_frame = u32::min(byte_count, self.schedule.bytes_per_frame);
        let slots = (u64::from(bytes_in_frame)
                     .div_ceil(u64::from(self.schedule.max_burst_bytes.max(1))))
                    as u32;
        self.total_slots = slots;
        if let Some(prev) = self.last_tick {
            let gap_us = now.saturating_duration_since(prev).as_micros() as f64;
            let expected_us = f64::from(self.schedule.target_interval_us);
            let jitter_us = (gap_us - expected_us).abs();
            self.jitter_ewma_us = 0.9 * self.jitter_ewma_us + 0.1 * jitter_us;
            if self.jitter_ewma_us > self.config.jitter_shrink_threshold.as_micros() as f64 {
                self.schedule.max_burst_bytes =
                    (self.schedule.max_burst_bytes / 2).max(1_200);
                debug!("shrinking max_burst_bytes to {}", self.schedule.max_burst_bytes);
            }
        }
    }

    /// Decide what to do next for the current frame.
    pub fn next_send_window(&mut self, now: Instant) -> PacingDecision {
        if self.next_slot >= self.total_slots {
            return PacingDecision::FrameComplete;
        }
        let k = self.next_slot;
        let bytes = bytes_in_slot(&self.schedule, k);
        if bytes == 0 {
            self.next_slot += 1;
            return self.next_send_window(now);
        }
        let slot_offset_us = u64::from(k) * u64::from(self.schedule.max_burst_bytes)
                             * u64::from(self.schedule.target_interval_us)
                             / u64::from(self.schedule.bytes_per_frame.max(1));
        let wake_at = match deadline_for(self.current_frame, self.send_base, &self.schedule) {
            Some(us) => now + Duration::from_micros(slot_offset_us.min(us)),
            None => now,
        };
        if now >= wake_at + self.config.drop_threshold {
            self.stats.frames_dropped += 1;
            warn!(
                "dropping frame {} — pacer {} µs behind drop_threshold",
                self.current_frame,
                now.saturating_duration_since(wake_at).as_micros()
            );
            self.next_slot = self.total_slots;
            return PacingDecision::Drop;
        }
        if now > wake_at {
            self.stats.wake_late_count += 1;
            self.stats.wake_late_total_us +=
                now.saturating_duration_since(wake_at).as_micros() as u64;
            trace!("late wake for frame {} slot {} by {} µs",
                   self.current_frame, k,
                   now.saturating_duration_since(wake_at).as_micros());
        }
        self.next_slot += 1;
        self.stats.slots_emitted += 1;
        self.stats.bytes_emitted += u64::from(bytes);
        if self.next_slot == self.total_slots {
            self.stats.frames_sent += 1;
        }
        PacingDecision::WaitThenSend { wake_at, bytes_for_slot: bytes }
    }

    /// Replace the schedule. Use when ADR-012 emits a new bandwidth
    /// estimate. The new schedule takes effect at `s.effective_frame_index`.
    pub fn reschedule(&mut self, s: FramePacingSchedule) {
        debug!(
            "FrameAwarePacer reschedule: id {} → {}, bytes_per_frame {} → {}",
            self.schedule.schedule_id, s.schedule_id,
            self.schedule.bytes_per_frame, s.bytes_per_frame,
        );
        self.schedule = s;
        self.total_slots = slots_per_frame(&s);
    }

    /// Feed an ACK-derived bandwidth estimate into the pacer.
    pub fn on_bandwidth_estimate(&mut self, bps: u64, framerate_hz: u32) {
        if framerate_hz == 0 { return; }
        let new_bytes_per_frame = u32::try_from(bps.div_ceil(8).div_ceil(u64::from(framerate_hz)))
            .unwrap_or(u32::MAX);
        if new_bytes_per_frame != self.schedule.bytes_per_frame {
            debug!(
                "on_bandwidth_estimate: bps={bps} fr={framerate_hz} \
                 → bytes_per_frame {} (was {})",
                new_bytes_per_frame, self.schedule.bytes_per_frame
            );
            self.schedule.bytes_per_frame = new_bytes_per_frame;
            self.total_slots = slots_per_frame(&self.schedule);
        }
    }

    pub fn schedule(&self) -> &FramePacingSchedule { &self.schedule }
    pub fn current_frame(&self) -> u32 { self.current_frame }
    pub fn next_slot(&self) -> u32 { self.next_slot }
    pub fn total_slots(&self) -> u32 { self.total_slots }
    pub fn jitter_ewma_us(&self) -> f64 { self.jitter_ewma_us }
}
