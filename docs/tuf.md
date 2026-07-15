# TUF Auto-Update — Architecture

Qubox uses [TUF (The Update Framework)](https://theupdateframework.com/) to
secure auto-updates for `qubox-daemon`. This document explains the trust
model and operator responsibilities. The TUF module lives in
`apps/qubox-daemon/src/tuf.rs`; metadata is generated and signed by the
release maintainer.

> **Production deployments:** the TUF root key lives offline with the
> release maintainer. Daemons ship with a hard-coded TOFU root; subsequent
> rotations are pinned by the trust chain. Never commit the root key.

## Why TUF

A remote-desktop daemon running on millions of machines is a high-value
target. A single compromised binary pushed through the update channel is
game-over: it runs with full display capture, mic capture, and inbound
network reach. TUF provides:

- **Trust-on-first-use (TOFU)** via a root key shipped in the binary.
- **Hash-pinned metadata chains** so a single signing key compromise does
  not let an attacker serve mixed-version metadata (mix-and-match attack).
- **Threshold signing (N-of-M)** so the loss or compromise of one
  maintainer key does not enable malicious updates.
- **Rollback protection** via monotonically increasing version fields —
  the daemon rejects any metadata with `version <= current`.
- **Endless-data protection** via per-metadata `length` and `hashes`.

## Trust Hierarchy

```
         ┌────────────────────────────────────────────┐
         │ root.json                                  │
         │  • threshold = 1 (dev)  / 2-of-3 (prod)    │
         │  • renews: yearly                          │
         └────────────────────────────────────────────┘
                       │
        ┌──────────────┼──────────────┐
        ▼              ▼              ▼
   targets.json   snapshot.json   timestamp.json
   • lists       • pins hashes    • pins snapshot
     release       + versions       hash + version
     targets     • renews: per-   • renews: every
   • renews:       release          1 day (CI cron)
     per-release
```

Verification chain:

1. Daemon has `root.json` baked in (TOFU).
2. `root.json` says which key signs `targets.json` → verify.
3. `snapshot.json` carries `meta["targets.json"].hash` → verify.
4. `timestamp.json` carries `meta["snapshot.json"].hash` → verify, and
   reject if older than 24 h (forces frequent timestamp rotation).
5. Pick a target by name → verify length and sha256 against `targets.json`.
6. Stream the binary, verify sha256 against `targets.json`.

## Role Keys

- **Dev / single-maintainer:** one Ed25519 key for all four roles.
- **Production:** split into multiple role keys with threshold signing:
  - `targets` and `snapshot` keys online (or in CI).
  - `timestamp` key rotated daily by CI.
  - `root` key stored offline; required only for the yearly root renewal
    and emergency rotation.

## Metadata Lifecycle

| File           | Renewed by  | Cadence       | Stored |
|----------------|-------------|---------------|--------|
| `root.json`    | Maintainer  | Yearly        | In repo + shipped with daemon |
| `targets.json` | Maintainer  | Per release   | In repo + served to daemon |
| `snapshot.json`| Maintainer  | Per release   | In repo + served to daemon |
| `timestamp.json`| CI cron    | ~Daily        | Served to daemon            |
| `root.key`     | Maintainer  | Never         | **Never committed**, offline only |

## Threat Model

| Threat                         | TUF defense                                  |
|--------------------------------|----------------------------------------------|
| Compromised signing key        | Threshold signing + hardware key + audit log |
| Compromised repo hosting       | TUF version pinning + length/hash checks     |
| Rollback to old (vulnerable) version | Monotonic `version` fields             |
| Mix-and-match across metadata files | Each metadata file pins hash of next     |
| Endless-data DoS via huge target | Per-target `length` + sha256                |
| Subtle inconsistency (e.g. `targets.json` v3 paired with `snapshot.json` v2) | Snapshot pins targets version & hash |

## Compromise Playbook

If the root key is **suspected or confirmed compromised**:

1. Notify `security@qubox.app`.
2. Publish a new `root.json` listing only the new key.
3. Cut a `qubox-daemon` point release that ships the new root as the
   TOFU bootstrap; clients will pick it up on next launch and migrate.
4. Revoke the old key from any key-management tooling.
5. Audit log: which metadata was signed between suspected-compromise
   time and rotation? Re-publish those with the new key.
6. Publish a security advisory; rotate any associated secrets.

## Where To Go Next

- `apps/qubox-daemon/src/tuf.rs` — implementation
- `docs/security-hardening.md` — broader security model
- `SECURITY.md` — how to report vulnerabilities