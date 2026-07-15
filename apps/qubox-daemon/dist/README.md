# dist/ — Distribution and installer artifacts

This directory contains the installer packaging configuration and build
scripts for the `qubox-daemon` daemon.

## Files

| File | Purpose |
|------|---------|
| `qubox.service` | systemd user service unit (`Type=notify`, socket-activated) |
| `qubox.socket` | systemd socket unit for IPC activation |
| `qubox.desktop` | Desktop entry for AppImage / Linux GUI launcher |
| `com.qubox.daemon.plist` | macOS LaunchDaemon plist |
| `qubox.wxs` | WiX 4.x installer source (Windows MSI) |
| `qubox.pkgproj` | Packages.app project file (macOS PKG, GUI workflow) |
| `pre-install.sh` | RPM pre-install: create `qubox` system user |
| `post-install.sh` | RPM post-install: `systemctl enable` the service |
| `pre-uninstall.sh` | RPM pre-uninstall: `systemctl stop && disable` |
| `post-uninstall.sh` | RPM post-uninstall: remove `qubox` user + group |
| `build-appimage.sh` | Build a portable Linux AppImage |
| `build-msi.sh` | Build the Windows `.msi` installer (requires WiX 4) |
| `build-pkg.sh` | Build the macOS `.pkg` installer (requires macOS + Xcode) |
| `README.md` | This file |

## How to Build Installers

### Linux AppImage

```bash
# Prerequisites: cargo
./apps/daemon/dist/build-appimage.sh
# Produces: Qubox-<version>-x86_64.AppImage
```

### Windows MSI

```bash
# Prerequisites: WiX Toolset v4, Windows (or WSL + .NET), built binary
cargo build --release --target x86_64-pc-windows-gnu -p qubox-daemon
./apps/daemon/dist/build-msi.sh
# Produces: QuboxDaemon-<version>-x64.msi
```

### macOS PKG

```bash
# Prerequisites: macOS + Xcode CLI tools
cargo build --release -p qubox-daemon
./apps/daemon/dist/build-pkg.sh
# Produces: QuboxDaemon-<version>-universal.pkg
```

### DEB / RPM

These are built via `cargo deb` and `cargo rpm` respectively, using the
metadata defined in `apps/daemon/Cargo.toml`:

```bash
cargo install cargo-deb cargo-rpm
cargo deb -p qubox-daemon
cargo rpm -p qubox-daemon
```

## CI Integration

The `.github/workflows/release.yml` workflow orchestrates all of the above
builds on GitHub-hosted runners and uploads the artifacts as a GitHub Release.
