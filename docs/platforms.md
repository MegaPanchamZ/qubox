# Platform Support Matrix

Qubox targets **Linux**, **Windows**, and **macOS** as first-class desktop platforms for both hosting and viewing.

| Capability | Linux | Windows | macOS |
|------------|-------|---------|-------|
| **Desktop App (`qubox-client-gui`)** | `.AppImage`, `.deb`, `.rpm` | `-setup.exe` (NSIS), `.msi` | `.dmg` (`aarch64` / Universal) |
| **Display Capture** | PipeWire (Wayland/X11) + X11 Shm fallback | DXGI Desktop Duplication + GDI fallback | ScreenCaptureKit / CoreGraphics |
| **Hardware Encode** | NVENC, VAAPI, AMF (`x264` SW fallback) | NVENC, AMF, QSV, MediaFoundation | VideoToolbox (`h264_videotoolbox`, `hevc_videotoolbox`) |
| **Hardware Decode** | VAAPI / NVDEC + FFmpeg SW fallback | D3D11VA / DXVA2 + FFmpeg SW fallback | VideoToolbox + FFmpeg SW fallback |
| **Renderer** | `wgpu` + `winit` (Vulkan / GL) | `wgpu` + `winit` (DX12 / Vulkan) | `wgpu` + `winit` (Metal) |
| **System Audio Capture** | PipeWire / PulseAudio loopback | WASAPI loopback | CoreAudio / ScreenCaptureKit |
| **Keyboard & Mouse Injection** | `enigo` / XTest / Wayland virtual input | `SendInput` (`enigo`) | `CGEvent` (`enigo`) |
| **4-Player Virtual Gamepad** | `/dev/uinput` (up to 4 virtual Xbox 360 pads) | `ViGEmBus` (up to 4 virtual Xbox 360 pads) | Planned (HID virtual device) |
| **Transport** | Iroh P2P (`quinn` QUIC v2) + Relay / TURN | Iroh P2P (`quinn` QUIC v2) + Relay / TURN | Iroh P2P (`quinn` QUIC v2) + Relay / TURN |

---

## Linux Notes

- **Capture:** Uses PipeWire on Wayland and modern X11 desktops, with automatic X11 capture fallback when running under headless Xvfb/Xephyr or traditional X11 window managers.
- **4-Player Co-Op Gamepads (`uinput`):** Ensure your user has write access to `/dev/uinput` (typically via the `input` group or a `udev` rule) so `qubox-host-agent` can create up to 4 virtual Xbox 360 controllers (`Player 1`–`Player 4`).

## Windows Notes

- **Capture & Encode:** Uses Desktop Duplication (`DXGI`) with hardware encoding via NVENC, AMF, Intel QSV, or MediaFoundation (`h264_mf`).
- **4-Player Co-Op Gamepads (`ViGEmBus`):** Install the [ViGEmBus driver](https://github.com/nefarius/ViGEmBus/releases) on the Windows host machine to enable virtual Xbox 360 controller passthrough for up to 4 connected guests.

## macOS Notes

- **Permissions:** macOS requires **Screen Recording** permission for `Qubox` / `qubox-host-agent` to capture the display and **Accessibility** permission to inject mouse and keyboard events. Grant both in **System Settings → Privacy & Security**.
- **Encode / Decode:** Uses Apple `VideoToolbox` hardware acceleration on Apple Silicon (`M1`/`M2`/`M3`/`M4`) and Intel Macs.
