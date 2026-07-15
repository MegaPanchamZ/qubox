# Windows + Linux media close-out

Skips Apple / Windows Store certs (org-owned). Status after this wave:

## Capture

| Path | Implementation | Fallback |
|------|----------------|----------|
| **Linux X11** | `X11RandrBackend` + `get_image` (existing) | — |
| **Linux Wayland** | `PipeWirePortalBackend` → FFmpeg `-f pipewire` raw BGRA (`QUBOX_PIPEWIRE_NODE`) | Soft frames in CI |
| **Windows DXGI** | `DxgiDuplicationSession` (`IDXGIOutputDuplication` → staging map) | FFmpeg `ddagrab` → gdigrab → soft |
| **Shared** | `FfmpegRawCaptureSession` (`ffmpeg_raw.rs`) for demuxer-backed BGRA | Soft |

Env:

- `QUBOX_SOFT_CAPTURE=1` / `CI` → soft frames
- `QUBOX_DXGI_REAL=1` → prefer real DXGI (Windows)
- `QUBOX_PIPEWIRE_NODE` → PipeWire node name (default `default`)
- `QUBOX_CAPTURE_WIDTH` / `HEIGHT` → soft/pipewire geometry

## Encode (host)

- FFmpeg plan: x11grab / pipewire / **lavfi ddagrab** / gdigrab via `plan_ffmpeg_*`
- Host `CaptureOrchestrator::build_platform_ffmpeg_args` picks OS source
- HW encoder probe: NVENC / VAAPI / QSV / AMF + no-op encode test (existing)

## Decode (client)

- `get_format` + preferred pixfmt order (VAAPI/D3D11/CUDA) — real
- `av_hwdevice_ctx_create`: **linked when `--features hw-decode`**; stub returns -1 otherwise → SW path
- Rebuild client: `cargo build -p qubox-client-cli --features hw-decode` (system FFmpeg ≥ 5)

## Signing (Linux)

- `ops/signing/sign-linux.sh` + `QUBOX_SIGN_DRY_RUN=1` → `.sha256` only
- `ops/signing/test-sign-linux-dry-run.sh` smoke
- Windows Authenticode / Store cert: **deferred** (user)

## Tests

```bash
cargo test -p qubox-display --lib
cargo test -p qubox-display --features pipewire --lib
cargo test -p qubox-media --lib
cargo test -p qubox-host-agent --bin qubox-host-agent build_platform
cargo test -p qubox-client-cli --lib
bash ops/signing/test-sign-linux-dry-run.sh
```

Windows GPU e2e: `crates/qubox-media/tests/test_dxgi_encoder.rs` (`cfg(windows)`).
