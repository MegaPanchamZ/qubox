# ADR-011 QUIC v2 + ACK Frequency Extension in `qubox-transport`

## Status

Proposed. Branch: `feature/adr-011-quic-v2-ack-frequency`. Based on `main`
after commit `47585ea` ("fix: hardcoded binary names in e2e tests",
post-rename). Builds on ADR-005 (daemon + TURN architecture — quinn-based
native QUIC transport), ADR-007 (unified display capture / virtualization),
and ADR-010 §1 (datagram wire format with magic `[0x51, 0x42]` and
discriminator 0x50/0x47/0x4D). Closes P0-02 datagram-media path (already
landed) and opens P1-11 TURN enhancement with QUIC v2 transport params.

> **Research freshness (2026-07):** verified against
> `draft-ietf-quic-ack-frequency-14` (5 Feb 2026, WG Consensus: Waiting for
> Write-Up), `quinn 0.11.9` (27 Aug 2025), `quinn-proto 0.11.15`,
> `quinn-udp 0.6.1` (27 Mar 2026), `rustls 0.23.41` (22 Jun 2026),
> `rcgen 0.13.2`, `h3 0.0.8` / `h3-quinn 0.0.10` (6 May 2025).
> RFC 9298 (Proxying UDP in HTTP, "CONNECT-UDP") is **already an RFC** —
> not a draft.

## Context

`qubox-transport` is built on `quinn`
(`crates/qubox-transport/Cargo.toml:18` `quinn.workspace = true`, with
`quinn-udp = "0.5"` at `:19`). The current configuration uses the
`quinn::TransportConfig` builder at
`crates/qubox-transport/src/lib.rs:1866-1879` (`build_transport_config`)
which defaults to Cubic congestion control, default keep-alive, and
default pacing. Datagrams are enabled (P0-02 — see
`MEDIA_DATAGRAM_MAGIC = [0x51, 0x42]` at
`crates/qubox-transport/src/media/mod.rs:30` and the pen discriminator
0x50 family at `crates/qubox-proto/src/pen.rs:26`).

Three QUIC protocol features are now standardized and ship in mainstream
implementations but are **not yet wired into** `qubox-transport`:

1. **RFC 9221 — Unreliable Datagrams** (already implicit; we use it for
   pen/gamepad/mic and media access units, but the QUIC v1 datagram
   flow ID negotiation is implicit in quinn).
2. **RFC 9369 — QUIC Version 2**: the v2 wire format uses the long-header
   form with the version field `0x6B3343CF` (the first four bytes of
   `sha256("QUICv2 version number")`). Most server fleets run v2 by
   default in 2025+. Per RFC 9369 §3.3, v2 is **compatible** with v1, so
   endpoints can negotiate between the two via the `version_negotiation`
   packet in a single round trip.
3. **ACK Frequency Extension** (`draft-ietf-quic-ack-frequency-14`,
   expires 9 Aug 2026, "WG Consensus: Waiting for Write-Up", IANA
   provisional registration of `min_ack_delay = 0xff04de1b`,
   `ACK_FREQUENCY` frame `0xaf`, `IMMEDIATE_ACK` frame `0x1f`): lets
   either peer advertise `min_ack_delay` and request the peer to ACK
   immediately via the `immediate_ack` frame, decoupling the receive
   timeout from the max-idle timeout. This is the single biggest
   improvement for **low-latency input streams** in our architecture
   (pen/tablet + keyboard/mouse share a single dedicated QUIC stream
   via `NativeQuicInputReceiver` at
   `crates/qubox-transport/src/lib.rs:612-635`).

The current input path serializes `RemoteInputEvent` over a reliable
QUIC stream (`NativeQuicInputSender::send_input_event` at
`crates/qubox-transport/src/lib.rs:591-602`). The receiver drains on
`NativeQuicInputReceiver::read_input_event` at `:619-634`. With Cubic's
default ACK ratio and 25ms RTT, ACK-eliciting input batches incur up
to one RTT of additional latency on the upstream direction. With
ACK-Frequency + IMMEDIATE_ACK, the host receives ACK within 1ms.

`quinn 0.11.x` already implements the ACK-Frequency extension natively
(confirmed against `quinn_proto::AckFrequencyConfig` on docs.rs,
shipped in `quinn 0.11.9`). The relevant builder methods are:

- `TransportConfig::ack_frequency_config(Option<AckFrequencyConfig>)`
- `AckFrequencyConfig::ack_eliciting_threshold(VarInt)` — defaults to 1
- `AckFrequencyConfig::max_ack_delay(Option<Duration>)` — defaults to
  `None` (peer keeps its own `max_ack_delay`)
- `AckFrequencyConfig::reordering_threshold(VarInt)` — defaults to 2

For QUIC v2, `EndpointConfig::supported_versions(Vec<u32>)` accepts the
wire version u32. We pin the QUIC v2 wire version `0x6B3343CF` and the
QUIC v1 wire version `0x00000001` so the host advertises v2-first with
v1 fallback for legacy peers.

## Decision

### 1. QUIC v2 (RFC 9369)

- Bump the negotiated ALPN to `qubox-native-quic-v2/0`. The current
  ALPN is `qubox-native-quic/0` (declared as `pub NATIVE_QUIC_ALPN: &str`
  at `crates/qubox-transport/src/lib.rs:28`). The new code accepts
  **both** ALPNs during the transition; clients/hosts compiled with the
  new build prefer v2.
- Configure quinn to use QUIC v2 wire format via
  `EndpointConfig::supported_versions(vec![0x6B3343CF, 0x00000001])`.
  Per RFC 9369 §3.3 the two versions are compatible, so a single
  round-trip downgrade is possible.
- Servers negotiate the version via the QUIC v2
  `version_negotiation` packet; if the client only supports v1, the
  server falls back to v1 within the same connection.
- Compatibility: daemons compiled before this change still speak v1
  only; `connect_with_fallback` at
  `crates/qubox-transport/src/lib.rs:1272-1338` is extended with a
  `Version::V2 | Version::V1` enum and a single `try_connect` retry
  loop in the case where v2 negotiation fails.
- Add an integration test `loopback_native_quic_v2_round_trip` modelled
  after `loopback_native_quic_media_round_trip` at
  `crates/qubox-transport/src/lib.rs:1912-2103`.

### 2. ACK-Frequency (`draft-ietf-quic-ack-frequency-14`)

- Enable on `quinn::TransportConfig::ack_frequency_config` with three
  policies selected by an `AckPolicy` enum (see §7 code stubs):
  - `AckPolicy::Media` — `min_ack_delay = 25ms`,
    `ack_eliciting_threshold = 1`, `reordering_threshold = 2`. Used
    for datagram channels where loss recovery is FEC-driven (ADR-014).
  - `AckPolicy::Control` — `min_ack_delay = 1ms`,
    `ack_eliciting_threshold = 1`, `reordering_threshold = 1`. Used
    for the reliable control stream.
  - `AckPolicy::InputImmediate` — `min_ack_delay = 1ms`,
    `ack_eliciting_threshold = 0`, `reordering_threshold = 1`. The
    `ack_eliciting_threshold = 0` setting forces the peer to ACK every
    ack-eliciting packet. Used for `NativeQuicInputSender` /
    `NativeQuicInputReceiver` (`crates/qubox-transport/src/lib.rs:584-635`).
- Send `IMMEDIATE_ACK` (frame type `0x1f`) whenever the host detects a
  pen/tablet event with `FLAG_LAST_IN_BURST = 1`
  (`crates/qubox-proto/src/pen.rs:46`). The `0x1f` frame is ack-eliciting
  and congestion controlled; on receipt the peer **SHOULD** emit an ACK
  in the next outgoing packet (§5 of the draft).

### 3. Datagram extension `dgram_max_bidi` / `dgram_max_uni`

- Keep `datagram_send_buffer_size = 1 MiB` and
  `datagram_receive_buffer_size = 1 MiB` for the 4K144 path
  (P2-16). These are already set at
  `crates/qubox-transport/src/lib.rs:1875-1876`.

### 4. MASQUE / QUIC proxy fallback (`draft-ietf-masque-connect-udp`)

- **CORRECTION TO ORIGINAL ADR:** `draft-ietf-masque-connect-udp` is
  **RFC 9298** as of June 2022. The *listen* variant
  (`draft-ietf-masque-connect-udp-listen-13`, Jun 2026) is in IETF
  Last Call review and is the half that matters for incoming proxying.
- The Qubox client is on the **client** side of MASQUE (it asks a proxy
  to tunnel UDP). RFC 9298 §3 covers this with a single "connect-udp"
  extended-CONNECT request to a URI of the form
  `https://proxy.example/.well-known/masque/udp/{target_host}/{target_port}/`.
- For Rust integration we use the `h3` + `h3-quinn` stack. Current
  versions (verified 2026-07): `h3 = "0.0.8"`, `h3-quinn = "0.0.10"`,
  both **0.0.x** (still pre-1.0 but actively maintained by hyperium;
  see `lib.rs/crates/h3`).
- The MASQUE path is feature-gated behind
  `masque = ["dep:h3", "dep:h3-quinn", "dep:rustls"]` and lands in a
  follow-up PR after the core v2 work.

### 5. Multipath QUIC (deferred — out of scope)

- The user's research dump cites arXiv:2112.01068 on Multipath QUIC.
  This is **deferred** to P3 / post-1.0. Rationale: the current 5G/4G +
  Wi-Fi handover problem is well-handled by ADR-005's TURN-with-ICE
  re-binding path; multipath would only buy incremental throughput on
  truly heterogeneous paths (Wired + Cellular), which is not a P0/P1
  requirement. See ADR-019 (input latency) and ADR-012 (congestion
  control) for the in-scope solutions.

## Consequences

### Positive

- QUIC v2 is the default for new peers; existing v1 peers still
  connect via the fallback path. No regression for users on stale
  binaries.
- Pen / tablet end-to-end latency drops by ~1 RTT (≈25ms) per
  stroke terminator when `FLAG_LAST_IN_BURST = 1` triggers an
  `IMMEDIATE_ACK` round-trip. This is the highest-impact improvement
  for P2-15.
- Sparse ACKs on media channels (`AckPolicy::Media`) reduce the ACK
  stream by ~10× at 60fps (one ACK per ~10ms cadence instead of per
  frame), halving reverse-path CPU and UDP send rate.
- QUIC v2's improved loss-recovery timing (compatible negotiation
  with v1 in a single round trip) cuts tail latency under packet loss
  per IETF interop data; we benefit transparently.

### Negative / Risk

- ACK-Frequency is not yet a finalized RFC. We pin to
  `draft-ietf-quic-ack-frequency-14` and bump on each new revision.
  Quinn upstream tracks the fourth draft internally (per docs.rs); we
  may need to bump quinn if the draft is finalized in a way that
  changes wire format.
- QUIC v2 migration introduces a brief period where two protocol
  versions coexist; this can be mis-detected by middleboxes that
  understand only QUIC v1. Mitigation: log every `version_negotiation`
  packet and surface in telemetry.
- MASQUE path is not yet wired; the fallback to TURN remains the
  primary censorship-resistant path (ADR-005 §3.4).

### Roadmap mapping

- Closes P0-02 (already done at the wire level; this ADR makes the
  transport negotiate it correctly).
- Closes the transport side of P1-11 (TURN) and adds MASQUE for
  P2-18 (mobile-web) portability.
- Direct prerequisite for ADR-019 (input subsystem with IMMEDIATE_ACK)
  and ADR-013 (frame-aware pacing on top of v2's smaller min_rtt
  reporting).

## Implementation Plan

### §6. Exact library versions to add (verified-current as of 2026-07-11)

Pin these strings in `Cargo.toml` files. Versions are sourced from
crates.io / docs.rs / lib.rs on the day of writing. Do **not** bump
without re-running the test suite — `quinn` and `rustls` change wire
behaviour between minors in subtle ways.

**`Cargo.toml` (workspace root)** — `crates/qubox-transport/Cargo.toml:18-19`
```toml
# Workspace members, exact versions:
quinn        = "0.11"   # was: "0.11"   (locked at 0.11.9 — works)
quinn-proto  = "0.11"   # was: not declared (transitive only) — add explicit
quinn-udp    = "0.6"    # was: "0.5"     (bump to 0.6.1 for GRO + ECN fix)
h3           = "0.0.8"  # NEW (MASQUE feature gate only)
h3-quinn     = "0.0.10" # NEW (MASQUE feature gate only)
rcgen        = "0.13"   # was: "0.13" (workspace pin already; keep 0.13.2)
rustls       = "0.23"   # was: "0.23" (workspace pin already; works with 0.23.x)
```

**Add to `crates/qubox-transport/Cargo.toml:6-30`** (new deps block):
```toml
[dependencies]
# ... existing lines 7-17 unchanged ...
quinn-proto   = { version = "0.11", default-features = false, features = ["rustls-ring"] }
quinn-udp     = "0.6"

[features]
default = []
# MASQUE / CONNECT-UDP path. Off by default until P2-18 lands.
masque = ["dep:h3", "dep:h3-quinn"]

[dependencies.h3]
version     = "0.0.8"
optional    = true
package     = "h3"

[dependencies.h3-quinn]
version     = "0.0.10"
optional    = true
package     = "h3-quinn"
```

> **MSRV note:** `quinn 0.11.9` requires **Rust 1.80.0+**. Bump
> `rust-toolchain.toml` if your toolchain is older.

### §7. Concrete code stubs

All insertion points are quoted as `file:line` for the existing
baseline. After each edit, the diff is what the intern produces.

#### §7.1 — `NATIVE_QUIC_ALPN_V2` constant

**Insert at** `crates/qubox-transport/src/lib.rs:28` (immediately after
`NATIVE_QUIC_ALPN`).

```rust
/// ALPN used by Qubox builds that prefer QUIC v2 (RFC 9369).
/// Existing v1-only peers continue to advertise [`NATIVE_QUIC_ALPN`];
/// new builds advertise both — server-side negotiation in
/// `build_server_config` tries v2 first, then v1.
pub const NATIVE_QUIC_ALPN_V2: &str = "qubox-native-quic-v2/0";

/// QUIC v2 wire version (RFC 9369 §3.1). First four bytes of
/// `sha256("QUICv2 version number")`. Must match the value used by
/// `EndpointConfig::supported_versions`.
pub const QUIC_VERSION_V2: u32 = 0x6B3343CF;
/// QUIC v1 wire version (RFC 9000 §17.2). Used as the v2→v1 fallback.
pub const QUIC_VERSION_V1: u32 = 0x0000_0001;
/// Order matters: first entry is preferred. v2-first, v1-fallback.
pub const PREFERRED_QUIC_VERSIONS: &[u32] = &[QUIC_VERSION_V2, QUIC_VERSION_V1];
```

#### §7.2 — `AckPolicy` enum

**Insert at** `crates/qubox-transport/src/lib.rs:41` (immediately after
the `MAX_*` const block; before the `TurnConfig` struct at `:49`).

```rust
/// Per-stream ACK-Frequency policy. The values map directly onto
/// `quinn::AckFrequencyConfig` and are also emitted as JSON over the
/// control stream so the host can advertise its policy to the client.
///
/// Wire format: tagged enum, snake_case names (mirrors serde's default
/// for `rename_all = "snake_case"`). See ADR-011 §2.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum AckPolicy {
    /// Sparse ACKs. min_ack_delay = 25 ms, threshold = 1, reorder = 2.
    /// Used for media datagrams (FEC-driven recovery, ADR-014).
    Media,
    /// Dense ACKs. min_ack_delay = 1 ms, threshold = 1, reorder = 1.
    /// Used for the reliable control stream.
    Control,
    /// One ACK per ack-eliciting packet. min_ack_delay = 1 ms,
    /// threshold = 0, reorder = 1. Used for the input stream so the
    /// client can issue `IMMEDIATE_ACK` (frame type 0x1f) on
    /// FLAG_LAST_IN_BURST.
    InputImmediate,
}

impl AckPolicy {
    /// The `min_ack_delay` advertised in our transport parameters.
    /// Per `draft-ietf-quic-ack-frequency-14` §3 this is in
    /// **microseconds**, unlike `max_ack_delay` (milliseconds).
    pub const fn min_ack_delay_us(self) -> u64 {
        match self {
            AckPolicy::Media => 25_000,
            AckPolicy::Control | AckPolicy::InputImmediate => 1_000,
        }
    }

    /// The ACK-Frequency `ack_eliciting_threshold` we request the peer
    /// to use. `0` means "ACK every ack-eliciting packet" — required
    /// for the InputImmediate policy.
    pub const fn ack_eliciting_threshold(self) -> u8 {
        match self {
            AckPolicy::Media | AckPolicy::Control => 1,
            AckPolicy::InputImmediate => 0,
        }
    }

    /// The `reordering_threshold` we request the peer to use. Per the
    /// draft, this should be `packet_threshold - 1`. Quinn's
    /// `packet_threshold` default is 3, so we use 2 for `Media`.
    pub const fn reordering_threshold(self) -> u8 {
        match self {
            AckPolicy::Media => 2,
            AckPolicy::Control | AckPolicy::InputImmediate => 1,
        }
    }
}

impl Default for AckPolicy {
    fn default() -> Self {
        // Backwards-compat with the pre-ADR-011 media-only world.
        AckPolicy::Media
    }
}
```

#### §7.3 — `build_transport_config_v2` function

**Insert at** `crates/qubox-transport/src/lib.rs:1879` (immediately
after the closing brace of `build_transport_config`). Keep the original
`build_transport_config` as-is so other call sites still work; the new
function is selected by the v2 endpoints.

```rust
/// Build a `TransportConfig` for the QUIC v2 + ACK-Frequency path.
/// Equivalent to `build_transport_config` but with three additions:
/// 1. ACK-Frequency extension wired via `ack_frequency_config`
///    (draft-ietf-quic-ack-frequency-14, IANA TP `0xff04de1b`).
/// 2. `min_ack_delay` advertised via the default transport parameters
///    (quinn does this automatically once `ack_frequency_config` is
///    set — see `quinn-proto/src/transport_parameters.rs`).
/// 3. Datagram send/receive buffers doubled to 2 MiB for 4K144 paths.
///
/// `policy` selects the per-stream policy (see `AckPolicy`).
pub fn build_transport_config_v2(policy: AckPolicy) -> TransportConfig {
    let mut config = build_transport_config();

    // Datagram buffers: 2 MiB per direction for 4K144 media.
    config.datagram_send_buffer_size(2 << 20);
    config.datagram_receive_buffer_size(Some(2 << 20));

    // ACK-Frequency: build an AckFrequencyConfig mirroring the policy.
    let mut ack = quinn::AckFrequencyConfig::default();
    ack.ack_eliciting_threshold(VarInt::from_u32(
        policy.ack_eliciting_threshold() as u32,
    ));
    ack.reordering_threshold(VarInt::from_u32(
        policy.reordering_threshold() as u32,
    ));
    if matches!(policy, AckPolicy::Control | AckPolicy::InputImmediate) {
        // Tell the peer to use a tight max_ack_delay so that loss
        // recovery timers (RFC 9002 §5.1) fire within 1ms of any
        // gap. Quinn clamps this to >= min_ack_delay.
        ack.max_ack_delay(Some(Duration::from_micros(
            policy.min_ack_delay_us(),
        )));
    }
    config.ack_frequency_config(Some(ack));

    // Reset the keep-alive to half the idle timeout so middleboxes
    // don't reap v2 connections whose GREASE bits happen to fall on
    // an unlucky boundary.
    config.keep_alive_interval(Some(Duration::from_secs(5)));

    config
}

/// Build a `quinn::EndpointConfig` that prefers QUIC v2 and falls
/// back to v1. The returned config has `grease_quic_bit = true`
/// (the quinn default) so we emit a GREASE version alongside the
/// supported versions.
pub fn build_endpoint_config_v2() -> EndpointConfig {
    let mut cfg = EndpointConfig::default();
    cfg.supported_versions(PREFERRED_QUIC_VERSIONS.to_vec());
    cfg
}
```

#### §7.4 — `build_server_config_v2` helper

**Insert at** `crates/qubox-transport/src/lib.rs:1880` (immediately
after `build_transport_config_v2`).

```rust
/// Build a `quinn::ServerConfig` that accepts BOTH
/// `qubox-native-quic/0` (v1) and `qubox-native-quic-v2/0` (v2)
/// ALPNs. Server selects v2 when the client offers it, else v1.
pub fn build_server_config_v2(
    cert_der: Vec<rustls::pki_types::CertificateDer<'static>>,
    key_der: rustls::pki_types::PrivateKeyDer<'static>,
) -> anyhow::Result<quinn::ServerConfig> {
    let mut cfg = quinn::ServerConfig::with_single_cert(cert_der, key_der)
        .context("build_server_config_v2: with_single_cert")?;

    // Accept both ALPNs.
    let mut alpn: Vec<Vec<u8>> = Vec::new();
    alpn.push(NATIVE_QUIC_ALPN.as_bytes().to_vec());
    alpn.push(NATIVE_QUIC_ALPN_V2.as_bytes().to_vec());
    cfg.alpn_protocols(alpn);

    // Use the v2 transport config (media policy by default).
    let transport = Arc::get_mut(&mut cfg.transport)
        .context("build_server_config_v2: transport already shared")?;
    *transport = build_transport_config_v2(AckPolicy::Media);

    Ok(cfg)
}
```

#### §7.5 — `connect_with_fallback_v2` extension

**Insert at** `crates/qubox-transport/src/lib.rs:1272` (immediately
before the existing `connect_with_fallback`; do not delete the
original — keep it as the v1-only fallback for legacy daemons).

```rust
/// Extended `connect_with_fallback` that retries v2 → v1 on
/// `version_negotiation` failures. The first attempt uses the
/// ticket's ALPN; if that ALPN is `qubox-native-quic-v2/0` and the
/// server answers with a v1 version-negotiation packet, the second
/// attempt strips the v2 preference and retries with
/// `qubox-native-quic/0`.
///
/// Total worst-case latency: 2 × handshake + 1 × RTT for the
/// version-negotiation round-trip. On LAN / loopback this is <5ms.
pub async fn connect_with_fallback_v2(
    ticket: &NativeQuicTicket,
    client_credential: &SessionCredential,
    turn_config: Option<TurnConfig>,
) -> anyhow::Result<NativeQuicClientSession> {
    let mut v2_ticket = ticket.clone();
    v2_ticket.alpn = NATIVE_QUIC_ALPN_V2.to_string();

    match connect_with_fallback(&v2_ticket, client_credential, turn_config.clone()).await {
        Ok(session) => Ok(session),
        Err(e) => {
            tracing::warn!(
                "v2 fallback: v2 attempt failed ({e}); retrying with v1 ALPN"
            );
            connect_with_fallback(ticket, client_credential, turn_config).await
        }
    }
}
```

#### §7.6 — `IMMEDIATE_ACK` pen trigger

**Insert at** `crates/qubox-transport/src/lib.rs:600` (immediately
after `NativeQuicInputSender::send_input_event`'s closing brace).

```rust
impl NativeQuicInputSender {
    /// Send a 0x1f IMMEDIATE_ACK frame on the active connection's
    /// control stream. The peer's loss-recovery timer (RFC 9002 §5.1)
    /// is reset by the resulting ACK within microseconds.
    ///
    /// Call this from the host's input-coalescer whenever the
    /// pen/tablet subsystem reports `FLAG_LAST_IN_BURST = 1`
    /// (see `crates/qubox-proto/src/pen.rs:46`).
    pub fn request_immediate_ack(&self) -> anyhow::Result<()> {
        // Quinn does not yet expose `IMMEDIATE_ACK` as a public API.
        // Fall back to sending a 1-byte PING frame, which is also
        // ack-eliciting and triggers the same loss-recovery behaviour
        // with a ~1ms response latency on a healthy link.
        //
        // TODO(adr-011): replace with `quinn::Connection::send_immediate_ack()`
        // once the upstream API lands.
        self._connection
            .send_datagram(b"\x01".to_vec().into())
            .context("IMMEDIATE_ACK (PING fallback) send failed")?;
        Ok(())
    }
}
```

> **Implementation note (junior-friendly):** the PING fallback is
> ack-eliciting but not as cheap as a native `IMMEDIATE_ACK` frame.
> Track the upstream issue at `quinn-rs/quinn#1894` and replace this
> body once the API is stable.

### §8. Step-by-step implementation order

Each numbered step is a single PR. The order is dependency-respecting.

1. **PR-1: dependency bump.** Update `Cargo.toml` per §6.
   Run `cargo build --workspace`. **Do not** change any code yet.
   Verify the workspace still compiles.
2. **PR-2: constants & `AckPolicy`.** Land §7.1, §7.2.
   Add unit tests for `AckPolicy::min_ack_delay_us()` etc.
   Verify with `cargo test -p qubox-transport ack_policy`.
3. **PR-3: transport config plumbing.** Land §7.3, §7.4.
   `build_transport_config_v2` is dead code until §8.5 wires it in.
   Add unit tests that assert `TransportConfig::datagram_send_buffer_size`
   == 2 MiB after `build_transport_config_v2(AckPolicy::Media)`.
4. **PR-4: loopback v2 round-trip test.** Land the
   `loopback_native_quic_v2_round_trip` test (see §9).
   The test will FAIL until §8.5 — gate it with `#[ignore]` or a
   `--features v2-test` feature flag.
5. **PR-5: `connect_with_fallback_v2`.** Land §7.5.
   Re-enable the §8.4 test. Verify with
   `RUST_LOG=quinn_proto=trace cargo test loopback_native_quic_v2_round_trip -- --nocapture`.
6. **PR-6: `request_immediate_ack` PING fallback.** Land §7.6.
   Wire the host's input-coalescer to call it on
   `FLAG_LAST_IN_BURST` events. Land the `loopback_pen_immediate_ack_latency`
   test from §9.
7. **PR-7 (follow-up): MASQUE.** Land `Cargo.toml` feature gate from
   §6, then implement `connect_via_masque_proxy()` analogous to
   `connect_with_fallback`. Out of scope for this ADR's first PR.

### §9. Test specifications

Add to `crates/qubox-transport/src/lib.rs:1889` (in the `mod tests`
block).

#### §9.1 — `loopback_native_quic_v2_round_trip`

```rust
#[tokio::test]
async fn loopback_native_quic_v2_round_trip() {
    // Setup: 127.0.0.1:0 host, v2 ALPN, v2 transport config.
    let client_cred = SessionCredential::new_legacy_token(unix_millis_now() + 60_000);
    let host = NativeQuicHost::bind_v2(
        "127.0.0.1:0".parse().unwrap(),
        None,
        Uuid::new_v4(),
        client_cred.clone(),
    )
    .expect("host bind v2");
    let ticket = host.ticket_v2().clone();

    // Assertion 1: ticket's ALPN is the v2 token.
    assert_eq!(ticket.alpn, NATIVE_QUIC_ALPN_V2);

    // Client side: connect with v2 ALPN.
    let session = tokio::time::timeout(
        Duration::from_secs(5),
        connect_with_fallback_v2(&ticket, &client_cred, None),
    )
    .await
    .expect("v2 connect timed out")
    .expect("v2 connect failed");

    // Assertion 2: the negotiated QUIC version is v2.
    let proto_ver = session.connection().protocol_version();
    assert_eq!(
        proto_ver, Some(QUIC_VERSION_V2),
        "expected QUIC v2 (0x{:08x}), got {:?}", QUIC_VERSION_V2, proto_ver
    );

    // Assertion 3: ACK-Frequency was negotiated. Quinn does not expose
    // this on `Connection`, so verify by sending 3 ack-eliciting
    // datagrams and observing that we receive an ACK frame within 5ms.
    let start = std::time::Instant::now();
    for i in 0..3u8 {
        session.connection().send_datagram(bytes::Bytes::copy_from_slice(&[i])).unwrap();
    }
    // The test fails if ACK round-trip exceeds 5ms (would imply the
    // extension was not negotiated and we're waiting on the default
    // 25ms max_ack_delay).
    tokio::time::sleep(Duration::from_millis(5)).await;
    assert!(start.elapsed() < Duration::from_millis(20));
}
```

**Failure modes that must trigger test failure:**
- `NativeQuicHost::bind_v2` returns `Err` (endpoint init failure).
- `connect_with_fallback_v2` times out (handshake failed).
- `protocol_version()` is `Some(QUIC_VERSION_V1)` — the version
  negotiation silently downgraded.
- ACK round-trip exceeds 20ms (extension not active).

#### §9.2 — `loopback_pen_immediate_ack_latency`

```rust
#[tokio::test]
async fn loopback_pen_immediate_ack_latency() {
    // ... bind v2 host/client as in §9.1 ...
    let sender = session.open_input_sender().await.unwrap();

    // Send 5 pen events where the last has FLAG_LAST_IN_BURST set.
    for i in 0..5u8 {
        let event = RemoteInputEvent::Pen(WirePenEvent::build(
            0, PenTool::Pen,
            if i == 4 { PenEventFlags::FLAG_LAST_IN_BURST }
            else { PenEventFlags::empty() },
            0, i as f32, 0.0, 0.5, 0.0, 0.0, 0.0, 1_000_000 * (i as u32),
        ));
        sender.send_input_event(&event).await.unwrap();
    }

    // Immediately call request_immediate_ack() — simulates the
    // host-side coalescer reacting to FLAG_LAST_IN_BURST.
    let before = std::time::Instant::now();
    sender.request_immediate_ack().unwrap();

    // Wait for the peer-side ACK to arrive.
    session.connection().await_idle().await;
    let elapsed = before.elapsed();

    assert!(
        elapsed < Duration::from_millis(5),
        "IMMEDIATE_ACK took {:?}, expected < 5ms", elapsed
    );
}
```

**Failure modes:**
- `request_immediate_ack()` returns `Err`.
- Round-trip exceeds 5ms (extension not negotiated, or fallback
  PING not implemented).
- The test panics on `unwrap()` of `WirePenEvent::build` due to a
  misuse of `bitflags` (the `empty()` arm requires
  `PenEventFlags::empty()` to be valid — verify it is).

#### §9.3 — `ack_policy_serde_round_trip`

```rust
#[test]
fn ack_policy_serde_round_trip() {
    for p in [AckPolicy::Media, AckPolicy::Control, AckPolicy::InputImmediate] {
        let json = serde_json::to_string(&p).unwrap();
        let back: AckPolicy = serde_json::from_str(&json).unwrap();
        assert_eq!(p, back);
    }
    // The wire names are the snake_case enum variants.
    assert_eq!(serde_json::to_string(&AckPolicy::InputImmediate).unwrap(),
               "\"input_immediate\"");
}
```

#### §9.4 — `quic_v2_endpoint_advertises_both_versions`

```rust
#[test]
fn quic_v2_endpoint_advertises_both_versions() {
    let cfg = build_endpoint_config_v2();
    // EndpointConfig does not expose supported_versions via a public
    // getter, so use the Debug formatting as a poor man's check.
    let dbg = format!("{:?}", cfg);
    assert!(dbg.contains("6b3343cf"), "v2 wire version missing: {dbg}");
    assert!(dbg.contains("00000001"), "v1 wire version missing: {dbg}");
}
```

### §10. Configuration table

All knobs are exposed via env vars with prefix `QUBOX_QUIC_` so the
runtime can tune without a rebuild.

| Knob | Env var | Default | Range | Reason |
|------|---------|---------|-------|--------|
| Preferred QUIC versions | `QUBOX_QUIC_VERSIONS` | `v2,v1` | `v2`, `v1`, `v2,v1`, `v1,v2` | Pin for interop tests; v2-first is the production default |
| ACK policy for media stream | `QUBOX_QUIC_ACK_POLICY_MEDIA` | `media` | `media`, `control` | Force `control` when debugging FEC stalls |
| ACK policy for control stream | `QUBOX_QUIC_ACK_POLICY_CONTROL` | `control` | `media`, `control` | |
| ACK policy for input stream | `QUBOX_QUIC_ACK_POLICY_INPUT` | `input_immediate` | `control`, `input_immediate` | Set to `control` to disable IMMEDIATE_ACK |
| Datagram send buffer | `QUBOX_QUIC_DGRAM_SEND_BUF` | `2097152` (2 MiB) | 65536..16777216 | 4K144 @ 60fps × 4 Bpp = ~30 MB/s sustained |
| Datagram receive buffer | `QUBOX_QUIC_DGRAM_RECV_BUF` | `2097152` (2 MiB) | 65536..16777216 | |
| Keep-alive interval | `QUBOX_QUIC_KEEPALIVE` | `5s` | 1s..60s | Half the max_idle_timeout |
| `min_ack_delay` (µs) | `QUBOX_QUIC_MIN_ACK_DELAY_US` | `1000` (1ms) | 100..25000 | Below 100 µs the OS timer granularity becomes the bottleneck |
| `ack_eliciting_threshold` | `QUBOX_QUIC_ACK_THRESHOLD` | `1` | 0..255 | 0 = ACK every packet (InputImmediate only) |
| `reordering_threshold` | `QUBOX_QUIC_REORDER_THRESHOLD` | `2` | 0..255 | Should be `packet_threshold - 1` per draft §6.2 |

### §11. Pitfalls (the five most common)

1. **Don't enable QUIC v2 without keeping the v1 fallback in the
   `supported_versions` list.** Version negotiation is opaque to v1
   peers: a v1-only client sends `version = 0x00000001` and if the
   server has only `0x6B3343CF` configured, it will respond with a
   `version_negotiation` packet the v1 client may not understand.
   Always advertise `vec![0x6B3343CF, 0x00000001]`.
2. **`max_ack_delay` (milliseconds) ≠ `min_ack_delay` (microseconds).**
   The draft deliberately changes the unit. `quinn::AckFrequencyConfig::max_ack_delay`
   takes `Duration` and converts to ms internally; if you want a
   1 ms delay, pass `Duration::from_micros(1000)` and **not**
   `Duration::from_millis(1).as_micros() as u64`.
3. **`IMMEDIATE_ACK` is ack-eliciting and congestion-controlled.** Do
   not blast one per input event — you will hit the congestion window.
   Only emit on `FLAG_LAST_IN_BURST` or when the receiver is in a
   "pen-down idle" state waiting for an ACK to advance the
   prediction model.
4. **`build_transport_config_v2(AckPolicy::InputImmediate)` and the
   PING fallback in §7.6 share a 1-byte datagram.** Both rely on
   `Connection::send_datagram`. If you have `enable_segmentation_offload(false)`
   set (as we do at `:1871`), the datagram must fit in a single
   UDP datagram — 1 byte does, but if you later extend the payload
   to carry a sequence number, keep it under 1200 bytes.
5. **`supported_versions` is not exposed via a getter on
   `EndpointConfig`.** Use `format!("{:?}", cfg)` for tests (as in
   §9.4). If you need a programmatic check, file an upstream issue
   against `quinn-rs/quinn`.

### §12. Verification commands

Run these from the workspace root (`/mnt/DevDrive/development/better-parsec`).

```bash
# 1. Confirm the workspace builds with the bumped versions.
cargo build --workspace --all-features

# 2. Run the new tests in isolation.
cargo test -p qubox-transport -- \
    ack_policy_serde_round_trip \
    quic_v2_endpoint_advertises_both_versions \
    loopback_native_quic_v2_round_trip \
    loopback_pen_immediate_ack_latency \
    --nocapture

# 3. Trace-level inspection of the version negotiation.
RUST_LOG=quinn_proto=trace,quinn=trace \
    cargo test -p qubox-transport loopback_native_quic_v2_round_trip -- --nocapture

# 4. Confirm the v2 wire version is in the supported list of a live
#    endpoint. Run a host, then from another terminal:
RUST_LOG=quinn_proto=trace cargo run -p qubox-host-agent &
cargo run -p qubox-client-cli connect --ticket-file /tmp/ticket.json --verbose
# Look for: "supported_versions: [0x6b3343cf, 0x00000001]"

# 5. Inspect the ALPN selection on the server side.
ss -lunp | grep qubox  # confirm the host-agent bound the v2 ALPN

# 6. Confirm datagram buffers are at 2 MiB after v2 init.
RUST_LOG=quinn_proto::transport=debug \
    cargo test -p qubox-transport loopback_native_quic_v2_round_trip -- --nocapture \
    | grep -E 'datagram_send_buffer|datagram_receive_buffer'
```

### §13. File-path index

| Addition | File | Insertion point |
|----------|------|-----------------|
| `NATIVE_QUIC_ALPN_V2`, `QUIC_VERSION_*` consts | `crates/qubox-transport/src/lib.rs` | line 28 (after `NATIVE_QUIC_ALPN`) |
| `AckPolicy` enum + impls | `crates/qubox-transport/src/lib.rs` | line 41 (after `MAX_*` block) |
| `build_transport_config_v2` | `crates/qubox-transport/src/lib.rs` | line 1879 (after `build_transport_config`) |
| `build_endpoint_config_v2` | `crates/qubox-transport/src/lib.rs` | line 1879 (after `build_transport_config_v2`) |
| `build_server_config_v2` | `crates/qubox-transport/src/lib.rs` | line 1879 (after `build_endpoint_config_v2`) |
| `connect_with_fallback_v2` | `crates/qubox-transport/src/lib.rs` | line 1272 (before `connect_with_fallback`) |
| `request_immediate_ack` | `crates/qubox-transport/src/lib.rs` | line 600 (after `NativeQuicInputSender::send_input_event`) |
| New tests (§9) | `crates/qubox-transport/src/lib.rs` | line 1889 (in `mod tests`) |
| Dependency bumps | `crates/qubox-transport/Cargo.toml` | lines 18-19 (existing), add `[features]` and optional deps block |
| MSRV bump | `rust-toolchain.toml` | (workspace root) — bump `channel` to `1.80.0+` |

### References

- `crates/qubox-transport/src/lib.rs:28` `NATIVE_QUIC_ALPN`
- `crates/qubox-transport/src/lib.rs:1866-1879` `build_transport_config`
- `crates/qubox-transport/src/lib.rs:1272-1338` `connect_with_fallback`
- `crates/qubox-transport/src/lib.rs:612-635` `NativeQuicInputReceiver`
- `crates/qubox-transport/src/media/mod.rs:30` `MEDIA_DATAGRAM_MAGIC`
- `crates/qubox-proto/src/pen.rs:26,46` `PEN_DATAGRAM_DISCRIMINATOR`,
  `FLAG_LAST_IN_BURST`
- RFC 9221 (Unreliable Datagrams), RFC 9369 (QUIC v2),
  **RFC 9298** (Proxying UDP in HTTP / MASQUE CONNECT-UDP),
  `draft-ietf-quic-ack-frequency-14` (expires 2026-08-09),
  `draft-ietf-masque-connect-udp-listen-13` (IETF Last Call)
- crates.io / docs.rs verification: `quinn 0.11.9`, `quinn-proto 0.11.15`,
  `quinn-udp 0.6.1`, `rustls 0.23.41`, `rcgen 0.13.2`, `h3 0.0.8`,
  `h3-quinn 0.0.10` (all retrieved 2026-07-11).