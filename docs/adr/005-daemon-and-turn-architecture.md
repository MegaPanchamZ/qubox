# ADR-005 Daemon and TURN Architecture

## Status

Proposed.

## Context

Qubox currently runs `host-agent` and `client-cli` as foreground processes that directly connect to the signaling server via WebSocket. There is no background service, no persistent state, no auto-update mechanism, and no NAT-traversal fallback when direct QUIC fails.

Two research documents (P1-13 daemon, P1-11 TURN) have been accepted and now require a concrete architecture that the backend-architect can implement against. This ADR covers Phase 1 only: the daemon skeleton, IPC surface, state persistence, TUF auto-update, TURN credential issuance, and QUIC-over-TURN transport.

The following constraints apply:
- The existing `host-agent` and `client-cli` binaries must remain runnable as foreground processes (development, CI, debugging).
- Wire format is JSON for app-level signaling, bincode for length-prefixed IPC.
- Tokio runtime, `tracing` for logs, `serde` + `serde_json` for serializable types.
- `cargo build --workspace` must keep working.
- Every new wire / IPC / redb field must have a default and a migration path.
- No Rust code in this document — only pseudocode type definitions when the on-the-wire layout needs to be unambiguous.

## Decision

### 1. The `qubox-daemon` Process Model

#### 1.1 Binary and crate

A new crate `apps/daemon/` produces the `qubox-daemon` binary. It is a long-running background process that owns:
- The signaling WebSocket connection (reconnect with exponential backoff).
- Pairing state (approve/reject/list/revoke).
- Host-agent lifecycle (start/stop a host session).
- Client-cli lifecycle (start/stop a client session).
- TUF auto-update metadata fetch, verification, staging, and self-replacement.
- IPC server (Unix socket / Named Pipe) for GUI and CLI to communicate.
- State persistence via `redb`.

#### 1.2 Startup sequence

```
qubox-daemon (no subcommand)
  → parse CLI (--help, --version exit early)
  → init tracing (journald on Linux, Event Log on Windows, stderr on macOS)
  → open redb database (create if absent; run schema migrations)
  → load state (pairings, settings, TUF metadata)
  → start IPC listener (Unix socket / Named Pipe)
  → sd_notify READY=1 (Linux) or report SERVICE_RUNNING (Windows)
  → enter main event loop:
      ├── accept IPC connections (dispatch requests)
      ├── maintain signaling WebSocket (reconnect on failure)
      ├── TUF update ticker (every N hours, configurable)
      └── graceful shutdown signal (SIGTERM / service stop)
```

#### 1.3 Backward compatibility

The `host-agent` and `client-cli` binaries retain their current `main()` entry points. When the daemon is not running (development, CI), they operate exactly as today — foreground processes that open their own WebSocket connection.

When the daemon IS running, both binaries gain a `--use-daemon` flag (default: auto-detect). When set or when the IPC socket is present, they delegate signaling, pairing, and session management to the daemon over IPC. The foreground media pipeline (capture → encode → send / receive → decode → render) stays in the foreground process — only control-plane operations move to the daemon.

Concretely:
- `host-agent --use-daemon` skips WebSocket connect; instead connects to daemon IPC, sends `StartHost { config }`, and receives the session ticket. The media pipeline (ffmpeg subprocess, QUIC connection) still runs in the `host-agent` process.
- `client-cli start-session --host X --use-daemon` same pattern.
- Without `--use-daemon`: same behavior as today.

#### 1.4 Per-user vs system-wide

**Decision: Per-user (LaunchAgent on macOS, per-user systemd on Linux, user-managed service on Windows).**

Justification:
- No sudo / admin rights required to install or run.
- Each user controls their own pairings and sessions.
- Privacy: the daemon only has access to the user's files.
- The dev box explicitly forbids sudo.
- On Linux this means a `systemd --user` service (`~/.config/systemd/user/qubox.service`).
- On Windows the installer runs as the user (not LocalSystem) — the SCM supports user-mode services via `SERVICE_WIN32_OWN_PROCESS` running as the logged-in user.
- On macOS a LaunchAgent (not LaunchDaemon).

Trade-off: multiple users on the same machine cannot share a single daemon. Each user runs their own instance. This is acceptable because gaming/streaming sessions are inherently per-user.

#### 1.5 Platform service integration

**Linux: systemd user service**

```
~/.config/systemd/user/qubox.service
```
- `Type=notify` — daemon sends `sd_notify("READY=1")` after init.
- `ExecStart=/usr/bin/qubox-daemon`.
- `Restart=on-failure`, `RestartSec=1`.
- `StandardOutput=journal`, `StandardError=journal`.

Hardening (user service subset — no system-level ProtectSystem):
```
NoNewPrivileges=true
PrivateTmp=true
RestrictAddressFamilies=AF_UNIX AF_INET AF_INET6
SystemCallFilter=@system-service
```

Socket activation (optional, recommended):
```
~/.config/systemd/user/qubox.socket
```
```
[Socket]
ListenStream=%t/qubox.sock
```
When present, the daemon receives the socket fd from systemd and avoids race conditions from the client connecting before the daemon is ready. The daemon uses `sd_listen_fds` (or the `systemd` crate) to accept the pre-opened socket.

On systems without socket activation: the daemon creates `/run/user/$UID/qubox/qubox.sock` (XDG_RUNTIME_DIR).

**Windows: SCM**

Service name: `QuboxDaemon`.
Type: `SERVICE_WIN32_OWN_PROCESS`.
Start: `SERVICE_AUTO_START`.
Account: the user who installed it (stored securely by the installer).

Identity: `LocalService` or the installing user. Prefer the installing user so the daemon has access to the user's profile (`%APPDATA%`, `%LOCALAPPDATA%`). The installer runs as the user and registers the service with the user's credentials.

Install command (done by installer, not by the binary):
```
sc create QuboxDaemon binPath= "C:\Program Files\Qubox\qubox-daemon.exe" start= auto
```

The binary itself implements `define_windows_service!` / `service_dispatcher::start` from the `windows-service` crate.

**macOS: LaunchAgent**

```
~/Library/LaunchAgents/com.qubox.daemon.plist
```
```xml
<key>Label</key>
<string>com.qubox.daemon</string>
<key>ProgramArguments</key>
<array>
    <string>/usr/local/bin/qubox-daemon</string>
</array>
<key>KeepAlive</key>
<true/>
<key>RunAtLoad</key>
<true/>
<key>StandardOutPath</key>
<string>~/Library/Logs/qubox.log</string>
<key>StandardErrorPath</key>
<string>~/Library/Logs/qubox.err.log</string>
<key>MachServices</key>
<dict>
    <key>com.qubox.daemon</key>
    <true/>
</dict>
```

#### 1.6 PID file / process discovery

On all platforms: the IPC socket path is deterministic. The CLI detects a running daemon by trying to connect to the socket. If the socket is absent or the connection fails, the daemon is not running.

On Linux, also write a PID file to `$XDG_RUNTIME_DIR/qubox/qubox-daemon.pid` for tools that need PID discovery.

#### 1.7 Restart / crash recovery

When the daemon crashes:
- systemd / launchd / SCM restarts it (restart policy configured at install).
- On restart, the daemon opens `state.db` (persisted), loads pairings and settings.
- The signaling WebSocket reconnects (automatic; the daemon maintains a reconnect loop with 1s, 5s, 30s, 5m exponential backoff capped at 5m).
- Active sessions in flight are lost — the host-agent / client-cli processes detect the IPC disconnect and report failure. They do NOT auto-restart; the user reconnects via the GUI/CLI.
- TUF update state: if the daemon crashed mid-staging, the staged binary survives in the staging directory. On restart, the daemon checks for a staged binary and applies it if the current version is the same as before the crash (or ignores if the staged binary version ≤ current).

#### 1.8 Privilege model

- `qubox-daemon` runs as the invoking user. No root / admin required.
- On Linux: user `$UID` from the systemd user service.
- On Windows: the user who installed the service.
- On macOS: the user who owns the LaunchAgent.
- The IPC socket is created with `0777` (user-only by default; `dirs::runtime_dir()` returns a user-private path).
- The `redb` database file is created with `0600` (user-readable only).

### 2. IPC Interface (GUI ↔ Daemon, CLI ↔ Daemon)

#### 2.1 Transport

| Platform | Transport | Path |
|----------|-----------|------|
| Linux | Unix domain socket (stream) | `$XDG_RUNTIME_DIR/qubox/qubox.sock` |
| Windows | Named pipe | `\\.\pipe\Qubox` |
| macOS | Unix domain socket (stream) | `$HOME/Library/Application Support/com.qubox.daemon/daemon.sock` |

On Linux, the socket file is removed by the daemon on graceful shutdown. On crash, systemd socket activation (if used) preserves the socket; otherwise, a stale socket is detected on startup and removed.

#### 2.2 Wire format

Every message on the wire is a binary frame:

```
Offset  Size  Field
0       4     magic: u32        = 0xB0_1A_1C_BE (little-endian)
4       2     version: u16      = 0x0001 (current)
6       2     kind: u16         = 0x0001 (Request) | 0x0002 (Response) | 0x0003 (Event) | 0x0004 (Subscribe)
8       8     correlation_id: u64  (0 for fire-and-forget, unique for request/response)
16      4     payload_len: u32  (little-endian, not including header)
20      N     payload: [u8; payload_len]  (bincode-serialized)
```

- Magic: `0xB0_1A_1C_BE` — little-endian it reads as `BE 1C 1A B0` on the wire, a recognizable sentinel.
- `version`: 1 for this design. Old clients (< version 1) are rejected. Version is validated by the daemon; if the client sends an unsupported version, the daemon closes the connection with an IpcError::UnsupportedVersion.
- `kind`: distinguishes request, response, server-push event, and subscribe.
- `correlation_id`: client-generated unique ID per request. Responses echo the same ID. 0 means fire-and-forget (no response expected).
- `payload_len`: max 1 MiB (enforced by both sides). If exceeded, the connection is dropped.
- `payload`: bincode-serialized (default config, little-endian).

The message framing is symmetric: the reader reads exactly 20 bytes (header), validates magic and version, reads `payload_len` bytes, then deserializes the payload via bincode.

#### 2.3 Auth (peer identity verification)

**Linux / macOS**: `SO_PEERCRED` on the accepted Unix socket connection. The daemon reads `{ pid, uid, gid }` of the connecting peer. The connection is accepted if `uid == daemon_uid` (same Unix user). For multi-user scenarios (future), a simple ACL stored in redb maps `uid -> allowed: bool`.

**Windows**: The named pipe is created with a security descriptor that only allows the current user. The `windows-service` / `winapi` `CreateNamedPipe` with `pSecurityAttributes` limiting access to the user's SID. Additionally, after accepting, the daemon calls `GetNamedPipeClientProcessId` and verifies the client process runs under the same user token.

**No token / secret in the IPC message itself** — the OS guarantees the peer identity.

If auth fails: the daemon sends an `IpcResponse { error: Some(IpcError::AccessDenied) }` and closes the connection.

#### 2.4 Request/Response model

All IPC methods follow one of three patterns:

1. **Request-Response** (`kind=Request`, `kind=Response`): client sends a request with a unique `correlation_id`, daemon processes, sends a single response. The client blocks or awaits the response matching the correlation ID.

2. **Fire-and-forget** (`kind=Request`, `correlation_id=0`): no response. Used for `Quit` and `SubscribeEvents` stop.

3. **Server-streaming** (`kind=Subscribe`, then the daemon pushes `kind=Event` messages): client sends a `SubscribeEvents` request with a correlation ID; the daemon replies with an initial `SubscribeAck` response, then pushes `IpcEvent` messages as events occur. The stream lives until the client disconnects or sends `UnsubscribeEvents` (fire-and-forget).

#### 2.5 Exhaustive IPC method list

All request/response types are versioned via a top-level tagged enum:

```
IpcRequest:
  version: u16  # = 1 for this design
  method: IpcMethod
```

```
enum IpcMethod:
  # Pairing
  ListPairings { }
  ApprovePairing { pairing_id: Uuid }
  RevokePairing { pairing_id: Uuid }
  
  # Host lifecycle
  StartHost { config: HostConfig }
  StopHost { }
  GetHostStatus { }
  
  # Client lifecycle
  StartClient { config: ClientConfig }
  StopClient { }
  GetClientStatus { }
  
  # Update
  CheckUpdate { }
  ApplyUpdate { staged_version: String }
  GetUpdateStatus { }
  
  # TURN
  TurnIssueCredentials { peer_id: Uuid }
  
  # Signaling proxy (GUI doesn't need second WebSocket)
  SignalingForward { body: String }    # forward a JSON message to the signaling WS
  
  # Event subscription
  SubscribeEvents { }
  UnsubscribeEvents { }
  
  # Misc
  GetDaemonInfo { }
  Quit { }
```

```
IpcResponse:
  version: u16
  correlation_id: u64
  result: Result<IpcResult, IpcError>
```

```
enum IpcResult:
  ListPairings { pairings: Vec<PairingInfo> }
  ApprovePairing { }
  RevokePairing { }
  StartHost { ticket_b64: String }
  StopHost { }
  GetHostStatus { status: HostStatus }
  StartClient { ticket_b64: String }
  StopClient { }
  GetClientStatus { status: ClientStatus }
  CheckUpdate { available: Option<UpdateInfo> }
  ApplyUpdate { }
  GetUpdateStatus { status: UpdateStatus }
  TurnIssueCredentials { credentials: TurnCredentials }
  SignalingForward { }
  SubscribeAck { }
  UnsubscribeAck { }
  GetDaemonInfo { info: DaemonInfo }
  QuitAck { }
```

```
enum IpcError:
  UnsupportedVersion { server_version: u16 }
  AccessDenied { }
  InvalidRequest { reason: String }
  DaemonBusy { }
  NotAuthenticated { }     # signaling not connected
  HostAlreadyRunning { }
  HostNotRunning { }
  ClientAlreadyRunning { }
  ClientNotRunning { }
  SignalingDisconnected { }
  TurnUnavailable { reason: String }
  UpdateFailed { reason: String }
  SessionActive { }        # cannot stop/update while session is active
  Internal { reason: String }
```

```
struct PairingInfo:
  pairing_id: Uuid
  host_peer_id: Uuid
  client_peer_id: Uuid
  host_device_name: String
  client_device_name: String
  created_at_unix_millis: u64
  last_used_unix_millis: u64
```

```
struct HostConfig:
  # mirror of existing host-agent -- flags (see host-agent/src/main.rs Args)
  # but without --server (daemon owns the signaling connection)
  identity_path: Option<String>
  auto_approve_pairing: bool
  media_width: u32          # default 1920
  media_height: u32         # default 1080
  media_fps: u32            # default 60
  media_bitrate_kbps: u32   # default 20000
  codec: String             # "h264", "h265", "av1"
  encoder: String           # "auto", "nvenc", "vaapi", etc.
  display: Option<u32>
  resolution: Option<String>  # "WxH"
  scale_mode: String        # "fit", "fill", "crop", "native"
  datagram_media: bool
  audio: bool               # enable/disable audio capture
  turn_force: bool          # skip direct QUIC, use TURN always
  turn_servers: Vec<TurnServerConfig>
```

```
struct ClientConfig:
  identity_path: Option<String>
  host_peer_id: Option<Uuid>     # if the client is reconnecting to a known host
  transport: Option<String>      # "native_quic", "web_rtc", "relay_quic"
  codec: Option<String>
  decoder: Option<String>
  resolution: Option<String>     # "WxH"
  framerate: u32
  bitrate_kbps: Option<u32>
  scale_mode: String
  mouse_mode: String             # "relative" | "absolute"
  datagram_media: bool
  use_hw_decode: bool
  capture_gamepad: bool
  turn_force: bool
  turn_servers: Vec<TurnServerConfig>
```

```
struct TurnServerConfig:
  url: String               # e.g. "turn:turn.example.com:3478"
  username: Option<String>  # pre-shared (if static) or None (daemon fetches short-term)
  password: Option<String>
```

```
enum HostStatus:
  Idle { }
  Running { session_id: Uuid, host_peer_id: Uuid, started_at_unix_millis: u64 }
  Error { reason: String }
```

```
enum ClientStatus:
  Idle { }
  Running { session_id: Uuid, host_peer_id: Uuid, started_at_unix_millis: u64 }
  Error { reason: String }
```

```
struct UpdateInfo:
  current_version: String
  available_version: String
  release_notes_url: Option<String>
```

```
enum UpdateStatus:
  Idle { }
  Checking { }
  Available { version: String }
  Downloading { version: String, progress_pct: u8 }
  Staging { version: String }
  Ready { version: String }         # staged, restart required
  Applying { version: String }
  Failed { version: String, reason: String }
  UpToDate { }
```

```
struct TurnCredentials:
  urls: Vec<String>
  username: String
  password: String
  ttl_secs: u32
```

```
struct DaemonInfo:
  version: String
  pid: u32
  signaling_connected: bool
  host_status: HostStatus
  client_status: ClientStatus
  update_status: UpdateStatus
  uptime_secs: u64
```

Events pushed to subscribers:

```
enum IpcEvent:
  PairingRequested { request_id: Uuid, client_peer_id: Uuid, client_name: String, client_label: String }
  HostStateChanged { status: HostStatus }
  ClientStateChanged { status: ClientStatus }
  UpdateStateChanged { status: UpdateStatus }
  SignalingConnected { }
  SignalingDisconnected { reason: String }
  Error { message: String }
```

#### 2.6 Backward compatibility

- The `version: u16` field in the header allows the daemon to reject old clients with a clear error.
- New methods are added at the end of the `IpcMethod` enum (bincode is backwards-compatible for tagged enums when variants are appended).
- New fields in request/response structs use `#[serde(default)]` (bincode ignores unknown fields by default when using `bincode::DefaultOptions`).
- Old clients ignore unknown response fields.

#### 2.7 Rate limiting and connection limits

- Max concurrent IPC connections: 8 (hard-coded, configurable via `QUBOXD_IPC_MAX_CONNS` env var).
- Per-connection rate limit: 1000 requests/second. Enforced via a simple token bucket (1000 tokens, refill at 1000/sec). Exceeding drops the connection.
- Per-connection inbound message size: max 1 MiB payload.
- `SubscribeEvents` is limited to 1 subscription per connection (the daemon enforces exactly one event stream per client connection).

### 3. The `redb` State Schema

#### 3.1 File location

| Platform | Path |
|----------|------|
| Linux | `$XDG_CONFIG_HOME/qubox/state.db` (typically `~/.config/qubox/state.db`) |
| Windows | `%APPDATA%\Qubox\state.db` |
| macOS | `~/Library/Application Support/com.qubox.daemon/state.db` |

The `directories` crate (`ProjectDirs::from("com", "qubox", "qubox")`) resolves these paths at runtime.

#### 3.2 Schema tables

All tables use `redb::TableDefinition`. Keys and values are bincode-serialized.

**Table: `meta`**
```
TableDefinition<&str, &[u8]>
```
| Key | Value (bincode) | Description |
|-----|-----------------|-------------|
| `"schema_version"` | `u32` (little-endian) | Current schema version. Starts at 1. Incremented on migration. |

**Table: `pairings`**
```
TableDefinition<&[u8; 16], &[u8]>  # key = UUID bytes (pairing_id), value = bincode(PairingRecord)
```
```rust
struct PairingRecord {
    pairing_id: [u8; 16],     // UUID bytes
    host_peer_id: [u8; 16],   // UUID bytes
    client_peer_id: [u8; 16], // UUID bytes
    host_device_name: String,
    client_device_name: String,
    created_at_unix_millis: u64,
    last_used_unix_millis: u64,
}
```

**Table: `host_state`**
```
TableDefinition<&str, &[u8]>  # key = "state", value = bincode(HostStateRecord)
```
```rust
struct HostStateRecord {
    last_seen_unix_millis: u64,
    last_session_id: Option<[u8; 16]>,
    config_hash: u64,          // hash of the last-active HostConfig (for change detection)
    peer_id: Option<[u8; 16]>, // persisted host peer ID after pairing
}
```

**Table: `client_state`**
```
TableDefinition<&str, &[u8]>  # key = "state"
```
Same shape as `HostStateRecord` but for the client role.

**Table: `settings`**
```
TableDefinition<&str, &str>
```
Generic key-value store for user configuration. Examples:
| Key | Value |
|-----|-------|
| `"server_url"` | `"ws://127.0.0.1:7000/ws"` |
| `"turn_credential_ttl_secs"` | `"3600"` |
| `"update_interval_hours"` | `"24"` |
| `"update_channel"` | `"stable"` |
| `"auto_start_host"` | `"false"` |
| `"auto_approve_pairing"` | `"false"` |

All values are strings. The daemon parses them to appropriate types on load.

**Table: `tuf_root`**
```
TableDefinition<u64, &[u8]>  # key = version (root.json version), value = raw JSON bytes
```
Stores the complete verified `root.json` metadata. The daemon keeps the latest trusted root version. Multiple versions are kept for root rotation rollback protection.

Key is TUF root version (`u64`). Value is the raw `root.json` bytes as fetched and verified.

**Table: `tuf_targets`**
```
TableDefinition<u64, &[u8]>  # key = version, value = raw JSON bytes
```
Stores the latest verified `targets.json`.

**Table: `tuf_snapshot`**
```
TableDefinition<u64, &[u8]>  # key = version, value = raw JSON bytes
```
Stores the latest verified `snapshot.json`.

**Table: `tuf_timestamp`**
```
TableDefinition<u64, &[u8]>  # key = version, value = raw JSON bytes
```
Stores the latest verified `timestamp.json`.

**Table: `update_history`**
```
TableDefinition<u64, &[u8]>  # key = epoch-millis when applied, value = bincode(UpdateRecord)
```
```rust
struct UpdateRecord {
    applied_at_unix_millis: u64,
    from_version: String,
    to_version: String,
    success: bool,
    error_message: Option<String>,
}
```
Capped at 100 entries. When inserting the 101st, the oldest entry is removed (maintained in application code, not via redb triggers).

**Table: `session_history`**
```
TableDefinition<u64, &[u8]>  # key = epoch-millis when session ended, value = bincode(SessionRecord)
```
```rust
struct SessionRecord {
    session_id: [u8; 16],
    started_at_unix_millis: u64,
    ended_at_unix_millis: u64,
    role: String,           // "host" or "client"
    peer_peer_id: [u8; 16], // the other side's peer ID
    transport: String,      // "native_quic", "turn_udp", "turn_tcp"
    bytes_sent: u64,
    bytes_received: u64,
    duration_secs: u64,
    error: Option<String>,
}
```
Capped at 200 entries. Eviction same as `update_history`.

#### 3.3 Schema versioning

When `meta["schema_version"]` is absent (first run), the daemon initializes all tables with version 1.

When the daemon is upgraded and the code expects a new schema version (e.g., version 2), the daemon runs a migration function:

```rust
fn migrate(db: &Database, from: u32) -> Result<(), MigrationError> {
    match from {
        0 | 1 => {
            // Create new table `settings` (no-op if exists)
            // Add column `config_hash` to host_state records
            // etc.
            write_meta_version(db, 2)?;
            // fall through
        }
        2 => {
            // Next migration
            write_meta_version(db, 3)?;
        }
        current_version => {
            // No migration needed
        }
    }
    Ok(())
}
```

The migration is called inside a write transaction. Each version step is committed atomically. If a migration fails (e.g., corrupted data), the daemon refuses to start and logs a clear error message instructing the user to restore from backup or delete `state.db`.

#### 3.4 Concurrency and locking

`redb` uses MVCC with a single writer and multiple concurrent readers.

**Locking discipline:**
- All writes go through the daemon's main event loop (single async task that serializes `IpcRequest` handling). This ensures exactly one write transaction at a time.
- Reads that do NOT need a consistent snapshot across multiple tables use `db.begin_read()` — no blocking on concurrent writes (MVCC).
- Reads that need a consistent snapshot across tables (e.g., reading `tuf_root` + `tuf_timestamp` together for verification) also use a single `begin_read()` — the snapshot covers both.
- Write transactions: `db.begin_write()`. If the write lock is contended (should not happen given the serial execution model), `redb` internally blocks the writer thread. Keep write transactions short (<10 ms) — write TUF metadata as a single atomic write, not field-by-field.
- Long-running reads (TUF verification) hold a read transaction for at most 5 seconds. After that, the transaction is dropped and retried (to avoid MVCC snapshot bloat).

### 4. The TUF State Machine (Auto-Update)

#### 4.1 State definitions

```
enum TufUpdateState:
  Idle
  FetchingTimestamp
  FetchingSnapshot
  FetchingTargets
  Verifying
  Downloading
  Staging
  Applying
  Failed { version: String, reason: String, retry_at_unix_millis: u64 }
  UpToDate
```

#### 4.2 Transitions and side effects

Each transition is triggered by a timer tick (configurable interval, default 24 hours), a manual `CheckUpdate` IPC call, or completion of the previous step.

| From | Trigger | Side effect | Timeout | Retry |
|------|---------|-------------|---------|-------|
| `Idle` | Timer tick OR `CheckUpdate` | → `FetchingTimestamp`. Fetch `timestamp.json` from `{repo_url}/timestamp.json`. | 10 s | Exponential: 1s, 5s, 30s, 5m max, 5 attempts |
| `FetchingTimestamp` | HTTP 200 + body | → `FetchingSnapshot`. Parse `timestamp.json`, extract snapshot version + hashes. | — | — |
| `FetchingTimestamp` | HTTP error / timeout | → `Failed`. If retries exhausted; else → `Idle` + schedule retry. | — | — |
| `FetchingTimestamp` | Signature verification fail | → `Failed` with `"timestamp signature invalid"`. | — | — |
| `FetchingSnapshot` | Fetched `{repo_url}/snapshot.json` | → `Verifying`. Verify snapshot signature against root, verify snapshot hash against timestamp. | 30 s | Exponential, 3 attempts |
| `Verifying` | All metadata verified | → `FetchingTargets`. Fetch `targets.json`. | — | — |
| `Verifying` | Any hash mismatch or signature invalid | → `Failed` with specific reason. | — | — |
| `FetchingTargets` | HTTP 200 + body | → `Verifying` (targets verification). Verify targets signature, hash against snapshot. | 30 s | Exponential, 3 attempts |
| `Verifying` (targets) | Targets verified | → `Downloading` if any target is newer. → `UpToDate` if no new targets. | — | — |
| `Downloading` | Per-target download complete | → `Staging`. Write binary to staging directory. Verify hash against targets. | 120 s per target | Exponential, 2 attempts |
| `Staging` | All targets staged and verified | → `Applying` (auto-restart daemon). | — | — |
| `Staging` | Hash mismatch | → `Failed` with `"binary hash mismatch"`. | — | — |
| `Applying` | Replace running binary, restart | → `Idle` (after restart). | 30 s for old daemon exit | Rollback if crash |
| `Failed` | Timer tick after `retry_at_unix_millis` | → `Idle`. Manual `CheckUpdate` also resets. | — | — |
| `UpToDate` | Timer tick OR `CheckUpdate` | → `Idle` → `FetchingTimestamp` (re-check). | — | — |

#### 4.3 Verification rules

- **Root verification**: the initial `root.json` is shipped with the daemon binary (embedded at compile time via `include_bytes!`). Subsequent root updates require multi-sig threshold (N-of-M keys, configurable in `root.json`). The threshold is validated by `tough`.
- **Timestamp verification**: signature checked against the root's timestamp key.
- **Snapshot verification**: signature checked against the root's snapshot key, AND hash checked against the timestamp's snapshot hash.
- **Targets verification**: hash checked against the snapshot's targets hash.
- **Binary hash**: SHA-256 of the downloaded binary is checked against the targets entry.
- **Expiry**: each metadata file has an `expires` field. If `expires < now`, the metadata is rejected. The daemon logs a warning 7 days before expiry.
- **Root rotation**: `tough` supports root rotation with multi-sig. The daemon downloads new `N.root.json` (where N is the next version), accumulates signatures until the threshold is met, then replaces the trusted root. This is handled by `tough`'s client.

#### 4.4 Self-update mechanics

**Linux:**
```
Stage:  $XDG_DATA_HOME/qubox/staging/qubox-daemon-v{N}
Apply:  rename(2) the staged binary over the running binary at /usr/bin/qubox-daemon
        then execve() itself (or exit and let systemd restart it)
```

The daemon:
1. Downloads the new binary to `{staging_dir}/qubox-daemon-v{version}`.
2. Verifies the hash.
3. If the running binary is at `/usr/bin/qubox-daemon`:
   - `rename(staging_bin, running_bin)` — atomic on Linux (same filesystem).
4. Sends `sd_notify("STOPPING=1")`.
5. Calls `execv(running_bin, args)` with a special `--post-update` flag that skips re-verifying the staged binary.
6. If `execv` fails (e.g., permission denied): logs error, goes to `Failed` state, keeps the old binary.

**Windows:**
Cannot replace a running `.exe`. The daemon:
1. Downloads new binary to `{staging_dir}\qubox-daemon-v{version}.exe`.
2. Verifies the hash.
3. Creates a scheduled task (or uses `MoveFileEx` with `MOVEFILE_DELAY_UNTIL_REBOOT`) to replace the running binary on next boot.
4. Stops the service via SCM. The SCM restart policy starts the new binary.

Alternatively (better UX for gaming): the daemon spawns a lightweight helper process (`qubox-daemon-switch.exe`) that waits for the daemon to exit, renames the staged binary over the running binary, and starts the service. The helper runs with `CREATE_NO_WINDOW` and inherits the daemon's identity.

**Windows approach for Phase 1**: use the helper process pattern. The daemon stages the binary, spawns `qubox-daemon-switch.exe` with the staged path and current binary path as arguments, then exits. The helper waits (up to 30s), renames, and starts the service via `sc start`.

**macOS:**
Same as Linux: `rename(2)` the staged binary over the running binary at `/usr/local/bin/qubox-daemon`, then `execv`. macOS `rename` is atomic when source and dest are on the same volume (they are, for staged and installed paths under `/usr/local`).

#### 4.5 Rollback

If the newly-applied daemon fails to start (crashes within 30 seconds, detected by systemd `Restart=on-failure` / launchd `KeepAlive` / SCM auto-restart):

1. The previous binary is backed up at `{data_dir}/qubox-daemon.prev` before the rename.
2. On crash, the service manager restarts the binary. If the new binary crashes again within 30s, it's a crash loop.
3. The daemon detects it was started with `--post-update` and the version matches the newly-applied version. It checks a "tried_update" flag file.
4. If crash loop detected (flag file exists): restore `qubox-daemon.prev` → binary, log "update rolled back", delete flag file, `execv` the previous binary.
5. If the daemon runs successfully for 60 seconds, the flag file is deleted and the `.prev` backup is removed.

#### 4.6 Pre-update hook

**Decision: refuse to update if a session is active.**

Rationale: pausing and resuming a session mid-update adds complexity (session state serialization, signaling re-negotiation) that is not justified for Phase 1. A session typically lasts hours; the user can finish their session and update later.

Implementation: before transitioning from `Idle` → `FetchingTimestamp` on a timer tick, check `host_status` and `client_status`. If either is `Running`, skip the tick and log "update deferred; session active". The CLI `qubox-daemon update` command returns an error `SessionActive` if called during a session.

#### 4.7 Network failure handling

- HTTP client: `reqwest` with a 10-second connect timeout and per-request timeout of 30-120 seconds (as specified in the transition table).
- Exponential backoff: 1s, 5s, 30s, 300s (capped). Jitter: ±20%.
- Max retries: per transition table.
- After all retries exhausted: state → `Failed { retry_at_unix_millis: now + 1h }`. The timer tick retries in 1 hour. Manual `CheckUpdate` resets the retry counter immediately.
- Offline detection: if all retries fail with `ConnectionRefused` or `DnsFailure`, the daemon sets `Failed` with a 5-minute retry (network may come back).
- Fallback: the CLI command `qubox-daemon update --force` skips the retry check and forces a fresh fetch cycle.

### 5. TURN Credential Issuance

#### 5.1 Signaling server endpoint

**New HTTP endpoint on the existing signaling server:**

```
POST /v1/turn/credentials
Authorization: Bearer <pairing_token>
Content-Type: application/json

Request body:
{
  "peer_id": "uuid-of-requesting-peer"
}

Response (200):
{
  "urls": ["turn:turn.example.com:3478", "turn:turn.example.com:443?transport=tcp"],
  "username": "1719878400:base64(hmac)",
  "password": "base64(hmac(secret, username))",
  "ttl": 3600
}

Response (401): Authorization header missing or invalid
Response (403): Peer not authorized for TURN
```

The endpoint is authenticated with the same pairing token mechanism as the WebSocket. The `Authorization` header is validated against the signaling server's active session store. If the token is valid and the peer is allowed, credentials are issued. If the token is expired or invalid, 401 is returned.

#### 5.2 Credential computation

```
username = "{expiry_unix}:{base64(hmac_sha1(shared_secret, expiry_unix_str))}"
password = base64(hmac_sha1(shared_secret, username))
```

Where:
- `expiry_unix` is the Unix timestamp of the credential expiry (current_time + ttl). Represented as a decimal ASCII string.
- `hmac_sha1` is HMAC-SHA1 per RFC 2104.
- `shared_secret` is the TURN server's static auth secret (configured in coturn via `static-auth-secret`).
- `base64` is standard base64 (RFC 4648) with padding.

The coturn server authenticates the credentials using its `static-auth-secret` and the same computation (standard coturn behavior).

#### 5.3 Shared secret storage

**Decision: environment variable `QUBOX_TURN_SECRET` on the signaling server.**

Rationale:
- The dev box is headless and has no keyring.
- Environment variables are the standard 12-factor app pattern.
- In production, the env var is set in the systemd service (or Docker Compose).
- Redb is on the daemon side, not the signaling server side. The signaling server is stateless for TURN.

The signaling server reads `QUBOX_TURN_SECRET` at startup and keeps it in memory. If unset, the TURN credential endpoint returns 501 Not Implemented.

The signaling server also reads `QUBOX_TURN_URLS` (comma-separated list of TURN URLs) and `QUBOX_TURN_TTL_SECS` (default 3600).

```
QUBOX_TURN_SECRET=supersecret
QUBOX_TURN_URLS=turn:turn.example.com:3478,turn:turn.example.com:443?transport=tcp
QUBOX_TURN_TTL_SECS=3600
```

#### 5.4 Multiple TURN servers and weighting

The signaling server config supports a list of TURN server entries, each with `url`, optional `weight`, and optional `region`:

```
QUBOX_TURN_SERVERS='[{"url":"turn:eu.example.com:3478","weight":10,"region":"eu"},{"url":"turn:us.example.com:3478","weight":5,"region":"us"}]'
```

The endpoint returns ALL configured servers in the `urls` array. The client picks one based on lowest RTT (the client pings each TURN server with a STUN binding request before connecting). This is a client-side decision; the daemon/signaling-server just provides the list.

For v1, a simple flat list suffices. Weight/region is reserved for future use.

#### 5.5 Auth secret rotation

The signaling server supports up to two concurrent secrets: `current` and `previous`.

```
QUBOX_TURN_SECRET=current_secret
QUBOX_TURN_SECRET_PREVIOUS=previous_secret
```

When issuing credentials: always sign with `current_secret`.
When verifying incoming credentials from a TURN client (if the signaling server ever needs to verify — it doesn't, coturn does): try both secrets. The coturn server must be configured with both secrets via the `static-auth-secret` option; coturn's `stale_nonce` mechanism handles the transition.

The server rotates secrets by changing the env var and restarting (or via SIGHUP). For Phase 1, restart is required.

### 6. QUIC-over-TURN Integration

#### 6.1 TURN client abstraction

New module `crates/qubox-transport/src/turn.rs`:

```rust
pub struct TurnClient {
    server_addr: SocketAddr,        // resolved TURN server
    credentials: TurnCredentials,
    allocation: TurnAllocation,
    channel_num: u16,
    peer_channel_map: HashMap<SocketAddr, u16>,
    // The UDP socket used for TURN communication (could be a regular UdpSocket
    // or a custom wrapper)
    socket: tokio::net::UdpSocket,
}

pub struct TurnAllocation {
    pub relayed_addr: SocketAddr,
    pub lifetime: u32,
}

pub struct TurnCredentials {
    pub urls: Vec<String>,
    pub username: String,
    pub password: String,
    pub ttl: u32,
}

impl TurnClient {
    /// Connect to a TURN server via UDP, authenticate, and allocate a relay address.
    pub async fn connect_udp(
        server: SocketAddr,
        credentials: &TurnCredentials,
    ) -> Result<Self>;

    /// Connect via TCP (TURN/TCP per RFC 6062).
    pub async fn connect_tcp(
        server: SocketAddr,
        credentials: &TurnCredentials,
    ) -> Result<Self>;

    /// Create a permission for a given peer address so the TURN server
    /// will relay traffic to/from it.
    pub async fn create_permission(&mut self, peer: SocketAddr) -> Result<()>;

    /// Bind a channel number to a peer address. After this, data sent to
    /// the TURN server with this channel number goes to the peer, and
    /// data from the peer arrives tagged with this channel number.
    pub async fn channel_bind(&mut self, peer: SocketAddr) -> Result<u16>;

    /// Send raw UDP data to a peer through the TURN relay.
    pub async fn send_to(&self, buf: &[u8], peer: SocketAddr) -> Result<()>;

    /// Receive raw UDP data from the TURN relay.
    pub async fn recv_from(&mut self) -> Result<(SocketAddr, Vec<u8>)>;

    /// Refresh the allocation (extends lifetime).
    pub async fn refresh(&mut self) -> Result<()>;

    /// Close the allocation.
    pub async fn close(&mut self) -> Result<()>;
}
```

The TURN protocol framing (STUN messages, ChannelData frames) is handled inside `TurnClient`. Use the `stun-rs` or `turn-client-proto` crate for STUN message encoding/decoding.

#### 6.2 The `TurnUdpSocket` adapter for quinn

`quinn` uses a runtime that provides `AsyncUdpSocket`. The `quinn_proto::runtime::UdpSocket` trait (or `quinn::UdpSocket` in quinn 0.11+) is what allows the QUIC endpoint to send/receive UDP datagrams.

The adapter wraps `TurnClient`:

```rust
/// A wrapper that presents a TURN relayed channel as a quinn-compatible
/// UDP socket. quinn sends QUIC packets into this adapter, which
/// packages them as TURN ChannelData frames and sends them to the TURN
/// server. Incoming ChannelData frames from the TURN server are unwrapped
/// and presented as UDP datagrams to quinn.
///
/// This works because TURN is transparent to the QUIC protocol — the TURN
/// server just forwards bytes.
pub struct TurnUdpSocket {
    turn_client: Arc<Mutex<TurnClient>>,
}

impl TurnUdpSocket {
    pub fn new(turn_client: TurnClient) -> Self;
}

// Implement the quinn UdpSocket trait:
// - async_send_to(&self, buf: &[u8], addr: SocketAddr) -> io::Result<()>
//   → calls TurnClient::send_to(buf, addr)
// - async_recv_from(&self, buf: &mut [u8]) -> io::Result<(usize, SocketAddr)>
//   → calls TurnClient::recv_from(), copies into buf
```

For quinn 0.11, the trait to implement is `quinn::UdpTransport` or the runtime's `AsyncUdpSocket`. The exact trait name depends on the quinn version; the backend-architect must check `quinn::Endpoint::new` / `Endpoint::new_with_abstract_udp_socket` or the `quinn_udp` crate's `UdpSocket` trait.

If quinn does not expose a trait to inject a custom UDP socket, the fallback is:
1. Open a local UDP socket.
2. Option A: run a local TURN-only UDP proxy (`send_to_turn` → forward to local UDP → quinn reads from local UDP).
3. Option B: use `Endpoint::new` with `SocketAddr` binding to the local UDP socket, and somehow intercept packets by using a userspace `MioUdpSocket` wrapper.

**Recommended approach for Phase 1**: Option A — a local loopback UDP proxy. The daemon opens a real UDP socket on `127.0.0.1:0`, creates the TURN client, then runs a small proxy task that reads from the local socket, sends via TURN, and vice versa. Quinn binds to the local proxy socket. This avoids needing a custom `AsyncUdpSocket` implementation that quinn may not support.

If `quinn::Endpoint::new_with_abstract_udp_socket` or similar exists in quinn 0.11, use it directly. The backend-architect determines feasibility during implementation.

#### 6.3 Fallback chain

**Client side** (`turn_connect` in `crates/qubox-transport`):

```
fn connect_to_host_via_turn(ticket, credentials, config) -> Result<Connection>:
    1.  Resolve TURN server addresses from credentials.urls.
        Filter to UDP URLs (preferred) and TCP URLs (fallback).
        Sort by RTT (send STUN binding request, measure response time).

    2.  For each TURN server (sorted by RTT):
        a. Try TURN/UDP:
           - Create TurnClient::connect_udp(server, credentials)
           - Allocate -> get relayed_addr
           - CreatePermission(host_relayed_addr)
           - ChannelBind(host_relayed_addr)
           - Build TurnUdpSocket / proxy
           - Create quinn Endpoint using the TurnUdpSocket
           - Connect to host's relayed address on the TURN server
           - Timeout: 5 seconds
           - On success: return Connection
        b. If TURN/UDP fails (timeout, no relay, protocol error):
           Try TURN/TCP:
           - Same as above but TurnClient::connect_tcp
           - Timeout: 5 seconds
        c. If TURN/TCP fails:
           Try next TURN server

    3.  If all TURN servers failed:
        Return error: "Could not establish TURN relay for any configured server"
```

**Host side** mirrors the client: the host fetches its own TURN credentials from the signaling server, allocates, and advertises its relayed address through the signaling channel.

The host's relayed address is communicated to the client via the existing session plan flow: `SessionRequested.host_credential` or via an additional `relayed_addr` field in `NativeQuicTicket`.

**For Phase 1**, extend `NativeQuicTicket` with:
```
struct NativeQuicTicket {
    // existing fields...
    // NEW:
    turn_relayed_addr: Option<SocketAddr>,   // if TURN is being used
    turn_server_addr: Option<SocketAddr>,    // the TURN server the host is on
}
```

Or, simpler: the host and client independently fetch TURN credentials and allocate. The host sends its relayed address via a new `SessionSignal::TurnRelayedAddress { addr: SocketAddr }`. The client, after receiving the ticket, also allocates and connects to the host's relayed address.

#### 6.4 Timeout and retry config

Add to `HostConfig` and `ClientConfig`:
```rust
struct TurnConfig {
    /// Per-TURN-server timeout for connection (allocation + permission + bind).
    turn_connect_timeout_ms: u32,       // default 5000
    /// Whether to skip the direct QUIC attempt entirely.
    turn_force: bool,                    // default false
    /// Whether to only use TURN (skip direct QUIC but also skip TURN/UDP fallback to TURN/TCP).
    turn_only: bool,                     // default false
    /// Custom TURN server list (overrides signaling-provided list).
    turn_servers: Vec<TurnServerConfig>,
}
```

#### 6.5 CLI flags on `host-agent` and `client-cli`

Add to both `Args` structs:
```
--turn-server <URL>           Repeatable. Custom TURN server URL (overrides signaling).
--turn-username <STRING>      Static TURN username (if not using short-term credentials).
--turn-password <STRING>      Static TURN password (if not using short-term credentials).
--turn-force                  Skip direct QUIC attempt, always use TURN.
--turn-only                   Skip direct QUIC AND skip TURN/TCP fallback (UDP only).
```

When `--turn-server` is not provided and the daemon is running, the daemon fetches credentials from the signaling server. When the daemon is not running (standalone mode), and `--turn-server` is not provided, TURN is not attempted.

#### 6.6 Logging

The TURN lifecycle is logged via `tracing` spans:

```
turn.connect { server = %server_addr, transport = "udp" }
turn.allocate { relayed_addr = %addr, lifetime_secs }
turn.create_permission { peer = %peer_addr }
turn.channel_bind { channel = %num, peer = %peer_addr }
turn.send { bytes = N, to = %peer_addr }
turn.recv { bytes = N, from = %peer_addr }
turn.refresh { lifetime_secs }
turn.close { }
turn.fallback { from = "udp", to = "tcp", reason = %error }
turn.failure { server = %server_addr, error = %error }
```

Each span has `session_id` attached for correlation.

### 7. Migration Plan: Current State → Daemon-Mediated State

#### 7.1 Order of merge

Each step is self-contained and does not break `cargo build --workspace`:

**Step 1: Signaling server TURN endpoint (additive)**
- Add `apps/signaling-server/src/turn.rs` module.
- Add `POST /v1/turn/credentials` handler.
- Add `TurnConfig` struct and credential issuance logic.
- No changes to existing WebSocket endpoints.
- New env vars: `QUBOX_TURN_SECRET`, `QUBOX_TURN_URLS`, `QUBOX_TURN_TTL_SECS`.
- `cargo build --workspace` passes.
- The host-agent and client-cli do not use this yet — it is dormant.

**Step 2: Transport TURN client (additive)**
- Add `crates/qubox-transport/src/turn.rs` with `TurnClient`.
- Add `TurnCredentials` struct to `qubox-proto` or keep it in transport.
- Add `TurnUdpSocket` adapter (or loopback proxy pattern).
- Unit tests for STUN message handling and HMAC computation.
- `cargo build --workspace` passes.
- No existing code is modified.

**Step 3: Fallback logic in host-agent and client-cli (behind feature flag)**
- Add `--turn-server`, `--turn-force`, `--turn-only` flags to both CLIs.
- Add fallback chain to `host-agent`'s session startup and `client-cli`'s `connect_to_native_quic`.
- When `--turn-server` is provided, the binary fetches TURN creds from signaling (new HTTP call) and uses the turn transport.
- Default behavior unchanged: no TURN unless flag is provided.
- `cargo build --workspace` passes.

**Step 4: Daemon skeleton (new crate, additive)**
- Create `apps/daemon/Cargo.toml` with dependencies.
- `apps/daemon/src/main.rs`: parse CLI, init tracing, run `DaemonService`.
- `apps/daemon/src/service.rs`: `DaemonService` struct with `run()` method.
- Platform-specific: `linux.rs`, `windows.rs`, `macos.rs` in `apps/daemon/src/service/`.
- No IPC yet. The daemon just starts and waits for a signal.
- `cargo build --workspace` passes.

**Step 5: Daemon IPC server + handlers**
- `apps/daemon/src/ipc.rs`: `IpcServer` with Unix socket / Named Pipe listener.
- Message framing (magic, version, length).
- Handler dispatch.
- Stub handlers that return `IpcError::NotAuthenticated` (no signaling connection yet).
- `cargo build --workspace` passes.

**Step 6: Daemon state (redb)**
- `apps/daemon/src/state.rs`: `StateDb` struct with open/close and table accessors.
- Schema version check and migration.
- No daemon logic uses it yet — just open/create and verify.
- `cargo build --workspace` passes.

**Step 7: Daemon signaling connection**
- `apps/daemon/src/signaling.rs`: WebSocket connection to signaling server.
- Auto-reconnect loop with exponential backoff.
- Pairing state machine integration.
- The daemon maintains `PeerDescriptor` and `PairingInfo` in memory and redb.
- IPC handlers become real (they forward to the signaling connection).

**Step 8: Daemon host/client lifecycle**
- `apps/daemon/src/host.rs` and `apps/daemon/src/client.rs`.
- `StartHost` IPC → daemon starts a subprocess `host-agent --use-daemon` and returns the ticket.
- `StopHost` IPC → daemon sends SIGTERM to the subprocess.
- `GetHostStatus` → returns status tracked by the daemon.
- Same for client.

**Step 9: Daemon TUF update**
- `apps/daemon/src/update.rs`: `UpdateChecker`.
- Timer tick, state machine, verification, staging, self-replace.
- IPC handlers: `CheckUpdate`, `ApplyUpdate`, `GetUpdateStatus`.

**Step 10: Installer**
- Linux: systemd user service unit files in `dist/linux/`.
- Windows: MSI/WiX installer with helper binary.
- macOS: LaunchAgent plist in `dist/macos/`.
- The daemon's CLI gains `install` and `uninstall` subcommands.

#### 7.2 Deprecation timeline

- Immediately: nothing deprecated. Everything works as before.
- After daemon deployment (Step 10): the daemon is the default way to run.
- One release cycle later: `host-agent` and `client-cli` default to `--use-daemon` auto-detect. Standalone mode still works via `--no-daemon`.
- Two releases later: `host-agent` and `client-cli` emit a deprecation warning when run without a daemon.
- TBD: remove standalone mode entirely (requires migration of CI/test scripts).

#### 7.3 File layout after migration

```
apps/
  daemon/
    Cargo.toml
    src/
      main.rs
      service.rs
      service/
        mod.rs
        linux.rs
        windows.rs
        macos.rs
      ipc.rs
      state.rs
      signaling.rs
      host.rs
      client.rs
      update.rs
  host-agent/     ← unchanged except --use-daemon / TURN flags
  client-cli/     ← unchanged except --use-daemon / TURN flags
  signaling-server/
    src/
      main.rs     ← modified: add turn.rs
      turn.rs     ← new

crates/
  qubox-transport/
    src/
      lib.rs      ← unchanged
      turn.rs     ← new
```

### 8. Test Plan

#### 8.1 Unit tests

| Area | Test | What it verifies |
|------|------|------------------|
| IPC | Message round-trip: serialize → deserialize all `IpcMethod` and `IpcResponse` variants | Wire format stability, bincode compat |
| IPC | Header validation: bad magic, bad version, oversized payload | Rejection logic |
| IPC | Auth: mock `SO_PEERCRED` with matching and non-matching UID | Auth gate |
| IPC | Rate limit: send 1001 requests in 1 second, expect connection drop | Token bucket enforcement |
| redb | Schema open/close: create DB, write meta, close, reopen, verify version | Persistence |
| redb | Schema migration: open DB with version 0, run migration to 1, verify | Migration path |
| redb | Concurrency: concurrent reader + writer on separate threads | MVCC correctness |
| redb | Capped tables insert N+1 entries, verify oldest evicted | Eviction |
| TURN | HMAC computation: known secret + expiry → expected username and password | RFC compliance |
| TURN | Credential issuance: `issue_credentials` output round-trips through coturn's verification | Interop |
| TUF | State machine transitions: exhaustive table-driven test for all (from_state, trigger) pairs | No stuck states |
| TUF | Signature verification: valid sig accepted, invalid sig rejected | Tough integration |
| TUF | Expiry check: expired metadata rejected | Time safety |
| TUF | Multi-sig threshold: N-of-M root rotation test | Root rotation |
| Transport `TurnClient` | STUN message encoding/decoding round-trip | Wire protocol |
| Transport `TurnClient` | ChannelData frame encoding/decoding | Wire protocol |
| Transport `TurnClient` | Allocation request/response lifecycle (mock server) | Protocol state machine |

#### 8.2 Integration tests

**Coturn in Docker** (devops-automator provides the container; the test code is in this project):
- `tests/turn.rs` (integration test, `#[cfg(feature = "integration-tests")]` or separate test binary):
  1. Docker compose file at `tests/docker/coturn.yml`.
  2. Start coturn with `static-auth-secret`.
  3. Compute valid short-term credentials via HMAC.
  4. `TurnClient::connect_udp` → allocate → verify `relayed_addr` is valid.
  5. `channel_bind` to a known peer → verify.
  6. `send_to` + `recv_from` loopback: send 100 random-byte payloads, verify received intact.
  7. `refresh` allocation.
  8. `close` allocation.
  9. TCP variant: same sequence over TURN/TCP.

**TURN credential endpoint integration**:
  1. Start signaling server with `QUBOX_TURN_SECRET`.
  2. `POST /v1/turn/credentials` with valid auth → 200 + valid credentials.
  3. `POST /v1/turn/credentials` with no auth → 401.
  4. Use credentials against coturn container → allocation succeeds.

**Daemon IPC integration**:
  1. Start `qubox-daemon` (foreground, test mode that doesn't require signaling).
  2. Connect client socket, send `ListPairings` request, verify response.
  3. Send `GetDaemonInfo` → verify version string.
  4. Send `Quit` → verify daemon exits cleanly.
  5. Test that a non-matching UID cannot connect (Linux/macOS only).

**Daemon + redb integration**:
  1. Start daemon with a temp DB path.
  2. Send `ApprovePairing` (synthetic) → verify state.db contains the record.
  3. Restart daemon with the same DB path.
  4. Send `ListPairings` → verify record survived restart.

**TUF update integration** (mock repository):
  1. Start a local HTTP server serving TUF metadata.
  2. Configure daemon with `--update-repo-url http://127.0.0.1:PORT`.
  3. Send `CheckUpdate` → verify transition through the state machine.
  4. If a new binary is staged: verify the staged file exists and hash matches.

#### 8.3 End-to-end tests

**TURN relayed QUIC session**:
  1. Start coturn in Docker.
  2. Start signaling server.
  3. Start `host-agent` with `--turn-force --turn-server turn:127.0.0.1:3478` (and credentials).
  4. Start `client-cli start-session --host X --turn-force`.
  5. Verify QUIC handshake completes through the relay.
  6. Verify at least 1 video frame is transmitted and decoded.
  7. Verify session stats show TURN was used.

**NAT simulation** (requires Linux with `iptables`):
  1. Create two network namespaces with NAT (simulating two different home networks).
  2. Host in namespace A, client in namespace B.
  3. TURN server in the root namespace (simulating public internet).
  4. Without TURN: direct QUIC fails (expected).
  5. With TURN: verify full streaming session works.
  6. Measure latency: average end-to-end, compare to direct (non-NAT) baseline.

**Fallback chain**:
  1. Start coturn, signaling, host, client.
  2. Block UDP to the TURN server (iptables drop).
  3. Client should fall back to TURN/TCP.
  4. Verify session starts over TCP relay.

**Daemon-mediated session**:
  1. Start daemon.
  2. `qubox-daemon status` → shows idle.
  3. Pair via CLI through daemon IPC.
  4. Start host via daemon IPC.
  5. Start client session.
  6. Verify streaming works.
  7. Stop host → verify cleanup.

**Self-update rollback**:
  1. Stage a "new" daemon binary that is actually the same binary (tests the rename/restart).
  2. Send `ApplyUpdate`.
  3. Verify daemon restarts and serves IPC again.
  4. Stage a binary that crashes on startup (e.g., `exit(1)`).
  5. Send `ApplyUpdate`.
  6. Verify daemon restarts, detects crash loop, rolls back, and serves IPC again.

## Consequences

### Positive
- A single background process owns all control-plane state, eliminating the duplicate connection pattern where every CLI instance opens its own WebSocket.
- TURN relay provides NAT traversal for users on symmetric NATs and restrictive firewalls.
- TUF-based auto-update keeps the daemon current without manual intervention.
- IPC surface is fully specified, enabling the GUI to be developed independently.
- redb provides ACID persistence with zero native dependencies.
- Every new component is additive; existing binaries remain unchanged.

### Negative
- The daemon adds ~5 MB to the installation size (Tokio, redb, tough, reqwest).
- TURN relay adds 30-80 ms of latency for relayed sessions.
- Self-update on Windows requires the helper `qubox-daemon-switch.exe` pattern, adding complexity.
- The daemon subprocess pattern for host-agent/client-cli means two processes for one session (daemon + host-agent or daemon + client-cli). Memory overhead: ~30 MB per process.

### Risks
- **quinn's custom UdpSocket trait**: if quinn 0.11 does not expose a publicly implementable `AsyncUdpSocket`, the loopback UDP proxy approach is a workaround but adds an extra socket pair. Verify during implementation.
- **Windows self-update rename**: the helper process approach was chosen over reboot-based replacement because it's more user-friendly. The helper must have permission to write to `C:\Program Files\Qubox\`. This is guaranteed by the installer (grants Users group MODIFY).
- **coturn interop**: the HMAC credential format must match coturn's `static-auth-secret` computation exactly. The unit test must verify against a real coturn instance.
- **TUF initial trust**: the first `root.json` is embedded in the binary. If the root expires or is revoked, the daemon cannot update without a manual intervention (reinstall). Mitigation: ship root.json with a long expiry (3+ years) and instruct the user to renew via a signed root update before expiry.

## Open Questions

1. **Quinn UdpSocket trait**: Does quinn 0.11's `Endpoint::new_with_udp_socket` or equivalent accept a user-provided `AsyncUdpSocket` implementation? The backend-architect must verify. If not, use the loopback proxy (Section 6.2, Option A).

2. **Windows service identity**: The installer runs as the user. Does the SCM accept a user-mode service without a stored password when the user is not logged in? If not, the service must run as `LocalService` and access user data via `KnownFolder` APIs. Alternatively, use `Task Scheduler` (run when user logs on) instead of SCM. The backend-architect resolves this during Windows implementation.

3. **TUF repository structure**: What is the base URL and directory layout of the TUF repository? The decision is deferred to the `devops-automator` role. This ADR assumes `{repo_url}/{metadata_filename}` (flat layout, which `tough` supports).

4. **coturn deployment**: Self-hosted or Cloudflare Calls? The decision is deployment-specific and belongs to the `devops-automator`. This ADR designs the credential issuance and client integration to work with any standard TURN server.

5. **macOS code signing**: The LaunchAgent may not load on macOS 14+ without a signed + notarized binary. This is tracked in P2-19 (signed binaries). For Phase 1 development on macOS, the user can load the LaunchAgent with `launchctl load` bypassing Gatekeeper.

6. **Multiple concurrent sessions**: Phase 1 does not support multiple concurrent host or client sessions from the same daemon. The daemon tracks exactly one host session and one client session at a time. Multi-session is deferred to a follow-up.

## References

- RFC 8656: TURN (Traversal Using Relays around NAT). https://datatracker.ietf.org/doc/rfc8656/
- RFC 6062: TURN over TCP/TLS. https://datatracker.ietf.org/doc/rfc6062/
- coturn: https://github.com/coturn/coturn
- tough (TUF): https://crates.io/crates/tough
- redb: https://docs.rs/redb
- windows-service crate: https://docs.rs/windows-service
- systemd.service: https://www.freedesktop.org/software/systemd/man/systemd.service.html
- systemd.socket: https://www.freedesktop.org/software/systemd/man/systemd.socket.html
- directories crate: https://docs.rs/directories
- P1-13 daemon research doc: `research/roadmap/p1-13-daemon.md`
- P1-11 TURN research doc: `research/roadmap/p1-11-turn.md`
- ADR-001 Transport research baseline: `research/decisions/ADR-001-transport-research-baseline.md`
- ADR-002 Target architecture: `research/decisions/ADR-002-target-architecture-and-upgrade-strategy.md`
