# ADR-022 Context-Aware, Process-Locked File Synchronization

**Status:** Accepted  
**Date:** 2026-07-15  
**Authors:** Qubox  
**Depends on:** ADR-005 (daemon), ADR-006 (delegation), ADR-008 (clipboard/mic), ADR-021 (dual-mode), transport `StreamPurpose` mux  
**Supersedes:** none (new capability)

## 1. Context and problem

Qubox is a native-QUIC remote workstation (video/audio/input/clipboard/mic/pen).
Users also need **safe transfer of project files** between machines they already
pair (emulator saves, DAW sessions, 3D assets, work trees that are *not*
active Git indexes).

Blind FS sync (Dropbox/Syncthing-style always-on upload) corrupts files when an
app is mid-write. Commercial tools solve pieces of this (Resilio: real-time
watch + binary delta + congestion yield). Academic work (LearnedSync) proposes
ML downtime prediction; **that is not a product requirement for Qubox v1**.

We already have:

| Piece | Reality in tree |
|-------|-----------------|
| Transport mux | `StreamPurpose` 0x01–0x05 (`VideoConfig`, `Audio`, `Media`, `HostControl`, `Input`) + `StreamEnvelope` magic `0xA1` in `crates/qubox-transport` |
| Control plane | WebSocket signaling; pair grants; short-lived `SessionCredential` |
| Identity | Ed25519 `DeviceIdentity` / `SignedHello`; ChaCha20-Poly1305 only for **private key at rest** |
| Daemon | Per-user agent; Unix socket / Named Pipe IPC; **redb** state (not sled/SQLite) |
| Clipboard | `ControlMsg::ClipboardChanged` on control streams (ADR-008) — small payloads only |
| Dual-mode | Self-host signaling stays thin; managed cloud **must not decrypt media** (ADR-021) |

`docs/architecture.md` Input plane already lists “Clipboard and optional file
handoff” as future work. This ADR makes that concrete.

### Non-goals (v1)

- Sync of live `.git/` trees (use Git; file sync on `.git` is unsafe).
- ML/idle prediction models (LearnedSync-style).
- Kernel drivers / minifilters / FS filters.
- Store-and-forward of file blobs on the signaling server.
- Full rsync/block-level delta engine for multi-GB game builds (phase 2).
- Multi-writer CRDT merge of opaque binaries.

## 2. Decision summary

Implement a **Context-Aware File Sync (CAFS)** feature as:

1. **Process-locked** — do not transfer a watched path while a configured
   process owns it (emulator, DAW, editor).
2. **Event-driven** — FS notifications + process exit trigger queueing; not cron.
3. **P2P only over established Qubox sessions** — same trust domain as remote
   desktop (paired peers + session credential). No new cloud blob store.
4. **Local outbox** — if peer offline, queue in daemon **redb**; flush when a
   session (or dedicated sync connection) is available.
5. **Fork-on-conflict** for opaque binaries — vector clocks; never silent merge.

## 3. Proof-check of Gemini ideation (vs this repo)

| Gemini claim | Verdict |
|--------------|---------|
| Add “7th substream” IDs 0x00–0x03 | **Wrong.** Existing purposes are 0x01–0x05. Add **`StreamPurpose::FileSync = 0x06`** (and optionally 0x07 control for sync handshake). |
| Encrypt files with “Ed25519 shared keys” on signaling | **Wrong.** Ed25519 is sign/verify; no ECDH file KEK today. Session is QUIC-TLS; app-layer file encrypt only if we later add sealed offline blobs (out of scope). |
| Upgrade signaling to encrypted drop-box | **Reject.** Violates ADR-021 thin control plane + cost/abuse surface. User already chose local queue. |
| Daemon uses sled/SQLite | **Wrong.** Use **redb** tables (same as pairings/settings). |
| Host-agent alone owns all watching | **Partial.** Sensors can live in host-agent **or** daemon; **state + outbox + policy** stay in **daemon** (ADR-005 ownership). |
| sysinfo + notify + debouncer | **Accept** as crate choices; none present in workspace deps yet. |
| User-session agent, no drivers | **Accept** — matches ADR-005 per-user daemon. |
| Vector clocks + quarantine | **Accept** for binary conflict policy. |
| ML downtime scheduler | **Reject v1**; process-lock + congestion already cover integrity + “don’t stomp interactive session”. |
| Syncthing/Resilio for GBA | External products exist; Qubox value is **paired-device, session-aware, process-locked** transfer inside the remote-workstation product. |

## 4. Architecture

### 4.1 Placement

```
┌─────────────────────────────────────────────────────────────┐
│ qubox-daemon (per-user)                                     │
│  • redb: tracked_files, outbox, vector_clocks, sync_rules   │
│  • policy: which paths, which process names, which peers    │
│  • outbox drain when peer online                            │
│  • IPC: ListSyncJobs / ResolveConflict / AddWatchRule       │
└───────────────┬─────────────────────────────▲───────────────┘
                │ IPC                         │ IPC events
┌───────────────▼──────────────┐   ┌──────────┴────────────────┐
│ qubox-host-agent (sensor)    │   │ client-cli / GUI          │
│  • notify-debouncer watches  │   │  • conflict UI            │
│  • process list (sysinfo)    │   │  • enable/disable rules   │
│  • sets lock / queues via IPC│   └───────────────────────────┘
└───────────────┬──────────────┘
                │ when session active: FileSync streams on native QUIC
                ▼
         peer (same stack)
```

**Why not only host-agent?** Daemon already owns identity, pairing, persistence,
and survives GUI close (ADR-005). Sync state must survive agent restarts.

**Why not only daemon for FS watch?** Daemon may run as user service without
full interactive session on some platforms; host-agent already sits in the
session where games/DAWs run. Prefer: host-agent = sensors; daemon = brain.
If host-agent is down, daemon still holds queue and can drain on next session.

### 4.2 Transport

Extend `StreamPurpose`:

```text
0x01 VideoConfig
0x02 Audio
0x03 Media
0x04 HostControl
0x05 Input
0x06 FileSync        // NEW: bi handshake + uni bulk (or bi for both)
```

Rules:

- Open FileSync **only after** post-auth session is established (same
  `SessionCredential` path as media).
- Prefer **one bi stream for manifest handshake**, then **one uni per file**
  (mirrors Gemini transfer pattern; fits `AcceptedStream::{Bi,Uni}`).
- Priority: below Input/VideoConfig/HostControl; treat as bulk. Do not steal
  interactive bandwidth — reuse existing congestion / rate-control observation
  (`qubox-transport` congestion module) to pause outbox when media is saturated.
- Framing: length-prefixed bincode or JSON for handshake (JSON matches
  control-plane style; bincode matches daemon IPC). **Recommendation:**
  handshake messages as `serde` JSON on FileSync bi (consistent with
  `ControlMsg`); bulk body raw bytes after small binary header
  (`file_id` len, size, blake3).

Envelope remains `[STREAM_MAGIC=0xA1, purpose]`.

### 4.3 Presence and “offline”

Signaling already provides presence/pairing. CAFS does **not** require both
nodes in a full remote-desktop media session forever:

**v1 (simpler):** Drain outbox only while a **native QUIC session** exists
between the two paired peers (user opened remote desktop or a future
“sync-only” session type).

**v1.1 (optional):** Lightweight **sync-only session plan** via signaling
(no video): same auth, open QUIC, FileSync only. Reduces “must stream desktop
to sync a .sav”.

Until 1.1 ships, product copy: “Files sync when both devices are online and
connected (or in a remote session).”

### 4.4 Local state (redb)

Add tables (schema_version bump in `apps/qubox-daemon/src/state.rs`):

| Table | Value (bincode/JSON) |
|-------|----------------------|
| `sync_rules` | `rule_id → { paths[], process_names[], peer_ids[], enabled }` |
| `tracked_files` | `file_id → { local_path, vector_clock, content_hash, sync_state }` |
| `sync_outbox` | `job_id → { file_id, target_peer, status, retry_count, queued_at }` |
| `sync_conflicts` | `conflict_id → { file_id, local_path, remote_path, peer_id, clocks }` |

`sync_state`: `Synced | LockedByProcess | Pending | Conflict | Disabled`.

Vector clock: `HashMap<peer_id_string, u64>` serialized JSON (same pragmatism
as Gemini; redb is KV — no need for SQLite joins).

### 4.5 Process lock

- Config: list of process name matchers (e.g. `mgba`, `mgba.exe`, `Ableton Live`).
- Sensor polls via `sysinfo` on a low cadence (1–2 s) **or** uses platform
  process-create/exit notifications where available without drivers.
- On match for a rule’s path set: set `LockedByProcess` for those `file_id`s;
  **do not** open FileSync transfers for them; **do not** apply inbound
  overwrites for them (quarantine inbound until unlock + user/policy).
- On process exit: debounce FS events (2 s via `notify-debouncer-mini`),
  rehash, bump local vector component, enqueue outbox.

No launch interception / kernel hooks in v1. Soft warning only (notification)
if another peer holds a **lease** (see §5).

### 4.6 Watcher

- Crate: `notify` + `notify-debouncer-mini` (collapse Create/Modify storms).
- Bridge sync watcher thread → tokio `mpsc` (same pattern as Gemini).
- Ignore temp patterns: `*.tmp`, `*.part`, `~*`, emulator `*.sa~` if needed.
- Optional `.quboxignore` / rule-level globs (ROM local, saves sync).

### 4.7 Transfer pipeline

1. Handshake bi: `ManifestExchange { peer_id, versions: Map<file_id, VectorClock + hash> }`.
2. Each side computes `PullRequest` / push set via clock compare.
3. Sender: `open_uni` + header + `tokio::io::copy` from disk (no full-file RAM).
4. Receiver: write `*.qubox-partial`, `sync_all`, verify blake3, atomic rename.
5. On success: ack on bi; sender deletes outbox job; both advance clocks to
   dominate union.

Hash: **blake3** (already workspace dep for clipboard).

### 4.8 Conflict resolution

Opaque binaries → **never merge**.

1. **Prevention (best effort):** on lock, publish short-lived **LockLease**
   metadata over signaling or next session hello (`peer_id`, `file_id`,
   `expires_at`). Peer GUI/daemon shows warning. Not a hard distributed lock.
2. **Detection:** vector clocks; concurrent increments → Conflict.
3. **Resolution:** write remote to
   `{stem}.conflict.{peer}.{utc}.{ext}`; mark `Conflict`; stop auto-sync until
   IPC `ResolveConflict { KeepLocal | KeepRemote | KeepBoth }`.

Do **not** use mtime as sole authority (clock skew).

## 5. Wire / IPC surface (sketch)

### 5.1 FileSync handshake messages (JSON, FileSync bi)

```text
ManifestExchange { peer_id, files: [{ file_id, clock, hash, size }] }
PullRequest { file_ids: [] }
PushOffer { file_ids: [] }          // optional symmetry
TransferComplete { file_id, hash }
ConflictNotice { file_id, clock_a, clock_b }
LockLease { file_id, holder_peer, expires_unix }
```

### 5.2 Daemon IPC (bincode, additive)

New methods (names illustrative):

- `SyncAddRule` / `SyncRemoveRule` / `SyncListRules`
- `SyncListJobs` / `SyncListConflicts` / `SyncResolveConflict`
- `SyncSetEnabled { rule_id, enabled }`

Events: `SyncJobUpdated`, `SyncConflict`, `SyncLockChanged`.

### 5.3 Crate split

| Crate | Role |
|-------|------|
| `crates/qubox-sync` (new) | clocks, hash, ignore, outbox types, pure logic |
| `qubox-daemon` | redb + IPC + drain orchestration |
| `qubox-host-agent` | notify + sysinfo sensors → IPC |
| `qubox-transport` | `StreamPurpose::FileSync` + open/accept helpers |
| `qubox-proto` | optional shared message types if reused on control |
| GUI | conflict panel |

## 6. Security and product mode

- Only **paired** peers; same session auth as media.
- Path allowlist only (user-configured roots). Never walk entire home by default.
- Symlink policy: refuse or resolve-within-root only.
- Size caps per file / per day (config) to avoid accidental 100GB queue.
- Self-host: zero server storage change.
- Managed: still no plaintext/cipher blob store on control plane.
- Logging: paths + hashes OK; never log file contents.

## 7. Phased delivery

### Phase A — Skeleton (MVP integrity)

1. `StreamPurpose::FileSync = 0x06` + accept path in transport.
2. `qubox-sync` crate: vector clock compare, blake3, atomic write helpers.
3. redb tables + IPC list/add rule stubs.
4. Manual “push this path now” over active session (no watcher yet).

**Exit:** two machines, active session, push a 128KB `.sav` safely.

### Phase B — Context lock

1. host-agent `sysinfo` process lock + `notify-debouncer-mini`.
2. Outbox + drain on session up.
3. Conflict quarantine + GUI list + resolve IPC.

**Exit:** play emulator on A while B online → no mid-save overwrite; close game → sync.

### Phase C — Product polish

1. Sync-only session type (no video).
2. Bandwidth pause when media congestion high.
3. Optional block-level delta (e.g. fastcdc) for large assets — **separate ADR**.
4. Shared folder presets (saves-only ignore ROMs).

### Explicitly deferred

- LearnedSync ML scheduler.
- Server-side store-and-forward.
- Git-aware sync.
- Kernel FS filters.

## 8. Consequences

### Positive

- Real differentiator vs pure remote desktop: **safe** hybrid offline file
  continuity for paired devices.
- Aligns with existing per-user daemon, redb, QUIC mux, dual-mode constraints.
- Avoids cloud storage cost and E2EE policy breakage.

### Negative / risks

- “Both must be online” UX weaker than Dropbox until Phase C sync-only session.
- Process-name locking is heuristic (renamed binaries, flatpaks).
- Large trees need ignore rules or users will queue disasters.
- Conflict UX is mandatory for trust (binary forks confuse non-technical users).

### Mitigations

- Presets for emulator saves; default max file size.
- Clear GUI copy for conflicts and locks.
- Congestion-aware pause so FileSync never feels like it “kills” the session.

## 9. Alternatives considered

| Option | Why not |
|--------|---------|
| Shell out to Syncthing | Second identity/pairing model; no process lock integration; support burden |
| Signaling blob store | ADR-021 + cost + abuse |
| Always-on P2P like Resilio | Overlaps product but rebuilds a company; we need session-aware subset |
| Git auto-commit wrapper | Wrong tool for ROMs/saves; already rejected for repos |
| SQLite instead of redb | Splits daemon persistence story; redb already proven in-tree |

## 10. Open questions

1. **Sync-only session in Phase A or C?** Recommendation: C; keep A tied to
   existing native QUIC session to reuse auth path.
2. **Who opens FileSync streams — host or client?** Recommendation: either;
   initiator = outbox owner; responder accepts uni.
3. **Multi-peer fanout** (A→B and A→C)? v1 single target peer per rule;
   multi later.
4. **Windows service session 0** vs user agent: keep CAFS sensors in
   **interactive host-agent**; daemon in user session (ADR-005) — do not put
   watchers in a session-0-only service.

## 11. Implementation checklist (for implementers)

- [x] `StreamPurpose::FileSync` + tests in `stream_purpose_from_byte_round_trip`
- [x] `crates/qubox-sync` + workspace member
- [x] redb migration `schema_version` + tables (`sync_rules`, `tracked_files`, `sync_outbox`, `sync_conflicts`; schema v2)
- [x] IPC methods (`SyncAddRule` / `SyncRemoveRule` / `SyncListRules` / `SyncSetEnabled` / `SyncListJobs` / `SyncListConflicts` / `SyncResolveConflict` / `SyncPushNow` / sensor hooks)
- [x] host-agent sensor task gated by feature `file-sync` (`sysinfo` + `notify-debouncer-mini`)
- [x] Transport helpers: `open_filesync_handshake` / `open_filesync_bulk` + `filesync::{send,recv}_file_bulk`
- [ ] GUI conflict surface (list + resolve)
- [ ] Docs: user guide “never sync `.git`”; emulator version matching note
- [x] Unit tests: clocks, ignore, atomic apply, plan_transfers, header roundtrip, state/outbox
- [x] Global never-track list (default `.git`) + CLI/GUI presets
- [x] GUI File Sync panel (conflicts, ignores, rules, push)
- [x] CLI: `qubox-daemon sync …` and `qubox-client-cli sync …`
- [x] Congestion pause gate (`FileSyncCongestionGate`) + bulk path tests
- [x] Phase C wire: `sync_only` on StartSessionRequest / SessionPlan / SessionRequested
- [x] User guide: `docs/user-guide-file-sync.md`
- [ ] Full live multi-machine QUIC drain of outbox under media load (soak)

## 12. References (in-repo)

- `crates/qubox-transport/src/lib.rs` — `StreamPurpose`, `StreamEnvelope`
- `apps/qubox-daemon/src/state.rs` — redb tables
- `research/decisions/ADR-005-daemon-and-turn-architecture.md` — per-user daemon
- `research/decisions/ADR-008-clipboard-mic-sync.md` — control-stream small data
- `research/decisions/ADR-021-dual-mode-control-plane.md` — no cloud media decrypt
- `docs/architecture.md` — input plane file handoff
