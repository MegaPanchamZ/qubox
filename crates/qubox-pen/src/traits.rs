//! Pen / tablet capture and injection trait surface.
//!
//! The platform implementations live behind `cfg(target_os = "...")`
//! modules. All callers go through these traits so the host-agent
//! can stay platform-agnostic.

use crossbeam_channel::{Receiver, Sender};
use qubox_proto::{PenDeviceDescriptor, PenTool};

use crate::error::{PenCaptureError, PenInjectError};

/// One captured pen event, in platform-independent form. The capture
/// side is responsible for filling every field; the injection side
/// only needs to consume the same struct. The struct is `Copy` so
/// pipelines that fan out events to multiple sinks don't need to
/// re-allocate.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PenEvent {
    /// Index into the device list advertised at session start.
    pub device_id: u16,
    /// Logical pen tool.
    pub tool: PenTool,
    /// Screen-space x coordinate in pixels.
    pub x: f32,
    /// Screen-space y coordinate in pixels.
    pub y: f32,
    /// 0..=1; 0 when out of range or no pressure is reported.
    pub pressure: f32,
    /// -90..=90 degrees, x tilt.
    pub tilt_x: f32,
    /// -90..=90 degrees, y tilt.
    pub tilt_y: f32,
    /// 0..=360 degrees, rotation.
    pub rotation: f32,
    /// Tool-specific button bitmask (Eraser tip, barrel buttons).
    pub button_state: u32,
    /// Hover distance in millimeters; 0 = contact; u16::MAX = out of range.
    pub hover_distance: u16,
    /// Microsecond timestamp from the source clock. Wraps every ~71
    /// minutes when fed through `WirePenEvent`.
    pub timestamp_us: u32,
    /// Raw flags bitmask (see `PenEventFlags`).
    pub flags: u8,
}

impl PenEvent {
    /// Encode this event into a 36-byte `WirePenEvent` ready to be
    /// pushed onto the QUIC datagram channel.
    pub fn to_wire(&self) -> qubox_proto::WirePenEvent {
        let mut flags = qubox_proto::PenEventFlags::empty();
        if self.flags & qubox_proto::PenEventFlags::FLAG_LAST_IN_BURST.bits() != 0 {
            flags |= qubox_proto::PenEventFlags::FLAG_LAST_IN_BURST;
        }
        if self.flags & qubox_proto::PenEventFlags::FLAG_HAS_HOVER.bits() != 0 {
            flags |= qubox_proto::PenEventFlags::FLAG_HAS_HOVER;
        }
        if self.flags & qubox_proto::PenEventFlags::FLAG_BARREL.bits() != 0 {
            flags |= qubox_proto::PenEventFlags::FLAG_BARREL;
        }
        if self.flags & qubox_proto::PenEventFlags::FLAG_ERASER_TIP.bits() != 0 {
            flags |= qubox_proto::PenEventFlags::FLAG_ERASER_TIP;
        }
        let hover_q = if self.hover_distance == u16::MAX {
            0x0F
        } else {
            (self.hover_distance.min(15)) as u8
        };
        qubox_proto::WirePenEvent::build(
            self.device_id,
            self.tool,
            flags,
            hover_q,
            self.x,
            self.y,
            self.pressure,
            self.tilt_x,
            self.tilt_y,
            self.rotation,
            self.timestamp_us,
        )
    }
}

/// Information about a connected pen / tablet device.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PenDeviceInfo {
    pub descriptor: PenDeviceDescriptor,
}

/// Capture trait. Implementations live in `linux` / `windows` /
/// `stub` modules and are selected by `platform::current_capture`.
pub trait PenCapture: Send + 'static {
    /// Enumerate connected pen / tablet devices.
    fn enumerate_devices(&self) -> Result<Vec<PenDeviceInfo>, PenCaptureError>;
    /// Begin streaming pen events to `event_tx`. The returned
    /// `Receiver` is a copy of `event_tx`'s paired end; callers can
    /// either hold it or close it to stop the stream.
    ///
    /// Implementations spawn a background thread / task and return
    /// immediately. The thread terminates when the receiver is
    /// dropped.
    fn start(&mut self, event_tx: Sender<PenEvent>) -> Result<Receiver<PenEvent>, PenCaptureError>;
}

/// Injection trait. Implementations create the platform virtual device
/// in `new` and tear it down in `Drop`.
pub trait PenInjector: Send + 'static {
    /// Inject a single captured event into the host's input subsystem.
    fn inject(&mut self, event: &PenEvent) -> Result<(), PenInjectError>;
    /// Inject a batch of events in one syscall where possible. The
    /// default implementation forwards to [`Self::inject`] per event.
    fn inject_batch(&mut self, events: &[PenEvent]) -> Result<(), PenInjectError> {
        for event in events {
            self.inject(event)?;
        }
        Ok(())
    }
    /// Name of the virtual device as it appears to other applications.
    fn device_name(&self) -> &str;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pen_event_to_wire_round_trips() {
        let event = PenEvent {
            device_id: 0,
            tool: PenTool::Pen,
            x: 100.5,
            y: 200.5,
            pressure: 0.75,
            tilt_x: 10.0,
            tilt_y: -5.0,
            rotation: 90.0,
            button_state: 0b0010,
            hover_distance: 0,
            timestamp_us: 1234,
            flags: 0,
        };
        let wire = event.to_wire();
        assert_eq!(wire.device_id_value(), 0);
        assert_eq!(wire.x_value(), 100.5);
        assert_eq!(wire.y_value(), 200.5);
        assert!((wire.pressure_value() - 0.75).abs() < 1e-6);
        assert_eq!(wire.timestamp_us_value(), 1234);
    }
}
