# 096 Hostile Review

## Verdict

The phase is big on purpose. That is acceptable only if the plan refuses to
let "one phase" become "one vague bucket".

The replacement claim is dangerous. Users will hear "I can move production
traffic from my current WebSocket stack to Tina." The phase must therefore
prove protocol compatibility, browser compatibility, long-lived operations,
bounded load, replay/debuggability, and room lifecycle in one coherent system.

## Findings Folded Into Plan

- Finding: "Production ready" could collapse into more happy-path e2e tests.
  Fixed by requiring Autobahn classification, load/soak, browser `wss://`, live
  replay, and production-room lifecycle before the replacement claim.
- Finding: Browser support can be accidentally fake. 094 already found that
  browsers offer `permessage-deflate` by default. Fixed by requiring real
  browser execution as a required command, not just served HTML or raw sockets.
- Finding: The phase could ship server-only support while users expect a client
  story. Fixed by forcing a native-client-versus-external-client decision with
  docs and tests either way.
- Finding: Auth/origin/subprotocols are often the first production blocker,
  even if the frame codec is correct. Fixed by requiring admission hooks and
  subprotocol negotiation.
- Finding: A room specimen is not enough for production replacement. Fixed by
  requiring a production-shaped room registry/system with health, readiness,
  shutdown, idle expiry, reports, and liveness.
- Finding: Load proof can become a pile of sleeps and logs. Fixed by requiring
  bounded harness parameters, high-water reports, deterministic completion, and
  checked summary numbers.
- Finding: "Tina can replay production bugs" would be hollow if WebSocket byte
  facts are not captured. Fixed by requiring live trace capture and simulator
  replay for representative WebSocket cases.
- Finding: Compression is a trap. Implementing `permessage-deflate` without
  strict budgets is worse than declining it. Fixed by requiring either
  explicit no-compression posture or bounded compression proof.
- Finding: One phase can become unreviewable. Fixed by naming rocks, commands,
  artifacts, and a final status checklist.

## Must Not Slip

- Do not claim replacement until Autobahn results are classified.
- Do not claim browser support without real browser `ws://` and `wss://`.
- Do not claim production room support without shutdown under load.
- Do not add an unbounded load harness, report collector, broadcast fanout, or
  replay event store.
- Do not hide auth/origin/subprotocol decisions in examples only.
- Do not make reconnect/resume look lossless unless it is actually durable.
- Do not implement compression without CPU and memory budgets.
- Do not let native client work smuggle in hidden Tokio or broad async runtime
  ownership.
- Do not make simulator replay silently ignore unsupported WebSocket facts.
- Do not leave slow/optional proof commands undocumented.

## Watch During Implementation

- `Sec-WebSocket-Protocol` must be selected once and exposed to app code.
- Browser `wss://` trust setup must be deterministic enough for CI.
- Close codes and rejection reasons must be visible to users and tests.
- Capacity high-water counters must be asserted, not printed.
- Load/soak should report resource counts before and after shutdown.
- Room deletion/recreation must not let stale handles hit new sessions.
- Replay facts must use typed records, not ad hoc log strings.
- Health/readiness should distinguish accepting, draining, shutting down, and
  stopped.
- Docs should say when to use external clients instead of a Tina-native client,
  if that decision is made.
- The final PR should be easy to review by rock: protocol, browser, admission,
  room system, load, client, replay, ops.

## Remaining Skepticism

This is a lot for one phase. The only way it stays sane is if implementation
lands behind small, independently reviewable rocks while preserving a single
phase/status file. The plan should not accept "we did most of it" as success.
Either all replacement gates are met, or the final status must say Tina still
has bounded WebSocket support but not replacement-grade WebSockets.

## Implementation Review So Far

- Finding: 094 had browser connectivity, but no production subprotocol path.
  Fixed by adding offered-subprotocol parsing, selected-subprotocol validation,
  `Sec-WebSocket-Protocol` response emission, and a `SessionAccepted` app event.
- Finding: The first subprotocol implementation could have selected a protocol
  the client never offered. Fixed by making `accept_subprotocol(...)` validate
  both token syntax and membership in the offered set.
- Finding: The first `accept_subprotocol(...)` shape consumed the upgrade
  request even on validation failure, which made admission fallback brittle for
  real apps. Fixed by making selected-protocol accept borrow the upgrade request
  and only clone the accept key after validation succeeds.
- Finding: Browser smoke should prove selected protocol, not only open. Fixed
  by making the specimen browser page request `tina.room.v1` and assert
  `ws.protocol` in the Playwright smoke.
- Finding: Extension offers need visibility for admission/compliance work.
  Fixed by exposing `extension_offers()` on the upgrade request while still
  declining unsupported extensions by omission.
- Finding: The specimen e2e tests were not isolated enough under default
  parallel `cargo test`; individual tests passed while the full live-room suite
  could fail or hang. Fixed with a local test mutex around live room tests, not
  sleeps.
- Finding: Several real-client specimen tests used blocking WebSocket reads
  without socket deadlines, so a missing broadcast or close could hang instead
  of failing. Fixed by routing specimen clients through local timeout helpers
  for TCP and TLS sockets.
- Finding: The raw copyable specimen client wrote frames without an explicit
  flush before waiting for peer-visible effects. Fixed by flushing text and
  close frames so the simplest specimen path proves the same behavior a user
  would see through a real client.
- Finding: Broadcast assertions could read before the room had published its
  visible send outcome, making failures depend on scheduler timing. Fixed by
  gating those reads on the room report and then verifying the receiving client
  frame.
- Finding: This does not complete 096. Autobahn, browser `wss://`, load/soak,
  live replay, native-client decision, and production room system remain
  replacement blockers.
