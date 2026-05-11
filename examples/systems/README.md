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
- README is where feelings go.

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

## Planned Systems

| System | Build | Pulls On |
|---|---|---|
| `system_mini_saas_api` | HTTPS API with routes, Postgres pool, outbound webhook, graceful shutdown, health/readiness. | `tina-http`, `tina-sqlx-bridge`, keepalive pool, cancellation docs, tracing, capacity reports, service shutdown. |
| `system_websocket_chat` | Rooms, users, slow clients, ping/pong, disconnect cleanup, bounded fanout. | WebSocket shape, child/session lifecycle, slow-consumer pressure, bounded event sink, fanout reports. |
| `system_job_queue` | Submit jobs, bounded workers, cancel jobs, retry, progress polling, worker panic/restart. | Supervision, child lifecycle, join-many, `PendingCallSet`, cancellation, worker pools, topology report. |
| `system_session_auth` | Login, cookie/session state, touch session, expire idle sessions, logout. | Sharded placement, recurring timers, owned keyed state, HTTP routing, state snapshot/restore. |
| `system_metrics_shipper` | Accept metrics, batch by time/size, flush to HTTP/DB, handle overload, shutdown flush. | Periodic work, bounded event sink, batcher pattern, outbound keepalive, DB bridge, graceful drain. |
| `system_checkout_saga` | Reserve item, charge payment, write DB row, send webhook, compensate on failure. | Saga pattern, DB bridge, outbound HTTP, race/join, cancellation, typed partial failure. |
| `system_live_replay_bugbox` | Run live-ish service, capture trace/input facts, replay or approximate in sim, shrink bad case. | DST, `ReplayCase`, trace observer, config manifest, topology/resource capture. |
| `system_redisish_keyspace` | TCP key/value service with hot keys, sharded map, persistence, snapshot/journal. | TCP loops, sharded placement, owner validation, persistence, hot-key pressure, capacity scopes. |
| `system_object_ingest` | Streaming upload, store object, write DB row, emit event/webhook, cancel on client drop. | HTTP body streaming/chunked, AWS bridge or local file stand-in, DB bridge, response-source cancel, saga cleanup. |
| `system_soak_http_db` | One-hour-ish load script over HTTP + DB + outbound calls, report high-water/full/leaks. | Load/soak harness, capacity summaries, tracing, health/readiness, shutdown, CI-friendly reports. |

## Build Order

Start cheap:

1. `system_mini_saas_api`
2. `system_job_queue`
3. `system_metrics_shipper`

Then pick based on pain:

- If child lifecycle hurts, build `system_websocket_chat` or
  `system_job_queue` deeper.
- If DST hurts, build `system_live_replay_bugbox`.
- If capacity hurts, build `system_soak_http_db`.
- If protocols hurt, build `system_object_ingest` or WebSocket.

## What Not To Do

- Do not make a shared mega harness first.
- Do not make the systems perfect.
- Do not add framework sugar just because one system is ugly.
- Do not hide bad Tina code in helper modules before writing the
  finding down.
- Do not claim production readiness from a passing smoke test.

System specimens exist to make Tina complain loudly while the code is
still cheap to change.
