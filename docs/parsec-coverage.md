# Parsec Feature Coverage

This is the tracking matrix for reaching feature parity. The current project is a young open-source control-plane and media-backend scaffold, not a production Parsec clone.

Legend:

- Done: implemented and tested in this repository.
- Partial: meaningful code exists, but the feature is incomplete.
- Planned: architecture or probe exists, but the user-facing feature is not implemented.
- Missing: no implementation yet.

| Category | Feature | Status | Linux | Windows | Notes |
| :--- | :--- | :--- | :--- | :--- | :--- |
| Core networking | Low-latency custom datagram protocol | Partial | Partial | Partial | Native QUIC session bootstrap now exists and carries H.264 access units over a live encrypted QUIC stream, but the current slice uses a reliable stream rather than the full datagram-oriented runtime Parsec would need. |
| Core networking | NAT traversal with STUN/TURN | Planned | Partial | Partial | WebRTC-compatible signaling messages and ICE server URL propagation exist; peer connection code and TURN credential service are not implemented. |
| Core networking | P2P handoff after signaling | Partial | Partial | Partial | Signaling server relays session setup messages between paired peers in an active planned session. |
| Core networking | Relay fallback | Planned | Planned | Planned | TURN/QUIC relay service is not implemented. |
| Identity | Persistent device identity | Done | Expected | Done | Local JSON identity store creates stable device, host peer, and client peer IDs. |
| Pairing | Host-approved pairing | Partial | Expected | Done | Pairing can be requested and approved; no UI, revocation, or signed trust model yet. |
| Security | End-to-end encryption | Partial | Partial | Partial | Native QUIC media is encrypted in transit and authenticated with short-lived session credentials; WebRTC, signed device trust, and broader session-security hardening are still missing. |
| Security | Account authentication | Missing | Missing | Missing | No user accounts yet. |
| Security | 2FA | Missing | Missing | Missing | Requires auth service. |
| Security | No background access unless app is running | Partial | Partial | Partial | Host must be connected to signaling server; no tray permission prompt yet. |
| Video | Hardware encode | Partial | Partial | Planned | FFmpeg H.264 probe, command plan, spawn/read wrapper, and native QUIC-connected Linux host path exist; real Linux desktop runtime validation is still needed. |
| Video | Hardware decode | Missing | Missing | Missing | Decoder abstraction is not implemented. |
| Video | H.264 support | Partial | Partial | Partial | H.264 is negotiated, probed, command-planned, framed into access units, streamed over native QUIC, and written by the client CLI to a `.h264` file; decoded presentation is not implemented yet. |
| Video | H.265 support | Planned | Planned | Planned | Negotiated in capabilities; not encoded yet. |
| Video | AV1 support | Planned | Planned | Planned | Negotiated in capabilities; not encoded yet. |
| Video | PipeWire capture | Partial | Partial | Not applicable | Linux probe, FFmpeg command plan, bounded spawned smoke command, and host-agent-to-QUIC wiring exist; Linux desktop runtime validation is still needed. |
| Video | Desktop Duplication capture | Planned | Not applicable | Planned | Windows capture adapter is documented but not implemented. |
| Video | ScreenCaptureKit capture | Planned | Not applicable | Not applicable | macOS is future work. |
| Video | High refresh rates | Missing | Missing | Missing | Needs capture, encode, transport, decoder, and renderer timing. |
| Video | Resolution scaling | Missing | Missing | Missing | Needs capture/output negotiation and virtual display support. |
| Video | V-Sync toggle | Missing | Missing | Missing | Requires renderer implementation. |
| Video | 4:4:4 color | Missing | Missing | Missing | Requires codec profile negotiation and encode support. |
| Video | Multi-monitor streaming | Missing | Missing | Missing | Requires capture topology and client renderer work. |
| Video | Virtual display/headless host | Missing | Missing | Missing | Linux and Windows require very different driver/compositor strategies. |
| Audio | System audio passthrough | Missing | Missing | Missing | PipeWire/WASAPI capture not implemented. |
| Audio | Echo cancellation | Missing | Missing | Missing | Requires audio graph plus voice-chat routing model. |
| Audio | App-specific audio routing | Missing | Missing | Missing | Needs per-platform audio session routing. |
| Input | Keyboard input | Missing | Missing | Missing | Needs input plane and host injection adapter. |
| Input | Mouse input | Missing | Missing | Missing | Needs absolute and relative modes. |
| Input | Relative mouse mode | Missing | Missing | Missing | Critical for games; not implemented. |
| Input | Immersive keyboard mode | Missing | Missing | Missing | Requires client capture and platform-specific shortcut handling. |
| Input | Gamepad passthrough | Missing | Missing | Missing | Needs controller capture, transport, and host injection. |
| Input | XInput emulation | Missing | Not applicable | Missing | Requires Windows virtual controller driver or ViGEm-like integration. |
| Input | Multiple controllers | Missing | Missing | Missing | Requires controller identity and permission model. |
| Input | Controller mapping/deadzones | Missing | Missing | Missing | Client UX feature, not started. |
| Input | Pen/tablet support | Missing | Missing | Missing | Requires high-fidelity pointer/tablet protocol. |
| Input | USB passthrough | Missing | Missing | Missing | Large separate subsystem; not started. |
| Social | Friends list | Missing | Missing | Missing | Requires accounts and contacts. |
| Social | Granular permissions | Planned | Planned | Planned | Pairing exists; per-user keyboard/mouse/gamepad grants do not. |
| Social | Share links | Missing | Missing | Missing | Requires account service, tokenized grants, and host approval UI. |
| Social | Auto-accept trusted users | Partial | Partial | Partial | Host CLI has auto-approve pairing for development only. |
| Social | Kick/ban connected user | Missing | Missing | Missing | Requires live session management and UI. |
| UX | Tray app | Missing | Missing | Missing | Host CLI exists; no tray app. |
| UX | Client app | Missing | Missing | Missing | Client CLI exists; no Tauri UI. |
| UX | Browser client | Planned | Planned | Planned | WebRTC signaling messages exist; no web client. |
| UX | On-screen overlay | Missing | Missing | Missing | Needs renderer/client UI. |
| UX | Fullscreen/windowed/borderless | Missing | Missing | Missing | Needs renderer/client UI. |
| Settings | Bandwidth limits | Missing | Missing | Missing | Needs media transport and congestion control. |
| Settings | Renderer selection | Missing | Missing | Missing | Needs renderer backends. |
| Settings | Advanced config file | Planned | Planned | Planned | Identity and pair store paths exist; full app config does not. |
| Privacy | Privacy mode/blank host monitor | Missing | Missing | Missing | Requires virtual display or platform-specific display control. |
| Enterprise | Teams/admin policy | Missing | Missing | Missing | Out of scope until core app works. |
| Enterprise | Audit logs | Missing | Missing | Missing | Needed before production use. |

## Current Priority Cut

1. Linux host: real PipeWire capture plus H.264 hardware encode.
2. Windows host: Desktop Duplication plus H.264 hardware encode.
3. WebRTC: real peer connection setup with STUN/TURN config.
4. Native desktop: QUIC datagrams, pacing, and client-side decode/render path.
5. Input: keyboard, mouse, relative pointer, and gamepad path.
