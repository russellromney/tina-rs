# 097 WebSocket Production Hardening Stretch

## Status

- In progress on PR #88.
- This phase follows 094 and the 096 subprotocol/admission slice on PR #83.
- Goal: make Tina WebSocket servers feel production-shaped for bounded
  realtime rooms.
- Non-goal: do not claim full WebSocket replacement until the roadmap follow-up
  ships Autobahn classification and live replay.
- Hostile review pass completed in `review.md`; the plan already folds in the
  review findings around browser `wss://`, bounded reports, no sleep-based
  proof, load high-water assertions, and explicit deferrals.
- Implemented so far:
  - public handle-routed `WebSocketSessionHandle::report_effect(...)`;
  - `WebSocketSessionReport` / `WebSocketSessionReportOutcome`;
  - specimen Origin/auth/subprotocol admission policy;
  - browser `ws://` and `wss://` Playwright smoke path;
  - `TlsRoomServer` for local TLS browser/specimen proof;
  - health/readiness endpoints;
  - richer bounded room report counters and high-water fields;
  - CI-short load/churn/shutdown proof.
  - bounded single-room lifecycle for `default`:
    `POST /rooms/default` creates/reopens, `DELETE /rooms/default` deletes and
    closes current members, idle expiry deletes an empty room through a
    Tina-owned timer, and deleted rooms reject new upgrades until recreated;
  - public session reports include last close code and close reason byte count;
  - browser `wss://` smoke now asserts the served report path and browser close
    event instead of skipping TLS report proof.
- Checks run for this implementation slice:
  - `cargo fmt --all --check`;
  - `cargo test -p tina-http websocket --tests`;
  - `cargo test --manifest-path examples/specimen_websocket_room/Cargo.toml -- --test-threads=1`;
  - `cargo clippy -p tina-http --tests -- -D warnings`;
  - `cargo clippy --manifest-path examples/specimen_websocket_room/Cargo.toml --tests -- -D warnings`;
  - `RUSTDOCFLAGS="-D warnings" cargo doc -p tina-http --no-deps`;
  - `npm ci && npm run browser:smoke` from
    `examples/specimen_websocket_room`.

## Claim

After this phase, a user can copy the Tina WebSocket room/server shape for a
production-shaped bounded WebSocket service:

- browser clients over `ws://` and `wss://`;
- external Rust clients through documented `tungstenite` interop;
- explicit Origin/auth/subprotocol admission;
- clear close, timeout, pressure, and shutdown behavior;
- bounded rooms, sessions, reports, and load proof.

## Required Reads

- `.intent/SYSTEM.md`
- `ROADMAP.md`
- `.intent/phases/087-websocket-first-form/plan.md`
- `.intent/phases/094-websocket-usable-server/plan.md`
- `.intent/phases/096-websocket-production-replacement/plan.md`
- `docs/tina-user-guide/20-native-websocket-server.md`
- `examples/specimen_websocket_room`
- `tina-http/src/websocket.rs`
- `tina-http/src/connection.rs`
- `tina-http/tests/websocket_live.rs`

## Non-Goals

- No hidden Tokio.
- No unbounded room/member/session/report/load queues.
- No compression unless a later phase gives it budgets and proof.
- No native Tina WebSocket client in this phase; document `tungstenite` and
  browser clients as the supported client story.
- No Autobahn gate in this phase; it is the roadmap follow-up.
- No live trace to simulator replay in this phase; it is the roadmap follow-up.

## Rock 1: Admission And Browser WSS

Add boring production upgrade gates.

Required:

- upgrade context exposes path, headers, peer address when available, offered
  subprotocols, and extension offers;
- Origin allowlist helper or specimen shape;
- header/cookie bearer-token specimen shape;
- selected subprotocol happy path, fallback path, and rejection path;
- visible HTTP rejection status for bad Origin, bad auth, unsupported
  subprotocol, full room, and shutdown;
- Chromium browser `ws://` and `wss://` smoke with deterministic local trust.

Tests must prove text, binary, selected `ws.protocol`, close event, and rejected
upgrades from the browser/user path where practical.

## Rock 2: Close And Heartbeat Policy

Make close and liveness behavior explicit.

Required:

- app close, peer close, protocol close, idle timeout, pong timeout, and write
  timeout each have visible outcomes;
- configurable heartbeat knobs:
  - ping interval;
  - pong timeout;
  - idle timeout;
  - close handshake timeout;
- no fake cancellation: already-sent close/write work reports completion,
  timeout, full, closed, or stale;
- docs name which close codes Tina sends for policy closes.

Tests must use reports, barriers, deadlines, and socket deadlines. No sleeps as
proof.

## Rock 3: Session And Room Reports

Expose enough state to operate a room without reading traces.

Required session report fields:

- session id and generation/incarnation;
- selected subprotocol;
- close state;
- queued outbound frames;
- queued outbound bytes;
- active write bytes if tracked;
- last pressure reason;
- last close code and bounded reason byte count.

Required room/server report fields:

- active rooms;
- active members;
- capacity limits;
- rejected joins by reason;
- broadcast outcomes;
- slow-peer closes;
- shutdown/drain progress;
- high-water counts for sessions, queued frames, queued bytes, and room
  mailboxes.

Reports must be snapshots over bounded owned state. Do not add an event log
that grows with traffic.

## Rock 4: Production Room Lifecycle

Upgrade the specimen into a production-shaped room system, still boring and
copyable.

Required:

- bounded room registry: one named room, `default`, with explicit capacity `1`;
- bounded members per room;
- join and leave;
- room create/delete via `POST /rooms/default` and `DELETE /rooms/default`;
- idle room expiry through Tina-owned timer policy for empty active rooms;
- graceful room drain;
- listener shutdown rejects new rooms and drains existing rooms;
- health/readiness endpoints distinguish accepting, draining, shutting down,
  and stopped;
- stale handle proof across room deletion and recreation.

This may move from `examples/specimen_websocket_room` to `examples/systems` if
the code is no longer specimen-sized.

## Rock 5: Slow Peer And Load/Soak Proof

Prove bounded behavior under user-shaped pressure.

Required harness knobs:

- clients;
- rooms;
- members per room;
- message size;
- broadcast rate;
- slow-reader fraction;
- reconnect churn rate;
- duration or deterministic completion target.

Required proof:

- CI-short run;
- documented local-long run;
- slow readers trigger visible `Full`, `Closed`, or `Timeout`;
- healthy clients continue after slow-peer policy fires;
- reconnect churn does not leak live members;
- shutdown under load completes with a terminal report;
- high-water report includes sessions, rooms, queued frames, queued bytes,
  mailbox pressure, close codes, and resource counts before/after shutdown.

The harness must not collect one result per message forever. Aggregate counters
only.

## Rock 6: External Client Story

Keep the client story simple.

Required:

- docs say Tina server supports browser clients and `tungstenite` clients;
- specimen has `tungstenite` `ws://` and `wss://` tests;
- docs say a Tina-native WebSocket client is future work unless this phase
  deliberately adds it;
- examples show TLS trust setup for local tests without disabling all checks in
  production prose.

## Hardcore E2E Gate

Before final status can say "shipped":

- `cargo test -p tina-http websocket --tests`;
- full room/system tests with socket deadlines and deterministic barriers;
- browser `ws://` and `wss://` Playwright smoke;
- slow-peer load CI-short run;
- shutdown-under-load run;
- clippy for touched crates;
- docs build for touched docs;
- `git diff --check`.

At least one test must prove each bad user path:

- bad Origin rejected;
- bad auth rejected;
- unsupported subprotocol rejected;
- full room rejected;
- shutdown rejects new upgrade;
- slow peer removed;
- healthy peer still receives after slow peer;
- stale handle cannot hit recreated room/session;
- app close, peer close, protocol close, timeout close are distinguishable;
- browser `wss://` can send, receive, and close.

## Deferred To Roadmap Follow-Up

- Autobahn compliance classification.
- Live trace to simulator replay for WebSocket facts.
- Tina-native WebSocket client, unless a real workload demands it sooner.
- `permessage-deflate` support, unless bounded compression becomes a product
  requirement.
