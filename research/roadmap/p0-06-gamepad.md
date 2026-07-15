# P0-6: Gamepad (uinput / ViGEmBus, gilrs)

Status: **complete** (commits `82adfc9`, `4b1afab`; PR https://github.com/MegaPanchamZ/qubox/pull/1). Client: gilrs 0.11.2 → 16-byte `WireGamepadState` over QUIC datagrams. Host: uinput open + Xbox360 event surface (`libc::O_NONBLOCK`, 14 buttons, 6 axes) with `EV_KEY` + `EV_ABS` + `EV_SYN`; `/dev/uinput` write path is code-complete but cannot be runtime-tested on the headless dev box (no `/dev/uinput` group access without sudo). macOS virtual gamepad deferred.
Owner: `crates/qubox-proto` (wire format), `apps/client-cli` (capture), `apps/host-agent` (inject).
Depends on: existing `RemoteInputEvent` (the wire format is an extension), `enigo` 0.6 input injector pattern (per-thread ownership).
Blockers: Windows requires ViGEmBus driver install (one-time admin); Linux requires udev rule for `/dev/uinput` (one-time setup); macOS is a hard problem, defer.

## Goal

Add gamepad support to the existing mouse+keyboard streaming path. The client reads local gamepads with **gilrs**, sends delta-encoded gamepad state over QUIC datagrams (or a reliable stream for control). The host creates a **virtual gamepad** (uinput on Linux, ViGEmBus on Windows) per client pad, and injects the state. Target end-to-end gamepad latency: <30 ms on LAN. Scope v1: buttons, sticks, triggers, d-pad, hot-plug, multi-pad. Scope v2: rumble. Scope v3: motion sensors.

## Research Summary

### Client-side: gilrs (v0.11.2 as of 2024-2026)

`gilrs` is the de-facto Rust cross-platform gamepad library. Backends:

- **Linux**: `/dev/input/js*` (legacy joystick API) + `/dev/input/event*` (evdev). Force feedback via `FF_RUMBLE`.
- **Windows**: XInput + DirectInput (auto-detected).
- **macOS**: IOHIDEventSystemClient.
- **Android**: NDK (via `android-activity`).
- **WASM**: limited support.

```rust
use gilrs::{Gilrs, EventType};

let mut gilrs = Gilrs::new()?;
loop {
    while let Some(ev) = gilrs.next_event() {
        match ev.event {
            EventType::Connected => { /* allocate gamepad_id */ }
            EventType::Disconnected => { /* release gamepad_id */ }
            EventType::ButtonPressed(b, _) => { /* build wire state */ }
            EventType::ButtonReleased(b, _) => { /* build wire state */ }
            EventType::AxisChanged(a, v, _) => { /* build wire state */ }
            _ => {}
        }
    }
    std::thread::sleep(Duration::from_millis(1));
}
```

Latency: 1-5 ms per event cycle (gilrs is event-driven, not polled). `gilrs` caches state per gamepad and only emits events on changes; the wire format is delta-encoded.

Alternatives considered:
- `sdl2` / `sdl3` gamepad: heavier, requires SDL2/SDL3 dep.
- `evdev-sys` / `evdev` crate: Linux-only.
- `usehid-core`: too low-level.

### Host-side: uinput on Linux (via `evdev` crate)

The Linux virtual-gamepad mechanism is **uinput** (`/dev/uinput`). The Rust binding of choice is `evdev::uinput::VirtualDeviceBuilder` (the `evdev` crate). The flow:

1. Open `/dev/uinput`.
2. Declare capabilities: `UI_SET_EVBIT(EV_KEY)`, `UI_SET_KEYBIT(BTN_SOUTH)`, ..., `UI_SET_EVBIT(EV_ABS)`, `UI_SET_ABSBIT(ABS_X)`, ...
3. Set axis ranges (sticks: -32768..32767, triggers: 0..255).
4. `UI_DEV_SETUP` with name, ID, etc.
5. `UI_DEV_CREATE`.
6. Write events: `UI_EV_KEY`, `UI_EV_ABS`, `UI_EV_SYN`.

```rust
use evdev::uinput::VirtualDeviceBuilder;
use evdev::{AttributeSet, Key, AbsoluteAxisType};

let mut keys = AttributeSet::<Key>::new();
keys.insert(Key::BTN_SOUTH); // A
keys.insert(Key::BTN_EAST);  // B
keys.insert(Key::BTN_NORTH); // Y
keys.insert(Key::BTN_WEST);  // X
keys.insert(Key::BTN_TL); keys.insert(Key::BTN_TR);
keys.insert(Key::BTN_SELECT); keys.insert(Key::BTN_START);
keys.insert(Key::BTN_THUMBL); keys.insert(Key::BTN_THUMBR);
keys.insert(Key::BTN_DPAD_UP); keys.insert(Key::BTN_DPAD_DOWN);
keys.insert(Key::BTN_DPAD_LEFT); keys.insert(Key::BTN_DPAD_RIGHT);

let mut abs = AttributeSet::<AbsoluteAxisType>::new();
abs.insert(AbsoluteAxisType::ABS_X);
abs.insert(AbsoluteAxisType::ABS_Y);
abs.insert(AbsoluteAxisType::ABS_RX);
abs.insert(AbsoluteAxisType::ABS_RY);
abs.insert(AbsoluteAxisType::ABS_Z);
abs.insert(AbsoluteAxisType::ABS_RZ);

let dev = VirtualDeviceBuilder::new()?
    .name("Qubox Virtual Gamepad")
    .with_keys(&keys)?
    .with_absolute_axes(&abs)?
    .build()?;
```

**Permissions**: `/dev/uinput` is root-only by default. **Udev rule** (one-time setup, ship with the host-agent installer):

```udev
KERNEL=="uinput", MODE="0660", GROUP="input", OPTIONS+="static_node=uinput"
```

Add the user to the `input` group: `usermod -aG input $USER`. Document in the install instructions. Some distros (Arch) use a `uinput` group instead; the rule should add both.

### Host-side: ViGEmBus on Windows (via `vigem-client` crate)

ViGEmBus is the de-facto Windows virtual gamepad bus (originally by Benjamin Höglinger-Stelzer, now maintained by Nektra). The Rust wrapper is `vigem-client`.

**Install**: ViGEmBus driver must be installed with **admin elevation** once per machine. Many users complain about this; the cleanest UX is a first-run installer step that does the install and reboots if needed. Steam ships ViGEmBus as part of its redistributable. Moonlight/Parsec ship it as part of their installers.

```rust
use vigem_client::{Client, XGamepad, XButtons};

let client = Client::connect()?;
let mut pad = XGamepad::new(TargetId::XBOX360_WIRED)?;
client.plugin(&mut pad)?;

let report = pad.state_mut();
report.buttons = XButtons::A | XButtons::START;
report.thumb_lx = 12345;
report.thumb_ly = -12000;
report.left_trigger = 180;
report.right_trigger = 0;
pad.update(report)?;
```

**Anti-cheat compatibility**: mixed. EAC, BattlEye, Vanguard *may* detect/block ViGEm. The mainstream remote-play ecosystem (Parsec, Moonlight, Steam Remote Play) uses ViGEm and has worked through the major anti-cheats in 2024-2026, but a few specific titles may still block. **Plan a fallback mode** that disables virtual gamepads on anti-cheat-restricted titles; this is opt-in by the user.

### Host-side: macOS (defer)

macOS has no equivalent to uinput or ViGEm. The options are:
- A userspace HID emulation via IOHIDDevice with a fake descriptor (limited; some games ignore it).
- A kernel extension (deprecated since macOS 11; System Integrity Protection blocks unsigned kexts).
- Steam's HID approach: a fake controller via IOHIDInterface.

**Recommendation**: defer macOS to v2 of this feature. Document that macOS hosts have keyboard+mouse only. For Apple Silicon, the user can use a hosted VM (Parallels) or Boot Camp, both of which expose full controller support.

### Wire format (16 bytes per state)

Compact packed struct, sent on change (delta encoding):

```rust
#[repr(C, packed)]
#[derive(Copy, Clone, Default, bytemuck::Pod, bytemuck::Zeroable)]
pub struct WireGamepadState {
    pub gamepad_id: u8,    // 0..=3 (max 4 pads)
    pub flags: u8,         // bit 0: dpad-up, bit 1: dpad-down, bit 2: dpad-left, bit 3: dpad-right
    pub buttons_lo: u8,    // bits 0-7: A,B,X,Y,LB,RB,Select,Start
    pub buttons_hi: u8,    // bits 0-7: L3,R3,Guide,Reserved*5
    pub lt: u8,            // 0..=255
    pub rt: u8,            // 0..=255
    pub lx: i16,           // -32768..=32767
    pub ly: i16,           // -32768..=32767
    pub rx: i16,
    pub ry: i16,
}
```

16 bytes per packet; sent over QUIC datagrams (P0-2) at the change rate (typically 30-200 Hz, lower than 1 kHz). A 60 Hz gamepad at 200 events/sec is 3.2 KB/s — negligible bandwidth.

### Control messages (over the reliable stream)

```rust
#[derive(Serialize, Deserialize)]
pub enum ControlMsg {
    GamepadConnect { id: u8, name: String, kind: PadKind },  // PadKind: Xbox, DualShock4, DualSense, SwitchPro
    GamepadDisconnect { id: u8 },
    GamepadState { state: WireGamepadState },                // delta-encoded; or use datagrams
    GamepadRumble { id: u8, low: u16, high: u16 },
}
```

The `GamepadState` is small enough to send over the control stream, but the latency budget is tighter for the data plane. **Use QUIC datagrams (P0-2) for the state and the reliable control stream for connect/disconnect/rumble.** A reliable stream's retransmit (10-50 ms) on a lost state packet is much worse than a dropped one (the next state will overwrite it).

### Latency budget

- Client poll (gilrs): 1-5 ms
- Wire (QUIC datagrams): 5-20 ms LAN, 20-50 ms WAN
- Host inject (uinput/ViGEm): 1-3 ms
- **Total**: 7-28 ms LAN, 22-58 ms WAN. Competitive for gaming on LAN.

### Rust crate recommendations

- **gilrs** 0.11.2: client-side gamepad.
- **evdev** 0.13+: Linux virtual gamepad (`evdev::uinput::VirtualDeviceBuilder`).
- **vigem-client** 0.3+: Windows virtual gamepad (check the latest on crates.io; API surface has been stable since 2023).
- **uinput** crate: older wrapper, prefer `evdev` for new code.
- **serde** / **bincode** / **bytemuck**: for the wire format and serialization.

### 2024-2026 status

- **gilrs** is actively maintained. 0.11.x is the current stable line. The project is in maintenance mode (no new features), but bug fixes continue.
- **evdev** 0.13+ has a stable `uinput` builder API. The `uinput` module was added in 0.10 and refined through 0.13.
- **vigem-client** 0.3+ is the stable line. The upstream ViGEmBus driver is at 1.21+ as of 2024; check the project page for the latest.
- **Linux kernel**: uinput has been stable since 2.6; no API changes expected.
- **macOS** is the long pole. Steam's IOHIDDevice approach is the only known working path; it's a lot of work and may break with each macOS release.

## Implementation Plan

### Step 1: Wire format

`crates/qubox-proto/src/lib.rs`:
- Add `WireGamepadState` (16 bytes, packed).
- Add `ControlMsg::GamepadConnect`, `GamepadDisconnect`, `GamepadRumble`.
- `RemoteInputEvent::GamepadState(WireGamepadState)` for the data plane (sent over datagrams).

### Step 2: Client-side capture

`apps/client-cli/src/input/gamepad.rs` (new):
- Spawn a `gilrs` polling thread.
- On `Connected`: allocate `gamepad_id` (0-3), send `GamepadConnect` on the control stream.
- On `Disconnected`: send `GamepadDisconnect`.
- On button/axis change: build `WireGamepadState`, send via `MediaDatagramSender` (or a new `GamepadDatagramSender` — they share the QUIC connection).
- Throttle state sends to 500 Hz max (sample the gamepad at 1 kHz, send at 500 Hz if changes).

### Step 3: Host-side injection — Linux

`apps/host-agent/src/input/gamepad_linux.rs` (new, behind `cfg(target_os = "linux")`):
- `pub struct VirtualGamepad { device: evdev::uinput::VirtualDevice, id: u8 }`.
- `pub fn new(id: u8, kind: PadKind) -> Result<Self>`: creates the uinput device, sets axis ranges, returns the handle.
- `pub fn apply(&mut self, state: &WireGamepadState) -> Result<()>`: writes EV_ABS for sticks/triggers, EV_KEY for buttons + dpad, EV_SYN to commit.
- The existing `RemoteInputInjector` pattern: per-thread ownership; this struct is owned by a `bp-gamepad-0` thread.
- The udev rule is shipped as `/etc/udev/rules.d/99-qubox-uinput.rules` in the host-agent installer (DEB / RPM / MSI).

### Step 4: Host-side injection — Windows

`apps/host-agent/src/input/gamepad_windows.rs` (new, behind `cfg(target_os = "windows")`):
- `pub struct VirtualGamepad { pad: vigem_client::XGamepad, id: u8, client: vigem_client::Client }`.
- `pub fn new(id: u8, kind: PadKind) -> Result<Self>`: connects to the ViGEm client, plugins an XGamepad.
- `pub fn apply(&mut self, state: &WireGamepadState) -> Result<()>`: maps wire state to `XButtons` and axis values, calls `pad.update()`.
- The ViGEmBus driver install is handled by the Windows MSI; the installer does the elevation step.

### Step 5: Host-side injection — macOS (defer)

A `gamepad_macos.rs` stub that returns "not implemented". Document in the user manual.

### Step 6: Backend abstraction

`apps/host-agent/src/input/gamepad.rs`:
- `pub trait VirtualGamepadBackend { fn new(id: u8, kind: PadKind) -> Result<Self> where Self: Sized; fn apply(&mut self, state: &WireGamepadState) -> Result<()>; fn name(&self) -> &'static str; }`.
- `#[cfg(target_os = "linux")]` `pub use LinuxBackend as Active;` etc.

### Step 7: Rumble (v2)

- Host: when the virtual pad's rumble state changes (via the OS's input event from the game), read it from the OS (uinput: `FF_RUMBLE` effect, ViGEm: `XGamepad::state().rumble`) and emit `ControlMsg::GamepadRumble` over the control stream.
- Client: receive `GamepadRumble`, call `gilrs::Gamepad::set_rumble(...)` on the local pad.
- Defer to v2; for v1, the wire message type exists but is unused.

### Step 8: Tests

- Unit test: `WireGamepadState` is `Copy` and has size 16 (assertion in test).
- Unit test: mapping from gilrs `Button` to `WireGamepadState` is correct (A → buttons_lo bit 0).
- Integration test: spawn a virtual gamepad, apply a state, verify the OS reports the new state via `/dev/input/event*` (Linux) or `XInputGetState` (Windows, not in our env).
- Soak test: 1 hour of synthetic gamepad events at 200 Hz, verify no memory leak and no dropouts.

## Risks and Open Questions

- **ViGEmBus driver install friction**: users complain about the elevation step. Mitigate with a clear installer message and a "skip ViGEm" option that disables gamepad support.
- **Anti-cheat blocking**: EAC, BattlEye, Vanguard may detect ViGEm. Some users report their accounts flagged. **Add a "use physical gamepad" mode** that asks the user to plug their local controller into the host's USB (no virtual device). This is the bulletproof fallback.
- **uinput permissions**: the udev rule requires a one-time admin step (or root at install time). On minimal container distros, `/dev/uinput` may not exist; the host-agent should detect this and report a clear error.
- **macOS support**: deferred. Document the limitation.
- **Linux kernel module signing** (Secure Boot): `/dev/uinput` works under Secure Boot, but some hardened distros restrict it. The host-agent should detect and report.
- **Hot-plug race**: if the client sends `GamepadDisconnect` but the message is lost, the host has a dangling virtual device. Add a per-pad timeout: if no state for 5 seconds, destroy the virtual device.
- **Multi-gamepad mapping**: 4 gamepads max (our `gamepad_id` is `u8` with 0..=3 range; reserve 4-255 for future). The client must enforce 4-pad max.
- **Linux HID descriptor quirks**: some games (mostly on Steam Deck / Proton) expect a specific device ID (e.g. `vendor=0x045e product=0x028e` for Xbox 360). The `VirtualDeviceBuilder::with_vendor_id(0x045e).with_product_id(0x028e)` call sets this; document the choice.
- **Triggers as buttons vs axes**: on Xbox 360, triggers are buttons with an analog range (0-255). uinput accepts either `ABS_Z`/`ABS_RZ` as analog or `BTN_TR2`/`BTN_TR` as digital; the game sees both. Use `ABS_Z`/`ABS_RZ` for analog fidelity.
- **D-pad as buttons vs hat**: uinput accepts both `BTN_DPAD_*` and `ABS_HAT0X`/`ABS_HAT0Y`. Buttons are more reliable across games; use buttons.
- **Android client**: gilrs supports Android via `android-activity`. Mobile clients are P2-18; defer the gamepad work for that platform.

## References

- gilrs: https://gitlab.com/gilrs-project/gilrs, https://docs.rs/gilrs/, https://crates.io/crates/gilrs
- evdev: https://lib.rs/crates/evdev
- Linux uinput how-to: https://gwilym.dev/2021/02/virtual-joystick-on-linux/
- vigem-client: https://crates.io/crates/vigem-client
- ViGEmBus driver (Nektra): https://github.com/nefarius/ViGEmBus
- Android virtual input with Rust: https://brunodmt.github.io/rust/2018/11/03/android-virtual-input-with-rust.html
- Are we game yet? input ecosystem: https://arewegameyet.rs/ecosystem/input/
- Perplexity research, 2026-07-02: gilrs, evdev uinput, vigem-client, macOS challenges.
- ADR-004 (this repo): virtual gamepad plan (see `research/decisions/ADR-004-virtual-gamepad-uinput-vigembus.md`).
