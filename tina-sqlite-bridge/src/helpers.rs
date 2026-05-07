//! Caller-side helpers: type aliases and call shorthands.
//!
//! The SQLite bridge has two error layers:
//!
//! - **Bridge delivery**: `CallOutcome::Full | Closed | Timeout` is what
//!   the runtime says about the IsolateCall — could it reach the worker,
//!   did the IsolateCall deadline elapse.
//! - **Worker outcome**: [`crate::SqliteError`] is what the worker says
//!   about the request it accepted — bad SQL, busy, constraint violation,
//!   I/O error, response cap exceeded.
//!
//! Both layers stay distinct in the default reply shape:
//!
//! ```ignore
//! AppMsg::DbDone(SqliteCallOutcome)
//! // CallOutcome::Replied(Ok(SqliteResponse::Executed { .. }))
//! // CallOutcome::Replied(Err(SqliteError::Constraint(_)))
//! // CallOutcome::Full | Closed | Timeout
//! ```

use std::time::Duration;

use tina::Address;
use tina_runtime::{CallOutcome, IsolateCall, call};

use crate::SqliteError;
use crate::types::{SqliteRequest, SqliteResponse, SqliteValue};
use crate::worker::SqliteMsg;

/// Worker reply type. Same as the inner `Result` carried inside a
/// [`CallOutcome`].
pub type SqliteResult = Result<SqliteResponse, SqliteError>;

/// Tina address shape for a registered SQLite worker. Use this in
/// isolate fields:
///
/// ```ignore
/// struct App { db: SqliteAddress, ... }
/// ```
pub type SqliteAddress = Address<SqliteMsg, SqliteResult>;

/// Full reply shape returned by the runtime when a Tina caller does
/// `call(addr, SqliteMsg::Request(...), timeout).reply(...)` against a
/// SQLite worker.
pub type SqliteCallOutcome = CallOutcome<SqliteResult>;

/// Submit one SQLite request through a registered worker.
///
/// Thin wrapper over `tina_runtime::call(addr, SqliteMsg::Request(req),
/// timeout)`. No hidden retry, no hidden timeout adjustment, no queue.
/// The returned `IsolateCall` still needs `.reply(...)` to produce an
/// `Effect`; the user picks how to translate the outcome into their
/// own message.
pub fn send_request(
    addr: SqliteAddress,
    request: SqliteRequest,
    timeout: Duration,
) -> IsolateCall<SqliteMsg, SqliteResult> {
    call(addr, SqliteMsg::Request(request), timeout)
}

/// Convenience for an `Execute` call.
pub fn execute(
    addr: SqliteAddress,
    sql: impl Into<String>,
    params: Vec<SqliteValue>,
    timeout: Duration,
) -> IsolateCall<SqliteMsg, SqliteResult> {
    send_request(
        addr,
        SqliteRequest::Execute {
            sql: sql.into(),
            params,
        },
        timeout,
    )
}

/// Convenience for a `QueryRows` call.
pub fn query_rows(
    addr: SqliteAddress,
    sql: impl Into<String>,
    params: Vec<SqliteValue>,
    max_rows: usize,
    timeout: Duration,
) -> IsolateCall<SqliteMsg, SqliteResult> {
    send_request(
        addr,
        SqliteRequest::QueryRows {
            sql: sql.into(),
            params,
            max_rows,
        },
        timeout,
    )
}
