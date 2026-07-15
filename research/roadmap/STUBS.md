# Stub Inventory

Generated 2026-07-06. Every place in the codebase where a real implementation is replaced by a placeholder, indexed by file:line with the real target and the trigger that surfaces it. Excludes `#[cfg(test)]` blocks and intentional no-ops (`// no-op stub … releases the most-recently-opened device` in `decoder_hw.rs` — that's a test stub, not production).

**Counts**: 6 backend stubs (`qubaix-display`), 1 pen platform-stub module + 1 windows pen module, 1 mic virtual-source stub, ~12 isolated `NotSupported` / `FeatureDisabled` / no-op methods inside otherwise-real modules, and a couple of fallback paths the user-facing code routes to when a real backend is missing.

---

## 1. Display backend stubs (compile-only)

The display crate's `lib.rs:detect_backend` / `display_manager` return one of these depending on `cfg(target_os)` + feature. All three non-X11 backends return `CaptureError::NotSupported` / `DisplayError::NotSupported` for every method.

### 1.1 `crates/qubaix-display/src/dxgi/mod.rs` (106 LOC) — **Windows DXGI**
- Methods: `enumerate_displays`, `open_capture`, `set_display_state`, `enumerate_virtual_displays`, `create_virtual_display`, `destroy_virtual_display`, `move_window_to_display` → all `NotSupported("DxgiBackend not yet implemented; use X11RandrBackend on Linux or ScreenCaptureKitBackend on macos")`.
- Lines: 34, 54, 63, 73, 83, 92, 98.
- **Real target**: `IDXGIOutputDuplication::AcquireNextFrame` → `ID3D11Texture2D` → NVENC. + `IddCx` indirect display for virtual displays (or DDC/CI per revised TODO §B.3).
- **Current bypass**: host-agent runs `gdigrab` ffmpeg subprocess on Windows (`media_config_from_runtime` in `apps/qubaix-host-agent/src/main.rs:1281`). Display crate is never used for Windows capture.

### 1.2 `crates/qubaix-display/src/screencapturekit/mod.rs` (100 LOC) — **macOS**
- 7 methods → `NotSupported("ScreenCaptureKitBackend not yet implemented; use X11RandrBackend on Linux")`. Lines: 28, 48, 57, 67, 77, 86, 92.
- **Real target**: `SCShareableContent::current().await.displays` + `SCStreamConfiguration` + `SCStream` delegate. Virtual displays: `CGVirtualDisplay` (entitlement-gated).
- **Current bypass**: macOS host unsupported entirely (`media_config_from_runtime` errors with `"native QUIC host capture is not implemented for {:?} yet"` at `main.rs:1289`).

### 1.3 `crates/qubaix-display/src/pipewire/mod.rs` (100 LOC) — **Wayland/PipeWire**
- 7 methods → `NotSupported("PipeWirePortalBackend not yet implemented; use X11RandrBackend on X11 sessions")`. Lines: 28, 48, 57, 67, 77, 86, 92.
- **Real target**: `ashpd` ScreenCast portal + `pipewire` stream import via `libspa`. Virtual displays via `wlr-output-management`.
- **Current bypass**: Wayland session triggers `NotSupported` at `lib.rs:60-65`; the daemon's `--linux-capture pipewire` flag goes through `ffmpeg`'s PipeWire source instead of this crate (`apps/qubaix-host-agent/src/main.rs:366-368`).

### 1.4 Stubs *inside* the working X11 backend (`crates/qubaix-display/src/x11/`)

These ride on top of the working X11/RandR pipeline and surface as `NotSupported` only when the working path can't satisfy the call.

| File:line | Symbol | What's stubbed | Trigger |
|---|---|---|---|
| `x11/window.rs:32` | `WindowOps::move_to_display` | x11rb `_NET_WM_MOVERESIZE` IPC | Comment: "Phase C: return NotSupported to trigger the xrandr fallback in manager.rs" |
| `x11/manager.rs:331` | `X11RandrDisplayManager::set_display_state(Blanked)` | DPMS / vkms blanking | Phase A → "Phase C: replaces Phase A stubs with real vkms + xrandr + DPMS logic" (`x11/manager.rs:29`) |
| `x11/manager.rs:318` | `set_display_state` for some branches | "OS-driven no-op" | Test environment without DPMS |
| `x11/manager.rs:229-235` | `blank_overlay_fallback` closure | no-op + warn log | Selected when `--privacy-mode blank-overlay` and vkms unavailable |
| `x11/mod.rs:10-12` | Virtual display + privacy + window move | Whole-capability stubs | All three marked "Phase C" |

---

## 2. Pen stubs

### 2.1 `crates/qubaix-pen/src/platform.rs` (169 LOC) — **dispatch stubs**
- `StubCapture` (`platform.rs:41-75`): `enumerate_devices` returns supplied list (empty by default), `start` returns a closed `crossbeam_channel::Receiver`. Real pen source never wired.
- `StubInjector` (`platform.rs:78-99`): every `inject` returns `Err(FeatureDisabled("uinput"))`.
- Constructors: `stub_capture()` (`:104`), `stub_injector()` (`:108`).
- Tag enum: `CurrentPlatformPen::Stub` (`platform.rs:24`).
- Trigger: `--no-default-features` or any pen feature disabled.

### 2.2 `crates/qubaix-pen/src/linux.rs` (135 LOC) — **partial stub**
- `linux.rs:37`: `enumerate_devices` → `Err(FeatureDisabled("libinput"))` if feature disabled.
- `linux.rs:67`: `start` (capture) → `Err(FeatureDisabled("libinput"))` if feature disabled.
- `linux.rs:89`: another capture path → same error.
- `linux.rs:121, 140`: `inject` → `Err(FeatureDisabled("uinput"))` if feature disabled.
- **Real (when features on)**: `linux.rs:134` shows `tracing::trace!("uinput inject (stub)")` — the *uinput path* logs but does not emit uinput events (libinput feature stubbed).
- Comment at `linux.rs:79`: "capture path fall back to a no-op until the upstream lands".

### 2.3 `crates/qubaix-pen/src/windows.rs` (123 LOC) — **entire file is a stub**
- Header: `//! Windows pen capture via WM_POINTER and injection via WinTab (stubs).`
- `windows.rs:20`: `StubCapture` always succeeds; returns `Ok(())` but emits nothing.
- `windows.rs:39`: capture path deferred: "by `POINTER_DEVICE_PRODUCT` strings; deferred to v2".
- `windows.rs:54`: `StubInjector` — every inject returns `FeatureDisabled`.
- **Real target**: `WM_POINTER` messages via `RegisterPointerInputTarget`; injection via WinTab `WTEnable` / `WTPacket`. Doc says no Windows CI runner.

### 2.4 `apps/qubaix-host-agent/src/main.rs:820-869` — **pen datagram loop stub**
- "For v0.1.0 the injector is a stub on most platforms; the loop correctly parses discriminator 0x50 packets and logs the decode status so the scaffolding is tested." (`main.rs:817-819`)
- Only real impl: Linux `qubaix_pen::linux::UinputInjector::new` (line 831). Windows/macOS paths log "not supported".

### 2.5 Client-side pen capture stub — `apps/qubaix-client-cli/src/main.rs:1384-1390`
- "pen capture started (stub — no events until platform APIs land)" — only logs.

---

## 3. Mic virtual-source stub

### 3.1 `crates/qubaix-mic/src/platform/mod.rs` (135 LOC)
- Header: `//! On Linux the v1 implementation is a stub… Windows and macOS implementations are also stubs in v1.`
- **Linux** (`linux.rs:104-135` inner module):
  - `pipewire_available()` (`platform/mod.rs:132-134`) **hardcoded `false`** — the whole branch never creates a real PipeWire virtual source.
  - `create_virtual_source` (`platform/mod.rs:110-119`) returns `Err("pipewire library not available at runtime")` for every call.
  - `push_to_virtual_source` (`platform/mod.rs:121-126`) drops the samples silently when no source is ready.
- **Windows / macOS** (`platform/mod.rs:64-76`): always `device_created: false`, debug-logs "not implemented on this platform".
- **Real target**: PipeWire `factory.make("api.alsa.pcm.source", …)` + `spa_hook` listener, or `pactl load-module module-pipe-source` over D-Bus. `libspa = "0.10"` is already in workspace deps.
- **Client-visible**: `MicConfigAck::virtual_device_ok` reflects `device_created`. Mic audio is still encoded + sent; only the local-app routing is stubbed.

---

## 4. Privacy fallback stubs

### 4.1 Host-side blank overlay manager — `apps/qubaix-host-agent/src/privacy.rs`
- Header (`privacy.rs:1`): `//! Privacy mode: BlankOverlayManager host-side stub.`
- Sends `ControlMsg::BlankOverlay` to client (real). On host with no control channel (`privacy.rs:71-74`), only logs.
- **Real target**: `vkms` modprobe path is the real Linux implementation; DDC/CI is the real Windows implementation. Currently both paths route through this client-side overlay as fallback.

### 4.2 Client-side overlay — `apps/qubaix-client-cli/src/blank_overlay.rs`
- Real: creates a fullscreen `minifb::Window`. Not a stub — listed here for completeness because it's the *fallback* real implementation behind `privacy.rs`.

### 4.3 Display manager fallback — `crates/qubaix-display/src/x11/manager.rs:229-235`
- `blank_overlay_fallback`: no-op with a `tracing::warn!`; the actual blanking is on the client. Triggered when privacy mode is requested but the platform path is unavailable.

---

## 5. Transport / TURN stubs

### 5.1 `crates/qubaix-transport/src/lib.rs:970-977` — **fallback chain**
- ```
  /// Fallback connection chain: direct QUIC → TURN-over-QUIC → TURN/TCP (stubbed).
  …
  /// 3. TURN/TCP (stubbed — returns an error).
  ```
- Step 3 is a stub: returns an error rather than implementing TCP TURN. Documented in the doc comment.

### 5.2 `crates/qubaix-transport/src/turn.rs:1-9` — **turn client not yet wired**
- ```
  //! Q1 finding: quinn 0.11.9 exposes `Endpoint::new_with_abstract_socket` which
  //! accepts `Arc<dyn AsyncUdpSocket>`. Task 2 should implement the trait on a
  //! wrapper around `TurnClient` instead of the loopback UDP proxy fallback.
  ```
- `TurnClient` (1528 LOC) implements the STUN/TURN codec but does not yet impl `AsyncUdpSocket`. The "fallback" referenced is the loopback UDP proxy, which itself isn't in this file — see `TurnClient` impl block for the TODO.

### 5.3 `crates/qubaix-transport/src/turn.rs:282, 478` — STUN encode placeholders
- `buf.extend_from_slice(&[0u8; 2]); // placeholder length` and "For encoding we use zeros as placeholder; caller must fix up". These are intentional write-then-patch buffers, not production stubs, but worth flagging.

---

## 6. Gamepad stubs

### 6.1 `apps/qubaix-host-agent/src/gamepad.rs:12-14`
- `//! - **Windows**: ViGEmBus client (deferred — not in scope on this ...`
- `//! - **macOS**: deferred per the spec; HID post-event hook on the …`
- The file is real for Linux (uinput virtual Xbox360 surface); Windows/macOS arms are stubs gated by `#[cfg]` per `apps/qubaix-host-agent/Cargo.toml:32-35` comment.

### 6.2 `apps/qubaix-host-agent/src/gamepad.rs:67` — non-Linux handle
- "On non-Linux platforms, returns a no-op handle and the …" — capture loop exits immediately.

---

## 7. Daemon IPC stubs (intentional 501)

### 7.1 `apps/qubaix-daemon/src/ipc.rs:1461-1493` — `ipc_stub_returns_501`
- Not a production stub — these are **deliberately unimplemented IPC request kinds** that the daemon returns `IpcResponse::Error { code: 501 }` for. The list (`stubs: Vec<IpcRequest>` at `ipc.rs:1474`) and the test that verifies each one returns 501 are part of the protocol surface, not bugs.

---

## 8. Client GUI legacy stubs (preserved imports)

### 8.1 `apps/qubaix-client-gui/src-tauri/src/lib.rs:3, 26, 250-252`
- `//! This module replaces the original client_gui stub with a …`
- `//! stub so the existing qubaix_client_cli::start_session import keeps …`
- `/// Legacy in-process stub preserved for the original import. The …`
- Preserved shims so the GUI compiles against the CLI binary's `start_session`. Not blocking work; can be removed when the Tauri commands replace the CLI shim.

---

## 9. Stats overlay stub (`apps/qubaix-client-cli/src/stats_overlay.rs:157`)

- "today the helper is a no-op stub that flips …" — a no-op render hook on the CPU path; the GPU path via `wgpu_glyph` is the real one. Listed for completeness; not a true stub, more of a unimplemented shortcut.

---

## 10. `decoder_hw.rs` test-only stubs

`apps/qubaix-client-cli/src/decoder_hw.rs:621, 624, 880, 886, 899` — labelled `SAFETY: stub` / `stub FFI returns -1 unconditionally`. These are the unit-test surfaces for the `av_buffer_unref` / `av_hwframe_ctx_alloc` paths and don't appear in production builds. Excluded from the real count but mentioned so they're not mistaken for missing impls.

---

## Summary table

| Stub | File:line | Real target | Bypass today |
|---|---|---|---|
| DXGI backend | `crates/qubaix-display/src/dxgi/mod.rs:34-98` | `IDXGIOutputDuplication` | ffmpeg `gdigrab` subprocess |
| ScreenCaptureKit backend | `crates/qubaix-display/src/screencapturekit/mod.rs:28-92` | `SCStream` | macOS host rejected at runtime |
| PipeWire portal backend | `crates/qubaix-display/src/pipewire/mod.rs:28-92` | `ashpd` + `pipewire` | ffmpeg PipeWire source |
| X11 window-move | `crates/qubaix-display/src/x11/window.rs:32` | `_NET_WM_MOVERESIZE` | xrandr fallback |
| X11 set_display_state(Blanked) | `crates/qubaix-display/src/x11/manager.rs:331` | DPMS / vkms | BlankOverlay (client) |
| Pen dispatch stubs | `crates/qubaix-pen/src/platform.rs:41-110` | per-platform impls | linux uinput when features on |
| Pen Linux (uinput inject) | `crates/qubaix-pen/src/linux.rs:121-140` | real uinput events | log-only trace |
| Pen Windows (entire file) | `crates/qubaix-pen/src/windows.rs:1-123` | WM_POINTER + WinTab | always-stubbed |
| Pen host datagram loop | `apps/qubaix-host-agent/src/main.rs:820-869` | real injection | log-only on non-Linux |
| Pen client capture | `apps/qubaix-client-cli/src/main.rs:1384-1390` | platform APIs | log-only |
| Mic virtual source (Linux) | `crates/qubaix-mic/src/platform/mod.rs:104-135` | libspa / `pactl` | always `device_created=false` |
| Mic virtual source (Win/Mac) | `crates/qubaix-mic/src/platform/mod.rs:64-76` | per-OS | always `device_created=false` |
| BlankOverlay fallback | `apps/qubaix-host-agent/src/privacy.rs` (whole file) | vkms / DDC/CI / CGVirtualDisplay | client-side minifb overlay |
| TURN `AsyncUdpSocket` impl | `crates/qubaix-transport/src/turn.rs:1-9` | `quinn::Endpoint::new_with_abstract_socket` wrapper | loopback UDP proxy fallback |
| TURN/TCP fallback | `crates/qubaix-transport/src/lib.rs:970-977` | RFC 6062 TCP TURN | returns error |
| Gamepad Win/Mac | `apps/qubaix-host-agent/src/gamepad.rs:12-14, 67` | ViGEmBus / IOHIDDevice | no-op handle |
| GUI legacy stub | `apps/qubaix-client-gui/src-tauri/src/lib.rs:3-26, 250-252` | Tauri commands replacing CLI shim | preserved import shim |
| Stats overlay no-op | `apps/qubaix-client-cli/src/stats_overlay.rs:157` | full second render pass | wgpu_glyph GPU path is the real one |
| IPC 501 stubs (intentional) | `apps/qubaix-daemon/src/ipc.rs:1461-1493` | unimplemented request kinds | `IpcResponse::Error { code: 501 }` |
| decoder_hw test stubs | `apps/qubaix-client-cli/src/decoder_hw.rs:621, 624, 880, 886, 899` | test-only FFI mocks | not in production |