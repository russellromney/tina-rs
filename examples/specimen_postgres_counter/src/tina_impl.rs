//! Tina side. The host thread calls into a `tina-sqlx-bridge` worker
//! that owns the `sqlx::PgPool`. The shard thread never blocks on
//! SQLx. Compare to `tokio_impl.rs` (`pool.execute(...)` directly):
//! same Postgres pool underneath, but the Tina side names every
//! pressure cap and surfaces typed failures.

use std::time::Duration;

use tina::prelude::*;
use tina_runtime::{CallOutcome, DefaultThreadedMailboxFactory, LocalSystem, ThreadedRuntime};
use tina_sqlx_bridge::{PgConfig, PgMsg, PgPoolConfig, PgRequest, PgResponse, PgWorker};

use crate::{INCREMENTS, Report, unique_table};

const SQL_TIMEOUT: Duration = Duration::from_secs(5);

pub fn run(url: &str) -> anyhow::Result<Report> {
    let app = LocalSystem::single_shard(SingleShard, DefaultThreadedMailboxFactory).try_build()?;
    Ok(app.run_to_shutdown_reported(Duration::from_secs(5), move |app| run_application(app, url))?)
}

fn run_application(
    app: &LocalSystem<SingleShard, DefaultThreadedMailboxFactory>,
    url: &str,
) -> anyhow::Result<Report> {
    // The bridge helpers keep the capability-typed host call form through
    // the lower host-control handle.
    let host = app.host_control();
    let cfg = PgConfig::new()
        .with_pool(
            PgPoolConfig::new(url)
                .with_max_connections(2)
                .with_acquire_timeout(Duration::from_secs(5)),
        )
        .with_default_timeout(Duration::from_secs(5))
        .with_poll_interval(Duration::from_millis(1))
        .with_max_in_flight(2);
    let bridge = PgWorker::<SingleShard>::install_local(app, cfg)
        .map_err(|e| anyhow::anyhow!("install pg bridge: {e}"))?;

    let table = unique_table("tina_counter");
    let run_result = run_counter_script(&host, bridge.address, &table);
    // Cleanup and metrics stay unconditional: the table is dropped and the
    // bridge metrics are printed even when the script failed, then the
    // script error propagates so the runner and binary fail honestly.
    if let Err(error) = execute(&host, bridge.address, drop_sql(&table)) {
        eprintln!("specimen_postgres_counter (tina): drop table failed: {error:#}");
    }
    bridge.closer.close();

    let snap = bridge.metrics.snapshot();
    eprintln!(
        "specimen_postgres_counter (tina) bridge metrics: \
         admitted={} executed={} row={} timeouts={} late={} full={} \
         pool_acquire_timeouts={} sqlx_errors={} high_water={}",
        snap.admitted,
        snap.responses_executed,
        snap.responses_row,
        snap.timeouts,
        snap.late_results,
        snap.full,
        snap.pool_acquire_timeouts,
        snap.sqlx_errors,
        snap.in_flight_high_water,
    );

    let final_value = run_result?;
    Ok(Report {
        final_value,
        exit_clean: true,
    })
}

fn run_counter_script(
    runtime: &ThreadedRuntime<SingleShard, DefaultThreadedMailboxFactory>,
    db: tina_sqlx_bridge::PgAddress,
    table: &str,
) -> anyhow::Result<u64> {
    execute(runtime, db, create_sql(table))?;
    execute(runtime, db, seed_sql(table))?;
    for _ in 0..INCREMENTS {
        let rows = execute(runtime, db, step_sql(table))?;
        if rows != 1 {
            anyhow::bail!("step affected {rows} rows, expected 1");
        }
    }

    let outcome = runtime.call_blocking_typed(
        db,
        PgMsg::Send(PgRequest::fetch_one(finalize_sql(table))),
        SQL_TIMEOUT,
    )?;
    match outcome {
        CallOutcome::Replied(Ok(PgResponse::Row(row))) => row
            .get_i64(0)
            .and_then(|v| u64::try_from(v).ok())
            .ok_or_else(|| anyhow::anyhow!("final query did not return one unsigned integer")),
        other => anyhow::bail!("unexpected finalize outcome {other:?}"),
    }
}

fn execute(
    runtime: &ThreadedRuntime<SingleShard, DefaultThreadedMailboxFactory>,
    db: tina_sqlx_bridge::PgAddress,
    sql: String,
) -> anyhow::Result<u64> {
    let outcome =
        runtime.call_blocking_typed(db, PgMsg::Send(PgRequest::execute(sql)), SQL_TIMEOUT)?;
    match outcome {
        CallOutcome::Replied(Ok(PgResponse::Executed { rows_affected })) => Ok(rows_affected),
        other => anyhow::bail!("unexpected execute outcome {other:?}"),
    }
}

fn create_sql(table: &str) -> String {
    format!("CREATE TABLE {table} (id INT8 PRIMARY KEY, value INT8 NOT NULL)")
}

fn seed_sql(table: &str) -> String {
    format!("INSERT INTO {table} (id, value) VALUES (0, 0)")
}

fn step_sql(table: &str) -> String {
    format!("UPDATE {table} SET value = value + 1 WHERE id = 0")
}

fn finalize_sql(table: &str) -> String {
    format!("SELECT value FROM {table} WHERE id = 0")
}

fn drop_sql(table: &str) -> String {
    format!("DROP TABLE {table}")
}
