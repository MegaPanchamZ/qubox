# TODO — Real-State Audit (delta vs the revised `TODO.md`)

Generated 2026-07-06. Method: walked `apps/`, `crates/`, `ops/` and every `Cargo.toml`. Anything below is grounded in a concrete file path. Where the revised TODO is wrong, I say so explicitly.

**TL;DR**: ~60% of the revised TODO is already implemented in the initial commit. The revised doc was written against `STATUS.md` (research state) and not against the actual tree. Significant deltas:

- 5 P1 items marked "research / must-do" are **shipped** (P1-7, P1-9, P1-10, P1-13, P1-12 partial).
- 1 P0 item marked "scaffold" is **done** (P0-5 wgpu Mailbox).
- Several stated decisions **contradict the actual deps** (`tarpc` vs `interprocess`, vendored vs shared ffmpeg-next, DDC/CI vs overlay, Sway vs Xephyr, etc.).
- The "macOS host dropped" line is half-true: abstractions kept, implementations stubbed.

---

## A. P0 carry-over — actual state

### P0-3 HW decode — `apps/qubaix-client-cli/src/decoder_hw.rs`
- **Code says**: full state machine (`Init → Detecting → HwActive / SwFallback → ShuttingDown → Stopped`) at `decoder_hw.rs:36-51`. Subprocess fallback is the documented fallback; HW path is per-backend driven via `av_hwdevice_ctx_create` + `get_format`. 906 LOC.
- **Cargo says** (`apps/qubaix-client-cli/Cargo.toml:42`): `ffmpeg-next = "8.1"`, `optional = true`, runtime requires **system FFmpeg ≥ 5.0** at runtime. **Not vendored.**
- **TODO claims**: "Use vendored ffmpeg-next via `build` feature". **Wrong** — code uses shared linkage with `hw-decode` feature gate.
- **Real gap**: requires system libav\* at runtime; needs `libclang-*-dev` only if you flip to vendored. Zero-copy HW → wgpu import (DXGI NT handle / dmabuf) is not yet wired — the code does `sws_scale` → BGRA → `queue.write_texture` (download path).

### P0-5 wgpu Mailbox swapchain
- **Code says** (`apps/qubaix-client-cli/src/render_wgpu.rs`): **already prefers `PresentMode::Mailbox`, falls back to `Fifo`** when the surface doesn't support it. `frame_pacing.rs` is the `ControlFlow::WaitUntil` driver.
- **TODO says**: "Replace softbuffer present with wgpu swapchain PresentMode::Mailbox (Fast VSync) to decouple decode from display refresh. Fallback to Fifo." → **Already done.** `softbuffer` remains in deps as a fallback renderer (`--renderer minifb`), not the wgpu path.
- **Real gap**: zero — drop this item.

### P0-4 NVENC runtime bitrate + FEC
- **Code says** (`apps/qubaix-host-agent/src/rate_control.rs`): GCC controller operates on the **subprocess restart** path (`-b:v` arg). No `NV_ENC_PIC_PARAMS` / `set_bitrate` wired.
- **No FEC**: `reed-solomon-erasure` is **not** in `Cargo.lock`; no parity code anywhere.
- **TODO claims**: "Use NV_ENC_PIC_PARAMS … Implement reed-solomon-erasure dynamically scaled (5-10%)". Both are net-new.

---

## B. P1 — actual state (the big surprise)

### B.1 P1-13 daemon — `apps/qubaix-daemon/` — **DONE, not "research"**
Full module split already shipped:
- `service.rs` (entry), `service_scm.rs` (Windows SCM), `notify.rs` (sd_notify), `pidfile.rs`, `socket_activation.rs`, `state.rs` (redb), `tuf.rs` (tough), `ipc.rs` (1661 LOC, custom framed protocol), `subprocess.rs`.
- Deps confirmed in `apps/qubaix-daemon/Cargo.toml`:
  - Linux: `systemd = "0.10"` (optional, on by default), `nix` 0.29.
  - Windows: `windows-service = "0.8"`, `windows = "0.58"` (`Win32_System_Pipes`, `Win32_Security`, `Win32_Security_Authorization`).
  - State: `redb = "4.1"`.
  - TUF: `tough = "0.17"`, `http` only, `reqwest = "0.12"` (rustls).
- `cargo deb` + `cargo rpm` metadata wired (units `qubaix.service` + `qubaix.socket`).
- IPC protocol: custom 20-byte framed `bincode` over Unix socket (`SO_PEERCRED` auth) / Windows named pipe (DACL auth). Magic `0xB0_1A_1C_BE`, version `0x0001`. Spec at `apps/qubaix-daemon/src/ipc.rs:5-34`.
- **TODO says**: "Use `tarpc` over Unix domain socket (Linux) and Named Pipe (Windows)". **Wrong** — actual code uses `interprocess = "2.4"` (feature `tokio`) plus its own framed protocol on `tokio::net::{UnixListener, UnixStream}` / `interprocess::local_socket::tokio`. No tarpc anywhere.

### B.2 P1-11 TURN — partially shipped
- `crates/qubaix-transport/src/turn.rs` (1528 LOC): hand-rolled STUN/TURN codec, RFC 8656 subset, `TurnClient` wrapper intended to impl `quinn::udp::AsyncUdpSocket` per `// Q1 finding: quinn 0.11.9 exposes Endpoint::new_with_abstract_socket`.
- `apps/qubaix-signaling-server/src/turn.rs` (317 LOC): short-term HMAC credential issuer (RFC 5389), env-driven via `QUBOX_TURN_{SECRET,URLS,TTL_SECS,SECRET_PREVIOUS}`.
- **Real gap**: `TurnClient` wrapper doesn't yet impl `AsyncUdpSocket` (per its own `Q1 finding` comment). QUIC-over-TURN path still falls back to the loopback UDP proxy. Need a coturn-specific config snippet in `ops/coturn/` enforcing `no-tcp-relay` + DTLS only.

### B.3 P1-8 privacy mode — `apps/qubaix-host-agent/src/privacy.rs`
- **Code says**: TWO modes wired:
  1. `--privacy-mode vkms` → modprobe + bind a CRTC (Linux only).
  2. `--privacy-mode blank-overlay` (default fallback) → host sends `ControlMsg::BlankOverlay { show, display_id }` over a control stream → client renders a fullscreen **minifb black window** (`apps/qubaix-client-cli/src/blank_overlay.rs`).
- vkms fallback comment: `"BlankOverlayManager: fallback path for privacy mode when vkms is unavailable (no sudo, Secure Boot)."`
- **TODO says**: "DDC/CI via SetVCPFeature code 0xD6 to force monitor backlight off/standby". **Not in code** — no DDC/CI crate (`ddc`/`ddc-hi`) in `Cargo.lock`. `SetVCPFeature` does not appear anywhere. The current fallback is the **client-side overlay window**, not DDC.
- **TODO says**: "WS_EX_TOPMOST | WS_EX_TOOLWINDOW overlay window spanning all bounds". Not implemented on Windows — uses the same client-side `minifb` overlay as Linux.
- **Real gaps**:
  1. DDC/CI backlight kill on Windows is net-new work.
  2. `crates/qubaix-display/src/screencapturekit/mod.rs` is a **compile-only stub** returning `NotSupported` for everything. macOS privacy is currently `NotSupported`.
  3. `crates/qubaix-display/src/pipewire/mod.rs` is also a **compile-only stub**. Wayland privacy depends on compositor (ext-image-copy-capture / xdg-shell) per stub comments.

### B.4 P1-7 multi-monitor — `apps/qubaix-host-agent/src/capture_orchestrator.rs` — **DONE, not "research"**
- 800 LOC: `CaptureOrchestrator` with `start_single_stream`, `start_multi_display`, `start_all_displays`, `subscribe`, `unsubscribe`, `wait_for_all`, per-display `DisplayPipeline` (session + ffmpeg subprocess + encoder task).
- One ffmpeg subprocess per display: `x11grab -framerate -video_size -i :99+X,Y -vf scale=WxH -c:v <enc> … -f h264 pipe:1` per display (`capture_orchestrator.rs:236-280`).
- Per-display QUIC uni-stream: `open_media_sender` per display, stream_id bits assigned.
- Two E2E tests in-tree at `capture_orchestrator.rs:531-656` and `662-799` — both gated on `DISPLAY=:99` (Xephyr). A second e2e is in `apps/qubaix-host-agent/tests/multi_display_e2e.rs`.
- **TODO says**: "Rip out X11/RandR. Use PipeWire (via the `ashpd` crate for xdg-desktop-portal integration) for Wayland-native capture". **Wrong on both counts**:
  1. X11/RandR is the working Linux backend. `crates/qubaix-display/src/x11/` has `capture.rs`, `coords.rs`, `enumerate.rs`, `manager.rs`, `window.rs` — fully implemented.
  2. `ashpd` is **not** in `Cargo.lock`. The `pipewire` feature flag exists but the stub returns `NotSupported`.
- **TODO says**: "Windows: DXGI Output Duplication … pass DXGI textures directly to NVENC (zero-copy)". **Not implemented.** `crates/qubaix-display/src/dxgi/mod.rs` exists but matches the stub pattern (returns `NotSupported`); real capture is via ffmpeg subprocess `gdigrab`.
- **Real gaps**: Wayland/PipeWire portal capture, DXGI zero-copy to NVENC. The X11 path is the production Linux backend today.

### B.5 P1-12 stats overlay — `apps/qubaix-client-cli/src/stats_overlay.rs` (961 LOC)
- **Code says**: TWO render paths wired, controlled by `--renderer` flag:
  1. **Software** (`minifb` fallback): CPU paint into the `&mut [u32]` buffer; ships today.
  2. **GPU** (`wgpu_glyph`): `GlyphRenderer` queues into `wgpu::GlyphBrush`, renders into the same swapchain as `WgpuRenderer`. Hotkey: `Ctrl+Alt+S`.
- Telemetry: subscribes to `TelemetrySnapshot` + `OverlayRenderData`. Transport `Stats` events already emitted by `qubaix-transport`.
- **TODO says**: "Use `egui` via `egui-wgpu`". **Wrong** — `egui` and `egui-wgpu` are **not** in `Cargo.lock`. Actual stack: `wgpu_glyph` + `glyph_brush`.
- **Real gaps**: small — e.g. capture-to-PNG for share.

### B.6 P1-9 clipboard — `crates/qubaix-clipboard/` — **DONE, not "research"**
- 262 LOC `lib.rs` + per-platform files for **linux, macos, windows** (`platform/{linux,macos,windows}.rs`). All three platforms have files (macos not dropped).
- `ClipboardWatcher` polls at 250 ms, hashes with **blake3**, emits `ControlMsg::ClipboardChanged` only on hash delta.
- `ClipboardApplier` applies via `arboard = "3.6"` (workspace dep).
- Wired into host-agent: `--clipboard-sync {off,host-to-client,client-to-host,both}` + `--clipboard-formats {text,image,both}` (`main.rs:152-159`).
- `crates/qubaix-clipboard/src/platform/macos.rs` exists with `read_snapshot_inner` / `apply_payload_inner` — **contradicts TODO's "macOS dropped"**.

### B.7 P1-10 mic — `crates/qubaix-mic/` — **DONE, not "research"**
- `lib.rs` + `pipeline.rs` + `reference.rs` + `ring.rs` + `platform/mod.rs`.
- Stack: `cpal = "0.17"` capture → `opus = "0.3"` encode → `webrtc-audio-processing = "0.3"` (bundled feature) → QUIC datagram.
- AEC reference signal via host loopback (`reference.rs`).
- Wired into host-agent: `mic_virtual_source_name`, `forward_audio_chunks`, `open_host_audio_capture`, `MicStart` control message creates `VirtualMicDevice`.

---

## C. P2 — actual state

### P2-14 HDR
- `crates/qubai-display` capability flags exist: `BackendCapabilities { supports_hdr, supports_scrgb }`.
- Host-agent `--advertise-hdr` flag emits `DisplayCapabilities { hdr_static_metadata }` at session start.
- **No actual HDR capture** — DXGI / SCKit stubs, no PQ pixel-format present path.

### P2-15 pen/tablet — `crates/qubaix-pen/` — **partial**
- `lib.rs` explicitly states macOS deferred per ADR-010 §14.
- `linux.rs`: `UinputInjector` (real). Capture: libinput (feature-gated).
- `windows.rs`: WM_POINTER capture + WinTab injection — **stubs** (per `lib.rs:26-28` table).
- `coalesce.rs`: rate-limits 240 Hz → 1 kHz receive path.
- Host-agent `--pen-virtual-device-name` flag wires the `0x50` pen datagram discriminator (`main.rs:172-178, 820-869`).

### P2-18 mobile/web
- **Not started**. No `wgpu` web target, no `wasm-bindgen`, no Cronet. `qubai-client-gui` is Tauri (desktop).

### P2-19 signed binaries
- `ops/tuf/` exists (key generation scripts), no Authenticode / notarytool / GPG / cosign in `ops/`.

### P2-17 cross-platform host — **mostly false on macOS**
- `crates/qubaix-display/src/{x11,dxgi,pipewire,screencapturekit}/` all exist, but **x11 is the only one that's not a stub**.
- macOS path (`screencapturekit/mod.rs`): 100 LOC, all methods return `NotSupported`.

---

## D. Cross-cutting — actual state vs TODO

| TODO claim | Actual state |
|------------|--------------|
| "Headless E2E → Sway headless (`sway --headless`), use `wtype` for input" | Code is on **Xephyr :99**. Two E2E suites gated on `DISPLAY=:99`/`":99.0"` (`capture_orchestrator.rs:533, 665`; `tests/multi_display_e2e.rs`; `tests/privacy_e2e.rs`). No `sway`/`wtype` references. |
| "IPC: `tarpc` over Unix socket / Named Pipe" | **`interprocess = "2.4"` + custom framed bincode** on `tokio::net::{UnixListener, UnixStream}` / `interprocess::local_socket::tokio`. No tarpc. |
| "State: `redb`" | Confirmed (`redb = "4.1"`). |
| "Auto-update: `tough`" | Confirmed (`tough = "0.17"`, http-only). |
| "Daemons run as SYSTEM/root for TUF updates" | Linux: systemd user service by default (`qubaix.service`). No privileged TUF install path coded. |
| "render HUD as a second render pass directly on top of the wgpu surface (egui-wgpu)" | `wgpu_glyph` + `glyph_brush`. No egui dep. |
| "P2-16 4K144: AV1 10-bit encode" | No AV1 encode path; H.264 only ("native QUIC media bridge only supports H.264 right now" — `host-agent/src/main.rs:873-878`). |
| "P1-9 clipboard: text/small images via QUIC datagram; large file via reliable stream" | All clipboard traffic is on the **control stream** (`ControlMsg::ClipboardChanged`). No file transfer path; no separate "blob" channel. |
| "macOS host dropped, IddCx driver dropped" | True for driver; **abstractions are kept**. `crates/qubaix-display/src/screencapturekit/` + `crates/qubaix-clipboard/src/platform/macos.rs` + `crates/qubaix-mic/src/platform/` exist as stubs. Pen is the only crate that explicitly removed macOS (`#[cfg_attr(macos, allow(dead_code))]`). |

---

## E. What's actually still open (real TODO)

Re-derived from code, in priority order. Anything not listed here is already done or stubbed.

### P0 (must finish before 1.0)
1. **P0-3 zero-copy HW → wgpu** — replace `sws_scale → BGRA → write_texture` (download path) with native handle import (DXGI NT shared handle via `ID3D12Device::OpenSharedHandle` on Windows; `VK_KHR_external_memory_fd` dmabuf on Linux; `CVPixelBuffer` → `wgpu::Texture` on macOS).
2. **P0-3 runtime decision** — keep system FFmpeg (current) or flip to vendored `build` feature (LGPL/GPL obligation). Code is on shared; pick one and document.
3. **P0-4 NVENC runtime bitrate** — `NV_ENC_PIC_PARAMS::encodePicParams.bitrate` or `nvEncSetBitrate` API, no subprocess restart. ~150 LOC behind a `dyn HwEncoderControl` trait.
4. **P0-4 FEC** — `reed-solomon-erasure` over a sliding window; scale 5–10 % dynamically off QUIC loss telemetry. Gated on whether 1% XOR parity proves insufficient in WAN traces (still TBD).

### P1 (must-do for 1.0)
1. **macOS path is a stub** (`screencapturekit`, `pipewire`, parts of `dxgi`). Pick: implement or formally mark as out-of-scope in README + ADRs.
2. **TURN: impl `AsyncUdpSocket` on `TurnClient`** (`crates/qubaix-transport/src/turn.rs:1-9` self-identified Q1 task). Wire to `quinn::Endpoint::new_with_abstract_socket`. Coturn deploy runbook in `ops/coturn/` should mandate `no-tcp-relay` + DTLS only (current is the HMAC-credential side; the server-side config isn't in the tree).
3. **DDC/CI backlight kill for Windows privacy** — `ddc-hi` crate + `SetVCPFeature(0xD6)`. ~80 LOC. Pair with `SendInput` loop guard so synthetic input doesn't wake the panel.
4. **DXGI zero-capture → NVENC** — replace `gdigrab` ffmpeg subprocess on Windows with `IDXGIOutputDuplication::AcquireNextFrame` + `ID3D11Texture2D` → `nvEncRegisterResource`. Big win for Windows latency.
5. **PipeWire portal capture on Wayland** — `ashpd` ScreenCast portal + PipeWire stream import via `pipewire` crate. Replaces stub at `crates/qubaix-display/src/pipewire/mod.rs`.
6. **Wayland privacy** — protocol-dependent (ext-image-copy-capture / xdg-shell). Defer if compositors don't ship it.

### P2 (nice-to-have, after P1)
1. **AV1 / HEVC encode** — current is H.264-only (host-agent rejects non-H.264 sessions at `main.rs:873-878`).
2. **HDR capture + present** — DXGI HDR (`DXGI_FORMAT_R16G16B16A16_FLOAT`), SCKit HDR streams (macOS 14+), `wgpu::TextureFormat::Rgba16Float` present path with PQ transfer.
3. **Pen/tablet Windows real impl** — current is stubbed (`crates/qubaix-pen/src/lib.rs:26-28`).
4. **Mobile/web client** — WebTransport + WebCodecs + WebGPU + Cronet wrapper.
5. **Signed binaries** — Authenticode + `notarytool` + GPG + `cosign`. TUF key ceremony script is the first concrete deliverable (pairs with the existing `tough` consumer in the daemon).
6. **CI: add `macos-latest`** — only Linux CI green today; macOS path is unverifiable.
7. **`docs/tuf.md`** — linked from README badge, file absent. Either write it or remove the badge.
8. **`docs/operations/`** — 5 dirs in `ops/` (`aws`, `coturn`, `signaling-server`, `tuf`, `vm-lab`) but no `docs/operations/` content; only `ops/README.md`.