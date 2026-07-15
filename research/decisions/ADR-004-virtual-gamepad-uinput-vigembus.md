# ADR-004 Gamepad / virtual-device input via uinput + ViGEmBus

## Status

Proposed.

## Context

The current host input pipeline uses `enigo 0.6` exclusively, which on
Linux/X11 maps to XTest synthetic events and on Windows maps to
`SendInput` with synthetic keyboard / mouse events. The 2026 client
captures `winit::event::DeviceEvent::MouseMotion` (raw relative
motion) and `winit::event::WindowEvent::KeyboardInput` /
`WindowEvent::MouseInput` / `DeviceEvent::MouseWheel`, ships them over
the QUIC stream, and the host injects them via `enigo`.

This covers the desktop and most games. Two important gaps remain:

1. **Gamepads / controllers**. The client side has no gamepad support
   today (no `gilrs`, no `RemoteInputEvent::Gamepad` variant). The host
   has no way to inject a gamepad state. This blocks console-style
   games and racing / fighting / sports titles that need a real
   controller.

2. **Anti-cheat / kernel-level input filters**. Some games and most
   anti-cheat systems treat XTest / `SendInput` events as lower
   fidelity than hardware-generated input. They use `RAW_INPUT`,
   `HID` device paths, and other kernel-level signals to decide
   whether input is "real". The current injection is rejected by
   those filters.

Parsec (2024-2025) added gamepad support via ViGEmBus on Windows and
uinput on Linux for this exact reason. Moonlight (the open-source
client for NVIDIA GameStream) does the same.

## Decision

Add a virtual-device input layer on the host (uinput on Linux,
ViGEmBus on Windows) and a corresponding gamepad capture path on the
client (gilrs).

### Wire format

Add a new `RemoteInputEvent::Gamepad` variant:

```rust
RemoteInputEvent::Gamepad {
    gamepad_id: u8,           // 0..3 — index into a virtual device
    state: GamepadState,
}

struct GamepadState {
    buttons: u32,             // bitmask: ABXY / dpad / shoulders / sticks
    left_stick: (i16, i16),   // -32768..32767
    right_stick: (i16, i16),
    left_trigger: u8,         // 0..255
    right_trigger: u8,
}
```

`buttons` is a stable bitmask defined in
`crates/qubox-proto/src/lib.rs` (e.g. `GAMEPAD_BUTTON_A = 1<<0`).
Both sides reference the same constants.

### Client capture

Add `gilrs` (`crates/qubox-input`) on the client. A dedicated
thread polls the controller at 250 Hz (Xbox One / DS4 / Switch Pro
standard), batches state changes (delta-only updates when no state
change, full snapshots every 100 ms to recover from packet loss), and
ships them over QUIC.

The client also uses the winit main thread for hot-plug events
(connected / disconnected) which surface as a `GamepadConnected` /
`GamepadDisconnected` control message over the existing signaling
channel.

### Host injection

The host's `RemoteInputInjector` (and its dedicated `bp-input-*` thread
established in 1ffc754) gets two new backends selected at session
start based on the host OS:

#### Linux: uinput

```rust
// pseudocode
let device = UinputDevice::open()?;
device.set_name("qubox virtual gamepad")?;
device.enable_event(EV_KEY::BTN_SOUTH)?;
device.enable_event(EV_KEY::BTN_EAST)?;
// ... etc for all 16 standard buttons
device.enable_event(EV_ABS::ABS_X)?;   // left stick X
device.enable_event(EV_ABS::ABS_Y)?;
device.enable_event(EV_ABS::ABS_RX)?;  // right stick X
device.enable_event(EV_ABS::ABS_RY)?;
device.enable_event(EV_ABS::ABS_Z)?;   // left trigger
device.enable_event(EV_ABS::ABS_RZ)?;  // right trigger
device.create()?;
```

The existing `enigo` path stays for keyboard + mouse. `uinput` only
handles the gamepad variant.

#### Windows: ViGEmBus

```rust
// pseudocode
let client = ViGEmClient::new()?;
let pad = client.create_x360_pad(X360Slot::SLOT_1)?;
// x360pad.submit_report(&X360Report {
//     buttons: state.buttons,
//     left_trigger: state.left_trigger as u8,
//     right_trigger: state.right_trigger as u8,
//     thumb_lx: state.left_stick.0 as i16,
//     thumb_ly: state.left_stick.1 as i16,
//     thumb_rx: state.right_stick.0 as i16,
//     thumb_ry: state.right_stick.1 as i16,
// })?;
```

Games see a real Xbox 360 controller — bypasses `SendInput` and
anti-cheat filters that reject synthetic mouse / keyboard events.

#### Threading

The existing per-thread `Enigo` pattern (ADR-D, host-agent/src/main.rs)
extends naturally: the `bp-input-*` thread gains a second virtual
device alongside `Enigo`. Both `enigo` and the virtual gamepad backend
live on the same thread (one thread owns all host input devices).

### Cross-platform

- macOS: not in scope for this ADR. macOS hosts are a small fraction
  of the target market and the IOKit / HID APIs add significant
  complexity. The wire format supports it; the impl is a follow-up.
- FreeBSD / other Unixes: not in scope.

## Consequences

### Positive

- Gamepad parity with Parsec / Moonlight. Titles that require a real
  controller (Rocket League, Elden Ring, fighting games, racing
  sims) work.
- Anti-cheat compatibility for games that reject synthetic
  keyboard / mouse.
- Wayland support on Linux: uinput virtual devices work uniformly on
  X11, Wayland, and pure-tty setups; enigo's XTest-only path
  doesn't.
- Cleanly extends the existing per-thread input injector pattern.

### Negative

- New platform dependency on the host: `libudev-dev` + write access
  to `/dev/uinput` on Linux (or membership in the `input` group),
  ViGEmBus driver installed on Windows.
- New client dependency: `gilrs` + a controller to actually test
  against.
- Slightly larger wire format; existing clients that don't speak
  `Gamepad` ignore the new variant via serde's
  `#[serde(other)]` fallback.

### Risks

- uinput requires root or the `uinput` udev rule on the host. Bad
  default: ship the udev rule in the install docs; show a clear
  error if `/dev/uinput` is not writable.
- ViGEmBus requires a kernel-mode driver install on Windows. The
  qubox-host-agent installer should bundle the driver
  signing key. (Parsec does this.)
- Controller hot-plug is tricky: if the client's controller
  disconnects mid-game, the host's virtual device should emit a
  neutral state (all axes centered, no buttons pressed) so the game
  doesn't see a "stuck" controller.

## Alternatives considered

### A. Pure software / no gamepad

Rejected. Console-style games and many racing / fighting titles
require a real controller. The current state blocks a meaningful
slice of remote-gaming use cases.

### B. Use SDL2's gamepad subsystem on both sides

Considered. SDL2 is widely used but adds a heavy C dependency on
both client and host. Stick with `gilrs` (client) and the native
virtual-device APIs (host).

### C. Skip ViGEmBus / use `SendInput` with `INPUT_HARDWARE` flag

Considered. The `INPUT_HARDWARE` flag claims hardware origin but
is documented as not actually changing the event source for the
receiving app. Most games still treat it as synthetic. Use
ViGEmBus for real gamepad support.
