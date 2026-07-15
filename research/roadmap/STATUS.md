# Roadmap Status

Last updated: 2026-07-03. Tracks the 19-item P0/P1/P2 roadmap at `research/roadmap/pN-NN-*.md`.

| # | Status | Doc | Commits | Notes |
|---|--------|-----|---------|-------|
| P0-1 HW encode | done | [p0-01-hw-encode.md](p0-01-hw-encode.md) | `87e0bc4`, `3aa4eb5` | `--list-encoders` CLI, runtime probe, software fallback verified |
| P0-2 datagram path | done | [p0-02-datagram-media-path.md](p0-02-datagram-media-path.md) | `db0492d`, `51557a4`, `6cacfc6` | on by default; `--no-datagram-media` opts back to reliable uni-stream |
| P0-3 HW decode | scaffold | [p0-03-hw-decode.md](p0-03-hw-decode.md) | `9d903c5`, `1524655` | `RunningHwFrameDecoder` returns `Err` → subprocess fallback. Per-AVHWDeviceType `get_format` wiring is the follow-up; needs `libclang-*-dev`. |
| P0-4 adaptive bitrate | done | [p0-04-adaptive-bitrate.md](p0-04-adaptive-bitrate.md) | `b1cfb00`, `3aa4eb5` | `GccRateController` 7/7 tests; 4Hz feedback wired; ffmpeg subprocess restart on rate change |
| P0-5 frame pacing | done | [p0-05-frame-pacing.md](p0-05-frame-pacing.md) | `5a3b5e3`, `6cacfc6` | `FramePacer` 4/4 tests; winit `ControlFlow::WaitUntil` integration. wgpu Mailbox swapchain is the next step. |
| P0-6 gamepad | done | [p0-06-gamepad.md](p0-06-gamepad.md) | `82adfc9`, `4b1afab` | gilrs capture + uinput open + Xbox360 event surface. macOS virtual gamepad deferred. |
| P1-7 multi-monitor | research | [p1-07-multi-monitor.md](p1-07-multi-monitor.md) | — | x11rb / DXGI / ScreenCaptureKit. Per-display ffmpeg subprocess for v1. |
| P1-8 privacy mode | research | [p1-08-privacy-mode.md](p1-08-privacy-mode.md) | — | vkms (Linux), IddCx (Windows, deferred to dummy HDMI plug), CGVirtualDisplay (macOS, deferred) |
| P1-9 clipboard | research | [p1-09-clipboard.md](p1-09-clipboard.md) | — | arboard 3.4+ |
| P1-10 mic | research | [p1-10-mic.md](p1-10-mic.md) | — | cpal 0.15+ + opus 0.3+ + webrtc-audio-processing |
| P1-11 TURN | research | [p1-11-turn.md](p1-11-turn.md) | — | coturn for v1; short-term credentials via signaling server |
| P1-12 stats overlay | research | [p1-12-stats-overlay.md](p1-12-stats-overlay.md) | — | egui 0.30+ two-pass render |
| P1-13 daemon | research | [p1-13-daemon.md](p1-13-daemon.md) | — | systemd Type=notify / windows-service / launchd; redb state; tough TUF auto-update |
| P2-14 HDR | research | [p2-14-hdr.md](p2-14-hdr.md) | — | scRGB (Windows) / ScreenCaptureKit HDR (macOS 14+); H.265 Main10 / AV1 10-bit |
| P2-15 pen/tablet | research | [p2-15-pen-tablet.md](p2-15-pen-tablet.md) | — | libinput / Pointer Input / NSEvent; 36-byte `WirePenEvent` |
| P2-16 4K144 | research | [p2-16-4k144.md](p2-16-4k144.md) | — | HEVC preferred over AV1 (B-frames cap on RTX 40); ~30ms LAN end-to-end |
| P2-17 cross-platform host | research | [p2-17-cross-platform-host.md](p2-17-cross-platform-host.md) | — | per-OS `pub trait CaptureBackend`; macOS build needs a Mac runner |
| P2-18 mobile/web | research | [p2-18-mobile-web.md](p2-18-mobile-web.md) | — | WebTransport + WebCodecs + WebGPU; Cronet on iOS/Android |
| P2-19 signed binaries | research | [p2-19-signed-binaries.md](p2-19-signed-binaries.md) | — | Authenticode OV + Apple Developer ID + GPG + cosign + TUF; v1 budget ~$250/yr |

## P0 deliverable status (PR https://github.com/MegaPanchamZ/qubox/pull/1)

| Item | Landed | Deferred |
|------|--------|----------|
| P0-1 | full | — |
| P0-2 | full | — |
| P0-3 | scaffold + build infra | per-AVHWDeviceType `get_format` + `av_hwframe_transfer_data` wiring |
| P0-4 | full | NVENC runtime bitrate set (needs NVENC host); Reed-Solomon FEC (only if 1% XOR insufficient) |
| P0-5 | full + winit integration | wgpu Mailbox swapchain (softbuffer path is unchanged) |
| P0-6 | full | macOS virtual gamepad; rumble / motion (v2/v3) |

## Tests

- transport 10/10 (jitter buffer, OW delay tracker, gamepad datagram, chunker, access-unit reassembly, loopback round-trip)
- frame_pacing 4/4 (first-frame immediate, rapid redraws skip, catch-up after stall, early tolerance)
- rate_control 7/7 (ready after reaction gate, multiplicative decrease, fast start, reaction gate, panic on OWD, panic on loss, stable OWD)

## E2E on Xephyr :99

- signaling-server ✅
- host-agent starts, accepts pairing, binds NativeQuic, captures audio + gamepad backend
- client-cli connects, renders frames with `FramePacer` + `ControlFlow::WaitUntil`
- video frame capture from the headless Xephyr display is limited by the dev box (no compositor, no game window) — same condition that applied to the winit baseline

## What's left (by priority)

### Must-do before a "1.0" release (P1)

1. **P1-13 daemon** — required for the auto-update chain (and pairs with P2-19 signed binaries). Implementation: `systemd Type=notify` + `sd-sys` (Linux), `windows-service` (Windows), `launchd` plist (macOS); redb for state; tough TUF for metadata; per-OS IPC.
2. **P1-11 TURN** — required for NAT-traversal on real-world networks. Implementation: `coturn` config + signaling server issues short-term HMAC credentials; quinn over TURN via custom `AsyncUdpSocket`.
3. **P1-8 privacy mode** — vkms is the cheapest path; ~50 lines of Rust + a modprobe invocation.
4. **P1-7 multi-monitor** — one ffmpeg subprocess per display; x11rb + RandR on Linux, DXGI on Windows.
5. **P1-12 stats overlay** — egui 0.30+ two-pass render; ~200 lines of Rust.

### Should-do (P1)

6. **P1-10 mic** — cpal + opus + webrtc-audio-processing. Important for the productivity use case.
7. **P1-9 clipboard** — arboard 3.4+; ~150 lines.

### Nice-to-have (P2)

8. **P2-19 signed binaries** — needed for any "real" distribution (App Store, Play Store, SmartScreen). Implementation: Authenticode + notarytool + GPG + cosign + TUF.
9. **P2-18 mobile/web** — WebTransport + WebCodecs + WebGPU; Cronet on iOS/Android.
10. **P2-17 cross-platform host** — per-OS `pub trait CaptureBackend`; needs Mac CI runner.
11. **P2-14 HDR** — 10-bit pipeline; needs HDR capture hardware.
12. **P2-16 4K144** — HW encode + HEVC; high-bandwidth link required.
13. **P2-15 pen/tablet** — niche; defer until P1 + P2-18 land.
