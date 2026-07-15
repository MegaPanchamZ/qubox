# ADR-021 Dual-mode control plane (self-host + managed)

**Status:** Accepted  
**Date:** 2026-07-12

## Context

Qubox needs two product surfaces that share one peer protocol:

1. **Self-host** — single Docker/compose, pair-only, no accounts required.
2. **Managed** — login, multi-tenant isolation, device enroll, pair policy, audit, TURN fleet.

Shipping “managed” by exposing the self-host binary on a public IP is not a product.

## Decision

| Mode | Control plane | Pairing | Identity |
|------|---------------|---------|----------|
| Self-host | `qubox-signaling-server` only | Local pair grants (JSON/SQLite path) | Local `DeviceIdentity` + `SignedHello` |
| Managed | Accounts API + tenant-scoped signaling | `PairPolicy` in account store | OIDC account → enrolled device cert |

**Same wire protocol** (`qubox-proto`): Hello/SignedHello, pair, session, relay.  
**Different backends:** pair-only store vs tenant + account store.

Managed media rule: cloud **never decrypts** media. Optional opaque QUIC relay only.

## Defaults

- Public / managed: `SignedHello` required; no `--auto-approve-pairing` in release UX.
- Self-host LAN migration: opt-in `--allow-unsigned-hello` only.
- Session credentials: HMAC-bound to host/client pubkeys (`SessionCredential::issue`).
- TURN: issue only with valid session credential.

## Scaffold

- `crates/qubox-accounts` — types + `MemoryAccountStore` (Postgres later).
- `ops/self-host/` — compose (signaling + coturn + optional Caddy).

## Consequences

- Do not stuff OIDC into the peer signaling lib forever; keep managed front separate.
- Self-host remains open-core path; managed is a hosted control plane + edge.
