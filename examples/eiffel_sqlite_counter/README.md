# eiffel_sqlite_counter

Tokio-vs-Tina counter persisted in SQLite. Each side initialises a
fresh `tempfile`-managed database, increments a single-row counter
50 times, reads it back, and ends with `final_value = 50`.

This specimen is **not** a proof that Tina's database story is
production-ready. It documents the shape of the gap and motivates a
real `tina-sqlx-bridge` (ROADMAP phase 055).

## Run

```sh
cargo run --manifest-path examples/eiffel_sqlite_counter/Cargo.toml -- both
cargo test --manifest-path examples/eiffel_sqlite_counter/Cargo.toml
```

```
side=tokio final_value=50 exit_clean=true
side=tina  final_value=50 exit_clean=true
```

## Read

- [`src/tokio_impl.rs`](src/tokio_impl.rs)
- [`src/tina_impl.rs`](src/tina_impl.rs)

## Tokio shape

`rusqlite` is sync. The textbook pattern is
`tokio::task::spawn_blocking` per query:

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

Each blocking call moves an `Arc<Mutex<Connection>>` clone into the
blocking pool. The async runtime is never blocked. This is the
generally-accepted pattern for any sync C library under Tokio.

## Tina shape

There is no `tina-sqlx-bridge` (yet — see ROADMAP phase 055). The
honest first-form shape is one `SqliteWorker` isolate that owns the
connection and runs each query inline in `handle`:

```rust
fn handle(&mut self, msg: SqliteMsg, ...) -> Effect<Self> {
    match msg {
        SqliteMsg::Increment => {
            self.conn.execute("UPDATE counter SET value = value + 1 WHERE id = 0", [])
                .expect("update");
            self.local_count += 1;
            noop()
        }
        SqliteMsg::Finalize => {
            let value: i64 = self.conn.query_row(...)?;
            self.report.final_value = value as u64;
            stop_with(self.report)
        }
    }
}
```

The host fires N `Increment` messages then `Finalize` and reads the
final `Report` via `observe_result::<Report>`.

## Discussion

What feels better:

- **Owned state through one isolate.** The connection lives in one
  place. There is no `Arc<Mutex<Connection>>`, no inter-thread
  contention, no question of "who else holds this lock." A single
  isolate mailbox serializes every database touch.
- **Final value through `stop_with`.** Same Phase 059 Rock 1 path
  the other specimens use. No mpsc, no atomic counter for the
  final read.

What feels worse — and where the missing bridge bites:

- **The shard thread blocks for the duration of every query.**
  Tina handlers are synchronous. There is no `await`, no
  `runtime.run_blocking(closure)` adapter. While
  `conn.execute(...)` runs, *every other isolate on this shard*
  is paused. SQLite operations are fast (microseconds for a
  single-row `UPDATE`), so for a single-shard adoption-grade
  specimen this is acceptable. For a multi-shard production server
  it is not. Postgres queries against a remote server can take
  *milliseconds* — blocking a shard thread for that long is a
  Tina-contract violation in spirit.
- **No bounded "in-flight queries" notion.** With a real DB bridge,
  `max_in_flight` would let a worker accept submissions but bound
  how many queries can be running concurrently against the server.
  Today, a hand-written worker is single-in-flight by virtue of
  being a single isolate; there is no way to fan queries out
  without losing the bounded-pressure contract that
  `tina-reqwest-bridge` ships.
- **Errors collapse to `expect("update")`.** A real bridge would
  surface a typed `SqliteError::*` (Closed, Busy, IoError,
  Constraint, Decode, etc.) at the call site. This specimen panics
  on any database error because there is no good place to put a
  reply translator: the handler returns `Effect<Self>`, not
  `Result<Effect<Self>, _>`.

## What this suggests

A `tina-sqlx-bridge` (or `tina-rusqlite-bridge` for the sync-only
path) shaped like `tina-reqwest-bridge` would shrink this specimen
to roughly:

```rust
let bridge = SqliteWorker::install(&runtime, SqliteConfig {
    path, max_in_flight: 4, query_timeout: Duration::from_secs(5),
})?;

call(bridge.address, SqliteMsg::Update("UPDATE counter SET ..."), timeout)
    .reply(MyMsg::Updated)
```

with the bridge handling: a Tokio-owned blocking-pool runtime for
the actual rusqlite calls; bounded ingress; typed `SqliteError`;
visible `Full` / `Closed` / `Timeout`; metrics shape comparable to
the reqwest bridge.

(See FINDINGS finding 13 — `tina-sqlx-bridge` — for the proposed
shape and ROADMAP phase 063 for the planned phase.)

## What this is not

- Not a benchmark. `INCREMENTS = 50` is small enough to run in
  milliseconds; the lesson is shape, not throughput.
- Not a transaction story. The single-row update is implicit-
  autocommit. WAL, BEGIN/COMMIT, and concurrent readers are out of
  scope.
- Not a multi-shard or pooled story. Both sides use one connection.
