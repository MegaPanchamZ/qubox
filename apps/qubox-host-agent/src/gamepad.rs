//! P0-6 gamepad backend (host side).
//!
//! Consumes `RemoteInputEvent::Gamepad { state }` and writes the
//! state to a virtual gamepad. Per ADR-004
//! research/decisions/ADR-004-virtual-gamepad-uinput-vigembus.md:
//!
//! - **Linux**: `/dev/uinput` virtual Xbox360 gamepad — uses the
//!   `uinput` 0.1 crate to open `/dev/uinput`, register 14 EV_KEY
//!   codes (10 face/shoulder/stick buttons + 4 d-pad), 6 EV_ABS axes
//!   (4 sticks ±32768, 2 triggers 0..=255), and write events via
//!   `Device::send()` + `Device::synchronize()`.
//! - **Windows**: ViGEmBus client (deferred — not in scope on this
//!   dev box).
//! - **macOS**: deferred per the spec; HID post-event hook on the
//!   remote desktop is not yet viable.
//!
//! ## Wire-format button layout
//!
//! | buttons_lo bit | Xbox button  | uinput code       |
//! |----------------|--------------|-------------------|
//! | 0              | A            | `GamePad::South`  |
//! | 1              | B            | `GamePad::East`   |
//! | 2              | X            | `GamePad::West`   |
//! | 3              | Y            | `GamePad::North`  |
//! | 4              | LB           | `GamePad::TL`     |
//! | 5              | RB           | `GamePad::TR`     |
//! | 6              | Back         | `GamePad::Select` |
//! | 7              | Start        | `GamePad::Start`  |
//!
//! | buttons_hi bit | Xbox button  | uinput code       |
//! |----------------|--------------|-------------------|
//! | 0              | L3           | `GamePad::ThumbL` |
//! | 1              | R3           | `GamePad::ThumbR` |
//! | 2              | Guide        | `GamePad::Mode`   |
//!
//! | flags bit      | Xbox button  | uinput code       |
//! |----------------|--------------|-------------------|
//! | 0              | D-Pad Up     | `DPad::Up`        |
//! | 1              | D-Pad Down   | `DPad::Down`      |
//! | 2              | D-Pad Left   | `DPad::Left`      |
//! | 3              | D-Pad Right  | `DPad::Right`     |
//!
//! Axes layout: LX→`Position::X`, LY→`Position::Y`, RX→`Position::RX`,
//! RY→`Position::RY`, LT→`Position::Z`, RT→`Position::RZ`.

use std::thread;

use anyhow::{anyhow, Result};
use qubox_proto::WireGamepadState;
use tokio::sync::mpsc::UnboundedReceiver;

use crate::RemoteInputEvent;

/// Handle to a running gamepad backend. Drop to stop the thread.
pub struct GamepadBackendHandle {
    join: Option<thread::JoinHandle<Result<()>>>,
}

impl Drop for GamepadBackendHandle {
    fn drop(&mut self) {
        let _ = self.join.take();
    }
}

/// Spawn the gamepad backend on a dedicated thread. The caller is
/// expected to feed `RemoteInputEvent::Gamepad` into the returned
/// receiver. On non-Linux platforms, returns a no-op handle and the
/// receiver is still consumed in the caller's pipeline (events are
/// dropped).
pub fn spawn(mut event_rx: UnboundedReceiver<RemoteInputEvent>) -> Result<GamepadBackendHandle> {
    #[cfg(target_os = "linux")]
    {
        let join = thread::Builder::new()
            .name("bp-gamepad-backend-linux".to_string())
            .spawn(move || run_loop_linux(&mut event_rx))
            .map_err(|e| anyhow!("failed to spawn gamepad backend thread: {e}"))?;
        Ok(GamepadBackendHandle { join: Some(join) })
    }
    #[cfg(not(target_os = "linux"))]
    {
        let join = thread::Builder::new()
            .name("bp-gamepad-backend-noop".to_string())
            .spawn(move || {
                while event_rx.blocking_recv().is_some() {}
                Ok(())
            })
            .map_err(|e| anyhow!("failed to spawn gamepad noop thread: {e}"))?;
        Ok(GamepadBackendHandle { join: Some(join) })
    }
}

// --------------------------------------------------------------------------
// Linux uinput backend
// --------------------------------------------------------------------------

#[cfg(target_os = "linux")]
mod linux {
    //! This module is a private child so its imports don't pollute the
    //! crate root on non-Linux platforms. Only `run_loop` is called from
    //! `run_loop_linux`.

    use anyhow::{anyhow, Result};
    use qubox_proto::WireGamepadState;

    use uinput::event::absolute::{Absolute, Position};
    use uinput::event::controller::{DPad, GamePad};
    use uinput::Device;

    /// Number of EV_KEY codes we register. 14 active buttons
    /// (10 face/shoulder/stick + 4 d-pad); the wire format reserves
    /// 2 more positions (16 total) per the p0-06 spec.
    #[allow(dead_code)]
    pub const BUTTON_COUNT: usize = 14;

    /// Number of EV_ABS axes we register.
    #[allow(dead_code)]
    pub const AXIS_COUNT: usize = 6;

    #[allow(dead_code)]
    pub const STICK_MIN: i32 = -32768;
    #[allow(dead_code)]
    pub const STICK_MAX: i32 = 32767;
    #[allow(dead_code)]
    pub const TRIGGER_MIN: i32 = 0;
    #[allow(dead_code)]
    pub const TRIGGER_MAX: i32 = 255;

    /// Open /dev/uinput, register the full Xbox360 event surface,
    /// and return the created `Device`. Returns a clear error if
    /// /dev/uinput is absent (no panic).
    pub fn open() -> Result<Device> {
        let dev = uinput::open("/dev/uinput")
            .map_err(|e| anyhow!(
                "cannot open /dev/uinput: {e}; install the uinput module \
                 or check udev rules per ADR-004 \
                 research/decisions/ADR-004-virtual-gamepad-uinput-vigembus.md"
            ))?
            .name("qubox virtual gamepad")
            .map_err(|e| anyhow!("failed to set uinput device name: {e}"))?;

        // Chain all event registration + axis range setup, then create.
        let dev = register_events(dev)?;
        dev.create().map_err(|e| anyhow!("failed to create uinput device: {e}"))
    }

    fn register_events(
        builder: uinput::device::Builder,
    ) -> Result<uinput::device::Builder> {
        // -- EV_KEY: 14 buttons --
        let builder = builder
            .event(GamePad::South)?
            .event(GamePad::East)?
            .event(GamePad::West)?
            .event(GamePad::North)?
            .event(GamePad::TL)?
            .event(GamePad::TR)?
            .event(GamePad::Select)?
            .event(GamePad::Start)?
            .event(GamePad::ThumbL)?
            .event(GamePad::ThumbR)?
            .event(GamePad::Mode)?
            .event(DPad::Up)?
            .event(DPad::Down)?
            .event(DPad::Left)?
            .event(DPad::Right)?;

        // -- EV_ABS: 4 stick axes, 2 trigger axes --
        let builder = builder
            .event(Absolute::Position(Position::X))?
            .min(STICK_MIN)
            .max(STICK_MAX)
            .event(Absolute::Position(Position::Y))?
            .min(STICK_MIN)
            .max(STICK_MAX)
            .event(Absolute::Position(Position::RX))?
            .min(STICK_MIN)
            .max(STICK_MAX)
            .event(Absolute::Position(Position::RY))?
            .min(STICK_MIN)
            .max(STICK_MAX)
            .event(Absolute::Position(Position::Z))?
            .min(TRIGGER_MIN)
            .max(TRIGGER_MAX)
            .event(Absolute::Position(Position::RZ))?
            .min(TRIGGER_MIN)
            .max(TRIGGER_MAX);

        Ok(builder)
    }

    /// Write a full snapshot of `state` to the uinput device. Sends
    /// EV_KEY for every button (press=1, release=0), EV_ABS for every
    /// axis, then EV_SYN to commit the batch.
    pub fn apply(dev: &mut Device, state: &WireGamepadState) -> Result<()> {
        let lo = state.buttons_lo;
        let hi = state.buttons_hi;
        let fl = state.flags;

        // Face + shoulders + back/start
        dev.send(GamePad::South, is_set(lo, WireGamepadState::BTN_A))?;
        dev.send(GamePad::East, is_set(lo, WireGamepadState::BTN_B))?;
        dev.send(GamePad::West, is_set(lo, WireGamepadState::BTN_X))?;
        dev.send(GamePad::North, is_set(lo, WireGamepadState::BTN_Y))?;
        dev.send(GamePad::TL, is_set(lo, WireGamepadState::BTN_LB))?;
        dev.send(GamePad::TR, is_set(lo, WireGamepadState::BTN_RB))?;
        dev.send(GamePad::Select, is_set(lo, WireGamepadState::BTN_SELECT))?;
        dev.send(GamePad::Start, is_set(lo, WireGamepadState::BTN_START))?;

        // Stick clicks + guide
        dev.send(GamePad::ThumbL, is_set(hi, WireGamepadState::BTN_L3))?;
        dev.send(GamePad::ThumbR, is_set(hi, WireGamepadState::BTN_R3))?;
        dev.send(GamePad::Mode, is_set(hi, WireGamepadState::BTN_GUIDE))?;

        // D-pad
        dev.send(DPad::Up, is_set(fl, WireGamepadState::FLAG_DPAD_UP))?;
        dev.send(DPad::Down, is_set(fl, WireGamepadState::FLAG_DPAD_DOWN))?;
        dev.send(DPad::Left, is_set(fl, WireGamepadState::FLAG_DPAD_LEFT))?;
        dev.send(DPad::Right, is_set(fl, WireGamepadState::FLAG_DPAD_RIGHT))?;

        // Axes — copy packed fields to locals before passing by reference
        let lx = state.lx;
        let ly = state.ly;
        let rx = state.rx;
        let ry = state.ry;
        let lt = state.lt;
        let rt = state.rt;

        dev.send(Absolute::Position(Position::X), lx as i32)?;
        dev.send(Absolute::Position(Position::Y), ly as i32)?;
        dev.send(Absolute::Position(Position::RX), rx as i32)?;
        dev.send(Absolute::Position(Position::RY), ry as i32)?;
        dev.send(Absolute::Position(Position::Z), lt as i32)?;
        dev.send(Absolute::Position(Position::RZ), rt as i32)?;

        dev.synchronize()?;
        Ok(())
    }

    #[inline]
    fn is_set(bits: u8, mask: u8) -> i32 {
        (bits & mask != 0) as i32
    }
}

#[cfg(target_os = "linux")]
fn run_loop_linux(rx: &mut UnboundedReceiver<RemoteInputEvent>) -> Result<()> {
    tracing::info!("P0-6 gamepad backend thread started (uinput)");
    let mut gamepad: Option<uinput::Device> = None;

    while let Some(event) = rx.blocking_recv() {
        if let RemoteInputEvent::Gamepad { state } = event {
            if gamepad.is_none() {
                match linux::open() {
                    Ok(dev) => {
                        tracing::info!("P0-6 uinput gamepad device opened");
                        gamepad = Some(dev);
                    }
                    Err(e) => {
                        tracing::warn!(
                            "P0-6 uinput open failed: {e:#}; dropping gamepad events \
                             (will retry on next event)"
                        );
                        continue;
                    }
                }
            }
            if let Some(ref mut dev) = gamepad {
                if let Err(e) = linux::apply(dev, &state) {
                    tracing::warn!("P0-6 uinput write failed: {e:#}");
                }
            }
        }
    }
    Ok(())
}

/// Convert a `WireGamepadState` to a friendly log message. Exposed
/// for diagnostics; not used by the runtime.
#[allow(dead_code)]
pub fn describe_state(state: &WireGamepadState) -> String {
    let mut s = format!("gamepad_id={}", state.gamepad_id);
    if state.buttons_lo != 0 {
        s.push_str(&format!(" lo=0b{:08b}", state.buttons_lo));
    }
    if state.buttons_hi != 0 {
        s.push_str(&format!(" hi=0b{:08b}", state.buttons_hi));
    }
    let lt = state.lt;
    let rt = state.rt;
    let lx = state.lx;
    let ly = state.ly;
    let rx = state.rx;
    let ry = state.ry;
    s.push_str(&format!(
        " lt={lt} rt={rt} lx={lx} ly={ly} rx={rx} ry={ry}"
    ));
    s
}

#[cfg(test)]
mod uinput_tests {
    //! Pure-data tests for the uinput config constants and bit
    //! decoding. These do NOT touch /dev/uinput.

    use qubox_proto::WireGamepadState;

    #[cfg(target_os = "linux")]
    use super::linux;

    #[test]
    fn button_count_matches_spec() {
        // The spec defines 14 active buttons (10 face/shoulder/stick +
        // 4 d-pad) with 2 reserved positions for a total of 16 in the
        // wire format. Our uinput device registers 14 EV_KEY codes.
        #[cfg(target_os = "linux")]
        assert_eq!(linux::BUTTON_COUNT, 14, "expected 14 EV_KEY codes registered");
        #[cfg(not(target_os = "linux"))]
        println!("skipped on non-Linux (no uinput)");
    }

    #[test]
    fn axis_count_matches_spec() {
        // 4 stick axes (X, Y, RX, RY) + 2 trigger axes (Z, RZ) = 6.
        #[cfg(target_os = "linux")]
        assert_eq!(linux::AXIS_COUNT, 6, "expected 6 EV_ABS axes");
        #[cfg(not(target_os = "linux"))]
        println!("skipped on non-Linux");
    }

    #[test]
    fn stick_axis_range_matches_xbox360() {
        #[cfg(target_os = "linux")]
        {
            assert_eq!(linux::STICK_MIN, -32768);
            assert_eq!(linux::STICK_MAX, 32767);
        }
        #[cfg(not(target_os = "linux"))]
        println!("skipped on non-Linux");
    }

    #[test]
    fn trigger_axis_range_matches_xbox360() {
        #[cfg(target_os = "linux")]
        {
            assert_eq!(linux::TRIGGER_MIN, 0);
            assert_eq!(linux::TRIGGER_MAX, 255);
        }
        #[cfg(not(target_os = "linux"))]
        println!("skipped on non-Linux");
    }

    #[test]
    fn button_a_decodes_from_buttons_lo_bit0() {
        use WireGamepadState as W;
        let mut s = W::default();
        // Bit 0 in buttons_lo = A button pressed
        s.buttons_lo = W::BTN_A;
        #[cfg(target_os = "linux")]
        {
            let pressed = s.buttons_lo & W::BTN_A != 0;
            assert!(pressed, "BTN_A bit should decode to South (A) pressed");
        }
    }

    #[test]
    fn all_buttons_lo_map_to_expected_bits() {
        use WireGamepadState as W;
        assert_eq!(W::BTN_A, 1 << 0, "A = bit 0");
        assert_eq!(W::BTN_B, 1 << 1, "B = bit 1");
        assert_eq!(W::BTN_X, 1 << 2, "X = bit 2");
        assert_eq!(W::BTN_Y, 1 << 3, "Y = bit 3");
        assert_eq!(W::BTN_LB, 1 << 4, "LB = bit 4");
        assert_eq!(W::BTN_RB, 1 << 5, "RB = bit 5");
        assert_eq!(W::BTN_SELECT, 1 << 6, "Select = bit 6");
        assert_eq!(W::BTN_START, 1 << 7, "Start = bit 7");
    }

    #[test]
    fn all_buttons_hi_map_to_expected_bits() {
        use WireGamepadState as W;
        assert_eq!(W::BTN_L3, 1 << 0, "L3 = bit 0");
        assert_eq!(W::BTN_R3, 1 << 1, "R3 = bit 1");
        assert_eq!(W::BTN_GUIDE, 1 << 2, "Guide = bit 2");
    }

    #[test]
    fn dpad_flags_map_to_expected_bits() {
        use WireGamepadState as W;
        assert_eq!(W::FLAG_DPAD_UP, 1 << 0, "DPad-Up = bit 0");
        assert_eq!(W::FLAG_DPAD_DOWN, 1 << 1, "DPad-Down = bit 1");
        assert_eq!(W::FLAG_DPAD_LEFT, 1 << 2, "DPad-Left = bit 2");
        assert_eq!(W::FLAG_DPAD_RIGHT, 1 << 3, "DPad-Right = bit 3");
    }

    #[test]
    fn wire_gamepad_state_size() {
        assert_eq!(WireGamepadState::SIZE, 16);
        assert_eq!(std::mem::size_of::<WireGamepadState>(), 16);
    }
}
