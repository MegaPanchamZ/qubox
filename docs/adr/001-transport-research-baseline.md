# ADR-001 Transport Research Baseline

## Status

Accepted

## Context

Qubox needs a documented baseline for transport, media, input, and platform architecture before larger runtime changes are attempted. The current implementation is a prototype and the project needs a concrete comparison against Sunshine and Moonlight plus a survey of mature Rust ecosystem options.

## Decision

Capture the current research as repository-native documents under `research/references/` and use Sunshine plus Moonlight as the initial reference stack for reliability, packetization, recovery, input handling, and platform coverage.

## Consequences

- Follow-up implementation work should refer back to these research documents.
- No runtime behavior changes are introduced by this ADR.
- Library adoption remains open until a later implementation ADR.
