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
| `ergonomics_playground` | Tiny service probes: first-success/no-winner quote races, debounced batching with drain, and single-flight cache fill. Run with `cargo test --manifest-path examples/systems/ergonomics_playground/Cargo.toml`. | `CallGroup`, `RequestContext`, cancelable calls, `PendingReplies`, timers, single-flight fill, visible helper candidates. |
| `system_bounded_object_lane` | Tiny S3-shaped object lane with concurrent callers, bounded in-flight work, typed busy replies, and hermetic fake object writes. Run with `cargo test --manifest-path examples/systems/system_bounded_object_lane/Cargo.toml`. | Request contexts, runtime-owned time, bounded in-flight admission, pressure vocabulary, future AWS bridge shape. |
| `system_realtime_rooms` | Bounded WebSocket room with join/leave, a recurring liveness tick, slow/idle eviction, and graceful shutdown. Run with `cargo test --manifest-path examples/systems/system_realtime_rooms/Cargo.toml`. | Native WebSocket (`WebSocketSessionHandle`, `WebSocketSessionMsg`), recurring `sleep_then`, bounded fan-out, slow-peer pressure, bootstrap-message pattern. |
| `system_job_queue` | N worker children with sync `Submit` parked in one `PendingCancelableCallSet`. Total admission cap is `workers`. Cancel-while-running uses `PendingCancelableCall::cancel(...)` to atomically close the wait and answer the parked caller. Worker panic respawns the slot and replies `Failed`. Run with `cargo test --manifest-path examples/systems/system_job_queue/Cargo.toml`. | `defer_cancelable(...).try_admit(...)`, `PendingCancelableCallSet`, `spawn_observed`, child lifecycle, `register_with_capacity_using`, runtime-owned timers. |
| `system_metrics_shipper` | Accept metrics, batch by time/size, single-flight one downstream sink (HTTP/DB stand-in), handle overload, shutdown flush. Run with `cargo test --manifest-path examples/systems/system_metrics_shipper/Cargo.toml`. | Periodic work, bounded event sink, batcher pattern, single-flight outbound call, graceful drain. |
| `system_session_auth` | Login, cookie/session state, touch session, expire idle sessions via a recurring sweep, logout. Run with `cargo test --manifest-path examples/systems/system_session_auth/Cargo.toml`. | Sharded placement, recurring timers, owned keyed state, HTTP routing, state snapshot/restore. |
| `system_delivery_daemon` | Queue email/webhook deliveries, rate-limit by domain/tenant, retry with backoff, suppress duplicates, drain on shutdown. | Backoff, jitter, idempotency, worker pools, outbound pools, bounded event sink, graceful shutdown. |
| `system_checkout_saga` | Reserve item, charge payment, write DB row, send webhook, compensate on failure. | Saga pattern, DB bridge, outbound HTTP, race/join, cancellation, typed partial failure. |
| `system_webhook_relay` | Receive webhooks, verify, persist, fan out to subscribers, retry failed subscribers. | HTTP server/client, DB bridge, outbound pool, bounded fanout, retry policy, subscriber pressure. |
| `system_live_replay_bugbox` | Run live-ish service, capture trace/input facts, replay or approximate in sim, shrink bad case. | DST, `ReplayCase`, trace observer, config manifest, topology/resource capture. |
| `system_redisish_keyspace` | TCP key/value service with hot keys, sharded map, persistence, snapshot/journal. | TCP loops, sharded placement, owner validation, persistence, hot-key pressure, capacity scopes. |
| `system_tenant_rate_limiter` | Multi-tenant API limiter with sliding windows, burst caps, cleanup, and hot tenants. | Sharded keyed state, timers, hot-key pressure, capacity policies, periodic cleanup. |
| `system_cache_with_fill` | Read-through cache where one miss triggers one upstream fill, concurrent callers wait behind a bounded cap, and stale fills are ignored after invalidation. Run with `cargo test --manifest-path examples/systems/system_cache_with_fill/Cargo.toml`. | `PendingReplies`, `CallContext`, single-flight, timers, stale-result handling, capacity reclamation. |
| `system_media_ingest_pipeline` | Streaming upload, parse/process file, make thumbnail, store object, write DB row, emit event, cancel on client drop. | Body streaming/chunked, process rail, AWS bridge or file stand-in, DB bridge, response-source cancel, saga cleanup. |
| `system_audit_log` | Append audit events, batch fsync, serve queries, recover from torn writes in tests. | Persistence correctness, append-before-apply, shutdown flush, DST crash/replay shape. |
| `system_rpc_gateway` | HTTP gateway to internal RPC services with deadlines, retries, and partial failure. | `tina-rpc`, HTTP routing, deadline propagation, race/join helpers, bridge conventions. |
| `system_api_gateway_limits` | Proxy-ish gateway with per-route and per-tenant caps, upstream pools, overload policy. | Capacity scopes, outbound pools, backpressure policy, health/readiness, pressure reports. |
| `system_lock_manager` | Local lock manager with leases, renewals, lease-expiry hand-off, FIFO per-key wait queues, and stale-handle detection. Run with `cargo test --manifest-path examples/systems/system_lock_manager/Cargo.toml`. | `PendingReplies`, `CallContext`, runtime-owned `sleep`, FIFO fairness, stale handle detection, bounded waiters. |
| `system_order_book` | Sharded in-memory order books for hot symbols with matching, snapshots, and streaming readers. | Hot-key pressure, sharded state, deterministic replay, slow streaming readers, capacity scopes. |
| `system_soak_http_db` | One-hour-ish load script over HTTP + DB + outbound calls, report high-water/full/leaks. | Load/soak harness, capacity summaries, tracing, health/readiness, shutdown, CI-friendly reports. |

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
