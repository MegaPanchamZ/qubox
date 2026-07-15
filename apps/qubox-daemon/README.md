# qubox-daemon — Qubox Daemon

`qubox-daemon` is the background service for Qubox. It owns the
signaling WebSocket connection, pairing state, host/client lifecycle, TUF
auto-update, and TURN credential issuance. Foreground CLIs (`host-agent`,
`client-cli`) communicate with it over a local IPC socket.

## Benefits

- **Persistent signaling**: the daemon keeps the WebSocket connection alive,
  reconnecting with exponential backoff.
- **State persistence**: pairings, settings, and session history survive
  restarts (stored in a `redb` database).
- **Auto-update**: the daemon periodically checks for new versions via TUF,
  downloads, verifies, and applies updates without user intervention.
- **TURN credentials**: the daemon fetches short-term TURN credentials from
  the signaling server, enabling NAT traversal for host/client connections.

## Installation

### Linux

#### DEB (Debian / Ubuntu)

```bash
sudo dpkg -i qubox-daemon_<version>_amd64.deb
systemctl --user enable --now qubox.service
```

#### RPM (Fedora / RHEL)

```bash
sudo rpm -i qubox-daemon-<version>-1.x86_64.rpm
systemctl --user enable --now qubox.service
```

#### AppImage (any distro)

```bash
chmod +x Qubox-<version>-x86_64.AppImage
./Qubox-<version>-x86_64.AppImage
```

For background operation, configure your system to run the AppImage at boot
(e.g., add it to your desktop environment's autostart).

### Windows

Run the MSI installer (`QuboxDaemon-<version>-x64.msi`) as Administrator.
The service is registered as `QuboxDaemon` with automatic startup.

### macOS

```bash
sudo installer -pkg QuboxDaemon-<version>-universal.pkg -target /
# The LaunchDaemon is loaded automatically by the postinstall script
```

## Running

### Linux (systemd user service)

```bash
# Enable at user login
systemctl --user enable qubox.service

# Start now
systemctl --user start qubox.service

# Check status
systemctl --user status qubox.service

# View logs
journalctl --user -u qubox.service -f
```

### Windows

```powershell
# Start the service
Start-Service QuboxDaemon

# Check status
Get-Service QuboxDaemon

# View Event Log
Get-WinEvent -LogName Application | Where-Object { $_.ProviderName -like "*better*" }
```

### macOS

```bash
# Start
sudo launchctl load /Library/LaunchDaemons/com.qubox.daemon.plist

# Check status
sudo launchctl list | grep qubox

# View logs
tail -f /var/log/qubox.log
```

## CLI Usage

```text
qubox-daemon                          # Run the daemon (foreground)
qubox-daemon update check             # Check for available updates
qubox-daemon update status            # Show update status
qubox-daemon update apply <version>   # Apply a staged update
qubox-daemon --help                   # Full help
```

## Configuration

The daemon reads the following environment variables:

| Variable | Default | Description |
|----------|---------|-------------|
| `RUST_LOG` | `info` | Log level (trace, debug, info, warn, error) |
| `QUBOXD_IPC_MAX_CONNS` | `8` | Max concurrent IPC connections |
| `QUBOXD_STATE_DB_PATH` | XDG path | Override state database path |
| `QUBOXD_SOCKET_PATH` | XDG path | Override IPC socket path |

Configuration can also be persisted via IPC `SetSetting` calls, stored in
the `redb` database under the `settings` table.

## Troubleshooting

### Port conflicts

The daemon does not listen on any network port. It uses a Unix domain socket
(Linux/macOS) or a Named Pipe (Windows) for IPC. Port conflicts are only
relevant for the `host-agent` / `client-cli` media pipelines (QUIC ports)
and the signaling server (port 7000).

### Permission issues

- **Linux**: ensure the systemd user service runs as your user
  (`systemctl --user`). The socket is created under `$XDG_RUNTIME_DIR`
  which is user-private.
- **Windows**: the MSI installer configures the service to run as
  `NT AUTHORITY\LocalService`. If the daemon cannot access your user profile,
  reinstall with a user-managed service configuration.
- **macOS**: the LaunchDaemon runs as root. If you prefer per-user operation,
  move the plist to `~/Library/LaunchAgents/` and adjust paths.

### Log locations

| Platform | Log |
|----------|-----|
| Linux | `journalctl --user -u qubox.service -f` |
| Windows | Event Log → Applications and Services Logs |
| macOS | `/var/log/qubox.log` (stdout), `/var/log/qubox.err.log` (stderr) |

### Daemon won't start

1. Check the logs (see above).
2. Verify the binary is at the expected path (`/usr/bin/qubox-daemon` on
   Linux, `C:\Program Files\Qubox\daemon\qubox-daemon.exe` on
   Windows, `/usr/local/bin/qubox-daemon` on macOS).
3. Ensure the state database directory is writable by the daemon's user.
4. Try running the daemon in the foreground to see error messages:
   ```bash
   RUST_LOG=debug /usr/bin/qubox-daemon
   ```
5. If the IPC socket is stale, remove it:
   ```bash
   rm -f $XDG_RUNTIME_DIR/qubox/qubox.sock
   ```
