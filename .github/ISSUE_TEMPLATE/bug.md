---
name: Bug report
about: Report a defect or unexpected behaviour
title: "[bug]: "
labels: ["bug", "needs-triage"]
assignees: []
---

## Summary

<!-- One or two sentences. What broke, and what did you expect instead? -->

## Environment

| Field | Value |
| ----- | ----- |
| OS (distro + version) | |
| Architecture (x86_64 / arm64) | |
| Qubox version (commit SHA or tag) | |
| Component affected (client / daemon / host-agent / signaling / docs / build) | |
| Install method (cargo install / deb / rpm / AppImage / MSI / PKG) | |
| GPU vendor + driver version (capture issues only) | |
| Display server (X11 / Wayland / macOS / Windows) | |

## Reproduction steps

1.
2.
3.

## Expected behaviour

<!-- What should have happened? -->

## Actual behaviour

<!-- What actually happened? Include error messages verbatim. -->

## Logs / screenshots

<!-- Paste `RUST_LOG=debug qubaix-client-cli ...` output between the
     triple backticks below. Remove any secrets before pasting. -->

```text
PASTE LOGS HERE
```

## Severity

<!-- Pick one. If you are unsure, leave it as 'Unknown'. -->

- [ ] Critical (data loss, security, no workaround)
- [ ] High (major feature broken, partial workaround)
- [ ] Medium (feature degraded, easy workaround)
- [ ] Low (cosmetic / minor inconvenience)

## Possible cause

<!-- Optional. If you have already traced it to a file:line, drop it here. -->

## Anything else?

<!-- Cross-references, related issues, screenshots of similar projects. -->