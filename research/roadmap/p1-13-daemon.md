# P1-13: Daemon (systemd, Windows SCM, launchd)

Status: research complete, implementation pending.
Owner: new `apps/daemon/` crate (the host-agent and client-cli currently run as foreground processes; the daemon owns pairing, signaling, and updates).
Depends on: the existing signaling server, host-agent, and client-cli.
Blockers: the daemon is a significant new architectural piece; design carefully.

## Goal

Convert the existing host-agent / client-cli into a daemon-based architecture: a single background service that owns pairing, signaling connections, state persistence, and auto-update. The foreground CLI/GUI talks to the daemon over a local IPC channel. The daemon starts at boot, runs as a system service on each platform, and survives user logout. Auto-update via TUF (The Update Framework).

## Research Summary

### Linux: systemd service

The standard daemon manager on modern Linux. Use `Type=notify` so systemd waits for the daemon to be ready; the daemon sends `READY=1` via `sd_notify` after initialization.

```ini
# /etc/systemd/system/qubox.service
[Unit]
Description=Qubox daemon
After=network-online.target
Wants=network-online.target

[Service]
Type=notify
ExecStart=/usr/bin/qubox-daemon
Restart=on-failure
RestartSec=1
User=qubox
Group=qubox
NoNewPrivileges=true
ProtectSystem=strict
ProtectHome=true
PrivateTmp=true
RestrictAddressFamilies=AF_UNIX AF_INET AF_INET6
SystemCallFilter=@system-service
StandardOutput=journal
StandardError=journal

[Install]
WantedBy=multi-user.target
```

A separate `.socket` unit for IPC activation (`/run/qubox.sock`).

**Hardening** is essential: `NoNewPrivileges`, `ProtectSystem=strict`, `ProtectHome`, `PrivateTmp`, `RestrictAddressFamilies`, `SystemCallFilter`. Tune `SystemCallFilter` to your real network/IPC needs.

**Logging**: stdout/stderr to journald via `tracing-journald` or `tracing-subscriber` with a journald layer.

Rust crate: `sd-sys` or `systemd` for `sd_notify`. Or hand-written FFI to `libsystemd`.

### Windows: Service Control Manager (SCM)

The standard Windows service manager. Use the `windows-service` crate for the boilerplate:

```rust
use windows_service::{define_windows_service, service::*};

const SERVICE_NAME: &str = "QuboxDaemon";

define_windows_service!(ffi_service_main, service_main);

fn service_main(_args: Vec<std::ffi::OsString>) {
    // register handler, report START_PENDING, init daemon, report RUNNING
}

fn main() -> windows_service::Result<()> {
    service_dispatcher::start(SERVICE_NAME, ffi_service_main)
}
```

**Service types**:
- `SERVICE_WIN32_OWN_PROCESS`: one process, one service. Use this.
- `SERVICE_AUTO_START`: starts at boot.
- `SERVICE_ERROR_NORMAL`: log errors to Event Log and continue.

**Service identity**: LocalSystem, LocalService, NetworkService, or a specific user (requires password storage in the installer). LocalSystem is the easiest; LocalService is more restricted.

**Logging**: Windows Event Log via `ReportEventW` or the `tracing` crate with a custom layer that writes to the Event Log.

**Install**: `sc create QuboxDaemon binPath= "C:\Program Files\Qubox\daemon.exe" start= auto` or via the WiX/MSI installer.

### macOS: launchd

The standard macOS service manager. Use a **LaunchDaemon** for system-wide (`/Library/LaunchDaemons/`) or a **LaunchAgent** for per-user (`~/Library/LaunchAgents/`).

```xml
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
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
    <string>/var/log/qubox.out.log</string>
    <key>StandardErrorPath</key>
    <string>/var/log/qubox.err.log</string>
</dict>
</plist>
```

For a foreground GUI that needs user-session interaction, use a **LaunchAgent** instead.

**Manage with `launchctl`**:
- `sudo launchctl load /Library/LaunchDaemons/com.qubox.daemon.plist`
- `sudo launchctl unload ...`
- `sudo launchctl start com.qubox.daemon`
- `sudo launchctl stop com.qubox.daemon`

### IPC (GUI ↔ daemon)

| OS | Recommended IPC |
|----|------------------|
| Linux | Unix socket at `/run/qubox.sock` (or D-Bus) |
| Windows | Named pipe `\\.\pipe\Qubox` |
| macOS | Unix domain socket at `~/Library/Application Support/qubox/daemon.sock` |

Wire format: **length-prefixed bincode** messages with a JSON header for debuggability:

```rust
#[repr(C)]
pub struct IpcMessage {
    pub magic: u32,    // 0xB0_1A_1C_BE
    pub length: u32,
    pub payload: Vec<u8>,  // bincode
}
```

### State persistence

For sessions, pairings, and settings, a small embedded store is enough.

- **`rusqlite`** (SQLite): best for complex queries.
- **`redb`**: pure Rust, ACID, BTree-based.
- **`sled`**: pure Rust, embedded, deprecated as of 2024 but still works.
- **`bincode`** file-based: simplest, no DB.

**Choice: `redb`** for our use case (simple key-value, ACID, pure Rust, no native deps).

File locations:
- Linux: `~/.config/qubox/state.db` (XDG_CONFIG_HOME).
- Windows: `%APPDATA%\Qubox\state.db`.
- macOS: `~/Library/Application Support/com.qubox.daemon/state.db`.

Use the `directories` crate for cross-platform path resolution.

### Auto-update (TUF)

Use the **`tough`** crate (the de-facto Rust TUF implementation, used by Docker / Notary v2). TUF metadata:

- `root.json`: trusted keys.
- `targets.json`: target files (binaries) and their hashes.
- `snapshot.json`: signed list of all target files.
- `timestamp.json`: signed expiry for the snapshot.

Update flow:
1. Daemon fetches `timestamp.json` from the update server.
2. Verifies the signature against the previously-trusted `root.json`.
3. Fetches `snapshot.json`, verifies.
4. Fetches `targets.json`, verifies.
5. Compares local vs remote; if newer, downloads the binary.
6. Verifies the binary's hash against `targets.json`.
7. Stages the new binary, restarts the daemon.

### Rust crate matrix (2024-2026)

- `sd-sys` or `systemd` (libsystemd FFI for `sd_notify`).
- `windows-service` (Windows SCM).
- `launchd` or hand-written FFI to launchd on macOS.
- `redb` 2.x (state persistence).
- `directories` 5.x (XDG paths).
- `tough` 0.17+ (TUF client).
- `tracing` + `tracing-journald` + `tracing-subscriber` (logging).
- `tokio` (async runtime).
- `serde` + `bincode` (IPC).
- `clap` (CLI subcommands).

### 2024-2026 status

- **systemd 255+** is in every modern Linux distro. `Type=notify` is the recommended pattern for non-trivial daemons.
- **Windows Service in Rust** is mature via `windows-service`. Microsoft's own `windows-service-rs` is the reference.
- **launchd** is stable; no API changes since macOS 10.4.
- **TUF in Rust**: `tough` is the standard; used by Docker Notary v2, Sigstore, and others.
- **redb** is gaining adoption as the pure-Rust alternative to SQLite for simple key-value workloads.

## Implementation Plan

### Step 1: New `daemon` crate

`apps/daemon/Cargo.toml`:
- Dependencies: `tokio`, `tracing`, `redb`, `directories`, `serde`, `bincode`, `clap`, `tough` (for update).
- The daemon is a long-running process that owns the signaling connection, the pairing state, the host-agent control, and the update check.

### Step 2: Service integration

`apps/daemon/src/service/mod.rs`:
- `pub trait Service { fn start(&mut self) -> Result<()>; fn stop(&mut self) -> Result<()>; }`.
- `pub struct DaemonService { ipc_server: IpcServer, signaling: SignalingClient, state_db: redb::Database, update_checker: UpdateChecker }`.

`apps/daemon/src/service/linux.rs`:
- `pub fn run() -> Result<()>` — initialize, sd_notify READY=1, run forever, handle SIGTERM.

`apps/daemon/src/service/windows.rs`:
- `pub fn run() -> windows_service::Result<()>` — `define_windows_service!` + `service_dispatcher::start`.

`apps/daemon/src/service/macos.rs`:
- `pub fn run() -> Result<()>` — run forever, write PID to `/var/run/qubox-daemon.pid`, handle SIGTERM.

### Step 3: IPC server

`apps/daemon/src/ipc.rs`:
- `pub struct IpcServer { listener: tokio::net::UnixListener | tokio::net::windows::named_pipe::ServerOptions }`.
- `pub async fn run(&self) -> Result<()>` — accepts connections, length-prefixes bincode messages, dispatches to handlers.
- The handlers are: `ListPairings`, `StartHost`, `StopHost`, `GetStatus`, `CheckUpdate`, `ApplyUpdate`, `Quit`.

### Step 4: State persistence

`apps/daemon/src/state.rs`:
- `pub struct StateDb { db: redb::Database }`.
- `pub fn open() -> Result<Self>` — opens the redb file.
- `pub fn load_pairings(&self) -> Result<Vec<Pairing>>`.
- `pub fn save_pairing(&self, p: &Pairing) -> Result<()>`.
- `pub fn get_setting(&self, key: &str) -> Result<Option<String>>`.
- `pub fn set_setting(&self, key: &str, value: &str) -> Result<()>`.

### Step 5: Update checker

`apps/daemon/src/update.rs`:
- `pub struct UpdateChecker { tuf: tough::Client, repo: String }`.
- `pub async fn check_for_update(&self) -> Result<Option<Version>>` — fetches TUF metadata, compares versions.
- `pub async fn download_update(&self, version: &Version) -> Result<PathBuf>` — downloads the binary to a staging dir.
- `pub fn apply_update(&self, staged: &Path) -> Result<()>` — verifies the hash, atomically replaces the running binary, restarts.

### Step 6: CLI subcommands

`apps/daemon/src/main.rs`:
- `qubox-daemon` (no subcommand): run the daemon.
- `qubox-daemon pair <host-id>`: send a `Pair` request to the daemon over IPC.
- `qubox-daemon list`: list pairings.
- `qubox-daemon status`: show the daemon's status.
- `qubox-daemon update`: check for updates.
- `qubox-daemon install`: register the service (calls `systemctl enable`, `sc create`, or `launchctl load`).
- `qubox-daemon uninstall`: unregister the service.

### Step 7: Installer

The installer (DEB / RPM / MSI / PKG) registers the service:
- Linux: install the `.service` and `.socket` files, run `systemctl daemon-reload && systemctl enable qubox.service`.
- Windows: install the binary, register the service via `sc create` (or via the WiX/MSI).
- macOS: install the `LaunchDaemon` plist, run `launchctl load`.

### Step 8: Tests

- Unit test: IPC message round-trip.
- Unit test: redb open/close, save/load.
- Integration test: start the daemon, send IPC messages, verify responses.
- Service test: install the service, start it, verify it's running, stop it, verify it's stopped.

## Risks and Open Questions

- **Permissions**: the daemon needs to do privileged things (read host config, write to system paths, install drivers). Running as LocalSystem on Windows or root on Linux is the simplest. Restrict with `NoNewPrivileges` and `ProtectSystem` where possible.
- **Self-update race**: when the daemon updates itself, the running process needs to be replaced atomically. On Linux: write to a staging path, then `rename` atomically. On Windows: the running binary can't be replaced while running; the new process is staged, the daemon schedules itself to exit, the service manager restarts it. On macOS: similar to Linux.
- **State migration**: when the state format changes (e.g. redb version upgrade), the daemon must migrate. Add a version field to the state and a migration step.
- **Logging levels**: `RUST_LOG=info` should be the default; `debug` for troubleshooting. The user can change via the daemon config or environment.
- **TUF metadata rotation**: the root key should be rotated periodically. `tough` supports multi-signature roots with a threshold.
- **Daemon crashed mid-session**: the user must be able to recover. The session state is persisted; on restart, the daemon reconnects to the signaling server and resumes the session.
- **Multi-user**: Linux/macOS allow multiple users. Should the daemon be system-wide (LaunchDaemon) or per-user (LaunchAgent)? Per-user is more privacy-friendly (the user controls the daemon) but harder to coordinate across users. Default to per-user.
- **Anti-virus on Windows**: the daemon binary may be flagged as suspicious by anti-virus. Sign the binary (P2-19) and add installation instructions.
- **macOS code signing + notarization**: required for the LaunchDaemon to load on macOS 14+ (similar to P2-19). Ship signed + notarized binary.

## References

- systemd.service: https://www.freedesktop.org/software/systemd/man/systemd.service.html
- systemd socket activation: https://www.freedesktop.org/software/systemd/man/systemd.socket.html
- Red Hat: Working with systemd unit files: https://docs.redhat.com/en/documentation/red_hat_enterprise_linux_9/html/using_systemd_unit_files_to_customize_and_optimize_your_system/assembly_working-with-systemd-unit-files_working-with-systemd
- windows-service crate: https://docs.rs/windows-service
- windows-service-rs source: https://github.com/mullvad/windows-service-rs/blob/master/src/service.rs
- Windows Service in Rust tutorial: https://davidhamann.de/2026/02/28/writing-a-windows-service-in-rust/
- Windows Service Rust forum: https://users.rust-lang.org/t/windows-service-in-rust/137389
- launchd reference: macOS launchd plist XML format
- launchctl commands: macOS launchctl CLI
- tough (TUF): https://crates.io/crates/tough
- redb: https://docs.rs/redb
- rustysd (systemd in Rust): https://github.com/KillingSpark/rustysd
- StackOverflow: start a Rust binary as a systemd daemon: https://stackoverflow.com/questions/63093667/start-a-rust-binary-as-a-systemd-daemon
- Perplexity research, 2026-07-02: systemd, Windows SCM, launchd, IPC, persistence, TUF, 2024-2026 status.
