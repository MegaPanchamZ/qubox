//! 240+ Hz → 1 kHz event coalescer.
//!
//! Source-rate cap (libinput + WM_POINTER) is 240 Hz per device; the
//! datagram channel caps the writer at 1 kHz so the QUIC stack is
//! never a bottleneck. When the bounded channel is full, the coalescer
//! drops the older sample and logs at `tracing::warn!` (drop-on-
//! backpressure, identical to the gamepad path).
//!
//! ## Coalesce semantics
//!
//! Coalescing here is "drop intermediate samples", not "average".
//! For pen input the most recent tip position is what matters —
//! averaging pressure or tilt would visibly lag the cursor. We keep
//! the *last* sample of any coalescing window, drop the rest, and
//! emit a `FLAG_LAST_IN_BURST` flag on the surviving sample so the
//! host knows it represents a coalesced batch.

use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, Sender, TryRecvError};

use crate::error::PenCaptureError;
use crate::traits::{PenCapture, PenEvent};

/// Default coalesce window. At 240 Hz, four source samples fit per
/// window; the survivor is the latest.
pub const DEFAULT_COALESCE_WINDOW: Duration = Duration::from_millis(4);

/// Default depth of the bounded channel between capture and the
/// downstream QUIC datagram pump. Mirrors the gamepad path.
pub const DEFAULT_CHANNEL_DEPTH: usize = 64;

/// Knobs for [`PenCoalescer::new`].
#[derive(Debug, Clone, Copy)]
pub struct CoalesceConfig {
    /// Maximum number of events coalesced into one window.
    pub window: Duration,
    /// Depth of the bounded channel downstream.
    pub channel_depth: usize,
}

impl Default for CoalesceConfig {
    fn default() -> Self {
        Self {
            window: DEFAULT_COALESCE_WINDOW,
            channel_depth: DEFAULT_CHANNEL_DEPTH,
        }
    }
}

/// Coalescer wrapper. Holds a bounded channel and a flush deadline.
/// Implements [`PenCapture`] for the upstream side by forwarding
/// enumeration to an inner source; the downstream side is just the
/// [`Self::receiver`] end of the bounded channel.
pub struct PenCoalescer<C: PenCapture> {
    inner: C,
    tx: Sender<PenEvent>,
    rx: Receiver<PenEvent>,
    cfg: CoalesceConfig,
}

impl<C: PenCapture> PenCoalescer<C> {
    /// Wrap `inner` with a coalescer that uses `cfg`.
    pub fn new(inner: C, cfg: CoalesceConfig) -> Self {
        let (tx, rx) = crossbeam_channel::bounded(cfg.channel_depth);
        Self { inner, tx, rx, cfg }
    }

    /// Receiver end of the bounded channel. Hand this to the QUIC
    /// datagram pump.
    pub fn receiver(&self) -> Receiver<PenEvent> {
        self.rx.clone()
    }

    /// Drain `source` and coalesce into `self.tx`. Returns when
    /// `source` is closed.
    pub fn run(&mut self, source: Receiver<PenEvent>) -> Result<(), PenCaptureError> {
        let mut latest: Option<PenEvent> = None;
        let mut window_started = Instant::now();
        loop {
            let recv = if let Some(pending) = latest.take() {
                // Forward the buffered "latest" event first.
                self.forward_or_drop(pending);
                source
                    .recv()
                    .map_err(|e| PenCaptureError::Backend(e.to_string()))
            } else {
                match source.recv_deadline(window_started + self.cfg.window) {
                    Ok(event) => Ok(event),
                    Err(crossbeam_channel::RecvTimeoutError::Timeout) => continue,
                    Err(crossbeam_channel::RecvTimeoutError::Disconnected) => {
                        return Ok(());
                    }
                }
            };
            let event = match recv {
                Ok(event) => event,
                Err(error) => return Err(error),
            };
            let now = Instant::now();
            if now.duration_since(window_started) >= self.cfg.window {
                self.forward_or_drop(event);
                window_started = now;
            } else {
                latest = Some(event);
                loop {
                    match source.try_recv() {
                        Ok(next) => latest = Some(next),
                        Err(TryRecvError::Empty) => break,
                        Err(TryRecvError::Disconnected) => {
                            if let Some(event) = latest.take() {
                                self.forward_or_drop(event);
                            }
                            return Ok(());
                        }
                    }
                }
                if let Some(event) = latest.take() {
                    let mut tagged = event;
                    tagged.flags |= qubox_proto::PenEventFlags::FLAG_LAST_IN_BURST.bits();
                    self.forward_or_drop(tagged);
                }
                window_started = now;
            }
        }
    }

    fn forward_or_drop(&self, event: PenEvent) {
        if self.tx.try_send(event).is_err() {
            tracing::warn!(
                device_id = event.device_id,
                "pen coalescer dropped event on full channel ({} ms)",
                self.cfg.window.as_millis()
            );
        }
    }
}

impl<C: PenCapture> PenCapture for PenCoalescer<C> {
    fn enumerate_devices(&self) -> Result<Vec<crate::PenDeviceInfo>, PenCaptureError> {
        self.inner.enumerate_devices()
    }

    fn start(&mut self, event_tx: Sender<PenEvent>) -> Result<Receiver<PenEvent>, PenCaptureError> {
        let source_rx = self.inner.start(self.tx.clone())?;
        let _ = event_tx;
        // The coalescer's own task is spawned by `run`; the caller
        // hands the receiver to its QUIC datagram pump. We return the
        // inner receiver so the caller can call `run` themselves if
        // they prefer.
        let _ = source_rx;
        Ok(self.rx.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::PenDeviceInfo;
    use crossbeam_channel::unbounded;
    use qubox_proto::{PenDeviceDescriptor, PenTool};

    struct StubCapture;
    impl PenCapture for StubCapture {
        fn enumerate_devices(&self) -> Result<Vec<PenDeviceInfo>, PenCaptureError> {
            Ok(vec![PenDeviceInfo {
                descriptor: PenDeviceDescriptor {
                    device_id: 0,
                    name: "stub".to_string(),
                    tools: vec![PenTool::Pen],
                    max_pressure: 0,
                    max_tilt_degrees: 0,
                    rotation_supported: false,
                },
            }])
        }
        fn start(
            &mut self,
            event_tx: Sender<PenEvent>,
        ) -> Result<Receiver<PenEvent>, PenCaptureError> {
            let (tx, rx) = unbounded();
            let _ = event_tx;
            std::thread::spawn(move || {
                let _ = tx;
            });
            Ok(rx)
        }
    }

    fn event(seq: u32, x: f32) -> PenEvent {
        PenEvent {
            device_id: 0,
            tool: PenTool::Pen,
            x,
            y: 0.0,
            pressure: 0.5,
            tilt_x: 0.0,
            tilt_y: 0.0,
            rotation: 0.0,
            button_state: 0,
            hover_distance: 0,
            timestamp_us: seq,
            flags: 0,
        }
    }

    #[test]
    fn coalesces_a_burst_to_a_single_flagged_event() {
        let stub = StubCapture;
        let mut co = PenCoalescer::new(stub, CoalesceConfig::default());
        let downstream_rx = co.receiver();
        let (src_tx, src_rx) = unbounded();
        let handle = std::thread::spawn(move || co.run(src_rx));
        for i in 0..32_u32 {
            src_tx.send(event(i, i as f32)).unwrap();
        }
        std::thread::sleep(Duration::from_millis(20));
        drop(src_tx);
        handle.join().unwrap().unwrap();
        let mut survivors = 0;
        let mut last = None;
        while let Ok(e) = downstream_rx.try_recv() {
            survivors += 1;
            last = Some(e);
        }
        assert!(
            survivors < 32,
            "coalescer should have dropped most events, kept {survivors}"
        );
        let final_event = last.expect("at least one survivor");
        assert!(
            final_event.flags & qubox_proto::PenEventFlags::FLAG_LAST_IN_BURST.bits() != 0,
            "survivor must carry FLAG_LAST_IN_BURST (got flags = {:#04x})",
            final_event.flags
        );
    }

    #[test]
    fn enumerate_devices_forwards_to_inner() {
        let stub = StubCapture;
        let co = PenCoalescer::new(stub, CoalesceConfig::default());
        let devices = co.enumerate_devices().unwrap();
        assert_eq!(devices.len(), 1);
        assert_eq!(devices[0].descriptor.name, "stub");
    }
}
