# P0-3: Hardware Decode (ffmpeg-next, libavcodec HW API)

Status: **scaffold + build infra** (commits `9d903c5`, `1524655`; PR https://github.com/MegaPanchamZ/qubox/pull/1). `RunningHwFrameDecoder::spawn` still returns `Err`; the subprocess decoder (which already supports `--decoder h264_vaapi` / `h264_cuvid` / `h264_qsv` etc.) is the production path. The per-AVHWDeviceType `get_format` + `av_hwframe_transfer_data` wiring needs `libclang-*-dev` at build time and is targeted at Windows / macOS dev boxes + CI runners. Follow-up after the P1 set.
Owner: `client-cli` (decoder pipeline), with a new `decoder` module.
Depends on: ADR-003 (ffmpeg-next migration plan), P0-2 (datagram media path).
Blockers: ffmpeg-next's static build (`build` feature) requires a C toolchain; on the dev box we currently link shared. Decide: vendored vs shared for the first release.

## Goal

Replace the subprocess ffmpeg decoder in `client-cli` with an in-process **ffmpeg-next** decoder that uses libavcodec's HW decode API. Reduce end-to-end decode latency from 5-10 ms (subprocess + pipe + fork overhead) to 1-3 ms for H.264 and 2-4 ms for AV1, drop CPU usage to <1% per frame, and enable zero-copy wgpu interop on D3D11VA / VideoToolbox. Keep the subprocess path as a fallback for driver crashes, GPU contention, and unusual formats.

## Research Summary

### ffmpeg-next crate (2024-2026)

- **Latest version**: 7.x track (6.1.1 is the most recent 6.x release); on crates.io as `ffmpeg-next`.
- **License**: WTFPL on the wrapper, LGPL/GPL on the bundled FFmpeg. Static linking (`build` feature) carries the LGPL/GPL obligations of the FFmpeg build.
- **Build modes**:
  - `build` feature: download + build FFmpeg from source, **statically link** into the Rust binary. Produces a self-contained .exe (no DLL). Linux: requires `clang/gcc` + `yasm/nasm` + `pkg-config`. Windows: requires MSVC or MinGW + nasm. macOS: requires Homebrew `nasm` + `pkg-config`.
  - System shared: omit `build`, link against the system `libavcodec` / `libavutil` / `libswscale`. Smaller binary, but the user's machine needs FFmpeg installed (or we ship a small FFmpeg bundle).
- **Version features**: `ffmpeg43`, `ffmpeg44`, `ffmpeg50`, ..., `ffmpeg61`, `ffmpeg62`, `ffmpeg7`. Pin to `ffmpeg61` for the first release (broadly available in package managers); migrate to `ffmpeg7` once our CI has it.
- **MSRV**: ~1.56+ documented; no hard policy.
- **Platforms**: Linux x86_64, Windows MSVC + MinGW, macOS x86_64 + Apple Silicon. ARM Linux works but is not a target.

```toml
# Cargo.toml
[dependencies]
ffmpeg-next = { version = "7.0", default-features = false, features = ["ffmpeg61", "avcodec", "avutil", "swscale"] }
```

### libavcodec HW decode API (ffmpeg 6.x / 7.x)

The HW decode API has three pieces:

1. **`AVHWDeviceContext`**: describes the GPU device (VAAPI render node, CUDA device index, D3D11 adapter, etc.). Created with `av_hwdevice_ctx_create(&dev_ref, AV_HWDEVICE_TYPE_*, device_string, options, 0)`.
2. **`AVHWFramesContext`**: describes a pool of GPU frames in a specific format (VAAPI surfaces in NV12, CUDA surfaces in NV12, D3D11 textures in NV12, etc.). Created via `avcodec_get_hw_frames_parameters` and initialized with `av_hwframe_ctx_init`.
3. **`AVCodecContext::get_format` callback**: FFmpeg calls this to ask which pixel format to use. The callback selects a HW format from the list, binds the device context, and returns the format. The decoder then writes HW frames into the pool.

Per-backend init:

| Backend       | AVHWDeviceType            | AVPixelFormat     | device arg                |
|---------------|---------------------------|-------------------|---------------------------|
| VAAPI (Linux) | `AV_HWDEVICE_TYPE_VAAPI`  | `AV_PIX_FMT_VAAPI`| `/dev/dri/renderD128`     |
| CUDA / NVDEC  | `AV_HWDEVICE_TYPE_CUDA`   | `AV_PIX_FMT_CUDA` | `0` (CUDA device index)   |
| D3D11VA (Win) | `AV_HWDEVICE_TYPE_D3D11VA`| `AV_PIX_FMT_D3D11` | `0` (adapter index)       |
| QSV (Win/Linux) | `AV_HWDEVICE_TYPE_QSV`  | `AV_PIX_FMT_QSV`  | `/dev/dri/renderD128`     |
| VideoToolbox (Mac) | `AV_HWDEVICE_TYPE_VIDEOTOOLBOX` | `AV_PIX_FMT_VIDEOTOOLBOX` | `NULL` |
| Vulkan (Linux) | `AV_HWDEVICE_TYPE_VULKAN` | `AV_PIX_FMT_VULKAN` | `0` (device index)     |
| DRM (Linux)   | `AV_HWDEVICE_TYPE_DRM`    | `AV_PIX_FMT_DRM_PRIME` | `/dev/dri/renderD128` |

### Per-codec HW decoders (ffmpeg 7.x)

| Codec   | NVDEC/CUDA           | VAAPI               | QSV                  | D3D11VA              | VideoToolbox         |
|---------|----------------------|---------------------|----------------------|----------------------|----------------------|
| H.264   | h264_cuvid / `h264`+CUDA hwaccel | h264_vaapi          | h264_qsv             | h264+d3d11va hwaccel | h264_videotoolbox    |
| H.265   | hevc_cuvid / `hevc`+CUDA | hevc_vaapi          | hevc_qsv             | hevc+d3d11va         | hevc_videotoolbox    |
| AV1     | av1 (Ada+ / Hopper+) | av1_vaapi (Arc+ / RDNA2+) | av1_qsv (Arc+ / Meteor Lake) | av1+d3d11va (driver-dependent) | M3+ on macOS 14+ |

With ffmpeg's standard decoder names (`h264`, `hevc`, `av1`) + a HW device context, FFmpeg auto-selects the right backend; we don't have to pick the per-codec HW decoder manually.

### AVFrame → presentable surface

Two paths:

- **Download (default)**: `av_hwframe_transfer_data(sw_frame, hw_frame, 0)` copies the GPU surface to a SW frame in NV12/P010/RGBA. CPU cost is ~0.5-1 ms for 1080p NV12. Then `sws_scale` to RGBA, `queue.write_texture` to wgpu. **Easy, robust, works everywhere.**
- **Zero-copy (advanced)**: extract the underlying handle from the HW frame:
  - **D3D11VA**: `frame->data[0]` is a `ID3D11Texture2D*` (FFmpeg sets it via `AVHWDeviceContext.d3d11va`); import into wgpu-hal's DXGI backend.
  - **VideoToolbox**: `frame->data[3]` is a `CVPixelBufferRef`; create a `wgpu::Texture` from a Metal texture wrapping the IOSurface.
  - **VAAPI/Vulkan/DRM**: dmabuf FD import via wgpu-hal's `import_memory_fd` (Linux); requires `wgpu::Features::EXTERNAL_MEMORY` and the right platform extensions.

Zero-copy saves 1-2 ms per frame. For game streaming the win is meaningful at 4K144; for 1080p60 it's negligible. **Start with download; add zero-copy in a follow-up.**

### Latency (HW vs SW, ffmpeg 7.x, 2024-2026 estimates)

| Resolution / FPS | SW libx264-style | HW (NVDEC/VAAPI/D3D11VA/VideoToolbox) |
|------------------|------------------|---------------------------------------|
| 1080p60          | 2-3 ms           | 0.5-1.5 ms                            |
| 1440p120         | 5-8 ms           | 1-2 ms                                |
| 4K144            | 18-30 ms         | 2-4 ms                                |
| 1080p60 H.265    | 4-7 ms           | 1-2 ms                                |
| 4K144 AV1        | 30-50 ms         | 3-6 ms                                |

HW decode is the only viable option for AV1 4K144. For H.264 1080p60 the absolute difference is small (1-2 ms), but HW decode also frees up CPU for input handling, network, and the winit render loop.

### Decode flow (send_packet / receive_frame with EAGAIN)

The canonical pattern (used in every ffmpeg-based decoder):

```rust
unsafe fn decode_packet(ctx: *mut AVCodecContext, pkt: *mut AVPacket, out: &mut Vec<*mut AVFrame>) -> i32 {
    let ret = avcodec_send_packet(ctx, pkt);
    if ret < 0 { return ret; }
    loop {
        let frame = av_frame_alloc();
        let ret = avcodec_receive_frame(ctx, frame);
        if ret == AVERROR_EAGAIN || ret == AVERROR_EOF {
            av_frame_free(&mut frame);
            break;
        } else if ret < 0 {
            av_frame_free(&mut frame);
            return ret;
        }
        out.push(frame);
    }
    0
}
```

Flush at end-of-stream: `avcodec_send_packet(ctx, NULL)`, then read until `AVERROR_EOF`, then `avcodec_flush_buffers(ctx)` if reusing for a new stream.

### Format quirks

- **10-bit HEVC**: HW decoders produce `AV_PIX_FMT_P010` (16-bit LE NV12). Don't convert to 8-bit; preserve 10-bit through to the wgpu texture (`R16Uint` or `Rgba16Unorm`).
- **High profile H.264**: standard support on all backends. VAAPI and D3D11VA need a 4:2:0 input; 4:2:2 / 4:4:4 are software-only on most HW.
- **HDR passthrough**: `AVFrame` carries `AVFrameSideData` of type `AV_FRAME_DATA_MASTERING_DISPLAY_METADATA` and `AV_FRAME_DATA_CONTENT_LIGHT_LEVEL`. These must be propagated to the wgpu swapchain as HDR metadata (P2-14).
- **Color space**: `AVFrame.color_range`, `color_primaries`, `color_trc`, `colorspace` must be honored. Default sRGB on capture; HDR uses BT.2020 / PQ.

### Why ffmpeg-next over direct vendor bindings

- **Cross-codec, cross-platform**: one API for H.264 / HEVC / AV1; FFmpeg handles the bitstream, container demux (none for us; raw AUs), and codec quirks.
- **HW abstraction**: the same `get_format` callback selects VAAPI/CUDA/D3D11VA/VideoToolbox by checking the codec's hw_config list.
- **Ecosystem maturity**: libavcodec has 20+ years of bug fixes. Direct vendor bindings (CUVID, NVDEC, VideoToolbox C API) require us to handle bitstream parsing, error recovery, and ref-frame management manually.
- **Crates considered**:
  - `ffmpeg-next` (chosen): mature, complete, but large compile time.
  - `video-decoder`: thin wrapper, less HW support, last update 2022.
  - `libavcodec-rs`: stale, no ffmpeg 6/7 support.
  - Direct `nvidia-video-codec-sdk` / `videotoolbox-sys` / `vaapi-sys`: faster compile, but we'd reimplement what FFmpeg already does.

### Subprocess fallback

Pattern:
- `enum DecodeBackend { InProcessHw, InProcessSw, SubprocessFfmpeg }`.
- On startup, try HW init in order: `CUDA → VAAPI → D3D11VA → QSV → VideoToolbox → Vulkan → DRM`.
- If all HW init fails, fall back to `InProcessSw` (no HW init; decode to RGBA in software).
- If the HW decoder crashes (driver returns errors on multiple consecutive frames), close the device, re-init in software, and warn the user. Spawning the subprocess ffmpeg path is the last resort because it costs 50-100 ms to start a new ffmpeg and reset the pipeline.

## Implementation Plan

### Step 1: Add ffmpeg-next to workspace

`Cargo.toml` (workspace) — add a workspace dep:

```toml
ffmpeg-next = { version = "7.0", default-features = false, features = ["ffmpeg61", "avcodec", "avutil", "swscale"] }
```

Pin to shared linking for the first release (smaller binary, easier to debug). Add a `static-ffmpeg` Cargo feature that flips on the `build` feature for self-contained release builds.

`apps/client-cli/Cargo.toml` — add the dependency, gate on a `ffnext` feature flag:

```toml
ffmpeg-next = { workspace = true, optional = true }

[features]
default = ["minifb-compat", "ffnext"]
ffnext = ["dep:ffmpeg-next"]
```

### Step 2: New `decoder` module in `client-cli`

`apps/client-cli/src/decoder/mod.rs`:

```rust
pub mod hw;
pub mod sw;
pub mod subprocess;
pub mod probe;
pub mod frame;

pub enum DecodeBackend { InProcessHw(HwBackend), InProcessSw, SubprocessFfmpeg }

pub struct HwBackend {
    pub family: HwFamily, // Nvenc, Vaapi, Qsv, Amf, VideoToolbox
    pub hw_device: *mut AVBufferRef,    // ffmpeg-sys-next
    pub hw_frames:  *mut AVBufferRef,
    pub codec_ctx: *mut AVCodecContext,
}
```

`apps/client-cli/src/decoder/hw.rs`:
- `pub fn open(codec: VideoCodec, hw_type: AVHWDeviceType, device_arg: &str) -> Result<HwBackend>` — open the device, create frames ctx, set `get_format`, `avcodec_open2`.
- `pub fn send_packet(&mut self, pkt: &Packet) -> Result<()>` — wraps `av_packet_ref` + `avcodec_send_packet`.
- `pub fn receive_frame(&mut self) -> Result<Option<DecodedFrame>>` — wraps the EAGAIN receive loop.
- `pub fn flush(&mut self)` — `avcodec_flush_buffers`.

`apps/client-cli/src/decoder/sw.rs`:
- Same shape as `hw.rs` but with no HW device context; `get_format` returns the codec's native software pixel format (NV12 or YUV420P). Used as the first fallback.

`apps/client-cli/src/decoder/subprocess.rs`:
- The existing ffmpeg subprocess decoder, moved into a module. Used as the last fallback.

`apps/client-cli/src/decoder/probe.rs`:
- `pub fn detect_best_hw_type() -> Option<AVHWDeviceType>` — try `CUDA → VAAPI → D3D11VA → QSV → VideoToolbox → Vulkan → DRM` in order; first one that creates a device + open2 succeeds wins.

`apps/client-cli/src/decoder/frame.rs`:
- `pub struct DecodedFrame { pub width: u32, pub height: u32, pub format: PixelFormat, pub pts: i64, pub data: Vec<u8>, pub hw_handle: Option<HwHandle> }`.
- `pub enum HwHandle { D3D11(*mut c_void), Metal(*mut c_void), DmaBuf(i32) }`.

### Step 3: Wire into `RunningFrameDecoder`

In `apps/client-cli/src/main.rs`:
- `RunningFrameDecoder::new` calls `decoder::probe::detect_best_hw_type()`. If `Some`, opens an `InProcessHw` backend. If `None`, opens `InProcessSw`. If `Sw` fails, opens `SubprocessFfmpeg`.
- The subprocess path's frame reader (currently in `decoder_reader_loop`) becomes a method on the `DecodeBackend` enum: `pub fn read_frame(&mut self) -> Result<Option<DecodedFrame>>`.
- The frame buffer consumer (softbuffer / wgpu) takes a `DecodedFrame`. For HW frames with a valid `HwHandle`, the wgpu path can do zero-copy; for everything else, `data: Vec<u8>` is the RGBA frame ready to blit.

### Step 4: Software fallback safety

- Detect HW errors (`AVERROR(EIO)`, `AVERROR(ENOSYS)`, repeated `AVERROR_INVALIDDATA` over 10 frames): log the error, `avcodec_free_context`, `av_buffer_unref` the device, switch to `InProcessSw`. This is the "driver crashed, recover gracefully" path.
- The user-facing CLI flag `--decoder` (which already exists) gains new values: `auto | nvenc | vaapi | qsv | amf | videotoolbox | software`. The existing `software` is the current subprocess path with no HW; we now distinguish `software-inprocess` (libavcodec SW) from `software-subprocess` (subprocess ffmpeg).

### Step 5: Tests

- Unit test: open an in-process H.264 SW decoder, feed a canned H.264 AU, verify the decoded frame is the expected size and has the right pixel format.
- Unit test: probe on the dev box (no GPU) returns `InProcessSw`.
- Integration test: same with a real GPU (CI doesn't have one; manual on a real machine) — verify HW decode produces the same frame as SW within a small PSNR threshold.
- Latency test: `std::time::Instant::now()` around `decode_packet` for 1000 frames; HW should be <2 ms/frame at 1080p60, SW should be <5 ms.

### Step 6: Migration safety

- The subprocess ffmpeg path stays in the codebase for one release behind the `ffnext` feature flag.
- `ffnext` is on by default in `client-cli` once it's stable. Until then, the subprocess path is the default.
- A `bp debug decoder` subcommand prints which decoder is active, the AVHWDeviceType, the device path, and the codec.

## Risks and Open Questions

- **Static vs shared linking**: vendored static FFmpeg adds ~30 MB to the client-cli .exe on Windows. Shared linking requires the user to install FFmpeg or for us to ship a small FFmpeg bundle. Decide: ship a `ffmpeg.dll` (~40 MB) + `client-cli.exe` (~15 MB) and rely on PATH, or use static and bump to ~50 MB .exe. Probably the former; static build only for the headless server.
- **FFmpeg version drift**: ffmpeg-next 7.0 vs the ffmpeg 6.1 system package on Debian/Ubuntu LTS (24.04 ships 6.1, 22.04 ships 5.1). Pin to `ffmpeg61` for compatibility, but the system ffmpeg (used by the encoder subprocess) is a separate version. They don't need to match; the wire format is H.264/HEVC/AV1 elementary streams.
- **LGPL compliance**: static linking FFmpeg with `--enable-gpl` (libx264, libx265, libaom) requires shipping object files or providing a relink option. With `--enable-lgpl --enable-shared`, dynamic linking satisfies LGPL. With `--enable-gpl --enable-static`, must provide source offer. The cleanest path: dynamically link FFmpeg (whether to system or to a bundled DLL/dylib/so), and treat the build as LGPL by configuring FFmpeg without `--enable-gpl`. x264 is GPL so we already accept that. Document the obligations in `LICENSE-FFMPEG.md`.
- **Driver recovery**: a HW decoder crash (driver returns EIO) should fall back to SW, but the fallback must be a clean transition — no half-decoded frames, no leaked contexts. The decoder state machine needs careful test coverage.
- **AV1 on Intel/AMD**: AV1 VAAPI on RDNA2 is broken in some Mesa versions; AV1 on Intel Arc requires oneVPL 2023.3+. Our runtime probe must check the actual codec + hwaccel combination works, not just that the device opens.
- **Cross-driver VAAPI stability**: VAAPI on iHD vs AMDVLK vs Nvidia proprietary driver has different failure modes for AV1. The `try { hw_decode_one_frame() } catch { fall back to sw }` pattern catches most issues.
- **Zero-copy interop**: the wgpu interop paths for D3D11VA / VideoToolbox / VAAPI are platform-specific and have sharp edges. Defer to a follow-up. Start with `av_hwframe_transfer_data` + RGBA blit.
- **Codec parameter changes mid-stream**: if the host changes the encoder bitrate or resolution mid-session, the decoder may need a reset. `avcodec_flush_buffers` works for bitrate changes; resolution changes need `avcodec_close` + re-open.
- **Decoder race with sender**: a real-time decoder runs in a tight loop on the `decode_packet` future. If the sender is paused (capture idle, e.g. menu screen), the decoder must not spin. The EAGAIN path handles this: `avcodec_receive_frame` returns EAGAIN when there's no complete frame, the loop yields.

## References

- ffmpeg-next on crates.io: https://crates.io/crates/ffmpeg-next
- ffmpeg-next features: https://lib.rs/crates/ffmpeg-next/features
- ffmpeg-sys-next repo: https://github.com/zmwangx/rust-ffmpeg-sys
- ffmpeg HWAccelIntro: https://trac.ffmpeg.org/wiki/HWAccelIntro
- libavcodec decoding docs (ffmpeg 6.1): https://ffmpeg.org/doxygen/6.1/group__lavc__decoding.html
- Intel oneVPL QSV tutorial: https://habr.com/en/companies/intel/articles/575632/
- StackOverflow: using HW acceleration with libavcodec: https://stackoverflow.com/questions/25791722/using-hardware-acceleration-with-libavcodec
- Reddit r/rust: ffmpeg-next + Windows: https://users.rust-lang.org/t/how-can-i-configure-or-use-correctly-ffmpeg-next-with-windows/134829
- Perplexity research, 2026-07-02: ffmpeg-next API, libavcodec HW decode, latency benchmarks.
