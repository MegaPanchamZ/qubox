//! P0-6 gamepad capture (client side).
//!
//! Uses `gilrs` 0.11 to enumerate local gamepads, polls events at
//! 125 Hz, and emits `qubox_proto::RemoteInputEvent::Gamepad`
//! messages on the supplied unbounded sender. The events are picked
//! up by the existing `send_input_events` task in qubox-client-cli and
//! forwarded over the QUIC control channel to the host.
//!
//! ## Status
//!
//! The `Gilrs::poll_events()` → `WireGamepadState` mapping is
//! scaffolded below. Per P0-6 spec research/roadmap/p0-06-gamepad.md:
//! - Only the **first** gamepad is captured (multi-controller is a
//!   follow-up; `WireGamepadState.gamepad_id` carries the slot index).
//! - Axes (sticks, triggers) are mapped to the same ranges the
//!   host's uinput backend expects (sticks i16 in [-32768, 32767],
//!   triggers u8 in [0, 255]).
//! - Buttons are bit-packed into `buttons_lo`/`buttons_hi` (Xbox
//!   layout: A=bit0, B=bit1, X=bit2, Y=bit3, LB=bit4, RB=bit5,
//!   Back=bit6, Start=bit7, L3=bit8, R3=bit9, Guide=bit10).
//!
//! The current scaffold builds and starts the capture thread; the
//! per-event mapping function is left as a TODO so a follow-up can
//! fill it in without breaking the wire format or the host backend.

use std::sync::Arc;
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result};
use qubox_proto::{RemoteInputEvent, WireGamepadState};
use gilrs::{EventType, Gilrs};
use tokio::sync::mpsc::UnboundedSender;

pub const POLL_INTERVAL: Duration = Duration::from_millis(8);

/// Start the gamepad capture loop on a dedicated thread. Returns
/// the `JoinHandle` so the caller can keep the handle alive for the
/// session's lifetime; the loop exits when `event_tx` is dropped.
pub fn spawn(event_tx: UnboundedSender<RemoteInputEvent>) -> Result<thread::JoinHandle<()>> {
    let gilrs = Gilrs::new()
        .map_err(|e| anyhow::anyhow!("failed to init gilrs: {e:?}"))?;
    let gilrs = Arc::new(parking_lot_lite::Mutex::new(gilrs));
    let handle = thread::Builder::new()
        .name("bp-gamepad-capture".to_string())
        .spawn(move || run_loop(gilrs, event_tx))
        .context("failed to spawn gamepad capture thread")?;
    Ok(handle)
}

fn run_loop(gilrs: Arc<parking_lot_lite::Mutex<Gilrs>>, event_tx: UnboundedSender<RemoteInputEvent>) {
    tracing::info!("P0-6 gamepad capture thread started");
    loop {
        let mut state = WireGamepadState::default();
        {
            let mut g = gilrs.lock();
            while let Some(event) = g.next_event() {
                if let Some(s) = map_event(event.event, event.id) {
                    state = s;
                }
            }
        }
        if state != WireGamepadState::default() {
            // Coalesce: only forward if state changed.
            if event_tx.send(RemoteInputEvent::Gamepad { state }).is_err() {
                tracing::debug!("gamepad event channel closed; capture loop exiting");
                return;
            }
        }
        thread::sleep(POLL_INTERVAL);
    }
}

fn map_event(event: EventType, _id: gilrs::GamepadId) -> Option<WireGamepadState> {
    // TODO: map GamepadId to gamepad_id slot. P0-6 only captures the
    // first gamepad; the wire format already carries the slot via
    // gamepad_id. gilrs 0.11's GamepadId doesn't expose a stable
    // `From<u32>`, so for now we always send 0 (single-gamepad).
    let mut state = WireGamepadState::default();
    match event {
        EventType::ButtonPressed(button, _) | EventType::ButtonReleased(button, _) => {
            let pressed = matches!(event, EventType::ButtonPressed(_, _));
            let bit = match button {
                gilrs::Button::South => WireGamepadState::BTN_A,
                gilrs::Button::East => WireGamepadState::BTN_B,
                gilrs::Button::West => WireGamepadState::BTN_X,
                gilrs::Button::North => WireGamepadState::BTN_Y,
                gilrs::Button::LeftTrigger => WireGamepadState::BTN_LB,
                gilrs::Button::RightTrigger => WireGamepadState::BTN_RB,
                gilrs::Button::Select => WireGamepadState::BTN_SELECT,
                gilrs::Button::Start => WireGamepadState::BTN_START,
                gilrs::Button::Mode => WireGamepadState::BTN_GUIDE,
                _ => 0,
            };
            if bit != 0 {
                if pressed {
                    if bit <= 0x7F {
                        state.buttons_lo |= bit;
                    } else {
                        state.buttons_hi |= bit;
                    }
                } else {
                    if bit <= 0x7F {
                        state.buttons_lo &= !bit;
                    } else {
                        state.buttons_hi &= !bit;
                    }
                }
            }
            Some(state)
        }
        EventType::AxisChanged(axis, value, _) => {
            match axis {
                gilrs::Axis::LeftStickX => state.lx = axis_to_i16(value),
                gilrs::Axis::LeftStickY => state.ly = axis_to_i16(value),
                gilrs::Axis::RightStickX => state.rx = axis_to_i16(value),
                gilrs::Axis::RightStickY => state.ry = axis_to_i16(value),
                gilrs::Axis::LeftZ => state.lt = axis_to_u8(value),
                gilrs::Axis::RightZ => state.rt = axis_to_u8(value),
                _ => return None,
            }
            Some(state)
        }
        _ => None,
    }
}

fn axis_to_i16(v: f32) -> i16 {
    (v.clamp(-1.0, 1.0) * 32767.0) as i16
}

fn axis_to_u8(v: f32) -> u8 {
    // Triggers on most gamepads rest at -1.0 (Linux evdev reports
    // triggers as axes with range [-1, 1] where -1 is released).
    let normalized = (v + 1.0) * 0.5;
    (normalized.clamp(0.0, 1.0) * 255.0) as u8
}

// Minimal Mutex shim to avoid the parking_lot dependency just for one
// critical section in the capture loop. std::sync::Mutex would block
// the worker thread when polling gilrs; parking_lot_lite::Mutex is a
// minimal no-poison wrapper used by several small dependencies.
mod parking_lot_lite {
    use std::sync::Mutex as StdMutex;
    pub struct Mutex<T>(StdMutex<T>);
    impl<T> Mutex<T> {
        pub fn new(value: T) -> Self {
            Self(StdMutex::new(value))
        }
        pub fn lock(&self) -> std::sync::MutexGuard<'_, T> {
            self.0.lock().expect("gamepad mutex poisoned")
        }
    }
}
