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
//! ```no_run
//! use std::convert::Infallible;
//! use std::time::Duration;
//!
//! use tina::prelude::*;
//! use tina_runtime::RuntimeCall;
//! use tina_sqlite_bridge::{
//!     SqliteAddress, SqliteCallOutcome, SqliteConfig, SqliteRequest, SqliteValue,
//!     send_request,
//! };
//!
//! enum AppMsg {
//!     Start,
//!     DbDone(SqliteCallOutcome),
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
//!         call: RuntimeCall<AppMsg>,
//!         shard: SingleShard,
//!     }
//!
//!     fn handle(&mut self, msg: AppMsg, _ctx: &mut Context<'_, SingleShard, Self::Reply>) -> Effect<Self> {
//!         match msg {
//!             AppMsg::Start => send_request(
//!                 self.db,
//!                 SqliteRequest::Execute {
//!                     sql: "INSERT INTO t (x) VALUES (?)".into(),
//!                     params: vec![SqliteValue::Integer(7)],
//!                 },
//!                 Duration::from_secs(2),
//!             )
//!             .reply(AppMsg::DbDone),
//!             AppMsg::DbDone(_) => stop(),
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
//! [`SqliteError::Full`]. A pooled form would change that — first
//! form is intentionally serial.
//!
//! # Cancellation rule
//!
//! `rusqlite` work is not cancelled. If the caller times out, or the
//! bridge's own `default_timeout` fires, the worker thread runs to
//! completion. Its terminal outcome is recorded; the dropped reply
//! shows up in the trace as `CallReplyRejected` and increments
//! `late_results`.
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

mod helpers;
mod metrics;
mod types;
mod worker;

pub use helpers::{
    SqliteAddress, SqliteCallOutcome, SqliteResult, execute, query_rows, send_request,
};
pub use metrics::{SqliteMetrics, SqliteMetricsHandle};
pub use types::{
    InstallError, SqliteConfig, SqliteConfigError, SqliteError, SqlitePath, SqliteRequest,
    SqliteResponse, SqliteValue,
};
pub use worker::{InstalledSqliteBridge, SqliteCloser, SqliteMsg, SqliteWorker};
