#![forbid(unsafe_code)]
#![deny(missing_docs)]

//! Bounded SQLite worker around `rusqlite`, for Tina services.
//!
//! Smallest honest SQL surface: one connection, one blocking thread,
//! autocommit only, buffered rows. Not pooled. Not transactional. Not
//! typed-row-mapped. Fails visibly under pressure.
//!
//! # Use
//!
//! Typed-helper path is the obvious one: [`execute_call`] projects
//! away the response enum and surfaces `Result<u64, SqliteError>` at
//! the call site. The full-truth [`send_request`] path stays
//! available — use it when you want the response enum visible.
//!
//! ```no_run
//! use std::convert::Infallible;
//! use std::time::Duration;
//!
//! use tina::prelude::*;
//! use tina_runtime::{CallOutcome, RuntimeCall};
//! use tina_sqlite_bridge::{SqliteAddress, SqliteExecutedOutcome, execute_call};
//!
//! enum AppMsg {
//!     Start,
//!     DbDone(SqliteExecutedOutcome),
//! }
//!
//! struct App {
//!     db: SqliteAddress,
//! }
//!
//! impl Isolate for App {
//!     tina::isolate_types! {
//!         message: AppMsg,
//!         reply: (),
//!         send: tina::Outbound<Infallible>,
//!         spawn: Infallible,
//!         io: RuntimeCall<AppMsg>,
//!         shard: SingleShard,
//!     }
//!
//!     fn handle(&mut self, msg: AppMsg, _ctx: &mut Context<'_, SingleShard, Self::Reply>) -> Effect<Self> {
//!         match msg {
//!             AppMsg::Start => execute_call(
//!                 self.db,
//!                 "INSERT INTO t (x) VALUES (?)",
//!                 vec![7.into()],
//!                 Duration::from_secs(2),
//!             )
//!             .then(AppMsg::DbDone),
//!             AppMsg::DbDone(outcome) => match outcome {
//!                 CallOutcome::Replied(Ok(_rows_changed)) => stop(),
//!                 _ => stop(),
//!             },
//!         }
//!     }
//! }
//! ```
//!
//! # Bridge doctrine
//!
//! Every database bridge in Tina names these caps separately:
//!
//! - `mailbox_capacity` — requests waiting to enter the bridge isolate.
//! - `pending_reply_capacity` — callers accepted and waiting for a later
//!   reply.
//! - `max_in_flight` — operations accepted into external work.
//! - `external_pool_size` — foreign blocking workers / connections.
//! - `default_timeout` — bridge-side per-attempt timeout.
//!
//! Pins:
//!
//! - `external_pool_size = 1` (one connection, one blocking worker)
//! - `max_in_flight = 1` (sequential admission)
//!
//! Both reject any other value at config validation. Pooled SQLite,
//! SQLx, and native Postgres bridges inherit the same named caps when
//! they land.
//!
//! `mailbox_capacity` smooths ingress before the worker isolate
//! observes messages; it is **not** an admission queue. Once one DB
//! op is in flight, any later Send the worker observes returns
//! [`SqliteError::Full`].
//!
//! # Bridge as pool
//!
//! `SqliteWorker` with `max_in_flight = 1` is the pool: one
//! connection, one lane, serial admission. Both `external_pool_size`
//! and `max_in_flight` are pinned to `1`. A pooled form would lift
//! these pins and document per-connection isolation rules.
//!
//! To observe the pool shape, use [`SqliteMetricsHandle::pressure_report`]:
//!
//! ```ignore
//! let report = bridge.metrics.pressure_report();
//! // report.capacity   == config.max_in_flight (always 1)
//! // report.leased     == 0 or 1
//! // report.available  == 1 or 0
//! // report.full_count == cumulative SqliteError::Full
//! // report.busy_count == cumulative SQLITE_BUSY
//! // report.high_water == peak in-flight observed (always 0 or 1)
//! ```
//!
//! `waiters` is always `0` because the bridge does not queue callers;
//! it replies [`SqliteError::Full`] immediately. SQL errors (including
//! [`SqliteError::Busy`]) do **not** retire the lane.
//!
//! # Cancellation rule
//!
//! `rusqlite` work is not cancelled. If the caller times out, or the
//! bridge's own `default_timeout` fires, the worker thread runs to
//! completion. Its terminal outcome is recorded; the dropped reply
//! shows up in the trace as `CallReplyRejected` and increments
//! `late_results`.
//!
//! [`SqliteWorker::install_local`] is the application-facade counterpart to
//! [`SqliteWorker::install`]. Both return the same callable address, closer,
//! metrics, and typed setup/registration errors.
//!
//! # Pressure rule
//!
//! ```text
//! mailbox full      -> CallError::TargetFull (Tina ingress)
//! max_in_flight     -> SqliteError::Full
//! per-attempt clock -> SqliteError::Timeout
//! row buffer cap    -> SqliteError::ResponseTooLarge
//! constraint viol.  -> SqliteError::Constraint(detail)
//! SQLITE_BUSY/LOCK  -> SqliteError::Busy
//! worker closed     -> SqliteError::Closed
//! ```
//!
//! # Bridge vs native
//!
//! There is no native SQLite path. SQLite *is* a C library; bridging
//! is the only honest way. Postgres is different: a future native
//! Postgres crate will speak the wire protocol over Tina's own TCP.
//! This crate settles the shape that native client will inherit.
//!
//! # Determinism
//!
//! Not simulator-replayable. The blocking thread and `rusqlite` C
//! calls are outside `tina-sim`'s observation; replay parity is
//! best-effort at the bridge boundary only.

mod budget;
mod helpers;
mod metrics;
mod types;
mod worker;

pub use helpers::{
    ExecuteCall, QueryCall, Row, SqliteAddress, SqliteCallOutcome, SqliteExecutedOutcome,
    SqliteFatalReason, SqliteOutcomeClass, SqliteOutcomeExt, SqliteResult, SqliteRows,
    SqliteRowsOutcome, SqliteTransientReason, execute_call, query_call, send_request,
    sqlite_bridge_class,
};
pub use metrics::{
    SQLITE_BRIDGE_SURFACE, SqliteMetrics, SqliteMetricsHandle, SqlitePressureReport,
};
// Shared bridge vocabulary lives in `tina_runtime::bridge`.
pub use tina_runtime::bridge::{
    BridgeFatal, BridgeOutcomeClass, BridgePressure, BridgeRetryable, BridgeUnavailable,
};
pub use types::{
    InstallError, SqliteConfig, SqliteConfigError, SqliteError, SqlitePath, SqliteProtocolError,
    SqliteRequest, SqliteResponse, SqliteValue, U64TooLarge,
};
pub use worker::{InstalledSqliteBridge, SqliteCloser, SqliteMsg, SqliteWorker};
