# Local dev scripts for the qubox remote-desktop stack.

All scripts are self-contained. Run from anywhere; they `cd` into the repo
root themselves. Pass `--help` where supported.

## One-time setup

| Script | What it does |
|---|---|
| `vm-sync.sh` | Sync the project tree into the Windows VM (`./scratch/` Vagrant box). |
| `start-signaling.sh` | Boot the signaling server on the Linux host (`cargo build` first). |

The VM-side setup is in `../../scratch/`:

| Script | What it does |
|---|---|
| `../../scratch/setup-vm-build.ps1` | One-time install of MSVC Build Tools, libclang (LLVM), Gyan FFmpeg dev build, Rust msvc toolchain. ~15 min, ~6 GB. |
| `../../scratch/build-qubox-windows.ps1` | `cargo build --release` for the host-agent + client-cli + signaling-server in the VM (Windows MSVC). |
| `../../scratch/run-host-agent.ps1` | Run `qubox-host-agent.exe` inside the VM so its desktop is the controlled host. |

## Typical end-to-end smoke test (Linux host as the controlled machine)

This is what works today:

```bash
# 0. one-time
cd "$(git rev-parse --show-toplevel)"
cargo build --release -p qubox-host-agent -p qubox-client-cli -p qubox-signaling-server

# 1. start signaling (terminal A)
ops/local/start-signaling.sh &

# 2. start host-agent (terminal B)
QUBOX_IDENTITY_PATH=/tmp/qubox-host-identity \
./target/release/qubox-host-agent \
  --server ws://127.0.0.1:7000/ws \
  --auto-approve-pairing \
  --name my-host

# 3. pair + start session (terminal C)
QUBOX_IDENTITY_PATH=/tmp/qubox-client-identity \
./target/release/qubox-client-cli \
  --server ws://127.0.0.1:7000/ws \
  pair --host my-host

QUBOX_IDENTITY_PATH=/tmp/qubox-client-identity \
./target/release/qubox-client-cli \
  --server ws://127.0.0.1:7000/ws \
  start-session --host my-host --codec h264
# window pops up showing your Linux desktop
```

Or use the wrapper scripts:

```bash
ops/local/start-signaling.sh &         # terminal A
./target/release/qubox-host-agent \    # terminal B (manual; uses the binary directly)
  --server ws://127.0.0.1:7000/ws --auto-approve-pairing --name my-host &
ops/local/list-hosts.sh                # should print "my-host Linux ..."
ops/local/connect.sh my-host           # terminal C (pair + open session window)
```

## Windows VM as host (build blocked — see "Known build issues")

Once `windows-sys` / opus cross-compile is unblocked upstream:

```bash
# 0. one-time
cd "$(git rev-parse --show-toplevel)/scratch"
vagrant up
vagrant winrm --command "powershell -ExecutionPolicy Bypass -File C:\Users\vagrant\setup-vm-build.ps1"
# (wait ~15 min for VS Build Tools + LLVM + Rust MSVC)

# 1. sync project tree into VM
ops/local/vm-sync.sh

# 2. build Windows binaries inside the VM
vagrant winrm --command "powershell -ExecutionPolicy Bypass -File C:\Users\vagrant\qubox\qubox\scratch\build-qubox-windows.ps1"

# 3. start host-agent in the VM
vagrant winrm --command "powershell -ExecutionPolicy Bypass -File C:\Users\vagrant\qubox\qubox\scratch\run-host-agent.ps1 -AutoApprovePairing"

# 4. pair + connect from host
ops/local/list-hosts.sh                # should list "my-vm Windows ..."
ops/local/connect.sh my-vm --codec h264
```

## Cross-OS sanity test (Linux host + Linux client works today)

```
# Terminal A (host) — signaling
ops/local/start-signaling.sh

# Terminal B (host or VM — host-agent runs the controlled machine's screen) — host
cd C:\Users\vagrant\qubox\qubox
.\target\release\qubox-host-agent.exe `
    --server ws://192.168.121.1:7000/ws `
    --name my-vm `
    --auto-approve-pairing

# Terminal C (host) — client
ops/local/connect.sh my-vm --transport native-quic --codec h264
```

### Currently working

- **Linux host ↔ any client (Linux/Mac/Windows client built on Linux):** full
  loop works. `cargo build --release -p qubox-host-agent -p qubox-client-cli
  -p qubox-signaling-server` on the host and you're done.
- **Windows VM as host:** VM is fully provisioned (VS Build Tools, LLVM,
  Rust MSVC, FFmpeg), scripts are reproducible. **Building the
  `qubox-host-agent.exe` is currently blocked** — see "Known build issues"
  below.

## Known build issues

Building `qubox-host-agent.exe` from this workspace on Windows MSVC is
currently blocked by an upstream toolchain bug:

| Path | Symptom | Status |
|---|---|---|
| In-VM (`build-qubox-windows.ps1`) | rustc 1.97 + MSVC 14.44 + `windows-sys` 0.61.x → `STATUS_STACK_BUFFER_OVERRUN` (0xc0000409). **Mitigation applied:** direct `windows-sys = "=0.59.0"` on Windows targets (`qubox-host-agent`, `qubox-daemon`). Rebuild with `cargo update -p windows-sys` then `build-qubox-windows.ps1`. | mitigated (pin 0.59) |
| Cross-compile (`cargo-xwin` from Linux) | `audiopus_sys` (opus C library) fails to build under clang-cl with this toolchain config. | blocked |

**Workarounds (any of these unblocks the build):**
1. **Pin `windows-sys = "=0.59"`** via `[patch.crates-io]` in `Cargo.toml` — the 0.59 series doesn't have the WDK feature gate that explodes the codegen.
2. **Use Rust 1.77 or earlier** (before the codegen change that triggers /GS on large compile units).
3. **Wait for upstream fix** — track https://github.com/rust-lang/rust/issues for `STATUS_STACK_BUFFER_OVERRUN` + `windows-sys`.

The `build-qubox-windows.ps1` script documents this in its header.

## All scripts

| Script | Purpose |
|---|---|
| `start-signaling.sh` | Linux host: start the signaling server (`QUBOX_BIND`, `QUBOX_PAIRING_STORE`, `QUBOX_SIGNALING_SECRET`). |
| `connect.sh <host>` | Linux host: pair + start-session. Pass `--codec h264\|h265\|av1`, `--transport native-quic\|web-rtc\|relay-quic`. |
| `list-hosts.sh` | Linux host: show hosts currently registered. |
| `vm-sync.sh` | Sync the workspace tree into the Windows VM (uses `vagrant upload` or scp fallback). |

| VM-side script | Purpose |
|---|---|
| `../../scratch/setup-vm-build.ps1` | One-time install of VS Build Tools, libclang (LLVM), FFmpeg dev (optional), Rust MSVC. ~15 min, ~6 GB. |
| `../../scratch/build-qubox-windows.ps1` | `cargo build --release` for host-agent + client-cli + signaling-server inside the VM. |
| `../../scratch/run-host-agent.ps1` | Run `qubox-host-agent.exe` inside the VM so its desktop is the controlled host. |

## VM access

```bash
# PowerShell into the VM
cd scratch && vagrant winrm

# RDP into the VM (cert accept on first connect)
xfreerdp /v:localhost:53389 /u:vagrant /p:vagrant /cert:tofu +clipboard /size:1440x900

# Halt / resume / destroy
cd scratch && vagrant halt
cd scratch && vagrant resume
cd scratch && vagrant destroy -f
```

## State locations

| State | Where |
|---|---|
| Host-agent identity | VM: `%USERPROFILE%\.qubox\identity-host.json` |
| Client identity | Linux: `<repo>/.local/qubox/identity-client.json` |
| Pairing store | Linux: `<repo>/.local/qubox/pairing.sqlite` |
| Workspace inside VM | VM: `C:\Users\vagrant\qubox\` (synced by `vm-sync.sh`) |

## Typical end-to-end smoke test

```bash
# 0. one-time
cargo build --release -p qubox-host-agent -p qubox-client-cli -p qubox-signaling-server
cd scratch && vagrant up && cd ..
ops/local/vm-sync.sh

# 1. inside VM (via RDP or vagrant winrm)
cd scratch
vagrant winrm --command "powershell -ExecutionPolicy Bypass -File C:\Users\vagrant\qubox\scratch\setup-vm-build.ps1"
# (wait ~15 min for VS Build Tools + LLVM + FFmpeg dev)
vagrant winrm --command "powershell -ExecutionPolicy Bypass -File C:\Users\vagrant\qubox\scratch\build-qubox-windows.ps1"

# 2. back on host
ops/local/start-signaling.sh &              # terminal A
ops/local/list-hosts.sh                     # should print "no hosts yet"

# 3. inside VM (terminal B) — run host
vagrant winrm --command "powershell -ExecutionPolicy Bypass -File C:\Users\vagrant\qubox\scratch\run-host-agent.ps1 -AutoApprovePairing"

# 4. on host (terminal C) — connect
ops/local/list-hosts.sh                     # should now list my-vm
ops/local/connect.sh my-vm --codec h264     # window pops up
```

For LAN / cross-network tests, set `QUBOX_BIND=0.0.0.0:7000` and use the
host's reachable IP. For WebRTC/relay, run a TURN server — see
`../coturn/`.