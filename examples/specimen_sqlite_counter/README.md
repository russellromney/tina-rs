# specimen_sqlite_counter

Tokio-vs-Tina counter persisted in SQLite. Each side initialises a
fresh `tempfile`-managed database, increments a single-row counter
50 times, reads it back, and ends with `final_value = 50`.

The Tina side now drives a real
[`tina-sqlite-bridge`](../../tina-sqlite-bridge) worker instead of
running rusqlite inline in a handler. The shard thread is no longer
blocked while SQLite runs; admission, in-flight, and timeouts are
named caps with typed failure modes.

## Run

```sh
cargo run --manifest-path examples/specimen_sqlite_counter/Cargo.toml -- both
cargo test --manifest-path examples/specimen_sqlite_counter/Cargo.toml
```

```
side=tokio final_value=50 exit_clean=true
side=tina  final_value=50 exit_clean=true
```

The Tina side also prints a one-line bridge metrics summary:

```
specimen_sqlite_counter (tina) bridge metrics: \
  admitted=51 executed=50 rows=1 timeouts=0 late=0 full=0 closed=0 high_water=1
```

### Failure-shape demos

`demo` runs four short scripts that surface each typed error a user
will hit at the call site:

```sh
cargo run --manifest-path examples/specimen_sqlite_counter/Cargo.toml -- demo
# or one at a time:
#   demo-constraint  — UNIQUE violation (SqliteError::Constraint)
#   demo-timeout     — bridge default_timeout fires; worker finishes;
#                      late_results bumps (SqliteError::Timeout)
#   demo-closed      — bridge closed before send (SqliteError::Closed)
#   demo-invalid     — over-cap params (SqliteError::InvalidRequest)
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

## Tina shape: `tina-sqlite-bridge` install + host `call_blocking`

```rust
use tina_runtime::CallOutcome;
use tina_sqlite_bridge::{SqliteConfig, SqliteMsg, SqliteRequest, SqliteResponse, SqliteWorker};

let cfg = SqliteConfig::path(&path)
    .with_default_timeout(Duration::from_secs(5))
    .with_busy_timeout(Duration::from_secs(2))
    .with_pragma("journal_mode = WAL")
    .with_poll_interval(Duration::from_millis(1));
let bridge = SqliteWorker::<SingleShard>::install(&runtime, cfg)?;

// From the host thread. Service isolates should still use
// execute_call(...).then(...) when they want a continuation message.
let outcome = runtime.call_blocking(
    bridge.address,
    SqliteMsg::Request(SqliteRequest::execute(
        "UPDATE counter SET value = value + 1 WHERE id = 0",
    )),
    Duration::from_secs(5),
)?;
assert!(matches!(
    outcome,
    CallOutcome::Replied(Ok(SqliteResponse::Executed { rows_changed: 1 }))
));
```

The service-isolate helpers `execute_call(...)` and `query_call(...)`
are still the copied path when you are already inside `handle()` and
want to continue through `.then(...)`. The host-side specimen uses
`call_blocking` because it is a script, not a long-lived app isolate.

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

## Why inline `rusqlite` in `handle()` was only a specimen, not production

The earlier version of this specimen ran `self.conn.execute(...)`
directly inside the handler. That works for a 50-row counter, but
hides three problems:

1. **The shard thread blocks while SQLite runs.** Every other
   isolate on the shard is paused for the duration of the query.
   Microsecond `UPDATE` calls hide it; a 50ms `Postgres` query
   against a remote server would not.
2. **No named caps.** There is no `mailbox_capacity`, no
   `max_in_flight`, no `default_timeout`. Pressure is invisible. A
   slow query becomes a slow shard becomes a slow service.
3. **Errors collapse to `expect(...)`.** A real bridge surfaces a
   typed `SqliteError::*`. The inline version had no place to put
   a reply translator because handlers return `Effect<Self>`, not
   `Result<Effect<Self>, _>`.

The bridge fixes all three. The shard thread does no SQLite work;
each cap is named in `SqliteConfig`; every failure has a typed
variant.

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
