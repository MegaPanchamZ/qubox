# ADR-003 In-process H.264/H.265/AV1 decoder via ffmpeg-next

## Status

Proposed.

## Context

The current client (`apps/client-cli/src/main.rs::RunningFrameDecoder::spawn`)
spawns `ffmpeg` as a subprocess and pipes:

- H.264/H.265/AV1 access units (Annex B) into the ffmpeg process's stdin
- BGRA rawvideo out of the ffmpeg process's stdout

The decoder thread reads exactly `width*height*4` bytes per decoded frame
from stdout and forwards them to the winit main thread via
`tokio::sync::mpsc` + `EventLoopProxy::send_event(WakeUp)`.

This works but has three structural costs:

1. **Process boundary**: each decoded frame pays for at least one
   `write(stdin)` and one `read(stdout)` syscall, a pipe-buffer copy
   (the OS copies bytes between the two address spaces), and a context
   switch into the ffmpeg process. For a 1080p BGRA frame at 60 fps that
   is 60 × (2 × 1920 × 1080 × 4) ≈ 1 GB/s of kernel-mediated copies, on
   top of the decode itself.

2. **CLI flag surface**: every low-latency knob is a CLI flag that the
   ffmpeg team can rename or remove (`-fflags nobuffer`,
   `-avioflags direct`, `-flush_packets`, `-probesize`,
   `-analyzeduration`, `-fflags +flush_packets`, etc.). We've already
   hit one bug where `-fflags nobuffer` produces zero decoded frames on
   a small stdin stream (2026-07-02 incident). The remediation requires
   either a flags recipe (brittle) or owning the decoder.

3. **Re-encoding / hardware decode**: hardware-accelerated decode
   (`-hwaccel cuda`, `-c:v h264_qsv`, VideoToolbox on macOS, VAAPI /
   Vulkan Video) is per-platform and not exposed through the same
   stable CLI surface. With `ffmpeg-next` we can pick the best
   `AVCodec` for the host's available backends and the negotiated
   codec, set the low-delay flag programmatically, and surface
   per-frame metadata (QP, frame type, decode time) into the protocol.

## Decision

Replace the subprocess decoder with `ffmpeg-next` (Rust bindings to
libavformat / libavcodec / libswscale) inside the `client-cli` binary.

### Module layout

```
crates/qubox-media/src/
  decoder.rs       // trait Decoder: decode(access_unit) -> Option<DecodedFrame>
  ffnext.rs        // FfmpegNextDecoder: implements Decoder via libavcodec
  swscale.rs       // BGRA conversion via libswscale
apps/client-cli/src/
  decoder.rs       // wraps Decoder, owns the waker thread / channel
```

The `Decoder` trait is codec-agnostic. The wire format already
distinguishes H.264/H.265/AV1 via `VideoCodec`; the decoder picks the
right `AVCodec` for it.

### Latency / control improvements

- Feed each access unit directly via `avcodec_send_packet` — no pipe
  buffer, no syscall, no extra copy.
- Pull decoded frames with `avcodec_receive_frame`. Multiple frames can
  be queued by the decoder (B-frame reordering); we keep only the
  newest.
- Convert YUV → BGRA with `sws_scale` into a reusable scratch buffer;
  copy directly into the winit `softbuffer::Surface` (or the eventual
  GPU surface).
- Hardware decode: probe the host for `AVCodec` capabilities
  (`avcodec_get_hw_config`) and pick the best hardware backend
  (`vaapi`, `cuda`, `videotoolbox`, `qsv`, `vulkan`, etc.) before
  falling back to software.
- Programmatic low-delay: `AVDictionary` entries (`tune=zerolatency`,
  `flags=+low_delay`) replace the brittle CLI flag recipe.

### Codec coverage

The current subprocess path supports H.264 / H.265 / AV1; ffmpeg-next
covers all three via the same `AVCodec` dispatch. No wire format
changes.

### Audio

Out of scope for this ADR. Audio remains decoded by cpal (output
device) and is fed raw PCM from the transport.

### Build & deploy

`ffmpeg-next` 7.x ships prebuilt static libraries for the major
platforms but our dev box needs ffmpeg's `-dev` packages installed
(`libavcodec-dev`, `libavformat-dev`, `libswscale-dev`, `libavutil-dev`
on Debian / Ubuntu; via Homebrew on macOS; via MSYS2 / vcpkg on
Windows). Cross-compile to Windows-x86_64 still requires
`x86_64-pc-windows-gnu` + the MinGW-built ffmpeg dev libraries. Our
existing `dist/windows-x86_64/README.md` and `dist/windows-x86_64/build.sh`
need an extra `pacman -S mingw-w64-x86_64-ffmpeg` step.

A `bundled` feature can be added (like `ffmpeg-next/bundled`) to
vendor a known-good ffmpeg version into the build, eliminating the
host dev-dependency. This makes the build hermetic at the cost of a
~80 MB static link.

### Rollout

1. Add `ffmpeg-next` to `crates/qubox-media/Cargo.toml` behind
   a `ffnext` feature flag (default off initially).
2. Port `RunningFrameDecoder` to use the trait + a feature-selected
   impl.
3. Keep the subprocess path as `RunningFrameDecoder::spawn_subprocess`
   behind the inverse flag for one release cycle — e2e parity tests on
   both paths.
4. Cut over to the in-process decoder after one release of soak time.

## Consequences

### Positive

- Lower latency: removes the subprocess + pipe round-trip per access
  unit. Expect 5-15 ms improvement on 60 fps streams at 1080p.
- Hardware decode unlocks 4K60+ on modest hardware (the current
  software path is CPU-bound at higher resolutions).
- Stable low-delay knobs via `AVDictionary` instead of CLI strings.
- Per-frame metadata (QP, frame type, decode time) can be exposed in
  the protocol for adaptive bitrate / quality-aware pacing.
- Eliminates the `-fflags nobuffer` / `-avioflags direct` /
  `-flush_packets` CLI flag recipe (and the bugs that come with it).

### Negative

- Requires ffmpeg dev packages on every build host until we add a
  `bundled` feature. Cross-compile to Windows needs an extra
  `pacman -S mingw-w64-x86_64-ffmpeg` step.
- Larger binary (libavcodec is ~20 MB stripped even when statically
  linked).
- We're now responsible for managing `AVPacket` / `AVFrame` lifetime
  correctly; the ffmpeg-next wrapper is a thin binding, not a high-
  level decoder API.

### Risks

- `ffmpeg-next` API churn (libavcodec's public API is stable but the
  Rust crate has had breaking changes between 5.x and 7.x). Pin to
  one major version per release.
- Hardware decode paths have per-platform quirks. Smoke-test each
  backend on real hardware before claiming support.
- Memory: an `AVFrame` + scratch `sws_scale` buffer + the eventual
  surface buffer is ~30 MB for 4K. Make sure the budget holds.

## Alternatives considered

### A. Keep the subprocess path; only fix the CLI flags

Rejected. The pipe round-trip and the per-platform CLI surface remain.
We'd just be papering over the symptom.

### B. Use a higher-level wrapper (e.g. `ac-ffmpeg`, `ez-ffmpeg`)

Considered. Both provide nicer Rust APIs but neither has hardware
decode as a first-class concept. Stick with `ffmpeg-next` and build
our own thin wrapper for the parts we need.

### C. Use a pure-Rust decoder (`rav1d` for AV1, `dav1d` bindings, `openh264`)

Considered for the long term. None of the pure-Rust decoders match
libavcodec's coverage (especially hardware decode and B-frame
reordering). Worth tracking, not worth switching yet.
