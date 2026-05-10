//! Tokio reference. Drives `sqlx::PgPool` directly from a Tokio
//! current-thread runtime.

use std::time::Duration;

use sqlx::postgres::PgPoolOptions;
use tokio::runtime::Builder;

use crate::{INCREMENTS, Report, unique_table};

pub fn run(url: &str) -> anyhow::Result<Report> {
    let rt = Builder::new_current_thread().enable_all().build()?;
    rt.block_on(async {
        let pool = PgPoolOptions::new()
            .max_connections(2)
            .acquire_timeout(Duration::from_secs(5))
            .connect(url)
            .await?;
        let table = unique_table("tokio_counter");

        sqlx::query(&format!(
            "CREATE TABLE {table} (id INT8 PRIMARY KEY, value INT8 NOT NULL)"
        ))
        .execute(&pool)
        .await?;
        sqlx::query(&format!("INSERT INTO {table} (id, value) VALUES (0, 0)"))
            .execute(&pool)
            .await?;

        for _ in 0..INCREMENTS {
            sqlx::query(&format!("UPDATE {table} SET value = value + 1 WHERE id = 0"))
                .execute(&pool)
                .await?;
        }

        let row: (i64,) = sqlx::query_as(&format!("SELECT value FROM {table} WHERE id = 0"))
            .fetch_one(&pool)
            .await?;
        let _ = sqlx::query(&format!("DROP TABLE {table}"))
            .execute(&pool)
            .await;

        pool.close().await;

        Ok(Report {
            final_value: u64::try_from(row.0).unwrap_or(0),
            exit_clean: true,
        })
    })
}
