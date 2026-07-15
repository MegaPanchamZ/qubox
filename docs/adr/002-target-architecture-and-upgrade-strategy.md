# ADR-002 Target Architecture and Upgrade Strategy

## Status

Accepted

## Context

Qubox currently proves that a host, signaling service, and native client can negotiate and stream media, but the runtime remains prototype-grade. The current implementation relies on reliable ordered media delivery, subprocess-based capture and decode flow, minimal input semantics, and a weak presentation stack. The project needs a target architecture and a sequencing strategy that can improve latency, input quality, scaling, and multi-platform support without requiring a risky big-bang rewrite.

## Decision

Adopt an incremental replacement strategy with the following architecture goals:

1. Keep the existing high-level plane separation: signaling, media, transport, platform, and apps.
2. Replace reliable ordered media streaming with a packetized real-time media layer built around QUIC datagrams for native media.
3. Add a client receive pipeline with reorder buffering, frame assembly, FEC, decode handoff, and presentation queues.
4. Replace the current native viewer runtime with a real windowing and presentation stack.
5. Expand the protocol and runtime around a first-class input subsystem, including gamepad support.
6. Preserve FFmpeg as a compatibility and fallback tool, but stop treating subprocess-driven FFmpeg flows as the final architecture for core runtime behavior.
7. Prefer platform-native capture and acceleration backends behind Rust-owned abstractions.

## Consequences

- Major work should proceed in phases rather than through a single rewrite.
- New protocol types, crates, and app modules will be introduced over time.
- Existing prototype paths may coexist temporarily with the production path behind feature flags or alternate commands.
- Browser support remains a later phase and should use a separate transport profile rather than distorting the native path.
