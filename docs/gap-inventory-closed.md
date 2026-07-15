# Gap inventory close-out (2026-07-15)

Maps the A–D inventory to code/docs/CI status.

## A. Platform media

| Item | Resolution |
|------|------------|
| Windows DXGI | `DxgiDuplicationSession` (Output Duplication → staging BGRA); FFmpeg `ddagrab` / gdigrab / soft fallback |
| Linux Wayland | `PipeWirePortalBackend` → FFmpeg `-f pipewire` raw BGRA; soft in CI |
| Linux X11 | `X11RandrBackend` + `get_image` (full) |
| Host orchestrator | `build_platform_ffmpeg_args`: x11grab / pipewire / ddagrab per OS |
| HW decode create | `get_format` real; `av_hwdevice_ctx_create` linked with `--features hw-decode` |
| wgpu import | Soft-fail `NotSupported` (no panic) |
| vkms/IddCx | Blank-overlay production path; logical set_display_state |
| Pen | Platform modules + stub devices |
| Rate feedback | `spawn_rate_feedback_with_hook` → `MediaBitrateSample` |
| macOS SCK | Soft capture only (out of this Win/Linux wave) |

See **`docs/win-linux-media-closeout.md`**.

## B. Ops / org

| Item | Resolution |
|------|------------|
| Linux signing | `sign-linux.sh` + `QUBOX_SIGN_DRY_RUN=1` sha256; `test-sign-linux-dry-run.sh` |
| Windows Store / Authenticode | **Deferred** (user) |
| TURN soak | `docs/operations/turn-soak.md` + CI `turn-soak.yml` |
| Managed HA | `docs/operations/managed-ha.md` |
| TUF in field | Update checker; 501 without checker (tested) |

## C. Polish

| Item | Resolution |
|------|------------|
| Tauri video | `StreamSurface` (CLI dual-window default) |
| QR | `src/lib/qr.ts` matrix + SVG |
| HDR | Existing flags + soft/DXGI color spaces |
| USB / mobile | Explicit out-of-scope |

## D. Testing

| Item | Resolution |
|------|------------|
| Display | soft + ffmpeg_raw + pipewire soft open (28 tests w/ pipewire) |
| Media plans | ddagrab/gdigrab/pipewire/x11 (71 lib tests) |
| Host | `build_platform_ffmpeg_args_*` |
| FileSync / daemon / GUI | prior waves |
| Xephyr e2e | CI `e2e-xephyr` + `QUBOX_REQUIRE_E2E` |
| Windows DXGI e2e | `test_dxgi_encoder.rs` (`cfg(windows)`) |

## Residual

- Paid Authenticode / Windows Store cert (user)
- Multi-site symmetric NAT soak beyond loopback coturn
- Apple notarization / ScreenCaptureKit real path
- Full DMA-BUF / D3D11 zero-copy into wgpu (soft-fail today)
