//! Process-wide winit glue types.
//!
//! The desktop session drives a single `winit::EventLoop` (project rule
//! #3 — one EventLoop per process). All cross-thread wake-ups the
//! event loop needs to hear about are funnelled through
//! [`WinitUserEvent`], the user-event payload that
//! `EventLoop::with_user_event()` carries.
//!
//! Today the only producer of these events is the
//! [`RunningHwFrameDecoder`](crate::decoder_hw::RunningHwFrameDecoder)
//! (a `WinitUserEvent::FrameReady` per decoded frame) and the future
//! wgpu-based video loop. The blank overlay and the stats overlay
//! piggy-back on the same enum rather than spinning up a second
//! EventLoop.

use crate::blank_overlay::OverlayCommand;

/// Events delivered to the winit `ApplicationHandler` via
/// `EventLoop::with_user_event`.
///
/// Add new variants whenever a new cross-thread producer needs to
/// wake the main render loop. The renderer's `user_event` arm in
/// [`crate::winit_app::AppState`] dispatches on these variants.
#[derive(Debug, Clone)]
pub enum WinitUserEvent {
    /// The HW decoder thread forwarded a new frame on its
    /// `crossbeam_channel`. The renderer should drain the channel
    /// inside the next `RedrawRequested`.
    FrameReady,
    /// Request a graceful exit. Carries an error message string;
    /// empty string means "clean exit".
    Exit(String),
    /// Show or hide the blank overlay window (from the control
    /// stream's `ControlMsg::BlankOverlay`).
    Overlay(OverlayCommand),
    /// Toggle the stats overlay visibility (Ctrl+Alt+S).
    ToggleStats,
    /// Toggle the tiled view mode (Ctrl+T).
    ToggleTile,
    /// Cycle to the next display stream (Ctrl+S).
    CycleStream,
    /// Toggle the privacy indicator visibility (Ctrl+P).
    TogglePrivacy,
    /// Sent when the host advertises new display capabilities
    /// (`ControlMsg::DisplayCapabilities`). The renderer may switch
    /// to HDR tone mapping and request 10-bit pixels.
    CapabilitiesChanged {
        /// True when the host can deliver HDR static metadata.
        hdr: bool,
        /// Max refresh rate the host can sustain, in Hz.
        max_hz: u32,
        /// Max resolution in pixels (width, height).
        max_resolution: [u16; 2],
    },
    /// Sent when the pen device list changes (hotplug).
    PenDeviceListChanged {
        /// Number of devices currently connected. Zero = pen
        /// capture should be paused.
        count: u16,
    },
    /// Sent when the user toggles tone mapping (Ctrl+Alt+T).
    CycleToneMap,
    /// Sent when the user adjusts the pen pressure curve
    /// (Ctrl+Alt+P). The argument is a `0.0..=1.0` curve exponent.
    PenPressureCurve(f32),
}

impl WinitUserEvent {
    /// Construct a `FrameReady` event. Shorthand for the common case.
    pub fn frame_ready() -> Self {
        WinitUserEvent::FrameReady
    }

    /// Construct an `Exit` event from a plain `&str`.
    pub fn exit(reason: impl Into<String>) -> Self {
        WinitUserEvent::Exit(reason.into())
    }
}
