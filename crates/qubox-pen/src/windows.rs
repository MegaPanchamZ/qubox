//! Windows pen capture via WM_POINTER and injection via WinTab (stubs).
//!
//! ADR-010 §4.3 calls for `RegisterPointerInputTarget` + the
//! `POINTER_PEN_INFO` decoder on the capture side; §4.4 calls for
//! `WinTab` `WT_PACKET` for injection (with `InjectTouchInput` as
//! fallback for touch devices).
//!
//! Today the `windows` crate's `Win32_UI_Input_Pointer` and
//! `Win32_UI_TabletPC` features are declared in `Cargo.toml`, but the
//! real decoder lives behind a follow-up PR — the project does not
//! yet have a Windows CI runner in a position to test it. The stubs
//! compile and surface `FeatureDisabled` so the host-agent always
//! builds on `windows-latest`.

use crossbeam_channel::{Receiver, Sender};

use crate::error::{PenCaptureError, PenInjectError};
use crate::traits::{PenCapture, PenDeviceInfo, PenEvent};

/// Capture stub for Windows. Returns `FeatureDisabled` so callers
/// can detect the missing backend at runtime.
pub struct WmPointerCapture;

impl WmPointerCapture {
    pub fn new() -> Result<Self, PenCaptureError> {
        Ok(Self)
    }
}

impl Default for WmPointerCapture {
    fn default() -> Self {
        Self::new().expect("Windows WM_POINTER capture stub always succeeds")
    }
}

impl PenCapture for WmPointerCapture {
    fn enumerate_devices(&self) -> Result<Vec<PenDeviceInfo>, PenCaptureError> {
        // Real implementation reads `GetPointerDevices` and filters
        // by `POINTER_DEVICE_PRODUCT` strings; deferred to v2.
        Ok(Vec::new())
    }
    fn start(
        &mut self,
        _event_tx: Sender<PenEvent>,
    ) -> Result<Receiver<PenEvent>, PenCaptureError> {
        Err(PenCaptureError::Backend(
            "Windows WM_POINTER capture pending v2 (ADR-010 §4.3)".to_string(),
        ))
    }
}

/// Injector stub for Windows. Returns `FeatureDisabled` until the
/// WinTab / `InjectTouchInput` decoder lands.
pub struct WinTabInjector {
    name: String,
}

impl WinTabInjector {
    pub fn new(name: impl Into<String>) -> Result<Self, PenInjectError> {
        Ok(Self { name: name.into() })
    }
}

impl crate::traits::PenInjector for WinTabInjector {
    fn inject(&mut self, _event: &PenEvent) -> Result<(), PenInjectError> {
        Err(PenInjectError::Backend(
            "Windows WinTab injection pending v2 (ADR-010 §4.4)".to_string(),
        ))
    }
    fn device_name(&self) -> &str {
        &self.name
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::traits::{PenCapture, PenInjector};

    #[test]
    fn wm_pointer_capture_new_succeeds() {
        let cap = WmPointerCapture::new();
        assert!(cap.is_ok());
    }

    #[test]
    fn wm_pointer_capture_enumerate_returns_empty() {
        let cap = WmPointerCapture::new().unwrap();
        let devices = cap.enumerate_devices().unwrap();
        assert!(devices.is_empty());
    }

    #[test]
    fn wm_pointer_capture_start_returns_backend_error() {
        let mut cap = WmPointerCapture::new().unwrap();
        let (tx, _rx) = crossbeam_channel::bounded(1);
        let result = cap.start(tx);
        assert!(matches!(result, Err(PenCaptureError::Backend(_))));
    }

    #[test]
    fn wintab_injector_returns_backend_error() {
        let mut inj = WinTabInjector::new("test").unwrap();
        let event = PenEvent {
            device_id: 0,
            tool: qubox_proto::PenTool::Pen,
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
        let result = inj.inject(&event);
        assert!(matches!(result, Err(PenInjectError::Backend(_))));
        assert_eq!(inj.device_name(), "test");
    }
}
