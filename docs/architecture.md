# Architecture

## Core Correction

The original plan mixed together two incompatible goals:

- A native QUIC-first stack.
- A browser client using WebCodecs without a special native app.

Those are both valid goals, but they do not ride the exact same network path. Browsers do not expose raw application-defined QUIC sockets. That means the architecture has to support two media-session profiles:

1. Native profile
   Host and client use direct QUIC with datagrams for video packets and reliable streams for control and recovery.
2. Web profile
   Host and browser use WebRTC. WebCodecs remains useful on the receive side, but the transport and session setup must still be browser-compatible.

That is not a compromise. It is the cleanest way to keep the native path fast while keeping the browser path deployable.

## Plane Decomposition

### Control Plane

Responsibilities:

- Identity and device registration.
- Peer presence.
- Session authorization.
- Transport selection.
- SDP and ICE exchange.
- Relay fallback policy.

Current implementation:

- WebSocket signaling server in Rust.
- Persistent local device identities for host and client roles.
- Pairing requests, host approval, and server-side pair grants.
- Optional JSON pair-store persistence on the signaling server.
- Short-lived host/client session credentials returned with planned sessions, with signaling-side expiry enforcement on session relay and native QUIC transport authentication.
- Configurable ICE server URLs that are propagated to hosts and clients during session setup.
- Shared protocol types for peers, session requests, WebRTC/native relayed signaling, codec preferences, and transport selection.
- A native QUIC transport crate that bootstraps an authenticated connection and carries framed H.264 access units.
- A first host/client integration where the Linux PipeWire/FFmpeg H.264 path can feed native QUIC and the client CLI can save the resulting stream.

### Media Plane

Responsibilities:

- Low-latency video and audio transport.
- Congestion control.
- Frame dropping policy.
- Relay fallback.
- Slice or tile-aware delivery.

Recommended shape:

- Native: QUIC datagrams and streams.
- Browser: WebRTC media path.
- Relayed fallback: TURN for WebRTC, QUIC relay for native.

### Input Plane

Responsibilities:

- Keyboard and mouse injection.
- Relative versus absolute pointer modes.
- Controller passthrough.
- Clipboard and optional file handoff.

Recommended shape:

- Small reliable control stream, separate from video delivery.
- Platform-specific injection adapters behind a shared trait.

### Platform Plane

Responsibilities:

- Capture.
- Encode and decode.
- Audio capture and playback.
- Display timing.
- Permissions and packaging.

This plane is where most remote desktop projects become Windows-only. The project should treat Linux, Windows, macOS, and Android as first-class targets in the API design from day one.

## Negotiation Model

Every peer should advertise:

- OS family.
- Role.
- Supported transports.
- Supported capture backends.
- Supported encoders.
- Supported decoders.

The signaling server should not just forward blobs blindly. It should compute a valid session plan by intersecting transport and codec capabilities and then forward the correct negotiation messages for that session type.

The current code already does that for transport and codec selection.

Session startup now requires a pair grant between the client peer ID and host peer ID. SDP offers, SDP answers, ICE candidates, and native QUIC tickets are relayed only between paired peers that belong to the active planned session ID.

## Security Model

Minimum bar:

- Account auth for the signaling plane.
- Device registration and approval.
- End-to-end authenticated session setup.
- Short-lived session tokens.
- Relay that cannot decrypt native media traffic.

Not yet implemented:

- Account authentication.
- Signed device certificates.
- Relay authentication.
- End-to-end media encryption beyond what the future WebRTC/QUIC transport will provide.
- Revocation UI and audit logs.

Partially implemented:

- Short-lived per-session bearer credentials exist and are enforced by the native QUIC bootstrap, but they are not yet bound to signed device certificates or a broader device-trust model.

## Roadmap

1. Add account authentication, signed device certificates, and pairing revocation.
2. Add browser-compatible WebRTC peer connection code and TURN support.
3. Expand native QUIC beyond the first authenticated H.264 stream into datagrams, pacing, and recovery behavior.
4. Validate and harden the Linux PipeWire plus H.264 runtime on a real Linux desktop.
5. Add Windows Desktop Duplication capture plus hardware encode.
6. Add input injection and audio streaming.
7. Add macOS ScreenCaptureKit plus VideoToolbox.
8. Add Android native client.
9. Add a desktop UI shell after the media path is proven.
