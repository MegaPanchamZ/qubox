# P1-11: TURN (Traversal Using Relays around NAT)

Status: research complete, implementation pending.
Owner: `apps/signaling-server` (TURN credential issuance), `apps/client-cli` (TURN client), `apps/host-agent` (TURN client).
Depends on: the existing QUIC transport, the signaling server.
Blockers: TURN server choice — self-hosted (coturn) or hosted (Cloudflare, Twilio, Xirsys) — is a deployment decision.

## Goal

Add TURN relay support so the host and client can connect when both are behind NATs / firewalls and direct QUIC fails. The signaling server issues short-term TURN credentials; both endpoints connect to the TURN server and run QUIC through the relay. Latency target: <100 ms added by the relay hop.

## Research Summary

### TURN protocol (RFC 8656 / 6062 / 5766)

TURN (Traversal Using Relays around NAT) is the standard for relaying application data through a middle box when direct peer-to-peer fails. Operations:

- **Allocate**: client asks the TURN server for a relayed IP+port; server returns a unique address.
- **CreatePermission**: client authorizes a peer IP to send through the allocation.
- **ChannelBind**: client binds a peer to a 4-byte channel number; subsequent traffic uses compact ChannelData frames (4-byte channel + 2-byte length + payload) instead of full STUN messages (36-byte header + payload). **The 16-byte savings per packet matters at 60+ fps.**
- **Refresh**: renews the allocation lifetime; `lifetime=0` deletes it.
- **Send/Data Indication**: fallback message format for relayed data when ChannelBind isn't used.

Transports:
- **TURN/UDP** (port 3478): lowest latency, but blocked by some firewalls.
- **TURN/TCP** (port 3478 or 443): firewall-friendly, higher latency.
- **TURN/TLS** (port 5349 or 443): encrypted, firewall-friendly, similar to TURN/TCP for latency.
- **TURN over WebSockets**: for HTTP-only networks.

TURN adds 30-80 ms latency (extra hop), but it's the only way to traverse symmetric NATs. The relay server's geographic location matters; pick a TURN server near both endpoints.

### Authentication

**Long-term credentials**: fixed username + password, hashed with HMAC-SHA1 per RFC 5389. Simple to set up, but passwords must be distributed to clients.

**Short-term credentials**: time-limited (e.g. 1 hour), derived from a shared secret on the TURN server and a REST API call to the signaling server. The WebRTC pattern. **Recommended** — the signaling server holds the TURN secret, never the client.

The signaling server's flow:
1. Client authenticates with the signaling server (existing pairing flow).
2. Client requests `POST /v1/turn/credentials` → returns `{ urls: ["turn:turn.example.com:3478", "turn:turn.example.com:443?transport=tcp"], username: "...", password: "...", ttl: 3600 }`.
3. The signaling server computes the username (`<expiry>:<hmac>`) and password (`base64(hmac)`) using the TURN shared secret.
4. Both client and host use these credentials.

### Rust TURN server options (2024-2026)

| Project | Lang | Status | Notes |
|---------|------|--------|-------|
| **coturn** | C | Production default | The de-facto reference TURN server. Widely deployed. Broad feature set. Strong interop. Not Rust, but stable. |
| **turn-rs** | Rust | Promising | Pure Rust, simpler deployment, fewer features than coturn. Good for simple relay. |
| **webrtc-rs/turn** | Rust | Library | Useful if you're already in the webrtc-rs stack. Not a full TURN server. |
| **stun-rs / turn-rs** | Rust | Experimental | Various experimental crates; check maintenance status. |

**Decision**: self-host **coturn** for the first release. turn-rs is a viable alternative if we want pure Rust, but coturn is battle-tested and well-understood. Use `turn-rs` as a future option if we want a single-binary deployment.

coturn config for game streaming:

```bash
listening-port=3478
tls-listening-port=5349
realm=bp
static-auth-secret=<shared-secret>
user-quota=12
total-quota=1200
no-tlsv1
no-tlsv1_1
no-cli
```

Deploy on a VPS (Hetzner, OVH, Vultr) with 1 Gbps port, 5+ TB bandwidth.

### Hosted TURN services (2024-2026)

| Provider | Pricing | Notes |
|----------|---------|-------|
| **Cloudflare Calls** | $0.05/GB (cheap) | UDP + TCP + TLS, global edge, easy API. Best for low-cost. |
| **Twilio Network Traversal Service** | $0.40/GB | Mature, well-documented, global. |
| **Metered** | $0.40-0.50/GB | Established. |
| **Xirsys** | $0.50+/GB | Mature, more expensive. |

**Trade-offs**:
- **Hosted**: no ops, fast setup, but cost at scale (1 TB/month = $50-200), data sovereignty concerns, rate limits.
- **Self-hosted**: cheaper at scale (1 TB/month on Hetzner = $20-50), full control, requires ops.

For the first release, support both: the signaling server can be configured with a list of TURN servers (self-hosted + hosted). The client picks based on the user-configured preference or the cheapest available.

### QUIC over TURN

TURN relays packets; it doesn't need to understand QUIC. The pattern:

1. Host and client both connect to the TURN server.
2. Each allocates a relayed address.
3. The host and client send their QUIC packets to each other's relayed address.
4. The TURN server forwards them.

The TURN server treats the QUIC packets as opaque application data (carried in ChannelData frames). The QUIC stack on each endpoint handles encryption, reliability, datagrams, etc. **The TURN server is application-agnostic.**

For QUIC specifically:
- QUIC over TURN/UDP: works, the TURN server doesn't need to be QUIC-aware.
- QUIC over TURN/TCP: works; the TURN server sees the QUIC packets as TCP byte stream. May have TCP-over-TCP issues (retransmit-on-loss compounding).
- **TURN/UDP is preferred** when available; fall back to TURN/TCP only when UDP is blocked.

### Latency budget

| Path | Latency |
|------|---------|
| Direct (no TURN) | 5-50 ms |
| TURN/UDP (relay) | 10-100 ms (5-50 to TURN + 5-50 from TURN) |
| TURN/TCP (relay) | 20-150 ms (TCP overhead) |
| TURN/TLS (relay) | 25-150 ms (TLS + TCP overhead) |

The TURN server's location is critical; pick one close to both endpoints. A TURN server in Europe with one peer in Europe and one in the US adds 50-100 ms vs. a TURN server in the middle (e.g. east coast US).

### 2024-2026 status

- **TURN over QUIC**: IETF work in progress (draft-duke-turn-quic). Some TURN servers (e.g. Cloudflare) support QUIC-aware TURN that understands QUIC packets; the relay can be more efficient.
- **Media over QUIC (MoQ)**: includes a "relay" mode that handles the TURN problem natively. As MoQ matures, TURN may become less necessary.
- **Cloudflare Calls**: widely deployed, low cost, good choice for hosted TURN.
- **Self-hosted coturn**: still the cheapest at scale; mature and stable.
- **turn-rs** is improving; check the project's status before depending on it for production.

### Rust crate matrix (2024-2026)

For the TURN client side (our host + client):
- **stun-rs** or **turn-client-proto** (TURN message encoding).
- **turn-server-sdk** (high-level TURN client; less common).
- **webrtc-turn** (in the webrtc-rs stack).

For the TURN server side (if we self-host turn-rs):
- **turn-rs** (the Rust TURN server crate).
- **coturn** (the C reference; not Rust).

For the integration with quinn:
- We don't need a special TURN-aware quinn. The TURN client just opens a UDP socket to the TURN server, runs the TURN protocol to bind a channel, and quinn runs over the resulting relayed path. (See the `TransportConfig::datagram_send_buffer_size` from P0-2.)

## Implementation Plan

### Step 1: Signaling server: TURN credential issuance

`apps/signaling-server/src/turn.rs` (new):
- `pub struct TurnConfig { urls: Vec<String>, shared_secret: String, default_ttl: u32 }`.
- `pub fn issue_credentials(config: &TurnConfig, peer_id: &str) -> TurnCredentials` — computes the time-limited username and HMAC-derived password.
- New HTTP endpoint: `POST /v1/turn/credentials` returns `{ urls, username, password, ttl }`.

### Step 2: TURN client abstraction

`crates/qubox-transport/src/turn.rs` (new):
- `pub struct TurnClient { server: SocketAddr, credentials: TurnCredentials, allocation: TurnAllocation }`.
- `pub async fn new(server: SocketAddr, credentials: TurnCredentials) -> Result<Self>` — connects via UDP, runs Allocate.
- `pub async fn create_permission(&mut self, peer: SocketAddr) -> Result<()>`.
- `pub async fn channel_bind(&mut self, peer: SocketAddr, channel: u16) -> Result<()>`.
- `pub async fn send_data(&self, peer: SocketAddr, data: &[u8]) -> Result<()>`.
- `pub async fn recv_data(&mut self) -> Result<(SocketAddr, Vec<u8>)>`.

### Step 3: QUIC over TURN

`crates/qubox-transport/src/conn.rs` (existing, extended):
- When the direct QUIC connection fails, the client falls back to TURN:
  1. Open a UDP socket to the TURN server.
  2. Allocate a relay address.
  3. ChannelBind to the host's public address.
  4. Open a QUIC connection from the client's local socket to the host's relayed address.
  5. Quinn runs over the local socket; packets flow: client → TURN → host.
- The host does the same in parallel.
- This requires quinn to use a custom `UdpSocket` (which quinn 0.10+ supports via `Endpoint::client` with a `Runtime` that implements `AsyncUdpSocket`).

### Step 4: Fallback logic

`apps/client-cli/src/connect.rs` (new):
- `pub async fn connect_to_host(host: &HostDescriptor) -> Result<Connection>`:
  1. Try direct QUIC (existing path).
  2. If fail, fetch TURN credentials from signaling.
  3. Try QUIC over TURN/UDP.
  4. If fail (UDP blocked), try QUIC over TURN/TCP.
  5. If fail, give up with a clear error.

### Step 5: Host-side TURN client

The host's `host-agent` runs the same TURN client logic. The host binds to the TURN server, allocates a relay, and waits for the client to connect.

### Step 6: Configuration

Add to `ClientConfig` / `HostConfig`:
- `turn_servers: Vec<TurnServerConfig>` — list of TURN servers to try.
- `turn_credential_ttl: u32` (default 3600 s).
- `turn_force: bool` — if true, always use TURN (for testing or privacy).

CLI flag: `--turn-server turn:turn.example.com:3478 --turn-username X --turn-password Y`.

### Step 7: Tests

- Unit test: TURN credential issuance produces valid HMAC.
- Integration test: spin up coturn in Docker, run the TURN client, allocate, channel bind, send/receive data.
- E2E test: client behind NAT, host behind NAT, both connect to a coturn server, verify the streaming session works.
- Latency test: compare direct vs TURN latency on loopback vs a real TURN server.

## Risks and Open Questions

- **TCP-over-TCP penalty**: when both endpoints are behind restrictive firewalls and TURN/UDP is blocked, the TURN/TCP fallback runs QUIC over TCP-over-TCP. TCP retransmits lost TCP segments; QUIC retransmits lost QUIC packets. The double retransmit is bad for real-time media. Mitigation: prefer TURN/UDP whenever possible; consider TURN/DTLS as an alternative.
- **Bandwidth cost**: TURN relay traffic is ~1.2x the media bandwidth (framing overhead). For 1 TB/month of user traffic, the TURN server sees 1.2 TB. On Hetzner this is $20-50/month; on Cloudflare it's $60.
- **TURN server capacity**: coturn's `user-quota=12` caps the number of concurrent allocations per user. `total-quota=1200` caps the server. Plan for ~100 concurrent sessions per 1 Gbps port at 4 Mbps each.
- **TURN over WebSocket**: HTTP-only networks require TURN-over-WS. coturn supports this; the client must too. Use the `webrtc-rs/turn` crate or a custom implementation.
- **QUIC-aware TURN (IETF draft)**: not yet widely supported. Defer to a follow-up.
- **TURN and 0-RTT QUIC**: TURN's UDP path doesn't break 0-RTT; the QUIC handshake still happens end-to-end. 0-RTT requires the host to have a recent session ticket for the client; we have the existing pairing flow.
- **TURN over IPv6**: most TURN servers support IPv6 only via separate addresses. Document the IPv6 support status.
- **Auth secret rotation**: rotate the TURN shared secret periodically. The signaling server can be configured with multiple secrets (current + previous); clients use whichever the credentials were signed with.
- **Logging**: TURN logs can leak user info. Configure coturn with `verbose-logging=off` in production.
- **Geo-routing**: pick the TURN server closest to both endpoints. The signaling server can ping multiple TURN servers and recommend the closest.

## References

- RFC 8656: TURN (Traversal Using Relays around NAT). https://datatracker.ietf.org/doc/rfc8656/
- RFC 6062: TURN over TCP/TLS. https://datatracker.ietf.org/doc/rfc6062/
- RFC 5766: Original TURN spec. https://datatracker.ietf.org/doc/rfc5766/
- RFC 6156: TURN IPv6 extensions. https://www.rfc-editor.org/info/rfc6156
- WebRTC Hacks: RFC 5766 TURN server. https://webrtchacks.com/rfc5766-turn-server/
- coturn: https://github.com/coturn/coturn
- turn-rs: https://github.com/mycrl/turn-rs
- turn-server-sdk: https://docs.rs/turn-server-sdk
- turn-client-proto: https://lib.rs/crates/turn-client-proto
- webrtc-turn: https://crates.io/crates/webrtc-turn
- EnableSecurity: TURN security best practices: https://www.enablesecurity.com/blog/turn-security-best-practices/
- StackOverflow: TCP support between TURN server and peer per RFC 8656: https://stackoverflow.com/questions/78419511/is-tcp-not-supported-between-turn-server-and-peer-by-turn-server-as-per-rfc-8656
- Perplexity research, 2026-07-02: TURN protocol, Rust TURN servers, hosted services, QUIC over TURN, latency, 2024-2026 status.
