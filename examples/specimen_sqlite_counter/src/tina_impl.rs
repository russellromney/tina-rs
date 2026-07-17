//! Tina side. A root isolate owns the counter script: it issues every
//! update and the final query through `tina-sqlite-bridge`, accumulates
//! query/update metrics privately, then `stop_with`s the report. The
//! host claims `observe_result` before start. The shard thread never
//! runs SQLite.
//!
//! Point-in-time inspection of the database uses the bridge's existing
//! typed query request (`query_call` / host `query_blocking`).
//! There is no result mutex, condvar, atomic completion flag, or
//! sleep-poll loop for application results.

use std::convert::Infallible;
use std::path::PathBuf;
use std::time::Duration;

use rusqlite::{Connection, params};
use tina::prelude::*;
use tina_runtime::{
    CallOutcome, DefaultThreadedMailboxFactory, LocalSystem, ResultWaitError, RunToShutdownError,
    StartupError, ThreadedRuntimeError, ThreadedTrySendError,
};
use tina_sqlite_bridge::{
    InstallError, SqliteAddress, SqliteCloseOutcome, SqliteConfig, SqliteError,
    SqliteExecutedOutcome, SqliteRowsOutcome, SqliteValue, SqliteWorker, execute_call,
    query_blocking, query_call,
};

use crate::{
    BridgeDeliveryFailure, CounterFailure, CounterProtocolFailure, INCREMENTS, Report,
    SqliteValueKind,
};

const SQL_TIMEOUT: Duration = Duration::from_secs(5);
const UPDATE_SQL: &str = "UPDATE counter SET value = value + 1 WHERE id = 0";
const QUERY_SQL: &str = "SELECT value FROM counter WHERE id = 0";

#[derive(Debug)]
enum CounterMsg {
    Begin,
    UpdateDone(SqliteExecutedOutcome),
    QueryDone(SqliteRowsOutcome),
}

struct Counter {
    db: SqliteAddress,
    remaining_updates: u32,
    update_sql: &'static str,
    query_sql: &'static str,
    report: Report,
}

type CounterTerminal = Result<Report, CounterFailure>;

#[tina_runtime::isolate(message = CounterMsg)]
impl Counter {
    fn handle(
        &mut self,
        msg: CounterMsg,
        _ctx: &mut Context<'_, SingleShard, Self::Reply>,
    ) -> Effect<Self> {
        match msg {
            CounterMsg::Begin => self.next_update(),
            CounterMsg::UpdateDone(outcome) => self.on_update(outcome),
            CounterMsg::QueryDone(outcome) => self.on_query(outcome),
        }
    }
}

impl Counter {
    fn next_update(&mut self) -> Effect<Self> {
        if self.remaining_updates == 0 {
            return query_call(self.db, self.query_sql, vec![], 1, SQL_TIMEOUT)
                .then(CounterMsg::QueryDone);
        }
        execute_call(self.db, self.update_sql, vec![], SQL_TIMEOUT).then(CounterMsg::UpdateDone)
    }

    fn on_update(&mut self, outcome: SqliteExecutedOutcome) -> Effect<Self> {
        match outcome {
            CallOutcome::Replied(Ok(rows_changed)) if rows_changed == 1 => {
                self.report.updates_ok += 1;
                self.report.rows_changed += rows_changed;
                self.remaining_updates = self.remaining_updates.saturating_sub(1);
                self.next_update()
            }
            CallOutcome::Replied(Ok(rows_changed)) => self.fail(CounterFailure::Protocol(
                CounterProtocolFailure::UnexpectedRowsChanged {
                    expected: 1,
                    actual: rows_changed,
                },
            )),
            other => self.fail(call_failure(other)),
        }
    }

    fn on_query(&mut self, outcome: SqliteRowsOutcome) -> Effect<Self> {
        match outcome {
            CallOutcome::Replied(Ok(rows)) => match decode_final_value(&rows) {
                Ok(final_value) => {
                    self.report.queries_ok += 1;
                    self.report.final_value = final_value;
                    self.report.exit_clean = true;
                    stop_with(Ok::<_, CounterFailure>(self.report))
                }
                Err(error) => self.fail(CounterFailure::Protocol(error)),
            },
            other => self.fail(call_failure(other)),
        }
    }

    fn fail(&mut self, error: CounterFailure) -> Effect<Self> {
        stop_with(Err::<Report, _>(error))
    }
}

/// Exact Tina runner failure. Workload and shutdown failures remain distinct
/// through [`RunToShutdownError`].
#[derive(Debug)]
pub enum TinaRunError {
    TempDir(std::io::Error),
    Seed(rusqlite::Error),
    Startup(StartupError),
    Run(Box<RunToShutdownError<TinaWorkloadError>>),
}

/// Exact failure while the local system is live.
#[derive(Debug)]
pub enum TinaWorkloadError {
    Install(InstallError),
    Register(ThreadedRuntimeError),
    Observer(ResultWaitError),
    Start(ThreadedTrySendError),
    Counter(CounterFailure),
    SettlementTimeout(tina_sqlite_bridge::SqliteMetrics),
}

impl std::fmt::Display for TinaRunError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::TempDir(error) => write!(f, "temporary database directory: {error}"),
            Self::Seed(error) => write!(f, "seed database: {error}"),
            Self::Startup(error) => write!(f, "start local system: {error}"),
            Self::Run(error) => error.fmt(f),
        }
    }
}

impl std::error::Error for TinaRunError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::TempDir(error) => Some(error),
            Self::Seed(error) => Some(error),
            Self::Startup(error) => Some(error),
            Self::Run(error) => Some(error),
        }
    }
}

impl std::fmt::Display for TinaWorkloadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Install(error) => write!(f, "install sqlite bridge: {error}"),
            Self::Register(error) => write!(f, "register counter: {error}"),
            Self::Observer(error) => write!(f, "observe counter result: {error:?}"),
            Self::Start(error) => write!(f, "start counter: {error}"),
            Self::Counter(error) => error.fmt(f),
            Self::SettlementTimeout(metrics) => {
                write!(
                    f,
                    "sqlite worker did not settle before deadline: {metrics:?}"
                )
            }
        }
    }
}

/// Adversarial input used by [`run_correction`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CorrectionScenario {
    /// SQLite rejects malformed SQL.
    MalformedSql,
    /// The bridge has been closed before the counter sends its request.
    ClosedBridge,
    /// The bridge deadline expires while SQLite continues synchronously.
    WorkerTimeout,
    /// A successful query returns a value with the wrong SQLite type.
    ProtocolValueType,
    /// The host claims the terminal result with the wrong Rust type.
    ObserverTypeMismatch,
}

/// Exact terminal observed by [`run_correction`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CorrectionOutcome {
    /// The counter completed successfully.
    Completed(Report),
    /// The counter stopped with its typed terminal failure.
    Counter(CounterFailure),
    /// Result observation itself failed.
    Observer(ResultWaitError),
}

/// Correction evidence captured only after the worker has settled and the bridge is closed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CorrectionReport {
    pub outcome: CorrectionOutcome,
    pub metrics: tina_sqlite_bridge::SqliteMetrics,
    pub bridge_closed: bool,
}

/// Run one deterministic non-happy scenario through the production counter and bridge.
pub fn run_correction(scenario: CorrectionScenario) -> Result<CorrectionReport, TinaRunError> {
    let app = LocalSystem::single_shard(SingleShard, DefaultThreadedMailboxFactory)
        .try_build()
        .map_err(TinaRunError::Startup)?;
    app.run_to_shutdown(Duration::from_secs(10), |app| {
        run_correction_application(app, scenario)
    })
    .map_err(|error| TinaRunError::Run(Box::new(error)))
}

fn run_correction_application(
    app: &LocalSystem<SingleShard, DefaultThreadedMailboxFactory>,
    scenario: CorrectionScenario,
) -> Result<CorrectionReport, TinaWorkloadError> {
    const HEAVY_QUERY: &str = "WITH RECURSIVE seq(x) AS (\
        SELECT 1 UNION ALL SELECT x + 1 FROM seq WHERE x < 1000000\
    ) SELECT sum(x) FROM seq";

    let timeout = if scenario == CorrectionScenario::WorkerTimeout {
        Duration::from_millis(1)
    } else {
        Duration::from_secs(5)
    };
    let cfg = SqliteConfig::memory()
        .with_default_timeout(timeout)
        .with_poll_interval(Duration::from_millis(1));
    let bridge =
        SqliteWorker::<SingleShard>::install_local(app, cfg).map_err(TinaWorkloadError::Install)?;

    let query_sql = match scenario {
        CorrectionScenario::MalformedSql => "SELEC broken",
        CorrectionScenario::WorkerTimeout => HEAVY_QUERY,
        CorrectionScenario::ProtocolValueType => "SELECT 'not-an-integer'",
        CorrectionScenario::ClosedBridge | CorrectionScenario::ObserverTypeMismatch => "SELECT 0",
    };
    let counter = Counter {
        db: bridge.address,
        remaining_updates: 0,
        update_sql: UPDATE_SQL,
        query_sql,
        report: Report::default(),
    };
    let counter_addr = app
        .register_root::<_, Infallible>(counter, 8)
        .map_err(TinaWorkloadError::Register)?;

    if scenario == CorrectionScenario::ClosedBridge {
        bridge.closer.close();
    }

    let outcome = if scenario == CorrectionScenario::ObserverTypeMismatch {
        let waiter = app
            .observe_result::<String, _, _>(counter_addr)
            .map_err(TinaWorkloadError::Observer)?;
        app.try_send(counter_addr, CounterMsg::Begin)
            .map_err(TinaWorkloadError::Start)?;
        match waiter.wait(Duration::from_secs(5)) {
            Ok(_) => unreachable!("counter terminal cannot be a String"),
            Err(error) => CorrectionOutcome::Observer(error),
        }
    } else {
        let waiter = app
            .observe_result::<CounterTerminal, _, _>(counter_addr)
            .map_err(TinaWorkloadError::Observer)?;
        app.try_send(counter_addr, CounterMsg::Begin)
            .map_err(TinaWorkloadError::Start)?;
        match waiter
            .wait(Duration::from_secs(5))
            .map_err(TinaWorkloadError::Observer)?
        {
            Ok(report) => CorrectionOutcome::Completed(report),
            Err(error) => CorrectionOutcome::Counter(error),
        }
    };

    let metrics = match bridge.close_and_wait(Duration::from_secs(5)) {
        SqliteCloseOutcome::Drained(metrics) => metrics,
        SqliteCloseOutcome::TimedOut(metrics) => {
            return Err(TinaWorkloadError::SettlementTimeout(metrics));
        }
    };
    Ok(CorrectionReport {
        outcome,
        metrics,
        bridge_closed: bridge.closer.is_closed(),
    })
}

impl std::error::Error for TinaWorkloadError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Install(error) => Some(error),
            Self::Register(error) => Some(error),
            Self::Start(error) => Some(error),
            Self::Counter(error) => Some(error),
            Self::Observer(_) | Self::SettlementTimeout(_) => None,
        }
    }
}

pub fn run() -> Result<Report, TinaRunError> {
    let dir = tempfile::tempdir().map_err(TinaRunError::TempDir)?;
    let path: PathBuf = dir.path().join("counter-tina.sqlite");
    seed_database(&path).map_err(TinaRunError::Seed)?;

    let app = LocalSystem::single_shard(SingleShard, DefaultThreadedMailboxFactory)
        .try_build()
        .map_err(TinaRunError::Startup)?;
    let report = app
        .run_to_shutdown(Duration::from_secs(10), |app| run_application(app, &path))
        .map_err(|error| TinaRunError::Run(Box::new(error)))?;
    drop(dir);
    Ok(report)
}

fn run_application(
    app: &LocalSystem<SingleShard, DefaultThreadedMailboxFactory>,
    path: &std::path::Path,
) -> Result<Report, TinaWorkloadError> {
    let cfg = SqliteConfig::path(path)
        .with_default_timeout(Duration::from_secs(5))
        .with_busy_timeout(Duration::from_secs(2))
        .with_pragma("journal_mode = WAL")
        .with_poll_interval(Duration::from_millis(1));
    let bridge =
        SqliteWorker::<SingleShard>::install_local(app, cfg).map_err(TinaWorkloadError::Install)?;

    let counter = Counter {
        db: bridge.address,
        remaining_updates: INCREMENTS,
        update_sql: UPDATE_SQL,
        query_sql: QUERY_SQL,
        report: Report::default(),
    };
    let counter_addr = app
        .register_root::<_, Infallible>(counter, 8)
        .map_err(TinaWorkloadError::Register)?;

    let waiter = app
        .observe_result::<CounterTerminal, _, _>(counter_addr)
        .map_err(TinaWorkloadError::Observer)?;

    app.try_send(counter_addr, CounterMsg::Begin)
        .map_err(TinaWorkloadError::Start)?;

    let terminal = waiter
        .wait(Duration::from_secs(10))
        .map_err(TinaWorkloadError::Observer)?;

    let snap = match bridge.close_and_wait(Duration::from_secs(5)) {
        SqliteCloseOutcome::Drained(metrics) => metrics,
        SqliteCloseOutcome::TimedOut(metrics) => {
            return Err(TinaWorkloadError::SettlementTimeout(metrics));
        }
    };
    eprintln!(
        "specimen_sqlite_counter (tina) bridge metrics: \
         admitted={} executed={} rows={} timeouts={} late={} full={} closed={} \
         high_water={}",
        snap.admitted,
        snap.worker_executed,
        snap.worker_rows,
        snap.timeouts,
        snap.late_results,
        snap.full,
        snap.closed,
        snap.in_flight_high_water,
    );

    terminal.map_err(TinaWorkloadError::Counter)
}

/// Point-in-time inspection helper: read the current counter value
/// through the bridge's existing typed query request. Used by demos
/// and tests that need a live SELECT without inventing a result waiter.
pub fn query_counter_value(
    app: &LocalSystem<SingleShard, DefaultThreadedMailboxFactory>,
    db: SqliteAddress,
) -> Result<u64, QueryCounterError> {
    let outcome = query_blocking(app, db, QUERY_SQL, vec![], 1, SQL_TIMEOUT)
        .map_err(QueryCounterError::Host)?;
    match outcome {
        CallOutcome::Replied(Ok(rows)) => {
            decode_final_value(&rows).map_err(QueryCounterError::Protocol)
        }
        CallOutcome::Replied(Err(error)) => Err(QueryCounterError::Sqlite(error)),
        CallOutcome::Full => Err(QueryCounterError::Delivery(BridgeDeliveryFailure::Full)),
        CallOutcome::Closed => Err(QueryCounterError::Delivery(BridgeDeliveryFailure::Closed)),
        CallOutcome::Timeout => Err(QueryCounterError::Delivery(BridgeDeliveryFailure::Timeout)),
        CallOutcome::Rejected(reason) => Err(QueryCounterError::Delivery(
            BridgeDeliveryFailure::Rejected(reason),
        )),
    }
}

/// Exact point-in-time query failure.
#[derive(Debug)]
pub enum QueryCounterError {
    Host(ThreadedRuntimeError),
    Sqlite(SqliteError),
    Delivery(BridgeDeliveryFailure),
    Protocol(CounterProtocolFailure),
}

impl std::fmt::Display for QueryCounterError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{self:?}")
    }
}

impl std::error::Error for QueryCounterError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Host(error) => Some(error),
            Self::Sqlite(error) => Some(error),
            Self::Delivery(_) | Self::Protocol(_) => None,
        }
    }
}

fn call_failure<T>(outcome: CallOutcome<Result<T, SqliteError>>) -> CounterFailure {
    match outcome {
        CallOutcome::Replied(Err(error)) => CounterFailure::Sqlite(error),
        CallOutcome::Full => CounterFailure::Delivery(BridgeDeliveryFailure::Full),
        CallOutcome::Closed => CounterFailure::Delivery(BridgeDeliveryFailure::Closed),
        CallOutcome::Timeout => CounterFailure::Delivery(BridgeDeliveryFailure::Timeout),
        CallOutcome::Rejected(reason) => {
            CounterFailure::Delivery(BridgeDeliveryFailure::Rejected(reason))
        }
        CallOutcome::Replied(Ok(_)) => unreachable!("successful outcomes are handled by caller"),
    }
}

fn decode_final_value(
    rows: &tina_sqlite_bridge::SqliteRows,
) -> Result<u64, CounterProtocolFailure> {
    if rows.len() != 1 {
        return Err(CounterProtocolFailure::UnexpectedRowCount { actual: rows.len() });
    }
    let row = rows.row(0).expect("row count checked");
    if row.len() != 1 {
        return Err(CounterProtocolFailure::UnexpectedColumnCount { actual: row.len() });
    }
    match row.col(0).expect("column count checked") {
        SqliteValue::Integer(value) if *value >= 0 => Ok(*value as u64),
        SqliteValue::Integer(value) => {
            Err(CounterProtocolFailure::NegativeFinalValue { actual: *value })
        }
        other => Err(CounterProtocolFailure::UnexpectedValueKind {
            actual: sqlite_value_kind(other),
        }),
    }
}

fn sqlite_value_kind(value: &SqliteValue) -> SqliteValueKind {
    match value {
        SqliteValue::Null => SqliteValueKind::Null,
        SqliteValue::Integer(_) => SqliteValueKind::Integer,
        SqliteValue::Real(_) => SqliteValueKind::Real,
        SqliteValue::Text(_) => SqliteValueKind::Text,
        SqliteValue::Blob(_) => SqliteValueKind::Blob,
    }
}

fn seed_database(path: &std::path::Path) -> rusqlite::Result<()> {
    let conn = Connection::open(path)?;
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS counter (id INTEGER PRIMARY KEY, value INTEGER NOT NULL);",
    )?;
    conn.execute(
        "INSERT OR IGNORE INTO counter (id, value) VALUES (0, 0)",
        params![],
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rows(cells: Vec<Vec<SqliteValue>>) -> tina_sqlite_bridge::SqliteRows {
        let columns = cells
            .first()
            .map(|row| (0..row.len()).map(|index| format!("c{index}")).collect())
            .unwrap_or_default();
        tina_sqlite_bridge::SqliteRows {
            columns,
            rows: cells,
        }
    }

    #[test]
    fn final_value_protocol_is_exhaustive_and_never_coerces() {
        assert_eq!(
            decode_final_value(&rows(vec![])),
            Err(CounterProtocolFailure::UnexpectedRowCount { actual: 0 })
        );
        assert_eq!(
            decode_final_value(&rows(vec![vec![], vec![]])),
            Err(CounterProtocolFailure::UnexpectedRowCount { actual: 2 })
        );
        assert_eq!(
            decode_final_value(&rows(vec![vec![
                SqliteValue::Integer(1),
                SqliteValue::Integer(2),
            ]])),
            Err(CounterProtocolFailure::UnexpectedColumnCount { actual: 2 })
        );
        assert_eq!(
            decode_final_value(&rows(vec![vec![SqliteValue::Integer(-1)]])),
            Err(CounterProtocolFailure::NegativeFinalValue { actual: -1 })
        );
        assert_eq!(
            decode_final_value(&rows(vec![vec![SqliteValue::Real(1.0)]])),
            Err(CounterProtocolFailure::UnexpectedValueKind {
                actual: SqliteValueKind::Real,
            })
        );
        assert_eq!(
            decode_final_value(&rows(vec![vec![SqliteValue::Integer(7)]])),
            Ok(7)
        );
    }
}
