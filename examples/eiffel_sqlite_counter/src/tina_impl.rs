//! Tina side. Driver calls into a `tina-sqlite-bridge` worker that
//! owns the single `rusqlite::Connection` on a blocking thread. Each
//! SQL op goes through `tina_runtime::call(...)`; the shard thread
//! never runs SQLite. Compare to `tokio_impl.rs` (`spawn_blocking` +
//! `Arc<Mutex<Connection>>`): same blocking-thread shape, but the
//! Tina side names every pressure cap and surfaces typed failures.

use std::convert::Infallible;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use rusqlite::{Connection, params};
use tina::prelude::*;
use tina_runtime::{CallOutcome, DefaultThreadedMailboxFactory, RuntimeCall, ThreadedRuntime};
use tina_sqlite_bridge::{
    SqliteAddress, SqliteCloser, SqliteConfig, SqliteExecutedOutcome, SqliteRequest,
    SqliteRowsOutcome, SqliteWorker, execute_call, query_call,
};

use crate::{INCREMENTS, Report};

#[derive(Debug)]
enum DriverMsg {
    Step,
    Stepped(SqliteExecutedOutcome),
    Finalize,
    Finalized(SqliteRowsOutcome),
}

struct Driver {
    db: SqliteAddress,
    remaining: u32,
    timeout: Duration,
    report: Report,
    closer: SqliteCloser,
}

impl Isolate for Driver {
    tina::isolate_types! {
        message: DriverMsg,
        reply: Report,
        send: tina::Outbound<Infallible>,
        spawn: Infallible,
        call: RuntimeCall<DriverMsg>,
        shard: SingleShard,
    }

    fn handle(
        &mut self,
        msg: DriverMsg,
        _ctx: &mut Context<'_, SingleShard, Self::Reply>,
    ) -> Effect<Self> {
        match msg {
            DriverMsg::Step => execute_call(
                self.db,
                SqliteRequest::execute("UPDATE counter SET value = value + 1 WHERE id = 0"),
                self.timeout,
            )
            .reply(DriverMsg::Stepped),
            DriverMsg::Stepped(outcome) => match outcome {
                CallOutcome::Replied(Ok(1)) => {
                    self.remaining -= 1;
                    // Self-message via zero sleep continuation.
                    let next = if self.remaining == 0 {
                        DriverMsg::Finalize
                    } else {
                        DriverMsg::Step
                    };
                    tina_runtime::sleep(Duration::from_millis(0)).reply(move |_| next)
                }
                other => {
                    eprintln!("eiffel_sqlite_counter (tina): unexpected step outcome {other:?}");
                    self.closer.close();
                    stop_with(self.report)
                }
            },
            DriverMsg::Finalize => query_call(
                self.db,
                SqliteRequest::query_rows("SELECT value FROM counter WHERE id = 0", 1),
                self.timeout,
            )
            .reply(DriverMsg::Finalized),
            DriverMsg::Finalized(outcome) => {
                match outcome {
                    CallOutcome::Replied(Ok(rows)) => {
                        if let Some(row) = rows.rows.into_iter().next() {
                            if let Some(v) = row.first().and_then(|c| c.as_i64()) {
                                self.report.final_value = v as u64;
                                self.report.exit_clean = true;
                            }
                        }
                    }
                    other => {
                        eprintln!(
                            "eiffel_sqlite_counter (tina): unexpected finalize outcome {other:?}"
                        );
                    }
                }
                self.closer.close();
                stop_with(self.report)
            }
        }
    }
}

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

    let driver = Driver {
        db: bridge.address,
        remaining: INCREMENTS,
        timeout: Duration::from_secs(5),
        report: Report::default(),
        closer: bridge.closer.clone(),
    };
    let driver_addr = runtime
        .register_with_capacity::<_, Infallible>(driver, 16)
        .map_err(|e| anyhow::anyhow!("register driver: {e:?}"))?;

    let waiter = runtime
        .observe_result::<Report, _, _>(driver_addr)
        .map_err(|e| anyhow::anyhow!("observe_result: {e:?}"))?;

    runtime
        .try_send(driver_addr, DriverMsg::Step)
        .map_err(|e| anyhow::anyhow!("try_send Step: {e:?}"))?;

    let report = waiter
        .wait(Duration::from_secs(30))
        .map_err(|e| anyhow::anyhow!("driver did not finish: {e:?}"))?;

    let snap = bridge.metrics.snapshot();
    eprintln!(
        "eiffel_sqlite_counter (tina) bridge metrics: \
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
