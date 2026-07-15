# Critical Architectural Review — Qubox / qubox

**Date:** 2026-07-11  
**Verdict:** Solid single-tenant control-plane + native QUIC media skeleton. Self-host is coherent. Managed hosting (selling point) is **not designed yet**. Public multi-tenant launch is **blocked** by spoofable identity and unauthenticated WS.

---

## 1. Runtime topology

```
┌─────────────┐   WS Hello / Pair / StartSession    ┌──────────────────────┐
│ client-cli  │◄──────────────────────────────────►│ signaling-server     │
│ client-gui  │   RelaySignal (SDP/ICE/QUIC ticket) │ (axum, in-mem peers) │
│  (Tauri)    │                                     │ optional JSON pairs  │
└──────┬──────┘                                     │ TURN REST /v1/turn/* │
       │                                            └──────────┬───────────┘
       │ IPC (UDS / Named Pipe)                                │ ICE URLs
┌──────▼──────┐                                     ┌──────────▼───────────┐
│   daemon    │  owns reconnect, pair UX, TUF,      │ coturn (optional)    │
│ redb state  │  subprocess lifecycle               │ HMAC-SHA1 short-term │
└──────┬──────┘                                     └──────────────────────┘
       │ spawn
┌──────▼──────┐         Native QUIC (quinn)         ┌──────────────────────┐
│ host-agent  │◄──── TLS 1.3 + SessionCredential ──►│ client media path    │
│ capture/enc │         (rcgen self-signed cert)    │ decode / wgpu present│
└─────────────┘                                     └──────────────────────┘
```

| Plane | Components | State |
|-------|------------|-------|
| **Control** | WS signaling, pair grants, session plan, capability negotiate | Skeleton solid |
| **Media** | Native QUIC + H.264 AUs; datagram/FEC growing | Partial |
| **Input** | Proto rich; host injection incomplete | Partial |
| **Platform** | PipeWire/X11, DXGI planned, wgpu present | Uneven by OS |
| **Ops** | systemd, coturn, AWS EC2 script, TUF | Self-host oriented |

**Session flow (actual code):**

1. Load local `DeviceIdentity` (UUIDs only) → `Hello(PeerDescriptor)`
2. Client `RequestPairing` → host `PairingDecision` → `PairingGrant` stored
3. Client `StartSession` (requires grant) → short-lived `SessionCredential` + ICE
4. Relay SDP/ICE/QUIC ticket only for paired peers in active non-expired session
5. Media: native QUIC bootstrap checks credential token + expiry

---

## 2. Self-host vs managed gaps

### Self-host — matches product claim

| Asset | Status |
|-------|--------|
| Single signaling binary + env bind | Yes |
| Optional pair JSON store | Yes |
| coturn compose + docs | Yes |
| systemd unit | Yes |
| Local identity file | Yes |
| Offline if peers share WS URL | Yes |

**Self-host is the design center.** Ship as open-core “bring your own server.”

### Managed — selling point, architecturally absent

| Need | Status |
|------|--------|
| Multi-tenant isolation | **None** — global peer/session maps |
| Account ↔ device binding | **None** |
| Org / billing / quotas | **None** |
| Regional signaling + TURN anycast | Config list only |
| Rate limit / ban / audit | Minimal / none on WS |
| HA, sticky sessions, durable store | In-memory peers; optional flat JSON |
| Admin API / console | **None** |
| Multi-region failover / SLA metrics | Not productized |

What exists as “managed”: `ops/aws/provision-signaling-ec2.ps1`, Docker signaling+coturn, TLS/TUF runbooks.

**Implication:** Managed ≠ “run the same binary harder.” Needs a **hosted control plane + TURN fleet** on the same peer protocol. Do not put media decryption in cloud if E2E trust is the brand.

**Product split:**

1. Open-core self-host (current stack + docs)
2. Managed control plane (accounts, device certs, pair policy, audit, multi-region)
3. Managed edge (TURN + optional opaque QUIC relay)

---

## 3. Security — threats vs controls

### Controls that work

- Session start requires pairing grant
- Signal relay gated to session participants + non-expired plan
- Native QUIC: TLS 1.3 + per-session credential check (`qubaix-transport`)
- Daemon IPC: OS peer creds (UID / named-pipe ACL)
- TUF update path designed
- SECURITY.md coordinated disclosure

### Threats (code-backed)

| Threat | Evidence | Severity |
|--------|----------|----------|
| WS `Hello` claims any `peer_id` | `handle_socket`: unauthenticated register | **Critical** |
| Identity = UUID JSON, no keys | `DeviceIdentity` has no keypair/signatures | **Critical** |
| Bearer = peer UUID string | `validate_bearer` parses UUID, no secret | **High** |
| TURN issue ignores peer binding | `issue_credentials(..., _peer_id)` unused; any non-empty Bearer works | **High** |
| GET relay address unauthenticated | `get_relay_address_handler` no auth | **Medium** |
| Pair store plaintext JSON | architecture + store design | **Medium** |
| Ephemeral self-signed QUIC certs | `rcgen::generate_simple_self_signed` per bind | **Medium** |
| `--auto-approve-pairing` | host-agent footgun → full remote access | **High** if left on |

**Trust model today:**

```
Trust = “I approved this peer_id once on this signaling instance”
      + short-lived random session tokens
```

**Required for public managed:**

```
Trust = account → device cert → signed pair grant → bound session token
```

---

## 4. Account / linking redesign

### Current (device-to-device only)

```
identity.json (device_id, host_peer_id, client_peer_id, name)
  → Hello
  → RequestPairing / PairingDecision
  → PairingGrant{host, client}
  → StartSession → SessionCredential (10 min UUID tokens)
```

No user account, email, friends list, share links, org, revoke UI, audit.

### Target model (managed + optional self-host local mode)

```
UserAccount (OIDC / email+2FA)
  └─ Device (Ed25519 keypair, display name, role, enrolled_at)
       └─ PairGrant (policy: always | ask | TTL | revoke)
            └─ Session (permissions: input|clipboard|mic, audit, kick)
```

**Wire changes (minimal):**

| Layer | Change |
|-------|--------|
| `qubaix-identity` | + `signing_key` / `public_key`; Hello signed |
| `qubaix-proto` | + `SignedHello`, `AccountId`, `DeviceCert`, `PairPolicy`, `SessionPermissions` |
| `qubaix-signaling` | Verify Hello; tenant-scoped maps; durable pair store |
| Managed-only crate | OIDC, device enroll, revoke, audit API |
| Self-host mode | Keep local pair-only (no account required) |

**Linking UX (managed):**

1. User signs in (browser / app)
2. Device generates keypair → enroll → account binds device cert
3. Pair request signed by client device → host approve (push/local UI)
4. Grant stored with policy + audit event
5. Session token = JWT/HMAC bound to both device pubkeys + session_id + expiry
6. Revoke grant → all new sessions fail; optional live kick

---

## 5. Rust crate / bindings boundaries & risks

### Layout (good)

```
crates/  identity | proto | signaling | transport | display | media | mic | pen | clipboard | platform
apps/    host-agent | client-cli | daemon | signaling-server | client-gui (Tauri)
```

**“Bindings” today** = Tauri (Rust↔TS) + `cfg` platform modules. **No C FFI / UniFFI / mobile SDK.**

### Strengths

- Shared `qubaix-proto` wire types
- Capability negotiation in signaling
- Daemon IPC cross-OS (UDS / named pipe)
- wgpu portable present path
- Dual media profiles (Native QUIC vs WebRTC) correctly separated in docs

### Risks

| Risk | Detail |
|------|--------|
| God binaries | High CC: host-agent session loops, client-cli main/viewer, daemon IPC dispatch |
| README vs readiness | Marketing ahead of `production-readiness.md` |
| FFmpeg transitional | Subprocess path not final architecture (ADRs already warn) |
| No mobile/web SDK | WebRTC profile designed, not productized |
| Workspace noise | `scratch/bindgen-test` in members; branding drift (Qubox/Qubox/Qubox) |
| Tauri scope creep | Must stay launcher/settings shell, not media plane |

### Boundary rules going forward

1. **Rust core** = single source of truth for protocol + media
2. **C ABI / UniFFI** only when Android/iOS need it
3. **Managed control plane** = separate deployable service (new crate/app), not stuffed into peer signaling lib forever
4. **Host/client media** extract from `main.rs` into lib modules (session runtime already partial in client)

---

## 6. Findings ranked by severity + next architecture moves

### P0 — Block public managed / internet-facing self-host

| # | Finding | Next move |
|---|---------|-----------|
| **C1** | Unauthenticated WS Hello → peer_id spoof | Require signed Hello; reject unsigned on public mode |
| **C2** | `DeviceIdentity` has no crypto | Add Ed25519 keypair to identity file; migrate schema v2 |
| **C3** | Session tokens not bound to devices | Issue HMAC/JWT: `session_id + host_pk + client_pk + exp` |
| **C4** | TURN Bearer is bare UUID / unused peer_id | Require session-bound token; embed peer in username; rate-limit |
| **C5** | No account plane for managed | New `qubaix-accounts` + managed signaling front: OIDC, device enroll, revoke |

**Concrete sequence:**

1. **Device certs** in `qubaix-identity` (schema_version=2, keypair on disk, public in Hello)
2. **Signaling verify** signatures on Hello / PairingDecision / StartSession
3. **SessionCredential rewrite** — opaque signed token, not `Uuid::new_v4().to_string()`
4. **TURN auth** — only issue after valid session token; bind username to peer+session
5. **Account service** (managed-only) — OIDC → device enroll API → pair policy store (Postgres)

### P1 — Multi-tenant managed control plane

| # | Finding | Next move |
|---|---------|-----------|
| **H1** | Global in-memory peer/session maps | Key all state by `tenant_id`; durable store |
| **H2** | Pair store = flat JSON | Postgres/Redis pair grants + audit log |
| **H3** | No HA / regional edge | Stateless signaling + sticky WS via LB; regional TURN |
| **H4** | No abuse controls | Rate limit Hello/pair/session; captcha on public enroll |
| **H5** | GET relay unauthenticated | Auth + session-scoped visibility |
| **H6** | `auto_approve_pairing` | Forbidden in release builds / managed hosts |

### P2 — Transport hardening & product honesty

| # | Finding | Next move |
|---|---------|-----------|
| **M1** | QUIC certs ephemeral self-signed | Pin device cert or session cert signed by device key |
| **M2** | Media E2E not explicit product rule | ADR: cloud never decrypts; optional opaque QUIC relay |
| **M3** | Congestion/pacing/FEC incomplete | Continue native datagram path; keep WebRTC separate |
| **M4** | Docs/marketing mismatch | Align README with production-readiness gate |
| **M5** | God binaries | Extract session runtime modules; cap CC |
| **M6** | No mobile bindings | Defer UniFFI until WebRTC client proven |

---

## 7. Architecture scores (0–5)

| Dimension | Score | Note |
|-----------|------:|------|
| Control-plane design | **4.0** | Pair/session/negotiate model clean |
| Media-plane maturity | **2.5** | QUIC works; full low-latency stack incomplete |
| Self-host deployability | **3.5** | Units + coturn + docs exist |
| Managed multi-tenant readiness | **1.0** | Selling point not in architecture |
| Security (LAN self-host) | **2.5** | Pairing helps; spoofable on open net |
| Security (public managed) | **0.5** | Unsafe without account + device PKI |
| Account / linking UX | **1.5** | Device pair only |
| Cross-platform core | **3.5** | Good crate split; OS depth uneven |
| Product honesty vs docs | **2.0** | README ahead of readiness |

---

## 8. Recommended 90-day architecture track

| Phase | Deliverable |
|-------|-------------|
| **Weeks 1–3** | Device keypairs + signed Hello + bound session tokens + TURN fix (**P0 trust rewrite**) |
| **Weeks 4–6** | Managed account service (OIDC, enroll, revoke, audit); tenant-scoped signaling store |
| **Weeks 7–9** | Managed TURN fleet + rate limits; self-host stays pair-only mode flag |
| **Weeks 10–12** | Opaque QUIC relay SKU design; extract host/client session libs; docs honesty pass |

---

## Bottom line

- **Runtime:** classic remote-desktop split (signaling + P2P/TURN media); crate boundaries are right.
- **Self-host:** architecturally closest to shippable.
- **Managed:** deployment of self-host, not multi-tenant product — **largest strategic gap**.
- **Security:** pairing + short-lived tokens are a start; **spoofable peer IDs + unauthenticated WS** block public launch.
- **Linking:** device-to-device only; redesign = account → device cert → signed grant → bound session.
- **Rust:** monorepo is correct; “bindings” = Tauri/platform only; add UniFFI later; keep media out of GUI.

**Follow-up:** managed-control-plane ADR — schemas (`DeviceCert`, `PairPolicy`, `SessionToken`), API surface, migration from current pair JSON, self-host compatibility mode.
