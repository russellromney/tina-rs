# Resource Owner Matrix

Companion to [mailbox-capacity.md](mailbox-capacity.md) and
[bridge-composition.md](bridge-composition.md).

This page is checked-in evidence, not an audit task. It names, for each
long-lived resource kind, **who owns the close path** and how close /
drain / force / report behave. The one rule that drives the whole table:

```text
a generic pool can mark, reject, and report.
it cannot close a resource it did not build.
the resource owner closes. the pool reports.
```

So the table separates two columns of truth: what the generic
`WorkerPool` does (mark stale, report, refill) versus what an
owner-specific resource does (actually close the socket / connection /
file).

## The matrix

| resource kind | owner | close path | drain path | force path | report |
|---|---|---|---|---|---|
| HTTP/1 keepalive connection | `KeepaliveConnection` isolate (`tina-http::keepalive`) | `Stop` closes the transport, isolate exits; `Maintain { now, max_idle }` closes an *idle* socket but keeps the slot | `shutdown_keepalive_pool(Drain)` waits for leases to return, then `Stop`s each connection | `shutdown_keepalive_pool(Force)` `Stop`s connections immediately | `KeepalivePoolShutdownReport`; `KeepaliveOutcome::Maintained { closed_idle }` |
| HTTP/2 / gRPC client connection | `Http2ClientConnection` isolate (`tina-http::http2::client`) | isolate owns GOAWAY / connection close | session manager drains active streams before closing the connection | session manager can force stream reset / connection close | connection-level facts |
| HTTP/2 stream slot | the connection isolate, under `max_concurrent_streams` + flow control | stream close / RST_STREAM | wait for app stream completion | RST_STREAM the active stream | stream-level facts |
| SQLite bridge | `SqliteWorker` isolate + one blocking thread holding one `rusqlite::Connection` (`tina-sqlite-bridge`) | dropping the isolate / `SqliteCloser` closes the one connection | serial: at most one in-flight; bridge waits for it | bridge per-attempt timeout surfaces `Timeout`; late result tracked | `SqlitePressureReport` → `BridgePressure` (`"sqlite.bridge"`) |
| SQLx bridge | SQLx owns the `PgPool`; `PgWorker` (`tina-sqlx-bridge`) bounds outer admission above it | dropping `PoolHolder` closes SQLx connections; SQLx owns inner close | outer admission drains; SQLx owns its own pool drain | bridge timeout surfaces `Timeout`; SQLx work may continue (`late_results`) | `PgPressureReport` → `BridgePressure` (`"pg.bridge"`) |
| `WorkerPool` generic handle | the owner that built the handles — **not** the pool | none in the pool: `Maintain` marks idle slots `Retired` + reports `RetireReason`; owner closes then `Refill`s | `Close(Drain)` stops admission, lets leases return | `Close(Force)` marks outstanding leases stale + retires | `PoolPressureReport`, `ResourcePolicyReport`, `PoolShutdownReport` |
| local file / journal rail | the app isolate owning the path; runtime rails (`tina-runtime::file_loops`, `persistence`) | owner closes the file / stream explicitly | flush pending writes | discard pending | persistence recovery facts (`JournalReplay`); durable shutdown report |
| Unix-domain socket rail | the runtime driver (`tina-runtime::driver::unix`), riding the per-shard Betelgeuse completion loop like TCP/TLS | `unix_close_listener` / `unix_close_stream` close the socket; the substrate unlinks a listener's socket file; close wins over pending accept/read/write (their continuations do not fire) | shutdown drains the shared loop; pending work tombstones | `cancel(call_id)` stops the runtime waiting; an already-submitted syscall is not unwound | `RuntimeCapabilities::unix` (completion-backed); close-cancelled ids surface via the runtime's `ResourceClosed` |

## Why connections and stream slots are not the same shape

The plan is explicit that "pool" must not mean one mechanism for all
three of HTTP/1, HTTP/2 streams, and DB internals. One vocabulary, not
one fake mechanism:

- **HTTP/1 keepalive leases a connection for one request.** One slot =
  one transport. The pool is a real `WorkerPool` over connection-isolate
  addresses.
- **HTTP/2 / gRPC admits streams on a connection.** A stream slot is not
  a `WorkerPool` lease — it is bounded by `max_concurrent_streams` and
  flow-control windows inside one connection isolate. Retiring a
  connection must not silently strand active streams; that lifecycle belongs
  to the protocol connection, not the generic pool.
- **DB bridges expose outer Tina pressure while DB internals stay
  database-specific.** SQLx owns its `PgPool`; the bridge surfaces
  bridge pressure and admission, and never queries or fakes SQLx
  internals (`pool.size()`, `num_idle()`). SQLite is sync C with no pool
  and pins `max_in_flight = 1`. Both project onto the shared
  `BridgePressure` vocabulary but keep their own truth.

## Generic pool resource rules (`WorkerPool`)

The generic pool (`tina-runtime::pool`) gained an optional
[`ResourceLifetime`](../tina/src/pool.rs) policy. The rules:

- **Idle retirement applies only to idle resources.** `Maintain { now }`
  retires idle resources past `max_idle`; an idle resource's clock is
  stamped by the first sweep that sees it idle, so run maintenance at
  least as often as `max_idle`.
- **Max lifetime never hands a stale resource to a new caller, and never
  steals a leased one.** A `Maintain` sweep retires idle resources past
  `max_lifetime`; a leased resource past `max_lifetime` is *reported*
  old in `ResourcePolicyReport::over_age_leased`, never reclaimed behind
  the caller's back.
- **Time is the owner's, not the pool's.** `Maintain { now }` carries
  `now` from the owner's Tina clock (`ctx.now()` off a timer). The pool
  never reads `Instant::now()` itself.
- **The pool reports; the owner closes and refills.** Retirement marks a
  slot `Retired`, drops the handle clone, and names the slot +
  `RetireReason` in the report. The owner closes the real resource and
  hands back a fresh handle with `Refill { resource_id, handle, now }`.
  Refill refuses a live (idle or leased) slot, so a resource is never
  replaced behind a caller's back.
- **Health is the caller's verdict.** A generic pool cannot probe an
  arbitrary `H`. `ResourceHealth { Healthy, Suspect, Retire }` is the
  vocabulary the caller uses at a check point; `Retire.disposition()`
  maps to a release that drops the resource (`RetireReason::Unhealthy`).
- **Close means no new admission. Drain waits for owned in-flight work.
  Force marks outstanding work stale.** `PoolShutdownReport` folds the
  close mode and a post-close pressure snapshot into the lifecycle words:
  drain / force / closed / leased count.

## Explicit Non-Goals

- No generic magic close for `WorkerPool` handles it does not own.
- No stealing a leased resource because a timer fired.
- No HTTP/2 / gRPC client protocol-state redesign.
- No faking SQLx pool internals behind one pool abstraction.
- No durable-state restore helper — `RecoveryReport`, append-before-apply
  type-state, and durable specimens own that vocabulary, avoiding a colliding
  second design of the same names.
