# P2-18: Mobile & Web Clients (iOS, Android, Browser)

Status: research complete, implementation pending.
Owner: `apps/client-web/` (browser), `apps/client-mobile/` (iOS + Android). The existing Rust `client-cli` is the desktop reference; this doc covers the cross-platform web + mobile clients that sit alongside it.
Depends on: P0-1 (HW encode; per-OS encoder), P0-2 (datagram media path; the server-side must be stable), P0-4 (adaptive bitrate; required for cellular), P1-11 (TURN; required for NAT-traversal of WebTransport on cellular), P1-13 (daemon; host must be discoverable).
Blockers: WebTransport on Safari only landed in 2026 (Safari 26.4). Older Safari and iOS Safari cannot run the web client. WebCodecs full audio only from Safari 26.0. Firefox for Android does not support WebCodecs. Each platform has a different default for "what works".

## Goal

Extend the host-agent's audience to iOS / Android / browsers without giving up the sub-100ms end-to-end target. Three target clients:
1. **Web** (Chrome/Edge 113+, Firefox 130+, Safari 26.4+): WebTransport + WebCodecs + WebGPU. One HTML/JS bundle.
2. **iOS** (16.4+ for video-only, 26+ for full): Swift, VideoToolbox, GameController, custom QUIC stack via `quiche-swift` or `swift-quic` (no native QUIC in iOS 18).
3. **Android** (10+): Kotlin, MediaCodec via `Media3` (or direct), Java/Kotlin QUIC stack via `msQuic`/`cronet`/`quiche-android`.

All three consume the same `WireAccessUnit` + `RemoteInputEvent` protocol from the host-agent (extended per P0-2 with QUIC datagrams). They differ only in their platform-native decode / display / input adapters.

## Research Summary

### Web client stack (Chromium 2024-2026)

The practical 2024-2026 stack is **WebTransport + WebCodecs + WebGPU**:

- **WebTransport** (W3C TR, IETF `draft-ietf-webtrans-http3`):
  - Runs over HTTP/3 + QUIC.
  - Two primitives: **bidirectional reliable streams** and **datagrams** (unreliable, unordered, low-latency).
  - Browser support (MDN + Can I Use):
    - Chrome 97+ (Mar 2022), Edge 97+, Firefox 114+ (Jun 2023), **Safari 26.4+ (2026)**.
    - Server-side: nginx 1.25+ with `--with-http_v3_module` and a recent `quictls`/BoringSSL; Envoy 1.21+ has experimental support; Cloudflare supports WebTransport as of 2023.
  - **No server push**; this is a session-based, bidirectional client-initiated model.
  - Requires HTTP/3 on the transport path.
- **WebCodecs** (W3C TR):
  - Browser-native video/audio encoders + decoders with hardware acceleration.
  - Surfaces: `VideoDecoder` / `AudioDecoder` (decode), `VideoEncoder` / `AudioEncoder` (encode), `VideoFrame` / `EncodedVideoChunk` (data).
  - Browser support:
    - Chrome 94+ (Sep 2021), Edge 94+, **Firefox 130+ (Sep 2024)**, **Safari 16.4 partial (video only) → 26.0+ full**.
    - **Firefox for Android: no support**.
  - Requires a separate demuxer (e.g. `mp4box.js`, `mediabunny`) to get from a container to `EncodedVideoChunk`. For a custom protocol (no container), this is trivial.
  - Per-frame control, low overhead, hardware-accelerated.
- **WebGPU** (W3C Candidate Recommendation):
  - Modern GPU API. Replaces WebGL2.
  - Browser support:
    - **Chrome 113+ (Apr 2023)** for the stable API; **Chrome 139+** added `importExternalTexture(VideoFrame)`.
    - Firefox 132+ (behind flag in 141), Safari 17+ on Apple Silicon, Android Chrome in late 2024.
  - **The zero-copy path**: `GPUDevice.importExternalTexture({ source: videoFrame })` samples a WebCodecs `VideoFrame` directly with no CPU copy. Best for low-latency display.
  - Fallback: `GPUQueue.copyExternalImageToTexture({ source: videoFrame }, ...)` for processing.
  - Benchmarks: ~2 ms per 1080p frame vs ~4 ms for WebGL2 fragment shader, ~45 ms for Canvas 2D `getImageData`.
- **Pointer Events** (W3C): unified input; multi-touch via `pointerId`.
- **Gamepad API** (W3C): standard; `navigator.getGamepads()`, `mapping === "standard"` for known controllers.
- **Screen Wake Lock API** (W3C): keep the screen on during play. `navigator.wakeLock.request("screen")`. Release on pause.

JS skeleton (from MDN + Chrome blog):

```js
// 1. Connect
const wt = new WebTransport(`https://${host}:4433`);
await wt.ready;
const stream = await wt.createBidirectionalStream();
const reader = stream.readable.getReader();
const writer = stream.writable.getWriter();

// 2. Decode
const decoder = new VideoDecoder({
  output: (frame) => {
    const external = device.importExternalTexture({ source: frame });
    // pass `external` to the WGSL render pass
    frame.close();
  },
  error: (e) => console.error(e),
});
decoder.configure({ codec: 'avc1.640028', codedWidth: 1280, codedHeight: 720 });

// 3. Receive
while (true) {
  const { value, done } = await reader.read();
  if (done) break;
  // value: Uint8Array with our 12-byte chunk header + H.264/AV1/Opus payload
  const chunk = new EncodedVideoChunk({
    type: isKeyframe ? 'key' : 'delta',
    timestamp: BigInt(timestampUs),
    data: payload,
  });
  decoder.decode(chunk);
}
```

### iOS native client stack (Swift)

- **Networking**: iOS 18 has no native QUIC API. Options:
  - `Cronet` via `cronet-ios` (Chromium's networking stack with QUIC): most production-ready.
  - `swift-quic` (Apple, experimental) or `quiche-swift` (Cloudflare binding).
  - WebSocket fallback (no datagrams; larger overhead).
- **Decode**: `VideoToolbox` (`VTDecompressionSession`) — hardware H.264/HEVC/AV1 (A17+/M3+). ~1-3 ms per 1080p frame.
- **Display**: `CAMetalLayer` + Metal; the `VTDecompressionSession` can output to a Metal texture directly.
- **Audio**: `AVAudioEngine` + Opus via the `opus-swift` bindings (or a `libopus` XCFramework).
- **Input**:
  - Touch: `UITouch` / `UIEvent` with multi-touch.
  - Gamepad: `GameController` (`GCController`, `GCExtendedGamepad`, `GCMicroGamepad`).
  - Mouse/keyboard: `UIResponder` chain + `UIKey` (iPadOS supports keyboards/pointers).
  - Motion: `CMMotionManager` for tilt / gyro (driving sims).
- **Thermal**: `ProcessInfo.processInfo.thermalState` and `NSProcessInfoThermalStateDidChangeNotification`. Adapt bitrate when state is `.serious` / `.critical`.
- **App Store**: must follow Apple's "cloud gaming" rules:
  - Each game available as a separate app, OR
  - A catalog app that doesn't bypass review / IAP.
  - The user must own the game on the host.
  - iPadOS supports trackpads and game controllers; iPhone does not support mouse natively (iOS 14+ supports pointer via accessibility only).
- **Best practice**: ship a Swift Package with a `xcframework` that exposes `startSession(host:completion:)`, the same shape as the Rust `client_cli` library.

### Android native client stack (Kotlin)

- **Networking**: `Cronet` (Google's QUIC stack) via the official `org.chromium.net:cronet-embedded` AAR. Production-ready; used by Chrome on Android and most Google apps.
  - Alternative: OkHttp's experimental HTTP/3 support (varies by version).
- **Decode**: `MediaCodec` (or `Media3` / ExoPlayer 1.4+) for H.264/HEVC/AV1; ~1-3 ms per 1080p frame on modern Snapdragon.
- **Display**: `SurfaceView` or `GLSurfaceView`; the `MediaCodec` output surface can be a `SurfaceTexture` rendered with OpenGL ES, or with Vulkan via `AHardwareBuffer`.
- **Audio**: `AudioTrack` (low-level) or `Media3` (higher-level). Opus via `libopus` JNI.
- **Input**:
  - Touch: `MotionEvent` with multi-touch via pointer indices.
  - Gamepad: `InputDevice` / `KeyEvent` / `MotionEvent`. Jetpack `GameController` library has cross-vendor mapping.
  - Keyboard/mouse: standard `KeyEvent` / `MotionEvent`.
  - Motion: `SensorManager` for tilt / gyro.
- **Thermal**: `PowerManager.getCurrentThermalStatus()` and `OnThermalStatusChangedListener`. Adapt bitrate at `MODERATE` / `SEVERE` / `CRITICAL` / `EMERGENCY`.
- **Google Play**: fewer restrictions than iOS; can ship a catalog-based streaming app.
- **Best practice**: ship an AAR library exposing a Kotlin API.

### Per-platform codec recommendations

| Platform | First-choice codec | Fallback |
|----------|--------------------|----------|
| Web (Chrome) | AV1 → HEVC → H.264 | VP9 |
| Web (Firefox) | H.264 → AV1 (no HEVC) | VP9 |
| Web (Safari 26.4+) | H.264 → HEVC (no AV1) | — |
| iOS | HEVC (preferred) → H.264 | ProRes 422 (no, too large) |
| Android (Snapdragon 8+) | AV1 → HEVC → H.264 | VP9 |
| Android (mid/low) | HEVC → H.264 | VP9 |

For our protocol, **H.264 is the lowest common denominator** that all 5 platforms decode. We ship H.264 by default and use a codec-negotiation handshake (already in `WireAccessUnitHeader.codec`) to upgrade to AV1 / HEVC when the client signals support.

### Cellular bitrate / resolution tiers (2024-2026)

These are the practical targets for interactive cloud gaming on mobile:

| Network | Downlink | Recommended tier | Codec |
|---------|----------|------------------|-------|
| 5G (sub-6 GHz) | 50-100 Mbps | 1080p60 @ 10-15 Mbps | H.264 High / HEVC |
| 5G (mmWave) | 200+ Mbps | 1440p60 @ 20 Mbps | HEVC / AV1 |
| LTE (good) | 10-20 Mbps | 1080p30 @ 5-7 Mbps | H.264 / HEVC |
| LTE (congested) | 5 Mbps | 720p30 @ 2-3 Mbps | H.264 |
| 3G / weak | < 2 Mbps | 480p15 @ 0.7-1.2 Mbps | H.264 Baseline |

The adaptive bitrate (P0-4) ladder should step **resolution first**, then **frame rate**, then **bitrate within a tier**. The P0-4 GCC rate controller handles the "stay in the current tier" logic.

### Touch input protocol

Pointer events map cleanly to a `RemoteInputEvent` extension. Add new variants to the existing enum:

```rust
pub enum RemoteInputEvent {
    MouseMove { x: f32, y: f32 },
    RelativeMouseMove { dx: i32, dy: i32 },
    MouseButton { button: u8, pressed: bool },
    MouseWheel { dx: i32, dy: i32 },
    Keyboard { key: String, pressed: bool, modifiers: u32 },
    Touch {        // NEW (P2-18)
        pointer_id: u32,   // multi-touch tracking
        phase: TouchPhase, // Down/Move/Up/Cancel
        x: f32, y: f32,    // normalized 0..1 in display space
        pressure: f32,     // 0..1
    },
    Gamepad { /* P0-6 */ },
    Pen { /* P2-15 */ },
}

pub enum TouchPhase { Down, Move, Up, Cancel }
```

On the web, `pointerdown` / `pointermove` / `pointerup` / `pointercancel` map directly. Multi-touch is handled by tracking `pointerId` on the client and only sending events for `pointerId`s that changed phase.

On iOS, `UITouch.phase` → `TouchPhase`. On Android, `MotionEvent.action` + `pointerIndex` → same.

### Gamepad / game-controller input

The web **Gamepad API** exposes a standard mapping. We add a single `Gamepad` variant to `RemoteInputEvent` (P0-6) that carries buttons + axes. On iOS `GCController` and on Android `InputDevice`, the same canonical 16-button / 4-axis layout is built.

For **motion** (gyro / accel) — driving sims, FPS aim — we add a separate `Motion` variant (v2) carrying 3-axis accel + 3-axis gyro. The server's uinput (Linux) / ViGEmBus (Windows) path on the host can synthesize this from the `gimx` or `DS4Windows` source if the user has motion hardware.

### Battery and thermal

The web client can use:
- **Screen Wake Lock API** to keep the display on during play.
- `navigator.connection` / `navigator.getBattery()` (limited browser support) to detect cellular and low battery.

The native iOS / Android clients use `ProcessInfo.thermalState` / `PowerManager.getCurrentThermalStatus()` to drop resolution, framerate, or bitrate when the device is under thermal stress.

### Screen orientation

Web: `window.matchMedia('(orientation: portrait)')` + `window.resize` listener; recompute the canvas / touch coordinate transform. Use `<meta name="viewport" content="width=device-width, initial-scale=1">`.

iOS: `viewWillTransition(to:with:)` callback; recompute the rendering surface and touch mapping.

Android: `onConfigurationChanged()` if `configChanges` is declared in the manifest; otherwise the Activity is recreated and the session must be re-established.

For game streaming, **landscape-locked is the norm**; allow rotation only for desktop / browser clients.

### Battery, performance, App Store notes

- **iOS App Store rules (2024-2026)**: cloud gaming apps must comply with the cloud gaming-specific guidelines. Each game is a separate app (mostly), or the catalog must not bypass review. Apple's stance has loosened since 2022 (Stadia web-only → GeForce NOW native in 2024) but the rules are still strict.
- **Google Play**: more permissive; catalog-based streaming is allowed.
- **Battery**: software decode is non-viable on phones for 1080p60; HW decode is required. The P0-3 HW decode plan extends naturally to the mobile clients (Android `MediaCodec` HW path, iOS `VideoToolbox` HW path).

### Crate / library matrix (2024-2026)

| Platform | Transport | Decode | Display | Input |
|----------|-----------|--------|---------|-------|
| Web (Chrome/Edge) | WebTransport (built-in) | WebCodecs (built-in) | WebGPU (built-in) | Pointer Events, Gamepad API (built-in) |
| Web (Firefox 130+) | WebTransport (built-in) | WebCodecs (built-in) | WebGPU (Firefox 132+, flag) | Pointer Events, Gamepad API |
| Web (Safari 26.4+) | WebTransport (built-in) | WebCodecs (built-in) | WebGPU (Apple Silicon) | Pointer Events |
| iOS | Cronet / `cronet-ios` (QUIC), or `URLSession` WebSocket fallback | VideoToolbox (built-in) | Metal / `CAMetalLayer` (built-in) | UIKit / GameController (built-in) |
| Android | Cronet (QUIC), or OkHttp HTTP/3 | MediaCodec (built-in), or `Media3` | SurfaceView / GLSurfaceView (built-in) | InputDevice / MotionEvent (built-in) |

**Note**: iOS 18 / Android 14 still don't ship a system QUIC API. The Cronet AAR/XCFramework is the de-facto choice for both platforms in 2024-2026.

### 2024-2026 status

- **WebTransport**: stable in Chromium 2022, Firefox 2023, **Safari 2026**. Server-side is widely deployed (Cloudflare, nginx 1.25+, Envoy).
- **WebCodecs**: stable in Chromium 2021, Firefox 2024, **Safari full 2026 (partial 2023)**.
- **WebGPU**: stable in Chromium 2023, **Firefox 2024 with flag, Safari 17+ Apple Silicon**, Android late 2024.
- **Media over QUIC (MoQ)**: pre-RFC (`draft-ietf-moq-transport-17`, Jan 2026). Cloudflare + Meta + Google have pilot deployments with hundreds of thousands of viewers. No native mobile player APIs; SDKs only. **Not suitable for interactive cloud gaming in 2026** — target latency is ~500ms glass-to-glass, far above the sub-100ms needed for gaming.
- **iOS QUIC**: Apple has shipped parts of QUIC in `Network.framework` (HTTP/3) but not a generic QUIC API. Most iOS apps use Cronet or a third-party library.
- **Android QUIC**: Cronet is production-grade. OkHttp 4.x has experimental support.
- **Cross-platform code reuse**: not feasible with a single codebase. The wire protocol (P0-2) is the common ground; the per-platform UI / decode / display is native.

## Implementation Plan

### Step 1: Wire protocol extensions (P2-18 deliverable)

`crates/qubox-proto/src/lib.rs`:
- Add `Touch { pointer_id, phase, x, y, pressure }` variant to `RemoteInputEvent`.
- Add `TouchPhase { Down, Move, Up, Cancel }` enum.
- Add `Motion { accel_x, accel_y, accel_z, gyro_x, gyro_y, gyro_z }` variant (v2).
- All with `#[serde(default)]` and `#[serde(tag = "kind", rename_all = "snake_case")]` to keep wire compatibility.

### Step 2: Signaling server extension (P2-18 deliverable)

`apps/signaling-server` already speaks WebTransport / QUIC. No changes needed beyond:
- Issue short-lived TURN credentials (P1-11).
- Accept `User-Agent` hints to negotiate codec (H.264 / HEVC / AV1).
- Accept `Connection-Type` hints to cap the initial bitrate (5G / LTE / 3G).

### Step 3: Web client

`apps/client-web/`:
- TypeScript + Vite + a thin Rust→WASM shim for the input codec (or pure TS).
- Uses WebTransport, WebCodecs, WebGPU, Pointer Events, Gamepad API, Screen Wake Lock.
- Codec negotiation on connect (capabilities advertisement).
- Render loop uses `importExternalTexture` (Chrome 139+) or `copyExternalImageToTexture` (older).
- Fallback to WebSocket + Canvas 2D (very low quality, but works on old browsers).
- Hosted as a static site; no backend other than the signaling server.

### Step 4: iOS client

`apps/client-mobile-ios/`:
- Swift Package: `QuboxKit` (an `xcframework`).
- Uses Cronet for QUIC.
- Uses VideoToolbox for H.264/HEVC decode.
- Uses `CAMetalLayer` + Metal for display.
- Uses `GameController` for gamepad, `UIKit` for touch, `CMMotionManager` for motion.
- Uses `ProcessInfo.thermalState` to adapt.
- Demo app: `QuboxIOS` SwiftUI app that wraps `QuboxKit.startSession(host:)`.

### Step 5: Android client

`apps/client-mobile-android/`:
- Kotlin library: `qubox-kit` (AAR).
- Uses Cronet for QUIC.
- Uses MediaCodec (or `Media3`) for H.264/HEVC/AV1 decode.
- Uses `SurfaceView` + OpenGL ES for display.
- Uses `InputDevice` / `MotionEvent` for input.
- Uses `PowerManager.getCurrentThermalStatus()` to adapt.
- Demo app: `QuboxAndroid` Jetpack Compose app.

### Step 6: Server-side codec support

`apps/host-agent`:
- Extend `VideoEncoderConfig` to include `HevcMain`, `Av1Main`, `HevcMain10`, `Av1Main10` (per P2-14 HDR).
- Runtime probe must report the host's codec capabilities to the client during handshake.
- FFmpeg (or ffmpeg-next) command lines for each codec.

### Step 7: Codec negotiation

The signaling server's connection handshake adds:
```
ClientHello {
  codecs: [H264, HEVC, AV1],
  max_resolution: (1920, 1080),
  max_fps: 60,
  hw_decoders: { H264: "VideoToolbox", HEVC: "VideoToolbox", AV1: "VideoToolbox" },
  connection_type: "wifi" | "5g" | "lte" | "3g",
}

ServerHello {
  codec: HEVC,            // best mutual
  max_resolution: (1920, 1080),
  max_fps: 60,
  initial_bitrate: 10000, // kbps, capped by connection_type
  stream_id: ...,
}
```

### Step 8: Testing

- **Web**: manual on Chrome desktop, Chrome Android, Firefox desktop, Safari 26+ macOS.
- **iOS**: manual on a real iPhone / iPad (no simulator for camera/motion; gamepad via Bluetooth or USB).
- **Android**: manual on a real Android device.
- CI: build only (compile the SDK). Runtime tests are manual on real hardware.

### Step 9: Distribution

- **Web**: static site hosted on Cloudflare Pages, GitHub Pages, or behind the host's signaling server.
- **iOS**: App Store (subject to Apple's cloud gaming rules).
- **Android**: Google Play (more permissive), or sideload APK.

### Step 10: Power / battery / thermal

- Web: use `navigator.wakeLock` to keep the screen on; release on pause.
- iOS: `ProcessInfo.thermalState` → reduce resolution / fps / bitrate.
- Android: `PowerManager.getCurrentThermalStatus()` → same.
- Both: cap initial bitrate based on `navigator.connection.effectiveType` (web) or `CTCarrier` / `NWPathMonitor` (native).

## Risks and Open Questions

- **WebTransport on Safari**: only landed in 2026. Older iPhones stuck on iOS 17 or 18 will not work. Provide a WebSocket fallback for those.
- **WebCodecs on Firefox for Android**: not supported. Users on Firefox for Android fall back to WebSocket + WASM decoder (high CPU, low quality).
- **WebGPU on Firefox**: behind a flag in 141. Production web client targets Chrome / Edge first; Firefox / Safari as best-effort.
- **iOS Cronet binary size**: ~5-10 MB extra to the IPA. Acceptable for a game streaming app.
- **iOS 17+ GameController**: changes to the API between iOS versions. Pin the deployment target to iOS 17 minimum.
- **Android MediaCodec on vendor ROMs**: Samsung, Xiaomi, Huawei have varying levels of HW decode reliability. `Media3` (ExoPlayer) is the safer abstraction.
- **App Store rejection**: Apple's cloud gaming rules are strict. The first version may need to be a "remote desktop" framing rather than a "cloud gaming" framing.
- **Google Play rejection**: less likely, but still possible. Submit early, get feedback.
- **Cellular costs**: the user may be on a metered connection. Show a "data usage" estimate (bitrate × time) in the UI; warn the user.
- **Battery**: long sessions drain the phone. Provide a "battery saver" preset (lower resolution, lower fps).
- **Multi-display**: not common on phones. Tablets (iPad) sometimes. Out of scope for v1; P1-7 covers desktop.
- **Gamepad motion / gyro**: limited browser support (`gamepad.vibrationActuator` is in the spec; motion not yet). Defer to v2.
- **Codec licensing**: H.264 has patent royalties for encoders (decoders are royalty-free). HEVC has a more complex patent pool. AV1 is royalty-free. AV1 is preferred for licensing, but the codec matrix above shows it isn't always available.

## References

- W3C WebTransport: https://www.w3.org/TR/webtransport/
- MDN WebTransport: https://developer.mozilla.org/en-US/docs/Web/API/WebTransport
- MDN WebTransport API: https://developer.mozilla.org/en-US/docs/Web/API/WebTransport_API
- caniuse WebTransport: https://caniuse.com/webtransport
- IETF draft-ietf-webtrans-http3: https://datatracker.ietf.org/doc/html/draft-ietf-webtrans-http3-15
- W3C WebCodecs: https://www.w3.org/TR/webcodecs/
- MDN WebCodecs API: https://developer.mozilla.org/en-US/docs/Web/API/WebCodecs_API
- MDN VideoDecoder: https://developer.mozilla.org/en-US/docs/Web/API/VideoDecoder
- MDN VideoFrame: https://developer.mozilla.org/en-US/docs/Web/API/VideoFrame
- caniuse WebCodecs: https://caniuse.com/webcodecs
- W3C WebGPU: https://www.w3.org/TR/webgpu/
- MDN importExternalTexture: https://developer.mozilla.org/en-US/docs/Web/API/GPUDevice/importExternalTexture
- MDN copyExternalImageToTexture: https://developer.mozilla.org/en-US/docs/Web/API/GPUQueue/copyExternalImageToTexture
- Chrome 139 WebGPU release: https://developer.chrome.com/blog/new-in-webgpu-139
- W3C Pointer Events: https://www.w3.org/TR/pointerevents3/
- W3C Gamepad API: https://www.w3.org/TR/gamepad/
- W3C Screen Wake Lock: https://www.w3.org/TR/screen-wake-lock/
- W3C Media Queries Level 5: https://www.w3.org/TR/mediaqueries-5/
- WebCodecs Fundamentals: https://webcodecsfundamentals.org/
- WebGPU + WebCodecs article: https://joanleon.dev/en/webgpu-video-browser/
- Apple VideoToolbox: https://developer.apple.com/documentation/videotoolbox
- Apple GameController: https://developer.apple.com/documentation/gamecontroller
- Apple ProcessInfo.thermalState: https://developer.apple.com/documentation/foundation/processinfo/thermalstate
- Android MediaCodec: https://developer.android.com/reference/android/media/MediaCodec
- Android PowerManager.getCurrentThermalStatus: https://developer.android.com/reference/android/os/PowerManager#getCurrentThermalStatus()
- Android GameController library: https://developer.android.com/games/agde
- Cronet for Android: https://developer.android.com/guide/topics/connectivity/cronet
- Cronet for iOS: https://github.com/chromium/cronet
- IETF draft-ietf-moq-transport: https://datatracker.ietf.org/doc/html/draft-ietf-moq-transport-17
- moxygen: https://github.com/facebook/moxygen
- moq-lite: https://github.com/niccokunzmann/moq-lite
- Cloudflare Calls (hosted MoQ): https://developers.cloudflare.com/calls/
- Apple cloud gaming rules: https://developer.apple.com/app-store/review/guidelines/#cloud-gaming
- Google Play cloud gaming policy: https://support.google.com/googleplay/android-developer/answer/9888077
- Moonlight: https://moonlight-stream.org/
- Parsec: https://parsec.app/
- NVIDIA CloudXR: https://developer.nvidia.com/cloudxr-sdk
- Forasoft protocol matrix: https://www.forasoft.com/learn/video-streaming/articles-streaming/protocol-comparison-matrix
- Perplexity research, 2026-07-02: WebTransport, WebCodecs, WebGPU, MoQ, mobile native stacks, App Store rules, cellular bitrate.
