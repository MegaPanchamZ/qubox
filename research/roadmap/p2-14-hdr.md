# P2-14: HDR (High Dynamic Range) Streaming

Status: research complete, implementation pending.
Owner: `apps/host-agent` (capture/encode), `apps/client-cli` (decode/present).
Depends on: P0-1 (HW encode; NVENC/VAAPI/VideoToolbox), P0-3 (HW decode; ffmpeg-next), P0-5 (frame pacing; wgpu HDR surface), the existing `WireAccessUnitHeader`.
Blockers: Linux HDR capture is not yet stable; macOS HDR capture requires macOS 14+ and a real HDR display; Windows HDR requires Windows 10+ with HDR enabled.

## Goal

Add end-to-end HDR support: capture the host's HDR desktop, encode as H.265 Main10 (or AV1 10-bit) with HDR10 metadata, transmit over QUIC, decode, and present on the client's HDR display. Tone-map if the client is SDR-only. The wire format carries color space metadata so the client can present correctly.

## Research Summary

### HDR capture on Windows (DXGI Desktop Duplication 1.2+)

DXGI Desktop Duplication with D3D11.1+ supports HDR capture. The texture format is typically `R16G16B16A16_FLOAT` (scRGB) for the desktop compositor. The captured scRGB must be converted to PQ-encoded YUV 10-bit (P010LE) for encoding.

- DXGI 1.2 API: `IDXGIOutputDuplication::AcquireNextFrame` returns a `ID3D11Texture2D` (HDR-aware).
- DXGI 1.5: `SetHDRMetaData` to set `DXGI_HDR_METADATA_HDR10` on the swapchain.
- Color space detection: check the output's `DXGI_OUTPUT_DESC1` for color space (`DXGI_COLOR_SPACE_RGB_STUDIO_G24_NONE_P709`, etc.).
- Latency: 1-3 ms per frame, similar to SDR capture.

**Caveat**: many GPUs / drivers whitewash HDR capture (the captured image is brighter than expected). Disable driver color enhancements and verify the monitor color profile is correct (sRGB) when validating.

Rust crate: `dxgi-capture-rs` (high-level wrapper around DXGI Desktop Duplication) + the `windows` crate for Direct3D 11.1+ bindings.

### HDR capture on Linux

**Not stable.** Wayland's `wlr-screencopy-unstable-v1` doesn't expose HDR; the X11 capture path is sRGB-only; DRM/KMS direct capture can read the framebuffer in any format the GPU outputs but requires root and bypasses the compositor. For the first release, **Linux HDR is deferred**. Capture in SDR, encode in SDR, present in SDR. The HDR path is Windows/macOS only.

### HDR capture on macOS (ScreenCaptureKit, macOS 14+)

`SCStreamConfiguration` supports HDR via `colorSpaceName: kCGColorSpaceExtendedSRGB` or `kCGColorSpaceDisplayP3` and 10/16-bit pixel formats (`kCVPixelFormatType_420YpCbCr10BiPlanarVideoRange` for 10-bit, or 16-bit float). The HDR transfer function is PQ (SMPTE 2084) for HDR10 or HLG (ARIB STD-B67) for HLG.

### HDR encoding (H.265 Main10 / AV1 10-bit)

NVENC HEVC Main10 HDR10 is the standard. ffmpeg flags:

```bash
-c:v hevc_nvenc -profile:v main10 -pix_fmt p010le \
  -color_primaries bt2020 -color_trc smpte2084 -colorspace bt2020nc \
  -max_cll 1000,400 \
  -master_display "G(13250,34500)B(7500,3000)R(34000,16000)WP(15635,16450)L(10000000,50)"
```

- `max_cll`: Max Content Light Level (nits). Comma-separated `max,max_fall`.
- `master_display`: G, B, R, WP (white point) CIE 1931 xy coordinates + L (min, max) luminance. The format is the standard HDR10 SEI message.
- For VAAPI: same flags; `hevc_vaapi` supports Main10 with HDR metadata.
- For QSV: `hevc_qsv` with Main10 profile.
- For VideoToolbox: `hevc_videotoolbox` with Main10 + HDR metadata.
- For AV1: `av1_nvenc` or `av1_vaapi` with the same color flags; AV1 has its own metadata OBU for HDR.

### HDR decoding (ffmpeg-next, P0-3)

`ffmpeg-next` supports P010LE for both decode and encode. In Rust:

```rust
use ffmpeg_next as ffmpeg;
let mut decoder = ffmpeg::codec::context::Context::from_parameters(input.parameters())?
    .decoder().video()?;
while decoder.receive_frame(&mut frame).is_ok() {
    // frame.format() == Pixel::P010LE
    // frame.color_range(), color_trc(), color_primaries() reflect the stream
}
```

The frame metadata includes `AV_FRAME_DATA_MASTERING_DISPLAY_METADATA` and `AV_FRAME_DATA_CONTENT_LIGHT_LEVEL` (HDR10 static metadata). Propagate these to the client via the wire format.

### HDR presentation on the client (wgpu)

wgpu 22+ supports HDR-friendly formats:
- `TextureFormat::Rgba16Float` (scRGB / 16-bit float linear).
- `TextureFormat::Rgb10a2Unorm` (HDR10 / 10-bit unorm).

Configure the surface with the HDR format:

```rust
let config = SurfaceConfiguration {
    usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
    format: TextureFormat::Rgba16Float,  // HDR linear
    width, height,
    present_mode: wgpu::PresentMode::Fifo,  // HDR doesn't support Mailbox on most backends
    alpha_mode: caps.alpha_modes[0],
    view_formats: vec![],
};
surface.configure(device, &config);
```

For HDR10 mode (Windows), the underlying DXGI swapchain needs `DXGI_HDR_METADATA_HDR10` set via `IDXGISwapChain4::SetHDRMetaData`. wgpu doesn't expose this directly; use `raw-window-handle` and the `windows` crate for the interop.

For macOS, `CAMetalLayer`'s `colorPixelFormat` is `.rgba16Float`; `colorspace` is `CGColorSpace(name: CGColorSpace.extendedSRGB)`. wgpu's Metal backend sets these automatically when the surface format is `Rgba16Float`.

### Tone mapping (HDR → SDR)

If the client is SDR-only, we tone-map the HDR stream to SDR. Two options:

- **ffmpeg tonemap filter**: `-vf "zscale=t=linear:npl=100,format=gbrpf32le,zscale=p=bt709,tonemap=hable:desat=0,zscale=t=bt709:m=bt709:r=tv"`. ffmpeg's Hable tone mapper is a simple filmic curve.
- **libplacebo**: a more sophisticated tone mapper (Hable, Reinhard, Mobius, BT.2390, etc.). Bind via the `libplacebo` crate.

For game streaming, the tone map should preserve as much detail as possible. The BT.2390 (ITU-R) curve is the broadcast standard; Hable is faster but lower quality.

### Color space metadata on the wire

Extend `WireAccessUnitHeader` with:

```rust
pub struct ColorInfo {
    pub color_space_id: u8,        // 0=SDR_BT709, 1=HDR10_BT2020_PQ, 2=HLG_BT2020
    pub transfer_function: u8,     // 1=BT709, 13=sRGB, 16=PQ (smpte2084), 18=HLG (arib-std-b67)
    pub color_primaries: u8,       // 1=BT709, 9=BT2020
    pub mastering_display: Option<MasteringDisplay>,
    pub content_light: Option<ContentLight>,
}

pub struct MasteringDisplay {
    pub display_primaries: [[u16; 2]; 3],  // G, B, R xy in 0.00002 units (BT.2408)
    pub white_point: [u16; 2],
    pub max_luminance: u32,  // nits
    pub min_luminance: u32,  // 0.0001 nits
}

pub struct ContentLight {
    pub max_cll: u16,    // nits
    pub max_fall: u16,   // nits
}
```

The client uses this to configure its swapchain and HDR metadata.

### Wire format compatibility

For backward compatibility:
- Old clients ignore the new fields (use `#[serde(default)]`).
- Old hosts don't set the fields; new clients default to SDR.
- HDR is opt-in: the host sets `hdr_enabled: true` in the session preferences.

### 2024-2026 status

- **Windows HDR**: mature. DXGI 1.2+ on Windows 10 1709+; HDR10 over scRGB on most modern GPUs.
- **macOS HDR**: ScreenCaptureKit on macOS 14+; reference modes on MacBook Pro M3+; Apple Pro Display XDR supports HDR.
- **Linux HDR**: KDE Plasma 6 (2024) and GNOME 45+ (2024) have desktop HDR. Wayland capture is not yet stable; HDR streaming from Linux is deferred.
- **ffmpeg HDR**: stable. libplacebo for tone mapping.
- **wgpu HDR**: formats supported; HDR metadata APIs are platform-specific.

## Implementation Plan

### Step 1: Color space detection

`apps/host-agent/src/capture/color.rs` (new):
- `pub fn detect_color_space(adapter: &wgpu::Adapter) -> ColorInfo` — returns the display's color space.
- Linux/X11: SDR only (deferred HDR).
- Windows: `IDXGIOutput::GetDesc1` returns color space.
- macOS: `CGDisplayCopyDisplayMode` + the IOKit registry.

### Step 2: HDR capture

`apps/host-agent/src/capture/dxgi.rs` (existing, extended):
- Detect HDR mode; if HDR, set the texture format to `R16G16B16A16_FLOAT`.
- Run a compute shader to convert scRGB → PQ BT.2020 P010LE for the encoder.

### Step 3: HDR encoding

`apps/host-agent/src/encoder/pipeline.rs` (existing, extended):
- Plan ffmpeg args for HDR: `-c:v hevc_nvenc -profile:v main10 -pix_fmt p010le` + color flags + `-max_cll` + `-master_display`.
- Pass the mastering display and content light metadata from the capture.

### Step 4: Wire format

`crates/qubox-proto/src/lib.rs`:
- Add `ColorInfo`, `MasteringDisplay`, `ContentLight` to `WireAccessUnitHeader`.
- All fields are `Option<>` and `#[serde(default)]` for backward compatibility.

### Step 5: HDR decoding

`apps/client-cli/src/decoder/ffnext.rs` (new, P0-3):
- Open the decoder with the right AVHWDeviceType; verify the frame is P010LE.
- Extract the `AV_FRAME_DATA_MASTERING_DISPLAY_METADATA` and `AV_FRAME_DATA_CONTENT_LIGHT_LEVEL` side data; pass to the renderer.

### Step 6: HDR presentation

`apps/client-cli/src/decoder/render.rs` (existing, extended):
- If `color_space_id == HDR10_BT2020_PQ`, configure the wgpu surface with `Rgba16Float` and apply the PQ → display curve.
- If `color_space_id == SDR_BT709`, use the existing sRGB path.
- On Windows, set the DXGI_HDR_METADATA_HDR10 via the `raw-window-handle` interop.
- On macOS, the wgpu Metal backend sets the CAMetalLayer color space automatically.

### Step 7: Tone mapping (HDR → SDR)

`apps/client-cli/src/decoder/tonemap.rs` (new):
- If the host is HDR but the client is SDR, run a tone-mapping shader pass on the GPU.
- Use a Hable curve (simple, fast) or a BT.2390 curve (broadcast standard).

### Step 8: Configuration

- `VideoStreamPreferences.hdr_enabled: bool` (default: false).
- `VideoStreamPreferences.hdr_mode: HdrMode { Auto, Hdr10, Hlg, Sdr }`.

CLI flag: `--hdr {auto,hdr10,hlg,sdr}`.

### Step 9: Tests

- Unit test: `ColorInfo` serde round-trip.
- Unit test: PQ encoding/decoding (encode sRGB → PQ → sRGB; check the result is close to the original).
- Manual: Windows HDR display, capture a 4K HDR game, verify the client renders the HDR stream with correct luminance.
- Manual: SDR client receives an HDR stream, verify the tone mapping produces a correct SDR image.

## Risks and Open Questions

- **Linux HDR**: not stable in 2024-2026. Defer. Document the limitation.
- **macOS HDR**: macOS 14+ only. Some users on macOS 13 can't receive HDR streams.
- **GPU HDR whitewash**: driver color corrections can wash out the captured HDR image. Test on multiple GPUs.
- **DXGI_HDR_METADATA_HDR10 via wgpu**: wgpu doesn't expose this. Need `raw-window-handle` + the `windows` crate for the interop. A small FFI block in the client.
- **Tone mapping on the client vs the host**: doing it on the client means the host sends more data (HDR is larger) and the client does more work. Doing it on the host means the host downsamples to SDR for SDR clients. Per-client preference.
- **Color accuracy**: the wire format carries the metadata, but if the client misinterprets the metadata (e.g. wrong `max_luminance`), the image is wrong. Test with known-good HDR content.
- **HDR on the encoder side**: ffmpeg's NVENC HDR10 has bugs in some driver versions (whitewash, wrong metadata). Test on the actual hardware.
- **AV1 HDR vs H.265 HDR**: AV1 has a cleaner HDR10 metadata model (separate OBU for metadata), but the encoder support is less mature than H.265. NVENC AV1 HDR works on Ada+; AMD AMF AV1 HDR works on RDNA3+.
- **Steam Deck OLED**: supports HDR; capture path is Linux (deferred).
- **Mastering display metadata**: must match the source content's metadata, not the display's. Capture from the game's HDR config or detect from the desktop.
- **Audio with HDR**: not affected (audio is independent of color space).

## References

- DXGI Desktop Duplication API: https://learn.microsoft.com/en-us/windows/win32/direct3ddxgi/desktop-dup-api
- dxgi-capture-rs: https://crates.io/crates/dxgi-capture-rs
- NVENC HDR10 forum: https://forums.developer.nvidia.com/t/nvenc-hdr10/351194
- AV1 HDR recording (OBS forum): https://obsproject.com/forum/threads/can-we-record-hdr-gameplay-with-av1-encoder.166103/
- ffmpeg HDR10 encoding guide: https://codecalamity.com/encoding-uhd-4k-hdr10-videos-with-ffmpeg/
- Voukoder HDR: https://www.voukoder.org/forum/thread/487-hdr-support-in-x265-and-nvenc-hevc/
- ffmpeg tonemap filter: https://ffmpeg.org/ffmpeg-filters.html#tonemap
- libplacebo: https://code.videolan.org/videolan/libplacebo
- ScreenCaptureKit on macOS 14+: https://developer.apple.com/documentation/screencapturekit
- DXGI color spaces: https://learn.microsoft.com/en-us/windows/win32/api/dxgi1_4/ns-dxgi1_4-dxgi_output_desc1
- Perplexity research, 2026-07-02: DXGI HDR capture, NVENC HDR10, ffmpeg P010LE, wgpu HDR, tone mapping, 2024-2026 status.
