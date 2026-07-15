# ADR-006 Daemon Delegation: PID Management, Subprocess Lifecycle, Collision Detection

## Status

Accepted.

## Context

Phase 1 of the qubox roadmap (P1-13 daemon + P1-11 TURN) is being finalized. ADR-005 established the `qubox-daemon` process model, IPC surface, state persistence via `redb`, TUF auto-update, and TURN credential issuance. Tasks 1–5 have been implemented — the daemon skeleton, IPC server with 20-byte binary frame protocol, `redb` state database with full table schema, WebSocket signaling connection with auto-reconnect, TUF `UpdateChecker` with rollback detection, and TURN `TurnClient` with UDP/TCP support.

Six gaps remain in the daemon lifecycle and foreground process integration. These gaps are documented in this ADR and form the scope of Task 6.

### Gap 1: PID file (Unix)

The daemon does not write a PID file. Tools like `monit`, custom scripts, and the `status` subcommand have no reliable way to discover the daemon's PID. On systems where the service manager (systemd, launchd) tracks the PID natively, the PID file should be skipped. On all other runs, the PID file must be created atomically and cleaned up on shutdown.

### Gap 2: Standalone collision detection

When a user runs `host-agent` or `client-cli` without `--use-daemon` while the daemon is already running, two independent signaling connections compete. The daemon's signaling connection is the authoritative one; the foreground process should detect the collision and fail closed with a clear diagnostic, or gracefully delegate when `--use-daemon` is passed.

### Gap 3: Windows named pipe DACL

The daemon's Windows named pipe `\\.\pipe\Qubox` currently uses default ACL. When run as `LocalService` (isolated in session 0), the pipe is not reachable from the user's session (session 1+). A correct DACL must grant access to `LocalSystem`, `LocalService`, and `Authenticated Users`, while denying `Interactive` as defense-in-depth.

### Gap 4: Media pipeline subprocess lifecycle

The daemon's `StartHost` / `StartClient` IPC handlers are stubs. They must spawn the foreground binary as a managed child process: pipe stdout/stderr to `tracing`, monitor exit via `waitpid`, implement throttled restart on crash, and support clean kill via `StopHost` / `StopClient` with SIGTERM → SIGKILL grace period.

### Gap 5: TUF repository URL decoupling

The TUF `UpdateChecker` reads `QUBOX_UPDATE_REPO` at startup. There is no CLI flag to override the URL, no way to persist a user-chosen URL across restarts, and the deprecated `QUBOX_UPDATE_REPO` env var (without the second underscore) is not recognized.

### Gap 6: State database auto-init

ADR-005 assumed `StateDb::open` creates the database and tables on first access. This has been verified in the implementation. No changes are needed.

### Out of scope (not addressed by this ADR)

- Full GUI integration (the GUI is developed independently against the IPC surface).
- Multiple concurrent host or client sessions (deferred to a follow-up; ADR-005 §Open Questions item 6).
- Self-update mechanics (covered by ADR-005 §4).
- TURN credential issuance (covered by ADR-005 §5).
- QUIC-over-TURN transport (covered by ADR-005 §6).
- macOS code signing (tracked as P2-19).
- Systemd socket activation (covered by ADR-005 §1.5 and implemented in `socket_activation.rs`).

## Decision

### 1. PID File Management (Unix)

#### 1.1 Platform paths

| Platform | Condition | PID file path |
|----------|-----------|---------------|
| Linux    | root (UID 0) | `/run/qubox.pid` |
| Linux    | user, `XDG_RUNTIME_DIR` set | `$XDG_RUNTIME_DIR/qubox.pid` |
| Linux    | user, `XDG_RUNTIME_DIR` unset | `~/.local/share/qubox/qubox-daemon.pid` |
| macOS    | any | `~/Library/Application Support/qubox/daemon.pid` |
| Windows  | any | **No PID file** (SCM tracks PIDs natively) |

The path is resolved at daemon startup and stored in `DaemonConfig.pid_file_path: Option<PathBuf>`. `None` means "do not write a PID file" (Windows always `None`, systemd/launchd-managed runs return `None`).

#### 1.2 Skip detection (service-managed)

The PID file is the mechanism for tools that do NOT use the service manager. When a service manager is active, the PID file is superfluous because the manager already tracks the process:

- **Linux**: if the environment variable `INVOCATION_ID` (set by systemd for every unit) is present, skip PID file creation. Use `std::env::var("INVOCATION_ID").is_ok()`.
- **macOS**: if `LAUNCHD_JOB` (set by launchd) is present, skip. Use `std::env::var("LAUNCHD_JOB").is_ok()`.
- **Windows**: always skip (SCM integration path `service_scm::run_scm` never calls PID file logic).

When the daemon detects a service manager, `DaemonConfig.pid_file_path` is set to `None`.

#### 1.3 Write protocol

```
fn write_pid_file(path: &Path) -> io::Result<()>
```

1. Create parent directories with `std::fs::create_dir_all(path.parent())`.
2. Write the PID as `"{}\n"` formatted ASCII (`std::process::id()`) to a temporary file at `path.with_extension("pid.tmp")`.
3. Atomically rename the temp file over the target path: `std::fs::rename(tmp_path, path)`. This prevents readers from seeing a half-written file on crash.
4. File permissions: `0o644` on Unix (world-readable; the PID is not secret).
5. On success, log `info!("pid file written to {}", path.display())`.

#### 1.4 Stale PID detection

Before writing, the daemon checks for a stale PID file:

```
pub fn check_and_clean_stale_pid_file(path: &Path) -> io::Result<bool>
```

Returns `true` if a stale PID file was removed, `false` if no file existed or the process was alive.

Implementation:
1. Read the file content via `std::fs::read_to_string(path)`. If the file doesn't exist, return `Ok(false)`.
2. Parse the first line as `u32` (PID). If parse fails, treat the file as corrupt, remove it, return `Ok(true)`.
3. Check whether a process with that PID is alive:
   - **Linux**: `path = format!("/proc/{pid}/exe")`, check `path.exists()`. If the path exists AND resolves to a file, the process is alive. If the path is a broken symlink or missing, the process is dead.
   - **macOS / other Unix**: `unsafe { libc::kill(pid as i32, 0) }`. Returns `0` if the process exists (signal `0` is a null check, no signal is sent). If `kill` returns `-1` with `errno == ESRCH`, the process is dead.
   - **Windows**: `OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, FALSE, pid)` — if `NULL`, process is dead.
4. If the process is alive AND the PID matches our own PID (stale file from a previous instance that died but the PID was reused), remove the file and return `Ok(true)`.
5. If the process is alive AND the PID does NOT match our own, return `Ok(false)` (another instance is running — should we refuse to start? Decision: log a `warn!` but continue; the file will be overwritten).
6. If the process is dead, remove the file, return `Ok(true)`.

#### 1.5 Unlink on shutdown

On clean shutdown, the daemon calls:

```
fn remove_pid_file(path: &Path) {
    if path.exists() {
        std::fs::remove_file(path).ok();
        info!("pid file removed");
    }
}
```

Called from `Daemon::run` after the selected block completes (both success and error paths). On crash, the PID file is not cleaned up — the stale detection on the next startup handles it.

### 2. Subprocess Management (Media Pipeline Lifecycle)

#### 2.1 The `HostManager` / `ClientManager` structs

Two new structs in `apps/daemon/src/`:

```
HostManager {
    state: Arc<StateDb>,
    host_state: HostManagedState,
}

ClientManager {
    state: Arc<StateDb>,
    client_state: ClientManagedState,
}

HostManagedState {
    child_pid: Option<u32>,
    child_handle: Option<tokio::process::Child>,
    session_id: Option<String>,
    spawn_times: VecDeque<Instant>,  // max 60s window
    event_tx: broadcast::Sender<IpcEvent>,
}
```

`ClientManagedState` is identical. `HostManagedState` is stored in-memory only; the `redb` `HostState` record stores persistent fields (last session ID, config hash, peer ID).

#### 2.2 Spawn protocol

On receiving `IpcRequest::StartHost { config }`:

1. Validate: if `host_state.running` is `true`, respond `IpcError::HostAlreadyRunning`.
2. Construct the `tokio::process::Command`:
    ```
    let binary_path = std::env::current_exe()?
        .parent()?                          // e.g. /usr/bin/
        .join("qubox-host-agent");  // or qubox-client-cli

    let mut cmd = tokio::process::Command::new(&binary_path);
    cmd.arg("--allow-standalone")         // skip daemon check (we ARE the daemon)
       .arg("--ipc-socket")
       .arg(&config.socket_path)           // the daemon's own IPC socket
       .arg("--server")
       .arg(&config.signaling_url)         // daemon's signaling URL
       .stdout(Stdio::piped())
       .stderr(Stdio::piped());
    ```
3. Forward media-pipeline args from `HostConfig` (see §3 for the full struct):
    ```
    if let Some(name) = &config.host_name {
        cmd.arg("--name").arg(name);
    }
    if let Some(path) = &config.identity_path {
        cmd.arg("--identity-path").arg(path);
    }
    if config.auto_approve_pairing {
        cmd.arg("--auto-approve-pairing");
    }
    if let Some(display) = &config.x11_display {
        cmd.arg("--x11-display").arg(display);
    }
    cmd.arg("--media-width").arg(config.media_width.to_string());
    cmd.arg("--media-height").arg(config.media_height.to_string());
    cmd.arg("--media-fps").arg(config.media_fps.to_string());
    cmd.arg("--media-bitrate-kbps").arg(config.media_bitrate_kbps.to_string());
    cmd.arg("--codec").arg(format!("{:?}", config.codec));
    cmd.arg("--encoder").arg(config.encoder.to_cli_label());
    if let Some(h264) = &config.h264_encoder {
        cmd.arg("--h264-encoder").arg(h264);
    }
    if config.datagram_media {
        cmd.arg("--datagram-media");
    }
    if let Some(turn) = &config.turn_server {
        cmd.arg("--turn-server").arg(turn);
    }
    if let Some(user) = &config.turn_username {
        cmd.arg("--turn-username").arg(user);
    }
    if let Some(pass) = &config.turn_password {
        cmd.arg("--turn-password").arg(pass);
    }
    if config.turn_only {
        cmd.arg("--turn-only");
    }
    if config.turn_force {
        cmd.arg("--turn-force");
    }
    if let Some(d) = config.display {
        cmd.arg("--display").arg(d.to_string());
    }
    ```
4. **DO NOT** forward `--use-daemon` (the spawned binary must NOT try to re-contact the daemon — `--allow-standalone` is the mutual exclusion).
5. Spawn: `let child = cmd.spawn()?`.
6. Record the PID: `state.host_state.last_child_pid = Some(child.id())`.
7. Set `host_state.running = true`, `host_state.session_id = Some(new_session_id)` (generated by the daemon, sent to the host-agent via `--session-id` flag or via the signaling WebSocket — the daemon owns signaling).

   **Clarification on session ID flow**: The daemon generates the session ID (UUID v4) and passes it to the host-agent via a new `--session-id <uuid>` flag. This flag is added to the host-agent CLI for use when spawned by the daemon. It is not exposed to end users. The host-agent sends the session ID in its `Hello` message.
8. Pipe stdout/stderr to `tracing`:
    ```
    let stdout = child.stdout.take().unwrap();
    let stderr = child.stderr.take().unwrap();
    tokio::spawn(pipe_child_output(stdout, "host-agent", tracing::Level::INFO));
    tokio::spawn(pipe_child_output(stderr, "host-agent", tracing::Level::WARN));
    ```
    Where `pipe_child_output` reads lines from the pipe and emits them as `tracing` events with a `target: "host-agent"` metadata:
    ```
    async fn pipe_child_output<R: AsyncRead + Unpin>(mut reader: R, label: &'static str, level: tracing::Level) {
        use tokio::io::AsyncBufReadExt;
        let mut reader = tokio::io::BufReader::new(&mut reader);
        let mut line = String::new();
        while reader.read_line(&mut line).await.unwrap_or(0) > 0 {
            tracing::event!(level, target: label, "{}", line.trim_end());
            line.clear();
        }
    }
    ```

#### 2.3 Exit monitoring

```
let exit_status = child.wait().await?;  // tokio::process::Child::wait
```

Map `ExitStatus`:

| Platform | ExitStatus field | Extraction |
|----------|-----------------|------------|
| Unix     | `exit_status.code()` | `Option<i32>` — `Some(n)` for normal exit, `None` for signal-killed |
| Unix     | `exit_status.signal()` | `Option<i32>` — `Some(sig)` if killed by signal |
| Windows  | `exit_status.code()` | `Option<i32>` — always `Some(u32 as i32)` for `GetExitCodeProcess` |

Reason string:
```
fn exit_reason(status: &ExitStatus) -> String {
    #[cfg(unix)] {
        if let Some(sig) = status.signal() {
            return format!("killed by signal {sig}");
        }
    }
    match status.code() {
        Some(0) => "exited successfully".into(),
        Some(n) => format!("exited with code {n}"),
        None => "exited with unknown status".into(),
    }
}
```

On exit:
1. Clear `host_state.child_pid`.
2. Log at `info!` if exit code 0, `warn!` if non-zero.
3. Emit `IpcEvent::HostStateChanged { running: false, session_id: None, child_pid: None, last_exit_code: status.code(), last_exit_reason: Some(reason) }`.
4. If exit code is non-zero AND the host was not intentionally stopped (`host_state.intentional_stop` flag is false), enter the restart throttling logic.

#### 2.4 Restart throttling

```
const MAX_RESTARTS: usize = 3;
const RESTART_WINDOW_SECS: u64 = 60;
const BACKOFF_BASE_SECS: u64 = 1;

fn should_restart(spawn_times: &mut VecDeque<Instant>) -> Option<Duration> {
    let now = Instant::now();
    // Prune entries older than 60s
    while let Some(&t) = spawn_times.front() {
        if now.duration_since(t).as_secs() > RESTART_WINDOW_SECS {
            spawn_times.pop_front();
        } else {
            break;
        }
    }
    if spawn_times.len() >= MAX_RESTARTS {
        return None;  // give up
    }
    let restart_count = spawn_times.len();
    let delay = Duration::from_secs(BACKOFF_BASE_SECS * 2u64.pow(restart_count as u32));
    spawn_times.push_back(now);
    Some(delay)
}
```

When `should_restart` returns `None`, emit `IpcEvent::HostStateChanged { running: false, ..., last_exit_reason: Some("restart limit exceeded (3 in 60s)".into()) }` and do NOT respawn.

When `should_restart` returns `Some(delay)`, `tokio::time::sleep(delay).await` then call the spawn logic again.

#### 2.5 Kill protocol (on `StopHost` / `StopClient`)

1. Set `host_state.intentional_stop = true` (so exit monitoring does not trigger restart).
2. On Unix: `child.start_kill()` sends SIGTERM (default, configurable via `std::process::Stdio`). Tokio's `Child::start_kill` calls `libc::kill(pid, SIGTERM)`.
3. Wait with 5-second grace: `let result = tokio::time::timeout(Duration::from_secs(5), child.wait()).await`.
4. On timeout: force kill. On Unix: `child.start_kill()` again with SIGKILL (tokio's `Child::start_kill` sends SIGKILL on the second call). On Windows: `kill()` which calls `TerminateProcess`.
5. On Windows: `child.start_kill()` calls `TerminateProcess(handle, 1)`. Tokio's implementation sends `CTRL_BREAK_EVENT` on first call and `TerminateProcess` on second. Use the same pattern: first `start_kill()` (graceful), wait 5s, second `start_kill()` (forceful).
6. Log at `info!` the kill method used and the final exit status.

### 3. Standalone Collision Detection

#### 3.1 The probe function

```
/// Try to connect to the daemon's IPC socket and send a Ping.
/// Returns true if the daemon responds with Pong within the timeout.
pub async fn check_daemon_running(socket_path: &Path) -> bool {
```

Implementation:
1. Connect to the socket with a 200ms timeout:
   ```
   let connect_fut = IpcStream::connect(socket_path);
   let mut stream = match tokio::time::timeout(Duration::from_millis(200), connect_fut).await {
       Ok(Ok(s)) => s,
       _ => return false,   // connection failed or timed out
   };
   ```
2. Send a `Ping` request frame (20-byte header with `kind=Request`, `correlation_id=1`, bincode `IpcRequest::Ping` payload):
   ```
   let ping_bytes = bincode::serialize(&IpcRequest::Ping).unwrap();
   write_frame(&mut stream, 1 /* correlation_id */, Kind::Request, &ping_bytes).await;
   ```
3. Await a response with 200ms timeout:
   ```
   let read_fut = read_frame(&mut stream);
   match tokio::time::timeout(Duration::from_millis(200), read_fut).await {
       Ok(Ok(frame)) => {
           if frame.kind == Kind::Response {
               let resp: IpcResponse = bincode::deserialize(&frame.payload).unwrap();
               matches!(resp, IpcResponse::Pong)
           } else { false }
       }
       _ => false,
   }
   ```
4. On success (Pong received): return `true`. On any failure: return `false`.

#### 3.2 Behavior in `host-agent` and `client-cli`

Both binaries gain three new CLI flags:

```
/// Delegate to the daemon via IPC. The foreground process sends
/// StartHost / StartClient and exits.
#[arg(long)]
use_daemon: bool,

/// Allow running standalone even if the daemon is running.
/// Skips the daemon probe entirely.
#[arg(long)]
allow_standalone: bool,

/// Path to the daemon's IPC socket. Overrides the default
/// platform-specific path. Used internally when the daemon
/// spawns the foreground binary.
#[arg(long)]
ipc_socket: Option<PathBuf>,
```

The logic at the start of `main()`:

```
async fn check_daemon_and_maybe_delegate(args: &Args) -> Result<Option<Delegation>> {
    let socket_path = args.ipc_socket.clone()
        .unwrap_or_else(default_daemon_socket_path);

    if args.allow_standalone {
        // Skip check entirely; run in-process.
        return Ok(None);
    }

    if args.use_daemon {
        // Must connect. Fail hard if daemon is not running.
        if !check_daemon_running(&socket_path).await {
            eprintln!("error: --use-daemon specified but daemon is not running at {socket_path:?}");
            process::exit(3);
        }
        return Ok(Some(Delegation::DelegateToDaemon { socket_path }));
    }

    // Default (no flag): probe.
    if check_daemon_running(&socket_path).await {
        tracing::warn!(
            "daemon already running at {}; \
             pass --use-daemon to delegate, or --allow-standalone to override",
            socket_path.display()
        );
        eprintln!(
            "error: daemon already running at {}.\n\
             Pass --use-daemon to delegate session management to the daemon,\n\
             or --allow-standalone to run standalone anyway (not recommended).",
            socket_path.display()
        );
        process::exit(2);
    }

    Ok(None)  // daemon not running; run in-process as today
}
```

#### 3.3 Delegate flow

When `use_daemon` is true and the probe succeeds:

1. **host-agent**: connect to the daemon IPC socket, send `IpcRequest::StartHost { config }` (where `config` is built from the host-agent's CLI args), await `IpcResponse::Unit` (indicating the daemon successfully spawned the subprocess), then exit 0.
2. **client-cli**: similar but sends `IpcRequest::StartClient { config }`, awaits `IpcResponse::Unit`, then exit 0.

If the daemon responds with an error (e.g., `HostAlreadyRunning`), print the error and exit 1.

#### 3.4 Default socket path resolution

```
fn default_daemon_socket_path() -> PathBuf {
    #[cfg(target_os = "linux")]
    {
        std::env::var("XDG_RUNTIME_DIR")
            .map(|d| PathBuf::from(d).join("qubox.sock"))
            .unwrap_or_else(|_| {
                directories::ProjectDirs::from("com", "qubox", "qubox")
                    .map(|d| d.data_local_dir().join("run").join("qubox.sock"))
                    .unwrap_or_else(|| PathBuf::from("/run/user/1000/qubox.sock"))
            })
    }
    #[cfg(target_os = "macos")]
    {
        directories::ProjectDirs::from("com", "qubox", "qubox")
            .map(|d| d.data_local_dir().join("daemon.sock"))
            .unwrap_or_else(|| {
                PathBuf::from("~/Library/Application Support/com.qubox.daemon/daemon.sock")
            })
    }
    #[cfg(windows)]
    {
        PathBuf::from(r"\\.\pipe\Qubox")
    }
}
```

This must be consistent with the daemon's `DaemonConfig::default()` socket path derivation.

### 4. IPC Schema Additions

#### 4.1 Extended `HostConfig` struct

The current `HostConfig` in `ipc.rs` is minimal:
```
struct HostConfig {
    identity_path: Option<String>,
    auto_approve_pairing: bool,
}
```

This is extended to match the full ADR-005 §2.5 `HostConfig` definition (which ADR-005 listed but was not fully implemented):

```
struct HostConfig {
    // Existing
    identity_path: Option<String>,
    auto_approve_pairing: bool,

    // NEW — media pipeline config
    host_name: Option<String>,             // --name
    signaling_url: String,                 // --server (the daemon's own WS URL)
    x11_display: Option<String>,           // --x11-display
    media_width: u32,                      // default 1920
    media_height: u32,                     // default 1080
    media_fps: u32,                        // default 60
    media_bitrate_kbps: u32,               // default 20000
    codec: VideoCodec,                     // H264 | H265 | Av1
    encoder: EncoderBackend,               // Auto | Software | Nvenc | Vaapi | Qsv | Amf | VideoToolbox
    h264_encoder: Option<String>,          // --h264-encoder override name
    datagram_media: bool,                  // --datagram-media
    turn_server: Option<String>,           // --turn-server
    turn_username: Option<String>,         // --turn-username
    turn_password: Option<String>,         // --turn-password
    turn_only: bool,                       // --turn-only
    turn_force: bool,                      // --turn-force
    display: Option<u32>,                  // --display
}
```

Where `VideoCodec` and `EncoderBackend` are existing enums from `qubox-proto` / `qubox-media`, serialized as strings (or as enum variant indices in bincode — either is compatible as long as both ends use the same schema).

#### 4.2 Extended `ClientConfig` struct

```
struct ClientConfig {
    identity_path: Option<String>,
    host_peer_id: Option<String>,          // existing
    host_name: Option<String>,
    signaling_url: String,
    transport: Option<String>,             // "native_quic", "web_rtc", "relay_quic"
    codec: Option<VideoCodec>,
    decoder: Option<String>,               // --decoder override
    resolution: Option<String>,            // WxH
    framerate: u32,
    bitrate_kbps: Option<u32>,
    max_bitrate_kbps: Option<u32>,
    scale_mode: String,                    // "fit", "fill", "crop", "native"
    mouse_mode: String,                    // "relative", "absolute"
    datagram_media: bool,
    use_hw_decode: bool,
    capture_gamepad: bool,
    turn_server: Option<String>,
    turn_username: Option<String>,
    turn_password: Option<String>,
    turn_only: bool,
    turn_force: bool,
}
```

#### 4.3 Extended `IpcEvent::HostStateChanged`

The existing `HostStateChanged` variant has two fields. It gains three more:

```
IpcEvent::HostStateChanged {
    running: bool,
    session_id: Option<String>,
    child_pid: Option<u32>,              // NEW: PID of the spawned child, None when idle
    last_exit_code: Option<i32>,         // NEW: None if never exited, Some(code) on exit
    last_exit_reason: Option<String>,    // NEW: human-readable reason
}
```

Same extension for `IpcEvent::ClientStateChanged`.

This allows GUI clients to display a process status indicator and last-crash reason.

#### 4.4 New `IpcRequest::GetChildProcessStatus`

Added to support the GUI polling the child state without subscribing to events:

```
IpcRequest::GetHostStatus   → IpcResponse::HostStatus { running, session_id, child_pid, last_exit_code, last_exit_reason }
IpcRequest::GetClientStatus → IpcResponse::ClientStatus { running, session_id, child_pid, last_exit_code, last_exit_reason }
```

The response types `HostStatus` and `ClientStatus` gain the same three new fields.

#### 4.5 Wire format

No changes to the 20-byte header. All additions are within the bincode payload, which is backward-compatible because bincode tagged enums tolerate new variants appended at the end, and structs with `#[serde(default)]` fields ignore unknown fields.

### 5. Windows Named Pipe DACL

#### 5.1 Security descriptor construction

The daemon on Windows creates the named pipe with a custom `SECURITY_ATTRIBUTES` that restricts access to specific SIDs.

Use the SDDL string for clarity and portability:

```
"D:(A;;GRGW;;;S-1-5-18)(A;;GRGW;;;S-1-5-19)(A;;GRGW;;;S-1-5-11)(D;;WD;;;S-1-5-4)"
```

Breakdown:

| ACE | Type | Rights | SID | Meaning |
|-----|------|--------|-----|---------|
| `A;;GRGW;;;S-1-5-18` | Allow | GENERIC_READ \| GENERIC_WRITE | S-1-5-18 (LocalSystem) | System account has full pipe access |
| `A;;GRGW;;;S-1-5-19` | Allow | GENERIC_READ \| GENERIC_WRITE | S-1-5-19 (LocalService) | The daemon itself (runs as LocalService) |
| `A;;GRGW;;;S-1-5-11` | Allow | GENERIC_READ \| GENERIC_WRITE | S-1-5-11 (Authenticated Users) | All authenticated user-session processes |
| `D;;WD;;;S-1-5-4` | Deny | WRITE_DAC (WD) | S-1-5-4 (Interactive) | Defense-in-depth: interactive logon sessions cannot modify the DACL |

The SDDL string is converted to a `SECURITY_DESCRIPTOR` via `ConvertStringSecurityDescriptorToSecurityDescriptorW` (from the `windows` crate, `Win32_Security::ConvertStringSecurityDescriptorToSecurityDescriptorW`).

#### 5.2 Pipe creation

```
use windows::Win32::System::Threading::CreateNamedPipeW;
use windows::Win32::Storage::FileSystem::PIPE_ACCESS_DUPLEX;
use windows::Win32::System::Pipes::PIPE_TYPE_MESSAGE;
use windows::Win32::Security::{
    ConvertStringSecurityDescriptorToSecurityDescriptorW,
    SDDL_REVISION_1,
    SECURITY_ATTRIBUTES,
};

fn create_secure_pipe() -> io::Result<OwnedHandle> {
    let sddl = "D:(A;;GRGW;;;S-1-5-18)(A;;GRGW;;;S-1-5-19)(A;;GRGW;;;S-1-5-11)(D;;WD;;;S-1-5-4)";
    let mut sd = SECURITY_DESCRIPTOR::default();
    unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            windows::core::w!("D:(A;;GRGW;;;S-1-5-18)(A;;GRGW;;;S-1-5-19)(A;;GRGW;;;S-1-5-11)(D;;WD;;;S-1-5-4)"),
            SDDL_REVISION_1,
            &mut sd as *mut _ as *mut *mut _,
            None,
        )?;
    }
    let mut sa = SECURITY_ATTRIBUTES {
        nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: sd.0 as *mut _,
        bInheritHandle: false.into(),
    };
    let handle = unsafe {
        CreateNamedPipeW(
            w!("\\.\pipe\Qubox"),
            PIPE_ACCESS_DUPLEX.0,
            PIPE_TYPE_MESSAGE.0 | 0x01, // PIPE_READMODE_MESSAGE
            1,      // max instances
            4096,   // out buffer
            4096,   // in buffer
            0,      // default timeout
            Some(&sa),
        )
    };
    // ... error handling ...
}
```

If the SDDL approach is simpler, use `interprocess` crate's `local_socket::tokio::Listener` with a custom `SecurityAttributes` parameter (check the `interprocess` API). Alternatively, use the `windows-service` crate's pipe creation with a security descriptor parameter.

#### 5.3 Auth verification

After accepting a named pipe connection, the daemon calls `GetNamedPipeClientProcessId` to retrieve the client's PID, then opens the process handle via `OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, FALSE, pid)` and calls `OpenProcessToken` → `GetTokenInformation(TokenUser)` to verify the client's user SID matches the daemon's user SID. If the SID does not match, the connection is closed with `IpcError::AccessDenied`.

This is an additional defense layer beyond the DACL — the DACL ensures the pipe is not accessible to non-authorized users at the OS level; the runtime check ensures that even if the DACL is misconfigured, only same-user processes can communicate.

### 6. TUF Repository URL Decoupling

#### 6.1 New CLI flag

The daemon's `Run` and `ServiceRun` subcommands gain:

```
/// TUF update repository base URL.
/// Overrides QUBOX_UPDATE_REPO env var.
#[arg(long, env = "QUBOX_UPDATE_REPO")]
update_repo: Option<String>,
```

The env var declaration `env = "QUBOX_UPDATE_REPO"` means clap reads the env var automatically; `--update-repo` on the CLI overrides it.

#### 6.2 Deprecated env var

The old env var name `QUBOX_UPDATE_REPO` (no second underscore after BETTER) is checked manually after clap parsing:

```
fn resolve_update_repo(cli_value: Option<String>) -> Option<String> {
    // Priority: 1. CLI flag  2. QUBOX_UPDATE_REPO  3. QUBOX_UPDATE_REPO (deprecated)
    if let Some(url) = cli_value {
        return Some(url);
    }
    if let Ok(url) = std::env::var("QUBOX_UPDATE_REPO") {
        return Some(url);
    }
    if let Ok(url) = std::env::var("QUBOX_UPDATE_REPO") {
        tracing::warn!(
            "QUBOX_UPDATE_REPO is deprecated, use QUBOX_UPDATE_REPO instead"
        );
        return Some(url);
    }
    None
}
```

#### 6.3 Persistence

The resolved URL is stored in the `settings` table under key `"update_repo"`:

```
// After resolving the URL:
if let Some(url) = &resolved_url {
    state.set_setting("update_repo", url)?;
}
```

On startup, the persistent value takes priority over the env var / CLI:

```
fn load_update_repo(state: &StateDb, cli_value: Option<String>) -> Option<String> {
    // Priority: 1. persistent (redb)  2. CLI flag  3. new env var  4. deprecated env var
    if let Ok(Some(persisted)) = state.get_setting("update_repo") {
        return Some(persisted);
    }
    resolve_update_repo(cli_value)
}
```

Rationale: the user may set the URL via `--update-repo` once, and the daemon remembers it. On subsequent restarts (where the systemd service file may not include the CLI flag), the persisted value is used.

#### 6.4 `UpdateChecker::new` integration

The resolved URL is passed to `UpdateChecker::new`:

```
let repo_url = load_update_repo(&state, config.update_repo);
let update_checker = repo_url.map(|url| {
    UpdateChecker::new(url, state.clone(), env!("CARGO_PKG_VERSION").to_string())
}).transpose()?;
```

If no URL is resolved (all sources are `None`), `UpdateChecker` is not constructed, and TUF update is not available. The `CheckUpdate` IPC returns `IpcError::UpdateFailed { reason: "no update repository configured" }`.

#### 6.5 `settings` table key

New well-known key in the `settings` table:

| Key | Value | Description |
|-----|-------|-------------|
| `"update_repo"` | URL string | Persisted TUF repository URL |

### 7. Auto-start `state.db` Init (Verification)

`StateDb::open` already creates the database file and all 11 tables on first access. The implementation at `apps/daemon/src/state.rs:93-130` confirms:

1. `redb::Database::create(path)` creates the file if absent.
2. A write transaction opens all 11 `TableDefinition`s, creating them if absent.
3. Schema version is read; if absent or `0`, it is set to `1`.

No changes needed. This ADR records the verification.

### 8. Process Topologies

Three distinct process topologies exist depending on the user's flags and whether the daemon is running.

#### Topology A — Standalone (no daemon)

```
┌─────────────────────────────────────────────────┐
│ $ host-agent --server ws://... --name test      │
│                                                 │
│  ┌──────────────────────────────────────────┐   │
│  │ host-agent process                        │   │
│  │   ├── signaling WebSocket (direct)        │   │
│  │   ├── QUIC endpoint (in-process)          │   │
│  │   ├── ffmpeg capture subprocess           │   │
│  │   └── media pipeline (in-process)         │   │
│  └──────────────────────────────────────────┘   │
└─────────────────────────────────────────────────┘
```

**Applies when**: `--use-daemon` not passed, `--allow-standalone` not passed, daemon socket probe returns `false`. OR `--allow-standalone` is passed explicitly.

**Behavior**: Identical to the pre-daemon architecture. The foreground process opens its own WebSocket, manages pairing, and runs the media pipeline. The daemon is not involved.

#### Topology B — Daemon-managed foreground shim

```
┌────────────────────────────────────────────────────┐
│ $ host-agent --use-daemon --server ws://...        │
│                                                    │
│  ┌──────────────────────┐     ┌──────────────────┐ │
│  │ host-agent process   │────→│ qubox-daemon   │ │
│  │ (CLI shim)           │ IPC │   (background)    │ │
│  │   Send StartHost ────│────→│   Owns signaling  │ │
│  │   Exit 0             │     │   Owns pairings   │ │
│  └──────────────────────┘     │                   │ │
│                               │   Spawns:         │ │
│                               │ ┌────────────────┐│ │
│                               │ │ host-agent     ││ │
│                               │ │ --allow-standalone││
│                               │ │ --ipc-socket   ││ │
│                               │ │ --session-id   ││ │
│                               │ │ (media pipeline)││ │
│                               │ │ stdout/stderr  ││ │
│                               │ │  → tracing     ││ │
│                               │ │ waitpid/restart ││ │
│                               │ └────────────────┘│ │
│                               └──────────────────┘  │
└────────────────────────────────────────────────────┘
```

**Applies when**: `--use-daemon` is passed, or (future default) the daemon socket probe returns `true` and the delegation is automatic.

**Behavior**:
1. Foreground shim: sends `StartHost` / `StartClient` to the daemon, receives acknowledgment, exits.
2. Daemon: spawns the foreground binary as a child process with `--allow-standalone` (so it does NOT try to re-contact the daemon) plus all the original media pipeline args.
3. Daemon: pipes child's stdout/stderr to tracing, monitors via `waitpid`.
4. Daemon: throttled restart on crash (3 in 60s with exponential backoff: 1s, 2s, 4s).
5. Daemon: clean kill on `StopHost` / `StopClient` (SIGTERM → wait 5s → SIGKILL).
6. Daemon: broadcasts `IpcEvent::HostStateChanged` / `ClientStateChanged` on every state change.

#### Topology C — Standalone + daemon running (collision)

```
┌────────────────────────────────────────────────────┐
│ $ host-agent --server ws://... --name test         │
│   (no --use-daemon, no --allow-standalone)          │
│                                                    │
│  ┌──────────────────────┐     ┌──────────────────┐ │
│  │ host-agent process   │     │ qubox-daemon   │ │
│  │   Probe socket ──────│────→│   already running │ │
│  │   ← Pong             │     │                   │ │
│  │                      │     │   Owns signaling  │ │
│  │   WARN: daemon       │     │   Owns pairings   │ │
│  │   already running    │     │                   │ │
│  │   at /run/...        │     │                   │ │
│  │                      │     │                   │ │
│  │   exit code 2        │     │   (unchanged)     │ │
│  └──────────────────────┘     └──────────────────┘  │
└────────────────────────────────────────────────────┘
```

**Applies when**: daemon is running, user runs `host-agent` / `client-cli` with no delegation flags.

**Behavior**:
1. Foreground process fails closed with exit code 2.
2. Diagnostic message: "daemon already running at <socket_path>; pass --use-daemon to delegate, or --allow-standalone to override."
3. The daemon is unchanged. No daemon resources are consumed.
4. The user must exit and re-run with the correct flag.

This forces the user to make an explicit choice: delegate to the daemon, or explicitly opt into standalone mode. The fail-closed approach prevents silent competition between two signaling connections.

### 9. Migration / Rollout Plan

#### Phase A — Merge and default-off

- Merge the code changes (this ADR's implementation) with `default = no --use-daemon`.
- All existing users (CI, dev, production) continue to run in standalone mode.
- The collision detection prints `tracing::warn!` in the log but daemon + standalone coexistence is not yet checked — actually, Phase A MUST include the collision detection because without it, a user who starts the daemon and then runs `host-agent` standalone would have two competing connections.
- PID file is written when the daemon runs outside systemd/launchd (e.g., dev/test). No user-visible change.
- New CLI flags (`--use-daemon`, `--allow-standalone`, `--ipc-socket`) are present but unused by default.

#### Phase B — Service enablement

- Update the install scripts (`dist/install.sh`, `dist/install-macos.sh`, WiX/MSI) to install and start the daemon by default.
- On upgrade: the package manager runs `qubox-daemon install` post-install, which registers the service and starts it.
- The daemon now runs on every boot. Users who interactively start `host-agent` see the collision warning and must add `--allow-standalone` to their development scripts.
- Phase B is the point at which CI/test scripts should add `--allow-standalone` or `--use-daemon` as appropriate.

#### Phase C — Auto-detect default

- Change the default behavior of `--use-daemon` from `false` to "auto-detect".
- Auto-detect: probe the IPC socket. If responsive, delegate (same as `--use-daemon`). If not, run standalone.
- The `--allow-standalone` flag becomes the explicit escape hatch for development.
- This is the user-friendly default: users who have the daemon installed get delegation automatically; users without the daemon get standalone.

#### Phase D — Standalone deprecation (host-agent only)

- Start emitting a deprecation warning when `host-agent` runs standalone without the daemon:
  ```
  warning: running host-agent without the daemon is deprecated.
  Install the daemon with `qubox-daemon install` for automatic updates
  and background session management.
  ```
- `client-cli` standalone remains supported (for CI/testing; the client role does not have the same update/pairing concerns).
- A future ADR will decide on full removal of `host-agent` standalone mode.

### 10. Test Plan

#### 10.1 Unit tests

| Test | What it verifies |
|------|------------------|
| `test_pid_file_write_and_read` | Write PID file, verify content matches `std::process::id()`, verify format is `"{pid}\n"` |
| `test_pid_file_stale_detection` | Dead PID file → `check_and_clean_stale_pid_file` returns true and removes file |
| `test_pid_file_alive_process` | File with real PID (self) → returns false, file not removed |
| `test_pid_file_corrupt` | File with non-numeric content → treated as stale, removed |
| `test_pid_file_skip_service_manager` | `INVOCATION_ID` set → `DaemonConfig.pid_file_path` is `None` |
| `test_probe_daemon_running` | Mock IPC server responds to Ping → `check_daemon_running` returns true |
| `test_probe_daemon_not_running` | No server → returns false |
| `test_probe_daemon_timeout` | Server connects but never responds → returns false (200ms timeout) |
| `test_restart_throttle` | Feed 4 instant spawns → first 3 return `Some(delay)`, 4th returns `None` |
| `test_restart_throttle_window` | Feed 3 spawns, advance clock by 61s → 4th spawn returns `Some(delay)` (window cleared) |
| `test_restart_backoff` | 1st spawn: 1s, 2nd: 2s, 3rd: 4s |
| `test_kill_protocol_timeout` | Mock child that ignores SIGTERM → daemon sends SIGKILL after 5s |
| `test_update_repo_resolve_priority` | All priority orders: CLI > new env var > deprecated env var > default |
| `test_update_repo_persist` | URL persisted to redb, loaded on restart |
| `test_exit_reason_normal` | `ExitStatus::from_raw(0)` → "exited successfully" |
| `test_exit_reason_error` | `ExitStatus::from_raw(1)` → "exited with code 1" |
| `test_exit_reason_signal` | Unix: `ExitStatus::from_raw(0x000b)` (SIGSEGV) → "killed by signal 11" |
| `test_ipc_event_host_state_fields` | Serialize/deserialize `HostStateChanged` with all new fields |

#### 10.2 Integration tests

| Test | Setup | What it verifies |
|------|-------|------------------|
| **Subprocess spawn + exit** | Start daemon in test mode, send `StartHost` with mock binary (scripts/echo-exit-0.sh) | Verifies child is spawned, stdout piped, exit detected, `HostStateChanged` emitted with exit code 0 |
| **Subprocess crash + restart** | Mock binary that exits code 1 immediately | Verifies restart throttle engages: 3 restarts within 60s, then give up |
| **Subprocess kill on StopHost** | Start host-agent sleep 60, send StopHost | Verifies child is killed within 5s |
| **Standalone collision probe** | Start daemon, then run `host-agent` (no flags) via `assert_cmd` | Exit code 2, stderr contains diagnostic message |
| **Use-daemon delegation** | Start daemon, run `host-agent --use-daemon` | Exit code 0, daemon's host_state shows running |
| **Allow-standalone override** | Start daemon, run `host-agent --allow-standalone` | Runs normally (no collision error) |
| **PID file lifecycle** | Run daemon standalone, check PID file exists, send `Quit`, verify PID file removed | Full lifecycle |

#### 10.3 End-to-end tests

| Test | What it verifies |
|------|------------------|
| **Daemon + host-agent + client-cli all via daemon** | Xephyr + signaling server + daemon + `host-agent --use-daemon` + `client-cli start-session --use-daemon` → full streaming session through the daemon delegation path |
| **Daemon crash recovery** | Kill daemon, restart daemon, verify it reconnects to signaling, accepts IPC, can start host |
| **PID file stale on crash** | Write PID file, simulate crash (SIGKILL), restart daemon, verify stale PID file is cleaned, new PID file written |

### Consequences

#### Positive

- **Explicit delegation model**: The three topologies cover all user intents (standalone, daemon-managed, or explicit conflict). No silent competition.
- **Crash resilience**: The subprocess manager restarts crashed media pipelines with throttled exponential backoff, preventing resource-exhaustion crash loops.
- **Process discovery**: PID files give tools like `monit`, system monitors, and the `status` CLI a reliable way to find the daemon.
- **Security**: Windows named pipe DACL follows the principle of least privilege, allowing only the necessary SIDs access.
- **Persistence**: The TUF repo URL is remembered across restarts; users set it once.
- **Backward compatibility**: Phase A does not break any existing workflow. The collision detection fails closed with a clear message.

#### Negative

- **Added complexity**: The subprocess manager (spawn, pipe, monitor, throttle, kill) adds ~300 lines of nontrivial concurrent code.
- **Windows DACL maintenance**: The SDDL string and its SIDs must be kept in sync with the Windows SID well-known SID documentation. If Microsoft ever changes `S-1-5-11` semantics, the DACL must be updated.
- **PID file race**: Writing the PID file before `sd_notify READY=1` could cause a reader to see the PID before the daemon is ready. The sequence must be: init → IPC bind → write PID → `sd_notify READY=1`.

#### Risks

- **Subprocess restart loop with side effects**: If the spawned host-agent crashes due to a permanent configuration error (e.g., invalid codec), the daemon retries 3 times in 60 seconds before giving up. Each restart attempts to create a new ffmpeg capture pipeline, which may leak GPU resources if ffmpeg's cleanup is incomplete on crash. Mitigation: the daemon's restart window resets after 60s, giving time for the system to recover.
- **Windows named pipe `bInheritHandle`**: The security descriptor has `bInheritHandle = FALSE`, which prevents child processes from inheriting the pipe handle. This is correct because the daemon's child (the spawned host-agent) does not need the daemon's IPC listener handle.
- **Deprecated env var phase-out**: The old name `QUBOX_UPDATE_REPO` will be supported for at least 3 releases after the new name `QUBOX_UPDATE_REPO` is introduced. After that, a warning is emitted. Removal is tracked in a future ADR.

### Open Questions

1. **What is the default signaling URL for the spawned subprocess?** The daemon knows the signaling URL from its own config (either from the env var `QUBOX_SERVER` or from the config file). It passes this URL to the spawned host-agent via `--server`. The user's original `--server` argument from the foreground shim is NOT forwarded — the daemon is the authoritative owner of the signaling connection. However, the shim's `--server` can be different from the daemon's configured server. Resolution: the daemon's signaling URL is authoritative. The shim's `--server` is ignored when delegating.

2. **Should the daemon pass `--session-id` to the spawned child, or should the child generate its own session ID?** The daemon should generate the session ID and pass it via `--session-id <uuid>`. This ensures the daemon knows the session ID before the child starts, enabling immediate `HostStateChanged` events with the correct session ID. The host-agent CLI gains a hidden `--session-id` flag used only when spawned by the daemon.

3. **What happens when the daemon is stopped while a host session is running?** ADR-005 §1.7 specifies that on daemon crash, the child processes detect IPC disconnect and report failure. On intentional daemon stop (`Quit` IPC), the daemon should first `StopHost` / `StopClient` (killing the children), then exit. This is already the expected cleanup sequence; the `Daemon::run` shutdown path should call `host_manager.stop_all()`.

4. **Windows service identity conflict**: ADR-005 §1.5 specifies `LocalService` for the daemon. The DACL in this ADR grants access to `LocalService` (S-1-5-19). When the service runs as `LocalService` but the user-session client runs as the interactive user, the DACL's `Authenticated Users` (S-1-5-11) ACE allows the client to connect. This is correct. However, `OpenProcessToken` → `GetTokenInformation(TokenUser)` at runtime will show different SIDs for the daemon and the client. The runtime auth check must compare the client's SID against the set of allowed user SIDs, not against the daemon's own SID. The allowed set includes: the daemon's own user SID (LocalService), the LocalSystem SID, and the Authenticated Users well-known SID group. Since any authenticated user matches this group, the runtime check effectively allows any authenticated user. The DACL is the actual enforcement boundary.

### References

- ADR-005 Daemon and TURN Architecture: `research/decisions/ADR-005-daemon-and-turn-architecture.md`
- P1-13 Daemon research doc: `research/roadmap/p1-13-daemon.md`
- systemd `INVOCATION_ID`: https://www.freedesktop.org/software/systemd/man/latest/systemd.exec.html#Environment%20variables%20set%20by%20the%20service%20manager
- launchd environment variables: `launchctl setenv` / `LAUNCHD_JOB`: https://developer.apple.com/documentation/servicemanagement
- Windows SID well-known identifiers: https://learn.microsoft.com/en-us/windows/win32/secauthz/well-known-sids
- SDDL syntax: https://learn.microsoft.com/en-us/windows/win32/secauthz/security-descriptor-definition-language
- `ConvertStringSecurityDescriptorToSecurityDescriptorW`: https://learn.microsoft.com/en-us/windows/win32/api/sddl/nf-sddl-convertstringsecuritydescriptortosecuritydescriptorw
- Tokio process management: https://docs.rs/tokio/latest/tokio/process/
- `redb` TableDefinition: https://docs.rs/redb/latest/redb/struct.TableDefinition.html
- `tough` TUF client: https://docs.rs/tough
