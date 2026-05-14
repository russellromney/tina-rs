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

## Implementer Hostile Review

- Finding: The first handle implementation used ordinary `send(...)`, so
  runtime-level closed/full/stale rejection could disappear before the session
  owner saw it. Fixed by making room/fanout use call-shaped handle sends and
  `WebSocketSendOutcome::from_connection_call(...)`; owner admission, runtime
  full/closed/timeout, and stale session ids now all map to typed outcomes.
- Finding: The handle could have become a second writer if it owned stream
  write authority. Fixed by making `WebSocketSessionHandle` only build
  `HttpConnectionMsg::WebSocketSend`; all writes are admitted and performed by
  the existing connection/session owner.
- Finding: 087 echo code would break if `Open` / `Text` were replaced by
  handle-bearing variants. Fixed by adding `SessionOpen`, `SessionText`, and
  `SessionBinary` while keeping the original variants and
  `WebSocketSessionOutcome` path.
- Finding: The first room leaked members because close/closed messages did not
  carry session ids. Fixed by adding `SessionClose`, `SessionPressure`, and
  `SessionClosed`; the specimen removes members on those lifecycle messages and
  proves fill-close-refill live.
- Finding: The first room specimen did not assert bidirectional broadcast or
  slow-peer pressure. Fixed by asserting both client directions, call-shaped
  send outcomes, outbound byte pressure, member removal, and refill.
- Finding: TLS WebSocket e2e was missing. Fixed with
  `websocket_tls_text_and_ping_work`, which starts `HttpsListener`, performs a
  real rustls WebSocket upgrade, sends text, observes echo, and proves ping/pong.
- Finding: The session id generation originally used the app isolate
  generation, not the connection owner generation. Fixed by constructing
  `WebSocketSessionId` with the owning connection address generation; the
  handle also stores the owner target address generation.
- Finding: The room report named stale-handle rejection without asserting a
  live stale send. Fixed by retaining one removed handle, attempting a send
  after refill, and asserting the visible failed outcome in the specimen smoke.
- Finding: The public room send example still made users spell the low-level
  `call(handle.target(), handle.text(...))` shape, leaking the connection
  reply plumbing. Fixed by adding `WebSocketSessionHandle::{send,text,binary,
  close}_effect(...)` helpers and updating the room specimen to use them.
- Finding: The additive legacy-event compatibility queue was practically small
  but not explicitly capped. Fixed with a named cap on pending compatibility
  app messages so the code matches the no-hidden-unbounded-queue rule.
- Finding: Rereading the roadmap shows this is not WebSocket "full support".
  It is the 094 bounded server usability slice. Full readiness still needs
  protocol/compliance breadth, browser/soak evidence, simulator/live-replay
  facts for the byte path, and production realtime-room lifecycle proof.
- Finding: Deferring fragmentation as "reject forever" left a core protocol
  gap when bounded reassembly is small enough to fit the session owner. Fixed
  by adding bounded data-message reassembly with interleaved control-frame
  support and live tests for text, binary, malformed continuation, and
  `max_message_bytes` pressure.
- Finding: The room proof still leaned on raw local helpers. That proved bytes
  but not the user path. Fixed by adding a reusable room server harness, `/`
  browser smoke page, `/room-report` counters, real `tungstenite` clients over
  `ws://`, and a real `tungstenite` client over `wss://` on top of rustls.
- Finding: Shutdown was still host/runtime-only. Fixed at the room layer with
  `WebSocketSessionMsg::Shutdown`, which closes stored handles through the
  same bounded owner path and rejects new upgrades once shutdown starts.
- Finding: The first real Chromium smoke failed because browsers offer
  `permessage-deflate` by default and Tina rejected the extension header.
  Fixed by ignoring unsupported extension offers without negotiating them, and
  by adding a focused test proving no `Sec-WebSocket-Extensions` response is
  emitted.
- Finding: The specimen had room-scale gaps: capacity+1, repeated reconnect,
  many-client shutdown, HTTP route coexistence, and shutdown during activity.
  Fixed with real-client tests for each path. The broad load/soak harness is
  still deferred.

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
