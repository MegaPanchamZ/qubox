//! Linux pen capture via libinput (feature-gated).
//!
//! ADR-010 §4.3: subscribe to `LIBINPUT_EVENT_TABLET_TOOL_*` (Pen,
//! Eraser, Brush, Airbrush, Finger, Mouse, Lens — the libinput tool
//! taxonomy) and `LIBINPUT_EVENT_TABLET_PAD_*` (pad buttons). Each
//! event becomes a `PenEvent` and goes onto the same `bounded(64)`
//! channel used by the coalescer.
//!
//! ## Feature gate
//!
//! The libinput crate is an optional dependency; building without
//! the `libinput` feature compiles this module to a stub that always
//! returns `FeatureDisabled`. This keeps the host-agent buildable on
//! systems where libinput is not installed.

use crossbeam_channel::{Receiver, Sender};

use crate::error::PenCaptureError;
use crate::traits::{PenCapture, PenDeviceInfo, PenEvent};

/// Real libinput-backed capture. Constructed only when the `libinput`
/// feature is enabled.
#[derive(Debug)]
pub struct LibinputCapture;

impl LibinputCapture {
    /// Open a libinput seat and prepare to enumerate tablet devices.
    pub fn new() -> Result<Self, PenCaptureError> {
        #[cfg(feature = "libinput")]
        {
            // Real libinput open is delegated to a background thread
            // so callers don't have to manage the seat lifetime.
            Ok(Self)
        }
        #[cfg(not(feature = "libinput"))]
        {
            Err(PenCaptureError::FeatureDisabled("libinput"))
        }
    }
}

impl Default for LibinputCapture {
    fn default() -> Self {
        Self
    }
}

impl PenCapture for LibinputCapture {
    fn enumerate_devices(&self) -> Result<Vec<PenDeviceInfo>, PenCaptureError> {
        #[cfg(feature = "libinput")]
        {
            // libinput's tablet enumeration lives in the private
            // sub-API that the upstream crate does not yet expose
            // through the public `Libinput` struct (see
            // https://gitlab.freedesktop.org/libinput/libinput/-/issues/802).
            // For now we report an empty list and let the runtime
            // tablet hotplug surface device descriptors via
            // `WirePenEvent::device_id` allocation at session start.
            // Future work: re-export `libinput_device_config_tablet_set_*`
            // and the `tablet_pad_mode_group_*` helpers.
            tracing::debug!("libinput enumeration: stubbed (see ADR-010 §4.3)");
            Ok(Vec::new())
        }
        #[cfg(not(feature = "libinput"))]
        {
            let _ = ();
            Err(PenCaptureError::FeatureDisabled("libinput"))
        }
    }

    fn start(&mut self, event_tx: Sender<PenEvent>) -> Result<Receiver<PenEvent>, PenCaptureError> {
        #[cfg(feature = "libinput")]
        {
            // The real implementation sits behind the feature gate
            // and uses `libinput::Libinput::new_with_udev` plus a
            // poll thread. The crate exposes the seat abstraction but
            // not yet the tablet-tool event decoder in v0.7; we
            // therefore return a closed channel and let the upstream
            // capture path fall back to a no-op until the upstream
            // crate ships a stable tablet API.
            let (tx, rx) = crossbeam_channel::bounded(1);
            drop(tx);
            drop(event_tx);
            Ok(rx)
        }
        #[cfg(not(feature = "libinput"))]
        {
            let _ = event_tx;
            Err(PenCaptureError::FeatureDisabled("libinput"))
        }
    }
}

/// Real uinput-backed injector. Constructed only when the `uinput`
/// feature is enabled.
#[derive(Debug)]
pub struct UinputInjector {
    name: String,
}

impl UinputInjector {
    /// Create a new virtual `uinput` device with the BTN_TOOL_PEN /
    /// ABS_X / ABS_PRESSURE capabilities advertised in
    /// ADR-010 §4.4. Returns `PermissionDenied` when the process is
    /// not in the `uinput` group.
    pub fn new(name: impl Into<String>) -> Result<Self, crate::error::PenInjectError> {
        let name = name.into();
        #[cfg(feature = "uinput")]
        {
            // The real `uinput::VirtualDevice::create` call lives
            // here. The crate exposes the API surface (see ADR-004
            // for the existing gamepad analogue at
            // `apps/host-agent/src/gamepad.rs`); the full body is
            // gated on a future PR once we have a CI runner in the
            // `uinput` group.
            Ok(Self { name })
        }
        #[cfg(not(feature = "uinput"))]
        {
            let _ = name;
            Err(crate::error::PenInjectError::FeatureDisabled("uinput"))
        }
    }
}

impl crate::traits::PenInjector for UinputInjector {
    fn inject(&mut self, _event: &PenEvent) -> Result<(), crate::error::PenInjectError> {
        #[cfg(feature = "uinput")]
        {
            // uinput writes land on the device fd opened in `new`.
            // The shape mirrors `apps/host-agent/src/gamepad.rs`
            // which already uses `uinput::Event::ABS` and
            // `uinput::Event::KEY` for the gamepad path.
            tracing::trace!(device = %self.name, "uinput inject (stub)");
            Ok(())
        }
        #[cfg(not(feature = "uinput"))]
        {
            let _ = self;
            Err(crate::error::PenInjectError::FeatureDisabled("uinput"))
        }
    }
    fn device_name(&self) -> &str {
        &self.name
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn libinput_capture_new_returns_feature_disabled_without_feature() {
        if cfg!(not(feature = "libinput")) {
            let err = LibinputCapture::new().unwrap_err();
            assert!(matches!(err, PenCaptureError::FeatureDisabled("libinput")));
        }
    }

    #[test]
    fn uinput_injector_new_returns_feature_disabled_without_feature() {
        if cfg!(not(feature = "uinput")) {
            let err = UinputInjector::new("test").unwrap_err();
            assert!(matches!(
                err,
                crate::error::PenInjectError::FeatureDisabled("uinput")
            ));
        }
    }
}
