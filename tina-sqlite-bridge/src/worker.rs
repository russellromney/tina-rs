//! Bridge isolate + one blocking std thread that owns the single
//! `rusqlite::Connection`. Bridge → thread is `sync_channel(1)`;
//! thread → bridge is one fresh oneshot per request.
//!
//! Not simulator-replayable. The blocking thread + `rusqlite` C calls
//! are outside `tina-sim`'s observation; replay parity is best-effort
//! at the bridge boundary only.

use std::convert::Infallible;
use std::marker::PhantomData;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender, SyncSender};
use std::thread::{self, JoinHandle};
use std::time::Instant;

use rusqlite::{Connection, OpenFlags, types::ValueRef};
use tina::prelude::*;
use tina_runtime::{MailboxFactory, RuntimeCall, ThreadedRuntime, sleep};
#[cfg(feature = "tracing")]
use tracing::{Level, event};

use crate::SqliteError;
use crate::helpers::SqliteResult;
use crate::metrics::{MetricsInner, SqliteMetricsHandle};
use crate::types::{
    InstallError, SqliteConfig, SqlitePath, SqliteRequest, SqliteResponse, SqliteValue,
};

/// `tracing` target for per-call events on the bridge isolate.
#[cfg(feature = "tracing")]
const TRACE_TARGET_CALL: &str = "tina_sqlite.bridge.call";

/// `tracing` target for bridge lifecycle events (close, etc.).
#[cfg(feature = "tracing")]
const TRACE_TARGET_BRIDGE: &str = "tina_sqlite.bridge";

fn request_kind_name(request: &SqliteRequest) -> &'static str {
    match request {
        SqliteRequest::Execute { .. } => "execute",
        SqliteRequest::QueryRows { .. } => "query",
    }
}

#[cfg(feature = "tracing")]
fn admission_reason_name(err: &SqliteError) -> &'static str {
    match err {
        SqliteError::Full => "Full",
        SqliteError::Closed => "Closed",
        SqliteError::InvalidRequest(_) => "InvalidRequest",
        SqliteError::Internal(_) => "Internal",
        // Worker-class errors never fire from admission. Fall through
        // to a stable string so a future variant doesn't silently log
        // an empty reason.
        _ => "Other",
    }
}

#[cfg(feature = "tracing")]
fn worker_reason_name(err: &SqliteError) -> &'static str {
    match err {
        SqliteError::Busy => "Busy",
        SqliteError::Constraint(_) => "Constraint",
        SqliteError::Io(_) => "Io",
        SqliteError::Sqlite(_) => "Sqlite",
        SqliteError::ResponseTooLarge => "ResponseTooLarge",
        SqliteError::Timeout => "Timeout",
        SqliteError::Internal(_) => "Internal",
        // Admission-class errors never come from the worker thread.
        _ => "Other",
    }
}

/// Emits one tracing event for a worker-terminal `SqliteResult` the
/// bridge isolate is about to forward to the caller. Success replies
/// log at DEBUG with `outcome=executed|rows`; typed errors log at
/// WARN, internal errors at ERROR. Bridge-side timeouts are emitted
/// at the timeout site, not here.
#[cfg(feature = "tracing")]
fn emit_replied(result: &SqliteResult, request_kind: &'static str) {
    match result {
        Ok(SqliteResponse::Executed { rows_changed }) => event!(
            target: TRACE_TARGET_CALL,
            Level::DEBUG,
            kind = "replied",
            outcome = "executed",
            request_kind,
            rows_changed = *rows_changed,
        ),
        Ok(SqliteResponse::Rows { rows, .. }) => event!(
            target: TRACE_TARGET_CALL,
            Level::DEBUG,
            kind = "replied",
            outcome = "rows",
            request_kind,
            row_count = rows.len() as u64,
        ),
        Err(err) => {
            let reason = worker_reason_name(err);
            let level = match err {
                SqliteError::Internal(_) => Level::ERROR,
                _ => Level::WARN,
            };
            // tracing::event! bakes the level into static callsite
            // metadata, so each level needs its own invocation.
            match level {
                Level::ERROR => event!(
                    target: TRACE_TARGET_CALL,
                    Level::ERROR,
                    kind = "replied",
                    reason,
                    request_kind,
                    detail = %err,
                ),
                _ => event!(
                    target: TRACE_TARGET_CALL,
                    Level::WARN,
                    kind = "replied",
                    reason,
                    request_kind,
                    detail = %err,
                ),
            }
        }
    }
}

/// Messages handled by [`SqliteWorker`]. `Request` is the user-facing
/// variant; `Poll` is the internal sleep continuation.
#[derive(Debug)]
pub enum SqliteMsg {
    /// Submit one SQLite operation and reply with its outcome.
    Request(SqliteRequest),
    /// Internal sleep wakeup. User code must not construct.
    #[doc(hidden)]
    Poll,
}

/// At most one of these lives at a time (max_in_flight = 1).
struct InFlight {
    response_rx: Receiver<SqliteResult>,
    started_at: Instant,
    abandoned: Arc<AtomicBool>,
    /// Stable name of the original request for tracing field reuse.
    /// `"execute"` or `"query"`. Carried even when the `tracing`
    /// feature is off so the field stays cheap and the type does
    /// not branch on cfg.
    request_kind: &'static str,
}

enum WorkerCmd {
    Run {
        request: SqliteRequest,
        response_tx: Sender<SqliteResult>,
        max_response_rows: usize,
        /// Set by the bridge when it surfaces `Timeout`. The worker
        /// thread checks it after running and bumps `late_results` if
        /// set. No tally double-count: the per-outcome counter is
        /// always bumped once, in `worker_loop`.
        abandoned: Arc<AtomicBool>,
    },
}

/// Joins the blocking thread on drop. Worker loop exits when its
/// command channel closes (`cmd_tx` is on the bridge isolate, dropped
/// before this).
struct WorkerThread(Option<JoinHandle<()>>);

impl Drop for WorkerThread {
    fn drop(&mut self) {
        if let Some(handle) = self.0.take() {
            let _ = handle.join();
        }
    }
}

/// Bounded SQLite worker isolate. Submits requests to one blocking
/// thread that holds the `rusqlite::Connection`; polls for replies.
/// Shard thread does no SQLite work.
pub struct SqliteWorker<S: Shard + 'static> {
    config: SqliteConfig,
    cmd_tx: SyncSender<WorkerCmd>,
    in_flight: Option<InFlight>,
    closed: Arc<AtomicBool>,
    metrics: Arc<MetricsInner>,
    _worker_thread: WorkerThread,
    _shard: PhantomData<S>,
}

/// Result of [`SqliteWorker::install`].
pub struct InstalledSqliteBridge<S: Shard + 'static> {
    /// Tina address callers use with `call(...)`.
    pub address: Address<SqliteMsg, SqliteResult>,
    /// Closer for graceful drain.
    pub closer: SqliteCloser,
    /// Metrics handle.
    pub metrics: SqliteMetricsHandle,
    _shard: PhantomData<S>,
}

/// Cloneable closer. Non-blocking. Flips the closed flag; new
/// admissions reply [`SqliteError::Closed`]. The in-flight request
/// runs to completion. There is no force-cancel: `rusqlite` is sync C
/// code with no cancellation handle, and the worker thread always
/// finishes its current SQLite call. Dropping the bridge isolate
/// closes the command channel, after which the worker thread exits at
/// its next `recv` — but only after it finishes whatever call is
/// already in progress.
#[derive(Debug, Clone)]
pub struct SqliteCloser {
    closed: Arc<AtomicBool>,
}

impl SqliteCloser {
    /// Mark the worker closed. Idempotent. Returns immediately; does
    /// not wait for in-flight to drain. The in-flight SQLite call,
    /// if any, runs to completion.
    pub fn close(&self) {
        #[cfg(feature = "tracing")]
        {
            // swap so we can suppress the trace event on idempotent re-close.
            let was_closed = self.closed.swap(true, Ordering::AcqRel);
            if !was_closed {
                event!(target: TRACE_TARGET_BRIDGE, Level::DEBUG, kind = "close");
            }
        }
        #[cfg(not(feature = "tracing"))]
        self.closed.store(true, Ordering::Release);
    }

    /// Whether the worker has been closed.
    pub fn is_closed(&self) -> bool {
        self.closed.load(Ordering::Acquire)
    }
}

impl<S: Shard + 'static> SqliteWorker<S> {
    /// Validates config, opens the connection, runs pragmas, spawns
    /// the blocking thread.
    pub fn new(config: SqliteConfig) -> Result<(Self, SqliteMetricsHandle), InstallError> {
        config.validate()?;

        let conn = open_connection(&config.path).map_err(|e| InstallError::Open(format!("{e}")))?;
        if !config.busy_timeout.is_zero() {
            conn.busy_timeout(config.busy_timeout)
                .map_err(|e| InstallError::Setup(format!("busy_timeout: {e}")))?;
        }
        for pragma in &config.pragmas {
            let stmt = format!("PRAGMA {pragma}");
            conn.execute_batch(&stmt)
                .map_err(|e| InstallError::Pragma(format!("'{pragma}': {e}")))?;
        }

        let metrics_inner = Arc::new(MetricsInner::default());
        let metrics_for_thread = Arc::clone(&metrics_inner);

        // sync_channel(1): bridge never sends a second Run before
        // reading the first reply, so 1 is enough.
        let (cmd_tx, cmd_rx) = mpsc::sync_channel::<WorkerCmd>(1);
        let join = thread::Builder::new()
            .name("tina-sqlite-bridge".into())
            .spawn(move || worker_loop(conn, cmd_rx, metrics_for_thread))
            .map_err(|e| InstallError::Spawn(format!("{e}")))?;

        let metrics_handle = SqliteMetricsHandle {
            inner: Arc::clone(&metrics_inner),
        };

        let worker = Self {
            config,
            cmd_tx,
            in_flight: None,
            closed: Arc::new(AtomicBool::new(false)),
            metrics: metrics_inner,
            _worker_thread: WorkerThread(Some(join)),
            _shard: PhantomData,
        };
        Ok((worker, metrics_handle))
    }

    /// Configured mailbox capacity. Use when registering manually.
    pub fn mailbox_capacity(&self) -> usize {
        self.config.mailbox_capacity
    }

    /// Cloneable closer for graceful drain.
    pub fn closer(&self) -> SqliteCloser {
        SqliteCloser {
            closed: Arc::clone(&self.closed),
        }
    }

    fn admit(&mut self, request: SqliteRequest) -> Effect<Self> {
        let request_kind = request_kind_name(&request);

        if self.closed.load(Ordering::Acquire) {
            self.metrics.closed.fetch_add(1, Ordering::Relaxed);
            #[cfg(feature = "tracing")]
            event!(
                target: TRACE_TARGET_CALL,
                Level::WARN,
                kind = "admission_rejected",
                reason = "Closed",
                request_kind,
            );
            return reply::<Self>(Err(SqliteError::Closed));
        }

        if let Err(err) = validate_request(&request, &self.config) {
            self.metrics.invalid.fetch_add(1, Ordering::Relaxed);
            #[cfg(feature = "tracing")]
            event!(
                target: TRACE_TARGET_CALL,
                Level::WARN,
                kind = "admission_rejected",
                reason = admission_reason_name(&err),
                request_kind,
                detail = %err,
            );
            return reply::<Self>(Err(err));
        }

        if self.in_flight.is_some() {
            self.metrics.full.fetch_add(1, Ordering::Relaxed);
            #[cfg(feature = "tracing")]
            event!(
                target: TRACE_TARGET_CALL,
                Level::WARN,
                kind = "admission_rejected",
                reason = "Full",
                request_kind,
            );
            return reply::<Self>(Err(SqliteError::Full));
        }

        let max_rows_for_request = match &request {
            SqliteRequest::QueryRows { max_rows, .. } => {
                core::cmp::min(*max_rows, self.config.max_response_rows)
            }
            SqliteRequest::Execute { .. } => self.config.max_response_rows,
        };

        let (response_tx, response_rx) = mpsc::channel::<SqliteResult>();
        let abandoned = Arc::new(AtomicBool::new(false));
        let cmd = WorkerCmd::Run {
            request,
            response_tx,
            max_response_rows: max_rows_for_request,
            abandoned: Arc::clone(&abandoned),
        };
        // Worker dead, or admission bug let a second Run past the
        // in_flight check. Either way, surface visibly.
        if self.cmd_tx.try_send(cmd).is_err() {
            #[cfg(feature = "tracing")]
            event!(
                target: TRACE_TARGET_CALL,
                Level::ERROR,
                kind = "admission_rejected",
                reason = "Internal",
                request_kind,
                detail = "worker thread unavailable",
            );
            return reply::<Self>(Err(SqliteError::Internal(
                "worker thread unavailable".into(),
            )));
        }

        self.in_flight = Some(InFlight {
            response_rx,
            started_at: Instant::now(),
            abandoned,
            request_kind,
        });
        self.metrics.admitted.fetch_add(1, Ordering::Relaxed);
        self.metrics.set_in_flight(1);
        self.metrics.note_in_flight(1);

        #[cfg(feature = "tracing")]
        event!(
            target: TRACE_TARGET_CALL,
            Level::DEBUG,
            kind = "admitted",
            request_kind,
        );

        sleep(self.config.poll_interval).reply(|_| SqliteMsg::Poll)
    }

    fn poll(&mut self) -> Effect<Self> {
        // Stray poll for an already-finished request. Drop.
        let Some(in_flight) = self.in_flight.take() else {
            return noop();
        };

        let request_kind = in_flight.request_kind;

        match in_flight.response_rx.try_recv() {
            Ok(result) => {
                self.metrics.set_in_flight(0);
                #[cfg(feature = "tracing")]
                emit_replied(&result, request_kind);
                #[cfg(not(feature = "tracing"))]
                let _ = request_kind;
                reply::<Self>(result)
            }
            Err(mpsc::TryRecvError::Empty) => {
                if in_flight.started_at.elapsed() >= self.config.default_timeout {
                    // Bridge timeout. Tell the worker to bump
                    // `late_results` when it finishes; tally happens
                    // in `worker_loop` regardless.
                    in_flight.abandoned.store(true, Ordering::Release);
                    self.metrics.timeouts.fetch_add(1, Ordering::Relaxed);
                    self.metrics.set_in_flight(0);
                    #[cfg(feature = "tracing")]
                    event!(
                        target: TRACE_TARGET_CALL,
                        Level::WARN,
                        kind = "timeout",
                        reason = "Timeout",
                        request_kind,
                        elapsed_ms = in_flight.started_at.elapsed().as_millis() as u64,
                    );
                    return reply::<Self>(Err(SqliteError::Timeout));
                }
                self.in_flight = Some(in_flight);
                sleep(self.config.poll_interval).reply(|_| SqliteMsg::Poll)
            }
            Err(mpsc::TryRecvError::Disconnected) => {
                self.metrics.set_in_flight(0);
                #[cfg(feature = "tracing")]
                event!(
                    target: TRACE_TARGET_CALL,
                    Level::ERROR,
                    kind = "replied",
                    reason = "Internal",
                    request_kind,
                    detail = "worker thread terminated mid-request",
                );
                reply::<Self>(Err(SqliteError::Internal(
                    "worker thread terminated mid-request".into(),
                )))
            }
        }
    }
}

impl<S: Shard + Send + 'static> SqliteWorker<S> {
    /// Validate, build, register. Returns address + closer + metrics.
    pub fn install<F>(
        runtime: &ThreadedRuntime<S, F>,
        config: SqliteConfig,
    ) -> Result<InstalledSqliteBridge<S>, InstallError>
    where
        F: MailboxFactory + Send + 'static,
    {
        let cap = config.mailbox_capacity;
        let (worker, metrics) = Self::new(config)?;
        let closer = worker.closer();
        let address = runtime
            .register_with_capacity::<_, Infallible>(worker, cap)
            .map_err(|e| InstallError::Register(format!("{e:?}")))?;
        Ok(InstalledSqliteBridge {
            address,
            closer,
            metrics,
            _shard: PhantomData,
        })
    }
}

impl<S: Shard + 'static> Isolate for SqliteWorker<S> {
    tina::isolate_types! {
        message: SqliteMsg,
        reply: SqliteResult,
        send: tina::Outbound<Infallible>,
        spawn: Infallible,
        call: RuntimeCall<SqliteMsg>,
        shard: S,
    }

    fn handle(&mut self, msg: SqliteMsg, _ctx: &mut Context<'_, S, Self::Reply>) -> Effect<Self> {
        match msg {
            SqliteMsg::Request(request) => self.admit(request),
            SqliteMsg::Poll => self.poll(),
        }
    }
}

/// Structural checks only. Database not touched. Multi-statement SQL
/// is not detected here; rusqlite rejects it at execute / prepare.
fn validate_request(request: &SqliteRequest, config: &SqliteConfig) -> Result<(), SqliteError> {
    let (sql, params_len) = match request {
        SqliteRequest::Execute { sql, params } => (sql, params.len()),
        SqliteRequest::QueryRows { sql, params, .. } => (sql, params.len()),
    };
    if sql.trim().is_empty() {
        return Err(SqliteError::InvalidRequest("empty sql".into()));
    }
    if params_len > config.max_request_params {
        return Err(SqliteError::InvalidRequest(format!(
            "params {params_len} exceeds cap {}",
            config.max_request_params
        )));
    }
    Ok(())
}

fn tally_worker_terminal(metrics: &MetricsInner, result: &SqliteResult) {
    match result {
        Ok(SqliteResponse::Executed { .. }) => {
            metrics.worker_executed.fetch_add(1, Ordering::Relaxed);
        }
        Ok(SqliteResponse::Rows { .. }) => {
            metrics.worker_rows.fetch_add(1, Ordering::Relaxed);
        }
        Err(SqliteError::ResponseTooLarge) => {
            metrics
                .worker_response_too_large
                .fetch_add(1, Ordering::Relaxed);
        }
        Err(SqliteError::Busy) => {
            metrics.worker_busy.fetch_add(1, Ordering::Relaxed);
        }
        Err(SqliteError::Constraint(_)) => {
            metrics.worker_constraint.fetch_add(1, Ordering::Relaxed);
        }
        Err(SqliteError::Io(_)) => {
            metrics.worker_io.fetch_add(1, Ordering::Relaxed);
        }
        Err(SqliteError::Sqlite(_)) => {
            metrics.worker_sqlite.fetch_add(1, Ordering::Relaxed);
        }
        // Admission-class errors never come from the worker thread.
        Err(_) => {}
    }
}

fn open_connection(path: &SqlitePath) -> Result<Connection, rusqlite::Error> {
    let flags = OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_CREATE;
    match path {
        SqlitePath::Memory => Connection::open_in_memory(),
        SqlitePath::File(p) => Connection::open_with_flags(p, flags),
    }
}

/// Blocking worker loop. Owns the single `rusqlite::Connection`. Exits
/// when the command channel closes.
fn worker_loop(mut conn: Connection, rx: Receiver<WorkerCmd>, metrics: Arc<MetricsInner>) {
    while let Ok(cmd) = rx.recv() {
        match cmd {
            WorkerCmd::Run {
                request,
                response_tx,
                max_response_rows,
                abandoned,
            } => {
                let result = run_request(&mut conn, request, max_response_rows);
                tally_worker_terminal(&metrics, &result);
                if abandoned.load(Ordering::Acquire) {
                    metrics.late_results.fetch_add(1, Ordering::Relaxed);
                }
                let _ = response_tx.send(result);
            }
        }
    }
    drop(conn);
}

fn run_request(
    conn: &mut Connection,
    request: SqliteRequest,
    max_response_rows: usize,
) -> SqliteResult {
    match request {
        SqliteRequest::Execute { sql, params } => {
            let bound = bind_params(params);
            match conn.execute(&sql, rusqlite::params_from_iter(bound.iter())) {
                Ok(rows) => Ok(SqliteResponse::Executed {
                    rows_changed: rows as u64,
                }),
                Err(e) => Err(map_rusqlite_error(e)),
            }
        }
        SqliteRequest::QueryRows {
            sql,
            params,
            max_rows,
        } => {
            let row_cap = core::cmp::min(max_rows, max_response_rows);
            let mut stmt = match conn.prepare(&sql) {
                Ok(s) => s,
                Err(e) => return Err(map_rusqlite_error(e)),
            };
            let column_count = stmt.column_count();
            let columns: Vec<String> = (0..column_count)
                .map(|i| {
                    stmt.column_name(i)
                        .expect("column index in 0..column_count")
                        .to_string()
                })
                .collect();
            let bound = bind_params(params);
            let mut query_rows = match stmt.query(rusqlite::params_from_iter(bound.iter())) {
                Ok(rows) => rows,
                Err(e) => return Err(map_rusqlite_error(e)),
            };

            let mut rows: Vec<Vec<SqliteValue>> = Vec::new();
            loop {
                match query_rows.next() {
                    Ok(Some(row)) => {
                        if rows.len() >= row_cap {
                            return Err(SqliteError::ResponseTooLarge);
                        }
                        let mut cells = Vec::with_capacity(column_count);
                        for i in 0..column_count {
                            match row.get_ref(i) {
                                Ok(value_ref) => cells.push(value_ref_to_owned(value_ref)),
                                Err(e) => return Err(map_rusqlite_error(e)),
                            }
                        }
                        rows.push(cells);
                    }
                    Ok(None) => break,
                    Err(e) => return Err(map_rusqlite_error(e)),
                }
            }
            Ok(SqliteResponse::Rows { columns, rows })
        }
    }
}

/// Owns the values so `ToSql` borrows live as long as the statement.
fn bind_params(params: Vec<SqliteValue>) -> Vec<BoundValue> {
    params.into_iter().map(BoundValue::from).collect()
}

enum BoundValue {
    Null,
    Integer(i64),
    Real(f64),
    Text(String),
    Blob(Vec<u8>),
}

impl From<SqliteValue> for BoundValue {
    fn from(v: SqliteValue) -> Self {
        match v {
            SqliteValue::Null => BoundValue::Null,
            SqliteValue::Integer(i) => BoundValue::Integer(i),
            SqliteValue::Real(r) => BoundValue::Real(r),
            SqliteValue::Text(s) => BoundValue::Text(s),
            SqliteValue::Blob(b) => BoundValue::Blob(b),
        }
    }
}

impl rusqlite::ToSql for BoundValue {
    fn to_sql(&self) -> rusqlite::Result<rusqlite::types::ToSqlOutput<'_>> {
        Ok(match self {
            BoundValue::Null => rusqlite::types::ToSqlOutput::Borrowed(ValueRef::Null),
            BoundValue::Integer(i) => rusqlite::types::ToSqlOutput::Borrowed(ValueRef::Integer(*i)),
            BoundValue::Real(r) => rusqlite::types::ToSqlOutput::Borrowed(ValueRef::Real(*r)),
            BoundValue::Text(s) => {
                rusqlite::types::ToSqlOutput::Borrowed(ValueRef::Text(s.as_bytes()))
            }
            BoundValue::Blob(b) => {
                rusqlite::types::ToSqlOutput::Borrowed(ValueRef::Blob(b.as_slice()))
            }
        })
    }
}

fn value_ref_to_owned(value_ref: ValueRef<'_>) -> SqliteValue {
    match value_ref {
        ValueRef::Null => SqliteValue::Null,
        ValueRef::Integer(i) => SqliteValue::Integer(i),
        ValueRef::Real(r) => SqliteValue::Real(r),
        ValueRef::Text(bytes) => match std::str::from_utf8(bytes) {
            Ok(s) => SqliteValue::Text(s.to_string()),
            Err(_) => SqliteValue::Blob(bytes.to_vec()),
        },
        ValueRef::Blob(bytes) => SqliteValue::Blob(bytes.to_vec()),
    }
}

fn map_rusqlite_error(err: rusqlite::Error) -> SqliteError {
    use rusqlite::ErrorCode;
    match &err {
        rusqlite::Error::SqliteFailure(code, msg) => {
            let detail = msg.clone().unwrap_or_else(|| format!("{}", err));
            match code.code {
                ErrorCode::DatabaseBusy | ErrorCode::DatabaseLocked => SqliteError::Busy,
                ErrorCode::ConstraintViolation => SqliteError::Constraint(detail),
                ErrorCode::FileLockingProtocolFailed
                | ErrorCode::DiskFull
                | ErrorCode::OperationInterrupted
                | ErrorCode::SystemIoFailure
                | ErrorCode::CannotOpen
                | ErrorCode::PermissionDenied => SqliteError::Io(detail),
                _ => SqliteError::Sqlite(detail),
            }
        }
        _ => SqliteError::Sqlite(format!("{err}")),
    }
}
