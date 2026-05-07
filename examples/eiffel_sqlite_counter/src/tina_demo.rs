//! Surface-shape demos for `tina-sqlite-bridge`. Each scenario
//! installs a fresh bridge, sends one request, and prints the typed
//! outcome. Together they document the failure surface a user will
//! see at the call site.

use std::convert::Infallible;
use std::sync::Arc;
use std::sync::{Condvar, Mutex};
use std::time::{Duration, Instant};

use tina::prelude::*;
use tina_runtime::{DefaultThreadedMailboxFactory, RuntimeCall, ThreadedRuntime};
use tina_sqlite_bridge::{
    InstalledSqliteBridge, SqliteAddress, SqliteCallOutcome, SqliteConfig, SqliteRequest,
    SqliteValue, SqliteWorker, send_request,
};

#[derive(Default)]
struct Sink {
    state: Mutex<Option<SqliteCallOutcome>>,
    cv: Condvar,
}

impl Sink {
    fn put(&self, outcome: SqliteCallOutcome) {
        *self.state.lock().expect("sink") = Some(outcome);
        self.cv.notify_all();
    }
    fn wait(&self, timeout: Duration) -> SqliteCallOutcome {
        let deadline = Instant::now() + timeout;
        let mut guard = self.state.lock().expect("sink");
        while guard.is_none() {
            let now = Instant::now();
            if now >= deadline {
                panic!("demo: sink wait exceeded {timeout:?}");
            }
            let (g, _) = self.cv.wait_timeout(guard, deadline - now).expect("wait");
            guard = g;
        }
        guard.take().expect("populated")
    }
}

#[derive(Debug)]
enum CallerMsg {
    Run(SqliteRequest),
    Done(SqliteCallOutcome),
}

struct Caller {
    bridge: SqliteAddress,
    timeout: Duration,
    sink: Arc<Sink>,
}

impl Isolate for Caller {
    tina::isolate_types! {
        message: CallerMsg,
        reply: (),
        send: tina::Outbound<Infallible>,
        spawn: Infallible,
        call: RuntimeCall<CallerMsg>,
        shard: SingleShard,
    }

    fn handle(
        &mut self,
        msg: CallerMsg,
        _ctx: &mut Context<'_, SingleShard, Self::Reply>,
    ) -> Effect<Self> {
        match msg {
            CallerMsg::Run(request) => {
                send_request(self.bridge, request, self.timeout).reply(CallerMsg::Done)
            }
            CallerMsg::Done(outcome) => {
                self.sink.put(outcome);
                stop()
            }
        }
    }
}

fn make_runtime() -> Arc<ThreadedRuntime<SingleShard, DefaultThreadedMailboxFactory>> {
    Arc::new(ThreadedRuntime::new(
        SingleShard,
        DefaultThreadedMailboxFactory,
    ))
}

fn install(
    runtime: &ThreadedRuntime<SingleShard, DefaultThreadedMailboxFactory>,
    cfg: SqliteConfig,
) -> InstalledSqliteBridge<SingleShard> {
    SqliteWorker::<SingleShard>::install(runtime, cfg).expect("install bridge")
}

fn run_one(
    runtime: &Arc<ThreadedRuntime<SingleShard, DefaultThreadedMailboxFactory>>,
    bridge: SqliteAddress,
    request: SqliteRequest,
    call_timeout: Duration,
    overall: Duration,
) -> SqliteCallOutcome {
    let sink = Arc::new(Sink::default());
    let caller = Caller {
        bridge,
        timeout: call_timeout,
        sink: Arc::clone(&sink),
    };
    let addr = runtime
        .register_with_capacity::<_, Infallible>(caller, 4)
        .expect("register caller");
    runtime
        .try_send(addr, CallerMsg::Run(request))
        .expect("kick");
    sink.wait(overall)
}

fn report(label: &str, outcome: &SqliteCallOutcome) {
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
    let runtime = make_runtime();
    let bridge = install(&runtime, SqliteConfig::memory());

    // Schema with a UNIQUE column.
    let _ = run_one(
        &runtime,
        bridge.address,
        SqliteRequest::Execute {
            sql: "CREATE TABLE t (k INTEGER PRIMARY KEY, v TEXT NOT NULL UNIQUE)".into(),
            params: vec![],
        },
        Duration::from_secs(2),
        Duration::from_secs(5),
    );
    // First insert: ok.
    let _ = run_one(
        &runtime,
        bridge.address,
        SqliteRequest::Execute {
            sql: "INSERT INTO t (k, v) VALUES (1, 'a')".into(),
            params: vec![],
        },
        Duration::from_secs(2),
        Duration::from_secs(5),
    );
    // Second insert with same v: constraint violation.
    let outcome = run_one(
        &runtime,
        bridge.address,
        SqliteRequest::Execute {
            sql: "INSERT INTO t (k, v) VALUES (2, 'a')".into(),
            params: vec![],
        },
        Duration::from_secs(2),
        Duration::from_secs(5),
    );
    report("constraint", &outcome);

    let snap = bridge.metrics.snapshot();
    println!(
        "demo=constraint metrics: admitted={} executed={} constraint={}",
        snap.admitted, snap.worker_executed, snap.worker_constraint,
    );

    shutdown(runtime);
    Ok(())
}

/// Demo: bridge `default_timeout` fires before the worker thread
/// finishes a long query. Caller sees
/// [`tina_sqlite_bridge::SqliteError::Timeout`]; metrics show
/// `late_results` once the worker terminal eventually lands.
pub fn demo_timeout() -> anyhow::Result<()> {
    let runtime = make_runtime();
    let cfg = SqliteConfig::memory()
        .with_default_timeout(Duration::from_millis(20))
        .with_poll_interval(Duration::from_millis(1));
    let bridge = install(&runtime, cfg);

    let outcome = run_one(
        &runtime,
        bridge.address,
        SqliteRequest::QueryRows {
            sql: "WITH RECURSIVE seq(x) AS (\
                SELECT 1 UNION ALL SELECT x + 1 FROM seq WHERE x < 1000000\
                ) SELECT SUM(x) FROM seq"
                .into(),
            params: vec![],
            max_rows: 1,
        },
        Duration::from_secs(15),
        Duration::from_secs(15),
    );
    report("timeout", &outcome);

    // Wait for the worker thread to finish so late_results lands.
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

/// Demo: a closed bridge replies
/// [`tina_sqlite_bridge::SqliteError::Closed`] to new admissions.
pub fn demo_closed() -> anyhow::Result<()> {
    let runtime = make_runtime();
    let bridge = install(&runtime, SqliteConfig::memory());

    bridge.closer.close();
    let outcome = run_one(
        &runtime,
        bridge.address,
        SqliteRequest::Execute {
            sql: "CREATE TABLE z (n INTEGER)".into(),
            params: vec![],
        },
        Duration::from_secs(2),
        Duration::from_secs(5),
    );
    report("closed", &outcome);

    let snap = bridge.metrics.snapshot();
    println!("demo=closed metrics: closed={}", snap.closed);

    shutdown(runtime);
    Ok(())
}

/// Demo: an over-cap parameter list surfaces as
/// [`tina_sqlite_bridge::SqliteError::InvalidRequest`] before the
/// worker thread sees the request.
pub fn demo_invalid() -> anyhow::Result<()> {
    let runtime = make_runtime();
    let bridge = install(&runtime, SqliteConfig::memory().with_max_request_params(2));

    let outcome = run_one(
        &runtime,
        bridge.address,
        SqliteRequest::Execute {
            sql: "SELECT ?, ?, ?".into(),
            params: vec![
                SqliteValue::Integer(1),
                SqliteValue::Integer(2),
                SqliteValue::Integer(3),
            ],
        },
        Duration::from_secs(2),
        Duration::from_secs(5),
    );
    report("invalid", &outcome);

    let snap = bridge.metrics.snapshot();
    println!("demo=invalid metrics: invalid={}", snap.invalid);

    shutdown(runtime);
    Ok(())
}
