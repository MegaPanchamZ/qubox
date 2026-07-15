# ADR-017 WebCodecs + WebTransport Browser Client

## Status

Proposed. Branch: `feature/adr-017-webcodecs-webtransport`. Based on
`main` after commit `47585ea`. Builds on ADR-011 (QUIC v2 + MASQUE),
ADR-014 (Reed-Solomon FEC k=10/m=2), ADR-015 (rkyv serialization for
JS-readable binary protocol), and ADR-016 (zero-copy surfaces — on the
browser side this is irrelevant since WebCodecs has its own GPU
pipeline). Closes P2-18 (mobile-web) on the client side; the host
side is unchanged.

## Context

The user's research dump asks for a browser-native client so a
mobile-web user can connect to a desktop host without installing a
native app. The research names three enabling technologies:

1. **WebTransport** (W3C): HTTP/3 + WebSockets hybrid transport over
   a single QUIC connection. Exposes `datagrams` (RFC 9221) — exactly
   what we need for media — and `bidirectional streams` for control.
2. **WebCodecs** (W3C): low-level codec API exposing `VideoDecoder`,
   `AudioDecoder`, etc. AV1, HEVC, H.264 decoders ship in modern
   browsers. Crucially: WebCodecs accepts a `VideoFrame` that can be
   uploaded zero-copy into a WebGL2 texture.
3. **Insertable Streams for WebCodecs** (W3C, draft + emerging
   `TransformStream`-based pattern): lets us apply JS-side FEC decode
   between the WebTransport datagram reader and `VideoDecoder.decode()`
   via a `TransformStream`. *Caveat:* as of 2026 the W3C WebCodecs
   spec does not yet expose native `decoder.readable` / `decoder.writable`
   on all browsers; we wrap the callback API in a `TransformStream`
   ourselves. This is the de-facto pattern documented by
   web.dev, MDN, and the WebCodecs breakout session minutes.

### Why not WebRTC?

1. **Datagram protocol mismatch**: WebRTC's RTP/RTCP has its own CC
   (GCC/SCReAM) and SRTP layer. Our native QUIC stack already has
   ACK-Frequency (ADR-011), SCReAM (ADR-012), and rkyv (ADR-015).
   Re-encoding to fit RTP is a non-trivial re-architecture.
2. **RTCPeerConnection has been deprecated for non-SFU use** (Chrome's
   2024 deprecation notice); the modern path is WebTransport.
3. **We already have `crates/qubox-signaling`** (ADR-005). Adding a
   WebTransport endpoint is incremental.

### Why not WASM-compile `qubox-client-cli`?

- Native client is ~30 MB; WASM grows to ~80 MB and ships poorly on
  mobile.
- WASM does not have access to WebCodecs directly without
  cross-compilation glue.
- WebCodecs gives native HW decode on every modern mobile browser;
  WASM would only have the SW decoder (3–5× worse battery).

## Browser Support Matrix (as of 2026-Q2)

Versions are the **first stable shipping release** that includes the
feature (no flag, no origin trial). "—" means the feature has shipped
in some form on the same version as the base feature. See
`§13 Pitfalls` for the Safari version-numbering caveat
(Apple moved to year-based "26.x" in 2025; both nomenclatures refer
to the same browser engine version).

| Browser            | WebTransport | WT datagrams | `serverCertificateHashes` | WebCodecs `VideoDecoder` | Insertable Streams (TransformStream wrapper) |
|--------------------|--------------|--------------|----------------------------|---------------------------|----------------------------------------------|
| Chrome (desktop)   | 97           | 97           | 97 (initial; refinements ongoing) | 94 | 94 (callback API; TransformStream wrapper) |
| Chrome (Android)   | 149          | 149          | 149                        | 147                       | 147                                          |
| Edge (desktop)     | 98           | 97           | 98                         | 94                        | 94                                           |
| Edge (Android)     | 149 (engine) | 149          | 149                        | 147                       | 147                                          |
| Firefox (desktop)  | 114          | 114          | 114 (buggy until 136 — see §13) | 130                  | 130                                          |
| Firefox (Android)  | 152          | 152          | 152 (post-bug-fix)         | 130 (desktop engine)      | 130                                          |
| Safari (macOS)     | 17.4 / 26.4  | 17.5 / 26.4  | 17.5 / 26.4                | 16.4 (video only) → 26.0 (full) | 16.4 (video only) → 26.0 (full)         |
| Safari (iOS / iPadOS) | 17.4 / 26.4 | 17.5 / 26.4 | 17.5 / 26.4                | 16.4 → 26.0               | 16.4 → 26.0                                  |
| Samsung Internet   | 18.0         | 18.0         | 18.0                       | 17.0                      | 17.0                                         |

**Codec support within WebCodecs (per browser):**

| Codec | Chrome | Edge | Firefox | Safari macOS / iOS |
|-------|--------|------|---------|--------------------|
| H.264 (`avc1.42E01E` constrained baseline) | ✅ 94+ (HW) | ✅ 94+ (HW) | ✅ 130+ (HW via DXVA/VA-API) | ✅ 16.4+ (HW) |
| HEVC (`hev1.1.6.L93.B0` Main 4.1)          | ✅ 107+ on platforms with HEVC codec¹ | ✅ Edge aligned w/ Chromium 107 | ❌ Not shipped² | ✅ 16.4+ on A9+/Intel Macs (HW) |
| AV1 (`av01.0.04M.08` Main 4.0)             | ✅ 94+ on AV1-capable GPU³ | ✅ 94+ requires "AV1 Video Extension" on Win | ✅ 130+ | ✅ Only A17 Pro / M3+ / M4 iPad (HW)⁴ |

¹ Chrome HEVC requires OS-level HEVC codec (Windows HEVC Video Extensions; macOS VideoToolbox; Linux VA-API). 10-bit and alpha-channel HEVC require Chromium 108+.
² Firefox deliberately avoids HEVC due to patent licensing; do not rely on it.
³ Chrome uses bundled libaom as SW fallback; HW decode requires AV1-capable GPU (Intel Xe 11th-gen+, NVIDIA RTX 30+, AMD RX 6000+, Apple A17/M3+).
⁴ Safari AV1 is hardware-gated with **no system software fallback**; A16 and earlier iPhones, and Intel Macs, cannot decode AV1 at all.

## Decision

### 1. New crate `crates/qubox-webtransport` (not a module inside `qubox-signaling`)

The WebTransport server is a distinct HTTP/3+QUIC concern that pulls in
`rustls`/`quinn`/`wtransport`/`rcgen`/`sha2` — large dependency trees
that should not be paid by every `qubox-signaling` consumer
(cli, tests, integration crates). Reasoning (full trade-off analysis
in `§13 Pitfalls`):

- **Compile-time isolation.** A separate crate means
  `cargo build -p qubox-signaling` does not pull HTTP/3 stack.
- **Feature gating.** MASQUE-style fallback (ADR-011 §4) lives behind
  `qubox-webtransport`'s own feature flags without bloating
  `qubox-signaling`'s `Cargo.toml`.
- **Swappable back-end.** We can switch between `wtransport`,
  Salvo's Quinn-based WebTransport, or a future crate without
  touching `qubox-signaling`.

**Crate layout (new files):**

```
crates/qubox-webtransport/
├── Cargo.toml                  # see §2.1
└── src/
    ├── lib.rs                  # re-export public API; ~30 LOC
    ├── config.rs               # WebTransportConfig + builder; ~120 LOC
    ├── cert.rs                 # self-signed cert gen + SHA-256(DER); ~150 LOC
    ├── server.rs               # wtransport::Server wrapper; ~250 LOC
    ├── session.rs              # WebTransportSession handler; ~400 LOC
    └── tests.rs                # see §11
```

### 2. Server-side `Cargo.toml` additions

`crates/qubox-webtransport/Cargo.toml` (full file — the workspace
`Cargo.toml` at `/Cargo.toml` must add the new member; line 12-30
of workspace `Cargo.toml` lists `members = ["crates/*"]`):

```toml
[package]
name = "qubox-webtransport"
edition.workspace = true
version.workspace = true

[dependencies]
anyhow.workspace = true
qubox-proto = { path = "../qubox-proto" }
qubox-identity = { path = "../qubox-identity" }
rcgen = "0.13"
ring = "0.17"
sha2 = "0.10"
tokio.workspace = true
tracing.workspace = true
wtransport = { version = "0.5", features = ["self-signed"] }
```

`crates/qubox-signaling/Cargo.toml` gains one line:

```toml
qubox-webtransport = { path = "../qubox-webtransport", optional = true }
```

…and the workspace `Cargo.toml` features list adds
`webtransport = ["dep:qubox-webtransport"]`.

### 3. Server-side module `crates/qubox-signaling/src/webtransport.rs` (new)

This is a thin integration layer (~80 LOC) that wires
`qubox-webtransport` into the existing signalling flow:

- Re-uses the `PairingGrant` + `SessionCredential` HMAC machinery in
  `crates/qubox-signaling/src/lib.rs:910-921` to authenticate the
  WebTransport upgrade.
- Re-uses the `NativeQuicTicket` serializer at
  `crates/qubox-transport/src/lib.rs:1480-1488` `encode_ticket_b64`
  for the ticket wire format (zero host-side changes).
- Adds a new `TransportKind::WebTransport` variant to
  `crates/qubox-proto/src/lib.rs:97-101` (currently only `NativeQuic`,
  `WebRtc`, `RelayQuic` — `WebTransport` is missing).
- Adds `WebTransportTicket { url, server_certificate_hash }` to
  the existing `RelaySignal::NativeQuicTicket { alpn, ticket_b64 }`
  variant family at `crates/qubox-proto/src/lib.rs:937-940`.

#### 3.1 New `crates/qubox-signaling/src/webtransport.rs` (insertion point: top of `src/lib.rs:33` `mod` declarations — add `pub mod webtransport;` after the existing `pub mod` block at line ~33)

```rust
// crates/qubox-signaling/src/webtransport.rs
//! HTTP/3 + WebTransport bridge for browser clients.
//! Lives in qubox-signaling to share pairing/credential machinery;
//! the actual QUIC stack lives in crates/qubox-webtransport.

use std::sync::Arc;
use axum::{
    extract::{ws::WebSocketUpgrade, Path, State},
    response::Response,
    routing::get,
    Router,
};
use qubox_identity::DeviceIdentity;
use qubox_proto::{TransportKind, WebTransportTicket};
use qubox_webtransport::{WebTransportConfig, run_webtransport_server};

/// Mount the WebTransport endpoints under the existing router.
/// Call from `SignalingState::router()` (currently `lib.rs:291`):
///     `.merge(self.webtransport_router())`
pub fn router(state: Arc<SignalingState>) -> Router {
    Router::new()
        .route("/v1/webtransport/cert", get(cert_handler))
        .route("/v1/ws/:peer_id", get(ws_upgrade))
        .with_state(state)
}

async fn cert_handler(
    State(state): State<Arc<SignalingState>>,
) -> Result<Json<WebTransportTicket>, (StatusCode, String)> {
    let ticket = state.webtransport_cert_hash().await?;
    Ok(Json(ticket))
}
```

#### 3.2 `crates/qubox-signaling/src/lib.rs` updates

At `crates/qubox-signaling/src/lib.rs:291-304` (the `router()` method),
add a `.merge(webtransport::router(Arc::new(self.clone())))` call:

```rust
pub fn router(self) -> Router {
    Router::new()
        .route("/healthz", get(healthz))
        .route("/ws", get(ws_handler))
        .route("/v1/turn/relay-address", post(publish_relay_address_handler))
        .route("/v1/turn/relay-address/{peer_id}", get(get_relay_address_handler))
        .merge(webtransport::router(Arc::new(self)))   // <-- NEW
        .with_state(self)
}
```

At `crates/qubox-signaling/src/lib.rs:49-69` (`SignalingState` struct),
add a new field:

```rust
pub struct SignalingState {
    // ... existing fields ...
    webtransport_cert_hash: Arc<RwLock<Option<WebTransportTicket>>>,
}
```

At `crates/qubox-signaling/src/lib.rs:706` (after the existing
`async fn healthz()`), add:

```rust
impl SignalingState {
    pub async fn webtransport_cert_hash(&self) -> anyhow::Result<WebTransportTicket> {
        // Delegate to qubox-webtransport for actual generation.
        let cfg = WebTransportConfig::for_signaling_server(&self.pairing_store)?;
        qubox_webtransport::current_cert_hash(&cfg).await
    }
}
```

### 4. Server-side `crates/qubox-webtransport/src/cert.rs` (full file)

```rust
use rcgen::{generate_simple_self_signed, CertifiedKey};
use ring::digest::{digest, SHA256};
use sha2::{Digest, Sha256};

/// Generate a self-signed cert valid for `qubox.local` plus a SHA-256
/// of the **DER** bytes (which is what `serverCertificateHashes`
/// expects — NOT the SPKI hash).
pub fn generate_self_signed() -> anyhow::Result<(Vec<u8>, [u8; 32])> {
    let CertifiedKey { cert, key_pair } = generate_simple_self_signed(
        vec!["qubox.local".into(), "localhost".into()],
    )?;
    let der = cert.der().to_vec();

    // rcgen already gives us the DER; we compute SHA-256 of that DER.
    let mut hasher = Sha256::new();
    hasher.update(&der);
    let hash: [u8; 32] = hasher.finalize().into();

    Ok((der, hash))
}
```

**Do NOT use `ring::digest::digest(&SHA256, cert.der())` alone** — it
works, but using `sha2` keeps the project dependency-uniform (the
workspace already uses `sha2`). Both produce the identical 32-byte
SHA-256.

### 5. Server-side `crates/qubox-webtransport/src/server.rs` (full file)

```rust
use std::net::SocketAddr;
use anyhow::Context;
use wtransport::{ServerConfig, Endpoint, Server};
use crate::cert::generate_self_signed;

pub async fn run(listen: SocketAddr) -> anyhow::Result<()> {
    let (der, hash) = generate_self_signed()?;
    tracing::info!(cert_hash = hex::encode(hash), "WebTransport cert generated");

    let config = ServerConfig::builder()
        .with_bind_address(listen)
        .with_certificate(der) // PEM or DER; wtransport handles both
        .build();

    let server = Server::new(config);
    server
        .accept(crate::session::handle)
        .await
        .context("WebTransport server crashed")?;
    Ok(())
}
```

### 6. Server-side `crates/qubox-webtransport/src/session.rs` (full file — abridged)

```rust
use anyhow::{anyhow, bail};
use qubox_proto::{ClientMessage, ServerMessage};
use wtransport::{Connection, Request, VarInt};

pub async fn handle(connection: Connection, request: Request) -> anyhow::Result<()> {
    let path = request.path();
    if !path.starts_with("/v1/session/") {
        bail!("unknown path: {}", path);
    }
    let session_id: Uuid = path.trim_start_matches("/v1/session/")
        .parse().map_err(|_| anyhow!("bad session id"))?;

    // 1. Validate the session credential from the request headers
    let cred_header = request.headers().get("x-qubox-session-credential")
        .ok_or_else(|| anyhow!("missing session credential"))?;
    let cred = decode_session_credential(cred_header.to_str()?)?;

    // 2. Accept the WebTransport session
    let session = request.accept().await?;

    // 3. Wait for the first bidirectional stream (control)
    let (mut send, mut recv) = session.accept_bi().await?
        .split();

    // 4. Handshake: client sends Hello; server replies with Welcome.
    //    (Re-uses the existing Hello/Welcome protocol from
    //    crates/qubox-signaling/src/lib.rs:931-942 RelaySignal.)
    //    ... (omitted; see ADR-005 for the full handshake) ...

    // 5. Run the session loop: route control via bidi stream, route
    //    media via datagrams.
    loop {
        tokio::select! {
            stream = session.accept_bi() => {
                let (mut tx, mut rx) = stream?.0.split();
                // control stream: rkyv-encoded ClientMessage frames
                tokio::spawn(async move {
                    while let Some(msg) = rx.next().await {
                        let bytes = msg?;
                        let msg: ClientMessage = rkyv::from_bytes(&bytes)?;
                        // route to existing handlers ...
                    }
                    anyhow::Ok(())
                });
            }
            datagram = session.receive_datagram() => {
                let bytes = datagram?.payload().to_vec();
                // forward to existing media dispatcher in
                // crates/qubox-transport/src/media/mod.rs:862 (magic-prefix
                // match) — wire format unchanged.
            }
            else => break,
        }
    }

    Ok(())
}
```

### 7. Client-side directory layout

New top-level directory `clients/webcodecs/` (sibling to `crates/`):

```
clients/webcodecs/
├── package.json                # see §7.1
├── pnpm-lock.yaml              # generated by pnpm install
├── tsconfig.json
├── vite.config.ts              # see §7.2
├── index.html                  # entry; <canvas> + <script type="module">
├── public/
│   └── favicon.svg
└── src/
    ├── main.ts                 # bootstrap; wires transport -> fec -> codec -> render
    ├── transport.ts            # WebTransport client; see §8
    ├── ticket.ts               # WebTransportTicket decode (base64 + JSON)
    ├── codec.ts                # WebCodecs VideoDecoder + Insertable Streams wrapper; see §9
    ├── fec.ts                  # Reed-Solomon FEC wrapper; see §10
    ├── fecTransform.ts         # TransformStream that buffers k symbols + reconstructs; see §10
    ├── render.ts               # CanvasRenderer: VideoFrame -> WebGL2 texImage2D
    ├── input.ts                # Pointer Events -> rkyv-encoded WirePenEvent; see §11
    └── workers/
        ├── fec.worker.ts       # FEC offloaded to a Web Worker
        └── decoder.worker.ts   # VideoDecoder offloaded to a Worker
└── tests/
    ├── transport.spec.ts       # Playwright E2E
    ├── codec.spec.ts           # Playwright + real encoded media fixture
    └── fec.spec.ts             # Vitest unit
```

#### 7.1 `package.json` dependencies (pinned)

```json
{
  "name": "@qubox/webcodecs-client",
  "version": "0.1.0",
  "private": true,
  "type": "module",
  "scripts": {
    "dev": "vite",
    "build": "tsc --noEmit && vite build",
    "preview": "vite preview",
    "test": "vitest run",
    "test:e2e": "playwright test",
    "typecheck": "tsc --noEmit"
  },
  "dependencies": {
    "@digitaldefiance/reed-solomon-erasure.wasm": "1.0.2",
    "rkyv": "0.8"
  },
  "devDependencies": {
    "@playwright/test": "1.49.0",
    "@types/node": "22.10.0",
    "typescript": "5.7.2",
    "vite": "6.0.5",
    "vite-plugin-checker": "0.8.0",
    "vitest": "2.1.8"
  }
}
```

**Critical note on Reed-Solomon library choice.**
The previous draft cited `@ronomon/reed-solomon`. **That package is a
Node.js native addon** (verified on npm — MIT-licensed but ships a
`.node` binary; cannot load in a browser). The browser-compatible
replacement is **`@digitaldefiance/reed-solomon-erasure.wasm`**,
which is a WebAssembly build of the same `darrenldl/reed-solomon-erasure`
Rust crate we use on the host (ADR-014 §1) and exposes a
`decode(shards: Uint8Array[], missing: number[]): Uint8Array[]` API
that matches our needs exactly.

#### 7.2 `vite.config.ts`

```ts
import { defineConfig } from 'vite';
import checker from 'vite-plugin-checker';

export default defineConfig({
  plugins: [checker({ typescript: true })],
  worker: {
    format: 'es',
  },
  build: {
    target: 'es2022',
    rollupOptions: {
      output: {
        manualChunks: {
          fec: ['@digitaldefiance/reed-solomon-erasure.wasm'],
        },
      },
    },
  },
  server: {
    https: {
      // For local dev only; the production HTTPS cert is served by
      // the signalling server.
      key: './dev-cert/key.pem',
      cert: './dev-cert/cert.pem',
    },
  },
});
```

**Why Vite (not Webpack/esbuild)?** Vite 6 is the modern choice for
TS+WebWorker+WASM apps: dev server cold-starts in ~0.8s (vs Webpack's
~34s), HMR in ~50ms (vs ~2.8s), production bundles ~10–15% smaller
than Webpack, and it has first-class support for `import myWorker
from './worker?worker'` syntax. esbuild is comparable but requires
hand-rolling the dev server story. For Qubox specifically the Vite
+ Rollup pipeline tree-shakes our dependency tree aggressively and
handles our WASM imports via `?init` out of the box.

### 8. Client-side `src/transport.ts` (full file)

```ts
// src/transport.ts
import type { WebTransportTicket } from './ticket';

export interface ConnectedSession {
  transport: WebTransport;
  /** Send pen / keyboard / gamepad events; receive ServerMessage control frames. */
  control: WebTransportBidirectionalStream;
  /** Read incoming datagrams (media, mic, gamepad input from host). */
  incoming: ReadableStreamDefaultReader<Uint8Array>;
}

export async function connect(sessionTicket: string): Promise<ConnectedSession> {
  // 1. Decode the ticket (delivered over the existing WebSocket pairing
  //    channel — see crates/qubox-signaling/src/lib.rs:910-921).
  const ticket: WebTransportTicket = JSON.parse(atob(sessionTicket));

  // 2. Open the WebTransport session. serverCertificateHashes is the
  //    SHA-256 of the **DER-encoded** cert, base64-decoded here.
  //    See crates/qubox-webtransport/src/cert.rs:12-22 for the server
  //    side that produces this hash.
  const transport = new WebTransport(ticket.url, {
    serverCertificateHashes: [{
      algorithm: 'sha-256',
      value: Uint8Array.from(atob(ticket.server_certificate_hash), c => c.charCodeAt(0)),
    }],
  });

  await transport.ready;
  console.log('[transport] session established, datagrams ready');

  // 3. Open the control bidirectional stream (used for
  //    ClientMessage / ServerMessage — same rkyv wire format as the
  //    existing QUIC path; see ADR-015 §6).
  const control = await transport.createBidirectionalStream();

  // 4. Reader for incoming datagrams (media plane).
  const incoming = transport.datagrams.readable.getReader();

  return { transport, control, incoming };
}

export function close(session: ConnectedSession): void {
  session.transport.close({ closeCode: 0, reason: 'client navigated away' });
}
```

### 9. Client-side `src/codec.ts` (full file)

```ts
// src/codec.ts
import type { VideoFrame, VideoDecoderConfig } from './types/webcodecs';

export interface DecoderHandle {
  decoder: VideoDecoder;
  /** Pipe FEC-decoded chunks in here. Frames flow out via `onFrame`. */
  writable: WritableStream<EncodedVideoChunk>;
  onFrame: (frame: VideoFrame) => void;
}

/**
 * Create a WebCodecs VideoDecoder wired to a WritableStream + callback.
 * The Insertable-Streams-style TransformStream wrapper is implemented
 * manually because browser support for `decoder.readable` /
 * `decoder.writable` is not yet universally shipped.
 *
 * Codec strings (all from the W3C WebCodecs codec registry):
 *   AV1  Main profile, level 4.0, 8-bit:  'av01.0.04M.08'
 *   AV1  Main profile, level 5.1, 8-bit:  'av01.0.05M.08'
 *   HEVC Main profile, level 4.1:         'hev1.1.6.L93.B0'
 *   HEVC Main profile, level 5.1:         'hev1.1.6.L123.B0'
 *   H.264 Constrained Baseline, level 3.1:'avc1.42E01F'
 *   H.264 High profile, level 4.0:        'avc1.640028'
 */
export function createDecoder(
  codec: 'av01.0.04M.08' | 'av01.0.05M.08' | 'hev1.1.6.L93.B0' | 'hev1.1.6.L123.B0' | 'avc1.42E01F' | 'avc1.640028',
  codedWidth: number,
  codedHeight: number,
  onFrame: (frame: VideoFrame) => void,
  onError: (e: DOMException) => void,
): DecoderHandle {
  const config: VideoDecoderConfig = {
    codec,
    codedWidth,
    codedHeight,
    bitrateMode: 'variable',
    hardwareAcceleration: 'prefer-hardware', // hint only — see §13
    optimizeForLatency: true,
  };

  const decoder = new VideoDecoder({
    output: (frame) => {
      try {
        onFrame(frame);
      } finally {
        frame.close();
      }
    },
    error: onError,
  });

  decoder.configure(config);

  // TransformStream wrapper around `decoder.decode(chunk)`.
  const writable = new WritableStream<EncodedVideoChunk>({
    write(chunk) {
      decoder.decode(chunk);
    },
    close() {
      decoder.flush();
    },
    abort() {
      decoder.close();
    },
  });

  return { decoder, writable, onFrame };
}

/** Feature detection — call before constructing any WebCodecs object. */
export async function detectCodecSupport(codec: string): Promise<boolean> {
  const result = await VideoDecoder.isConfigSupported({
    codec,
    codedWidth: 1920,
    codedHeight: 1080,
    hardwareAcceleration: 'prefer-hardware',
  });
  return result.supported === true;
}
```

**Render path:** `onFrame` is called with a `VideoFrame`. In
`src/render.ts` we upload the frame as a zero-copy texture:

```ts
// src/render.ts (key snippet)
gl.bindTexture(gl.TEXTURE_2D, tex);
gl.texImage2D(
  gl.TEXTURE_2D, 0, gl.RGBA, gl.RGBA, gl.UNSIGNED_BYTE,
  frame,                  // VideoFrame implements TexImageSource
);
gl.drawArrays(gl.TRIANGLE_STRIP, 0, 4);
```

`VideoFrame` passed directly to `gl.texImage2D` is supported on all
shipping Chrome/Edge/Firefox/Safari (no copy on the GPU path). This is
the zero-copy win referenced in ADR-016.

### 10. Client-side `src/fec.ts` + `src/fecTransform.ts` (full files)

#### 10.1 `src/fec.ts` — Reed-Solomon wrapper around the WASM library

```ts
// src/fec.ts
import init, { ReedSolomonErasure } from '@digitaldefiance/reed-solomon-erasure.wasm';

let initialized = false;
async function ensureInit(): Promise<void> {
  if (!initialized) {
    await init();
    initialized = true;
  }
}

/** k = data symbols, m = parity symbols per ADR-014 §1. */
export const FEC_K = 10;
export const FEC_M = 2;

export async function reconstruct(
  received: Uint8Array[],
  missingIndices: number[],
  shardSize: number,
): Promise<Uint8Array[]> {
  await ensureInit();

  // The WASM API requires fixed-size shards; if a received shard is
  // missing, fill its slot with a zero-filled buffer of the same size.
  const allShards: Uint8Array[] = [];
  for (let i = 0; i < FEC_K + FEC_M; i++) {
    if (missingIndices.includes(i)) {
      allShards.push(new Uint8Array(shardSize));
    } else {
      allShards.push(received[i]);
    }
  }

  const rs = new ReedSolomonErasure(FEC_K, FEC_M);
  rs.reconstruct(allShards, missingIndices);

  // Return only the data shards (the host may also have used the
  // parity for its own diagnostic; the client doesn't need them).
  return allShards.slice(0, FEC_K);
}
```

#### 10.2 `src/fecTransform.ts` — full TransformStream with block window

The host's media wire format (ADR-014 §3) gives every datagram a
`frame_id` (the "block id") and a `chunk_id` (the "symbol index").
The transform groups up to `k+m` datagrams per block and emits
reconstructed `EncodedVideoChunk`s.

```ts
// src/fecTransform.ts
import { FEC_K, FEC_M, reconstruct } from './fec';
import { reconstructParityDatagram, EncodedVideoChunkView } from './wire';

interface PendingBlock {
  frame_id: number;
  shards: (Uint8Array | null)[]; // length k+m
  shardSize: number;
  codec: number;          // 0=H264, 1=H265, 2=AV1
  flags: number;
  stream_id: number;
  firstTimestamp: number | null;
}

const pending = new Map<number, PendingBlock>();

/** Wire format from crates/qubox-transport/src/media/mod.rs:118:
 *  offset 0..2   magic [0xB2, 0x16]
 *  offset 2      flags (bit0=keyframe, bit1=parity, bit2=last)
 *  offset 3      codec (0=H264, 1=H265, 2=AV1)
 *  offset 4..6   stream_id BE
 *  offset 6..10  frame_id BE        <-- block id
 *  offset 10..12 chunk_id BE        <-- symbol index
 *  offset 12..14 chunk_count BE
 *  offset 14..   payload
 */
const HEADER_SIZE = 14;
const MAGIC = [0xb2, 0x16];

export function newFecTransform(): TransformStream<Uint8Array, EncodedVideoChunk> {
  return new TransformStream({
    transform(datagram, controller) {
      if (datagram.byteLength < HEADER_SIZE ||
          datagram[0] !== MAGIC[0] ||
          datagram[1] !== MAGIC[1]) {
        // Not a media datagram (could be gamepad 0x47, pen 0x50,
        // mic 0x4D); pass through unchanged so other handlers can deal.
        // The renderer typically enqueues these to their own queues.
        // For FEC purposes we just ignore.
        return;
      }

      const view = new DataView(datagram.buffer, datagram.byteOffset, datagram.byteLength);
      const flags     = datagram[2];
      const codec     = datagram[3];
      const stream_id = view.getUint16(4, false);
      const frame_id  = view.getUint32(6, false);
      const chunk_id  = view.getUint16(10, false);
      const chunk_count = view.getUint16(12, false);

      const block = pending.get(frame_id) ?? createBlock(frame_id);
      if (block.shardSize === 0) {
        block.shardSize = datagram.byteLength - HEADER_SIZE;
        block.codec = codec;
        block.flags = flags;
        block.stream_id = stream_id;
      }

      // chunk_id 0..k-1 are data; chunk_id k..k+m-1 are parity.
      // (ADR-014 §3 reuses chunk_id for parity index.)
      block.shards[chunk_id] = datagram.subarray(HEADER_SIZE);
      pending.set(frame_id, block);

      // Try to emit.
      tryEmit(frame_id, controller);
    },
    flush(controller) {
      // On stream end, drop all pending blocks (we don't have all k+m).
      pending.clear();
    },
  });
}

function createBlock(frame_id: number): PendingBlock {
  const shards = new Array(FEC_K + FEC_M).fill(null) as (Uint8Array | null)[];
  return {
    frame_id,
    shards,
    shardSize: 0,
    codec: 0,
    flags: 0,
    stream_id: 0,
    firstTimestamp: null,
  };
}

function tryEmit(frame_id: number, controller: TransformStreamDefaultController<EncodedVideoChunk>) {
  const block = pending.get(frame_id);
  if (!block) return;

  const missing: number[] = [];
  for (let i = 0; i < FEC_K + FEC_M; i++) {
    if (!block.shards[i]) missing.push(i);
  }
  if (missing.length > FEC_M) return; // can't recover yet

  // We have all we need.
  const allShards = reconstruct(block.shards as Uint8Array[], missing, block.shardSize);

  // Reconstruct the access unit by concatenating the k data shards.
  // (For AV1/HEVC the entire frame may be one AU; for H.264 it might
  // be fragmented across multiple AUs in the same block — see
  // ADR-014 §3.)
  const totalLen = block.shardSize * FEC_K;
  const au = new Uint8Array(totalLen);
  for (let i = 0; i < FEC_K; i++) {
    au.set(allShards[i], i * block.shardSize);
  }

  const isKey = (block.flags & 0x01) !== 0;
  // The first 8 bytes of the first data shard hold the timestamp
  // (matches the host's EncodedVideoChunk serialization; see
  // crates/qubox-media/src/encoder_hw.rs:142-148).
  const timestamp = Number(new DataView(allShards[0].buffer).getBigUint64(0, false));

  controller.enqueue(new EncodedVideoChunk({
    type: isKey ? 'key' : 'delta',
    timestamp,
    duration: 0,
    data: au,
  }));

  pending.delete(frame_id);
}
```

### 11. Client-side `src/input.ts` — Pointer Events → rkyv → WebTransport bidi

```ts
// src/input.ts
import { rkyv } from 'rkyv';
import { WirePenEvent } from './wire';

export function attachInput(canvas: HTMLCanvasElement,
                            control: WebTransportBidirectionalStream): void {
  const writer = control.writable.getWriter();

  function send(event: PointerEvent | WheelEvent | KeyboardEvent) {
    const wire = encodeWirePenEvent(event);  // mirrors ADR-010 §1.2
    const bytes = rkyv.to_bytes(wire);
    writer.write(bytes);
  }

  canvas.addEventListener('pointerdown', send);
  canvas.addEventListener('pointermove', (e) => {
    if (e.pressure > 0 || e.buttons !== 0) send(e);
  });
  canvas.addEventListener('pointerup',   send);
  canvas.addEventListener('wheel',       send, { passive: false });
  window.addEventListener('keydown',     send);
  window.addEventListener('keyup',       send);
}
```

The wire format is identical to the existing native path (ADR-010 §1.2
`WirePenEvent`, ADR-015 §6 rkyv migration). The same `rkyv-js` 0.8.x
crate handles the byte-for-byte serialization. **No host-side changes
required.**

### 12. `main.ts` — pipeline assembly

```ts
// src/main.ts
import { connect } from './transport';
import { createDecoder } from './codec';
import { newFecTransform } from './fecTransform';
import { attachRenderer } from './render';
import { attachInput } from './input';

async function main(sessionTicket: string, canvas: HTMLCanvasElement) {
  // 1. Open the WebTransport session.
  const session = await connect(sessionTicket);

  // 2. Set up decoder + renderer.
  const decoder = createDecoder(
    'av01.0.04M.08', // AV1 Main, level 4.0, 8-bit (preferred)
    3840, 2160,
    (frame) => attachRenderer(canvas).draw(frame),
    (err) => console.error('[decoder] error:', err),
  );

  // 3. Wire: datagrams -> FEC transform -> decoder writable.
  session.incoming
    .pipeThrough(newFecTransform())
    .pipeTo(decoder.writable)
    .catch((e) => console.error('[pipeline]', e));

  // 4. Hook up pointer/keyboard input.
  attachInput(canvas, session.control);

  return session;
}
```

### 13. Pitfalls (specific gotchas a junior dev must know)

1. **Safari version-numbering caveat.** Apple shipped WebTransport in
   Safari **17.4** (March 2024) and datagrams in **17.5** (May 2024).
   In 2025 Apple moved to a year-based scheme, so the same browser
   is also labelled **Safari 26.4**. Both numbers refer to the same
   engine. The feature-detect on
   `'WebTransport' in self && 'datagrams' in WebTransport.prototype`
   is the only reliable check; do not parse version strings.

2. **`@ronomon/reed-solomon` does NOT work in the browser.** It is a
   Node.js native addon (`.node` binary). The first draft of this
   ADR cited it; that was wrong. Use
   `@digitaldefiance/reed-solomon-erasure.wasm` instead (verified on
   npm; current version 1.0.2; ~30 KB gz).

3. **`serverCertificateHashes` is the SHA-256 of the DER cert, NOT
   the SPKI.** Some blog posts say "SHA-256 of the public key" —
   that is wrong. Pass the SHA-256 of the DER-encoded
   `Certificate` message as produced by `rcgen::cert.serialize_der()`.

4. **Firefox `serverCertificateHashes` was buggy until v136.** A
   regression in Firefox 114–135 silently rejected correct hashes.
   If you see WebTransport fail on Firefox < 136, that's why.

5. **`hardwareAcceleration: 'prefer-hardware'` is a HINT, not a
   directive.** All browsers treat it as advisory; the platform
   codec availability wins. If you need to know whether HW decode is
   actually in use, query
   `decoder.getStats()` (where supported) or just measure CPU.

6. **Safari AV1 is hardware-gated with NO software fallback.** iPhone
   14 / A16 and earlier, and Intel Macs, cannot decode AV1 at all.
   Always feature-detect with
   `VideoDecoder.isConfigSupported({ codec: 'av01.0.04M.08', ... })`
   and fall back to HEVC or H.264 on those devices.

7. **Native `decoder.readable` / `decoder.writable` are NOT YET
   shipped on all browsers as of 2026.** The W3C WebCodecs spec still
   defines the callback API (`output`/`error` + `decode()`); native
   stream surfaces are in active discussion. The pattern in §9 (manual
   `TransformStream` wrapper) works on every shipping browser.

8. **H.264 codec strings must distinguish `avc1` vs `avc3`.** Use
   `avc1.42E01F` for Constrained Baseline (most remoting traffic)
   and `avc3.640028` for High profile in MP4 containers. The
   trailing 6 hex digits are profile_idc (42/64), constraint_set
   flags (E0), and level_idc (1F=3.1, 28=4.0).

9. **Edge AV1 on Windows requires the "AV1 Video Extension" from the
   Microsoft Store.** Without it, AV1 WebCodecs fails. Chrome bundles
   its own AV1 software decoder; Firefox uses system codecs.

10. **`gl.texImage2D(..., frame)` zero-copy is supported in Chrome
    and Safari today.** Firefox added support in 2024 and is solid
    on 130+. If you see "frame is not a valid TexImageSource",
    either the browser is too old OR you forgot `frame.close()` after
    the draw, double-freeing the GPU resource.

11. **Backpressure in the FEC transform.** If `tryEmit` runs while
    the renderer is busy, the `WritableStream` of `decoder.writable`
    will signal backpressure via `desiredSize`; the FEC transform
    must respect this or you'll OOM. The Streams API does this
    automatically as long as you don't hold a reference to old blocks
    in `pending` forever. The `pending.clear()` in `flush()` handles
    teardown; you should also call it on a periodic GC sweep
    (`pending.forEach((b, id) => { if (Date.now() - b.createdAt > 500) pending.delete(id); })`)
    to drop blocks that never complete (k+m symbols never arrived).

12. **Cert pinning vs HSTS interaction.** The first time a user
    connects, the browser may cache the SHA-256 in HSTS-like state.
    Rotating the cert (which we do on every server restart by
    default — see `§3.1`) means users must hard-refresh OR you must
    persist the cert across restarts. **Decision: persist the cert**
    in `~/.qubox/webtransport-cert.pem` and re-load on startup.

13. **`@ronomon/reed-solomon` again, just to hammer it home: this is
    Node-only.** If you copy/paste a Stack Overflow snippet that
    imports `@ronomon/reed-solomon`, the build will succeed but
    `new ReedSolomon(...)` will throw at runtime with "Buffer is
    not defined" or similar. Use the WASM package.

### 14. Permissions / cert provisioning

#### 14.1 `Permissions-Policy: webtransport`

Chrome enforces `Permissions-Policy: webtransport` for embedded
(iframed) contexts only; for top-level pages it has no restrictive
effect. We add the header to all responses from
`crates/qubox-signaling/src/webtransport.rs` defensively, as a
hygiene measure:

```rust
async fn add_permissions_headers(mut resp: Response) -> Response {
    resp.headers_mut().insert(
        "Permissions-Policy",
        HeaderValue::from_static("webtransport=(self)"),
    );
    resp
}
```

#### 14.2 Cert hash delivery

The existing `crates/qubox-identity` Ed25519 mTLS-style pairing is
**independent of TLS server identity**. The WebTransport
`serverCertificateHashes` is a separate concern: we generate a
self-signed TLS cert at startup, compute its SHA-256(DER), and ship
that hash alongside the existing `NativeQuicTicket` over the
authenticated WebSocket pairing channel.

The flow:

1. User opens the pairing page on the host (existing flow, ADR-005).
2. Host generates `NativeQuicTicket` (existing
   `crates/qubox-transport/src/lib.rs:1480-1488`).
3. **New:** Host also generates a WebTransport self-signed cert
   (persisted at `~/.qubox/webtransport-cert.pem`), SHA-256(DER) is
   computed, and a `WebTransportTicket { url, server_certificate_hash }`
   is added to the `Welcome` message in
   `crates/qubox-proto/src/lib.rs:937-940`.
4. Client receives the `Welcome` over the existing authenticated
   WebSocket and persists the `WebTransportTicket` in localStorage
   (keyed by host peer id).
5. Subsequent connects reuse the cached ticket (cert persists across
   server restarts thanks to step 3).

### 15. Step-by-step implementation order (numbered PRs)

1. **PR #1 — proto additions.** Add `TransportKind::WebTransport`
   variant to `crates/qubox-proto/src/lib.rs:97-101`. Add
   `WebTransportTicket` struct + `RelaySignal::WebTransportTicket`
   variant. Land as a no-op PR; just types. ~50 LOC + serde derives.
2. **PR #2 — new `qubox-webtransport` crate skeleton.** Empty
   `lib.rs`, `Cargo.toml`, workspace member registration. Verify
   `cargo build -p qubox-webtransport` succeeds.
3. **PR #3 — `cert.rs` + unit tests.** Implement
   `generate_self_signed()` and the SHA-256(DER) helper. Add 3 unit
   tests: hash is 32 bytes; hash matches a known vector; cert is
   valid for 30 days.
4. **PR #4 — `server.rs` + wtransport wiring.** Bring up an HTTP/3
   listener on `:4443` that prints "ready" and accepts a session.
   Manual `curl --http3 https://localhost:4443/healthz` smoke test.
5. **PR #5 — `session.rs` handshake.** Implement the Hello/Welcome
   handshake over the first bidi stream (re-using the existing
   rkyv-encoded `ClientMessage`/`ServerMessage`). Land with 2 unit
   tests (reject missing cred; accept valid cred).
6. **PR #6 — `crates/qubox-signaling/src/webtransport.rs` glue.**
   Mount the new endpoints under the existing `SignalingState::router()`.
   `cargo test -p qubox-signaling` must stay green.
7. **PR #7 — Vite + TS scaffolding.** `pnpm create vite`, add
   `clients/webcodecs/` with empty `transport.ts`, `codec.ts`,
   `fec.ts`. `pnpm dev` opens a blank page; `pnpm test` passes.
8. **PR #8 — `transport.ts` + ticket decoding.** Implement
   `connect(sessionTicket)`. Verified with Playwright against
   `crates/qubox-signaling`'s built-in test server
   (`cargo test -p qubox-signaling webtransport_smoke`).
9. **PR #9 — `codec.ts` + Playwright spec.** Implement
   `createDecoder()`. Add `clients/webcodecs/tests/codec.spec.ts`
   that feeds a known AV1 IVF fixture through the decoder and
   asserts ≥1 `VideoFrame` is emitted.
10. **PR #10 — `fec.ts` + `fecTransform.ts`.** Implement RS
    reconstruct + block window. Vitest unit tests for the
    transform: `fec_transform_reconstructs_from_k_symbols`,
    `fec_transform_drops_blocks_with_more_than_m_missing`.
11. **PR #11 — `render.ts` (WebGL2).** Implement
    `gl.texImage2D(..., frame)`. Playwright visual regression test
    using a known image fixture.
12. **PR #12 — `input.ts` (Pointer Events).** Implement pointer +
    keyboard capture + rkyv wire format. Manual smoke test on a
    touchscreen laptop.
13. **PR #13 — E2E pipeline test.** `main.ts` wired up. Playwright
    launches Chromium, navigates to the dev page, completes pairing
    against the test server, captures the canvas screenshot, and
    asserts it's not all-black.
14. **PR #14 — Firefox + Safari parity.** Add `--project=firefox`
    and `--project=webkit` to `playwright.config.ts`. Known-failing
    Firefox test for `serverCertificateHashes` is marked
    `test.fixme()` until we can require Firefox ≥ 136.
15. **PR #15 — iOS Safari real-device validation.** Build a static
    bundle, deploy to a TestFlight-equivalent (or just S3+CloudFront
    with HTTPS), install on an iPhone 14 (A15) + iPhone 15 Pro (A17)
    and manually verify AV1 fallback to HEVC on the A15.

### 16. Test specifications

#### 16.1 Vitest unit tests (run via `pnpm test`)

| Test name                                | File                                       | What it asserts                                                          |
|------------------------------------------|--------------------------------------------|--------------------------------------------------------------------------|
| `fec_transform_reconstructs_from_k_symbols` | `tests/fec.spec.ts`                       | Reconstruct when only k of k+m shards present.                           |
| `fec_transform_drops_blocks_with_more_than_m_missing` | `tests/fec.spec.ts`            | No emit; `pending.delete` after window expiry.                           |
| `fec_transform_passes_non_media_through` | `tests/fec.spec.ts`                        | Gamepad / pen / mic datagrams (magic != `[0xB2,0x16]`) are untouched.   |
| `codec_detects_av1_support`             | `tests/codec.spec.ts`                      | `detectCodecSupport('av01.0.04M.08')` returns boolean on all browsers.  |
| `transport_decode_ticket_base64`        | `tests/transport.spec.ts`                  | `JSON.parse(atob(...))` round-trips a known ticket blob.                |

#### 16.2 Playwright E2E tests (run via `pnpm test:e2e`)

| Test name                                | Project       | What it asserts                                                          |
|------------------------------------------|---------------|--------------------------------------------------------------------------|
| `webtransport_connects_to_signaling`     | chromium      | `await transport.ready` resolves; `transport.closed` not closed.        |
| `webtransport_datagrams_round_trip`      | chromium      | Send a 100-byte datagram; receive it back unchanged.                     |
| `videodecoder_decodes_av1_at_1080p60`    | chromium      | Feed a known 1080p60 AV1 IVF clip; ≥300 frames emitted in ≤5s.           |
| `videodecoder_decodes_av1_at_4k60`       | chromium      | Feed a known 4K60 AV1 clip; ≥240 frames emitted in ≤5s on capable HW.   |
| `videodecoder_decodes_hevc_on_safari`    | webkit        | Feed HEVC clip; ≥60 frames emitted. Marks `fixme()` on iOS <17.5.       |
| `fec_endto_end_reconstructs_lost_packets`| chromium      | Send FEC block with 1 shard missing; client emits reconstructed chunk.  |
| `render_draws_videoframe_to_canvas`      | chromium      | After decoding 1 frame, `gl.readPixels` returns non-zero RGB sum.       |
| `input_pointer_event_serializes_to_rkyv` | chromium      | Dispatch a `pointerdown`; bytes on the control stream match rkyv vec.    |

#### 16.3 Cargo tests (existing + new)

| Test name                                | Crate                  | What it asserts                                                          |
|------------------------------------------|------------------------|--------------------------------------------------------------------------|
| `webtransport_smoke`                     | `qubox-webtransport`   | Listener binds + accepts + drops session cleanly.                        |
| `webtransport_cert_hash_is_deterministic` | `qubox-webtransport` | Same seed → same DER hash (deterministic for testing).                   |
| `signaling_mounts_webtransport_router`   | `qubox-signaling`      | `/v1/webtransport/cert` returns 200 + valid `WebTransportTicket` JSON.   |
| `proto_serializes_web_transport_ticket`  | `qubox-proto`          | Round-trip JSON of `WebTransportTicket`.                                 |

### 17. Verification commands (per milestone)

```bash
# PR #2 — crate skeleton
cargo build -p qubox-webtransport

# PR #3 — cert.rs
cargo test -p qubox-webtransport cert

# PR #4 — listener up
cargo run -p qubox-webtransport -- --listen 127.0.0.1:4443 &
curl --http3 -k https://127.0.0.1:4443/healthz

# PR #6 — integration
cargo test -p qubox-signaling webtransport
cargo test -p qubox-signaling                 # existing tests stay green

# PR #7 — TS scaffolding
cd clients/webcodecs
pnpm install
pnpm dev                                      # http://localhost:5173

# PR #9 — codec
pnpm vitest run codec
pnpm playwright test codec.spec.ts --project=chromium

# PR #10 — FEC
pnpm vitest run fec

# PR #13 — E2E
pnpm playwright test --project=chromium       # full pipeline

# PR #14 — multi-browser
pnpm playwright test --project=firefox
pnpm playwright test --project=webkit

# All tests, all crates
cargo test --workspace
cd clients/webcodecs && pnpm test && pnpm test:e2e
```

### 18. File paths and insertion points (index)

| Change                                            | File                                                  | Line range        |
|---------------------------------------------------|-------------------------------------------------------|-------------------|
| New enum variant `WebTransport`                   | `crates/qubox-proto/src/lib.rs`                       | `97-101`          |
| New `WebTransportTicket` struct                   | `crates/qubox-proto/src/lib.rs`                       | new (after `:940`) |
| New `RelaySignal::WebTransportTicket` variant     | `crates/qubox-proto/src/lib.rs`                       | new (after `:942`) |
| Add `qubox-webtransport` dep                      | `crates/qubox-signaling/Cargo.toml`                   | new line at `:22` |
| New `webtransport.rs` module                      | `crates/qubox-signaling/src/webtransport.rs`          | new file          |
| Declare module in lib.rs                           | `crates/qubox-signaling/src/lib.rs`                   | new line at `:33` |
| Mount router in `router()`                        | `crates/qubox-signaling/src/lib.rs`                   | `291-304`         |
| Add `webtransport_cert_hash` field                | `crates/qubox-signaling/src/lib.rs`                   | `49-69`           |
| Add helper method on `SignalingState`             | `crates/qubox-signaling/src/lib.rs`                   | new (after `:706`)|
| New crate scaffold                                | `crates/qubox-webtransport/Cargo.toml`                | new file          |
| New crate public API                              | `crates/qubox-webtransport/src/lib.rs`                | new file          |
| Cert generation                                   | `crates/qubox-webtransport/src/cert.rs`               | new file          |
| Server (wtransport wrapper)                       | `crates/qubox-webtransport/src/server.rs`             | new file          |
| Session handler                                   | `crates/qubox-webtransport/src/session.rs`            | new file          |
| Register crate as workspace member                | `/Cargo.toml`                                         | `12-30`           |
| New TS client                                     | `clients/webcodecs/`                                  | new directory     |
| `package.json`                                    | `clients/webcodecs/package.json`                      | new file          |
| Vite config                                       | `clients/webcodecs/vite.config.ts`                   | new file          |
| Transport client                                  | `clients/webcodecs/src/transport.ts`                  | new file          |
| Codec wrapper                                     | `clients/webcodecs/src/codec.ts`                      | new file          |
| FEC wrapper                                       | `clients/webcodecs/src/fec.ts`                        | new file          |
| FEC TransformStream                               | `clients/webcodecs/src/fecTransform.ts`               | new file          |
| Render (WebGL2)                                   | `clients/webcodecs/src/render.ts`                     | new file          |
| Pointer input                                     | `clients/webcodecs/src/input.ts`                      | new file          |
| Main pipeline                                     | `clients/webcodecs/src/main.ts`                       | new file          |
| FEC Web Worker                                    | `clients/webcodecs/src/workers/fec.worker.ts`         | new file          |
| Decoder Web Worker                                | `clients/webcodecs/src/workers/decoder.worker.ts`     | new file          |
| Vitest config                                     | `clients/webcodecs/vitest.config.ts`                  | new file          |
| Playwright config                                 | `clients/webcodecs/playwright.config.ts`              | new file          |
| Vitest unit specs                                 | `clients/webcodecs/tests/{fec,codec,transport}.spec.ts` | new files      |
| Playwright E2E specs                              | `clients/webcodecs/tests/e2e/*.spec.ts`               | new files         |

### 19. Consequences

#### Positive

- Zero native install for browser-side users. Mobile-web parity is
  the explicit P2-18 ask.
- WebCodecs HW decode is **strictly better** than WASM-compiled
  decoder: native AV1 decode on Apple A17/M3+, ARM Mali 6th-gen+,
  Adreno 7th-gen+, Intel Xe 11th-gen+, RTX 30+. Battery savings
  ~3-5× vs WASM.
- FEC decode is co-located with WebCodecs via a TransformStream,
  keeping the JS bundle small (~30 KB RS WASM decoder).
- The native QUIC path (host side) is **unchanged** for the media
  wire format. We add zero host code for the media plane; the
  WebTransport server is just routing.

#### Negative / Risk

- iOS Safari 17.4 introduced WebTransport but datagrams only landed
  in 17.5. Users on older iOS get a fallback to no-datagram mode
  (reliable streams only — pen events degrade to JSON over
  WebSockets). The fallback is documented in
  `crates/qubox-signaling/README.md`.
- WebCodecs codec matrix is browser-controlled. We declare a
  preference list (AV1 → HEVC → H.264) and let `isConfigSupported`
  pick. ADR-018 covers the codec selection on the host side.
- No 4K144 in browser: WebCodecs supports 4K60 reliably; 4K144 is
  only possible on platforms with the right GPU/driver combination.
  We cap the browser client at 4K60 / 1440p144.
- Cert provisioning: WebTransport requires `serverCertificateHashes`.
  We piggy-back on the existing `crates/qubox-identity` pairing flow
  by adding a `WebTransportTicket` field to the `Welcome` message.

### 20. Roadmap mapping

- Closes P2-18 (mobile-web).
- Reuses ADR-011 (MASQUE proxy transport), ADR-014 (RS FEC
  k=10/m=2 — same parameters), ADR-015 (rkyv for JS via `rkyv-js`).
- The browser client **cannot** host a session — only join one.
  (Mobile-web is client-only by design.)

### 21. References

- `crates/qubox-signaling/` existing signalling crate (location)
- `crates/qubox-identity/` device pairing + Ed25519 mTLS-style auth
- `crates/qubox-transport/src/lib.rs:1480-1488` ticket encode/decode
- `crates/qubox-proto/src/lib.rs:97-101` `TransportKind` enum
- `crates/qubox-proto/src/lib.rs:937-940` `RelaySignal::NativeQuicTicket`
- `crates/qubox-transport/src/media/mod.rs:118` media datagram
  header wire format
- ADR-005 (signalling + TURN architecture)
- ADR-010 §1.2 `WirePenEvent` (pointer wire format)
- ADR-011 §4 (MASQUE CONNECT-UDP)
- ADR-014 §1 (FEC parameters k=10, m=2)
- ADR-014 §3 (media datagram wire format with `FLAG_PARITY`)
- ADR-015 §6 (rkyv for JS via `rkyv-js`)
- ADR-018 (codec selection matrix)
- W3C WebTransport: https://www.w3.org/TR/webtransport/
- W3C WebCodecs: https://www.w3.org/TR/webcodecs/
- W3C WebCodecs AV1 codec registration: https://www.w3.org/TR/webcodecs-av1-codec-registration/
- W3C WebCodecs HEVC codec registration: https://www.w3.org/TR/webcodecs-hevc-codec-registration/
- W3C WebCodecs AVC/H.264 codec registration: https://www.w3.org/TR/webcodecs-avc-codec-registration/
- MDN WebTransport: https://developer.mozilla.org/en-US/docs/Web/API/WebTransport
- MDN VideoDecoder: https://developer.mozilla.org/en-US/docs/Web/API/VideoDecoder
- `wtransport` Rust crate: https://docs.rs/wtransport
- `@digitaldefiance/reed-solomon-erasure.wasm`: https://www.npmjs.com/package/@digitaldefiance/reed-solomon-erasure.wasm
- `rcgen` Rust crate (cert generation): https://docs.rs/rcgen