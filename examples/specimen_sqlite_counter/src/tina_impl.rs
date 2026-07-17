//! Tina side. A root isolate owns the counter script: it issues every
//! update and the final query through `tina-sqlite-bridge`, accumulates
//! query/update metrics privately, then `stop_with`s the report. The
//! host claims `observe_result` before start. The shard thread never
//! runs SQLite.
//!
//! Point-in-time inspection of the database uses the bridge's existing
//! typed query request (`query_call` / host `call_blocking_typed`).
//! There is no result mutex, condvar, atomic completion flag, or
//! sleep-poll loop for application results.

use std::convert::Infallible;
use std::path::PathBuf;
use std::time::Duration;

use rusqlite::{Connection, params};
use tina::prelude::*;
use tina_runtime::{CallOutcome, DefaultThreadedMailboxFactory, LocalSystem};
use tina_sqlite_bridge::{
    SqliteAddress, SqliteConfig, SqliteExecutedOutcome, SqliteRowsOutcome, SqliteWorker,
    execute_call, query_call,
};

use crate::{INCREMENTS, Report};

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
    report: Report,
}

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
            return query_call(self.db, QUERY_SQL, vec![], 1, SQL_TIMEOUT)
                .then(CounterMsg::QueryDone);
        }
        execute_call(self.db, UPDATE_SQL, vec![], SQL_TIMEOUT).then(CounterMsg::UpdateDone)
    }

    fn on_update(&mut self, outcome: SqliteExecutedOutcome) -> Effect<Self> {
        match outcome {
            CallOutcome::Replied(Ok(rows_changed)) if rows_changed == 1 => {
                self.report.updates_ok += 1;
                self.report.rows_changed += rows_changed;
                self.remaining_updates = self.remaining_updates.saturating_sub(1);
                self.next_update()
            }
            other => self.fail(format!("unexpected update outcome {other:?}")),
        }
    }

    fn on_query(&mut self, outcome: SqliteRowsOutcome) -> Effect<Self> {
        match outcome {
            CallOutcome::Replied(Ok(rows)) => {
                let value = rows
                    .row(0)
                    .and_then(|row| row.col(0))
                    .and_then(|cell| cell.as_i64())
                    .and_then(|v| u64::try_from(v).ok());
                match value {
                    Some(final_value) => {
                        self.report.queries_ok += 1;
                        self.report.final_value = final_value;
                        self.report.exit_clean = true;
                        stop_with(self.report)
                    }
                    None => self.fail("final query did not return one unsigned integer".into()),
                }
            }
            other => self.fail(format!("unexpected finalize outcome {other:?}")),
        }
    }

    fn fail(&mut self, message: String) -> Effect<Self> {
        eprintln!("specimen_sqlite_counter (tina): {message}");
        self.report.exit_clean = false;
        stop_with(self.report)
    }
}

pub fn run() -> anyhow::Result<Report> {
    let dir = tempfile::tempdir()?;
    let path: PathBuf = dir.path().join("counter-tina.sqlite");
    seed_database(&path)?;

    let app = LocalSystem::single_shard(SingleShard, DefaultThreadedMailboxFactory).try_build()?;
    let report = app.run_to_shutdown_reported(Duration::from_secs(10), |app| {
        run_application(app, &path)
    })?;
    drop(dir);
    Ok(report)
}

fn run_application(
    app: &LocalSystem<SingleShard, DefaultThreadedMailboxFactory>,
    path: &std::path::Path,
) -> anyhow::Result<Report> {
    let cfg = SqliteConfig::path(path)
        .with_default_timeout(Duration::from_secs(5))
        .with_busy_timeout(Duration::from_secs(2))
        .with_pragma("journal_mode = WAL")
        .with_poll_interval(Duration::from_millis(1));
    let bridge = SqliteWorker::<SingleShard>::install_local(app, cfg)
        .map_err(|e| anyhow::anyhow!("install sqlite bridge: {e}"))?;

    let counter = Counter {
        db: bridge.address,
        remaining_updates: INCREMENTS,
        report: Report::default(),
    };
    let counter_addr = app
        .register_root::<_, Infallible>(counter, 8)
        .map_err(|e| anyhow::anyhow!("register counter: {e:?}"))?;

    let waiter = app
        .observe_result::<Report, _, _>(counter_addr)
        .map_err(|e| anyhow::anyhow!("observe_result: {e:?}"))?;

    app.try_send(counter_addr, CounterMsg::Begin)
        .map_err(|e| anyhow::anyhow!("kick counter: {e:?}"))?;

    let report = waiter
        .wait(Duration::from_secs(10))
        .map_err(|e| anyhow::anyhow!("counter did not finish: {e:?}"))?;

    bridge.closer.close();

    let snap = bridge.metrics.snapshot();
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

    Ok(report)
}

/// Point-in-time inspection helper: read the current counter value
/// through the bridge's existing typed query request. Used by demos
/// and tests that need a live SELECT without inventing a result waiter.
pub fn query_counter_value(
    app: &LocalSystem<SingleShard, DefaultThreadedMailboxFactory>,
    db: SqliteAddress,
) -> anyhow::Result<u64> {
    use tina_sqlite_bridge::{SqliteMsg, SqliteRequest, SqliteResponse};

    let outcome = app
        .call_blocking(
            db.address(),
            SqliteMsg::Request(SqliteRequest::query_rows(QUERY_SQL, 1)),
            SQL_TIMEOUT,
        )
        .map_err(|e| anyhow::anyhow!("point-in-time query: {e:?}"))?;
    match outcome {
        CallOutcome::Replied(Ok(SqliteResponse::Rows { rows, .. })) => rows
            .first()
            .and_then(|row| row.first())
            .and_then(|cell| cell.as_i64())
            .and_then(|v| u64::try_from(v).ok())
            .ok_or_else(|| anyhow::anyhow!("point-in-time query returned no integer")),
        other => anyhow::bail!("unexpected point-in-time outcome {other:?}"),
    }
}

fn seed_database(path: &std::path::Path) -> anyhow::Result<()> {
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
