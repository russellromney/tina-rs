#![forbid(unsafe_code)]
#![deny(missing_docs)]

//! Bounded Postgres worker around `sqlx::PgPool`, for Tina services.
//!
//! Adoption bridge. Not a native Tina DB client. Tokio and SQLx own
//! the Postgres connection pool, the wire protocol, and TLS. Tina
//! owns bounded ingress, visible pressure, per-attempt timeout, and
//! late-result truth.
//!
//! This crate is **Postgres-first**. It ships:
//!
//! - [`PgRequest::Execute`] — row-less statement;
//! - [`PgRequest::FetchOne`] — exactly zero or one row, with
//!   streaming peek so a query that accidentally returns many rows
//!   surfaces [`PgError::TooManyRows`] without buffering;
//! - [`PgRequest::FetchMany`] — bounded streaming, hard row cap with
//!   a `truncated` flag, never grows the bridge buffer past the cap;
//! - [`PgRequest::Transaction`] — atomic script of [`PgStep`]s, all
//!   committed or first-failure rolled back (no nesting);
//! - opt-in DB-side cancel via
//!   [`PgConfig::with_cancel_on_timeout`] — when the bridge
//!   per-attempt timeout fires, a sidecar pool fires
//!   `pg_cancel_backend(pid)` against the captured backend so
//!   Postgres stops actually executing the query.
//!
//! # Cargo features (value types)
//!
//! Beyond the boring core (`bool`, `i64`, `f64`, `String`,
//! `Vec<u8>`), wider Postgres types are gated behind cargo features.
//! Each feature pulls the matching SQLx feature too, so SQLx itself
//! can encode and decode the type:
//!
//! - `uuid` — `PgValue::Uuid(uuid::Uuid)` <-> Postgres `UUID`.
//! - `json` — `PgValue::Json(serde_json::Value)` <-> Postgres
//!   `JSON` and `JSONB` (both decode to the same Rust value; bind
//!   sends as `JSONB`).
//! - `numeric` — `PgValue::Numeric(rust_decimal::Decimal)` <->
//!   Postgres `NUMERIC`. `Decimal` carries up to 28-29 significant
//!   digits.
//! - `time` — temporal types via the `time` crate:
//!   `Timestamp(PrimitiveDateTime)` for `TIMESTAMP`,
//!   `TimestampTz(OffsetDateTime)` for `TIMESTAMPTZ`,
//!   `Date(Date)` for `DATE`.
//!
//! Trade-offs the crate has already made:
//!
//! - `rust_decimal` over `bigdecimal` — fits in 16 bytes, fast,
//!   covers the common case. Users who need arbitrary precision
//!   should ask for a `bigdecimal` feature in a follow-up.
//! - `time` over `chrono` — fewer transitive deps, tighter API. A
//!   `chrono` feature can be added side by side later.
//!
//! Columns whose Postgres type isn't enabled (or isn't covered at
//! all) decode to [`PgError::Decode`] with the type name. Nothing is
//! silently coerced.
//!
//! # NULL bind hints
//!
//! [`PgValue::Null`] sends `INT8 NULL`. Postgres infers the actual
//! parameter type from query context — table columns in
//! `INSERT INTO t (col) VALUES ($1)`, an explicit cast in
//! `SELECT $1::TEXT`, etc. This is fine most of the time.
//!
//! When Postgres can't infer (a positional NULL bind into a
//! non-INT8 column without surrounding type hints), use
//! [`PgValue::TypedNull`] — or one of the `null_*()` shorthands
//! (`PgValue::null_text()`, `PgValue::null_uuid()`, …) — to send a
//! NULL with the right wire-level type oid:
//!
//! ```ignore
//! // Postgres rejects: INT8 NULL bound to UUID column.
//! .param(PgValue::Null)
//!
//! // Works: typed UUID NULL.
//! .param(PgValue::null_uuid())
//!
//! // Equivalent: PgType::Uuid.into() builds a TypedNull.
//! .param(PgValue::from(PgType::Uuid))
//! ```
//!
//! On decode, NULLs land in [`PgValue::Null`] regardless of how they
//! were bound. Only the bind side cares about the type tag.
//!
//! Generic `sqlx::Database` support, user-struct row mapping, an
//! ORM, migrations, and a transaction *handle* (vs. atomic script)
//! are explicit non-goals. See the phase plan for the declined set
//! and the rationale.
//!
//! # Use
//!
//! Build a worker, register it on a Tina runtime, then `execute_call`
//! / `fetch_one_call` from any Tina handler:
//!
//! ```no_run
//! use std::convert::Infallible;
//! use std::time::Duration;
//!
//! use tina::prelude::*;
//! use tina_runtime::{CallOutcome, RuntimeCall};
//! use tina_sqlx_bridge::{PgAddress, PgExecutedOutcome, execute_call};
//!
//! enum AppMsg {
//!     Start,
//!     DbDone(PgExecutedOutcome),
//! }
//!
//! struct App {
//!     db: PgAddress,
//! }
//!
//! impl Isolate for App {
//!     tina::isolate_types! {
//!         message: AppMsg,
//!         reply: (),
//!         send: tina::Outbound<Infallible>,
//!         spawn: Infallible,
//!         call: RuntimeCall<AppMsg>,
//!         shard: SingleShard,
//!     }
//!
//!     fn handle(&mut self, msg: AppMsg, _ctx: &mut Context<'_, SingleShard, Self::Reply>) -> Effect<Self> {
//!         match msg {
//!             AppMsg::Start => execute_call(
//!                 self.db,
//!                 "INSERT INTO t (x) VALUES ($1)",
//!                 vec![7.into()],
//!                 Duration::from_secs(2),
//!             )
//!             .then(AppMsg::DbDone),
//!             AppMsg::DbDone(outcome) => match outcome {
//!                 CallOutcome::Replied(Ok(_rows_affected)) => stop(),
//!                 _ => stop(),
//!             },
//!         }
//!     }
//! }
//! ```
//!
//! # Two install paths
//!
//! - [`PgWorker::install`] builds a `PgPool` (and a small Tokio
//!   runtime) from [`PgConfig`]'s pool settings. Use this when the
//!   bridge is the only consumer of the pool.
//! - [`PgWorker::install_with_pool`] wraps a caller-supplied `PgPool`
//!   and Tokio runtime handle. Use this when other code in the
//!   application already holds a pool. The supplied pool owns its
//!   own SQLx settings; the bridge does not re-apply them.
//!
//! # Tina caps vs SQLx pool caps
//!
//! Both layers are bounded and both report independently:
//!
//! ```text
//! mailbox_capacity   -> CallError::TargetFull (Tina ingress)
//! max_in_flight      -> PgError::Full
//! per-attempt clock  -> PgError::Timeout
//! pool acquire clock -> PgError::PoolAcquireTimeout
//! pool closed        -> PgError::PoolClosed
//! invalid request    -> PgError::InvalidRequest(detail)
//! sqlx error         -> PgError::Sqlx(detail)
//! decode error       -> PgError::Decode(detail)
//! too many rows      -> PgError::TooManyRows
//! worker closed      -> PgError::Closed
//! ```
//!
//! `Full` is not `PoolAcquireTimeout`. Tina admission and SQLx pool
//! pressure are different bottlenecks; the bridge surfaces them
//! separately.
//!
//! # Bridge as pool
//!
//! `PgWorker` with `max_in_flight = N` already acts as the pool.
//! SQLx owns the connections; the bridge bounds admitted query work
//! above that pool. There is no extra wrapper needed.
//!
//! To observe the pool shape, use [`PgMetricsHandle::pressure_report`]:
//!
//! ```ignore
//! let report = bridge.metrics.pressure_report();
//! // report.capacity      == config.max_in_flight
//! // report.leased        == current in-flight count
//! // report.available     == free slots
//! // report.full_count    == cumulative PgError::Full
//! // report.timeout_count == bridge deadlines fired
//! // report.high_water    == peak in-flight observed
//! ```
//!
//! `waiters` is always `0` because the bridge does not queue callers;
//! it replies [`PgError::Full`] immediately. SQL errors (including
//! [`PgError::PoolAcquireTimeout`]) do **not** retire the lane.
//!
//! # Cancellation rule
//!
//! Same as every Tina bridge: the bridge does not lie about the
//! database.
//!
//! - Before admission: no SQLx work starts.
//! - After admission, the spawned task runs on Tokio. The bridge's
//!   per-attempt deadline detaches the result receiver and surfaces
//!   `PgError::Timeout` to the caller. The spawned future is
//!   **not** aborted — abort would cancel SQLx at the next `.await`
//!   and skip the tally entirely.
//! - **Default**: Postgres keeps executing the query until it
//!   completes. The connection stays held until then. Treat
//!   `PgError::Timeout` as "Tina stopped waiting," not "the database
//!   stopped working."
//! - **Opt-in via [`PgConfig::with_cancel_on_timeout`]**: the bridge
//!   builds a sidecar pool from the same URL and fires
//!   `pg_cancel_backend(pid)` when the per-attempt timeout fires.
//!   Postgres still does not guarantee mid-flight cancellation, but
//!   most cancellable statements (sleeps, long-running queries)
//!   abort and the connection returns to the pool sooner. Cost: one
//!   extra round trip per admitted request to capture
//!   `pg_backend_pid()` before running the user's query.
//! - Cancel is best-effort. There is a small race window between
//!   "bridge fires cancel against PID 5" and "spawn finishes, conn
//!   returns to pool, conn is reused for another query." A cancel
//!   that arrives in that window targets a different query on the
//!   same backend. Mitigations: keep the bridge per-attempt deadline
//!   well above normal latency, and accept that
//!   `db_cancels_sent` is an attempt counter, not a "we killed your
//!   exact query" counter.
//! - Caller `CallOutcome::Timeout` is **not** the same as bridge
//!   `PgError::Timeout`. The first means the caller stopped
//!   waiting; the second means the bridge stopped waiting.
//!
//! # Late results
//!
//! When the bridge's per-attempt timeout fires, the abandoned flag
//! is set and the receiver is dropped. The spawned task continues;
//! when it finishes, `tally_worker_terminal` records the actual
//! outcome (success / SQLx error / decode / pool variant) and the
//! abandoned flag triggers `late_results`. The eventual `tx.send` is
//! a no-op against a dropped receiver. So `late_results` is reliable
//! for cases where SQLx returned a value after the bridge gave up.
//!
//! `late_results` does **not** count Postgres-side execution that
//! continues past the future drop, nor does it count the
//! caller-observed `CallOutcome::Timeout` path. The first needs a
//! `CancelRequest` story; the second lives in the runtime trace as
//! `CallReplyRejected`.
//!
//! # Bridge vs SQLite bridge
//!
//! Use `tina-sqlite-bridge` for SQLite. SQLite is sync C and the
//! bridge owns one blocking connection — there is no two-runtime
//! cost, no async pool, no pool acquire timeout. This crate is for
//! Postgres specifically; the SQLx pool's parallelism, acquire
//! latency, and TLS setup are real costs that an SQLite bridge does
//! not pay.
//!
//! # Preserved Tina guarantees
//!
//! - Bounded ingress: `mailbox_capacity` caps queued sends; past the
//!   cap the caller observes `CallError::TargetFull`.
//! - Bounded in-flight: `max_in_flight` caps concurrent SQLx tasks
//!   independently of the SQLx pool's own `max_connections`.
//! - Visible failure: every failure mode is a typed [`PgError`]
//!   variant. Nothing is silently absorbed.
//! - Synchronous handlers: the worker isolate handles each message in
//!   one synchronous turn. SQLx work runs on Tokio via spawn; the
//!   shard thread does not block.
//!
//! # Weakened Tina guarantees
//!
//! - Deterministic replay: SQLx network IO is not deterministic and
//!   is not observed by `tina-sim`. Replay parity is best-effort at
//!   the bridge boundary only.
//! - Cancellation precision: cancel-on-timeout is best-effort.
//!   Default is no cancel — Postgres keeps executing past the
//!   bridge's deadline. With cancel enabled, the sidecar fires
//!   `pg_cancel_backend(pid)`; Postgres may or may not honor it,
//!   and a small race window exists between cancel firing and the
//!   target connection being released to the pool. Late completions
//!   are tallied in `late_results` regardless.
//! - Polling latency: the bridge wakes via Tina's `sleep` to
//!   re-check the result channel each `poll_interval`. That is
//!   visible chatter in the trace and adds bounded latency to
//!   completion.
//!
//! # Tested support
//!
//! Compile-only tests cover the SQLx code paths without a database.
//! Integration tests against a real Postgres are gated behind
//! `#[ignore]` and an environment variable; CI does not need real
//! credentials, and ordinary `cargo test -p tina-sqlx-bridge` runs
//! the full unit/fake suite without contacting a server.

mod helpers;
mod metrics;
mod types;
mod worker;

pub use helpers::{
    ExecuteCall, FetchManyCall, FetchOneCall, PgAddress, PgCallOutcome, PgExecutedOutcome,
    PgFatalReason, PgFetchManyOutcome, PgFetchOneOutcome, PgOutcomeClass, PgOutcomeExt, PgResult,
    PgRows, PgTransactionCallOutcome, PgTransientReason, TransactionCall, bridge_class,
    execute_call, fetch_many_call, fetch_one_call, send_request, transaction_call,
};
pub use metrics::{PG_BRIDGE_SURFACE, PgMetrics, PgMetricsHandle, PgPressureReport};
// Shared bridge vocabulary lives in `tina_runtime::bridge`.
pub use tina_runtime::bridge::{
    BridgeFatal, BridgeOutcomeClass, BridgePressure, BridgeRetryable, BridgeUnavailable,
};
pub use types::{
    InstallError, PgCancelConfig, PgConfig, PgConfigError, PgError, PgPoolConfig, PgRequest,
    PgResponse, PgRow, PgStep, PgStepOk, PgTransactionOutcome, PgType, PgValue, U64TooLarge,
};
pub use worker::{InstalledPgBridge, PgCloser, PgMsg, PgWorker};
