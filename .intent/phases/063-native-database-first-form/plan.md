# 063 Native Database First Form

## Status

- Done: `tina-sqlite-bridge` first form landed. One connection, one
  blocking std thread, autocommit only. `external_pool_size`,
  `max_in_flight`, and `pending_reply_capacity` pinned to `1` at
  config validation. Typed `SqliteError` covers admission (`Full`,
  `Closed`, `InvalidRequest`), per-attempt timeout, response cap,
  `Busy`, `Constraint`, `Io`, generic `Sqlite`, and `Internal`. Late
  worker results after a bridge timeout are observed via an
  abandoned-flag the worker thread checks before sending; they bump
  `late_results` and the per-outcome counter once each (no double
  tally). Specimen `specimen_sqlite_counter` rewired to use the bridge;
  both Tokio and Tina sides land on `final_value = 50`.
- Done: typed-helper ergonomics polish — `execute_call`/`query_call`,
  `Row`/`SqliteRows` accessors, `From` impls for common Rust types
  (no silent `From<u64>`; use `TryFrom<u64>` instead), generic
  classifier `SqliteOutcomeClass<T>` over the success carrier.
  Demo modes (constraint, timeout, closed, invalid, retry) live on
  the example.
- Closed: SQLite first form and its ergonomics polish landed. Later SQLx and
  DB pool work moved into their own completed follow-up phases.
- Deferred: native Postgres wire, migrations, ORM, schema tools, and typed
  user-struct row mapping.
- Parallel: this phase may start while 064 runs. Do not wait for the
  064 bridge audit. Follow the current `tina-reqwest-bridge` style
  for install/config/metrics/shutdown, keep the surface small, and
  expect a later 064 polish pass if the bridge convention tightens
  names.

## Goal

Tina services need a database path that does not block shard threads and does not
drag unbounded Tokio-shaped pressure into the app.

Start with `tina-sqlite-bridge`, backed by `rusqlite`. SQLite is small, sync,
real, and already painful in Specimen. Use it to settle the bridge rule:

```text
Tina isolate state is serial.
external bridge work may be parallel.
all parallelism must have names and caps.
```

This is a concrete bridge, not the shared bridge framework. Do not invent a
`tina-bridge-common` crate here. 064 owns the bridge convention audit. 063
should build one honest database bridge whose shape is easy to compare against
the existing bridge crates.

## Non-Goals

- No ORM.
- No migrations.
- No schema discovery.
- No transaction framework.
- No hidden retry.
- No "query futures" in Tina handlers.
- No native Postgres in this slice.
- No pretending one connection/isolate means parallel queries.
- No SQLite transaction handle. First form is autocommit statements only.
- No connection pool in this slice.
- No typed row mapping. First form returns bridge-owned SQLite values.

## Bridge Doctrine

Every bridge must name these separately:

- `mailbox_capacity` — requests waiting to enter the bridge isolate.
- `pending_reply_capacity` — callers accepted and waiting for a later reply.
- `max_in_flight` — operations accepted into external work.
- `external_pool_size` — foreign runtime/blocking workers/connections.
- `default_timeout` — bridge-side attempt timeout.

If any cap fills, caller sees typed `Full`, not buffering fog.

If caller times out first, accepted work may finish late; late reply is dropped
visibly through Tina trace/metrics.

Use the current bridge vocabulary unless a local reason says otherwise:

- `install` wires runtime registration and returns the address/closer/metrics
  handles;
- config validation returns typed errors, never silent clamping;
- metrics count worker-terminal outcomes, not caller-observed outcomes after a
  Tina call timeout;
- shutdown stops admission, drains or visibly closes accepted work, and returns
  terminal truth;
- supplied external resources, if supported, must say which policy knobs are
  caller-owned.

If 064 later standardizes any of these names, adapt in a small follow-up. Do
not block first-form SQLite on that audit.

## Rock 1: `tina-sqlite-bridge` Crate

Add a first-form SQLite bridge crate. This phase is **not** `tina-sqlx-bridge`.
SQLx is the next ecosystem bridge after this one proves the DB bridge shape.

Shape:

```rust
let db = SqliteWorker::<SingleShard>::install(&runtime, SqliteConfig::path(path))?;

call(db.address, SqliteMsg::Execute(sql), timeout)
    .reply(AppMsg::DbDone)
```

Types:

- `SqliteWorker`
- `SqliteConfig`
- `SqliteMsg`
- `SqliteRequest`
- `SqliteResponse`
- `SqliteValue`
- `SqliteError`
- `SqliteMetrics`
- `SqliteMetricsHandle`
- `SqliteCloser`

First-form request/response shape:

```rust
enum SqliteRequest {
    Execute {
        sql: String,
        params: Vec<SqliteValue>,
    },
    QueryRows {
        sql: String,
        params: Vec<SqliteValue>,
        max_rows: usize,
    },
}

enum SqliteResponse {
    Executed { rows_changed: u64 },
    Rows { columns: Vec<String>, rows: Vec<Vec<SqliteValue>> },
}
```

`SqliteValue` first form:

- `Null`
- `Integer(i64)`
- `Real(f64)`
- `Text(String)`
- `Blob(Vec<u8>)`

Defer typed decoding, streaming rows, prepared statement handles, and row
mappers.

Rules:

- `rusqlite` runs outside the shard thread.
- no unbounded queue;
- no silent config clamp;
- supplied connection/client policy must be honest if supported;
- `Close` either replies or is send-only and documented.

Proof:

- happy execute/query path;
- request cap;
- row cap / response cap on buffered `QueryRows`;
- `Full`;
- caller timeout and late result;
- worker close;
- sequential calls;
- after-failure recovery.
- `install()` smoke path, so examples do not copy manual registration ceremony.

## Rock 2: Blocking Worker / Connection Model

First form is one connection, one blocking worker.

The config still names the bridge caps, but pool fields are pinned to one:

```rust
SqliteConfig {
    external_pool_size,      // must be 1 in this phase
    max_in_flight,           // must be 1 in this phase
    pending_reply_capacity,
    mailbox_capacity,
    default_timeout,
    busy_timeout,
    pragmas,
}
```

Rules:

- Reject `external_pool_size != 1`.
- Reject `max_in_flight != 1`.
- Never share `rusqlite::Connection` through `Arc<Mutex<_>>` as the public bridge model.
- Open one connection on the blocking worker.
- Autocommit only.
- Optional startup pragmas are explicit config.
- Busy timeout is explicit config. No silent SQLite default hand-wave.

Proof:

- `max_in_flight = 1` serializes;
- `max_in_flight = 0` rejects config;
- `max_in_flight > 1` rejects config;
- `external_pool_size > 1` rejects config;
- bad pragma reports a typed startup/config error.

## Rock 3: Error Surface

Typed first form:

- `Full`
- `Closed`
- `Timeout`
- `InvalidRequest`
- `Busy`
- `Constraint`
- `Io`
- `Sqlite`
- `Internal`

Rules:

- HTTP-style flattening is not default.
- Do not panic on SQL error.
- Preserve enough detail to log/debug.
- Tie every variant to an operation. Do not add variants that no test can hit.

Proof:

- constraint violation;
- bad SQL;
- closed worker;
- timeout;
- oversized response/request if caps exist.
- busy/locked if feasible without flaky timing; otherwise document why deferred.

## Rock 4: Metrics And Shutdown

Mirror the useful bridge shapes from reqwest.

Metrics should count worker-terminal outcomes, not pretend to know caller
observed outcomes after Tina call timeout.

Cancellation rule:

- if a Tina caller times out after the query is accepted, the `rusqlite`
  operation is not cancelled;
- it runs to completion on the blocking worker;
- metrics record the worker-terminal outcome;
- the late deferred reply is rejected/dropped visibly through Tina trace.

Shutdown:

- stop admitting;
- settle or visibly close pending replies;
- close DB connections;
- return report.

Proof:

- close with no in-flight;
- close with in-flight;
- close rejects new work;
- metrics match worker terminal facts.
- caller timeout before query completion produces late-result truth, not fake
  cancellation.

## Rock 5: Specimen Rewrite

Rewrite `examples/specimen_sqlite_counter` to use the bridge.

README must compare:

- Tokio `spawn_blocking` + `Arc<Mutex<Connection>>`;
- Tina bridge install + bounded call surface;
- serial one-connection mode vs named pool mode;
- why inline `rusqlite` in `handle()` was only a specimen, not production.

Update `examples/FINDINGS.md`.

## Rock 6: Postgres / SQLx Follow-Up Design

Do not implement here.

Write the next plan slice after SQLite teaches the shape:

- `tina-sqlx-bridge` for ecosystem adoption;
- pooled SQLite with N independent connections if Specimen asks for it;
- later native Postgres wire over Tina TCP using `postgres-protocol`;
- how cancellation maps when a query has reached the DB;
- how pools report `Full`, `Closed`, timeout, and late result.

## Order

1. Bridge doctrine doc in plan/docs, explicitly saying this phase can run in
   parallel with 064 and may receive a later naming polish.
2. Crate skeleton and config validation.
3. One-connection SQLite worker.
4. Deferred reply + pending cap.
5. Metrics/shutdown.
6. Specimen rewrite.
7. Postgres/SQLx follow-up design note.

## Required Proof

- `cargo fmt --all --check`.
- `cargo test -p tina-sqlite-bridge`.
- workspace clippy gate for touched crates.
- Specimen sqlite smoke test.
- No hidden unbounded queue.
- No shard-thread DB query in the bridge.

## Done Means

- Tina has a bounded SQLite bridge.
- Bridge parallelism doctrine is documented.
- Specimen sqlite no longer teaches inline DB work as the Tina shape.
- Postgres/SQLx next slice is clear.
