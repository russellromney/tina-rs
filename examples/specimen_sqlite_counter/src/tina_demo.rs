//! Surface-shape demos for `tina-sqlite-bridge`. Each scenario
//! installs a fresh bridge, runs a short isolate script that ends in
//! `stop_with`, and prints the typed outcome observed by the host.
//! Together they document the failure surface a user will see at the
//! call site, plus the `classify()` retry pattern.
//!
//! Application results travel through terminal observation only. There
//! is no result mutex, condvar, or host poll loop.

use std::convert::Infallible;
use std::time::Duration;

use tina::prelude::*;
use tina_runtime::{DefaultThreadedMailboxFactory, LocalSystem};
use tina_sqlite_bridge::{
    InstalledSqliteBridge, SqliteAddress, SqliteConfig, SqliteExecutedOutcome, SqliteOutcomeClass,
    SqliteOutcomeExt, SqliteRowsOutcome, SqliteTransientReason, SqliteWorker, execute_call,
    query_call,
};

// ---------- Execute demo actor ----------

#[derive(Debug)]
enum ExecMsg {
    Run {
        sql: String,
        params: Vec<tina_sqlite_bridge::SqliteValue>,
    },
    Done(SqliteExecutedOutcome),
}

struct ExecCaller {
    bridge: SqliteAddress,
    timeout: Duration,
}

#[tina_runtime::isolate(message = ExecMsg)]
impl ExecCaller {
    fn handle(
        &mut self,
        msg: ExecMsg,
        _ctx: &mut Context<'_, SingleShard, Self::Reply>,
    ) -> Effect<Self> {
        match msg {
            ExecMsg::Run { sql, params } => {
                execute_call(self.bridge, sql, params, self.timeout).then(ExecMsg::Done)
            }
            ExecMsg::Done(outcome) => stop_with(outcome),
        }
    }
}

// ---------- Query demo actor ----------

#[derive(Debug)]
enum QueryMsg {
    Run {
        sql: String,
        params: Vec<tina_sqlite_bridge::SqliteValue>,
        max_rows: usize,
    },
    Done(SqliteRowsOutcome),
}

struct QueryCaller {
    bridge: SqliteAddress,
    timeout: Duration,
}

#[tina_runtime::isolate(message = QueryMsg)]
impl QueryCaller {
    fn handle(
        &mut self,
        msg: QueryMsg,
        _ctx: &mut Context<'_, SingleShard, Self::Reply>,
    ) -> Effect<Self> {
        match msg {
            QueryMsg::Run {
                sql,
                params,
                max_rows,
            } => query_call(self.bridge, sql, params, max_rows, self.timeout).then(QueryMsg::Done),
            QueryMsg::Done(outcome) => stop_with(outcome),
        }
    }
}

// ---------- Shared helpers ----------

fn with_app<T>(
    f: impl FnOnce(&LocalSystem<SingleShard, DefaultThreadedMailboxFactory>) -> anyhow::Result<T>,
) -> anyhow::Result<T> {
    let app = LocalSystem::single_shard(SingleShard, DefaultThreadedMailboxFactory).try_build()?;
    Ok(app.run_to_shutdown_reported(Duration::from_secs(30), f)?)
}

fn install(
    app: &LocalSystem<SingleShard, DefaultThreadedMailboxFactory>,
    cfg: SqliteConfig,
) -> InstalledSqliteBridge<SingleShard> {
    SqliteWorker::<SingleShard>::install_local(app, cfg).expect("install bridge")
}

fn run_exec(
    app: &LocalSystem<SingleShard, DefaultThreadedMailboxFactory>,
    bridge: SqliteAddress,
    sql: &str,
    params: Vec<tina_sqlite_bridge::SqliteValue>,
    call_timeout: Duration,
) -> anyhow::Result<SqliteExecutedOutcome> {
    let addr = app
        .register_root::<_, Infallible>(
            ExecCaller {
                bridge,
                timeout: call_timeout,
            },
            4,
        )
        .map_err(|e| anyhow::anyhow!("register exec caller: {e:?}"))?;
    let waiter = app
        .observe_result::<SqliteExecutedOutcome, _, _>(addr)
        .map_err(|e| anyhow::anyhow!("observe_result: {e:?}"))?;
    app.try_send(
        addr,
        ExecMsg::Run {
            sql: sql.to_string(),
            params,
        },
    )
    .map_err(|e| anyhow::anyhow!("kick exec: {e:?}"))?;
    waiter
        .wait(Duration::from_secs(15))
        .map_err(|e| anyhow::anyhow!("exec caller did not finish: {e:?}"))
}

fn run_query(
    app: &LocalSystem<SingleShard, DefaultThreadedMailboxFactory>,
    bridge: SqliteAddress,
    sql: &str,
    params: Vec<tina_sqlite_bridge::SqliteValue>,
    max_rows: usize,
    call_timeout: Duration,
) -> anyhow::Result<SqliteRowsOutcome> {
    let addr = app
        .register_root::<_, Infallible>(
            QueryCaller {
                bridge,
                timeout: call_timeout,
            },
            4,
        )
        .map_err(|e| anyhow::anyhow!("register query caller: {e:?}"))?;
    let waiter = app
        .observe_result::<SqliteRowsOutcome, _, _>(addr)
        .map_err(|e| anyhow::anyhow!("observe_result: {e:?}"))?;
    app.try_send(
        addr,
        QueryMsg::Run {
            sql: sql.to_string(),
            params,
            max_rows,
        },
    )
    .map_err(|e| anyhow::anyhow!("kick query: {e:?}"))?;
    waiter
        .wait(Duration::from_secs(15))
        .map_err(|e| anyhow::anyhow!("query caller did not finish: {e:?}"))
}

fn report_exec(label: &str, outcome: &SqliteExecutedOutcome) {
    println!("demo={label} outcome={outcome:?}");
}

fn report_query(label: &str, outcome: &SqliteRowsOutcome) {
    println!("demo={label} outcome={outcome:?}");
}

/// Demo: a `UNIQUE` constraint violation surfaces as
/// [`tina_sqlite_bridge::SqliteError::Constraint`] with the underlying
/// SQLite message preserved.
pub fn demo_constraint() -> anyhow::Result<()> {
    with_app(|app| {
        let bridge = install(app, SqliteConfig::memory());

        let _ = run_exec(
            app,
            bridge.address,
            "CREATE TABLE t (k INTEGER PRIMARY KEY, v TEXT NOT NULL UNIQUE)",
            vec![],
            Duration::from_secs(2),
        )?;
        let _ = run_exec(
            app,
            bridge.address,
            "INSERT INTO t (k, v) VALUES (1, 'a')",
            vec![],
            Duration::from_secs(2),
        )?;
        let outcome = run_exec(
            app,
            bridge.address,
            "INSERT INTO t (k, v) VALUES (2, 'a')",
            vec![],
            Duration::from_secs(2),
        )?;
        report_exec("constraint", &outcome);

        let snap = bridge.metrics.snapshot();
        println!(
            "demo=constraint metrics: admitted={} executed={} constraint={}",
            snap.admitted, snap.worker_executed, snap.worker_constraint,
        );

        bridge.closer.close();
        Ok(())
    })
}

/// Demo: bridge `default_timeout` fires before the worker thread
/// finishes a long query. Caller sees `SqliteError::Timeout`;
/// metrics show `late_results` once the worker terminal lands.
pub fn demo_timeout() -> anyhow::Result<()> {
    with_app(|app| {
        let cfg = SqliteConfig::memory()
            .with_default_timeout(Duration::from_millis(20))
            .with_poll_interval(Duration::from_millis(1));
        let bridge = install(app, cfg);

        let outcome = run_query(
            app,
            bridge.address,
            "WITH RECURSIVE seq(x) AS (\
                SELECT 1 UNION ALL SELECT x + 1 FROM seq WHERE x < 1000000\
                ) SELECT SUM(x) FROM seq",
            vec![],
            1,
            Duration::from_secs(15),
        )?;
        report_query("timeout", &outcome);

        // Single bounded wait for the worker terminal to land as a late
        // result. Not a poll loop publishing application results — the
        // call outcome already arrived via stop_with/observe_result.
        std::thread::sleep(Duration::from_millis(500));
        let snap = bridge.metrics.snapshot();
        println!(
            "demo=timeout metrics: timeouts={} late_results={} worker_rows={}",
            snap.timeouts, snap.late_results, snap.worker_rows,
        );

        bridge.closer.close();
        Ok(())
    })
}

/// Demo: a closed bridge replies `SqliteError::Closed` to new
/// admissions.
pub fn demo_closed() -> anyhow::Result<()> {
    with_app(|app| {
        let bridge = install(app, SqliteConfig::memory());

        bridge.closer.close();
        let outcome = run_exec(
            app,
            bridge.address,
            "CREATE TABLE z (n INTEGER)",
            vec![],
            Duration::from_secs(2),
        )?;
        report_exec("closed", &outcome);

        let snap = bridge.metrics.snapshot();
        println!("demo=closed metrics: closed={}", snap.closed);

        Ok(())
    })
}

/// Demo: an over-cap parameter list surfaces as
/// `SqliteError::InvalidRequest` before the worker thread sees the
/// request.
pub fn demo_invalid() -> anyhow::Result<()> {
    with_app(|app| {
        let bridge = install(app, SqliteConfig::memory().with_max_request_params(2));

        let outcome = run_exec(
            app,
            bridge.address,
            "SELECT ?, ?, ?",
            vec![1.into(), 2.into(), 3.into()],
            Duration::from_secs(2),
        )?;
        report_exec("invalid", &outcome);

        let snap = bridge.metrics.snapshot();
        println!("demo=invalid metrics: invalid={}", snap.invalid);

        bridge.closer.close();
        Ok(())
    })
}

/// Demo: classify() guides a transient-vs-fatal decision. Here we
/// fabricate the loop on the caller side: each attempt's typed
/// outcome is classified, transient outcomes retry, fatal outcomes
/// stop. The bridge does no retrying internally — it surfaces
/// truth, the caller decides.
///
/// In this demo we issue the same `Execute` twice. The first attempt
/// admits and succeeds; the second trips a UNIQUE constraint and is
/// classified as `Fatal(Constraint)`. We do not retry it (constraint
/// violations are not retryable), and we print the classification
/// chain so users see the shape.
pub fn demo_retry() -> anyhow::Result<()> {
    with_app(|app| {
        let bridge = install(app, SqliteConfig::memory());

        let _ = run_exec(
            app,
            bridge.address,
            "CREATE TABLE t (k INTEGER PRIMARY KEY, v TEXT NOT NULL UNIQUE)",
            vec![],
            Duration::from_secs(2),
        )?;
        let _ = run_exec(
            app,
            bridge.address,
            "INSERT INTO t (k, v) VALUES (1, 'a')",
            vec![],
            Duration::from_secs(2),
        )?;

        let mut attempts = 0;
        let max_attempts = 3u32;
        loop {
            attempts += 1;
            let outcome = run_exec(
                app,
                bridge.address,
                "INSERT INTO t (k, v) VALUES (?, ?)",
                vec![2.into(), "a".into()],
                Duration::from_secs(2),
            )?;
            match outcome.classify() {
                SqliteOutcomeClass::Succeeded(rows_changed) => {
                    println!(
                        "demo=retry attempt={attempts} class=Succeeded rows_changed={rows_changed}"
                    );
                    break;
                }
                SqliteOutcomeClass::Transient(reason) => {
                    println!("demo=retry attempt={attempts} class=Transient reason={reason:?}");
                    if attempts >= max_attempts {
                        println!("demo=retry budget_exhausted");
                        break;
                    }
                    // Sleep before retry; for `Busy` you'd typically
                    // back off here.
                    std::thread::sleep(match reason {
                        SqliteTransientReason::Busy => Duration::from_millis(50),
                        _ => Duration::from_millis(10),
                    });
                }
                SqliteOutcomeClass::Fatal(reason) => {
                    println!("demo=retry attempt={attempts} class=Fatal reason={reason:?}");
                    break;
                }
            }
        }

        bridge.closer.close();
        Ok(())
    })
}

/// Point-in-time inspection demo: seed a value, then read it back
/// through the bridge's existing typed query request (host
/// `call_blocking` of `SqliteRequest::query_rows`). No result sidecar.
pub fn demo_point_in_time_query() -> anyhow::Result<()> {
    with_app(|app| {
        let bridge = install(app, SqliteConfig::memory());
        let _ = run_exec(
            app,
            bridge.address,
            "CREATE TABLE counter (id INTEGER PRIMARY KEY, value INTEGER NOT NULL)",
            vec![],
            Duration::from_secs(2),
        )?;
        let _ = run_exec(
            app,
            bridge.address,
            "INSERT INTO counter (id, value) VALUES (0, 7)",
            vec![],
            Duration::from_secs(2),
        )?;

        let value = crate::tina_impl::query_counter_value(app, bridge.address)?;
        println!("demo=point_in_time value={value}");
        assert_eq!(value, 7);

        bridge.closer.close();
        Ok(())
    })
}
