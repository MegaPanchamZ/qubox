# P0-1: Hardware Encode (Low-Latency Game Streaming)

Status: **complete** (commits `87e0bc4`, `3aa4eb5`; PR https://github.com/MegaPanchamZ/qubox/pull/1).
Owner: host-agent (encoder pipeline)
Depends on: ADR-003 (ffmpeg-next decoder), x11grab/DXGI/ScreenCaptureKit capture (P1-7)
Blockers: none on Linux (ffmpeg VAAPI via Mesa works in CI/Xephyr), Windows NVENC/AMF/QSV require a real GPU; macOS VideoToolbox requires a real Mac.

## Goal

Replace the host's `libx264 -preset ultrafast -tune zerolatency` software encoder with a runtime-selected **hardware encoder** (NVENC, VAAPI, QSV, AMF, or VideoToolbox) for H.264, H.265, and AV1. Target a 5-15 ms encode latency at 1080p60 and 4-10 ms at 1440p120/4K144 (vs ~25-40 ms for `libx264 -preset ultrafast` on a modern 8-core CPU), with <2% CPU on the encode path.

## Research Summary

### Per-platform encoder matrix (ffmpeg 6.x / 7.x, 2024-2026)

| Vendor | Encoder       | Min GPU (H.264)             | Min GPU (HEVC)             | Min GPU (AV1)                  | ffmpeg hwaccel pair     |
|--------|---------------|-----------------------------|----------------------------|--------------------------------|-------------------------|
| NVIDIA | h264_nvenc    | Kepler+ (2012+, GK)         | Maxwell+ (2014+, GM)       | Ada (RTX 40)+ / Hopper (H100)  | `-hwaccel cuda`         |
| NVIDIA | hevc_nvenc    | Maxwell+                    | Maxwell+                   | Ada+ / Hopper+                 | `-hwaccel cuda`         |
| NVIDIA | av1_nvenc     | n/a                         | n/a                        | Ada / Hopper (NVENC SDK 12+)   | `-hwaccel cuda`         |
| Intel  | h264_qsv      | Sandy Bridge+ iGPU (2011+)  | Broadwell+ iGPU (2014+)    | Arc/Alchemist (2022+)          | `-init_hw_device qsv`   |
| Intel  | hevc_qsv      | Broadwell+                  | Broadwell+                 | Arc+ (oneVPL 2023+)            | `-init_hw_device qsv`   |
| Intel  | av1_qsv       | n/a                         | n/a                        | Arc+ / Meteor Lake (oneVPL)    | `-init_hw_device qsv`   |
| Intel/AMD (Linux) | h264_vaapi | Intel Broadwell+ / AMD Raven Ridge+ | Intel KBL+ / AMD Picasso+ | Intel Arc+ / AMD RDNA2+ | `-vaapi_device /dev/dri/renderD128` |
| Intel/AMD (Linux) | hevc_vaapi | Intel KBL+ / AMD Raven Ridge+ | Intel KBL+ / AMD Raven+   | Intel Arc+ / AMD RDNA2+        | `-vaapi_device ...`     |
| Intel/AMD (Linux) | av1_vaapi  | n/a                       | n/a                       | Intel Arc+ / AMD RDNA2+        | `-vaapi_device ...`     |
| AMD    | h264_amf      | Polaris+ (2016+, RX 400)    | Vega+ (2017+, RX Vega)     | RDNA3 (RX 7000)+ (AMF 1.4.30+) | `-hwaccel d3d11va`      |
| AMD    | hevc_amf      | Vega+                       | Vega+                      | RDNA3+                         | `-hwaccel d3d11va`      |
| AMD    | av1_amf       | n/a                         | n/a                        | RDNA3+ (RX 7600+ for full rate) | `-hwaccel d3d11va`     |
| Apple  | h264_videotoolbox | macOS 10.14+ on any Mac (T2+ recommended) | macOS 10.14+ | macOS 13+ on M3+       | `-hwaccel videotoolbox` |
| Apple  | hevc_videotoolbox | macOS 10.14+            | macOS 10.14+                | macOS 13+ on M3+               | `-hwaccel videotoolbox` |

`ffmpeg -encoders | egrep '_(nvenc|vaapi|qsv|amf|videotoolbox)$'` is the canonical availability check on every platform.

### Low-latency tuning (per encoder)

The pattern across all HW encoders is: **small buffer, no B-frames, GOP ≈ 2 seconds, CBR, no lookahead, no rate-distortion optimization on the motion side**.

#### NVENC (NVIDIA Video SDK 12+, driver 545+)

- `-preset p1` (fastest, 1-frame encode delay) through `p7` (highest quality, ~6 frame delay). For game streaming, `p1` is the only correct choice. `p2..p4` are useful when you have headroom to trade latency for PSNR.
- `-tune ll_hq` for HQ-mode NVENC, `-tune ull` for ultra-low-latency (Turing+), `-tune lossless` for desktop text.
- `-rc cbr` with `-b:v 4000k -maxrate 4000k -bufsize 2000k` is the canonical Parsec/Moonlight setting. `bufsize = 2x bitrate` is the empirical sweet spot; `1x` causes rate-shock on scene cuts (HDR explosions, ultrawide panning).
- `-bf 0` (no B-frames) is mandatory for minimum latency. B-frames cost 1+ frame reordering delay and are a tax for game streaming.
- `-g 120` for 60 fps (= 2-second GOP). Lower (`-g 60`) for faster recovery; higher (`-g 240`) for ~6% bitrate savings at the cost of recovery.
- `-temporal-aq 0 -spatial-aq 0 -rc-lookahead 0` to disable all adaptive quantization on the motion side. AQ *can* help in dark cinematic scenes but causes bitrate spikes on HUD-heavy FPS games.
- `-b_ref_mode 0` (no B-frame references, redundant with `-bf 0` but explicit).
- **AV1 only on Ada/Hopper**: `-preset p1` still works; `-tier high` for 1080p+, `-tier main` for 720p.

Command template:

```bash
ffmpeg -hide_banner -loglevel warning -nostdin \
  -f x11grab -framerate 60 -video_size 1920x1080 -i :0.0 \
  -c:v h264_nvenc -preset p1 -tune ll_hq -rc cbr \
  -b:v 4000k -maxrate 4000k -bufsize 2000k \
  -g 120 -bf 0 -temporal-aq 0 -spatial-aq 0 -rc-lookahead 0 \
  -pix_fmt yuv420p \
  -f h264 pipe:1
```

#### VAAPI (Intel/AMD on Linux, Mesa 22.0+ / iHD / AMDVLK)

- `-vaapi_device /dev/dri/renderD128` (or any render node).
- `-vf 'format=nv12,hwupload'` for software → VAAPI surface upload. `format=nv12` is required because VAAPI encoders are NV12-only (P010 for 10-bit HEVC).
- `-rc_mode CBR` (use `VBR` if the user prefers bitrate flexibility; `CQP` for offline).
- `-b:v 4000k -maxrate 4000k -bufsize 2000k -g 120 -bf 0`.
- `-low_power 0` (default for newer drivers; enables VME → VME2 → MFC path, sometimes faster; sometimes -lower quality, depends on the kernel driver).
- `-async_depth 1` (lower = less buffered frames in the encoder, lower latency at the cost of throughput; 4 is a good default if you have headroom).
- `-compression_level 1` (fastest) through `7` (slowest, best PSNR). `1` for game streaming.
- B-frames: `-bf 0` (recommended). Some Mesa versions default to 2 B-frames which costs ~30 ms.

Command template:

```bash
ffmpeg -hide_banner -loglevel warning -nostdin \
  -vaapi_device /dev/dri/renderD128 \
  -f x11grab -framerate 60 -video_size 1920x1080 -i :0.0 \
  -vf 'format=nv12,hwupload' \
  -c:v h264_vaapi -rc_mode CBR \
  -b:v 4000k -maxrate 4000k -bufsize 2000k \
  -g 120 -bf 0 -compression_level 1 \
  -f h264 pipe:1
```

#### QSV (Intel, oneVPL 2023.3+ / libmfx 22.5+)

- `-init_hw_device qsv=hw:/dev/dri/renderD128 -filter_hw_device hw` for the device init.
- `-vf 'format=nv12,hwupload=extra_hw_frames=16'` (the `extra_hw_frames=16` is critical — QSV needs extra pool frames for lookahead/RC).
- `-rc_mode CBR` (or `LA_CBR` for VBR with capped peaks).
- `-look_ahead 0` (default) for game streaming. `1` adds ~1 frame latency.
- `-async_depth 1` for minimum latency (4 is the default).
- B-frames: `-bf 0`. QSV's default includes B-frames.
- AV1 on QSV: only on Arc and Meteor Lake with oneVPL 2023.3+; otherwise fallback to HEVC VAAPI on the same hardware.

Command template:

```bash
ffmpeg -hide_banner -loglevel warning -nostdin \
  -init_hw_device qsv=hw:/dev/dri/renderD128 -filter_hw_device hw \
  -f x11grab -framerate 60 -video_size 1920x1080 -i :0.0 \
  -vf 'format=nv12,hwupload=extra_hw_frames=16' \
  -c:v h264_qsv -rc_mode CBR \
  -b:v 4000k -maxrate 4000k -bufsize 2000k \
  -g 120 -bf 0 -look_ahead 0 -async_depth 1 \
  -f h264 pipe:1
```

#### AMF (AMD, driver 23.10+ / AMF 1.4.30+)

- `-c:v h264_amf -preset speed` (`speed`/`balanced`/`quality` map to AMF's `ULTRA_LOW_LATENCY`/`LOW_LATENCY`/`QUALITY`).
- `-rc cbr` for streaming. AMD's wiki explicitly recommends `cbr` for game streaming. `vbr_latency_controlled` is a useful middle ground.
- `-b:v 4000k -maxrate 4000k -bufsize 2000k` is canonical.
- `-enforce_hrd 1` to make the bitstream HRD-conformant (helps decoders with HRD-based rate control; some client decoders need this for CBR).
- `-preanalysis 0` (no lookahead) for game streaming. `1` adds 1+ frame latency.
- `-qmin 0 -qmax 51` (defaults; don't change).
- `-filler_data 0` to skip null-byte padding in CBR (lower bitrate overhead).
- `-smart_access_video 0` (default) — no cross-frame access.
- `-frame_interval 1` (no frame skipping).
- AV1 on AMF: requires RDNA3 (RX 7600+ for full-rate, RX 7900 GRE/XT for ≥1080p120). Older RDNA2 cards don't have AV1 encode silicon.

Command template (H.264):

```bash
ffmpeg -hide_banner -loglevel warning -nostdin \
  -f dshow -framerate 60 -video_size 1920x1080 -i video="Desktop Capture" \
  -c:v h264_amf -preset speed -rc cbr \
  -b:v 4000k -maxrate 4000k -bufsize 2000k \
  -g 120 -bf 0 -enforce_hrd 1 -preanalysis 0 -filler_data 0 \
  -f h264 pipe:1
```

#### VideoToolbox (Apple macOS)

- `-c:v h264_videotoolbox` or `hevc_videotoolbox`. AV1 only on M3+ (macOS 14+).
- `-b:v 4000k -maxrate 4000k -bufsize 2000k` is the canonical setting. VideoToolbox does **not** expose the full NVENC/AMF vocabulary; the practical low-latency recipe is bitrate + bufsize + GOP.
- `-pix_fmt yuv420p` (10-bit HEVC requires `p010le`, supported on M1+).
- `-realtime true` (default in modern ffmpeg) is fine for streaming. `-realtime false` enables better quality at the cost of latency.
- `-prio_speed` is sometimes exposed on the encoder; it caps the encoder's quality knob.
- maxKeyframeInterval in frames (e.g. `-g 120`).

Command template:

```bash
ffmpeg -hide_banner -loglevel warning -nostdin \
  -f avfoundation -framerate 60 -i "1:0" \
  -c:v h264_videotoolbox -b:v 4000k -maxrate 4000k -bufsize 2000k \
  -g 120 -pix_fmt yuv420p \
  -f h264 pipe:1
```

### Runtime probe (host startup)

The host must decide which encoder to use before starting the capture pipeline. Three checks, in order of cost:

1. **`ffmpeg -encoders | egrep '_(nvenc|vaapi|qsv|amf|videotoolbox)$'`** — fast, tells you what the ffmpeg build supports.
2. **`vainfo`** (Linux) — tells you which VAAPI profiles the driver exposes. If `vainfo` returns `VAProfileH264High : VAEntrypointEnc`, VAAPI H.264 is available.
3. **No-op encode test** — `ffmpeg -f lavfi -i color=c=black:s=64x64:d=0.04:r=25 -frames:v 1 -c:v <encoder> -f null -` and check exit code. This catches driver/library mismatches that `-encoders` and `vainfo` miss.

Per-platform probe scripts in our codebase will live in `apps/host-agent/src/encoder/probe.rs`:

```rust
// Pseudo-code
pub async fn detect_best_encoder(codec: VideoCodec) -> Option<HwEncoder> {
    let encs = list_ffmpeg_encoders().await?;
    let order = match (codec, target_os()) {
        (VideoCodec::H264,   TargetOs::Linux)   => ["h264_nvenc", "h264_vaapi", "h264_qsv", "h264_amf"],
        (VideoCodec::H264,   TargetOs::Windows) => ["h264_nvenc", "h264_amf",   "h264_qsv", "h264_vaapi"],
        (VideoCodec::H264,   TargetOs::Mac)     => ["h264_videotoolbox"],
        (VideoCodec::Hevc,   TargetOs::Linux)   => ["hevc_nvenc", "hevc_vaapi", "hevc_qsv", "hevc_amf"],
        // ...
    };
    for name in order {
        if encs.contains(name) && try_no_op_encode(name).await.is_ok() {
            return Some(HwEncoder::from_name(name));
        }
    }
    None  // fall back to software
}
```

### Pixel-format conversion (capture → encoder)

The capture pipeline outputs BGRA (x11grab), BGRA (DXGI), or `kCVPixelFormatType_32BGRA` (ScreenCaptureKit). All HW encoders want either **NV12** (8-bit) or **P010** (10-bit HEVC). Two paths:

- **Software path (default)**: `-vf 'format=nv12'` (or `p010le` for 10-bit). CPU cost is ~1 ms/frame at 1080p on a modern CPU. This is the right choice for VAAPI without DMA-Buf and for QSV without D3D11.
- **HW upload path**: `-vf 'format=nv12,hwupload'` (VAAPI), `-vf 'format=nv12,hwupload=extra_hw_frames=16'` (QSV), `-hwaccel d3d11va -hwaccel_output_format d3d11` (DXGI → D3D11 → AMF/NVENC), `-hwaccel videotoolbox` with IOSurface passthrough on macOS.

Zero-copy (DMA-Buf on Linux, D3D11VA on Windows, IOSurface on macOS) saves 1-2 ms/frame but requires: matching render node permissions, D3D11 device sharing (NVENC + DXGI need a shared device), and explicit coordination with the capture source. **Defer zero-copy to P1-7 (multi-monitor)** — the win is marginal for 1080p60 and not worth the integration complexity at this stage.

### CBR vs VBR vs CQP for game streaming

- **CBR** is correct for game streaming over QUIC datagrams (P0-2). Parsec, Moonlight, and Steam Remote Play all use CBR with `bufsize = 2 * bitrate`. CBR is the only rate-control mode where the client can predict wire-size and the server can predict overflow.
- **VBR** saves 10-20% bitrate on cinematic content but causes periodic spikes on scene cuts (boss spawns, ultimatums). Avoid for game streaming.
- **CQP** (constant QP) is offline-only. Not appropriate for streaming.
- **Adaptive bitrate with HW encoders**: with `-rc cbr` you can change `-b:v` per-frame on NVENC without re-init (the encoder's rate controller respects the new target within ~1 frame). On VAAPI/QSV/AMF you must reopen the encoder or use the encoder's `set_bitrate` callback. Our rate controller (P0-4) will set a single target bitrate every ~250 ms and re-open only on large swings.

### 2024-2026 status

- **AV1 HW encode is now mainstream on NVIDIA Ada+** (GeForce 40 series, RTX 6000 Ada, L4, L40, H100 NVENC SDK 12+). RDNA3 (RX 7600/7700/7800/7900) has AV1 AMF since driver 23.10. Intel Arc is the only x86 iGPU with AV1 QSV (oneVPL 2023.3+). Apple M3+ has AV1 VideoToolbox on macOS 14+.
- **B-frame lookahead on NVENC**: Ada+ supports B-frames at `-preset p1` (disabled by default). The latency cost is ~1 frame and the bitrate savings are ~10%. Not worth it for game streaming.
- **Screen Content Coding (SCC)** is an H.265 extension for desktop text. NVENC supports it via `-tune scc`; AMF/VAAPI/QSV do not. Useful for 4K desktop work; for 1080p60 gaming the win is <5% and not worth the encoder-restart cost.
- **HDR10 metadata passthrough**: NVENC supports `-color_range tv -colorspace bt2020 -color_primaries bt2020 -color_trc smpte2084` for HDR capture. Our capture pipeline doesn't yet support HDR (P2-14).
- **AV1 super-resolution**: NVIDIA's RTX 40+ and Intel Arc support AV1 decode-side super-resolution. Decoded AV1 streams can be upscaled in the client. This is a decoder concern (P0-3), not encoder.

## Implementation Plan

### Step 1: Encoder selection module (host-agent)

`apps/host-agent/src/encoder/probe.rs` (new):
- `pub async fn list_ffmpeg_encoders() -> Result<HashSet<String>>` runs `ffmpeg -hide_banner -encoders` and parses.
- `pub async fn try_no_op_encode(encoder: &str) -> Result<()>` runs a 1-frame lavfi test and checks exit code.
- `pub async fn detect_best_encoder(codec: VideoCodec) -> Option<HwEncoder>` returns the best available encoder for the codec+platform combination.
- Cache the result in the session config (don't re-probe per-frame).

### Step 2: Encoder config module (host-agent)

`apps/host-agent/src/encoder/config.rs` (new):
- `pub struct HwEncoder { name: String, family: HwEncoderFamily }` with `HwEncoderFamily::Nvenc | Vaapi | Qsv | Amf | VideoToolbox`.
- `pub fn plan_ffmpeg_args(&self, codec: VideoCodec, prefs: &VideoStreamPreferences, capture: &CaptureBackend) -> Vec<String>` returns the codec-specific argument list.
- For each `(family, codec)` pair, a `match` arm that produces the canonical low-latency command line.
- Document the footguns: NVENC `-tune ll_hq` vs `-tune ull`, VAAPI `low_power`, QSV `look_ahead`, AMF `preanalysis`.

### Step 3: Encoder pipeline integration (host-agent)

In `apps/host-agent/src/encoder/pipeline.rs` (existing):
- Replace the static `plan_ffmpeg_pipewire_h264` / `plan_ffmpeg_h264` with a dispatch through `plan_ffmpeg_args`.
- If `prefs.hw_encoder == Some("auto")` or unset, call `detect_best_encoder(codec).await` and use the result.
- If `prefs.hw_encoder == Some("software")`, use the existing software path.
- If `prefs.hw_encoder == Some("h264_nvenc")` (etc.), use that encoder directly and skip the probe.

### Step 4: Wire format compatibility

HW encoders produce a normal H.264/HEVC/AV1 bitstream; the existing `WireAccessUnitHeader { codec: VideoCodec }` transport doesn't change. The HW encoder's `h264_metadata=aud=insert` bsf is **not** required when the encoder's stream already has AUDs (NVENC/AMF/QSV all emit AUDs by default; VAAPI does not — keep the bsf for VAAPI only).

### Step 5: Probe results in the `start-session` config

The host's `HostSession` struct gains a `selected_encoder: Option<HwEncoder>` field, populated by the probe and exposed via the `host-agent list-encoders` CLI command for debugging.

### Step 6: Tests

- Unit test: `plan_ffmpeg_args` for each `(family, codec)` returns the canonical low-latency line.
- Integration test (manual, on a real machine): start host-agent with each encoder and verify end-to-end decode on the client.
- Xephyr smoke test: verify software fallback still works when no HW encoder is present (the dev box has no GPU).
- Latency benchmark: capture-to-display latency in ms for each encoder at 1080p60, 1440p120, 4K144. Use a stopwatch on a frame counter.

## Risks and Open Questions

- **Driver compatibility**: NVENC, QSV, AMF, and VideoToolbox all have version-specific bugs. The probe (`try_no_op_encode`) catches most of them but not all (e.g. NVENC driver 535.x has a bug where H.265 HDR metadata is dropped). Plan for a `hw_encoder_min_driver_version` field in the host config.
- **AV1 in B-frames**: Ada+ NVENC and RDNA3+ AMF support AV1 B-frames but with very high bitrate overhead. The `-bf 0` default is correct; a future preset could enable B-frames for desktop text (SCC).
- **Color space**: HW encoders default to Rec.709 8-bit. For HDR (P2-14) and 10-bit wide-gamut capture we'll need explicit `-color_range tv -colorspace bt2020nc -color_trc smpte2084` plumbing.
- **Cross-driver stability on Linux**: VAAPI works on Mesa + iHD + AMDVLK + the Nvidia proprietary driver (which exposes NVENC directly). The `vainfo` failure modes are different per driver; our probe must be tolerant.
- **Audio**: HW encoder templates above don't touch audio. The existing pipewire/pulse audio path is unaffected.
- **Encoder restart cost on bitrate change**: NVENC/AMF accept `-b:v` changes per frame, but VAAPI/QSV require encoder re-init. P0-4's adaptive bitrate must coalesce bitrate changes to ≤1 Hz to avoid per-second reinit overhead on VAAPI/QSV.
- **`-fflags nobuffer` is a demuxer flag and doesn't apply to our stdin capture**, but it's still useful when the host reads from a V4L2 / video4linux2 source. Don't apply to our x11grab/dshow/avfoundation pipeline. (Confirmed by our earlier debugging: `-fflags nobuffer` on a short stdin H.264 stream caused "No filtered frames".)

## References

- ffmpeg HWAccelIntro: https://trac.ffmpeg.org/wiki/HWAccelIntro
- AMD AMF recommended ffmpeg settings: https://github.com/GPUOpen-LibrariesAndSDKs/AMF/wiki/Recommended-FFmpeg-Encoder-Settings
- NVIDIA ffmpeg low-latency thread: https://forums.developer.nvidia.com/t/ffmpeg-and-low-latency-h264-streaming/249906
- StreamFX NVENC wiki: https://github.com/Vhonowslend/StreamFX-Public/wiki/Encoder-FFmpeg-NVENC
- ffmpeg ffmpeg.html: https://ffmpeg.org/ffmpeg.html
- Streaming Media ffmpeg 5.0+: https://www.streamingmedia.com/Articles/Editorial/Featured-Articles/How-to-Encode-with-FFmpeg-5.0-152090.aspx
- Perplexity research, 2026-07-02: HW encoder matrix, low-latency tuning, runtime probe.
