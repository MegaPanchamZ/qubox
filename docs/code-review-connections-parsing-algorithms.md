# Code Review: Connections, Parsing, Algorithms

**Date:** 2026-07-11  
**Scope:** Signaling WS, native QUIC bootstrap, TURN helpers, length-prefixed JSON framing, datagram media path, FEC, jitter buffer, GCC rate control.  
**Companion:** [critical-architectural-review.md](./critical-architectural-review.md)

---

## Executive summary

The dual-path design (reliable uni-stream JSON AUs vs binary datagram + jitter buffer) is directionally right, but several **contract bugs** make TURN unusable today, and several **parser / algorithm loose ends** will corrupt media or DoS the process under adversarial or real-world loss.

| Area | Health | Top issue |
|------|--------|-----------|
| Signaling WS | OK skeleton | No size/rate limits; spoofable Hello |
| QUIC stream media | Works loopback | JSON-per-frame; unbounded `byte_len` |
| QUIC handshake | Fragile | Fixed stream accept order; single accept |
| TURN HTTP | **Broken** | Client/server path + field name mismatch |
| Datagram media | Promising | Trailing-zero trim; header size docs; demux |
| FEC | Split brain | XOR wired; RS implemented but not used |
| GCC / OWD | Partial | Clock-skew OWD; gradient ignores EWMA |

---

## 1. Connection handling

### 1.1 Signaling WebSocket (`qubaix-signaling`)

**Flow:** upgrade → first message must be `Hello` → register peer → message loop → on close unregister + drop sessions + presence.

| Finding | Severity | Detail |
|---------|----------|--------|
| Unauthenticated Hello | Critical (security) | Any client can claim any `peer_id` |
| No max message size / rate limit | High | Large JSON or flood → CPU/memory |
| Invalid JSON continues loop | Low (good) | Error message, does not drop peer |
| Binary frames rejected, peer stays | Low | OK |
| Disconnect removes all peer sessions | Medium | Correct cleanup; no graceful “session end” to peer |
| Presence broadcast O(n) clone per peer | Low | Fine at small N; not multi-tenant scale |
| `register` rejects duplicate peer_id | OK | But attacker can hold a peer_id forever |

**Loose end:** pending pairings are never TTL-pruned if host never decides.

### 1.2 Native QUIC bootstrap (`qubaix-transport`)

**Host:** `bind` → self-signed cert → `accept()` once → bi auth stream → token/session checks → media/audio/control streams.

**Client:** trust ticket cert → connect → bi auth → **accept streams in fixed order**: control bi → audio uni → media uni → control uni.

| Finding | Severity | Detail |
|---------|----------|--------|
| Single `accept_authenticated_connection(self)` | Medium | One client per host bind; second connection not explicitly rejected at endpoint level |
| Fixed accept order | **High** | Host must open streams in exact order client expects; race/reorder → 10–30s timeout, opaque failure |
| Auth stream `read_to_end(1024)` after auth | Low | Limits junk; OK |
| Client drops auth recv after ACK | OK | |
| Ticket cert pin (trust store = one cert) | OK for session | No device binding |
| Keepalive 5s, 8 bi/uni streams | OK | May be tight for multi-display + clip/mic + control |
| `connect_with_fallback` 3s direct then TURN | Medium | Direct timeout aborts in-flight connect; no parallel ICE-like racing |

### 1.3 TURN path — **contract bugs (critical)**

Server routes (`SignalingState::router`):

- `POST /v1/turn/relay-address` body: `{ peer_id, relay_address }`
- `GET /v1/turn/relay-address/{peer_id}` response: `{ peer_id, relay_address }`

Client helpers (`fetch_*` / `publish_*` in `qubaix-transport`):

| Client call | Actual request | Server expects | Result |
|-------------|----------------|----------------|--------|
| `fetch_host_turn_address` | `GET .../relay-address?peer_id=` | path param `/{peer_id}` | **404 / wrong route** |
| `fetch_host_turn_address` body key | `turn_address` | `relay_address` | **parse fail even if hit** |
| `publish_own_turn_address` | `turn_address`, `lifetime_secs` | `relay_address` only | **400 deserialize** |
| `publish_own_turn_address` value | publishes `config.turn_server` | should be allocated relayed addr | **wrong semantics** |

**∴ TURN fallback is non-functional against current signaling server.** Loopback direct QUIC still works.

Also: each TURN HTTP helper does `reqwest::Client::new()` (no pool) and Bearer is peer UUID (see arch review).

### 1.4 Stream lifecycle loose ends

- Host `open_input_receiver` finishes the bi send side after `VideoConfig` — client reuses same bi for input send. OK if only one direction used after setup.
- Host control uni must be opened before client finishes handshake accept order — host session code must not open media before audio.
- No application-level session teardown message on either path; relies on QUIC close / process exit.
- Datagram + reliable media can both run; no negotiation flag that client should prefer one.

---

## 2. Data parsing / framing

### 2.1 Length-prefixed JSON (reliable streams)

```
[u32 BE length][json bytes][optional raw payload for media/audio]
```

Used for: auth, video config, input, control, **every access unit header**, audio chunks.

| Finding | Severity | Detail |
|---------|----------|--------|
| **No max length** | **Critical (DoS)** | `vec![0; len]` / `vec![0; header.byte_len]` — malicious peer sets length/byte_len to huge value → OOM |
| `flush()` after every write | Medium (perf) | Extra syscall / delayed batching; bad for input at 100+ Hz |
| JSON for video headers | High (perf) | Hot path; datagram path already uses 14-byte binary header |
| `byte_len: usize` in JSON | Medium | Cross-arch ambiguity; prefer `u32` with cap |
| `session_id` re-checked per packet | OK | Defense in depth |
| H.265/AV1 NAL inspect skipped | Low | Pass-through; OK if decoder handles |
| Input event `clone()` into JSON enum | Low | Alloc per event |

**Fix sketch:**

```text
const MAX_JSON_FRAME: u32 = 256 * 1024;
const MAX_MEDIA_AU: u32 = 16 * 1024 * 1024;
// reject len > MAX before allocate
// media: fixed binary header (like datagram) instead of JSON
```

### 2.2 Datagram media header

```text
WIRE_HEADER_SIZE = 14
magic[2] flags codec stream_id[2] frame_id[4] chunk_id[2] chunk_count[2]
```

| Finding | Severity | Detail |
|---------|----------|--------|
| `MediaDatagramHeader::SIZE = 12` but wire is 14 | Medium | Doc/API lie; `write_into` writes 14 bytes into “12” mental model |
| `#[repr(C, packed)]` unused for IO | Low | Manual BE encode is correct; packed is red herring |
| No `original_len` on wire | **High** | Receiver cannot know true frame size after pad |
| Magic shared with gamepad/pen | Medium | `MediaDatagramReceiver` treats all magic-matched packets as media chunks — **no discriminator demux** |
| Silent drop on short/bad magic | OK for UDP | No metrics |

### 2.3 Frame reassembly (`assemble_frame`)

```rust
// Trim trailing zero-padding from the last chunk.
while bytes.last().copied() == Some(0) && bytes.len() > 1 {
    bytes.pop();
}
```

| Finding | Severity | Detail |
|---------|----------|--------|
| **Trims all trailing zeros** | **Critical (correctness)** | Valid Annex-B / CABAC tails can end in `0x00`; **corrupts frames** |
| Pad was full-chunk zero pad | — | Need explicit `original_len` or pad length field |

### 2.4 Gamepad / pen parsers

- Gamepad: magic + `0x47` + fixed fields — OK; wrong magic returns `Short` not `BadMagic` (misleading error).
- Pen: magic + discriminator + wire block — OK.
- **Loose end:** no unified demux on host/client datagram loop for media vs gamepad vs pen.

### 2.5 Signaling JSON

- Full `ClientMessage` / `ServerMessage` serde on every WS text frame — fine for control plane.
- No schema version field on wire messages — migration will be hard.

---

## 3. Algorithms

### 3.1 Transport / codec negotiation

```text
transport: prefer requested ∩ caps, else NativeQuic > WebRtc > RelayQuic
codec: prefer if either side has enc OR dec; else Av1 > H265 > H264 host-enc ∩ client-dec
```

| Finding | Severity | Detail |
|---------|----------|--------|
| Preferred codec allows `encoders.contains` on client or `decoders` on host | Medium | Can select codec host cannot encode |
| RelayQuic listed but incomplete | Medium | Negotiate success then fail at media |

### 3.2 XOR FEC (`FrameChunker` + `recover_with_parity`)

- Chunk ≤ 1200 B, pad last to max, XOR parity per block.
- Recovery: only if **exactly one** missing per block; returns after **first** recovered block index (does not walk all blocks in one call).

| Finding | Severity | Detail |
|---------|----------|--------|
| `try_fec_recovery` counts “frames” when any block recovers | Low | Stats overcount |
| Multi-block frames need multiple recover calls | Medium | Incomplete recovery in one pass |
| XOR only recovers 1 loss/block | By design | Burst loss → fail |

### 3.3 Reed–Solomon FEC (`rs_fec.rs`)

- Solid encode/reconstruct tests; adaptive `FecController` with step-up hysteresis.
- **`MediaDatagramSender::send_frame` still uses XOR `FrameChunker` only** — RS is dead code on the live path.
- Wire claims “compatible via FLAG_PARITY” but multi-parity RS needs receiver to know `(block_size, parity_shards)` — **not on wire**, only session-negotiated (negotiation not implemented).

### 3.4 Jitter buffer

- BTreeMap by `frame_id`, deadline = first_arrival + target_delay.
- Adaptive target delay 3–15 ms from jitter EWMA.
- max_inflight drops oldest; stats mark as `frames_dropped_deadline` even when dropped for capacity (mislabel).

| Finding | Severity | Detail |
|---------|----------|--------|
| Jitter EWMA assumes 16.67 ms inter-chunk | Medium | Wrong for multi-chunk frames and non-60fps |
| No frame_id gap detection / NACK generation wired to ControlMsg | Medium | `DeadlineFrame` computed; host NACK path unclear |
| `pop_ready` does not run FEC first | Medium | Callers must `try_fec_recovery` manually — easy to forget |
| chunk_count fixed at first packet | OK | Late larger chunk_count ignored |

### 3.5 OWD tracker + GCC (`OwDelayTracker`, `GccRateController`)

| Finding | Severity | Detail |
|---------|----------|--------|
| OWD = recv_ts − send_ts wall clocks | **High** | Unsynchronized clocks → garbage OWD / permanent panic or underuse |
| GCC `classify` uses raw consecutive OWD delta | Medium | `owd_ewma_ms` updated but **unused** for gradient |
| Loss unit comments inconsistent | Low | “parts per thousand” vs “1000 = 100%” in module docs — code uses `/1000.0` as fraction (so 200 = 20%) |
| Panic freeze 1s + min reaction 250ms | OK | |
| Bitrate change may force FFmpeg respawn | Arch | Not a bug, but expensive; HW bitrate API not fully wired |

### 3.6 Fallback / connection selection

- Serial: direct 3s → TURN 5s → fail.
- No simultaneous candidates; no RTT-based selection after both work.
- TURN/TCP stubbed.

---

## 4. Ranked fix list (implementation)

### P0 — correctness / DoS

1. **Align TURN HTTP contract** (one source of truth):
   - Path: `GET /v1/turn/relay-address/{peer_id}`
   - JSON field: `relay_address` (or rename both sides)
   - Publish body must match `PublishRelayRequest`
   - Publish the **allocation’s relayed address**, not the TURN server URL
2. **Cap all length-prefixed reads** (`MAX_JSON`, `MAX_AU`, `MAX_AUDIO_CHUNK`); reject before alloc.
3. **Stop trimming trailing zeros** in `assemble_frame`; add `original_len` (or pad_len) to datagram header / last-chunk flag payload.
4. **Demux datagrams** by byte after magic (media flags vs `0x47` gamepad vs pen discriminator) before jitter buffer.

### P1 — connection robustness

5. Explicit stream mux: type byte / stream purpose map instead of accept-order coupling.
6. Host: reject extra QUIC connections after session bound; idle timeout on auth.
7. Parallel or overlapping direct+TURN connect; fix publish-before-fetch race (host must publish before client fetch).
8. Preferred codec: require `host.encoders ∩ client.decoders`.

### P2 — performance / algorithms

9. Binary headers for reliable media path (reuse datagram layout).
10. Batch writes; flush only on control/input boundaries or timer.
11. Wire RS FEC into sender/receiver with negotiated `(block_size, parity)` or header fields.
12. OWD: use relative delay (arrival delta vs send delta) or NTP/sync; feed EWMA into GCC gradient.
13. Shared `reqwest::Client` for TURN helpers.
14. Prune pending pairings; rate-limit Hello/pair/session.

---

## 5. Suggested test gaps

| Test | Why |
|------|-----|
| Client TURN helpers against live `SignalingState::router` | Would have caught field/path mismatch |
| Malicious `byte_len` / JSON len | OOM regression |
| Annex-B frame ending in `0x00` through jitter assemble | Trailing-zero bug |
| Host opens streams out of order | Handshake hang |
| Two losses in one XOR block | Expect unrecovered + deadline |
| RS encode path if enabled in sender | Currently unintegrated |
| Clock skew ±500 ms on OWD → GCC | Panic/stability |

---

## 6. File map

| Topic | Primary files |
|-------|----------------|
| WS + pairing + sessions | `crates/qubaix-signaling/src/lib.rs` |
| TURN REST issue | `apps/qubaix-signaling-server/src/turn.rs` |
| QUIC connect/auth/media streams | `crates/qubaix-transport/src/lib.rs` |
| Datagram / FEC / jitter / control | `crates/qubaix-transport/src/media/mod.rs` |
| RS FEC (unused live) | `crates/qubaix-transport/src/media/rs_fec.rs` |
| GCC ABR | `apps/qubaix-host-agent/src/rate_control.rs` |

---

## Bottom line

- **Direct QUIC + length-prefixed JSON media works for lab/loopback**, with costly framing and weak bounds.
- **TURN client/server API is mismatched — fix before any NAT demo.**
- **Datagram path is the right long-term media plane**, but reassembly padding and demux must be fixed before trusting quality.
- **RS FEC and GCC are half-integrated:** algorithms exist; wire + clock-safe metrics + sender hooks lag.

Next implementation slice if desired: (1) TURN contract fix + e2e test, (2) length caps, (3) `original_len` on datagram wire, (4) stream-type mux for handshake.
