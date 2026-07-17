# specimen_sqlite_counter

Tokio-vs-Tina counter persisted in SQLite. Each side initialises a
fresh `tempfile`-managed database, increments a single-row counter
50 times, reads it back, and ends with matching reports:

```
final_value=50 updates_ok=50 queries_ok=1 rows_changed=50 exit_clean=true
```

The Tina side drives a real
[`tina-sqlite-bridge`](../../tina-sqlite-bridge) worker from a root
isolate. That isolate privately accumulates query/update metrics and
publishes them once through `stop_with`. The host claims
`observe_result` before start. The shard thread is never blocked while
SQLite runs; admission, in-flight, and timeouts are named caps with
typed failure modes. Point-in-time inspection uses the bridge's
existing typed query request — there is no result mutex or poll loop.

## Run

```sh
cargo run --manifest-path examples/specimen_sqlite_counter/Cargo.toml -- both
cargo test --manifest-path examples/specimen_sqlite_counter/Cargo.toml
```

Public certification targets:

```sh
cargo test --manifest-path examples/specimen_sqlite_counter/Cargo.toml \
  --test public_smoke public_smoke -- --exact
cargo test --manifest-path examples/specimen_sqlite_counter/Cargo.toml \
  --test public_smoke public_characterization -- --exact
```

```
comparison=specimen_sqlite_counter side=tokio final_value=50 updates_ok=50 queries_ok=1 rows_changed=50 exit_clean=true
comparison=specimen_sqlite_counter side=tina  final_value=50 updates_ok=50 queries_ok=1 rows_changed=50 exit_clean=true
```

The Tina side also prints a one-line bridge metrics summary (bridge
pressure, not the application report):

```
specimen_sqlite_counter (tina) bridge metrics: \
  admitted=51 executed=50 rows=1 timeouts=0 late=0 full=0 closed=0 high_water=1
```

### Failure-shape demos

`demo` runs short scripts that surface each typed error a user will
hit at the call site. Each demo isolate ends with `stop_with`; the host
reads the outcome through `observe_result`:

```sh
cargo run --manifest-path examples/specimen_sqlite_counter/Cargo.toml -- demo
# or one at a time:
#   demo-constraint  — UNIQUE violation (SqliteError::Constraint)
#   demo-timeout     — bridge default_timeout fires; worker finishes;
#                      late_results bumps (SqliteError::Timeout)
#   demo-closed      — bridge closed before send (SqliteError::Closed)
#   demo-invalid     — over-cap params (SqliteError::InvalidRequest)
#   demo-retry       — classify() transient-vs-fatal loop
#   demo-point-in-time — host typed query request reads current value
```

## Read

- [`src/tokio_impl.rs`](src/tokio_impl.rs)
- [`src/tina_impl.rs`](src/tina_impl.rs)

## Tokio shape: `spawn_blocking` + `Arc<Mutex<Connection>>`

`rusqlite` is sync. The textbook Tokio pattern is one
`tokio::task::spawn_blocking` per query, with the connection wrapped
in `Arc<Mutex<_>>` so it can move into successive blocking tasks:

```rust
let conn = Arc::new(Mutex::new(open()?));
for _ in 0..N {
    let conn = Arc::clone(&conn);
    tokio::task::spawn_blocking(move || {
        let conn = conn.lock().unwrap();
        conn.execute("UPDATE counter SET value = value + 1 WHERE id = 0", []).unwrap();
    }).await?;
}
```

The async runtime is never blocked; the blocking pool absorbs the
sync calls. Pressure (how many blocking tasks are queued, how long
each one waits) lives entirely inside Tokio's blocking pool defaults
and is not nameable from the call site.

## Tina shape: root isolate + bridge + terminal report

```rust
use tina::prelude::*;
use tina_runtime::{CallOutcome, LocalSystem};
use tina_sqlite_bridge::{SqliteConfig, SqliteWorker, execute_call, query_call};

// Inside the counter isolate:
execute_call(self.db, "UPDATE counter SET value = value + 1 WHERE id = 0", vec![], timeout)
    .then(CounterMsg::UpdateDone);

// On the final SELECT:
//   self.report.queries_ok += 1;
//   self.report.final_value = value;
//   stop_with(self.report)

// Host:
let waiter = app.observe_result::<Report, _, _>(counter_addr)?;
app.try_send(counter_addr, CounterMsg::Begin)?;
let report = waiter.wait(Duration::from_secs(10))?;
```

Point-in-time inspection of the live database uses the existing typed
query request (host `call_blocking` of `SqliteRequest::query_rows`, or
`query_call` from another isolate). Application metrics for the full
script arrive only through the terminal report.

The service-isolate helpers `execute_call(...)` and `query_call(...)`
are the copied path when you are already inside `handle()` and want to
continue through `.then(...)`.

Under the hood the bridge owns one std-thread blocking worker that
holds the `rusqlite::Connection`. The Tina shard thread submits
requests to a `mpsc::sync_channel` and folds the eventual reply
into a continuation message. Failure modes are visible at the
boundary:

```
mailbox full      -> CallError::TargetFull (Tina ingress)
max_in_flight     -> SqliteError::Full
per-attempt clock -> SqliteError::Timeout
row buffer cap    -> SqliteError::ResponseTooLarge
SQLITE_BUSY/LOCK  -> SqliteError::Busy
constraint viol.  -> SqliteError::Constraint(detail)
worker closed     -> SqliteError::Closed
```

## Serial one-connection mode vs named pool mode

The bridge today ships only the **serial one-connection** mode:

- `external_pool_size = 1`
- `max_in_flight = 1`

Both are pinned to `1` and rejected at config validation otherwise.
The doctrine "external bridge work may be parallel; all parallelism
must have names and caps" is honest once the serial shape is
settled. A pooled form would lift these pins, inherit the same
config knobs and typed errors, document a per-connection isolation
rule (no shared `Arc<Mutex<Connection>>`), and add tests that prove
the pool's `Full` boundary.

To observe the pool shape today, call [`SqliteMetricsHandle::pressure_report`](../../tina-sqlite-bridge):

```rust
let report = bridge.metrics.pressure_report();
// report.capacity   == config.max_in_flight (always 1)
// report.leased     == 0 or 1
// report.available  == 1 or 0
// report.full_count == cumulative SqliteError::Full
// report.busy_count == cumulative SQLITE_BUSY
// report.high_water == peak in-flight observed (always 0 or 1)
```

`waiters` is always `0` because the bridge does not queue callers;
it replies `SqliteError::Full` immediately. SQL errors (including
`SqliteError::Busy`) do **not** retire the lane.

This shape is proven in the bridge's own
`pressure_report_reflects_serial_pool_shape` test.

## What this is not

- Not a benchmark. `INCREMENTS = 50` is small enough to run in
  milliseconds; the lesson is shape, not throughput.
- Not a transaction story. The single-row update is
  implicit-autocommit. `BEGIN`/`COMMIT`, savepoints, and explicit
  transaction handles are out of scope.
- Not typed row mapping. Rows come back as `Vec<Vec<SqliteValue>>`.
- Not multi-shard or pooled. Both sides use one connection.
