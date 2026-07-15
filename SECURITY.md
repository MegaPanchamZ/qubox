# Security Policy

The Qubox project takes the security of its software seriously. This
document explains how to report vulnerabilities, what to expect from the
maintainers, and how we handle disclosures.

## Supported versions

| Version | Supported          |
| ------- | ------------------ |
| 0.1.x   | Yes (pre-1.0 development; security fixes backported on request) |
| < 0.1.0 | No                 |

During the 0.x development cycle we ship breaking changes and the API is
not yet stable. We will publish security advisories on GitHub for any
issue that affects a released version. Adopters who need a stable target
should pin to a specific git revision.

## Reporting a vulnerability

**Please do not open a public GitHub issue for security issues.**

Report privately by email to **security@qubox.app** (PGP key on
request). We acknowledge reports within **3 business days** and aim to
publish a fix or mitigation within **30 days** for high-severity issues.

Please include in your report:

1. A clear description of the issue and its impact.
2. Reproduction steps or a proof-of-concept. Self-contained reproducers
   are strongly preferred.
3. The affected version(s) and commit SHA(s).
4. Your assessment of severity (Critical / High / Medium / Low) and any
   suggested CVSS vector.
5. Whether you intend to disclose publicly, and on what timeline.

We follow **coordinated disclosure**: we will work with you on a
mutually-agreed disclosure date. Default embargo is 90 days from report
or until a fix is shipped, whichever comes first. Extensions are
granted on request for complex issues.

## What we will do

- Confirm receipt of your report within 3 business days.
- Triage and assign a severity within 7 business days.
- Develop and ship a fix on the timeline above.
- Credit you in the security advisory (unless you ask to remain
  anonymous).
- Coordinate the GitHub Security Advisory and CVE request (via GitHub
  CNA where applicable).

## Scope

In-scope targets include everything in this repository:

- `qubox-daemon` and its IPC protocol
- The QUIC / WebRTC transport stack
- The signaling protocol used by host / client sessions
- The TUF update channel and root key management
- Build, install, and update scripts in `ops/` and `apps/*/dist/`
- CI workflows under `.github/workflows/`

Out of scope by default: research-grade demos, third-party Coturn
configurations, the VM lab, and any path explicitly marked
**EXPERIMENTAL** in source. We will still triage reports against those
surfaces but may decline to issue a CVE.

## Disclosure policy

After a fix ships we will:

1. Publish a GitHub Security Advisory with full technical details.
2. Update `CHANGELOG.md` with a `[SECURITY]` entry.
3. Tag the fix commit with `security:` in the conventional-commits
   subject prefix.
4. Announce on the project's public channels (GitHub Releases).

## Recognition

We maintain a Security Hall of Fame for researchers who follow this
policy. Hall of Fame entries are opt-in; contact
**security@qubox.app** to be listed.

## Safe-harbour

We will not pursue legal action against researchers who:

- Make a good-faith effort to avoid privacy violations, data
  destruction, or service disruption.
- Only interact with accounts they own or have explicit permission to
  access.
- Stop testing immediately if they encounter user data and report it to
  us.
- Do not exploit a vulnerability beyond what is necessary to
  demonstrate it.

This safe-harbour applies to research conducted against the software in
this repository. Infrastructure operated by the Qubox Cloud service
(<https://qubox.app>) is governed by the security policy linked from
that service; please report cloud-only issues through the in-product
channel or to **security@qubox.app** marked "cloud".

## Cryptography notes

Qubox uses standard primitives and libraries (RustCrypto for Ed25519,
ChaCha20-Poly1305, Argon2id, SHA-2; quinn for QUIC with rustls; TUF for
update integrity). We do not implement custom cryptography. See
`docs/security-hardening.md` for the full threat model.