# 094 Hostile Review

## Verdict

Good follow-up phase. It names the difference between "first form exists" and
"users can build WebSocket apps on Tina".

Main risk: session handles can accidentally become a mini broker with hidden
queues. The implementation must keep one owner per stream and make every send
admission result typed.

Second risk: the plan can pass protocol unit tests while still not proving a
real user can build a room. The e2e proof must use public API, live sockets,
and the specimen's room path.

Third risk: the goal says TCP/TLS rails, but implementers may only test
cleartext. TLS WebSocket upgrade must be a required check or the claim is too
large.

Fourth risk: the phase is big enough to invite scope creep. The plan now has an
implementation order and cut line; keep fragmentation, browser smoke, and extra
helpers behind that line unless the core claim is already proved.

## Findings Folded Into Plan

- Added explicit TLS WebSocket e2e proof for the TCP/TLS rails claim.
- Required copied examples to compile through public API.
- Required the room specimen's `run()` report to prove the same path users run.
- Required live multi-client room behavior and a real non-reading/slow peer.
- Added public-API-only e2e proof and a grug implementation order.
- Required 087 echo-path compatibility to stay documented while room/fanout
  graduates to session handles.
- Required room/session pressure reports so users do not need trace spelunking.
- Required session handles to carry generation/incarnation and prove
  fill-close-refill.

## Must Not Slip

- Do not claim "Tina WebSockets" broadly. Claim native bounded server-side
  HTTP/1.1 WebSockets.
- Do not add a broad WebSocket client crate. Tiny test client helper only.
- Do not hide room fanout behind an unbounded list of pending sends.
- Do not turn session handles into logical user identities. They name one
  session incarnation.
- Do not half-implement fragmentation. Reject explicitly or implement bounded
  reassembly correctly.
- Do not use sleep-heavy flaky tests to prove slow peer pressure.
- Do not move ownership of an upgraded stream into two independent writers.
- Do not let stale session handles send to a new connection.
- Do not let closed sessions keep member-table capacity forever.
- Do not rely on private test helpers for copied docs/examples.
- Do not let the specimen's `run()` test a different path than the user-facing
  room.
- Do not say browser clients work unless docs show the handshake shape and CI
  proves standards-compatible framing on the server side.

## Watch During Implementation

- Active write bytes must count against outbound byte budget.
- App/session mailbox full must be distinguishable from wire closed.
- Close reason taxonomy should be useful but not huge.
- Public API names should be few and stable enough to document.
- Room specimen should prove real live multi-client behavior, not just unit
  pressure.
- Docs should include unsupported features right next to the happy path.
- TLS test should use `HttpsListener`, not a fake "same as TCP" assertion.
- Slow-peer pressure should use a live non-reading peer or deterministic
  equivalent, not just direct queue insertion.
- Public examples should compile, ideally as doctests or specimen code.
- The room report should be asserted by tests, not printed and eyeballed.
- Session id generation must be boring and visible enough to debug.

## Likely Deferrals

Acceptable to defer from 094 if documented clearly:

- HTTP/2 WebSocket;
- permessage-deflate;
- native broad client;
- browser session/auth helpers;
- automatic reconnect;
- full Autobahn suite.

Less acceptable to defer:

- session handles or equivalent bounded send API;
- real room specimen;
- public send outcome;
- room/session pressure report;
- pressure tests;
- copyable docs;
- TLS e2e;
- public-API-only room e2e.
- stale-handle/fill-close-refill proof.

Without those, the honest phrase remains "WebSocket first form", not "you can
use Tina for WebSockets".
