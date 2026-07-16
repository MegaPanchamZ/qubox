//! macOS pen capture stub — NSEvent binding skeleton.
//!
//! Per ADR-019 §7.3, NSEvent capture is defined here but **injection
//! is deferred** (no public CoreGraphics API for tablet event synthesis).
//! The struct compiles on all targets; only macOS with the `nsevent`
//! feature provides a real implementation.
//!
//! ## TCC requirement
//!
//! The user must grant **Input Monitoring** permission under
//! *System Settings → Privacy & Security → Input Monitoring*.
//! Without this, `NSEventTypeTabletPoint` events never arrive.
//! Log `tracing::error!` instructing the user when capture returns
//! zero events after 5 seconds.
//!
//! ## Dependencies
//!
//! - `objc2 = "0.5.1"` (safe msg_send bindings)
//! - `objc2-foundation = "0.2.0"` (`MainThreadMarker`, `NSDefaultRunLoopMode`)
//! - `objc2-app-kit = "0.2.0"` (`NSEvent`, `NSEventType`, `NSTilt`)

use crossbeam_channel::{Receiver, Sender};

use crate::error::PenCaptureError;
use crate::traits::{PenCapture, PenDeviceInfo, PenEvent};

/// NSEvent-backed pen capture.
///
/// Must run on the main thread (`MainThreadMarker::new()`). Spawns a
/// dedicated OS thread that calls `NSEvent::nextEventMatchingMask` in
/// a polling loop.
#[derive(Debug)]
pub struct NseventCapture;

impl NseventCapture {
    /// Create a new macOS capture instance.
    ///
    /// On non-macOS targets or when the `nsevent` feature is disabled,
    /// returns `FeatureDisabled`.
    pub fn new() -> Result<Self, PenCaptureError> {
        #[cfg(all(target_os = "macos", feature = "nsevent"))]
        {
            Ok(Self)
        }
        #[cfg(not(all(target_os = "macos", feature = "nsevent")))]
        {
            Err(PenCaptureError::FeatureDisabled("nsevent"))
        }
    }
}

impl Default for NseventCapture {
    fn default() -> Self {
        Self
    }
}

impl PenCapture for NseventCapture {
    fn enumerate_devices(&self) -> Result<Vec<PenDeviceInfo>, PenCaptureError> {
        #[cfg(all(target_os = "macos", feature = "nsevent"))]
        {
            // NSEvent does not expose a device enumeration API.
            // Tablets are hot-plugged; the first received
            // NSEventTypeTabletPoint creates a device entry.
            Ok(Vec::new())
        }
        #[cfg(not(all(target_os = "macos", feature = "nsevent")))]
        {
            Err(PenCaptureError::FeatureDisabled("nsevent"))
        }
    }

    fn start(&mut self, event_tx: Sender<PenEvent>) -> Result<Receiver<PenEvent>, PenCaptureError> {
        #[cfg(all(target_os = "macos", feature = "nsevent"))]
        {
            // Real implementation spawns a thread with MainThreadMarker
            // and polls NSEvent::nextEventMatchingMask for
            // NSEventType::TabletPoint events. Deferred to PR 10.
            let (tx, rx) = crossbeam_channel::bounded(64);
            let _ = event_tx;
            std::thread::Builder::new()
                .name("bp-pen-nsevent".into())
                .spawn(move || {
                    tracing::info!("macOS NSEvent capture thread started (stub)");
                    // Real impl: loop { NSEvent::nextEventMatchingMask → PenEvent → tx }
                    loop {
                        std::thread::sleep(std::time::Duration::from_millis(100));
                        if tx.is_full() {
                            break;
                        }
                    }
                })
                .map_err(|e| PenCaptureError::Backend(e.to_string()))?;
            Ok(rx)
        }
        #[cfg(not(all(target_os = "macos", feature = "nsevent")))]
        {
            let _ = event_tx;
            Err(PenCaptureError::FeatureDisabled("nsevent"))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nsevent_capture_new_returns_feature_disabled_without_feature() {
        if cfg!(not(all(target_os = "macos", feature = "nsevent"))) {
            let err = NseventCapture::new().unwrap_err();
            assert!(matches!(err, PenCaptureError::FeatureDisabled("nsevent")));
        }
    }

    #[test]
    fn nsevent_capture_enumerate_returns_empty_without_feature() {
        if cfg!(not(all(target_os = "macos", feature = "nsevent"))) {
            let cap = NseventCapture;
            let err = cap.enumerate_devices().unwrap_err();
            assert!(matches!(err, PenCaptureError::FeatureDisabled("nsevent")));
        }
    }

    #[test]
    fn nsevent_capture_start_returns_feature_disabled_without_feature() {
        if cfg!(not(all(target_os = "macos", feature = "nsevent"))) {
            let mut cap = NseventCapture;
            let (tx, _rx) = crossbeam_channel::bounded(1);
            let err = cap.start(tx).unwrap_err();
            assert!(matches!(err, PenCaptureError::FeatureDisabled("nsevent")));
        }
    }
}
