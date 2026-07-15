# Contributing to Qubox

Thanks for your interest in Qubox! This guide covers how to file
issues, send pull requests, run the build locally, and follow our
project conventions.

## Code of Conduct

This project follows the [Contributor Covenant v2.1](CODE_OF_CONDUCT.md).
By participating you agree to its terms.

## Getting started

1. **Read the README** — the [README](README.md) describes the
   architecture, supported platforms, and a quick-start of the
   workspace.
2. **Skim the docs** — `docs/` contains design notes for the major
   subsystems (transport, capture, signaling, identity, TUF updates).
3. **Set up a dev environment** — Qubox builds on Linux, macOS, and
   Windows. Minimum: Rust 1.78+, `git`, `pkg-config`, platform build
   tools (Xcode CLT, MSVC Build Tools, or the relevant `*-dev`
   packages).
4. **Run the tests** — `cargo test --workspace` from the repo root.

## How to file an issue

- **Bugs**: include platform, version/commit, exact reproduction
  steps, and the full `RUST_LOG=debug` output if relevant.
- **Feature requests**: describe the use case, not just the
  implementation; link related issues.
- **Questions / support**: open a discussion or use the issue tracker.
  Please do not include private credentials or hostnames.

## How to send a pull request

1. **Open an issue first** for non-trivial changes so we can agree on
   the direction.
2. **Fork** the repo and create a feature branch off `main`.
3. **Sign off** every commit (`git commit --signoff` or `-s`). The DCO
   bot checks for this on PRs. Sign-off certifies the
   [Developer Certificate of Origin 1.1](https://developercertificate.org/).
4. **Format and lint** before pushing:
   ```sh
   cargo fmt --all
   cargo clippy --workspace --all-targets -- -D warnings
   ```
5. **Write tests** for behavioural changes. New crates must have at
   least one smoke test in `tests/` or `#[cfg(test)]` modules.
6. **Update the docs** if you change a public API, the wire protocol,
   or the self-host flow.
7. **Open the PR** — describe the change, link related issues with
   `Closes #1234`.

## Commit message conventions

We follow [Conventional Commits 1.0](https://www.conventionalcommits.org/):

```
<type>(<scope>): <description>

[optional body]

[optional footer(s)]
```

Allowed types: `feat`, `fix`, `docs`, `style`, `refactor`, `perf`,
`test`, `build`, `ci`, `chore`, `revert`, `security`.

Examples:

- `feat(transport): add 0-RTT support for repeat sessions`
- `fix(daemon): restore TUF root on rollback`
- `docs: clarify QUIC ALPN negotiation`
- `security(daemon): validate pairing nonce length`

## Repo layout

Qubox is a multi-binary, multi-crate Rust workspace:

| Path                              | Role                                    |
|-----------------------------------|-----------------------------------------|
| `apps/qubox-signaling-server`     | WebSocket signaling + TURN cred issuer  |
| `apps/qubox-host-agent`           | Capture / input agent on the host       |
| `apps/qubox-client-cli`           | Native CLI client (Rust + wgpu)         |
| `apps/qubox-client-gui`           | Tauri/React GUI client                  |
| `apps/qubox-daemon`               | Background service (state, IPC, TUF)    |
| `crates/qubox-signaling`          | Signaling protocol (host + client)      |
| `crates/qubox-transport`          | QUIC + WebRTC data-channel multiplexer  |
| `crates/qubox-webtransport`       | WebTransport binding                    |
| `crates/qubox-display`            | Cross-platform capture backends         |
| `crates/qubox-clipboard`          | Clipboard sync + image hash dedup       |
| `crates/qubox-mic`                | Microphone capture + RNNoise + Opus     |
| `crates/qubox-pen`                | Pen / stylus capture                    |
| `crates/qubox-identity`           | Local identity keypair + signed hellos  |
| `crates/qubox-platform`           | OS-specific helpers                     |
| `crates/qubox-media`              | Media pipeline glue (test fixtures)     |
| `crates/qubox-proto`              | Wire protocol, frame types, codec config|
| `crates/qubox-sync`               | File sync engine                        |
| `crates/qubox-rl-policy`          | Adaptive bitrate feedback (optional)    |
| `clients/webcodecs`               | Browser-based client                    |

Public APIs are re-exported from each crate's `lib.rs`. Internal modules
are private. Anything `pub` in a non-root path is considered stable.

## Coding style

- Rust 2021 edition, MSRV 1.78.
- `cargo fmt --all` with default `rustfmt` config.
- `cargo clippy --workspace --all-targets -- -D warnings` must pass.
- Prefer `tracing` over `println!` for runtime diagnostics.
- Use `anyhow::Result` at the top of `fn main` / CLI boundaries;
  `thiserror`-style error enums inside library crates.
- Doc comments on every `pub` item; module-level `//!` on every file.
- Avoid `unsafe`. If you must, isolate it behind a `#[cfg(...)]` and
  add a SAFETY comment.

## Tests

- Unit tests live next to the code (`#[cfg(test)] mod tests`).
- Integration tests live in `tests/` directories.
- Run the full suite: `cargo test --workspace --all-features`.
- Run a single test: `cargo test -p qubox-display --lib -- capture::x11::tests`.

## Review process

- One approval from a maintainer is required.
- CI must be green (fmt + clippy + tests on Linux, macOS, Windows).
- Reviews typically take a few business days.
- Force-pushes are not allowed after review has begun — squash new
  commits onto the branch instead.

## Release process

Maintainers cut releases via the `.github/workflows/release.yml`
workflow. The release commit updates the changelog, tags the commit
(`vX.Y.Z`), builds signed artifacts, signs them with `cosign`, and
publishes a GitHub Release. The TUF channel is updated out-of-band by
maintainers with access to the offline root key — see `docs/tuf.md`.

## License

By contributing you agree that your contributions will be licensed
under the [GNU AGPLv3](LICENSE) (or, at your option, any later version).
You retain copyright on your contributions.