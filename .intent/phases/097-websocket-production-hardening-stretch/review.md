# 097 Hostile Review

## Verdict

This is the right size only if it stays about production-shaped server
operation, not full standards replacement. The plan must make real user paths
fail loudly: browser TLS, bad admission, slow peers, shutdown, stale handles,
and reconnect churn.

## Findings Folded Into Plan

- Finding: The phase could drift back into the huge 096 replacement bucket.
  Fixed by making Autobahn and live replay explicit roadmap follow-ups.
- Finding: "WSS works" can become a raw TLS client test. Fixed by requiring a
  Chromium browser `wss://` smoke with deterministic local trust.
- Finding: Admission hooks could become docs-only. Fixed by requiring Origin,
  auth, subprotocol, full-room, and shutdown rejection tests.
- Finding: Reports can become an unbounded event stream. Fixed by requiring
  bounded snapshots and aggregate counters only.
- Finding: Heartbeats often hide fake cancellation. Fixed by requiring visible
  close/write outcomes: completion, timeout, full, closed, or stale.
- Finding: Load tests often print vibes. Fixed by requiring CI-short and
  local-long commands with checked high-water summaries and before/after
  resource counts.
- Finding: Room lifecycle can become a framework. Fixed by keeping the target
  copyable and allowing an `examples/systems` move only if specimen size is no
  longer honest.
- Finding: Native client work can swallow the phase. Fixed by choosing browser
  plus `tungstenite` interop as the required client story.
- Finding: Timing flakes can masquerade as proof. Fixed by banning sleeps as
  proof and requiring barriers, reports, deadlines, and socket deadlines.

## Must Not Slip

- Do not claim full WebSocket replacement after this phase.
- Do not add an unbounded collector to make load reporting easy.
- Do not use a browser-served HTML file as browser proof.
- Do not disable TLS verification in docs except in clearly labeled local-test
  setup.
- Do not let room shutdown accept a new upgrade after readiness flips.
- Do not let stale handles succeed after room/session recreation.
- Do not make reconnect/resume look lossless.
- Do not make ping/pong timers depend on sleeps in tests.

## Extra Test Pressure

The plan should hurt regressions from the user's side:

- browser connects with `wss://`, sends text and binary, receives both, sees
  selected subprotocol, and observes close;
- raw/bad clients hit bad Origin, bad auth, bad subprotocol, malformed close,
  and protocol-error close;
- slow readers force `Full`/`Timeout` while healthy readers keep receiving;
- load run churns clients while rooms expire and refill;
- shutdown starts while broadcasts are in flight and still reaches a terminal
  report;
- every e2e client has read/write deadlines so missing frames fail instead of
  hanging.

## Remaining Skepticism

Even after this phase, two hard claims remain unproven:

- standards compatibility across Autobahn;
- production bug replay through live trace to simulator facts.

Those belong in the roadmap follow-up, not as quiet stretch promises here.

## Implementation Review So Far

- Finding: A first attempt had the room request a connection-owned session
  report during `SessionOpen`. That made the simple two-client broadcast path
  timing-sensitive because the app was asking the connection for a report while
  the connection was still unwinding the join/open sequence. Fixed by keeping
  the public report API but not auto-calling it during join; the specimen room
  report remains a bounded room-owned snapshot.
- Finding: Browser TLS proof could accidentally use the Rust TLS client only.
  Fixed by adding a TLS-start mode to the specimen binary and requiring the
  Playwright smoke to open the served page over `https://`, which then uses
  browser `wss://`.
- Finding: Admission could be invisible framework prose. Fixed by adding a
  local `AdmissionPolicy` and e2e rejection assertions for bad Origin, bad
  bearer token, and unsupported required subprotocol.
- Finding: Load proof could grow one result per message. Fixed by adding a
  CI-short churn test that asserts aggregate high-water and rejection counters
  only.
- Finding: The room lifecycle was still too report-shaped: `room_capacity` was
  visible, but create/delete/idle expiry were not explicit enough. Fixed by
  making the specimen a one-room bounded registry with `POST /rooms/default`,
  `DELETE /rooms/default`, idle expiry of empty rooms through a Tina timer, and
  e2e tests proving delete rejects, create refills, and idle expiry rejects
  until recreate.
- Finding: The public session report API exposed useful queue/close state but
  did not carry the last close code/reason shape. Fixed by adding
  `last_close_code` and `last_close_reason_bytes` to
  `WebSocketSessionReport`; this stays bounded and avoids a close-event log.
- Finding: The session report API could ossify into a broad observability
  surface. Fixed by marking `WebSocketSessionReport` `#[non_exhaustive]` and
  documenting it as a narrow connection-owner diagnostic snapshot. Room
  registry/admission/fanout/slow-peer helpers stay out of `tina-http` and are
  named as future helper-crate material.
- Finding: The browser `wss://` proof skipped `/room-report`. Fixed by fetching
  the report from inside the TLS-loaded browser page, then asserting selected
  subprotocol, close event, peer close, and live-member drain for both `ws://`
  and `wss://`.
