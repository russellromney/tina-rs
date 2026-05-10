//! Tina side. The host thread calls into a `tina-sqlite-bridge`
//! worker that owns the single `rusqlite::Connection` on a blocking
//! thread. The shard thread never runs SQLite. Compare to
//! `tokio_impl.rs` (`spawn_blocking` + `Arc<Mutex<Connection>>`): same
//! blocking-thread shape, but the Tina side names every pressure cap
//! and surfaces typed failures.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use rusqlite::{Connection, params};
use tina::prelude::*;
use tina_runtime::{CallOutcome, DefaultThreadedMailboxFactory, ThreadedRuntime};
use tina_sqlite_bridge::{
    SqliteConfig, SqliteMsg, SqliteRequest, SqliteResponse, SqliteWorker,
};

use crate::{INCREMENTS, Report};

const SQL_TIMEOUT: Duration = Duration::from_secs(5);

pub fn run() -> anyhow::Result<Report> {
    let dir = tempfile::tempdir()?;
    let path: PathBuf = dir.path().join("counter-tina.sqlite");
    seed_database(&path)?;

    let runtime = Arc::new(ThreadedRuntime::new(
        SingleShard,
        DefaultThreadedMailboxFactory,
    ));

    let cfg = SqliteConfig::path(&path)
        .with_default_timeout(Duration::from_secs(5))
        .with_busy_timeout(Duration::from_secs(2))
        .with_pragma("journal_mode = WAL")
        .with_poll_interval(Duration::from_millis(1));
    let bridge = SqliteWorker::<SingleShard>::install(&runtime, cfg)
        .map_err(|e| anyhow::anyhow!("install sqlite bridge: {e}"))?;

    let mut report = Report::default();
    let run_result = run_counter_script(&runtime, bridge.address);
    match run_result {
        Ok(final_value) => {
            report.final_value = final_value;
            report.exit_clean = true;
        }
        Err(error) => {
            eprintln!("specimen_sqlite_counter (tina): {error:#}");
        }
    }
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

    if let Ok(rt) = Arc::try_unwrap(runtime) {
        let _ = rt.shutdown();
    }
    drop(dir);
    Ok(report)
}

fn run_counter_script(
    runtime: &ThreadedRuntime<SingleShard, DefaultThreadedMailboxFactory>,
    db: tina_sqlite_bridge::SqliteAddress,
) -> anyhow::Result<u64> {
    for _ in 0..INCREMENTS {
        let outcome = runtime.call_blocking(
            db,
            SqliteMsg::Request(SqliteRequest::execute(
                "UPDATE counter SET value = value + 1 WHERE id = 0",
            )),
            SQL_TIMEOUT,
        )?;
        match outcome {
            CallOutcome::Replied(Ok(SqliteResponse::Executed { rows_changed: 1 })) => {}
            other => anyhow::bail!("unexpected step outcome {other:?}"),
        }
    }

    let outcome = runtime.call_blocking(
        db,
        SqliteMsg::Request(SqliteRequest::query_rows(
            "SELECT value FROM counter WHERE id = 0",
            1,
        )),
        SQL_TIMEOUT,
    )?;
    match outcome {
        CallOutcome::Replied(Ok(SqliteResponse::Rows { rows, .. })) => rows
            .first()
            .and_then(|row| row.first())
            .and_then(|cell| cell.as_i64())
            .and_then(|v| u64::try_from(v).ok())
            .ok_or_else(|| anyhow::anyhow!("final query did not return one unsigned integer")),
        other => anyhow::bail!("unexpected finalize outcome {other:?}"),
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
