# ADR-021 Control plane boundary (self-host OSS + external cloud)

**Status:** Accepted (supersedes earlier “in-repo managed scaffold” wording)  
**Date:** 2026-07-12  
**Updated:** 2026-07-17

## Context

Qubox has two product surfaces that share one **peer wire protocol**:

1. **Self-host (this repository, AGPL)** — signaling + clients + TURN; pair-only; no accounts product.
2. **Qubox Cloud (separate private product)** — login, multi-tenant isolation, device enroll, friends/access policy, audit, hosted edge.

Shipping “cloud” by exposing the self-host binary on a public IP is **not** a product. Cloud control-plane code (accounts API, OIDC, billing, managed enrollment HTTP client) does **not** live in this tree.

## Decision

| Surface | Control plane | Pairing | Identity |
|---------|---------------|---------|----------|
| **Self-host (OSS)** | `qubox-signaling-server` only | Local pair grants (JSON store) | Local `DeviceIdentity` + `SignedHello` |
| **Cloud (private)** | External accounts API + tenant-scoped signaling binary | `PairPolicy` / session auth in accounts | Account → enrolled device; friends/grants stay off the host |

**Same wire protocol** (`qubox-proto`): Hello/SignedHello, pair, session, relay.  
**Different backends:** pair-only store (OSS) vs accounts + managed signaling (private).

### Media rule (both surfaces)

Cloud and self-host relays **never decrypt media**. Optional opaque QUIC/TURN relay only. File sync and media stay peer-encrypted / peer-path.

### Optional library hook (not a cloud product)

`crates/qubox-signaling` may expose an **enrollment lookup trait** and an Open default so integrators (including Qubox Cloud’s private wrapper) can enforce “device must be known” without embedding a proprietary HTTP client in the public binary.

- **Public binary default:** Open enrollment (anyone who can reach the WS can participate subject to pairing).
- **Cloud binary (private repo):** implements lookup against its accounts API; not distributed as the stock OSS server entrypoint.

## Defaults (self-host)

- `SignedHello` required; no `--auto-approve-pairing` on internet-facing hosts.
- LAN migration only: opt-in `--allow-unsigned-hello`.
- Session credentials: HMAC-bound to host/client pubkeys (`SessionCredential::issue`).
- TURN: issue only with a valid session credential.
- TLS: use `ops/self-host` Caddy profile (`wss://`); do not expose raw `:7000` publicly.

## In this repository

- `apps/qubox-signaling-server` — Open self-host signaling.
- `ops/self-host/` — compose (signaling + coturn + optional Caddy).
- `ops/coturn/` — TURN image/config used by self-host compose.
- Clients default to **user-supplied** signaling URLs (self-host first).

## Not in this repository

- Accounts API, OIDC, Stripe/billing, friends graph, managed EC2/Caddy product stack.
- Hardcoded production hostnames as required defaults for first-run.

Those belong to the private Qubox Cloud product. See the public README “Managed cloud” note and <https://qubox.app>.

## Consequences

- Do **not** stuff OIDC, friends, or billing into `qubox-signaling` or the public server binary.
- Self-host remains a complete open product path (pair + stream + TURN).
- Cloud reuses the wire protocol and may wrap the signaling **library** privately; OSS docs never claim `crates/qubox-accounts` exists here.
- Integrators who need enforced enrollment implement the lookup trait outside this repo (or wait for a documented generic contract)—the stock server stays Open.
