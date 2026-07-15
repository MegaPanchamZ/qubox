//! Host-side virtual audio device creation.
//!
//! On Linux the v1 implementation is a stub: we return
//! `Ok(false)` from `try_create()` because PipeWire's libspa
//! virtual-source API is non-trivial and the project's risk
//! register calls this out as a follow-up. The mic pipeline on
//! the client still encodes the audio; the host simply doesn't
//! surface it to apps in v1. The `MicConfigAck::virtual_device_ok`
//! flag reflects this so the client can surface a warning.
//!
//! Windows and macOS implementations are also stubs in v1.

use std::ffi::CString;

use qubox_proto::MicStreamConfig;

/// Result of attempting to create a virtual mic sink.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VirtualDeviceStatus {
    /// True if a virtual input device is now exposing the mic
    /// stream to local apps.
    pub device_created: bool,
}

/// Public façade for creating / destroying a virtual mic sink.
/// Construction never fails — the worst case is `device_created
/// = false` (no virtual sink; the user is informed via
/// `MicConfigAck::virtual_device_ok`).
pub struct VirtualMicDevice {
    pub name: String,
    pub status: VirtualDeviceStatus,
}

impl VirtualMicDevice {
    /// Try to create a virtual mic sink with the given name.
    pub fn try_create(name: &str, _config: &MicStreamConfig) -> Self {
        let c_name = CString::new(name).unwrap_or_else(|_| CString::new("bp-virtual-mic").unwrap());
        #[cfg(target_os = "linux")]
        {
            match linux::create_virtual_source(&c_name) {
                Ok(()) => {
                    tracing::info!(name = %name, "PipeWire virtual mic source created");
                    Self {
                        name: name.to_string(),
                        status: VirtualDeviceStatus {
                            device_created: true,
                        },
                    }
                }
                Err(error) => {
                    tracing::warn!(
                        ?error,
                        "PipeWire virtual mic source creation failed; mic will be encoded but unavailable to local apps"
                    );
                    Self {
                        name: name.to_string(),
                        status: VirtualDeviceStatus {
                            device_created: false,
                        },
                    }
                }
            }
        }
        #[cfg(not(target_os = "linux"))]
        {
            let _ = c_name;
            tracing::debug!(
                "virtual mic sink not implemented on this platform; mic will be encoded but unavailable to local apps"
            );
            Self {
                name: name.to_string(),
                status: VirtualDeviceStatus {
                    device_created: false,
                },
            }
        }
    }

    /// Push decoded PCM samples into the virtual sink. No-op when
    /// no virtual device was created.
    pub fn push_samples(&self, _samples: &[f32]) {
        if !self.status.device_created {
            return;
        }
        #[cfg(target_os = "linux")]
        {
            linux::push_to_virtual_source(_samples);
        }
    }
}

impl Drop for VirtualMicDevice {
    fn drop(&mut self) {
        if !self.status.device_created {
            return;
        }
        #[cfg(target_os = "linux")]
        {
            linux::destroy_virtual_source(&self.name);
        }
    }
}

#[cfg(target_os = "linux")]
mod linux {
    use std::sync::atomic::{AtomicBool, Ordering};

    static VIRTUAL_SOURCE_READY: AtomicBool = AtomicBool::new(false);

    pub fn create_virtual_source(
        name: &std::ffi::CStr,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let _ = name;
        if !pipewire_available() {
            return Err("pipewire library not available at runtime".into());
        }
        VIRTUAL_SOURCE_READY.store(true, Ordering::Release);
        Ok(())
    }

    pub fn push_to_virtual_source(samples: &[f32]) {
        if !VIRTUAL_SOURCE_READY.load(Ordering::Acquire) {
            return;
        }
        let _ = samples;
    }

    pub fn destroy_virtual_source(_name: &str) {
        VIRTUAL_SOURCE_READY.store(false, Ordering::Release);
    }

    fn pipewire_available() -> bool {
        false
    }
}
