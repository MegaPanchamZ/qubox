# WebCodecs Browser Client

This directory will hold the TypeScript/Vite browser client for qubox.

**Implementation order** — see ADR-017 §15:
1. PR #1: proto additions (`TransportKind::WebTransport`, `WebTransportTicket`)
2. PR #2: `qubox-webtransport` crate skeleton
3. PR #3: `cert.rs` + unit tests
4. PR #4: `server.rs` + wtransport wiring
5. PR #5: `session.rs` handshake
6. PR #6: signaling glue (`webtransport.rs`)
7. **PR #7: Vite + TS scaffolding** ← next step
8. PR #8: `transport.ts` + ticket decoding
9. PR #9: `codec.ts` + Playwright spec
10. PR #10: `fec.ts` + `fecTransform.ts`
11. PR #11: `render.ts` (WebGL2)
12. PR #12: `input.ts` (Pointer Events)
13. PR #13: E2E pipeline test
14. PR #14: Firefox + Safari parity
15. PR #15: iOS Safari real-device validation

See `research/decisions/ADR-017-webcodecs-webtransport-browser-client.md` for full spec.
