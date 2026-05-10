# 075 SQLx Postgres Bridge First Form

## Status

- Done: plan extracted from 051E and updated with 063 SQLite lessons.
- Decisions for first PR:
  - **Test strategy**: hybrid. Pure unit and fake tests for everything
    that does not need a live database — value conversion, row
    accessors, config validation including no-op cap rejection, the
    classifier, admission `Full` / `Closed` / `InvalidRequest`,
    supplied-pool ownership rules, dropped-caller / late-result path
    where a fake oneshot stands in for SQLx. Plus `#[ignore]`-gated
    integration tests that read `DATABASE_URL` and exercise the real
    SQLx/Postgres path: happy `Execute`, happy `FetchOne`, no rows,
    SQL error then recovery, SQLx pool acquire timeout distinct from
    Tina `Full`, per-attempt bridge `Timeout`, and close with
    in-flight. CI does not need real credentials; `cargo check` and
    the default `cargo test -p tina-sqlx-bridge` do not need a
    database.
  - **SQLx compile mode**: runtime-checked `sqlx::query(...)` and
    `sqlx::query_as(...)` only. No `query!` / `query_as!` macros
    and no checked-in offline metadata. The bridge accepts SQL from
    messages, so compile-time SQL checking would be dishonest.
  - **Crate**: `tina-sqlx-bridge`. Postgres-shaped public API:
    `PgWorker`, `PgConfig`, `InstalledPgBridge`, `PgCloser`,
    `PgMsg`, `PgRequest`, `PgResponse`, `PgValue`, `PgRow`,
    `PgError`, `PgMetrics`, `PgMetricsHandle`, `PgOutcomeExt`,
    `PgOutcomeClass`, `PgFatalReason`, `PgTransientReason`.
    Helpers: `send_request`, `execute_call`, `fetch_one_call`.
  - **`FetchMany` deferred**. First form ships `Execute` and
    `FetchOne` only. Bounded row caps without unbounded buffering
    is its own design pass (cap-while-streaming, truncated flag,
    too-many-rows detection); it earns a follow-up slice.
- Deferred:
  - `FetchMany`;
  - transactions;
  - streaming rows;
  - generic `sqlx::Database` support;
  - user struct mapping;
  - migrations / schema tools / ORM;
  - DB-side cancellation guarantee;
  - native Postgres wire over Tina TCP.

## Grug Truth

DB bridge has two queues and one database.

```text
Tina full is not SQL pool busy
SQL pool busy is not query timeout
query timeout is not caller timeout
caller timeout is not DB cancellation
late result is truth, not bug
```

## Goal

Build `tina-sqlx-bridge` as an ecosystem bridge for Postgres.

This is not the native Tina DB story. It is the adoption bridge for users
who already use SQLx/Postgres and want Tina-owned bounded ingress,
typed outcomes, shutdown truth, and visible late results.

First form is **Postgres-first**. Do not make a generic abstraction over
every `sqlx::Database`. That way lies type soup.

## Non-Goals

- No transactions.
- No streaming rows.
- No generic SQLx database support.
- No ORM / schema / migration tool.
- No user struct mapping.
- No hidden retry.
- No hidden row buffer.
- No claim that Tina cancels a query already accepted by SQLx/Postgres.
- No shared pool framework extraction.
- No SQLx compile-time query macros unless offline metadata is deliberately
  checked in. First form should not make ordinary `cargo check` require a
  live database.

## API Shape

Crate:

```text
tina-sqlx-bridge
```

Public first-form names should be Postgres-shaped:

```text
PgWorker
PgConfig
PgInstall
PgCloser
PgMsg
PgRequest
PgResponse
PgValue
PgRow
PgRows
PgError
PgMetrics
PgOutcomeExt
PgOutcomeClass
```

The crate may be named SQLx because the bridge depends on SQLx. The
public operations should not pretend to be database-generic yet.

## Pool Ownership

Support two setup paths if both stay small:

```rust
PgWorker::install(&runtime, PgConfig::from_url(...))
PgWorker::install_with_pool(&runtime, supplied_pg_pool, PgConfig::bridge_only(...))
```

Rules:

- SQLx pool size/acquire timeout stays SQLx config;
- Tina mailbox / max-in-flight stays bridge config;
- both caps are reported;
- supplied pool owns its own SQLx settings;
- bridge config owns Tina admission, bridge timeout, response caps, metrics,
  and shutdown behavior.

Do not hide SQLx's pool behind a Tina pool abstraction in this phase.

## Operations

Required:

```rust
PgRequest::execute(sql).param(...)
PgRequest::fetch_one(sql).param(...)
```

Optional only if still boring:

```rust
PgRequest::fetch_many(sql).max_rows(n).param(...)
```

First response shape:

```text
Execute -> rows_affected: u64
FetchOne -> PgRow or NoRows / TooManyRows
FetchMany -> PgRows { rows, truncated }
```

Rows and values:

```text
PgValue:
  Null
  Bool
  I64
  F64
  String
  Bytes

PgRow:
  column names + values
  get_i64 / get_text / get_bool / get_bytes helpers
```

Steal the 063 ergonomics lesson: helper calls and accessors ship in first
form. Do not make users pattern-match through three layers for every
counter query and then call it polish later.

## Errors

Keep layers honest.

Likely shape:

```text
PgError:
  Full                 // Tina bridge admission
  Closed               // Tina bridge closed
  Timeout              // bridge per-attempt timeout
  PoolAcquireTimeout   // SQLx pool did not provide a connection
  PoolClosed
  Sqlx
  Decode
  ResponseTooLarge
  InvalidRequest
  Internal
```

Caller-observed `CallOutcome::Timeout` is still outside `PgError` on the
raw layered path. Provide helpers, but do not lie about the two layers.

Classifier:

```rust
outcome.classify() -> PgOutcomeClass<T>
```

Classes should at least distinguish:

- succeeded;
- retryable/transient (`PoolAcquireTimeout`, maybe SQLSTATE transient
  classes if easy);
- fatal/invalid;
- Tina admission/closed/timeout separately enough that callers can decide.

## Cancellation And Late Results

Rule:

- before the bridge admits work: no DB work starts;
- after SQLx/Postgres accepts work: Tina timeout/cancel means Tina stopped
  waiting;
- DB work may still complete;
- worker-terminal metrics record what happened;
- runtime late reply/rejection truth remains visible;
- docs say "not cancelled" when not cancelled.

No Postgres `CancelRequest` in first form. That needs its own design.

## Metrics

Metrics are worker-terminal, not caller-observed.

Minimum:

```text
admitted
full
closed
timeouts
pool_acquire_timeouts
sqlx_errors
responses
rows_returned
response_too_large
late_results
in_flight_current
in_flight_high_water
```

Docs must say: caller timeout may happen first; worker metrics may later
record success/error/late result.

## Rock 0: Test Strategy

Before coding, choose test strategy and write it in this plan.

Options:

1. real Postgres in CI;
2. ignored/local integration test with clear command;
3. fake worker/control-plane tests for Tina bridge behavior plus compile
   check for SQLx path.

Do not let infrastructure uncertainty block value conversion, caps,
timeout, closed/full, metrics, and docs tests.

Also decide SQLx compile mode:

- prefer runtime-checked `sqlx::query(...)` / `query_as` shapes for first
  form;
- if using `query!` macros, commit offline metadata and prove `cargo check`
  works without a database.

## Rock 1: Crate Skeleton And Config

Add crate and workspace entry.

Config validates:

- mailbox capacity > 0;
- max in flight > 0;
- response row cap > 0;
- field/row byte caps if present;
- bridge timeout > 0;
- supplied pool vs config-built pool ownership.

Reject no-op public caps. A public cap must change behavior or be pinned
to the only supported value.

## Rock 2: Types And Ergonomics

Add request/response/value/row/error types.

Required ergonomics:

- empty params are cheap;
- builder-style `.param(...)`;
- `execute_call(address, request, timeout)` helper;
- `fetch_one_call(address, request, timeout)` helper;
- row accessors;
- classifier.

No `From<u64>` that silently wraps. If conversion can fail, use
`TryFrom` or an explicit constructor.

## Rock 3: Worker / Install / Close

Implement bounded worker around SQLx/Postgres.

Rules:

- Tina-facing ingress is bounded;
- max in flight is enforced;
- close stops new admission;
- in-flight work runs to completion unless SQLx itself returns;
- close report / metrics stay truthful;
- dropped caller / timeout after admission increments late result path.

## Rock 4: Query Execution

Implement:

- Execute;
- FetchOne;
- FetchMany only if bounded rows remain simple.

Proof:

- happy execute;
- happy fetch one;
- no rows;
- too many rows for `FetchOne`, if detectable without buffering too much;
- response too large;
- SQL error then later recovery.

Do not require SQL strings to be known at compile time. The bridge receives
SQL from messages, so runtime-checked SQLx calls are the honest first form.

## Rock 5: Pressure And Late Truth

Prove the bridge pressure surface:

- Tina admission `Full`;
- bridge `Closed`;
- SQLx pool acquire timeout distinct from Tina `Full`;
- per-attempt bridge timeout;
- caller timeout before worker completion -> late result recorded;
- worker-terminal metrics do not claim caller-observed truth.

## Rock 6: Specimen

Add one specimen if test infrastructure permits.

Preferred:

```text
specimen_postgres_counter
```

Shape:

- Tokio side uses SQLx directly;
- Tina side uses `tina-sqlx-bridge`;
- create table;
- insert/update counter;
- fetch final value;
- optional retry demo for pool acquire timeout only if easy.

If real Postgres is too heavy for normal CI, make the specimen local/ignored
and put the exact command in the README. Do not require real credentials in
CI.

## Rock 7: Docs

Update bridge guide:

- SQLx bridge is ecosystem boundary, not native Tina DB;
- two-runtime / async-pool cost;
- Tina caps vs SQLx pool caps;
- timeout layers;
- cancellation non-claim;
- supplied-pool ownership;
- when to use SQLite bridge instead.

## Proof Targets

- Value conversion unit tests.
- Row accessor unit tests.
- Config validation tests.
- Response cap tests.
- Full / Closed / Timeout tests.
- Pool acquire timeout distinct from Tina admission Full.
- Dropped caller / late result test.
- Close with in-flight test.
- Metrics docs/tests match worker-terminal truth.
- Compile or integration proof for real SQLx/Postgres path.
- Specimen smoke if infrastructure is reasonable.
- fmt/clippy.

## Done Means

- Tina has a bounded Postgres SQLx bridge first form.
- Users can run execute/fetch-one without blocking shard threads.
- Tina admission pressure and SQLx pool pressure are distinct.
- Late DB results after caller timeout are visible.
- Ergonomic helpers/accessors ship with the first form.
- Generic SQLx, transactions, streaming rows, and DB cancellation remain
  explicit future work.

## Follow-Ups

Two buckets:

- **deferred** — earns its own phase later;
- **declined** — not coming, reason stays loud.

### Deferred

#### A. FetchMany

Smallest. Land first.

Streaming peek already works for FetchOne. Same shape, larger cap.

```text
PgRequest::fetch_many(sql, max_rows)
  -> PgResponse::Rows { rows: Vec<PgRow>, truncated: bool }
```

Rules:

- read at most `max_rows + 1` rows from the SQLx stream;
- past `max_rows`: stop pulling, set `truncated = true`;
- bridge config keeps `max_response_rows` ceiling; caller cannot ask
  for `usize::MAX`;
- helper: `fetch_many_call`;
- per-row / per-cell byte caps deferred; first form caps row count.

Open: truncated success vs hard `ResponseTooLarge` error?

Pick `truncated = true`. Rows are still useful. Caller who wants strict
fail-past-N sets `max_rows + 1` and checks `truncated`.

#### B. Transactions

Two shapes. Pick before coding.

**Shape 1 — atomic script.** Caller sends ordered list of requests.
Bridge runs them in one transaction. COMMIT on all-OK. ROLLBACK on
first error. Reply with per-step outcomes.

```text
PgTransaction::new()
    .step(PgRequest::execute("UPDATE ... -100 ..."))
    .step(PgRequest::execute("UPDATE ... +100 ..."))
    .commit_call(addr, timeout)
    .reply(AppMsg::Transferred)
```

Good:

- one Tina admission;
- one pool acquire;
- no tx-id threading;
- no long-lived bridge state.

Bad:

- cannot rollback based on app logic between steps.

**Shape 2 — transaction handle.** `Begin -> TxId`. Subsequent
`Execute(TxId, ...)` runs on the pinned connection. `Commit(TxId)` /
`Rollback(TxId)` close it. Bridge holds `HashMap<TxId, PoolConnection>`
plus idle timeout so a forgotten handle does not leak a connection.

Good:

- full flexibility.

Bad:

- long-lived per-call state;
- tx-id threading;
- idle / leak policy is a real choice;
- unhappy paths multiply: caller drop, bridge close, runtime drop.

Pick Shape 1 first. Covers most real transactions. No state machine
surprises. Add Shape 2 only when Specimen or a real user pushes.

Cancel/timeout interaction:

- per-attempt timeout still fires;
- future still runs to completion (075 cancellation rule);
- connection stays held until the script finishes;
- consider a separate `tx_timeout` distinct from `default_timeout`.

#### C. DB-side CancelRequest

Highest payoff for late-result truth. Hardest protocol work.

Postgres `CancelRequest`:

- separate TCP connection;
- carries target backend `(pid, secret_key)`;
- best effort — Postgres may ignore mid-flight.

Two paths:

1. **`pg_cancel_backend(pid)` via sidecar pool.** Portable. No protocol
   work. Capture PID once via `SELECT pg_backend_pid()`; store on the
   in-flight slot. On timeout, spawn a sidecar task: `SELECT
   pg_cancel_backend($1)`.

2. **Raw cancel connection.** Lower level. Needs `(pid, secret)` from
   SQLx startup negotiation. Check current SQLx 0.8 API surface
   (`PgConnection::pg_id()` etc.) before committing.

Pick path 1 first. Correct, boring, drives `late_results` toward zero
in normal operation. Move to path 2 only if sidecar overhead becomes a
real complaint.

New metric: `db_cancels_sent`. Not the same as `late_results`. Doc:
even a successful cancel does not kill every query.

### Order

```text
A FetchMany    -> simplest; reuses streaming peek; small surface
B Transactions -> shape 1 first; constrains cancel/timeout interaction
C CancelRequest -> waits on B; mid-tx cancel is different from
                   single-statement cancel; ship cancel knowing what
                   it is cancelling
```

A and B are independent. C waits on B.

### Declined

Two original non-goals get no follow-up. Reasons stay loud so a future
contributor does not quietly add them.

#### D. Generic `sqlx::Database`

Generic over `sqlx::Database` needs its own decode trait, its own value
enum (or generic `Encode + Decode`), per-database error mapping. Type
soup. One concrete bridge per backend is the boring answer.

If MySQL or another SQLx backend earns adoption, ship
`tina-mysql-bridge` (or similar) as a *separate crate* with the same
shape. Do not generalize this crate.

051E said no. Same call here.

#### E. ORM, migrations, schema discovery, user-struct row mapping

Different products from "bounded ingress around a SQL pool."

User-struct mapping alone needs a derive macro, type-level column
matching, and a nullable/missing-column story. Each of the four items
would dwarf the bridge they live next to.

Existing answers are better:

- `sqlx`'s own `query_as!` for struct mapping (when offline metadata
  is acceptable);
- `sqlx-migrate` / `refinery` for migrations;
- domain-specific SQL builders for query construction;
- application code on top of `PgRow` for ad-hoc decode.

Bridge job: bounded admission and visible pressure. Not modeling.
