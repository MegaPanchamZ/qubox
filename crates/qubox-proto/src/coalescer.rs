//! Shared input event coalescer — 1 ms window, 16-event max.
//!
//! Extracted from `qubox-pen`'s `PenCoalescer` per ADR-019 §5. Used by
//! mouse, pen, and gamepad event sources. The core state machine
//! handles deadline-based flush and burst-terminator flush.

use std::time::{Duration, Instant};

/// Default coalesce window: 1 ms = 1000 µs.
pub const DEFAULT_COALESCE_WINDOW: Duration = Duration::from_micros(1000);

/// Max events per batch. At 1 ms this caps at 16 events / packet.
pub const COALESCE_MAX_EVENTS: usize = 16;

/// Why the coalescer decided to flush. Stable ordering for telemetry.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum FlushReason {
    /// Coalesce window elapsed since first event.
    Deadline,
    /// Input queue drained (no more data available).
    QueueEmpty,
    /// Caller passed `FLAG_LAST_IN_BURST` (pen-down terminator, etc.).
    BurstTerminator,
}

/// Generic coalescer over `Copy` events.
///
/// Designed to be shared across mouse motion, pen motion, and gamepad
/// axis event sources. Each source creates its own `InputCoalescer`
/// with the appropriate `window` and `max_events`.
#[derive(Debug)]
pub struct InputCoalescer<E: Copy> {
    pending: Vec<(E, Instant)>,
    last_event_time: Instant,
    window: Duration,
    max_events: usize,
}

impl<E: Copy> InputCoalescer<E> {
    /// Create a new coalescer with the given window and capacity.
    pub fn new(window: Duration, max_events: usize) -> Self {
        Self {
            pending: Vec::with_capacity(max_events),
            last_event_time: Instant::now(),
            window,
            max_events,
        }
    }

    /// Push an event into the coalescer buffer.
    ///
    /// When the buffer is full the *last* slot is overwritten (newest
    /// supersedes oldest), which is the correct behaviour for motion
    /// streams where the latest sample is the most valuable.
    pub fn push(&mut self, event: E) {
        let now = Instant::now();
        if self.pending.is_empty() {
            self.last_event_time = now;
        }
        if self.pending.len() < self.max_events {
            self.pending.push((event, now));
        } else {
            *self.pending.last_mut().unwrap() = (event, now);
        }
    }

    /// Check whether the coalescer should flush.
    ///
    /// Returns `Some(reason)` if either:
    /// - `has_burst_flag` is true (immediate flush), or
    /// - the coalesce window has elapsed since the first buffered event.
    pub fn should_flush(&self, has_burst_flag: bool) -> Option<FlushReason> {
        if has_burst_flag {
            return Some(FlushReason::BurstTerminator);
        }
        if self.pending.is_empty() {
            return None;
        }
        let now = Instant::now();
        if now.duration_since(self.last_event_time) >= self.window {
            return Some(FlushReason::Deadline);
        }
        None
    }

    /// Drain buffered events into a `Vec`. Clears the internal buffer.
    ///
    /// Callers are responsible for encoding the events (e.g. via rkyv
    /// when ADR-015 lands) and shipping them on the QUIC stream or
    /// datagram.
    pub fn flush(&mut self) -> Vec<E> {
        self.pending.drain(..).map(|(e, _)| e).collect()
    }

    /// Number of events currently buffered.
    pub fn len(&self) -> usize {
        self.pending.len()
    }

    /// Returns `true` when there are no buffered events.
    pub fn is_empty(&self) -> bool {
        self.pending.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Pushing 3 events then flushing with burst flag returns exactly
    /// those 3 events.
    #[test]
    fn input_coalescer_flushes_on_flag_last_in_burst() {
        let mut c = InputCoalescer::new(DEFAULT_COALESCE_WINDOW, COALESCE_MAX_EVENTS);
        c.push(1);
        c.push(2);
        c.push(3);
        assert_eq!(c.should_flush(true), Some(FlushReason::BurstTerminator));
        let batch = c.flush();
        assert_eq!(batch, vec![1, 2, 3]);
        assert!(c.is_empty());
    }

    /// Pushing 2 events and waiting past the deadline triggers a flush.
    #[test]
    fn input_coalescer_flushes_on_deadline() {
        let mut c = InputCoalescer::new(Duration::from_micros(500), 16);
        c.push(10);
        c.push(20);
        // Small window — should be past deadline immediately
        // (last_event_time was set on first push).
        std::thread::sleep(Duration::from_millis(1));
        assert_eq!(
            c.should_flush(false),
            Some(FlushReason::Deadline)
        );
        let batch = c.flush();
        assert_eq!(batch, vec![10, 20]);
    }

    /// Pushing events does not trigger a flush before the window elapses.
    #[test]
    fn input_coalescer_does_not_flush_below_deadline() {
        let mut c = InputCoalescer::new(Duration::from_millis(100), 16);
        c.push(100);
        c.push(200);
        assert_eq!(c.should_flush(false), None);
        let batch = c.flush();
        assert_eq!(batch, vec![100, 200]);
    }

    /// Max events cap: pushing more than 16 overwrites the last slot.
    #[test]
    fn input_coalescer_respects_max_events() {
        let mut c = InputCoalescer::new(DEFAULT_COALESCE_WINDOW, 3);
        c.push(10);
        c.push(20);
        c.push(30);
        c.push(40); // overwrites slot 2
        assert_eq!(c.len(), 3);
        let batch = c.flush();
        // 10, 20, 40 (40 replaced 30)
        assert_eq!(batch, vec![10, 20, 40]);
    }

    /// Empty coalescer returns None from should_flush.
    #[test]
    fn input_coalescer_empty_does_not_flush() {
        let c: InputCoalescer<u32> = InputCoalescer::new(DEFAULT_COALESCE_WINDOW, 16);
        assert_eq!(c.should_flush(false), None);
        assert_eq!(c.should_flush(true), Some(FlushReason::BurstTerminator));
    }

    /// After flush the coalescer is empty.
    #[test]
    fn input_coalescer_flush_drains_all() {
        let mut c = InputCoalescer::new(DEFAULT_COALESCE_WINDOW, 16);
        c.push(1);
        c.push(2);
        assert!(!c.is_empty());
        let _ = c.flush();
        assert!(c.is_empty());
        assert_eq!(c.len(), 0);
    }
}
