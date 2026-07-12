//! Surface-shape demos for `tina-sqlite-bridge`. Each scenario
//! installs a fresh bridge, sends one or more requests, and prints
//! the typed outcome. Together they document the failure surface a
//! user will see at the call site, plus the `classify()` retry
//! pattern.

use std::convert::Infallible;
use std::sync::Arc;
use std::sync::{Condvar, Mutex};
use std::time::{Duration, Instant};

use tina::prelude::*;
use tina_runtime::{DefaultThreadedMailboxFactory, ThreadedRuntime};
use tina_sqlite_bridge::{
    InstalledSqliteBridge, SqliteAddress, SqliteConfig, SqliteExecutedOutcome,
    SqliteOutcomeClass, SqliteOutcomeExt, SqliteRowsOutcome, SqliteTransientReason, SqliteWorker,
    execute_call, query_call,
};

#[derive(Default)]
struct ExecSink {
    state: Mutex<Option<SqliteExecutedOutcome>>,
    cv: Condvar,
}

impl ExecSink {
    fn put(&self, outcome: SqliteExecutedOutcome) {
        *self.state.lock().expect("sink") = Some(outcome);
        self.cv.notify_all();
    }
    fn wait(&self, timeout: Duration) -> SqliteExecutedOutcome {
        let deadline = Instant::now() + timeout;
        let mut guard = self.state.lock().expect("sink");
        while guard.is_none() {
            let now = Instant::now();
            if now >= deadline {
                panic!("demo: ExecSink wait exceeded {timeout:?}");
            }
            let (g, _) = self
                .cv
                .wait_timeout(guard, deadline - now)
                .expect("wait");
            guard = g;
        }
        guard.take().expect("populated")
    }
}

#[derive(Default)]
struct QuerySink {
    state: Mutex<Option<SqliteRowsOutcome>>,
    cv: Condvar,
}

impl QuerySink {
    fn put(&self, outcome: SqliteRowsOutcome) {
        *self.state.lock().expect("sink") = Some(outcome);
        self.cv.notify_all();
    }
    #[allow(dead_code)]
    fn wait(&self, timeout: Duration) -> SqliteRowsOutcome {
        let deadline = Instant::now() + timeout;
        let mut guard = self.state.lock().expect("sink");
        while guard.is_none() {
            let now = Instant::now();
            if now >= deadline {
                panic!("demo: QuerySink wait exceeded {timeout:?}");
            }
            let (g, _) = self
                .cv
                .wait_timeout(guard, deadline - now)
                .expect("wait");
            guard = g;
        }
        guard.take().expect("populated")
    }
}

#[derive(Debug)]
enum CallerMsg {
    RunExec {
        sql: String,
        params: Vec<tina_sqlite_bridge::SqliteValue>,
    },
    DoneExec(SqliteExecutedOutcome),
}

struct Caller {
    bridge: SqliteAddress,
    timeout: Duration,
    sink: Arc<ExecSink>,
}

#[tina_runtime::isolate(message = CallerMsg)]
impl Caller {
    fn handle(
        &mut self,
        msg: CallerMsg,
        _ctx: &mut Context<'_, SingleShard, Self::Reply>,
    ) -> Effect<Self> {
        match msg {
            CallerMsg::RunExec { sql, params } => {
                execute_call(self.bridge, sql, params, self.timeout).then(CallerMsg::DoneExec)
            }
            CallerMsg::DoneExec(outcome) => {
                self.sink.put(outcome);
                stop()
            }
        }
    }
}

fn make_runtime() -> anyhow::Result<Arc<ThreadedRuntime<SingleShard, DefaultThreadedMailboxFactory>>>
{
    ThreadedRuntime::try_new(SingleShard, DefaultThreadedMailboxFactory)
        .map(Arc::new)
        .map_err(anyhow::Error::new)
}

fn install(
    runtime: &ThreadedRuntime<SingleShard, DefaultThreadedMailboxFactory>,
    cfg: SqliteConfig,
) -> InstalledSqliteBridge<SingleShard> {
    SqliteWorker::<SingleShard>::install(runtime, cfg).expect("install bridge")
}

fn run_exec(
    runtime: &Arc<ThreadedRuntime<SingleShard, DefaultThreadedMailboxFactory>>,
    bridge: SqliteAddress,
    sql: &str,
    params: Vec<tina_sqlite_bridge::SqliteValue>,
    call_timeout: Duration,
    overall: Duration,
) -> SqliteExecutedOutcome {
    let sink = Arc::new(ExecSink::default());
    let caller = Caller {
        bridge,
        timeout: call_timeout,
        sink: Arc::clone(&sink),
    };
    let addr = runtime
        .register_with_capacity::<_, Infallible>(caller, 4)
        .expect("register caller");
    runtime
        .try_send(
            addr,
            CallerMsg::RunExec {
                sql: sql.to_string(),
                params,
            },
        )
        .expect("kick");
    sink.wait(overall)
}

#[derive(Debug)]
enum QueryCallerMsg {
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
    sink: Arc<QuerySink>,
}

#[tina_runtime::isolate(message = QueryCallerMsg)]
impl QueryCaller {
    fn handle(
        &mut self,
        msg: QueryCallerMsg,
        _ctx: &mut Context<'_, SingleShard, Self::Reply>,
    ) -> Effect<Self> {
        match msg {
            QueryCallerMsg::Run {
                sql,
                params,
                max_rows,
            } => query_call(self.bridge, sql, params, max_rows, self.timeout)
                .then(QueryCallerMsg::Done),
            QueryCallerMsg::Done(outcome) => {
                self.sink.put(outcome);
                stop()
            }
        }
    }
}

fn run_query(
    runtime: &Arc<ThreadedRuntime<SingleShard, DefaultThreadedMailboxFactory>>,
    bridge: SqliteAddress,
    sql: &str,
    params: Vec<tina_sqlite_bridge::SqliteValue>,
    max_rows: usize,
    call_timeout: Duration,
    overall: Duration,
) -> SqliteRowsOutcome {
    let sink = Arc::new(QuerySink::default());
    let caller = QueryCaller {
        bridge,
        timeout: call_timeout,
        sink: Arc::clone(&sink),
    };
    let addr = runtime
        .register_with_capacity::<_, Infallible>(caller, 4)
        .expect("register caller");
    runtime
        .try_send(
            addr,
            QueryCallerMsg::Run {
                sql: sql.to_string(),
                params,
                max_rows,
            },
        )
        .expect("kick");
    sink.wait(overall)
}

fn report_exec(label: &str, outcome: &SqliteExecutedOutcome) {
    println!("demo={label} outcome={outcome:?}");
}

fn report_query(label: &str, outcome: &SqliteRowsOutcome) {
    println!("demo={label} outcome={outcome:?}");
}

fn shutdown(runtime: Arc<ThreadedRuntime<SingleShard, DefaultThreadedMailboxFactory>>) {
    if let Ok(rt) = Arc::try_unwrap(runtime) {
        let _ = rt.shutdown();
    }
}

/// Demo: a `UNIQUE` constraint violation surfaces as
/// [`tina_sqlite_bridge::SqliteError::Constraint`] with the underlying
/// SQLite message preserved.
pub fn demo_constraint() -> anyhow::Result<()> {
    let runtime = make_runtime()?;
    let bridge = install(&runtime, SqliteConfig::memory());

    let _ = run_exec(
        &runtime,
        bridge.address,
        "CREATE TABLE t (k INTEGER PRIMARY KEY, v TEXT NOT NULL UNIQUE)",
        vec![],
        Duration::from_secs(2),
        Duration::from_secs(5),
    );
    let _ = run_exec(
        &runtime,
        bridge.address,
        "INSERT INTO t (k, v) VALUES (1, 'a')",
        vec![],
        Duration::from_secs(2),
        Duration::from_secs(5),
    );
    let outcome = run_exec(
        &runtime,
        bridge.address,
        "INSERT INTO t (k, v) VALUES (2, 'a')",
        vec![],
        Duration::from_secs(2),
        Duration::from_secs(5),
    );
    report_exec("constraint", &outcome);

    let snap = bridge.metrics.snapshot();
    println!(
        "demo=constraint metrics: admitted={} executed={} constraint={}",
        snap.admitted, snap.worker_executed, snap.worker_constraint,
    );

    shutdown(runtime);
    Ok(())
}

/// Demo: bridge `default_timeout` fires before the worker thread
/// finishes a long query. Caller sees `SqliteError::Timeout`;
/// metrics show `late_results` once the worker terminal lands.
pub fn demo_timeout() -> anyhow::Result<()> {
    let runtime = make_runtime()?;
    let cfg = SqliteConfig::memory()
        .with_default_timeout(Duration::from_millis(20))
        .with_poll_interval(Duration::from_millis(1));
    let bridge = install(&runtime, cfg);

    let outcome = run_query(
        &runtime,
        bridge.address,
        "WITH RECURSIVE seq(x) AS (\
            SELECT 1 UNION ALL SELECT x + 1 FROM seq WHERE x < 1000000\
            ) SELECT SUM(x) FROM seq",
        vec![],
        1,
        Duration::from_secs(15),
        Duration::from_secs(15),
    );
    report_query("timeout", &outcome);

    let deadline = Instant::now() + Duration::from_secs(30);
    while Instant::now() < deadline {
        if bridge.metrics.snapshot().late_results >= 1 {
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    let snap = bridge.metrics.snapshot();
    println!(
        "demo=timeout metrics: timeouts={} late_results={} worker_rows={}",
        snap.timeouts, snap.late_results, snap.worker_rows,
    );

    shutdown(runtime);
    Ok(())
}

/// Demo: a closed bridge replies `SqliteError::Closed` to new
/// admissions.
pub fn demo_closed() -> anyhow::Result<()> {
    let runtime = make_runtime()?;
    let bridge = install(&runtime, SqliteConfig::memory());

    bridge.closer.close();
    let outcome = run_exec(
        &runtime,
        bridge.address,
        "CREATE TABLE z (n INTEGER)",
        vec![],
        Duration::from_secs(2),
        Duration::from_secs(5),
    );
    report_exec("closed", &outcome);

    let snap = bridge.metrics.snapshot();
    println!("demo=closed metrics: closed={}", snap.closed);

    shutdown(runtime);
    Ok(())
}

/// Demo: an over-cap parameter list surfaces as
/// `SqliteError::InvalidRequest` before the worker thread sees the
/// request.
pub fn demo_invalid() -> anyhow::Result<()> {
    let runtime = make_runtime()?;
    let bridge = install(&runtime, SqliteConfig::memory().with_max_request_params(2));

    let outcome = run_exec(
        &runtime,
        bridge.address,
        "SELECT ?, ?, ?",
        vec![1.into(), 2.into(), 3.into()],
        Duration::from_secs(2),
        Duration::from_secs(5),
    );
    report_exec("invalid", &outcome);

    let snap = bridge.metrics.snapshot();
    println!("demo=invalid metrics: invalid={}", snap.invalid);

    shutdown(runtime);
    Ok(())
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
    let runtime = make_runtime()?;
    let bridge = install(&runtime, SqliteConfig::memory());

    let _ = run_exec(
        &runtime,
        bridge.address,
        "CREATE TABLE t (k INTEGER PRIMARY KEY, v TEXT NOT NULL UNIQUE)",
        vec![],
        Duration::from_secs(2),
        Duration::from_secs(5),
    );
    let _ = run_exec(
        &runtime,
        bridge.address,
        "INSERT INTO t (k, v) VALUES (1, 'a')",
        vec![],
        Duration::from_secs(2),
        Duration::from_secs(5),
    );

    let mut attempts = 0;
    let max_attempts = 3u32;
    loop {
        attempts += 1;
        let outcome = run_exec(
            &runtime,
            bridge.address,
            "INSERT INTO t (k, v) VALUES (?, ?)",
            vec![2.into(), "a".into()],
            Duration::from_secs(2),
            Duration::from_secs(5),
        );
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

    shutdown(runtime);
    Ok(())
}
