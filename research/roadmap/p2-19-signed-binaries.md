# P2-19: Signed Binaries (Authenticode, Notarization, GPG, Sigstore, TUF)

Status: research complete, implementation pending.
Owner: CI/CD (`.github/workflows/`), `dist/` artifacts, host-agent / client-cli / signaling-server.
Depends on: P1-13 (daemon) for the auto-update chain (tough/TUF). Each P2 deliverable (`host-agent.exe`, `client-cli.exe`, `signaling-server.exe`, `host-agent.app`, `.deb`, `.rpm`, `AppImage`, the web bundle, the mobile AARs/XCFrameworks) is a separate signing target.
Blockers: **cert cost** ($130-700/year depending on type and provider), **HFS+ Keychain / Apple Developer Program** ($99/year), and **time to build SmartScreen reputation** for new publishers.

## Goal

Ship the `host-agent`, `client-cli`, and `signaling-server` binaries such that end users on Windows, macOS, and Linux can install and run them without security warnings. The auto-update chain (P1-13) must be cryptographically verifiable: a compromised mirror or a man-in-the-middle must not be able to install a malicious update.

The deliverables are:
1. **Windows**: Authenticode-signed `.exe` / `.msi` / `.msix`. Builds SmartScreen reputation over time.
2. **macOS**: Developer ID-signed, hardened-runtime-enabled, notarized `.app` / `.pkg` / `.dmg`.
3. **Linux**: GPG-signed `.deb` / `.rpm` / `AppImage` + repository metadata; Sigstore cosign for the build artifacts themselves.
4. **Auto-update**: Sigstore cosign for the artifact, TUF (via the `tough` crate) for the update metadata chain.
5. **Reproducible builds**: same source → same hash, so the signature is meaningful.
6. **SBOMs**: an SPDX SBOM per release for supply-chain transparency.

## Research Summary

### Windows: Authenticode, OV vs EV, SmartScreen

- **Authenticode** is the standard Microsoft code-signing format. The certificate must be from a trusted CA and must include a timestamp from a trusted TSA (RFC 3161).
- **OV (Organization Validation)** vs **EV (Extended Validation)**:
  - OV validates the organization. Cost: $130-300/year from providers like Sectigo, SSL.com, DigiCert.
  - EV adds stricter identity checks (usually 7-14 business days). Cost: $400-700/year. EV historically gave SmartScreen instant reputation; **as of August 2024 Microsoft no longer gives EV a special SmartScreen advantage for some flows** (changes per product area), so the main reason to pay for EV in 2024-2026 is enterprise procurement, policy, or HSM-backed key custody.
  - The dominant recommendation: **OV + timestamping + reputation building**.
- **SmartScreen**:
  - New publishers see "Unknown publisher" / "Windows protected your PC" warnings until enough clean installs build reputation.
  - Levers: consistent publisher identity (same subject name), clean code (no false positives from Defender), gradual rollout (insiders first), stable signing keys (don't rotate the cert unless forced).
- **Tools**: `signtool.exe` (Windows SDK), `osslsigncode` (cross-platform). The `x509-issuer-sign` Rust crate exists; for production we use the system tools.
- **HVCI (Hypervisor-Protected Code Integrity) / Memory Integrity**:
  - Affects **kernel drivers**, not user-mode binaries like ours.
  - Our `host-agent` is user-mode. No HVCI compliance work needed.
  - **Exception**: if we ever ship a kernel component (e.g., for low-latency input on Windows), it must be HLK-tested and dashboard-signed.
- **Repository signing**: not a Windows concept; the .msi / .exe IS the installable.

### macOS: Developer ID, Hardened Runtime, Notarization

- **Apple Developer Program**: $99/year. Required for any distribution outside the App Store.
- **Developer ID Application** certificate: the cert used to sign apps distributed outside the App Store.
- **Hardened Runtime** (`--options runtime`): required for notarization. Limits JIT, blocks unsigned executable memory, blocks DYLD env vars.
- **Notarization** workflow (Xcode 13+):
  1. Sign the `.app` with `codesign --options runtime --timestamp`.
  2. Package to `.dmg` or `.zip` (use `ditto`, not `zip`, to preserve the bundle structure).
  3. Submit via `xcrun notarytool submit ... --wait` (or `--keychain-profile "MyProfile"` after `store-credentials`).
  4. On success, `xcrun stapler staple ...` to attach the ticket to the artifact (for offline Gatekeeper checks).
  5. Verify with `xcrun stapler validate ...` and `spctl --assess --type execute ...`.
- **Gatekeeper**: if a binary is downloaded from the internet, macOS attaches the `com.apple.quarantine` xattr; on launch, Gatekeeper runs `spctl` and blocks the app if it's not signed + notarized.
- **Stapling**: the notarization ticket is normally fetched from Apple's servers, but stapling embeds it so the user can launch the app without internet.
- **Entitlements**: required for capability use. For a game streaming host we need:
  - `com.apple.security.app-sandbox` (optional; not needed for a full-privilege host).
  - `com.apple.security.network.server` (signaling server).
  - `com.apple.security.network.client` (client-cli).
  - `com.apple.security.device.camera` (capture; not needed for SCK which uses Screen Recording permission instead).
  - `com.apple.security.device.audio-input` / `-output`.
  - `com.apple.security.device.bluetooth` (gamepad).

### Linux: GPG signing, package signing, repository keys

- **.deb / apt**:
  - Repository metadata is signed with GPG (or `sq`/Sequoia in 2024+); `apt` trusts the key.
  - Packages themselves can also be signed (less common).
  - Key rotation: 5-year typical; ship a key transition plan (old key + new key for 12 months).
- **.rpm / dnf / yum**:
  - Packages are GPG-signed.
  - Repositories have GPG keys in `/etc/pki/rpm-gpg/`.
  - Modern Fedora uses `rpm-sequoia` and supports reproducible builds.
- **AppImage**:
  - A portable single-file artifact. Not repository-managed.
  - Sign with GPG (detached) or Sigstore cosign.
- **Flatpak**:
  - Flathub requires GPG signing of the `.flatpak` bundle.
- **Snap**:
  - Auto-updated via the Snap Store; signature is handled by the store.

### Sigstore cosign

- **cosign** is a tool for signing OCI artifacts (container images, binaries, etc.).
- **Keyless signing** uses OIDC identity (e.g., GitHub Actions `id-token: write`) → short-lived cert from **Fulcio** → signature recorded in **Rekor** transparency log.
- **Verify** checks the signature, the Fulcio certificate subject, and the Rekor inclusion proof.
- **Use case for us**: sign each build artifact and publish the signature + cert + Rekor entry. The auto-updater (P1-13) downloads the artifact, the signature, the cert, and queries Rekor to verify. If the OIDC subject matches `repo:org/qubox@refs/tags/v1.2.3`, the artifact is trusted.
- **Alternative**: classic `cosign sign --key cosign.key` with a long-lived key. Simpler but less safe.

### TUF (The Update Framework) via the `tough` crate

- TUF protects the **update metadata** chain:
  - `root.json` (keys + roles)
  - `targets.json` (artifact hashes + sizes)
  - `snapshot.json` (versions of `targets.json`)
  - `timestamp.json` (latest `snapshot.json`)
- **Prevents**:
  - Rollback attacks (downgrade to an old version)
  - Freeze attacks (refuse to update because metadata is stale)
  - Compromised mirrors (the metadata chain is signed)
  - Endless data attacks (size limits)
- **`tough` crate** (used by Bottlerocket, AWS IoT):
  - Rust client library; parses and verifies TUF metadata; downloads the artifact and verifies hash + size against `targets.json`.
- **Use case for us**: the host-agent's auto-updater (P1-13) uses `tough` to download a new release. The TUF repo is hosted on the signaling server (or a CDN) and is updated by the release pipeline. Cosign signs the artifacts; TUF signs the metadata.

### Cost (2024-2026)

| Item | Provider / Type | Cost |
|------|-----------------|------|
| OV code-signing cert (Windows) | Sectigo, SSL.com, DigiCert | $130-300/year |
| EV code-signing cert (Windows) | Sectigo, SSL.com, DigiCert | $400-700/year + HSM |
| Apple Developer Program | Apple | $99/year |
| Linux GPG key | self-managed | $0 (key generation + HSM optional) |
| Sigstore cosign | open source | $0 (the public Rekor is free; private Rekor costs ops) |
| TUF repo hosting | self-hosted or CDN | varies |

For the v1 release: **OV + Apple Developer Program + GPG + cosign + TUF = ~$250/year**.

### CI integration

- **GitHub Actions**:
  - **Windows runner** (`windows-latest`): `signtool sign /fd sha256 /tr http://timestamp.digicert.com /td sha256 /f cert.pfx /p $CERT_PASSWORD file.exe`. The `.pfx` is base64-encoded in a repository secret.
  - **macOS runner** (`macos-latest`): `codesign --options runtime --timestamp --sign "Developer ID Application: ..." file.app`, then `xcrun notarytool submit --keychain-profile ...`, then `xcrun stapler staple ...`. The Apple ID + app-specific password are in secrets, stored in keychain once via `notarytool store-credentials`.
  - **Linux runner** (`ubuntu-latest`): `gpg --armor --detach-sign --pinentry-mode loopback --passphrase $GPG_PASSPHRASE file.deb`. For cosign, `cosign sign-blob --output-signature file.sig file` (or keyless via OIDC).
- **GitLab CI**: same pattern with `protected variables`.
- **Cosign keyless OIDC**:
  - GitHub Actions: `permissions: id-token: write` on the job.
  - cosign auto-detects `ACTIONS_ID_TOKEN_REQUEST_TOKEN` and uses it.
- **Verification in the auto-updater**:
  - `cosign verify-blob --certificate-identity-regexp 'https://github.com/.*/qubox' --certificate-oidc-issuer 'https://token.actions.githubusercontent.com' file.sig file`
  - `tough` validates the TUF metadata chain, then verifies the artifact hash against `targets.json`.

### Reproducible builds

- Pin the Rust toolchain in `rust-toolchain.toml`.
- Pin dependencies via `Cargo.lock` (we already do).
- Set `SOURCE_DATE_EPOCH` in CI (e.g., the commit timestamp) to normalize build times.
- Strip paths via `RUSTFLAGS="-C strip=symbols -C debuginfo=0"`.
- Use a deterministic linker: mold or wild with reproducible flags.
- Verify reproducibility: build the same commit twice, compare `sha256sum` of the binaries (modulo any non-deterministic metadata). Tools: `repro-build`, `diffoscope`.
- **Note**: full reproducibility is hard for Rust (proc-macros, LLVM). Aim for "binary is close enough that signing is meaningful" rather than bit-exact.

### SBOMs

- **SPDX** is the standard. Generate via `cargo sbom` (or `cyclonedx-bom` for CycloneDX).
- Include the SBOM in the release artifacts.
- Future-proofing for the EU Cyber Resilience Act (CRA) — SBOMs will be mandatory for software distributed in the EU by 2027.

## Implementation Plan

### Step 1: Acquire certs

- **OV code-signing cert** from Sectigo or SSL.com ($200/year).
- **Apple Developer Program** membership ($99/year) → Developer ID Application cert.
- Generate a **GPG keypair** for Linux packages. Store the private key on a YubiKey or in GitHub Actions secrets.

### Step 2: GitHub Actions: Windows signing

`.github/workflows/release.yml`:
```yaml
- name: Sign Windows binaries
  env:
    CERT_PFX: ${{ secrets.WINDOWS_CERT_PFX_BASE64 }}
    CERT_PASSWORD: ${{ secrets.WINDOWS_CERT_PASSWORD }}
  run: |
    echo "$CERT_PFX" | base64 -d > cert.pfx
    for f in dist/windows-x86_64/*.exe; do
      signtool sign /fd sha256 /tr http://timestamp.digicert.com /td sha256 /f cert.pfx /p "$CERT_PASSWORD" "$f"
    done
```

### Step 3: GitHub Actions: macOS notarization

```yaml
- name: Import signing cert
  env:
    APPLE_CERT_P12: ${{ secrets.APPLE_CERT_P12_BASE64 }}
    APPLE_CERT_PASSWORD: ${{ secrets.APPLE_CERT_PASSWORD }}
  run: |
    KEYCHAIN_PATH=$RUNNER_TEMP/qubox.keychain-db
    security create-keychain -p "$KEYCHAIN_PASSWORD" $KEYCHAIN_PATH
    security set-keychain-settings -lut 21600 $KEYCHAIN_PATH
    security unlock-keychain -p "$KEYCHAIN_PASSWORD" $KEYCHAIN_PATH
    echo "$APPLE_CERT_P12" | base64 -d > cert.p12
    security import cert.p12 -k $KEYCHAIN_PATH -P "$APPLE_CERT_PASSWORD" -T /usr/bin/codesign
    security set-key-partition-list -S apple-tool:,apple: -s -k "$KEYCHAIN_PASSWORD" $KEYCHAIN_PATH

- name: Sign + notarize
  env:
    APPLE_ID: ${{ secrets.APPLE_ID }}
    APPLE_TEAM_ID: ${{ secrets.APPLE_TEAM_ID }}
    APPLE_PASSWORD: ${{ secrets.APPLE_APP_SPECIFIC_PASSWORD }}
  run: |
    codesign --options runtime --timestamp --sign "Developer ID Application: Qubox" dist/macos/host-agent.app
    ditto -c -k dist/macos/host-agent.app dist/host-agent.zip
    xcrun notarytool store-credentials "Qubox" --apple-id "$APPLE_ID" --team-id "$APPLE_TEAM_ID" --password "$APPLE_PASSWORD"
    xcrun notarytool submit dist/host-agent.zip --keychain-profile "Qubox" --wait
    xcrun stapler staple dist/macos/host-agent.app
    ditto -c -k dist/macos/host-agent.app dist/host-agent-notarized.zip
```

### Step 4: GitHub Actions: Linux GPG + cosign

```yaml
- name: Sign Linux packages
  env:
    GPG_PRIVATE_KEY: ${{ secrets.GPG_PRIVATE_KEY }}
    GPG_PASSPHRASE: ${{ secrets.GPG_PASSPHRASE }}
  run: |
    echo "$GPG_PRIVATE_KEY" | gpg --import --batch --pinentry-mode loopback --passphrase "$GPG_PASSPHRASE"
    for f in dist/linux/*.deb dist/linux/*.rpm; do
      gpg --armor --detach-sign --pinentry-mode loopback --passphrase "$GPG_PASSPHRASE" --sign-with default "$f"
    done

- name: cosign sign-blob (keyless)
  run: |
    for f in dist/linux/host-agent.AppImage; do
      cosign sign-blob --yes --output-signature "$f.sig" --output-certificate "$f.pem" "$f"
    done
```

### Step 5: TUF repository

- Host TUF metadata on the signaling server (or a dedicated CDN).
- The release pipeline:
  1. Builds artifacts.
  2. Signs each artifact (Authenticode, Developer ID + notarize, GPG, cosign).
  3. Computes SHA-256 of each artifact.
  4. Generates new `targets.json`, `snapshot.json`, `timestamp.json`.
  5. Signs the new TUF metadata with the release key.
  6. Uploads to the TUF repo.
- The `tough` crate in the host-agent's auto-updater (P1-13) fetches and verifies the chain.

### Step 6: Reproducible builds

- Pin the Rust toolchain in `rust-toolchain.toml`.
- Set `SOURCE_DATE_EPOCH` in CI.
- Strip binaries.
- Add a `repro-check` job to the release pipeline: build twice, compare hashes.
- Document the residual non-determinism in `docs/reproducible-builds.md`.

### Step 7: SBOMs

- Add a `sbom` step to the release pipeline.
- Generate CycloneDX or SPDX SBOMs via `cargo cyclonedx` or `cargo sbom`.
- Attach to the GitHub Release.

### Step 8: Auto-updater (P1-13) integration

- The host-agent's auto-updater (P1-13) downloads from the TUF repo.
- For each artifact:
  1. Verify the TUF chain with `tough`.
  2. Verify the artifact hash against `targets.json`.
  3. (Linux) Verify the GPG signature on the package.
  4. (Any) Verify the cosign signature on the raw binary.
  5. (Windows) Verify the Authenticode signature in the PE header.
  6. (macOS) Verify the Developer ID + notarization via `codesign -dv` + `spctl`.
  7. Replace the running binary; restart the daemon.

### Step 9: SmartScreen reputation

- Start with a small, opted-in beta cohort (the host-agent's existing users).
- Publish a clean, signed release.
- Iterate without changing the cert (preserve the subject name).
- Track the SmartScreen reputation via Microsoft Defender Security Center.
- Over time, expand distribution.

### Step 10: Documentation

- `docs/signing.md` — end-to-end signing guide for the maintainer.
- `docs/auto-update.md` — how the TUF + cosign chain works.
- `SECURITY.md` — how to report a vulnerability; how the signing infra is protected.

## Risks and Open Questions

- **Cert cost**: $200/year for OV + $99/year for Apple. Worth it for a real release; the cost of a SmartScreen warning or a Gatekeeper block is much higher (users won't install).
- **EV cert**: skip for v1; revisit if enterprise customers require it.
- **Apple Developer Program**: $99/year. Required for notarization. No way around it for macOS distribution outside the App Store.
- **GPG key custody**: a leaked GPG key means an attacker can sign malicious packages. Use a YubiKey for the master key; only the signing subkey lives in CI.
- **Cosign keyless OIDC**: depends on GitHub's OIDC issuer. The OIDC subject is `repo:org/qubox:ref:refs/tags/v1.2.3`. The auto-updater must verify this matches the expected release tag.
- **TUF metadata staleness**: the `timestamp.json` has a short expiry (e.g., 1 day). If the repo is unavailable for >1 day, clients refuse to update (fail-closed). Plan for high-availability TUF repo hosting.
- **Reproducible builds**: full bit-exact reproducibility is hard for Rust. Aim for "the same source produces a binary with the same SHA-256 of the stripped binary content (modulo timestamps and the COFF debug directory)". Document the residual non-determinism.
- **HVCI / kernel drivers**: not a v1 concern. If we later need a Windows kernel component (e.g., for low-latency input), it's a multi-month HLK test cycle.
- **SmartScreen reputation**: takes weeks to months. Plan a beta program.
- **macOS Gatekeeper**: if the user has a strict policy (e.g., App Allowlist), they may need to approve the Developer ID explicitly. Document this.
- **EU Cyber Resilience Act (CRA)**: by 2027, SBOMs and signed updates are mandatory for software distributed in the EU. Plan ahead.
- **Reproducible builds + native deps**: ffmpeg is a C library; reproducing its build is also required for full reproducibility. Use a known-good ffmpeg build (e.g., from the BtbN nightly builds) and pin its hash.
- **YubiKey for GPG signing in CI**: possible but adds hardware cost. For v1, store the GPG subkey in GitHub secrets; rotate annually.
- **Time**: setting up signing is a 1-2 week effort (cert acquisition, CI integration, testing).

## References

- Microsoft Authenticode: https://learn.microsoft.com/en-us/windows-hardware/drivers/install/authenticode
- Microsoft SmartScreen: https://learn.microsoft.com/en-us/windows/security/identity-protection/access-control/smart-screen
- Microsoft HVCI: https://learn.microsoft.com/en-us/windows/security/hardware-security/hypervisor-protected-code-integrity
- Microsoft driver signing offerings: https://learn.microsoft.com/en-us/windows-hardware/drivers/dashboard/driver-signing-offerings
- Sectigo code signing: https://www.sectigo.com/ssl-certificates-tls/code-signing
- SSL.com code signing: https://www.ssl.com/code-signing/
- Apple notarization: https://developer.apple.com/documentation/security/notarizing_macos_software_before_distribution
- Apple notarytool: https://developer.apple.com/documentation/security/notarizing_macos_software_before_distribution/customizing_the_notarization_workflow
- Apple hardened runtime: https://developer.apple.com/documentation/security/hardened_runtime
- Apple Developer ID: https://developer.apple.com/developer-id/
- Apple Gatekeeper: https://support.apple.com/guide/security/gatekeeper-sec5599b1eb6/web
- Sigstore: https://www.sigstore.dev/
- cosign: https://docs.sigstore.dev/
- Fulcio: https://docs.sigstore.dev/fulcio/overview
- Rekor: https://docs.sigstore.dev/rekor/overview
- TUF: https://theupdateframework.io/
- tough crate: https://crates.io/crates/tough
- Bottlerocket (uses tough): https://github.com/bottlerocket-os/bottlerocket
- GPG signing: https://www.gnupg.org/documentation/
- Debian package signing: https://www.debian.org/doc/manuals/securing-debian-manual/deb-pack-sign.en.html
- RPM package signing: https://docs.fedoraproject.org/en-US/package-signing/
- cosign keyless: https://docs.sigstore.dev/cosign/keyless/
- Reproducible builds: https://reproducible-builds.org/
- cargo-cyclonedx: https://crates.io/crates/cargo-cyclonedx
- cargo-sbom: https://crates.io/crates/cargo-sbom
- CycloneDX: https://cyclonedx.org/
- SPDX: https://spdx.dev/
- EU Cyber Resilience Act: https://www.enisa.europa.eu/topics/cyber-threats/cyber-resilience-act
- Forasoft Electron signing guide: https://www.forasoft.com/blog/article/electron-desktop-app-development-guide-for-business
- ivansingleton cert pricing: https://ivansingleton.dev/the-cheapest-code-signing-certificate-for-business-central-appsource-in-2026-a-complete-comparison-guide/
- electron-builder code signing: https://www.electron.build/docs/features/code-signing/
- Perplexity research, 2026-07-02: Authenticode, Apple notarization, GPG, Sigstore, TUF, HVCI, SmartScreen, CI integration, cost.
