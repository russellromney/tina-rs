# 096 WebSocket Production Replacement

## Status

- In progress on PR #83 as a continuation of Phase 094.
- Implemented so far:
  - public subprotocol offer inspection with
    `WebSocketUpgradeRequest::offered_subprotocols()`;
  - selected subprotocol acceptance with `accept_subprotocol(...)`;
  - selected protocol emitted as `Sec-WebSocket-Protocol` in the `101`;
  - `WebSocketSessionMsg::SessionAccepted` so apps can observe the selected
    protocol;
  - extension offer inspection with `extension_offers()`;
  - browser/specimen path requests and proves `tina.room.v1`.
- Not complete yet: browser `wss://`, full production room system, load/soak
  harness, production ops reports, Autobahn classification, native-client
  decision implementation, and live trace to simulator replay.
- Follow-up split after review:
  - Phase 097 owns the stretch-but-implementable production-server hardening:
    browser `wss://`, admission/auth/subprotocol hardening, close/heartbeat
    policy, session/room reports, production room lifecycle, load/soak proof,
    and external-client docs.
  - The roadmap follow-up owns the broader replacement gates: Autobahn
    classification, live trace to simulator replay, Tina-native client if still
    desired, and bounded compression if it becomes a product requirement.
- Checks run for this in-progress slice:
  - `cargo fmt --all --check`;
  - `cargo test -p tina-http websocket --tests`;
  - `cargo test --manifest-path examples/specimen_websocket_room/Cargo.toml -- --test-threads=1`;
  - `cargo clippy -p tina-http --tests -- -D warnings`;
  - `cargo clippy --manifest-path examples/specimen_websocket_room/Cargo.toml --tests -- -D warnings`;
  - `RUSTDOCFLAGS="-D warnings" cargo doc -p tina-http --no-deps`;
  - `npm ci && npm run browser:smoke` from
    `examples/specimen_websocket_room`;
  - `git diff --check`.
- The original 096 monster phase is now split on purpose. Keep the current PR
  honest: 096 started the production replacement push, 097 carries the
  production-server stretch, and the roadmap names the remaining replacement
  follow-up.
- Phase 094 makes Tina usable for bounded server-side HTTP/1.1 WebSockets.
  Phase 096 is the bar for saying:

```text
You can replace a production WebSocket stack with Tina for bounded realtime
rooms and protocol clients.
```

## Claim

After this phase, Tina may claim production-shaped WebSocket replacement for:

- HTTP/1.1 WebSocket servers over Tina-owned TCP and TLS;
- browser clients over `ws://` and `wss://`;
- external Rust clients, including `tungstenite`;
- Tina-native WebSocket client first form, if we decide to own a client story;
- bounded realtime rooms with lifecycle, shutdown, health/readiness, liveness,
  observability, slow-peer policy, reconnect/resume specimen, and load/soak
  proof;
- standards/compliance posture with classified Autobahn results;
- live trace capture and simulator replay for representative WebSocket byte and
  session facts.

This still does not mean every ecosystem feature is implemented. It means the
remaining gaps are explicit product choices, not unknown readiness holes.

## Non-Goals

- No hidden Tokio runtime.
- No unbounded room, broadcast, client, replay, or compliance queues.
- No magical reconnect framework that hides delivery loss.
- No compression unless `permessage-deflate` is bounded and proven.
- No HTTP/2 WebSocket unless explicitly added and tested inside this phase.
- No pretending external tools are proof unless their outputs are checked into
  the status/review with dates, commands, and pass/fail classification.

## Required Reads

- `.intent/SYSTEM.md`
- `ROADMAP.md`
- `.intent/phases/087-websocket-first-form/plan.md`
- `.intent/phases/087-websocket-first-form/review.md`
- `.intent/phases/094-websocket-usable-server/plan.md`
- `.intent/phases/094-websocket-usable-server/review.md`
- `docs/tina-user-guide/20-native-websocket-server.md`
- `examples/specimen_websocket_room`
- `tina-http/src/websocket.rs`
- `tina-http/src/connection.rs`
- `tina-http/tests/websocket_live.rs`

## Rock 1: Standards And Compliance

Add an Autobahn server compliance harness.

Required shape:

- one documented command to run the suite locally;
- one checked-in classification artifact under the phase directory or docs;
- every case classified as `pass`, `intentional-unsupported`, or `bug`;
- failing `bug` cases either fixed in this phase or explicitly listed in the
  final status as a blocker to the replacement claim;
- browser-default extension offers must remain interoperable without
  negotiating unsupported extensions;
- RSV, masking, close payloads, fragmentation, continuation state, UTF-8, ping,
  pong, and close semantics must be covered by either Autobahn or local tests.

If `permessage-deflate` stays unsupported, the server must decline it by
omission and tests must prove browser clients still connect. If compression is
implemented, it must have explicit memory, CPU, and reassembly budgets.

## Rock 2: Browser And Real Client Matrix

Make browser proof required, not optional.

Required tests:

- Chromium `ws://` opens, sends text, sends binary, receives text, receives
  binary, closes with expected event;
- Chromium `wss://` with deterministic local trust setup;
- Firefox or WebKit smoke if the local/CI tooling can run it without making the
  phase flaky; otherwise document why Chromium is the required CI browser;
- external Rust `tungstenite` over `ws://`;
- external Rust `tungstenite` over `wss://`;
- browser default `permessage-deflate` offer is ignored unless negotiated;
- subprotocol negotiation succeeds and failure cases reject visibly.

The browser smoke must be in CI or a named required command. A served HTML file
alone is not proof.

## Rock 3: Upgrade Admission, Subprotocol, Auth Hooks

Add production upgrade admission surfaces without building an auth framework.

Required APIs or specimen shapes:

- read-only upgrade context carrying method, path, headers, peer address when
  available, offered subprotocols, and extension offers;
- app-controlled accept/reject decision;
- selected subprotocol returned in the `101` response;
- visible rejection status/reason for bad origin, bad auth, unsupported
  subprotocol, full room, and shutdown;
- docs for Origin checks, bearer token/header checks, cookies, tenant id, and
  reverse-proxy forwarded headers;
- tests for Origin, cookie/header admission, subprotocol selection, and
  subprotocol rejection.

Keep the simple 087/094 `websocket_upgrade` path working or provide a boring
adapter.

## Rock 4: Production Realtime Room System

Upgrade from specimen room to production-shaped realtime-room system.

Required components:

- room registry with bounded active-room capacity;
- bounded members per room;
- join/leave lifecycle;
- room create/delete;
- idle room expiry;
- liveness tick for ping/pong or app-level heartbeat;
- graceful room drain;
- listener shutdown and room shutdown orchestration;
- health and readiness endpoints;
- report endpoint with room/session/capacity counters;
- slow-peer policy library or copyable local policies:
  `disconnect`, `drop_newest`, `drop_oldest`, and `timeout`;
- stale-handle proof across room deletion and recreation;
- reconnect/resume specimen that names message loss semantics plainly.

This can live under `examples/systems` if it is broader than a specimen.

## Rock 5: Load And Soak Harness

Add a reusable WebSocket load/soak harness.

Required proof:

- configurable clients, rooms, members per room, message size, broadcast rate,
  slow-reader fraction, churn rate, and duration;
- CI-friendly short run;
- longer local run documented;
- high-water reports for member count, room count, outbound queue frames,
  outbound queued bytes, active write bytes, app mailbox pressure, send outcome
  counts, close codes, and resource counts;
- no unbounded client-side result collection;
- no sleeps as proof: use barriers, reports, deadlines, and deterministic
  completion conditions;
- shutdown under load tested.

The final phase status must include exact run commands and summary numbers.

## Rock 6: Native Client Story

Decide and implement one of two paths:

1. Tina owns a native WebSocket client first form.
2. Tina explicitly documents external clients as the supported client story.

If native client is chosen, required shape:

- Tina-owned TCP/TLS I/O;
- bounded read/frame/message/outbound queues;
- text/binary/ping/pong/close;
- fragmented receive under `max_message_bytes`;
- visible connection and send outcomes;
- browser/external-client interop tests against Tina server and one external
  server fixture.

If external clients are chosen, docs must say so plainly and examples must show
browser and `tungstenite` as the supported client path.

## Rock 7: Live Trace To Simulator Replay

Capture enough live facts to replay representative WebSocket failures.

Required facts:

- upgrade accepted/rejected;
- frame received/sent metadata;
- message reassembled;
- pressure outcome;
- close sent/received;
- stale handle rejected;
- shutdown started/completed;
- room member join/leave;
- selected subprotocol.

Required proof:

- record at least one live browser or `tungstenite` room run;
- replay the equivalent session/room facts in `tina-sim`;
- reject unsupported facts loudly rather than silently dropping them;
- include at least one replayed bug-shaped case: slow peer, malformed
  fragmentation, stale handle, or shutdown race.

## Rock 8: Observability And Operations

Expose production-useful reports and docs.

Required surfaces:

- per-session report;
- per-room report;
- listener/server report;
- capacity report projection;
- close-code counts;
- send outcome counts;
- ping/pong health;
- shutdown state;
- rejected-upgrade counts by reason;
- high-water counters;
- trace/correlation names that let a user connect room/session reports to
  runtime events.

Required docs:

- reverse proxy and TLS termination notes;
- idle timeout and ping interval notes;
- browser behavior notes;
- load balancer affinity notes;
- file descriptor and OS socket limit notes;
- capacity planning example;
- what is unsupported.

## Rock 9: CI And Release Gate

Before merging, the PR must run:

- `cargo fmt --all --check`
- `cargo test -p tina-http websocket --tests`
- `cargo clippy -p tina-http --tests -- -D warnings`
- `cargo test --manifest-path examples/specimen_websocket_room/Cargo.toml`
- production room/system tests
- browser `ws://` smoke
- browser `wss://` smoke
- `tungstenite` `ws://` and `wss://` e2e
- Autobahn harness, with classification artifact updated
- CI-friendly load/soak command
- live trace capture and simulator replay command
- `RUSTDOCFLAGS="-D warnings" cargo doc -p tina-http --no-deps`

If a command is intentionally not required in default CI because it is slow or
requires Docker/browser installation, the plan status must say exactly where it
runs and what artifact proves it.

## Cut Line

Because this is one phase, the cut line is not "ship half and defer the rest".
The cut line is:

- no replacement claim until all rocks either pass or are downgraded by an
  explicit roadmap decision;
- if Autobahn reveals protocol bugs, fix them before claiming replacement;
- if browser `wss://` is flaky, fix trust/setup rather than documenting around
  it;
- if load/soak cannot stay bounded, the implementation is not production-ready;
- if simulator replay cannot represent the byte/session facts, call that out as
  a Tina capability gap.

## Final Status Must Include

- exact public API names added or changed;
- whether compression is unsupported or implemented;
- subprotocol API shape;
- upgrade admission API shape;
- production room/system storage shape;
- slow-peer policies and defaults;
- native client decision;
- browser matrix;
- Autobahn artifact path and summary;
- load/soak summary;
- live replay artifact path and summary;
- exact checks run;
- hostile review findings and fixes.
