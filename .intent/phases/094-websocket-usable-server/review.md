# 094 Hostile Review

## Verdict

Good follow-up phase. It names the difference between "first form exists" and
"users can build WebSocket apps on Tina".

Main risk: session handles can accidentally become a mini broker with hidden
queues. The implementation must keep one owner per stream and make every send
admission result typed.

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

## Watch During Implementation

- Active write bytes must count against outbound byte budget.
- App/session mailbox full must be distinguishable from wire closed.
- Close reason taxonomy should be useful but not huge.
- Public API names should be few and stable enough to document.
- Room specimen should prove real live multi-client behavior, not just unit
  pressure.
- Docs should include unsupported features right next to the happy path.

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
- pressure tests;
- copyable docs.

Without those, the honest phrase remains "WebSocket first form", not "you can
use Tina for WebSockets".
