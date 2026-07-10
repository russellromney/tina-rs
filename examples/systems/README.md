# System Specimens

System specimens are bigger than normal specimens.

Normal specimen:

```text
one sharp comparison
one rough bit found
small code
```

System specimen:

```text
small app
many Tina parts together
try hard to find rough bits
write down what hurts
```

They are still not benchmarks. They are not product demos. They are
cheap stress stories for the framework.

## Rules

- Build the app twice only when useful: Tokio side and Tina side.
- If Tokio side is boring boilerplate, keep it small.
- Tina side should use current blessed helpers.
- Do not hide Tina rough bits with a giant harness.
- Prefer one readable app over perfect feature parity.
- Every queue, pool, body, pending set, and bridge needs a cap.
- Every cap should be reported or easy to inspect.
- Every completed system needs at least one smoke test that actually
  runs.
- Each system README must show the exact smoke-test command.
- README is where feelings go.
- If a needed Tina feature does not exist, do not build it inside the
  system. Use the smallest stand-in, or stop and write the finding.

## Feedback Loop

Each system README should end with this short block:

```md
## Findings

What felt good:
- ...

What felt rough:
- ...

Tina capability pulled:
- ...

Suggested follow-up:
- ...

Verdict:
- keep / fix / defer
```

Rules for findings:

- If pain is local, fix it in the system.
- If pain repeats in two systems, add it to `examples/FINDINGS.md`.
- If pain repeats in three systems, promote it to a real phase.
- If Tina was better, say exactly why.
- If Tokio was better, say exactly why.

## Idea Rule

Do not make one system per idea.

Many ideas pull the same Tina string. Combine them when the same app can
hurt the same way:

- chat + game rooms + presence -> realtime rooms
- object ingest + file pipeline + thumbnailer -> media ingest pipeline
- webhook relay + notification prefs -> webhook relay
- leaderboard + market data + order book -> order book
- token refresh + email retry -> delivery daemon

Keep a separate system only when it finds a different class of pain.

## Planned Systems

| System | Build | Pulls On |
|---|---|---|
| `mini_saas_api` | Native HTTP API with routes, SQLite bridge pool shape, outbound keepalive webhook, graceful shutdown with in-flight work, health/readiness, asserted capacity/pressure report, and live-replay fact. Run with `cargo test --manifest-path examples/systems/mini_saas_api/Cargo.toml`. | `tina-http`, `tina-sqlite-bridge`, keepalive pool, tracing, capacity reports, service shutdown. |
| `ergonomics_playground` | Tiny service probes: first-success/no-winner quote races, debounced batching with drain, and single-flight cache fill. Run with `cargo test --manifest-path examples/systems/ergonomics_playground/Cargo.toml`. | `CallGroup`, `RequestContext`, cancelable calls, `PendingReplies`, `SharedWork`, timers, single-flight fill, visible helper candidates. |
| `system_bounded_object_lane` | Tiny S3-shaped object lane with concurrent callers, bounded in-flight work, typed busy replies, and hermetic fake object writes. Run with `cargo test --manifest-path examples/systems/system_bounded_object_lane/Cargo.toml`. | Request contexts, runtime-owned time, bounded in-flight admission, pressure vocabulary, future AWS bridge shape. |
| `system_realtime_rooms` | Bounded WebSocket room with join/leave, a recurring liveness tick, slow/idle eviction, and graceful shutdown. Run with `cargo test --manifest-path examples/systems/system_realtime_rooms/Cargo.toml`. | Native WebSocket (`WebSocketSessionHandle`, `WebSocketSessionMsg`), recurring `sleep_then`, bounded fan-out, slow-peer pressure, bootstrap-message pattern. |
| `system_copied_service_path` | Canonical copied service skeleton: one real isolate on a real `ThreadedRuntime`, bounded admission via `SharedCapacityScope`, a durable-state ledger step, real concurrent callers through `tina_proof_harness::load`, and a leak check that reads the scope's real post-shutdown state. Run with `cargo test --manifest-path examples/systems/system_copied_service_path/Cargo.toml`. | `SharedCapacityScope`, `GuardedPendingReplies`, `tina_proof_harness::load`, `assert_no_leaked_capacity_at_shutdown`. |
| `system_scoped_request_tree` | One streaming HTTP route owns one `RequestScope`: a tombstoned deadline timer, one cancelable enrich child, and one `ScopedRequestReport`. A mid-body client disconnect cancels the child, the timer fires late and is ignored, and the scope slot is reclaimed. Run with `cargo test --manifest-path examples/systems/system_scoped_request_tree/Cargo.toml`. | `RequestScope`, `RequestScopeSet`, `ScopedRequestReport`, `ScopedTimerSet` tombstone, streaming-body disconnect, `tina_sim::dst` live-replay agreement. |
| `system_job_queue` | N worker children with sync `Submit` parked in one `PendingCancelableCallSet`. Total admission cap is `workers`. Cancel-while-running uses `PendingCancelableCall::cancel(...)` to atomically close the wait and answer the parked caller. Worker panic respawns the slot and replies `Failed`. Run with `cargo test --manifest-path examples/systems/system_job_queue/Cargo.toml`. | `defer_cancelable(...).try_admit(...)`, `PendingCancelableCallSet`, `spawn_observed`, child lifecycle, `register_with_capacity_using`, runtime-owned timers. |
| `system_metrics_shipper` | Accept metrics, batch by time/size, single-flight one downstream sink (HTTP/DB stand-in), handle overload, shutdown flush. Run with `cargo test --manifest-path examples/systems/system_metrics_shipper/Cargo.toml`. | Periodic work, bounded event sink, batcher pattern, single-flight outbound call, graceful drain. |
| `system_session_auth` | Login, cookie/session state, touch session, expire idle sessions via a recurring sweep, logout. Run with `cargo test --manifest-path examples/systems/system_session_auth/Cargo.toml`. | Sharded placement, recurring timers, owned keyed state, HTTP routing, state snapshot/restore. |
| `system_delivery_daemon` | Queue email/webhook deliveries, rate-limit by domain/tenant, retry with backoff, suppress duplicates, drain on shutdown. | Backoff, jitter, idempotency, worker pools, outbound pools, bounded event sink, graceful shutdown. |
| `system_checkout_saga` | Reserve item, charge payment, write DB row, send webhook, compensate on failure. | Saga pattern, DB bridge, outbound HTTP, race/join, cancellation, typed partial failure. |
| `system_webhook_relay` | Receive webhooks, verify, persist, fan out to subscribers, retry failed subscribers. | HTTP server/client, DB bridge, outbound pool, bounded fanout, retry policy, subscriber pressure. |
| `system_live_replay_bugbox` | Live runtime + observer captures a real trace; same isolate logic runs in the sim with a pinned `ReplayCase`; overload bugbox helpers save/replay the case; `delete_shrink` reduces an 8-op history down to its minimum bug-preserving subset. Run with `cargo test --manifest-path examples/systems/system_live_replay_bugbox/Cargo.toml`. | DST, `ReplayCase`, `assert_replay_case`, `observe_replay_case`, `discover_constants`, `capture_overload_run`, `save_overload_bug`, `replay_overload_bug`, `delete_shrink`, `tina_proof_harness::LiveTrace`. |
| `system_redisish_keyspace` | TCP key/value service with hot keys, sharded map, persistence, snapshot/journal. | TCP loops, sharded placement, owner validation, persistence, hot-key pressure, capacity scopes. |
| `system_tenant_rate_limiter` | Multi-tenant API limiter with sliding windows, burst caps, cleanup, and hot tenants. | Sharded keyed state, timers, hot-key pressure, capacity policies, periodic cleanup. |
| `system_cache_with_fill` | Read-through cache where one miss triggers one upstream fill, concurrent callers wait behind a bounded cap, and stale fills are ignored after invalidation. Run with `cargo test --manifest-path examples/systems/system_cache_with_fill/Cargo.toml`. | `SharedWork`, `CallContext`, single-flight, timers, stale-result handling, capacity reclamation. |
| `system_media_ingest_pipeline` | Streaming upload, parse/process file, make thumbnail, store object, write DB row, emit event, cancel on client drop. | Body streaming/chunked, process rail, AWS bridge or file stand-in, DB bridge, response-source cancel, saga cleanup. |
| `system_audit_log` | Append audit events, batch fsync, serve queries, recover from torn writes in tests. | Persistence correctness, append-before-apply, shutdown flush, DST crash/replay shape. |
| `system_rpc_gateway` | HTTP gateway to internal RPC services with deadlines, retries, and partial failure. | `tina-rpc`, HTTP routing, deadline propagation, race/join helpers, bridge conventions. |
| `system_api_gateway_limits` | Two routes share two shard-local weighted `SharedCapacityScope`s through one all-or-nothing `SharedCapacityReservation`. Proves shared caps fill across routes and Owner-Stop releases held charges. Run with `cargo test --manifest-path examples/systems/system_api_gateway_limits/Cargo.toml`. | `SharedCapacityScope`, `SharedCapacityReservation`, `GuardedPendingReplies`, `CapacitySummary::assert_no_full`, `format_assertion_failure`. |
| `system_lock_manager` | Local lock manager with leases, renewals, lease-expiry hand-off, FIFO per-key wait queues, and stale-handle detection. Run with `cargo test --manifest-path examples/systems/system_lock_manager/Cargo.toml`. | `PendingReplies`, `CallContext`, runtime-owned `sleep`, FIFO fairness, stale handle detection, bounded waiters. |
| `system_order_book` | Sharded in-memory order books for hot symbols with matching, snapshots, and streaming readers. | Hot-key pressure, sharded state, deterministic replay, slow streaming readers, capacity scopes. |
| `system_soak_http_db` | Fast in-process soak that emits the discovery lines a real HTTP+DB service would print: `scope name=…`, `events sink=…`, `capacity surface=…`, `service=… full=N …`, and copyable `FAIL surface=…` lines. Run with `cargo test --manifest-path examples/systems/system_soak_http_db/Cargo.toml`. | `SharedCapacityScope`, `BoundedEventSink`, `ServicePressureReport`, `CapacitySummary::assert_no_full`, `format_assertion_failure`. |
| `perf_native` | Native performance rows for Tina designs against bounded Tokio designs: host enqueue, observed admission, host request/reply, service request/reply chain, HTTP/1 close, HTTP/1 keepalive, and fixed body. Run with `cargo test --release --manifest-path examples/systems/perf_native/Cargo.toml --test perf -- --nocapture` or `make perf-compare`. | `PerfReport`, `PerfComparisonReport`, median-of-five release timing, allocation counts, pressure/leak truth, semantic-match labels. |

## Folded Ideas

These are not lost. They are folded into bigger systems:

- `system_websocket_chat`, `system_game_room`,
  `system_chat_persistence`, and `system_presence_service` ->
  `system_realtime_rooms`.
- `system_object_ingest`, `system_file_processing_pipeline`, and
  `system_image_thumbnailer` -> `system_media_ingest_pipeline`.
- `system_live_leaderboard` and `system_market_data_fanout` ->
  `system_order_book`.
- `system_email_delivery` and `system_token_refresh_daemon` ->
  `system_delivery_daemon`.
- `system_notification_preferences` -> `system_webhook_relay`.
- `system_search_indexer` and `system_background_compactor` are
  parked until media ingest or audit log proves a different pain.

## Build Order

Recent work has already built the first wave of small systems:
`mini_saas_api`, `ergonomics_playground`, `system_job_queue`,
`system_metrics_shipper`, `system_realtime_rooms`, `system_live_replay_bugbox`,
`system_api_gateway_limits`, `system_soak_http_db`, `system_cache_with_fill`,
`system_lock_manager`, `system_session_auth`, `system_tenant_rate_limiter`,
`system_webhook_relay`, and the copied service path trio. Their repeated pain is summarized in
[`../FINDINGS.md`](../FINDINGS.md).

Start with small systems that pull hard:

1. `mini_saas_api`
2. `system_cache_with_fill`
3. `system_job_queue`
4. `system_metrics_shipper`

Then pick based on pain:

- If child lifecycle hurts, build `system_realtime_rooms` or
  `system_job_queue` deeper.
- If DST hurts, build `system_live_replay_bugbox`.
- If capacity hurts, build `system_api_gateway_limits` or
  `system_soak_http_db`.
- If protocols hurt, build `system_media_ingest_pipeline` or
  `system_rpc_gateway`.
- If sharding hurts, build `system_tenant_rate_limiter` or
  `system_order_book`.
- If a missing core feature blocks a system, stop there. Record the
  exact missing feature and build the implementation phase outside the
  system.

## What Not To Do

- Do not make a shared mega harness first.
- Do not make the systems perfect.
- Do not add framework sugar just because one system is ugly.
- Do not hide bad Tina code in helper modules before writing the
  finding down.
- Do not claim production readiness from a passing smoke test.
- Do not merge a completed system with only prose and no runnable
  proof.

System specimens exist to make Tina complain loudly while the code is
still cheap to change.

## Proof Harness

`tina-proof-harness` is the small reusable kit specimens reach for
when they want a typed proof instead of a hand-rolled driver. Three
pieces, each tiny on purpose:

- `tina_proof_harness::load` — concurrent op driver with typed
  latency, err-kind tally, leak-check hook, and a `PressureSummary`
  (rate per mille, max consecutive errors, first error op index).
  Used by `mini_saas_api/tests/soak.rs`.
- `tina_proof_harness::bad_peer` — reusable bad TCP/HTTP clients
  (`HalfClose`, `ResetImmediately`, `Slowloris`, `StalledReader`,
  `StalledWriter`, `MalformedFrame`, `TlsHandshakeFailure`,
  `ReconnectStorm`). Each returns a typed `BadPeerOutcome` with
  `connects_ok`/`bytes_sent`/`bytes_read`/`server_closed`/`peer_reset`.
  Used by `system_realtime_rooms/tests/bad_peer.rs`.
- `tina_proof_harness::live_replay::LiveTrace` — thin
  `TraceObserver` that captures live events and exposes a live
  `tina_sim::dst::TraceShape` fingerprint plus a
  `tina_runtime::PressureSummary` for visible pressure facts. For
  live-to-sim replay, materialize the live facts into
  `tina_sim::dst::LiveReplayCapture` before comparing. Used by
  `system_live_replay_bugbox`.
- `tina_proof_harness::protocol_chaos` — one typed `ProtocolChaosReport`
  for every bad-peer story (TCP, WebSocket, HTTP/2, gRPC): family, byte
  tallies, peer/terminal action, app delivery count, close/reset/status,
  the typed `ProtocolFact` sequence, and unsupported facts. The fact
  fingerprint hashes typed `ProtocolFact` values, never debug strings.
- `tina_proof_harness::websocket` — a pure WebSocket session engine and a
  hermetic compliance corpus (valid/fragmented text, invalid UTF-8 across
  fragments, reserved bits, oversized control/message, masking direction,
  ping/pong and close edges). Each case names the exact app messages that
  reach app code, so "valid data reaches app once" and "malformed bytes
  never do" are both provable.
- `tina_proof_harness::byte_replay` — `ProtocolByteReplayCase`: save a
  bad-frame case as ordered byte chunks, reproduce it, and shrink it.
  Unsupported facts or an over-budget case fail closed; they never pass
  as an exact replay.
- `tina_proof_harness::http2` / `::grpc` — hermetic bad-peer probes that
  map malformed HTTP/2 frames and bad gRPC framing to typed reset /
  GOAWAY / flow-control / status facts, not a bare "connection closed".
- `LiveReplayFact::Protocol(ProtocolFact)` lets a live capture save
  protocol facts beside capacity facts. A mixed capture fails replay if
  either family diverges; `classify_protocol_facts` tells a real
  divergence apart from a live-only simulator-coverage gap.

Copy-pasteable proof targets live in the top-level `Makefile`:

```sh
make proof-fast               # PR gate (includes the bounded protocol corpus)
make proof-soak               # nightly load + protocol corpus at higher count
make proof-bad-peer           # local bad-peer + typed ProtocolChaosReport lines
make proof-replay-regression  # saved-seed sim regression
make perf                     # local release-mode performance evidence
make perf-compare             # native Tina vs bounded Tokio rows
```

If a specimen would otherwise hand-roll a slow-reader / RST /
malformed-HTTP / bad-frame driver, reach for `tina-proof-harness`
instead. The typed outcome is part of what makes "the bug reproduces"
cheap.

### Proof harness vs local test

Use a **local `#[test]`** when the thing under test is the specimen's own
glue: a route table, a handler's reply shape, a config default. The proof
lives next to the code and never needs to be reused.

Reach for the **proof harness** when the thing under test is a protocol or
load behaviour Tina must answer the same way everywhere:

- bad-peer transport twists (half-close, RST, slowloris, reconnect storm)
  → `bad_peer` + `ProtocolChaosReport`;
- WebSocket/HTTP2/gRPC framing abuse → the compliance corpus and probes;
- "this exact byte sequence breaks the parser" → save a
  `ProtocolByteReplayCase`, then shrink it;
- "this live overload reproduces in the sim" → `LiveReplayCapture` with
  protocol and capacity facts.

The harness output is a typed value, so a regression test asserts on
fields and a fact fingerprint rather than scraping logs.
