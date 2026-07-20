//! Linux uinput-based remote input injection.
//!
//! Opens `/dev/uinput` once and emits EV_KEY / EV_REL / EV_ABS events for
//! `RemoteInputEvent`s received over the WebRTC `qubox-input` data channel.
//!
//! Requires:
//!   * Linux kernel with uinput support (CONFIG_INPUT_UINPUT=y).
//!   * Read+write access to `/dev/uinput` — usually membership in the
//!     `input` group, or running as root.
//!
//! Falls back to a no-op shim when uinput isn't available so the agent
//! still runs on systems without the device node (CI, macOS, Windows).
//!
//! macOS / Windows hosts are handled in their respective platform modules;
//! this file is Linux-only. The Tauri host-agent on those platforms will
//! dispatch via the native Tauri shell IPC instead.

use anyhow::{anyhow, Result};
#[cfg(target_os = "linux")]
use anyhow::Context;
use qubox_proto::{InputMouseButton, RemoteInputEvent};
#[cfg(target_os = "linux")]
use std::fs::File;
#[cfg(target_os = "linux")]
use std::fs::OpenOptions;
#[cfg(target_os = "linux")]
use std::io::{IoSlice, Write};
#[cfg(target_os = "linux")]
use std::os::unix::fs::OpenOptionsExt;
#[cfg(target_os = "linux")]
use std::path::Path;
use std::sync::Mutex;

/// Maximum length of a uinput name string (including NUL).
#[cfg(target_os = "linux")]
const UINPUT_MAX_NAME_SIZE: usize = 80;

/// Pre-defined input-event-codes constants (subset of linux/input.h).
/// Avoids pulling in `uinput` / `input-event-codes` crates — the host-agent
/// already has plenty of native deps.
#[cfg(target_os = "linux")]
const EV_SYN: u16 = 0x00;
#[cfg(target_os = "linux")]
const EV_KEY: u16 = 0x01;
#[cfg(target_os = "linux")]
const EV_REL: u16 = 0x02;
#[cfg(target_os = "linux")]
const EV_MSC: u16 = 0x04;
#[cfg(target_os = "linux")]
const EV_ABS: u16 = 0x03;

#[cfg(target_os = "linux")]
const SYN_REPORT: u16 = 0x00;
#[cfg(target_os = "linux")]
const MSC_SCAN: u16 = 0x04;

#[cfg(target_os = "linux")]
const REL_X: u16 = 0x00;
#[cfg(target_os = "linux")]
const REL_Y: u16 = 0x01;
#[cfg(target_os = "linux")]
const REL_WHEEL: u16 = 0x08;
#[cfg(target_os = "linux")]
const REL_HWHEEL: u16 = 0x06;

#[cfg(target_os = "linux")]
const ABS_X: u16 = 0x00;
#[cfg(target_os = "linux")]
const ABS_Y: u16 = 0x01;

/// Common X11 keysyms mapped to Linux KEY_* codes. We translate the browser's
/// `KeyboardEvent.key` strings (which are already in `KeyboardEvent.code`
/// form like "KeyA", "Enter", "ArrowLeft") to evdev codes.
#[cfg(target_os = "linux")]
fn map_key(name: &str) -> Option<u16> {
    use linux_keys::*;
    match name {
        // Letters A-Z (evdev KeyA=30..KeyZ=89).
        s if s.starts_with("Key") && s.len() == 4 => {
            let c = s.as_bytes()[3] as char;
            if c.is_ascii_uppercase() {
                Some(KEY_A + (c as u16 - 'A' as u16))
            } else {
                None
            }
        }
        // Digits: KEY_1=2 .. KEY_9=10, KEY_0=11 (not sequential from KEY_0).
        s if s.starts_with("Digit") && s.len() == 6 => {
            let c = s.as_bytes()[5] as char;
            match c {
                '0' => Some(KEY_0),
                '1'..='9' => Some(KEY_1 + (c as u16 - b'1' as u16)),
                _ => None,
            }
        }
        // Arrow keys.
        "ArrowLeft" => Some(KEY_LEFT),
        "ArrowRight" => Some(KEY_RIGHT),
        "ArrowUp" => Some(KEY_UP),
        "ArrowDown" => Some(KEY_DOWN),
        // Whitespace / control.
        "Enter" => Some(KEY_ENTER),
        "Escape" => Some(KEY_ESC),
        "Backspace" => Some(KEY_BACKSPACE),
        "Tab" => Some(KEY_TAB),
        "Space" => Some(KEY_SPACE),
        // Editing.
        "Delete" => Some(KEY_DELETE),
        "Insert" => Some(KEY_INSERT),
        "Home" => Some(KEY_HOME),
        "End" => Some(KEY_END),
        "PageUp" => Some(KEY_PAGEUP),
        "PageDown" => Some(KEY_PAGEDOWN),
        // Modifiers.
        "ShiftLeft" => Some(KEY_LEFTSHIFT),
        "ShiftRight" => Some(KEY_RIGHTSHIFT),
        "ControlLeft" => Some(KEY_LEFTCTRL),
        "ControlRight" => Some(KEY_RIGHTCTRL),
        "AltLeft" => Some(KEY_LEFTALT),
        "AltRight" => Some(KEY_RIGHTALT),
        "MetaLeft" => Some(KEY_LEFTMETA),
        "MetaRight" => Some(KEY_RIGHTMETA),
        "CapsLock" => Some(KEY_CAPSLOCK),
        "NumLock" => Some(KEY_NUMLOCK),
        "ScrollLock" => Some(KEY_SCROLLLOCK),
        // Function row.
        s if s.starts_with("F") && s.len() <= 3 => {
            let n: u16 = s[1..].parse().ok()?;
            if (1..=12).contains(&n) {
                Some(KEY_F1 + (n - 1))
            } else {
                None
            }
        }
        // Punctuation (US layout; other layouts remapped by X server).
        "Minus" => Some(KEY_MINUS),
        "Equal" => Some(KEY_EQUAL),
        "BracketLeft" => Some(KEY_LEFTBRACE),
        "BracketRight" => Some(KEY_RIGHTBRACE),
        "Backslash" => Some(KEY_BACKSLASH),
        "Semicolon" => Some(KEY_SEMICOLON),
        "Quote" => Some(KEY_APOSTROPHE),
        "Backquote" => Some(KEY_GRAVE),
        "Comma" => Some(KEY_COMMA),
        "Period" => Some(KEY_DOT),
        "Slash" => Some(KEY_SLASH),
        _ => None,
    }
}

#[cfg(target_os = "linux")]
mod linux_keys {
    //! Subset of `<linux/input-event-codes.h>` we use.
    pub const KEY_ESC: u16 = 1;
    pub const KEY_1: u16 = 2;
    pub const KEY_2: u16 = 3;
    pub const KEY_3: u16 = 4;
    pub const KEY_4: u16 = 5;
    pub const KEY_5: u16 = 6;
    pub const KEY_6: u16 = 7;
    pub const KEY_7: u16 = 8;
    pub const KEY_8: u16 = 9;
    pub const KEY_9: u16 = 10;
    pub const KEY_0: u16 = 11;
    pub const KEY_MINUS: u16 = 12;
    pub const KEY_EQUAL: u16 = 13;
    pub const KEY_BACKSPACE: u16 = 14;
    pub const KEY_TAB: u16 = 15;
    pub const KEY_Q: u16 = 16;
    pub const KEY_W: u16 = 17;
    pub const KEY_E: u16 = 18;
    pub const KEY_R: u16 = 19;
    pub const KEY_T: u16 = 20;
    pub const KEY_Y: u16 = 21;
    pub const KEY_U: u16 = 22;
    pub const KEY_I: u16 = 23;
    pub const KEY_O: u16 = 24;
    pub const KEY_P: u16 = 25;
    pub const KEY_LEFTBRACE: u16 = 26;
    pub const KEY_RIGHTBRACE: u16 = 27;
    pub const KEY_ENTER: u16 = 28;
    pub const KEY_LEFTCTRL: u16 = 29;
    pub const KEY_A: u16 = 30;
    pub const KEY_S: u16 = 31;
    pub const KEY_D: u16 = 32;
    pub const KEY_F: u16 = 33;
    pub const KEY_G: u16 = 34;
    pub const KEY_H: u16 = 35;
    pub const KEY_J: u16 = 36;
    pub const KEY_K: u16 = 37;
    pub const KEY_L: u16 = 38;
    pub const KEY_SEMICOLON: u16 = 39;
    pub const KEY_APOSTROPHE: u16 = 40;
    pub const KEY_GRAVE: u16 = 41;
    pub const KEY_LEFTSHIFT: u16 = 42;
    pub const KEY_BACKSLASH: u16 = 43;
    pub const KEY_Z: u16 = 44;
    pub const KEY_X: u16 = 45;
    pub const KEY_C: u16 = 46;
    pub const KEY_V: u16 = 47;
    pub const KEY_B: u16 = 48;
    pub const KEY_N: u16 = 49;
    pub const KEY_M: u16 = 50;
    pub const KEY_COMMA: u16 = 51;
    pub const KEY_DOT: u16 = 52;
    pub const KEY_SLASH: u16 = 53;
    pub const KEY_RIGHTSHIFT: u16 = 54;
    pub const KEY_KPASTERISK: u16 = 55;
    pub const KEY_LEFTALT: u16 = 56;
    pub const KEY_SPACE: u16 = 57;
    pub const KEY_CAPSLOCK: u16 = 58;
    pub const KEY_F1: u16 = 59;
    pub const KEY_F2: u16 = 60;
    pub const KEY_F3: u16 = 61;
    pub const KEY_F4: u16 = 62;
    pub const KEY_F5: u16 = 63;
    pub const KEY_F6: u16 = 64;
    pub const KEY_F7: u16 = 65;
    pub const KEY_F8: u16 = 66;
    pub const KEY_F9: u16 = 67;
    pub const KEY_F10: u16 = 68;
    pub const KEY_NUMLOCK: u16 = 69;
    pub const KEY_SCROLLLOCK: u16 = 70;
    pub const KEY_KP7: u16 = 71;
    pub const KEY_KP8: u16 = 72;
    pub const KEY_KP9: u16 = 73;
    pub const KEY_KPMINUS: u16 = 74;
    pub const KEY_KP4: u16 = 75;
    pub const KEY_KP5: u16 = 76;
    pub const KEY_KP6: u16 = 77;
    pub const KEY_KPPLUS: u16 = 78;
    pub const KEY_KP1: u16 = 79;
    pub const KEY_KP2: u16 = 80;
    pub const KEY_KP3: u16 = 81;
    pub const KEY_KP0: u16 = 82;
    pub const KEY_KPDOT: u16 = 83;
    pub const KEY_F11: u16 = 87;
    pub const KEY_F12: u16 = 88;
    pub const KEY_KPENTER: u16 = 96;
    pub const KEY_RIGHTCTRL: u16 = 97;
    pub const KEY_KPSLASH: u16 = 98;
    pub const KEY_RIGHTALT: u16 = 100;
    pub const KEY_HOME: u16 = 102;
    pub const KEY_UP: u16 = 103;
    pub const KEY_PAGEUP: u16 = 104;
    pub const KEY_LEFT: u16 = 105;
    pub const KEY_RIGHT: u16 = 106;
    pub const KEY_END: u16 = 107;
    pub const KEY_DOWN: u16 = 108;
    pub const KEY_PAGEDOWN: u16 = 109;
    pub const KEY_INSERT: u16 = 110;
    pub const KEY_DELETE: u16 = 111;
    pub const KEY_LEFTMETA: u16 = 125;
    pub const KEY_RIGHTMETA: u16 = 126;
}

/// Linux uinput injector (real device).
#[cfg(target_os = "linux")]
pub struct UinputInjector {
    file: Mutex<File>,
}

#[cfg(target_os = "linux")]
impl UinputInjector {
    /// Open `/dev/uinput` and register a virtual keyboard + mouse + touchpad.
    ///
    /// On non-Linux or when `/dev/uinput` is unavailable, returns `Ok(None)`
    /// — the caller should treat that as a no-op injector (logged but
    /// non-fatal). This lets the agent run on CI / macOS / Windows where
    /// `input_inject` is a stub.
    pub fn open() -> Result<Option<Self>> {
        if !cfg!(target_os = "linux") {
            return Ok(None);
        }
        let path = Path::new("/dev/uinput");
        if !path.exists() {
            tracing::warn!("/dev/uinput not present; input injection disabled");
            return Ok(None);
        }
        let mut opts = OpenOptions::new();
        opts.read(true)
            .write(true)
            .custom_flags(libc_like::O_NONBLOCK);
        let file = opts
            .open(path)
            .with_context(|| format!("open {}", path.display()))?;
        let mut me = Self {
            file: Mutex::new(file),
        };
        me.register_device().with_context(|| {
            format!(
                "register uinput device (path: {:?}, evbit_first: {:?})",
                path,
                std::io::Error::last_os_error()
            )
        })?;
        Ok(Some(me))
    }

    fn register_device(&mut self) -> Result<()> {
        let mut f = self.file.lock().unwrap();

        // UI_SET_EVBIT — enable event types.
        for &ev_type in &[EV_KEY, EV_REL, EV_ABS, EV_MSC] {
            me_set_evbit(&mut f, ev_type)?;
        }
        // UI_SET_KEYBIT — keyboard + mouse buttons + gamepad.
        // BTN_LEFT/RIGHT/MIDDLE are 0x110..=0x112 — outside 1..=127.
        for code in 1u16..=127 {
            me_set_keybit(&mut f, code)?;
        }
        for code in 0x100u16..=0x14fu16 {
            me_set_keybit(&mut f, code)?;
        }
        for code in 0x130u16..=0x13fu16 {
            me_set_keybit(&mut f, code)?;
        }
        // UI_SET_RELBIT — relative axes.
        for &rel in &[REL_X, REL_Y, REL_WHEEL, REL_HWHEEL] {
            me_set_relbit(&mut f, rel)?;
        }
        // UI_SET_ABSBIT — touch / pen / gamepad absolute axes.
        for &abs in &[ABS_X, ABS_Y, 0x02, 0x03, 0x04, 0x05, 0x10, 0x11] {
            me_set_absbit(&mut f, abs)?;
        }
        // UI_SET_MSCBIT
        me_set_mscbit(&mut f, MSC_SCAN)?;
        // INPUT_PROP_DIRECT — map ABS 0..65535 onto the full desktop
        // (tablet-style). Without this, libinput often ignores ABS motion.
        me_set_propbit(&mut f, 0x01)?; // INPUT_PROP_DIRECT

        // UI_ABS_SETUP — ABS_X/Y range must match browser coords (0..=65535).
        // Default kernel range is 0..0 → absolute moves are no-ops.
        me_abs_setup(&mut f, ABS_X, 0, 65535)?;
        me_abs_setup(&mut f, ABS_Y, 0, 65535)?;
        // Gamepad sticks / triggers (reasonable defaults).
        me_abs_setup(&mut f, 0x02, 0, 255)?; // ABS_Z
        me_abs_setup(&mut f, 0x03, -32768, 32767)?; // ABS_RX
        me_abs_setup(&mut f, 0x04, -32768, 32767)?; // ABS_RY
        me_abs_setup(&mut f, 0x05, 0, 255)?; // ABS_RZ
        me_abs_setup(&mut f, 0x10, -1, 1)?; // ABS_HAT0X
        me_abs_setup(&mut f, 0x11, -1, 1)?; // ABS_HAT0Y

        // UI_DEV_SETUP — required before UI_DEV_CREATE on modern kernels
        // (>= 4.5). Without this, UI_DEV_CREATE returns EINVAL.
        // struct uinput_setup {
        //   char name[UINPUT_MAX_NAME_SIZE];   /* 80 bytes */
        //   struct input_id {                  /* 4 × u16 = 8 bytes */
        //     __u16 bustype; __u16 vendor; __u16 product; __u16 version;
        //   } id;
        // };
        // C sizeof(uinput_setup) = 92 (name 80 + 4-byte padding + 8 id).
        // The kernel doesn't strictly enforce the ioctl's encoded size —
        // it copies `sizeof(struct uinput_setup)` bytes regardless — but
        // matching the value the kernel expects avoids any _IOC check.
        // UI_DEV_SETUP = _IOW('U', 3, sizeof(uinput_setup)) = 0x405c5503.
        let mut setup = [0u8; 92];
        let name_bytes = b"qubox-host-agent\0";
        setup[..name_bytes.len()].copy_from_slice(name_bytes);
        // input_id at offset 80: bustype=BUS_VIRTUAL (0x06) as little-endian u16.
        setup[80..82].copy_from_slice(&0x0006u16.to_le_bytes());
        f.ioctl(0x405c_5503, (&setup as *const _) as u64)
            .context("UI_DEV_SETUP ioctl failed")?;

        // UI_DEV_CREATE — magic constant 0x5501 on Linux x86_64 / aarch64.
        const UI_DEV_CREATE: libc_like::Ioctl = 0x5501;
        f.ioctl(UI_DEV_CREATE, 0)
            .context("UI_DEV_CREATE ioctl failed")?;
        tracing::info!("uinput virtual pointer+keyboard registered (ABS 0..65535, PROP_DIRECT)");
        Ok(())
    }

    /// Emit a single `RemoteInputEvent` as a uinput event burst.
    pub fn dispatch(&self, ev: &RemoteInputEvent) -> Result<()> {
        match ev {
            RemoteInputEvent::MouseMove { x, y } => {
                let mut f = self.file.lock().unwrap();
                write_abs_xy(&mut f, *x, *y)?;
            }
            RemoteInputEvent::RelativeMouseMove { dx, dy } => {
                let mut f = self.file.lock().unwrap();
                write_event(&mut f, EV_REL, REL_X, *dx)?;
                write_event(&mut f, EV_REL, REL_Y, *dy)?;
                write_event(&mut f, EV_SYN, SYN_REPORT, 0)?;
            }
            RemoteInputEvent::MouseButton {
                button,
                pressed,
                x,
                y,
            } => {
                let code = match button {
                    InputMouseButton::Left => 0x110,   // BTN_LEFT
                    InputMouseButton::Right => 0x111,  // BTN_RIGHT
                    InputMouseButton::Middle => 0x112, // BTN_MIDDLE
                };
                let mut f = self.file.lock().unwrap();
                // Browser often sends coords on the click itself (tap) —
                // warp first so press lands at the right pixel.
                if let (Some(px), Some(py)) = (*x, *y) {
                    write_abs_xy(&mut f, px, py)?;
                }
                write_event(&mut f, EV_KEY, code, if *pressed { 1 } else { 0 })?;
                write_event(&mut f, EV_SYN, SYN_REPORT, 0)?;
            }
            RemoteInputEvent::MouseWheel { dx, dy } => {
                // Browser wheel deltas are pixel-ish; uinput wants notches.
                let notch_y = normalize_wheel(*dy);
                let notch_x = normalize_wheel(*dx);
                let mut f = self.file.lock().unwrap();
                if notch_y != 0 {
                    write_event(&mut f, EV_REL, REL_WHEEL, notch_y)?;
                }
                if notch_x != 0 {
                    write_event(&mut f, EV_REL, REL_HWHEEL, notch_x)?;
                }
                write_event(&mut f, EV_SYN, SYN_REPORT, 0)?;
            }
            RemoteInputEvent::Keyboard { key, pressed } => {
                let Some(code) = map_key(key) else {
                    tracing::warn!(key, "ignoring unknown key");
                    return Ok(());
                };
                let mut f = self.file.lock().unwrap();
                write_event(&mut f, EV_KEY, code, if *pressed { 1 } else { 0 })?;
                write_event(&mut f, EV_SYN, SYN_REPORT, 0)?;
            }
            RemoteInputEvent::Gamepad { state } => {
                let mut f = self.file.lock().unwrap();

                // 1. Buttons (lo)
                write_event(
                    &mut f,
                    EV_KEY,
                    0x130,
                    if (state.buttons_lo & (1 << 0)) != 0 {
                        1
                    } else {
                        0
                    },
                )?; // A
                write_event(
                    &mut f,
                    EV_KEY,
                    0x131,
                    if (state.buttons_lo & (1 << 1)) != 0 {
                        1
                    } else {
                        0
                    },
                )?; // B
                write_event(
                    &mut f,
                    EV_KEY,
                    0x133,
                    if (state.buttons_lo & (1 << 2)) != 0 {
                        1
                    } else {
                        0
                    },
                )?; // X
                write_event(
                    &mut f,
                    EV_KEY,
                    0x134,
                    if (state.buttons_lo & (1 << 3)) != 0 {
                        1
                    } else {
                        0
                    },
                )?; // Y
                write_event(
                    &mut f,
                    EV_KEY,
                    0x136,
                    if (state.buttons_lo & (1 << 4)) != 0 {
                        1
                    } else {
                        0
                    },
                )?; // LB
                write_event(
                    &mut f,
                    EV_KEY,
                    0x137,
                    if (state.buttons_lo & (1 << 5)) != 0 {
                        1
                    } else {
                        0
                    },
                )?; // RB
                write_event(
                    &mut f,
                    EV_KEY,
                    0x13a,
                    if (state.buttons_lo & (1 << 6)) != 0 {
                        1
                    } else {
                        0
                    },
                )?; // Select
                write_event(
                    &mut f,
                    EV_KEY,
                    0x13b,
                    if (state.buttons_lo & (1 << 7)) != 0 {
                        1
                    } else {
                        0
                    },
                )?; // Start

                // 2. Buttons (hi)
                write_event(
                    &mut f,
                    EV_KEY,
                    0x13d,
                    if (state.buttons_hi & (1 << 0)) != 0 {
                        1
                    } else {
                        0
                    },
                )?; // LS (L3)
                write_event(
                    &mut f,
                    EV_KEY,
                    0x13e,
                    if (state.buttons_hi & (1 << 1)) != 0 {
                        1
                    } else {
                        0
                    },
                )?; // RS (R3)
                write_event(
                    &mut f,
                    EV_KEY,
                    0x13c,
                    if (state.buttons_hi & (1 << 2)) != 0 {
                        1
                    } else {
                        0
                    },
                )?; // Guide (Mode)

                // 3. DPAD (Hat)
                let hat_x = if (state.flags & (1 << 2)) != 0 {
                    -1
                } else if (state.flags & (1 << 3)) != 0 {
                    1
                } else {
                    0
                };
                let hat_y = if (state.flags & (1 << 0)) != 0 {
                    -1
                } else if (state.flags & (1 << 1)) != 0 {
                    1
                } else {
                    0
                };
                write_event(&mut f, EV_ABS, 0x10, hat_x)?; // ABS_HAT0X
                write_event(&mut f, EV_ABS, 0x11, hat_y)?; // ABS_HAT0Y

                // 4. Analog Sticks
                write_event(&mut f, EV_ABS, ABS_X, state.lx as i32)?;
                write_event(&mut f, EV_ABS, ABS_Y, state.ly as i32)?;
                write_event(&mut f, EV_ABS, 0x03, state.rx as i32)?; // ABS_RX
                write_event(&mut f, EV_ABS, 0x04, state.ry as i32)?; // ABS_RY

                // 5. Analog Triggers
                write_event(&mut f, EV_ABS, 0x02, state.lt as i32)?; // ABS_Z
                write_event(&mut f, EV_ABS, 0x05, state.rt as i32)?; // ABS_RZ

                write_event(&mut f, EV_SYN, SYN_REPORT, 0)?;
            }
            // HoverDisplay / Pen are not injected by host — they're
            // either client→server telemetry or host-emitted (hover).
            other => {
                tracing::debug!(?other, "ignoring non-injectable event");
            }
        }
        Ok(())
    }
}

#[cfg(target_os = "linux")]
impl Drop for UinputInjector {
    fn drop(&mut self) {
        if let Ok(mut f) = self.file.lock() {
            const UI_DEV_DESTROY: libc_like::Ioctl = 0x5502;
            let _ = f.ioctl(UI_DEV_DESTROY, 0);
        }
    }
}

#[cfg(target_os = "linux")]
fn write_event(file: &mut File, etype: u16, code: u16, value: i32) -> Result<()> {
    // struct input_event { struct timeval time; unsigned short type; unsigned short code; int value; }
    // timeval = 16 bytes (tv_sec: 8 + tv_usec: 8).
    let mut buf = [0u8; 24];
    buf[16..18].copy_from_slice(&etype.to_le_bytes());
    buf[18..20].copy_from_slice(&code.to_le_bytes());
    buf[20..24].copy_from_slice(&value.to_le_bytes());
    let slices = [IoSlice::new(&buf)];
    let n = file
        .write_vectored(&slices)
        .context("write input_event to uinput")?;
    if n != buf.len() {
        return Err(anyhow!("short write to uinput: {n}/{}", buf.len()));
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn write_abs_xy(file: &mut File, x: u32, y: u32) -> Result<()> {
    let x = x.min(65535) as i32;
    let y = y.min(65535) as i32;
    write_event(file, EV_ABS, ABS_X, x)?;
    write_event(file, EV_ABS, ABS_Y, y)?;
    write_event(file, EV_SYN, SYN_REPORT, 0)?;
    Ok(())
}

/// Map browser wheel delta (often ±100+ pixels) to a small number of notches.
fn normalize_wheel(delta: i32) -> i32 {
    if delta == 0 {
        return 0;
    }
    let sign = if delta > 0 { 1 } else { -1 };
    let mag = (delta.abs() / 40).max(1).min(5);
    sign * mag
}

// ── Minimal libc re-exports so we don't pull `nix` for two ioctls ──

#[cfg(target_os = "linux")]
mod libc_like {
    pub type Ioctl = u32;
    pub const O_NONBLOCK: i32 = 0x800;

    pub trait IoctlExt {
        fn ioctl(&mut self, req: Ioctl, arg: u64) -> std::io::Result<i32>;
    }

    impl IoctlExt for std::fs::File {
        fn ioctl(&mut self, req: Ioctl, arg: u64) -> std::io::Result<i32> {
            // Use the raw libc::ioctl through std::os::fd::AsRawFd.
            use std::os::fd::AsRawFd;
            extern "C" {
                fn ioctl(fd: i32, request: u64, ...) -> i32;
            }
            // SAFETY: variadic call but we pass exactly one trailing `unsigned long`
            // for the int/pointer-sized arg, matching `libc::ioctl(fd, req, arg)`.
            let fd = self.as_raw_fd();
            let rc = unsafe { ioctl(fd, req as u64, arg) };
            if rc < 0 {
                Err(std::io::Error::last_os_error())
            } else {
                Ok(rc)
            }
        }
    }

    // ioctl bit setters (UI_SET_EVBIT etc.) are encoded as long values that
    // describe the event bit position. We compute them inline.
    pub fn _evbit_offset(ev_type: u16) -> u32 {
        const UI_SET_EVBIT: u32 = 0x4004_55_60;
        UI_SET_EVBIT | ev_type as u32
    }
}

#[cfg(target_os = "linux")]
fn me_set_evbit(f: &mut File, ev_type: u16) -> Result<()> {
    // UI_SET_EVBIT = _IOW('U', 100, int) = 0x40045564
    f.ioctl(0x4004_5564, ev_type as u64)
        .map(|_| ())
        .with_context(|| {
            format!(
                "UI_SET_EVBIT(ev_type={ev_type}, errno={})",
                std::io::Error::last_os_error()
            )
        })
}
#[cfg(target_os = "linux")]
fn me_set_keybit(f: &mut File, code: u16) -> Result<()> {
    // UI_SET_KEYBIT = _IOW('U', 101, int) = 0x40045565
    f.ioctl(0x4004_5565, code as u64)
        .map(|_| ())
        .context("UI_SET_KEYBIT")
}
#[cfg(target_os = "linux")]
fn me_set_relbit(f: &mut File, code: u16) -> Result<()> {
    // UI_SET_RELBIT = _IOW('U', 102, int) = 0x40045566
    f.ioctl(0x4004_5566, code as u64)
        .map(|_| ())
        .context("UI_SET_RELBIT")
}
#[cfg(target_os = "linux")]
fn me_set_absbit(f: &mut File, code: u16) -> Result<()> {
    // UI_SET_ABSBIT = _IOW('U', 103, int) = 0x40045567
    f.ioctl(0x4004_5567, code as u64)
        .map(|_| ())
        .context("UI_SET_ABSBIT")
}
#[cfg(target_os = "linux")]
fn me_set_mscbit(f: &mut File, code: u16) -> Result<()> {
    // UI_SET_MSCBIT = _IOW('U', 104, int) = 0x40045568
    f.ioctl(0x4004_5568, code as u64)
        .map(|_| ())
        .context("UI_SET_MSCBIT")
}

#[cfg(target_os = "linux")]
fn me_set_propbit(f: &mut File, code: u16) -> Result<()> {
    // UI_SET_PROPBIT = _IOW('U', 110, int) = 0x4004556e
    f.ioctl(0x4004_556e, code as u64)
        .map(|_| ())
        .context("UI_SET_PROPBIT")
}

#[cfg(target_os = "linux")]
/// UI_ABS_SETUP — set absinfo for one axis before UI_DEV_CREATE.
/// Layout matches kernel `struct uinput_abs_setup` (28 bytes on x86_64):
///   u16 code; u16 pad; s32 value,minimum,maximum,fuzz,flat,resolution;
fn me_abs_setup(f: &mut File, code: u16, minimum: i32, maximum: i32) -> Result<()> {
    let mut buf = [0u8; 28];
    buf[0..2].copy_from_slice(&code.to_le_bytes());
    // value at offset 4
    buf[4..8].copy_from_slice(&0i32.to_le_bytes());
    buf[8..12].copy_from_slice(&minimum.to_le_bytes());
    buf[12..16].copy_from_slice(&maximum.to_le_bytes());
    // fuzz/flat/resolution stay 0
    // UI_ABS_SETUP = _IOW('U', 4, sizeof(uinput_abs_setup)) = 0x401c5504
    f.ioctl(0x401c_5504, (&buf as *const _) as u64)
        .map(|_| ())
        .with_context(|| format!("UI_ABS_SETUP(code={code})"))
}

/// Trait extension to expose a thin ioctl on `File`.
#[cfg(target_os = "linux")]
trait FileIoctl {
    fn ioctl(&mut self, req: u32, arg: u64) -> std::io::Result<i32>;
}
#[cfg(target_os = "linux")]
impl FileIoctl for File {
    fn ioctl(&mut self, req: u32, _arg: u64) -> std::io::Result<i32> {
        libc_like::IoctlExt::ioctl(self, req, _arg)
    }
}

// ── X11/Wayland enigo (XTEST) injector ──────────────────────────────
// On this host, X11 never attaches the uinput virtual tablet (device
// missing from `xinput list`), so ABS writes succeed but never move the
// cursor. Enigo uses XTEST which is already in the X core pointer tree.

use enigo::{
    Button, Coordinate,
    Direction::{Press, Release},
    Enigo, Key, Keyboard, Mouse, Settings,
};

/// Browser coords are absolute 0..=65535; map onto the primary display.
pub struct EnigoInjector {
    enigo: Mutex<Enigo>,
    display_width: i32,
    display_height: i32,
}

impl EnigoInjector {
    pub fn open() -> Result<Self> {
        let settings = Settings {
            release_keys_when_dropped: true,
            ..Settings::default()
        };
        let enigo = Enigo::new(&settings).map_err(|e| anyhow!("enigo init failed: {e}"))?;
        let (display_width, display_height) = enigo
            .main_display()
            .map_err(|e| anyhow!("enigo main_display failed: {e}"))?;
        tracing::info!(display_width, display_height, "enigo/XTEST injector ready");
        Ok(Self {
            enigo: Mutex::new(enigo),
            display_width: display_width.max(1),
            display_height: display_height.max(1),
        })
    }

    fn scale(&self, v: u32, extent: i32) -> i32 {
        if extent <= 1 {
            return 0;
        }
        let clamped = u64::from(v.min(65535));
        ((clamped * u64::from((extent - 1) as u32)) / 65535) as i32
    }

    pub fn dispatch(&self, ev: &RemoteInputEvent) -> Result<()> {
        let mut enigo = self.enigo.lock().unwrap();
        match ev {
            RemoteInputEvent::MouseMove { x, y } => {
                let tx = self.scale(*x, self.display_width);
                let ty = self.scale(*y, self.display_height);
                enigo
                    .move_mouse(tx, ty, Coordinate::Abs)
                    .map_err(|e| anyhow!("enigo move: {e}"))?;
            }
            RemoteInputEvent::RelativeMouseMove { dx, dy } => {
                enigo
                    .move_mouse(*dx, *dy, Coordinate::Rel)
                    .map_err(|e| anyhow!("enigo rel move: {e}"))?;
            }
            RemoteInputEvent::MouseButton {
                button,
                pressed,
                x,
                y,
            } => {
                if let (Some(px), Some(py)) = (*x, *y) {
                    let tx = self.scale(px, self.display_width);
                    let ty = self.scale(py, self.display_height);
                    enigo
                        .move_mouse(tx, ty, Coordinate::Abs)
                        .map_err(|e| anyhow!("enigo move before click: {e}"))?;
                }
                let btn = match button {
                    InputMouseButton::Left => Button::Left,
                    InputMouseButton::Right => Button::Right,
                    InputMouseButton::Middle => Button::Middle,
                };
                enigo
                    .button(btn, if *pressed { Press } else { Release })
                    .map_err(|e| anyhow!("enigo button: {e}"))?;
            }
            RemoteInputEvent::MouseWheel { dx, dy } => {
                // enigo scroll: positive = up
                let notches = normalize_wheel(*dy);
                if notches != 0 {
                    enigo
                        .scroll(notches, enigo::Axis::Vertical)
                        .map_err(|e| anyhow!("enigo scroll: {e}"))?;
                }
                let h = normalize_wheel(*dx);
                if h != 0 {
                    enigo
                        .scroll(h, enigo::Axis::Horizontal)
                        .map_err(|e| anyhow!("enigo hscroll: {e}"))?;
                }
            }
            RemoteInputEvent::Keyboard { key, pressed } => {
                let Some(k) = map_browser_key(key) else {
                    tracing::debug!(key, "enigo: ignoring unmapped key");
                    return Ok(());
                };
                enigo
                    .key(k, if *pressed { Press } else { Release })
                    .map_err(|e| anyhow!("enigo key: {e}"))?;
            }
            _ => {}
        }
        Ok(())
    }
}

fn map_browser_key(code: &str) -> Option<Key> {
    // KeyboardEvent.code form from browsers.
    if let Some(rest) = code.strip_prefix("Key") {
        if rest.len() == 1 {
            let c = rest.chars().next()?.to_ascii_lowercase();
            if c.is_ascii_alphabetic() {
                return Some(Key::Unicode(c));
            }
        }
    }
    if let Some(rest) = code.strip_prefix("Digit") {
        if rest.len() == 1 {
            let c = rest.chars().next()?;
            if c.is_ascii_digit() {
                return Some(Key::Unicode(c));
            }
        }
    }
    match code {
        "ArrowLeft" | "Left" => Some(Key::LeftArrow),
        "ArrowRight" | "Right" => Some(Key::RightArrow),
        "ArrowUp" | "Up" => Some(Key::UpArrow),
        "ArrowDown" | "Down" => Some(Key::DownArrow),
        "Enter" => Some(Key::Return),
        "Escape" => Some(Key::Escape),
        "Backspace" => Some(Key::Backspace),
        "Tab" => Some(Key::Tab),
        "Space" => Some(Key::Space),
        "Delete" => Some(Key::Delete),
        "Home" => Some(Key::Home),
        "End" => Some(Key::End),
        "PageUp" => Some(Key::PageUp),
        "PageDown" => Some(Key::PageDown),
        "ShiftLeft" | "LeftShift" => Some(Key::LShift),
        "ShiftRight" | "RightShift" => Some(Key::RShift),
        "ControlLeft" | "LeftCtrl" => Some(Key::LControl),
        "ControlRight" | "RightCtrl" => Some(Key::RControl),
        "AltLeft" | "LeftAlt" => Some(Key::Alt),
        "AltRight" | "RightAlt" => Some(Key::Alt),
        "MetaLeft" | "MetaRight" | "OSLeft" | "OSRight" => Some(Key::Meta),
        "CapsLock" => Some(Key::CapsLock),
        "Minus" => Some(Key::Unicode('-')),
        "Equal" => Some(Key::Unicode('=')),
        "BracketLeft" => Some(Key::Unicode('[')),
        "BracketRight" => Some(Key::Unicode(']')),
        "Backslash" => Some(Key::Unicode('\\')),
        "Semicolon" => Some(Key::Unicode(';')),
        "Quote" => Some(Key::Unicode('\'')),
        "Backquote" => Some(Key::Unicode('`')),
        "Comma" => Some(Key::Unicode(',')),
        "Period" => Some(Key::Unicode('.')),
        "Slash" => Some(Key::Unicode('/')),
        "F1" => Some(Key::F1),
        "F2" => Some(Key::F2),
        "F3" => Some(Key::F3),
        "F4" => Some(Key::F4),
        "F5" => Some(Key::F5),
        "F6" => Some(Key::F6),
        "F7" => Some(Key::F7),
        "F8" => Some(Key::F8),
        "F9" => Some(Key::F9),
        "F10" => Some(Key::F10),
        "F11" => Some(Key::F11),
        "F12" => Some(Key::F12),
        s if s.len() == 1 => {
            let c = s.chars().next()?;
            if c.is_ascii() {
                Some(Key::Unicode(c))
            } else {
                None
            }
        }
        _ => None,
    }
}

/// Prefer enigo (XTEST — works on X11 today). Fall back to uinput.
#[allow(clippy::large_enum_variant)] // variants are mutually-exclusive at runtime; boxing would add an extra alloc on the hot path
pub enum HostInputInjector {
    Enigo(EnigoInjector),
    #[cfg(target_os = "linux")]
    Uinput(UinputInjector),
}

impl HostInputInjector {
    #[cfg(target_os = "linux")]
    pub fn open_best() -> Result<Self> {
        match EnigoInjector::open() {
            Ok(e) => Ok(Self::Enigo(e)),
            Err(enigo_err) => {
                tracing::warn!(?enigo_err, "enigo unavailable; trying uinput");
                match UinputInjector::open()? {
                    Some(u) => Ok(Self::Uinput(u)),
                    None => Err(anyhow!("no input injector available (enigo+uinput failed)")),
                }
            }
        }
    }

    #[cfg(not(target_os = "linux"))]
    pub fn open_best() -> Result<Self> {
        Ok(Self::Enigo(EnigoInjector::open()?))
    }

    pub fn dispatch(&self, ev: &RemoteInputEvent) -> Result<()> {
        match self {
            Self::Enigo(e) => e.dispatch(ev),
            #[cfg(target_os = "linux")]
            Self::Uinput(u) => u.dispatch(ev),
        }
    }

    pub fn backend_name(&self) -> &'static str {
        match self {
            Self::Enigo(_) => "enigo",
            #[cfg(target_os = "linux")]
            Self::Uinput(_) => "uinput",
        }
    }
}
