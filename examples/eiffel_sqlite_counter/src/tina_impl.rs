//! Tina side. A single `SqliteWorker` isolate owns a
//! `rusqlite::Connection` and runs each query synchronously inside
//! its handler. There is no native database bridge yet (see
//! ROADMAP phase 055 "native database first form" and Round 2
//! finding 8); this specimen documents the shape today and what is
//! missing.
//!
//! Because rusqlite calls run inline in `handle`, the isolate's
//! shard thread is blocked for the duration of each query. SQLite
//! operations are fast in practice (microseconds for a single
//! `UPDATE`), so for a single-shard adoption-grade specimen this is
//! acceptable. **It is not the right shape for a multi-shard
//! production database client**, where blocking the shard thread
//! prevents *every* other isolate on that shard from running. The
//! README discussion calls this out.

use std::convert::Infallible;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use rusqlite::{Connection, params};
use tina::prelude::*;
use tina_runtime::{DefaultThreadedMailboxFactory, ThreadedRuntime};

use crate::{INCREMENTS, Report};

#[derive(Debug)]
enum SqliteMsg {
    /// One `UPDATE counter SET value = value + 1` query.
    Increment,
    /// Read the final value, populate the `Report`, and `stop_with`.
    Finalize,
}

struct SqliteWorker {
    conn: Connection,
    /// Local count, kept so the host can spot drift between the
    /// isolate's view and the database's view at finalize time.
    local_count: u64,
    report: Report,
}

#[tina_runtime::isolate(message = SqliteMsg)]
impl SqliteWorker {
    fn handle(&mut self, msg: SqliteMsg, _ctx: &mut Context<'_, SingleShard>) -> Effect<Self> {
        match msg {
            SqliteMsg::Increment => {
                // Synchronous SQL inside the handler. The shard
                // thread blocks for the duration. See README.
                self.conn
                    .execute("UPDATE counter SET value = value + 1 WHERE id = 0", [])
                    .expect("update");
                self.local_count += 1;
                noop()
            }
            SqliteMsg::Finalize => {
                let value: i64 = self
                    .conn
                    .query_row("SELECT value FROM counter WHERE id = 0", [], |row| row.get(0))
                    .expect("read final value");
                self.report.final_value = value as u64;
                self.report.exit_clean = true;
                debug_assert_eq!(
                    self.report.final_value, self.local_count,
                    "isolate counter ({}) and database ({}) should agree",
                    self.local_count, self.report.final_value,
                );
                stop_with(self.report)
            }
        }
    }
}

pub fn run() -> anyhow::Result<Report> {
    let dir = tempfile::tempdir()?;
    let path: PathBuf = dir.path().join("counter-tina.sqlite");
    let conn = open_and_init(&path)?;

    let runtime = Arc::new(ThreadedRuntime::new(
        SingleShard,
        DefaultThreadedMailboxFactory,
    ));

    let worker = SqliteWorker {
        conn,
        local_count: 0,
        report: Report::default(),
    };
    let addr = runtime
        .register_with_capacity::<_, Infallible>(worker, 128)
        .map_err(|e| anyhow::anyhow!("register sqlite worker: {e:?}"))?;

    let waiter = runtime
        .observe_result::<Report, _, _>(addr)
        .map_err(|e| anyhow::anyhow!("observe_result: {e:?}"))?;

    for _ in 0..INCREMENTS {
        runtime
            .try_send(addr, SqliteMsg::Increment)
            .map_err(|e| anyhow::anyhow!("try_send Increment: {e:?}"))?;
    }
    runtime
        .try_send(addr, SqliteMsg::Finalize)
        .map_err(|e| anyhow::anyhow!("try_send Finalize: {e:?}"))?;

    let report = waiter
        .wait(Duration::from_secs(10))
        .map_err(|e| anyhow::anyhow!("worker did not finish: {e:?}"))?;

    if let Ok(rt) = Arc::try_unwrap(runtime) {
        let _ = rt.shutdown();
    }
    drop(dir);
    Ok(report)
}

fn open_and_init(path: &std::path::Path) -> anyhow::Result<Connection> {
    let conn = Connection::open(path)?;
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS counter (id INTEGER PRIMARY KEY, value INTEGER NOT NULL);",
    )?;
    conn.execute(
        "INSERT OR IGNORE INTO counter (id, value) VALUES (0, 0)",
        params![],
    )?;
    Ok(conn)
}
