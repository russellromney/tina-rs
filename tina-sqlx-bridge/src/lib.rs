#![forbid(unsafe_code)]
#![deny(missing_docs)]

//! Bounded Postgres worker around `sqlx::PgPool`, for Tina services.
//!
//! Adoption bridge. Not a native Tina DB client. Tokio and SQLx own
//! the Postgres connection pool, the wire protocol, and TLS. Tina
//! owns bounded ingress, visible pressure, per-attempt timeout, and
//! late-result truth.
//!
//! This crate is **Postgres-first** and **first-form**: `Execute` and
//! `FetchOne` only. Generic `sqlx::Database` support, transactions,
//! row streaming, `FetchMany`, user-struct mapping, ORM/migrations,
//! and DB-side cancellation are explicit non-goals in this slice. See
//! the phase plan for the deferred set.
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
//!             .reply(AppMsg::DbDone),
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
//! # Cancellation rule
//!
//! Same as every Tina bridge: the bridge does not lie about the
//! database.
//!
//! - Before admission: no SQLx work starts.
//! - After admission, the spawned task runs on Tokio. The bridge's
//!   per-attempt deadline detaches the result receiver and surfaces
//!   `PgError::Timeout` to the caller. The spawned future is
//!   **not** aborted — it keeps running until SQLx returns
//!   naturally. The connection stays held until then.
//! - The bridge does **not** issue a Postgres `CancelRequest` in
//!   first form. Postgres keeps executing the query until it
//!   completes. Treat `PgError::Timeout` as "Tina stopped waiting,"
//!   not "the database stopped working." DB-side cancellation is a
//!   deferred follow-up.
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
//! - Cancellation precision: once the spawned SQLx task starts on
//!   Tokio, the bridge cannot stop it. The per-attempt timeout
//!   detaches but does not abort; Postgres keeps executing until the
//!   query naturally completes. Late completions are tallied in
//!   `late_results`, not in caller-observed metrics. DB-side
//!   `CancelRequest` is a deferred follow-up.
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
    PgRows, PgTransactionCallOutcome, PgTransientReason, TransactionCall, execute_call,
    fetch_many_call, fetch_one_call, send_request, transaction_call,
};
pub use metrics::{PgMetrics, PgMetricsHandle};
pub use types::{
    InstallError, PgConfig, PgConfigError, PgError, PgPoolConfig, PgRequest, PgResponse, PgRow,
    PgStep, PgStepOk, PgTransactionOutcome, PgValue, U64TooLarge,
};
pub use worker::{InstalledPgBridge, PgCloser, PgMsg, PgWorker};
