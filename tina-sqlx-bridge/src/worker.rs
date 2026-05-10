//! Postgres worker isolate around `sqlx::PgPool`.
//!
//! Bridge isolate runs on the Tina shard. Per-request work runs as a
//! `tokio::spawn` on the bridge's Tokio runtime; the shard thread
//! never blocks on SQLx. The worker isolate polls a `oneshot` for
//! each in-flight request through Tina's own `sleep`, which keeps
//! each result delivery on the trace.
//!
//! Not simulator-replayable. The Tokio runtime + SQLx network IO are
//! outside `tina-sim`'s observation; replay parity is best-effort at
//! the bridge boundary only.

use std::collections::HashMap;
use std::convert::Infallible;
use std::marker::PhantomData;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

use futures_util::TryStreamExt;
use sqlx::postgres::{PgPool, PgPoolOptions, PgRow as SqlxRow};
use sqlx::{Column, Row as _, TypeInfo};
use tina::prelude::*;
use tina_runtime::{MailboxFactory, RuntimeCall, ThreadedRuntime, sleep};
use tokio::runtime::Handle;
use tokio::sync::oneshot;
#[cfg(feature = "tracing")]
use tracing::{Level, event};

use crate::metrics::{MetricsInner, PgMetricsHandle};
use crate::types::{InstallError, PgConfig, PgError, PgRequest, PgResponse, PgRow, PgValue};

/// `tracing` target for per-call events on the bridge isolate.
#[cfg(feature = "tracing")]
const TRACE_TARGET_CALL: &str = "tina_sqlx.bridge.call";

/// `tracing` target for bridge lifecycle events.
#[cfg(feature = "tracing")]
const TRACE_TARGET_BRIDGE: &str = "tina_sqlx.bridge";

/// Worker reply type.
pub type PgResult = Result<PgResponse, PgError>;

fn request_kind_name(request: &PgRequest) -> &'static str {
    match request {
        PgRequest::Execute { .. } => "execute",
        PgRequest::FetchOne { .. } => "fetch_one",
        PgRequest::FetchMany { .. } => "fetch_many",
    }
}

#[cfg(feature = "tracing")]
fn pg_error_reason(err: &PgError) -> &'static str {
    match err {
        PgError::Full => "Full",
        PgError::Closed => "Closed",
        PgError::Timeout => "Timeout",
        PgError::PoolAcquireTimeout => "PoolAcquireTimeout",
        PgError::PoolClosed => "PoolClosed",
        PgError::InvalidRequest(_) => "InvalidRequest",
        PgError::TooManyRows => "TooManyRows",
        PgError::Decode(_) => "Decode",
        PgError::Sqlx(_) => "Sqlx",
        PgError::Internal(_) => "Internal",
    }
}

/// Messages handled by [`PgWorker`]. Only `Send` is constructible by
/// user code.
#[derive(Debug)]
pub enum PgMsg {
    /// Submit one Postgres operation and reply with its outcome.
    Send(PgRequest),
    /// Internal sleep wakeup. User code must not construct.
    #[doc(hidden)]
    Poll(u64),
}

struct InFlight {
    started_at: Instant,
    receiver: oneshot::Receiver<PgResult>,
    abandoned: Arc<AtomicBool>,
    request_kind: &'static str,
}

/// Result of [`PgWorker::install`] / [`PgWorker::install_with_pool`].
pub struct InstalledPgBridge<S: Shard + 'static> {
    /// Tina address callers use with `call(...)`.
    pub address: Address<PgMsg, PgResult>,
    /// Closer for graceful drain.
    pub closer: PgCloser,
    /// Metrics handle.
    pub metrics: PgMetricsHandle,
    _shard: PhantomData<S>,
}

/// Cloneable closer. Non-blocking. Flips the closed flag; new
/// admissions reply [`PgError::Closed`]. In-flight requests run to
/// completion (or hit their per-attempt timeout).
///
/// Closing the bridge does **not** close the SQLx pool. If the bridge
/// owns the pool (config-built path), dropping the bridge isolate
/// drops the pool, which closes it. If the pool was supplied by the
/// caller, the caller owns its lifetime and must close it separately.
#[derive(Debug, Clone)]
pub struct PgCloser {
    closed: Arc<AtomicBool>,
}

impl PgCloser {
    /// Mark the bridge closed. Idempotent.
    pub fn close(&self) {
        #[cfg(feature = "tracing")]
        {
            let was_closed = self.closed.swap(true, Ordering::AcqRel);
            if !was_closed {
                event!(target: TRACE_TARGET_BRIDGE, Level::DEBUG, kind = "close");
            }
        }
        #[cfg(not(feature = "tracing"))]
        self.closed.store(true, Ordering::Release);
    }

    /// Whether the bridge has been closed.
    pub fn is_closed(&self) -> bool {
        self.closed.load(Ordering::Acquire)
    }
}

/// Wraps an owned `tokio::runtime::Runtime` so that drop on the Tina
/// shard thread returns immediately instead of blocking on pending
/// tasks.
struct OwnedRuntime(Option<tokio::runtime::Runtime>);

impl Drop for OwnedRuntime {
    fn drop(&mut self) {
        if let Some(rt) = self.0.take() {
            rt.shutdown_background();
        }
    }
}

/// Drops the `PgPool` inside a Tokio context. Required because SQLx's
/// `Pool` drop calls `tokio::spawn` for shared-state cleanup; dropping
/// it on the Tina shard thread without an active runtime panics with
/// "this functionality requires a Tokio context."
struct PoolHolder {
    pool: Option<PgPool>,
    handle: Handle,
}

impl PoolHolder {
    fn new(pool: PgPool, handle: Handle) -> Self {
        Self {
            pool: Some(pool),
            handle,
        }
    }

    fn pool(&self) -> &PgPool {
        self.pool
            .as_ref()
            .expect("pool present until PoolHolder drop")
    }
}

impl Drop for PoolHolder {
    fn drop(&mut self) {
        if let Some(pool) = self.pool.take() {
            let _entered = self.handle.enter();
            drop(pool);
        }
    }
}

/// Bounded Postgres worker isolate. Runs SQLx queries through the
/// supplied (or config-built) `PgPool` from the bridge's Tokio
/// runtime; the Tina shard thread does no blocking SQL work.
pub struct PgWorker<S: Shard + 'static> {
    config: PgConfig,
    pool: PoolHolder,
    runtime: Handle,
    in_flight: HashMap<u64, InFlight>,
    next_id: u64,
    closed: Arc<AtomicBool>,
    metrics: Arc<MetricsInner>,
    _owned_runtime: Option<OwnedRuntime>,
    _shard: PhantomData<S>,
}

impl<S: Shard + 'static> PgWorker<S> {
    /// Configured mailbox capacity. Use when registering manually.
    pub fn mailbox_capacity(&self) -> usize {
        self.config.mailbox_capacity
    }

    /// Cloneable closer for graceful drain.
    pub fn closer(&self) -> PgCloser {
        PgCloser {
            closed: Arc::clone(&self.closed),
        }
    }

    fn assemble(
        config: PgConfig,
        pool: PgPool,
        runtime: Handle,
        owned_runtime: Option<OwnedRuntime>,
    ) -> (Self, PgMetricsHandle) {
        let metrics_inner = Arc::new(MetricsInner::default());
        let metrics_handle = PgMetricsHandle {
            inner: Arc::clone(&metrics_inner),
        };
        let pool = PoolHolder::new(pool, runtime.clone());
        let worker = Self {
            config,
            pool,
            runtime,
            in_flight: HashMap::new(),
            next_id: 1,
            closed: Arc::new(AtomicBool::new(false)),
            metrics: metrics_inner,
            _owned_runtime: owned_runtime,
            _shard: PhantomData,
        };
        (worker, metrics_handle)
    }

    fn admit(&mut self, request: PgRequest) -> Effect<Self> {
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
            return reply::<Self>(Err(PgError::Closed));
        }

        if let Err(err) = validate_request(&request, &self.config) {
            self.metrics.invalid.fetch_add(1, Ordering::Relaxed);
            #[cfg(feature = "tracing")]
            event!(
                target: TRACE_TARGET_CALL,
                Level::WARN,
                kind = "admission_rejected",
                reason = pg_error_reason(&err),
                request_kind,
                detail = %err,
            );
            return reply::<Self>(Err(err));
        }

        if self.in_flight.len() >= self.config.max_in_flight {
            self.metrics.full.fetch_add(1, Ordering::Relaxed);
            #[cfg(feature = "tracing")]
            event!(
                target: TRACE_TARGET_CALL,
                Level::WARN,
                kind = "admission_rejected",
                reason = "Full",
                request_kind,
            );
            return reply::<Self>(Err(PgError::Full));
        }

        let id = self.next_id;
        self.next_id = self.next_id.wrapping_add(1);

        let (tx, rx) = oneshot::channel();
        let pool = self.pool.pool().clone();
        let abandoned = Arc::new(AtomicBool::new(false));
        let abandoned_for_task = Arc::clone(&abandoned);
        let metrics_for_task = Arc::clone(&self.metrics);
        let max_response_rows = self.config.max_response_rows;
        self.runtime.spawn(async move {
            let result = run_request(&pool, request, max_response_rows).await;
            tally_worker_terminal(&metrics_for_task, &result);
            if abandoned_for_task.load(Ordering::Acquire) {
                metrics_for_task
                    .late_results
                    .fetch_add(1, Ordering::Relaxed);
            }
            // Receiver may already be dropped (timeout path); send is
            // best-effort. The tally above is the source of truth.
            let _ = tx.send(result);
        });
        self.in_flight.insert(
            id,
            InFlight {
                started_at: Instant::now(),
                receiver: rx,
                abandoned,
                request_kind,
            },
        );
        let in_flight = self.in_flight.len() as u64;
        self.metrics.admitted.fetch_add(1, Ordering::Relaxed);
        self.metrics.set_in_flight(in_flight);
        self.metrics.note_in_flight(in_flight);

        #[cfg(feature = "tracing")]
        event!(
            target: TRACE_TARGET_CALL,
            Level::DEBUG,
            kind = "admitted",
            request_kind,
            in_flight,
        );

        sleep(self.config.poll_interval).reply(move |_| PgMsg::Poll(id))
    }

    fn poll(&mut self, id: u64) -> Effect<Self> {
        let Some(mut in_flight) = self.in_flight.remove(&id) else {
            return noop();
        };
        let request_kind = in_flight.request_kind;

        match in_flight.receiver.try_recv() {
            Ok(result) => {
                self.note_terminal();
                #[cfg(feature = "tracing")]
                emit_replied(&result, request_kind);
                #[cfg(not(feature = "tracing"))]
                let _ = request_kind;
                reply::<Self>(result)
            }
            Err(oneshot::error::TryRecvError::Empty) => {
                if in_flight.started_at.elapsed() >= self.config.default_timeout {
                    // Mark abandoned and drop the receiver; the
                    // spawned task continues until its SQLx future
                    // completes naturally. When it does, tally records
                    // the actual outcome and `late_results`
                    // increments. We do not abort the task — abort
                    // would cancel SQLx at the next .await point and
                    // skip the tally entirely, while doing nothing to
                    // stop Postgres from finishing the query.
                    // Postgres-side cancellation needs `CancelRequest`
                    // and is a deferred follow-up.
                    in_flight.abandoned.store(true, Ordering::Release);
                    self.metrics.timeouts.fetch_add(1, Ordering::Relaxed);
                    self.note_terminal();
                    #[cfg(feature = "tracing")]
                    event!(
                        target: TRACE_TARGET_CALL,
                        Level::WARN,
                        kind = "timeout",
                        reason = "Timeout",
                        request_kind,
                        elapsed_ms = in_flight.started_at.elapsed().as_millis() as u64,
                    );
                    return reply::<Self>(Err(PgError::Timeout));
                }
                self.in_flight.insert(id, in_flight);
                sleep(self.config.poll_interval).reply(move |_| PgMsg::Poll(id))
            }
            Err(oneshot::error::TryRecvError::Closed) => {
                self.note_terminal();
                #[cfg(feature = "tracing")]
                event!(
                    target: TRACE_TARGET_CALL,
                    Level::ERROR,
                    kind = "replied",
                    reason = "Internal",
                    request_kind,
                    detail = "spawned task ended without result",
                );
                reply::<Self>(Err(PgError::Internal(
                    "spawned task ended without result".into(),
                )))
            }
        }
    }

    fn note_terminal(&self) {
        let in_flight = self.in_flight.len() as u64;
        self.metrics.set_in_flight(in_flight);
    }
}

#[cfg(feature = "tracing")]
fn emit_replied(result: &PgResult, request_kind: &'static str) {
    match result {
        Ok(PgResponse::Executed { rows_affected }) => event!(
            target: TRACE_TARGET_CALL,
            Level::DEBUG,
            kind = "replied",
            outcome = "executed",
            request_kind,
            rows_affected = *rows_affected,
        ),
        Ok(PgResponse::Row(_)) => event!(
            target: TRACE_TARGET_CALL,
            Level::DEBUG,
            kind = "replied",
            outcome = "row",
            request_kind,
        ),
        Ok(PgResponse::NoRows) => event!(
            target: TRACE_TARGET_CALL,
            Level::DEBUG,
            kind = "replied",
            outcome = "no_rows",
            request_kind,
        ),
        Ok(PgResponse::Rows { rows, truncated }) => event!(
            target: TRACE_TARGET_CALL,
            Level::DEBUG,
            kind = "replied",
            outcome = "rows",
            request_kind,
            row_count = rows.len() as u64,
            truncated = *truncated,
        ),
        Err(err) => {
            let reason = pg_error_reason(err);
            let level = match err {
                PgError::Internal(_) => Level::ERROR,
                _ => Level::WARN,
            };
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

impl<S: Shard + Send + 'static> PgWorker<S> {
    /// Build a worker that owns its own Tokio runtime and SQLx pool.
    /// Requires `config.pool` to be `Some`. Returns
    /// [`InstallError::MissingPoolConfig`] otherwise.
    ///
    /// The owned Tokio runtime is sized small (two worker threads) and
    /// is not configurable in first form. Teams that need a larger
    /// runtime, or that already have one, should build a `PgPool` on
    /// their own runtime and use [`Self::install_with_pool`].
    pub fn install<F>(
        runtime: &ThreadedRuntime<S, F>,
        config: PgConfig,
    ) -> Result<InstalledPgBridge<S>, InstallError>
    where
        F: MailboxFactory + Send + 'static,
    {
        config.validate()?;
        let pool_cfg = config.pool.clone().ok_or(InstallError::MissingPoolConfig)?;
        let tokio_rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .worker_threads(2)
            .thread_name("tina-sqlx-bridge")
            .build()
            .map_err(|e| InstallError::Runtime(format!("{e}")))?;
        let handle = tokio_rt.handle().clone();
        let pool = handle
            .block_on(async {
                PgPoolOptions::new()
                    .max_connections(pool_cfg.max_connections)
                    .acquire_timeout(pool_cfg.acquire_timeout)
                    .connect(&pool_cfg.url)
                    .await
            })
            .map_err(|e| InstallError::Pool(format!("{e}")))?;
        let owned = OwnedRuntime(Some(tokio_rt));
        let cap = config.mailbox_capacity;
        let (worker, metrics) = Self::assemble(config, pool, handle, Some(owned));
        let closer = worker.closer();
        let address = runtime
            .register_with_capacity::<_, Infallible>(worker, cap)
            .map_err(|e| InstallError::Register(format!("{e:?}")))?;
        Ok(InstalledPgBridge {
            address,
            closer,
            metrics,
            _shard: PhantomData,
        })
    }

    /// Build a worker around a caller-supplied `PgPool` and Tokio
    /// runtime handle.
    ///
    /// **Ownership split.** The supplied `PgPool` owns its own SQLx
    /// settings — `max_connections`, `acquire_timeout`, TLS, idle
    /// timeout. The bridge does not re-apply [`PgConfig`]'s pool
    /// fields to it. `PgConfig::pool` is ignored on this path.
    ///
    /// The bridge still owns Tina-side admission, `max_in_flight`,
    /// `default_timeout` (per-attempt bridge deadline), `mailbox_capacity`,
    /// and `max_request_params`. Set the supplied pool's
    /// `acquire_timeout` to a value at least as large as your typical
    /// `default_timeout`, since a pool acquire that takes longer than
    /// the bridge per-attempt deadline lands as
    /// [`PgError::Timeout`], not [`PgError::PoolAcquireTimeout`].
    ///
    /// The caller must keep the underlying `tokio::runtime::Runtime`
    /// alive for the bridge's lifetime.
    ///
    /// **Tokio-context requirement.** SQLx 0.8 spawns pool maintenance
    /// tasks at pool construction. The `PgPool` must therefore be
    /// built *inside* an active Tokio context — for example via
    /// `tokio_handle.enter()` or `tokio_handle.block_on(...)`. The
    /// bridge re-enters the supplied handle when it drops the pool,
    /// but it cannot retroactively give a context to a pool that was
    /// constructed without one.
    pub fn install_with_pool<F>(
        runtime: &ThreadedRuntime<S, F>,
        config: PgConfig,
        pool: PgPool,
        tokio_handle: Handle,
    ) -> Result<InstalledPgBridge<S>, InstallError>
    where
        F: MailboxFactory + Send + 'static,
    {
        // Tina-only validation: `config.pool` is ignored on this path
        // because the supplied `PgPool` owns its SQLx settings.
        // Validating `config.pool` here would reject callers for
        // fields the bridge promised not to apply.
        config.validate_tina()?;
        let cap = config.mailbox_capacity;
        let (worker, metrics) = Self::assemble(config, pool, tokio_handle, None);
        let closer = worker.closer();
        let address = runtime
            .register_with_capacity::<_, Infallible>(worker, cap)
            .map_err(|e| InstallError::Register(format!("{e:?}")))?;
        Ok(InstalledPgBridge {
            address,
            closer,
            metrics,
            _shard: PhantomData,
        })
    }
}

impl<S: Shard + 'static> Isolate for PgWorker<S> {
    tina::isolate_types! {
        message: PgMsg,
        reply: PgResult,
        send: tina::Outbound<Infallible>,
        spawn: Infallible,
        call: RuntimeCall<PgMsg>,
        shard: S,
    }

    fn handle(&mut self, msg: PgMsg, _ctx: &mut Context<'_, S, Self::Reply>) -> Effect<Self> {
        match msg {
            PgMsg::Send(request) => self.admit(request),
            PgMsg::Poll(id) => self.poll(id),
        }
    }
}

/// Structural checks only. Database not touched.
fn validate_request(request: &PgRequest, config: &PgConfig) -> Result<(), PgError> {
    let (sql, params_len) = match request {
        PgRequest::Execute { sql, params } => (sql, params.len()),
        PgRequest::FetchOne { sql, params } => (sql, params.len()),
        PgRequest::FetchMany { sql, params, .. } => (sql, params.len()),
    };
    if sql.trim().is_empty() {
        return Err(PgError::InvalidRequest("empty sql".into()));
    }
    if params_len > config.max_request_params {
        return Err(PgError::InvalidRequest(format!(
            "params {params_len} exceeds cap {}",
            config.max_request_params
        )));
    }
    Ok(())
}

fn tally_worker_terminal(metrics: &MetricsInner, result: &PgResult) {
    match result {
        Ok(PgResponse::Executed { .. }) => {
            metrics.responses_executed.fetch_add(1, Ordering::Relaxed);
        }
        Ok(PgResponse::Row(_)) => {
            metrics.responses_row.fetch_add(1, Ordering::Relaxed);
            metrics.rows_returned.fetch_add(1, Ordering::Relaxed);
        }
        Ok(PgResponse::NoRows) => {
            metrics.responses_no_rows.fetch_add(1, Ordering::Relaxed);
        }
        Ok(PgResponse::Rows { rows, truncated }) => {
            metrics.responses_rows.fetch_add(1, Ordering::Relaxed);
            metrics
                .rows_returned
                .fetch_add(rows.len() as u64, Ordering::Relaxed);
            if *truncated {
                metrics.responses_truncated.fetch_add(1, Ordering::Relaxed);
            }
        }
        Err(PgError::PoolAcquireTimeout) => {
            metrics
                .pool_acquire_timeouts
                .fetch_add(1, Ordering::Relaxed);
        }
        Err(PgError::PoolClosed) => {
            metrics.pool_closed.fetch_add(1, Ordering::Relaxed);
        }
        Err(PgError::Sqlx(_)) => {
            metrics.sqlx_errors.fetch_add(1, Ordering::Relaxed);
        }
        Err(PgError::Decode(_)) => {
            metrics.decode_errors.fetch_add(1, Ordering::Relaxed);
        }
        Err(PgError::TooManyRows) => {
            metrics.too_many_rows.fetch_add(1, Ordering::Relaxed);
        }
        // Admission-class errors are tallied at admission, not here;
        // and Timeout is tallied at the bridge poll site.
        Err(_) => {}
    }
}

async fn run_request(pool: &PgPool, request: PgRequest, max_response_rows: usize) -> PgResult {
    match request {
        PgRequest::Execute { sql, params } => {
            let mut q = sqlx::query(&sql);
            for p in params {
                q = bind_param(q, p);
            }
            match q.execute(pool).await {
                Ok(r) => Ok(PgResponse::Executed {
                    rows_affected: r.rows_affected(),
                }),
                Err(e) => Err(map_sqlx_error(e)),
            }
        }
        PgRequest::FetchOne { sql, params } => {
            let mut q = sqlx::query(&sql);
            for p in params {
                q = bind_param(q, p);
            }
            // Stream at most two rows so a query that accidentally
            // returns a large result set surfaces `TooManyRows`
            // without first buffering the whole thing in bridge
            // memory.
            let mut stream = q.fetch(pool);
            let first = match stream.try_next().await {
                Ok(opt) => opt,
                Err(e) => return Err(map_sqlx_error(e)),
            };
            let row = match first {
                None => return Ok(PgResponse::NoRows),
                Some(row) => row,
            };
            match stream.try_next().await {
                Ok(None) => decode_row(&row).map(PgResponse::Row),
                Ok(Some(_)) => Err(PgError::TooManyRows),
                Err(e) => Err(map_sqlx_error(e)),
            }
        }
        PgRequest::FetchMany {
            sql,
            params,
            max_rows,
        } => {
            // Effective cap: caller's `max_rows` capped by the
            // bridge's `max_response_rows` ceiling. We pull at most
            // `cap + 1` rows so we can distinguish "exactly cap rows"
            // from "Postgres had more." Either way we never grow the
            // bridge's buffer past `cap`.
            let cap = core::cmp::min(max_rows, max_response_rows);
            let mut q = sqlx::query(&sql);
            for p in params {
                q = bind_param(q, p);
            }
            let mut stream = q.fetch(pool);
            let mut rows: Vec<PgRow> = Vec::with_capacity(cap.min(64));
            let mut truncated = false;
            loop {
                if rows.len() == cap {
                    // Pull one more to discriminate "exactly cap" vs
                    // "more on the wire"; if there's another row, we
                    // drop it on the floor and set truncated.
                    match stream.try_next().await {
                        Ok(None) => break,
                        Ok(Some(_)) => {
                            truncated = true;
                            break;
                        }
                        Err(e) => return Err(map_sqlx_error(e)),
                    }
                }
                match stream.try_next().await {
                    Ok(None) => break,
                    Ok(Some(row)) => match decode_row(&row) {
                        Ok(decoded) => rows.push(decoded),
                        Err(e) => return Err(e),
                    },
                    Err(e) => return Err(map_sqlx_error(e)),
                }
            }
            Ok(PgResponse::Rows { rows, truncated })
        }
    }
}

fn bind_param<'q>(
    q: sqlx::query::Query<'q, sqlx::Postgres, sqlx::postgres::PgArguments>,
    value: PgValue,
) -> sqlx::query::Query<'q, sqlx::Postgres, sqlx::postgres::PgArguments> {
    match value {
        // Bind NULL with a typed `Option::<i64>::None`; Postgres infers
        // the parameter type from the query, but SQLx's `Encode` needs
        // a concrete carrier. `INT8` is the safest default for first
        // form. Callers who need a typed null at a non-INT8 column
        // should bind a sentinel and `IS NULL`-check in SQL.
        PgValue::Null => q.bind(Option::<i64>::None),
        PgValue::Bool(b) => q.bind(b),
        PgValue::I64(i) => q.bind(i),
        PgValue::F64(f) => q.bind(f),
        PgValue::String(s) => q.bind(s),
        PgValue::Bytes(b) => q.bind(b),
    }
}

fn decode_row(row: &SqlxRow) -> Result<PgRow, PgError> {
    let columns: Vec<String> = row.columns().iter().map(|c| c.name().to_string()).collect();
    let mut cells = Vec::with_capacity(columns.len());
    for (i, col) in row.columns().iter().enumerate() {
        let type_name = col.type_info().name();
        let cell = decode_cell(row, i, type_name)?;
        cells.push(cell);
    }
    Ok(PgRow { columns, cells })
}

fn decode_cell(row: &SqlxRow, idx: usize, type_name: &str) -> Result<PgValue, PgError> {
    macro_rules! try_get {
        ($ty:ty) => {
            match row.try_get::<Option<$ty>, _>(idx) {
                Ok(Some(v)) => Ok(v),
                Ok(None) => return Ok(PgValue::Null),
                Err(e) => Err(PgError::Decode(format!("column {idx} ({type_name}): {e}"))),
            }
        };
    }
    Ok(match type_name {
        "BOOL" => PgValue::Bool(try_get!(bool)?),
        "INT2" => PgValue::I64(i64::from(try_get!(i16)?)),
        "INT4" => PgValue::I64(i64::from(try_get!(i32)?)),
        "INT8" => PgValue::I64(try_get!(i64)?),
        "FLOAT4" => PgValue::F64(f64::from(try_get!(f32)?)),
        "FLOAT8" => PgValue::F64(try_get!(f64)?),
        "TEXT" | "VARCHAR" | "CHAR" | "BPCHAR" | "NAME" | "CITEXT" => {
            PgValue::String(try_get!(String)?)
        }
        "BYTEA" => PgValue::Bytes(try_get!(Vec<u8>)?),
        other => {
            return Err(PgError::Decode(format!(
                "column {idx}: unsupported Postgres type {other}"
            )));
        }
    })
}

fn map_sqlx_error(err: sqlx::Error) -> PgError {
    match err {
        sqlx::Error::PoolTimedOut => PgError::PoolAcquireTimeout,
        sqlx::Error::PoolClosed => PgError::PoolClosed,
        other => PgError::Sqlx(format!("{other}")),
    }
}
