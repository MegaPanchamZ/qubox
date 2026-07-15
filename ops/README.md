# ops — Operations and Infrastructure

This directory contains operational tooling for Qubox: CI workflows,
Docker configurations, TUF metadata management, and VM-lab automation.

## Layout

```
ops/
├── README.md                   ← this file
├── aws/                        ← AWS provisioning scripts
│   └── provision-signaling-ec2.ps1
├── coturn/                     ← TURN/STUN relay (staging)
│   ├── Dockerfile
│   ├── turnserver.conf
│   └── docker-compose.yml
├── signaling-server/           ← signaling server deployment
│   ├── qubox-signaling.service
│   ├── install-service.sh
│   └── run-signaling-server.sh
├── tuf/                        ← TUF auto-update metadata
│   ├── root.json
│   ├── targets.json
│   ├── snapshot.json
│   ├── timestamp.json
│   ├── init-tuf.sh
│   ├── publish-target.sh
│   └── README.md
└── vm-lab/                     ← local VM test lab
    ├── README.md
    └── virtualbox-jammy-cloud.ps1
```

## CI Workflows

### `.github/workflows/ci.yml`

Triggered on push/PR to `main`. Runs on `ubuntu-latest`, `windows-latest`,
`macos-latest`:
- `cargo fmt --check`
- `cargo clippy -- -D warnings`
- `cargo test --workspace --all-targets`
- Cross-compile the daemon for Windows MinGW on Ubuntu
- Upload binaries + systemd units + plist as artifacts

### `.github/workflows/release.yml`

Triggered on `v*` tag push or workflow_dispatch:
- Build release binaries on all 3 OSes
- Build .deb, .rpm, .msi, .pkg, .AppImage installers
- Publish a GitHub Release
- Call `ops/tuf/publish-target.sh` to update TUF metadata

## coturn (TURN/STUN)

The `ops/coturn/` directory contains a Docker Compose stack for local
TURN testing:

```bash
# Start the stack
docker compose -f ops/coturn/docker-compose.yml up -d

# View logs
docker compose -f ops/coturn/docker-compose.yml logs -f

# Tear down
docker compose -f ops/coturn/docker-compose.yml down
```

Services:
- **coturn**: TURN/STUN relay on ports 3478 (UDP/TCP) and 5349 (TLS).
- **signaling-server**: WebSocket signaling + TURN credential endpoint on
  port 7000. Configured with `QUBOX_TURN_SECRET=dev_shared_secret_change_me`
  and `QUBOX_TURN_URLS=turn:coturn:3478`.

## TUF Auto-Update

See [ops/tuf/README.md](tuf/README.md) for the TUF metadata layout, key
custody, and release publishing workflow.

## Release Process

1. Ensure all CI checks pass on `main`.
2. Create a signed tag:
   ```bash
   git tag -s v0.2.0 -m "v0.2.0"
   git push origin v0.2.0
   ```
3. The release workflow builds, packages, and publishes artifacts.
4. Update TUF metadata (automated via `publish-target.sh`, may require
   manual sign-off for the first few releases).
5. Deploy updated metadata to the TUF repo hosting (GitHub Pages / S3).

## VM Lab

See [ops/vm-lab/README.md](vm-lab/README.md) for local end-to-end testing
with VirtualBox + Ubuntu Jammy.

## Self-host (Docker)

Preferred single-stack deploy:

```bash
export QUBOX_SIGNALING_SECRET=$(openssl rand -hex 32)
export QUBOX_TURN_SECRET=$(openssl rand -hex 16)
docker compose -f ops/self-host/docker-compose.yml up -d --build
```

See `ops/self-host/README.md`. Dual-mode product design: ADR-021 + `docs/product-gaps.md`.
