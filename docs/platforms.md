# Platform Notes

## Linux

Priority: first host platform.

- Capture: PipeWire.
- Zero-copy path: DMA-BUF where available.
- Encode: VAAPI, NVENC, AMF depending on GPU.
- Audio: PipeWire loopback or graph capture.
- Input: evdev, uinput, libei, and gamepad bridging.

Constraints:

- Wayland permissions and compositor differences matter.
- Portal integration matters for sandboxed packaging.
- DMA-BUF support varies by compositor and driver stack.

Current implementation:

- Media readiness probe checks for PipeWire runtime/tooling and FFmpeg H.264 hardware encoders.
- Host agent can print a tested FFmpeg command plan for PipeWire capture to low-latency H.264 Annex B stdout.
- Host agent has a finite Linux capture smoke command that spawns FFmpeg, reads stdout, and frames H.264 access units.
- Host agent can relay a native QUIC ticket and stream those framed H.264 access units into an authenticated client connection.
- Client CLI can receive the native QUIC stream and write a `.h264` file.
- Datagram transport, pacing, backpressure, decoded presentation, and Linux runtime validation are not complete yet.

## Windows

Priority: first parity platform.

- Capture: Desktop Duplication first, Windows Graphics Capture as an alternate path.
- Encode: NVENC, AMF, Intel QSV.
- Audio: WASAPI loopback.
- Input: SendInput, Raw Input, controller virtualization when needed.

Constraints:

- HDR, high-refresh displays, and multi-monitor setups need explicit handling.
- Capture performance differs between windowed and full-screen content.

Current implementation:

- Identity, pairing, discovery, and signaling compile and run on Windows.
- Media readiness probe reports the Windows capture adapter as planned.
- Real Desktop Duplication capture is not implemented yet.

## macOS

Priority: full desktop support after Linux and Windows base path is stable.

- Capture: ScreenCaptureKit.
- Encode and decode: VideoToolbox.
- Audio: system capture path must follow current macOS entitlement and API rules.
- Input: CGEvent taps and related HID-safe injection paths.

Constraints:

- Packaging, permissions, and entitlements are part of the runtime architecture.
- Some input and audio capabilities differ between App Store-safe and direct-distribution builds.

## Android

Priority: client first, host later.

- Decode: MediaCodec to a Surface or SurfaceView.
- Display: frame pacing through Choreographer-aware rendering.
- Input: touch, keyboard, mouse, and controller adaptation.
- Network: QUIC or WebRTC depending on session type.

Constraints:

- Hosting via MediaProjection is viable but not a first milestone because the permission and service model adds latency and product complexity.
- Thermal throttling and mobile radio variability need bitrate adaptation earlier than on desktop.

## Cross-Platform Rules

1. Treat codec and transport support as runtime-negotiated capabilities.
2. Separate capture traits from encoder traits.
3. Keep relay fallback outside the host capture loop.
4. Keep browser compatibility as a session profile, not a universal transport requirement.
5. Make Android a first-class client target instead of forcing the desktop UI model onto it.

## Local Probes

Run this from any host candidate:

```powershell
& "$HOME\.cargo\bin\cargo.exe" run -p host-agent -- --probe-media
& "$HOME\.cargo\bin\cargo.exe" run -p host-agent -- --plan-linux-pipewire-h264 --pipewire-node 0 --h264-encoder nvenc
& "$HOME\.cargo\bin\cargo.exe" run -p host-agent -- --run-linux-pipewire-h264 --pipewire-node 0 --h264-encoder nvenc --max-media-frames 120
```

On Linux, a production-ready result requires PipeWire capture availability, at least one preferred hardware H.264 encoder from FFmpeg, stable frame output from the run command, verified native QUIC end-to-end delivery on a real Linux host, and transport-layer pacing/backpressure. The plan command is a dry run; the run command is a bounded capture smoke test. On Windows today, the probe is expected to report the capture adapter as planned rather than ready.

