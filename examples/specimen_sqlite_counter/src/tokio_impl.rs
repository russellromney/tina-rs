//! Tokio reference. rusqlite is sync; each query goes through
//! `spawn_blocking` so the async runtime is never blocked.
//!
//! `Connection` is `Send + !Sync`. We share it across blocking calls
//! via an `Arc<Mutex<_>>`. That's the textbook Tokio shape.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use rusqlite::{Connection, params};
use tokio::runtime::Builder;

use crate::{INCREMENTS, Report};

pub fn run() -> anyhow::Result<Report> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("counter-tokio.sqlite");

    let rt = Builder::new_current_thread().enable_all().build()?;
    rt.block_on(async {
        let conn = Arc::new(Mutex::new(open_and_init(&path).await?));

        for _ in 0..INCREMENTS {
            let conn = Arc::clone(&conn);
            tokio::task::spawn_blocking(move || -> anyhow::Result<()> {
                let conn = conn.lock().expect("conn mutex");
                conn.execute("UPDATE counter SET value = value + 1 WHERE id = 0", [])?;
                Ok(())
            })
            .await
            .map_err(|e| anyhow::anyhow!("blocking task: {e}"))??;
        }

        let conn = Arc::clone(&conn);
        let final_value = tokio::task::spawn_blocking(move || -> anyhow::Result<u64> {
            let conn = conn.lock().expect("conn mutex");
            let value: i64 =
                conn.query_row("SELECT value FROM counter WHERE id = 0", [], |row| row.get(0))?;
            Ok(value as u64)
        })
        .await
        .map_err(|e| anyhow::anyhow!("blocking task: {e}"))??;

        Ok(Report {
            final_value,
            exit_clean: true,
        })
    })
}

async fn open_and_init(path: &std::path::Path) -> anyhow::Result<Connection> {
    let path = PathBuf::from(path);
    tokio::task::spawn_blocking(move || -> anyhow::Result<Connection> {
        let conn = Connection::open(path)?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS counter (id INTEGER PRIMARY KEY, value INTEGER NOT NULL);",
        )?;
        conn.execute(
            "INSERT OR IGNORE INTO counter (id, value) VALUES (0, 0)",
            params![],
        )?;
        Ok(conn)
    })
    .await
    .map_err(|e| anyhow::anyhow!("blocking task: {e}"))?
}
