# Architecture

## Overview

Qubox separates session negotiation (**Control Plane**) from real-time streaming (**Media & Input Planes**) and supports two media profiles:

1. **Native Profile (Primary — Desktop & Mobile)**
   Host and client establish a direct **QUIC v2** connection powered by **Iroh P2P** (`qubox-transport`) for automatic NAT hole-punching by Ed25519 `NodeId`, with automatic zero-config fallback to an embedded `iroh-relay` server (`:3340`) or TURN (`:3478`). Video packets use framed QUIC streams/datagrams with Reed-Solomon FEC and frame-aware pacing; audio, input, and File Sync ride dedicated multiplexed QUIC streams.
2. **Web Profile (`web/` + `qubox-webtransport`)**
   Browser viewers connect over **WebTransport** (HTTP/3 over QUIC) and decode video frames via the browser's hardware-accelerated **WebCodecs** (`VideoDecoder`) API.

---

## Plane Decomposition

### 1. Control Plane (`qubox-signaling`, `qubox-signaling-server`, `qubox-identity`)

Responsibilities:
- Persistent Ed25519 device identity and signed signaling envelopes (`SignedHello`).
- Peer presence and human-readable device names.
- Pairing requests, host approval, persistent pair grants, and **auto-approval** (`auto_approve`) for trusted friends.
- **Party Share Codes**: 15-minute multi-peer share codes (`ShareCreate` / `ShareRedeem`) so up to 4 friends can join a host without manual pairing prompts.
- Short-lived HMAC-signed `SessionCredential` tokens enforced during QUIC/Iroh transport bootstrap.
- Exchange of direct UDP socket candidates and Iroh `EndpointAddr` (`NodeId` + relay URLs + direct addresses).

### 2. Media Plane (`qubox-transport`, `qubox-media`, `qubox-rl-policy`)

Responsibilities:
- Low-latency H.264 / HEVC / AV1 video transport and Opus stereo audio transport over Iroh P2P / QUIC v2.
- BBRv2 congestion control, QUIC ACK-frequency tuning, and Pensieve-style ABR bitrate adaptation (`qubox-rl-policy`).
- Reed-Solomon forward error correction (FEC) on keyframes and frame-aware packet pacing.
- Automatic relay fallback via `iroh-relay` (`:3340`) when symmetric NATs block direct UDP hole-punching.

### 3. Input & Access Control Plane (`qubox-platform`, `qubox-host-agent`, `qubox-client-gui`)

Responsibilities:
- Low-latency keyboard and mouse injection (`enigo`, XTest, `SendInput`, `CGEvent`) with immediate input ACKs.
- **4-Player Co-Op Controller Multiplexing**: Up to 4 concurrent remote guests per host, each mapped to a dedicated virtual Xbox 360 gamepad slot (`Player 1`–`Player 4`) via `/dev/uinput` on Linux and `ViGEmBus` on Windows.
- **Parsec-Style Per-Peer Access Control**:
  - Host enforces per-peer input permissions in real time (`allow_gamepad`, `allow_keyboard`, `allow_mouse`) via `UpdatePeerPermissions` (`qubox-host-agent` drops unauthorized input events immediately at the host boundary).
  - Persistent per-peer preferences (`gamepad`, `keyboard`, `mouse`, `auto_approve`) are saved locally on the host (`~/.qubox/gui-peer-permissions.json`) and surfaced as instant icon toggles on the active host banner.
- Bidirectional clipboard sync (`qubox-clipboard`), client-to-host microphone passthrough (`qubox-mic`), stylus pressure/tilt (`qubox-pen`), and `blake3` content-addressed File Sync (`qubox-sync`).

### 4. Platform Plane (`qubox-media`, `qubox-display`, `qubox-platform`)

Responsibilities:
- **Linux:** PipeWire & X11 capture, VAAPI/NVENC/AMF/x264 encode, VAAPI/NVDEC/FFmpeg decode, `wgpu` viewer, `/dev/uinput` virtual gamepads, privacy blanking overlay / `vkms`.
- **Windows:** DXGI Desktop Duplication & GDI fallback, NVENC/AMF/QSV/MediaFoundation encode, D3D11VA/FFmpeg decode, `wgpu` viewer, `ViGEmBus` virtual gamepads.
- **macOS:** ScreenCaptureKit / CoreGraphics capture, `VideoToolbox` hardware encode/decode, `wgpu` Metal viewer, `CGEvent` input injection.

---

## Security Summary

- **Cryptographic Identity:** Every host and client authenticates to signaling using an Ed25519 keypair (`SignedHello` required by default).
- **End-to-End Encrypted Transport:** Both direct QUIC v2 and Iroh P2P (`tls-ring`) authenticate peer keys and encrypt all video, audio, input, and File Sync streams end-to-end; relays (`iroh-relay` / `coturn`) only forward encrypted ciphertext.
- **Least-Privilege Guest Input:** Guests connected via Party Share Codes or pairing are subject to per-peer host toggles for **Controller**, **Keyboard**, and **Mouse**, and can be kicked individually or all at once.
