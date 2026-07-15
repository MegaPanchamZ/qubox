//! Platform dispatch for pen capture and injection.
//!
//! Each platform module re-exports `current_capture` and
//! `current_injector` constructors. When a feature gate is disabled
//! (e.g. `--no-default-features`), the dispatch falls back to a stub
//! so the host-agent always compiles.
//!
//! The host-agent must check the returned type at runtime; stubs
//! return errors from `enumerate_devices` / `start` so the
//! failure mode is loud and discoverable rather than silent.

use crate::error::{PenCaptureError, PenInjectError};
use crate::traits::{PenCapture, PenEvent, PenInjector};
use crossbeam_channel::{Receiver, Sender};

/// Tag describing which implementation `current_capture` /
/// `current_injector` returned. Useful for log messages and tests.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CurrentPlatformPen {
    LinuxLibinput,
    LinuxUinput,
    WindowsPointer,
    WindowsWinTab,
    Stub,
}

impl CurrentPlatformPen {
    /// Human-readable label for log lines.
    pub fn label(self) -> &'static str {
        match self {
            CurrentPlatformPen::LinuxLibinput => "linux-libinput",
            CurrentPlatformPen::LinuxUinput => "linux-uinput",
            CurrentPlatformPen::WindowsPointer => "windows-wm_pointer",
            CurrentPlatformPen::WindowsWinTab => "windows-wintab",
            CurrentPlatformPen::Stub => "stub",
        }
    }
}

/// Capture stub used when no platform feature is enabled.
pub struct StubCapture {
    devices: Vec<crate::PenDeviceInfo>,
}

impl StubCapture {
    pub fn new() -> Self {
        Self {
            devices: Vec::new(),
        }
    }

    pub fn with_devices(devices: Vec<crate::PenDeviceInfo>) -> Self {
        Self { devices }
    }
}

impl Default for StubCapture {
    fn default() -> Self {
        Self::new()
    }
}

impl PenCapture for StubCapture {
    fn enumerate_devices(&self) -> Result<Vec<crate::PenDeviceInfo>, PenCaptureError> {
        Ok(self.devices.clone())
    }
    fn start(
        &mut self,
        _event_tx: Sender<PenEvent>,
    ) -> Result<Receiver<PenEvent>, PenCaptureError> {
        let (tx, rx) = crossbeam_channel::bounded(1);
        drop(tx);
        Ok(rx)
    }
}

/// Injector stub. Always fails with `FeatureDisabled`.
pub struct StubInjector;

impl StubInjector {
    pub fn new() -> Self {
        Self
    }
}

impl Default for StubInjector {
    fn default() -> Self {
        Self::new()
    }
}

impl PenInjector for StubInjector {
    fn inject(&mut self, _event: &PenEvent) -> Result<(), PenInjectError> {
        Err(PenInjectError::FeatureDisabled("uinput"))
    }
    fn device_name(&self) -> &str {
        "stub"
    }
}

/// Convenience constructors that always succeed, returning a stub
/// implementation. Use these in `main()` as the default and let the
/// `linux::` / `windows::` modules provide real implementations.
pub fn stub_capture() -> StubCapture {
    StubCapture::new()
}

pub fn stub_injector() -> StubInjector {
    StubInjector::new()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::PenDeviceInfo;
    use qubox_proto::pen::PenDeviceDescriptor;
    use qubox_proto::PenTool;

    #[test]
    fn stub_capture_enumerates_empty_devices() {
        let cap = StubCapture::new();
        let devices = cap.enumerate_devices().unwrap();
        assert!(devices.is_empty());
    }

    #[test]
    fn stub_capture_enumerates_supplied_devices() {
        let info = PenDeviceInfo {
            descriptor: PenDeviceDescriptor {
                device_id: 7,
                name: "test".to_string(),
                tools: vec![PenTool::Pen],
                max_pressure: 0,
                max_tilt_degrees: 0,
                rotation_supported: false,
            },
        };
        let cap = StubCapture::with_devices(vec![info.clone()]);
        let devices = cap.enumerate_devices().unwrap();
        assert_eq!(devices, vec![info]);
    }

    #[test]
    fn stub_injector_returns_feature_disabled() {
        let mut inj = StubInjector::new();
        let event = PenEvent {
            device_id: 0,
            tool: PenTool::Pen,
            x: 0.0,
            y: 0.0,
            pressure: 0.0,
            tilt_x: 0.0,
            tilt_y: 0.0,
            rotation: 0.0,
            button_state: 0,
            hover_distance: 0,
            timestamp_us: 0,
            flags: 0,
        };
        let err = inj.inject(&event).unwrap_err();
        assert!(matches!(err, PenInjectError::FeatureDisabled(_)));
    }

    #[test]
    fn current_platform_label_is_stable() {
        assert_eq!(CurrentPlatformPen::LinuxLibinput.label(), "linux-libinput");
        assert_eq!(CurrentPlatformPen::Stub.label(), "stub");
    }
}
