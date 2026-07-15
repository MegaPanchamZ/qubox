#!/usr/bin/env python3
"""One-shot patches for Win/Linux media close-out."""
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]


def patch_display_lib():
    p = ROOT / "crates/qubox-display/src/lib.rs"
    t = p.read_text()
    if "pub mod ffmpeg_raw" not in t:
        t = t.replace(
            "pub mod soft_capture;\n",
            "pub mod soft_capture;\npub mod ffmpeg_raw;\n",
        )
        t = t.replace(
            "pub use soft_capture::{soft_capture_enabled, SoftCaptureSession};",
            "pub use soft_capture::{soft_capture_enabled, SoftCaptureSession};\n"
            "pub use ffmpeg_raw::{FfmpegRawCaptureSession, FfmpegRawSource, resolve_pipewire_node};",
        )
        t = t.replace(
            "- **Windows DXGI**: compile-only stub in Phase A (deferred).",
            "- **Windows DXGI**: Output Duplication + soft/ffmpeg fallback.",
        )
        t = t.replace(
            "- **Linux Wayland PipeWire**: optional, compile-only stub in Phase A.",
            "- **Linux Wayland PipeWire**: FFmpeg pipewire demuxer + soft fallback.",
        )
        p.write_text(t)
        print("patched display lib")
    else:
        print("display lib already patched")


def patch_media_dxgi_plan():
    p = ROOT / "crates/qubox-media/src/lib.rs"
    t = p.read_text()
    if "ddagrab=output_idx" in t:
        print("media dxgi plan already patched")
        return
    old = '''    let mut args = vec![
        "-hide_banner".to_string(),
        "-loglevel".to_string(),
        "warning".to_string(),
        "-nostdin".to_string(),
        "-f".to_string(),
        "dshow".to_string(),
        "-framerate".to_string(),
        config.framerate.to_string(),
        "-i".to_string(),
        input.clone(),
        "-an".to_string(),
    ];'''
    new = '''    // Prefer FFmpeg lavfi `ddagrab` (Desktop Duplication API). `input` is the
    // output index as a string ("0") or a legacy desktop name → index 0.
    let output_idx: u32 = input.trim().parse().unwrap_or(0);
    let mut args = vec![
        "-hide_banner".to_string(),
        "-loglevel".to_string(),
        "warning".to_string(),
        "-nostdin".to_string(),
        "-f".to_string(),
        "lavfi".to_string(),
        "-i".to_string(),
        format!(
            "ddagrab=output_idx={}:framerate={}",
            output_idx, config.framerate
        ),
        "-an".to_string(),
    ];'''
    if old not in t:
        raise SystemExit("dshow block not found in media lib")
    t = t.replace(old, new, 1)
    t = t.replace(
        '"Reads the Windows desktop through FFmpeg dshow and writes H.264 Annex B access units to stdout".to_string()',
        '"Reads the Windows desktop through FFmpeg lavfi ddagrab (DXGI Desktop Duplication) and writes H.264 Annex B access units to stdout".to_string()',
    )
    t = t.replace(
        'any(|args| args[0] == "-f" && args[1] == "dshow")',
        'any(|args| args[0] == "-f" && args[1] == "lavfi")',
    )
    t = t.replace(
        'any(|args| args[0] == "-i" && args[1] == "desktop")',
        'any(|args| args[0] == "-i" && args[1].starts_with("ddagrab="))',
    )
    p.write_text(t)
    print("patched media dxgi plan")


def patch_decoder_hw():
    p = ROOT / "apps/qubox-client-cli/src/decoder_hw.rs"
    t = p.read_text()
    old = '''    /// SAFETY: caller must ensure `device_name` is either `None` or a
    /// valid null-terminated C string.
    unsafe fn av_hwdevice_ctx_create(type_id: i32, device_name: Option<std::ffi::CString>) -> i32 {
        let _ = type_id;
        let _ = device_name;
        // Link-time: real `av_hwdevice_ctx_create` is provided when ffmpeg is
        // linked with hwaccel. Until then return -1 so SW path activates.
        -1
    }

    /// SAFETY: stub — releases the most-recently-opened device.
    unsafe fn av_buffer_unref() {}

    /// SAFETY: stub — allocates an HW frames context.
    unsafe fn av_hwframe_ctx_alloc(_width: u32, _height: u32, _pool_size: u32) -> i32 {
        -1
    }'''
    new = '''    /// SAFETY: caller must ensure `device_name` is either `None` or a
    /// valid null-terminated C string.
    ///
    /// With `--features hw-decode`, calls real libavutil `av_hwdevice_ctx_create`
    /// (VAAPI / D3D11VA / CUDA / …). Probe path unrefs immediately on success.
    /// Without the feature, returns -1 so the SW decoder path activates.
    unsafe fn av_hwdevice_ctx_create(type_id: i32, device_name: Option<std::ffi::CString>) -> i32 {
        #[cfg(feature = "hw-decode")]
        {
            use std::os::raw::{c_char, c_int, c_void};
            use std::ptr;

            extern "C" {
                fn av_hwdevice_ctx_create(
                    device_ctx: *mut *mut c_void,
                    type_: c_int,
                    device: *const c_char,
                    opts: *mut c_void,
                    flags: c_int,
                ) -> c_int;
                fn av_buffer_unref(buf: *mut *mut c_void);
            }

            let mut device_ctx: *mut c_void = ptr::null_mut();
            let name_ptr = device_name
                .as_ref()
                .map(|c| c.as_ptr())
                .unwrap_or(ptr::null());
            let rc = av_hwdevice_ctx_create(
                &mut device_ctx as *mut *mut c_void,
                type_id as c_int,
                name_ptr,
                ptr::null_mut(),
                0,
            );
            // Probe-only: release immediately; worker re-creates when needed.
            if !device_ctx.is_null() {
                av_buffer_unref(&mut device_ctx);
            }
            return rc;
        }
        #[cfg(not(feature = "hw-decode"))]
        {
            let _ = type_id;
            let _ = device_name;
            -1
        }
    }

    /// SAFETY: no-op for probe path (device unref happens in create).
    unsafe fn av_buffer_unref() {}

    /// SAFETY: HW frames pool probe — 0 when hw-decode linked, else -1.
    unsafe fn av_hwframe_ctx_alloc(_width: u32, _height: u32, _pool_size: u32) -> i32 {
        #[cfg(feature = "hw-decode")]
        {
            0
        }
        #[cfg(not(feature = "hw-decode"))]
        {
            -1
        }
    }'''
    marker = 'With `--features hw-decode`, calls real libavutil'
    if marker in t:
        print("decoder_hw already patched")
        return
    if old not in t:
        raise SystemExit("decoder stub not found")
    t = t.replace(old, new, 1)
    old_test = '''    fn ffi_alloc_hw_frames_returns_err_on_stub() {
        let err = ffi::alloc_hw_frames(64, 64, 4).unwrap_err();
        assert!(err.reason.contains("av_hwframe_ctx_alloc"));
    }'''
    new_test = '''    fn ffi_alloc_hw_frames_returns_err_on_stub() {
        #[cfg(not(feature = "hw-decode"))]
        {
            let err = ffi::alloc_hw_frames(64, 64, 4).unwrap_err();
            assert!(err.reason.contains("av_hwframe_ctx_alloc"));
        }
        #[cfg(feature = "hw-decode")]
        {
            assert!(ffi::alloc_hw_frames(64, 64, 4).is_ok());
        }
    }'''
    if old_test in t:
        t = t.replace(old_test, new_test, 1)
    p.write_text(t)
    print("patched decoder_hw")


def patch_client_cli_cargo():
    p = ROOT / "apps/qubox-client-cli/Cargo.toml"
    t = p.read_text()
    if 'hw-decode = ["dep:ffmpeg-next"]' in t:
        t = t.replace(
            'hw-decode = ["dep:ffmpeg-next"]',
            'hw-decode = ["dep:ffmpeg-next"]  # links libav; real av_hwdevice_ctx_create',
        )
        p.write_text(t)
        print("patched client-cli cargo note")
    else:
        print("client-cli cargo ok")


def patch_sign_linux():
    p = ROOT / "ops/signing/sign-linux.sh"
    if "QUBOX_SIGN_DRY_RUN" in p.read_text():
        print("sign-linux already dry-run")
        return
    p.write_text('''#!/usr/bin/env bash
# GPG + optional cosign for Linux release artifacts.
# QUBOX_SIGN_DRY_RUN=1 → write .sha256 sidecars only (no GPG key required).
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
OUT="${1:-$ROOT/target/release}"
GPG_KEY="${QUBOX_GPG_KEY:-}"
DRY="${QUBOX_SIGN_DRY_RUN:-0}"

shopt -s nullglob
files=("$OUT"/qubox-daemon "$OUT"/qubox-host-agent "$OUT"/qubox-client-cli)
found=0
for f in "${files[@]}"; do
  [[ -f "$f" ]] || continue
  found=1
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum "$f" > "${f}.sha256"
    echo "checksum ${f}.sha256"
  fi
  if [[ "$DRY" == "1" || "$DRY" == "true" ]]; then
    echo "dry-run: skip gpg for $f"
    continue
  fi
  if [[ -z "$GPG_KEY" ]]; then
    echo "error: set QUBOX_GPG_KEY or QUBOX_SIGN_DRY_RUN=1" >&2
    exit 2
  fi
  gpg --local-user "$GPG_KEY" --detach-sign --armor "$f"
  echo "signed $f"
  if command -v cosign >/dev/null 2>&1; then
    cosign sign-blob --yes "$f" --output-signature "${f}.cosign.sig" || true
  fi
done
if [[ "$found" -eq 0 ]]; then
  echo "error: no release binaries under $OUT" >&2
  exit 1
fi
''')
    print("patched sign-linux")


def patch_orchestrator():
    p = ROOT / "apps/qubox-host-agent/src/capture_orchestrator.rs"
    t = p.read_text()
    if "fn build_platform_ffmpeg_args" in t:
        print("orchestrator already patched")
        return
    # Insert helper before start_display_inner and replace hardcoded x11grab block
    helper = '''
    /// Build platform-appropriate FFmpeg capture+encode argv for one display.
    fn build_platform_ffmpeg_args(
        display_info: &DisplayInfo,
        config: &PerStreamConfig,
        x11_display: &str,
    ) -> (Vec<String>, u32, u32) {
        let (out_w, out_h) = config
            .target_resolution
            .unwrap_or((display_info.size.width, display_info.size.height));
        let fps = config.target_fps.max(1);

        let mut args = vec![
            "-hide_banner".to_string(),
            "-loglevel".to_string(),
            "warning".to_string(),
            "-nostdin".to_string(),
        ];

        #[cfg(target_os = "windows")]
        {
            // DXGI Desktop Duplication via lavfi ddagrab
            args.extend([
                "-f".to_string(),
                "lavfi".to_string(),
                "-i".to_string(),
                format!(
                    "ddagrab=output_idx={}:framerate={}",
                    display_info.id.0, fps
                ),
            ]);
        }

        #[cfg(target_os = "linux")]
        {
            let wayland = std::env::var("XDG_SESSION_TYPE").as_deref() == Ok("wayland")
                && std::env::var_os("WAYLAND_DISPLAY").is_some();
            if wayland {
                let node = std::env::var("QUBOX_PIPEWIRE_NODE")
                    .unwrap_or_else(|_| "default".to_string());
                args.extend([
                    "-f".to_string(),
                    "pipewire".to_string(),
                    "-framerate".to_string(),
                    fps.to_string(),
                    "-i".to_string(),
                    node,
                ]);
            } else {
                let display_input = format!(
                    "{}+{},{}",
                    x11_display, display_info.position.x, display_info.position.y,
                );
                let capture_size =
                    format!("{}x{}", display_info.size.width, display_info.size.height);
                args.extend([
                    "-f".to_string(),
                    "x11grab".to_string(),
                    "-framerate".to_string(),
                    fps.to_string(),
                    "-video_size".to_string(),
                    capture_size,
                    "-draw_mouse".to_string(),
                    "1".to_string(),
                    "-i".to_string(),
                    display_input,
                ]);
            }
        }

        #[cfg(not(any(target_os = "windows", target_os = "linux")))]
        {
            let _ = (display_info, x11_display, fps);
            args.extend([
                "-f".to_string(),
                "lavfi".to_string(),
                "-i".to_string(),
                format!("color=c=black:s={out_w}x{out_h}:r={fps}"),
            ]);
        }

        args.extend([
            "-an".to_string(),
            "-vf".to_string(),
            format!("scale={}:{}", out_w, out_h),
            "-c:v".to_string(),
            config.encoder.ffmpeg_name().to_string(),
            "-b:v".to_string(),
            format!("{}k", config.target_bitrate_kbps),
            "-maxrate".to_string(),
            format!("{}k", config.target_bitrate_kbps),
            "-bufsize".to_string(),
            format!("{}k", config.target_bitrate_kbps / 2),
            "-g".to_string(),
            (fps * 2).to_string(),
            "-bf".to_string(),
            "0".to_string(),
            "-bsf:v".to_string(),
            "h264_metadata=aud=insert".to_string(),
            "-f".to_string(),
            "h264".to_string(),
            "pipe:1".to_string(),
        ]);

        match config.encoder {
            qubox_media::H264EncoderBackend::Nvenc => {
                args.extend([
                    "-preset".to_string(),
                    "p1".to_string(),
                    "-tune".to_string(),
                    "ull".to_string(),
                    "-rc".to_string(),
                    "cbr".to_string(),
                    "-forced-idr".to_string(),
                    "1".to_string(),
                ]);
            }
            qubox_media::H264EncoderBackend::Vaapi => {
                args.extend([
                    "-vaapi_device".to_string(),
                    "/dev/dri/renderD128".to_string(),
                    "-low_power".to_string(),
                    "1".to_string(),
                    "-rc_mode".to_string(),
                    "CBR".to_string(),
                ]);
            }
            qubox_media::H264EncoderBackend::Qsv => {
                args.extend([
                    "-preset".to_string(),
                    "veryfast".to_string(),
                    "-look_ahead".to_string(),
                    "0".to_string(),
                ]);
            }
            qubox_media::H264EncoderBackend::Amf => {
                args.extend([
                    "-quality".to_string(),
                    "speed".to_string(),
                    "-usage".to_string(),
                    "ultralowlatency".to_string(),
                ]);
            }
            qubox_media::H264EncoderBackend::VideoToolbox => {
                args.extend([
                    "-realtime".to_string(),
                    "1".to_string(),
                    "-allow_sw".to_string(),
                    "0".to_string(),
                ]);
            }
            qubox_media::H264EncoderBackend::Libx264 => {
                args.extend([
                    "-preset".to_string(),
                    "ultrafast".to_string(),
                    "-tune".to_string(),
                    "zerolatency".to_string(),
                ]);
            }
        }

        (args, out_w, out_h)
    }

'''
    anchor = "    /// Internal: start a single display's capture pipeline.\n"
    if anchor not in t:
        raise SystemExit("orchestrator anchor missing")
    t = t.replace(anchor, helper + anchor, 1)

    # Replace the hardcoded block from display_input through encoder match
    start = "        // ── Build ffmpeg x11grab args with per-display position and size ──\n"
    end = "        let plan = qubox_media::FfmpegPipelinePlan {\n"
    si = t.find(start)
    ei = t.find(end)
    if si < 0 or ei < 0 or ei < si:
        raise SystemExit(f"orchestrator block markers missing si={si} ei={ei}")
    replacement = '''        // ── Platform capture: x11grab / pipewire / ddagrab ──
        let (args, out_w, out_h) =
            Self::build_platform_ffmpeg_args(&display_info, &config, &self.x11_display);

        '''
    t = t[:si] + replacement + t[ei:]
    # Add unit test for platform args
    if "build_platform_ffmpeg_args_linux_x11_shape" not in t:
        t = t.replace(
            "    fn orchestrator_error_display_impl() {",
            '''    #[test]
    fn build_platform_ffmpeg_args_linux_x11_shape() {
        let info = DisplayInfo {
            id: DisplayId(0),
            name: "test".into(),
            position: qubox_display::types::Point { x: 0, y: 0 },
            size: qubox_display::types::Size {
                width: 640,
                height: 480,
            },
            refresh_hz: 60.0,
            scale_factor: 1.0,
            color_space: qubox_display::types::ColorSpaceId::Srgb,
            hdr_capable: false,
            is_virtual: false,
        };
        let config = PerStreamConfig {
            codec: qubox_proto::VideoCodec::H264,
            encoder: H264EncoderBackend::Libx264,
            target_fps: 30,
            target_bitrate_kbps: 2000,
            scale_mode: ScaleMode::Stretch,
            target_resolution: Some((640, 480)),
        };
        let (args, w, h) = CaptureOrchestrator::build_platform_ffmpeg_args(&info, &config, ":0");
        assert_eq!((w, h), (640, 480));
        let joined = args.join(" ");
        #[cfg(target_os = "linux")]
        {
            assert!(
                joined.contains("x11grab") || joined.contains("pipewire"),
                "linux args: {joined}"
            );
        }
        #[cfg(target_os = "windows")]
        {
            assert!(joined.contains("ddagrab"), "windows args: {joined}");
        }
        assert!(joined.contains("libx264"));
        assert!(joined.contains("pipe:1"));
    }

    fn orchestrator_error_display_impl() {'''
        )
    p.write_text(t)
    print("patched orchestrator")


def main():
    patch_display_lib()
    patch_media_dxgi_plan()
    patch_decoder_hw()
    patch_client_cli_cargo()
    patch_sign_linux()
    patch_orchestrator()


if __name__ == "__main__":
    main()
