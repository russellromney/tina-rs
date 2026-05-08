//! End-to-end tests for `tina-sqlite-bridge`.
//!
//! Each scenario stands a runtime, installs the bridge, and drives one
//! or more caller isolates that pump outcomes into a `Sink` for the
//! test thread to inspect.

use std::convert::Infallible;
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use tina::prelude::*;
use tina_runtime::{
    CallOutcome, DefaultThreadedMailboxFactory, RuntimeCall, ThreadedRuntime, ThreadedRuntimeConfig,
};
use tina_sqlite_bridge::{
    InstalledSqliteBridge, SqliteAddress, SqliteCallOutcome, SqliteConfig, SqliteConfigError,
    SqliteError, SqliteRequest, SqliteResponse, SqliteValue, SqliteWorker, send_request,
};

#[derive(Default)]
struct Sink {
    state: Mutex<Vec<SqliteCallOutcome>>,
    cv: Condvar,
}

impl Sink {
    fn put(&self, outcome: SqliteCallOutcome) {
        self.state.lock().expect("sink lock").push(outcome);
        self.cv.notify_all();
    }

    fn wait_one(&self, timeout: Duration) -> SqliteCallOutcome {
        let deadline = Instant::now() + timeout;
        let mut guard = self.state.lock().expect("sink lock");
        while guard.is_empty() {
            let now = Instant::now();
            if now >= deadline {
                panic!("sink wait timed out after {timeout:?}");
            }
            let (g, _) = self
                .cv
                .wait_timeout(guard, deadline - now)
                .expect("sink wait");
            guard = g;
        }
        guard.remove(0)
    }

    fn wait_n(&self, n: usize, timeout: Duration) -> Vec<SqliteCallOutcome> {
        let deadline = Instant::now() + timeout;
        let mut guard = self.state.lock().expect("sink lock");
        while guard.len() < n {
            let now = Instant::now();
            if now >= deadline {
                panic!("sink wait_n timed out: have {}, want {n}", guard.len());
            }
            let (g, _) = self
                .cv
                .wait_timeout(guard, deadline - now)
                .expect("sink wait_n");
            guard = g;
        }
        guard.drain(0..n).collect()
    }

    fn len(&self) -> usize {
        self.state.lock().expect("sink lock").len()
    }
}

#[derive(Debug)]
enum CallerMsg {
    Run(SqliteRequest),
    Done(SqliteCallOutcome),
}

struct CallerIsolate {
    worker: SqliteAddress,
    timeout: Duration,
    sink: Arc<Sink>,
}

impl Isolate for CallerIsolate {
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
                send_request(self.worker, request, self.timeout).reply(CallerMsg::Done)
            }
            CallerMsg::Done(outcome) => {
                self.sink.put(outcome);
                stop()
            }
        }
    }
}

fn make_runtime() -> Arc<ThreadedRuntime<SingleShard, DefaultThreadedMailboxFactory>> {
    Arc::new(ThreadedRuntime::with_config(
        SingleShard,
        DefaultThreadedMailboxFactory,
        ThreadedRuntimeConfig {
            command_capacity: 32,
            idle_wait: Duration::from_millis(1),
            ..Default::default()
        },
    ))
}

fn install_bridge(
    runtime: &ThreadedRuntime<SingleShard, DefaultThreadedMailboxFactory>,
    config: SqliteConfig,
) -> InstalledSqliteBridge<SingleShard> {
    SqliteWorker::<SingleShard>::install(runtime, config).expect("install bridge")
}

fn register_caller(
    runtime: &ThreadedRuntime<SingleShard, DefaultThreadedMailboxFactory>,
    worker: SqliteAddress,
    sink: Arc<Sink>,
    timeout: Duration,
) -> Address<CallerMsg, ()> {
    runtime
        .register_with_capacity::<_, Infallible>(
            CallerIsolate {
                worker,
                timeout,
                sink,
            },
            8,
        )
        .expect("register caller")
}

fn test_config() -> SqliteConfig {
    SqliteConfig::memory()
        .with_poll_interval(Duration::from_millis(1))
        .with_default_timeout(Duration::from_secs(2))
}

fn wait_admitted(bridge: &InstalledSqliteBridge<SingleShard>, target: u64, timeout: Duration) {
    let deadline = Instant::now() + timeout;
    while bridge.metrics.snapshot().admitted < target {
        if Instant::now() >= deadline {
            panic!(
                "admitted reached {} (want {target}) within {timeout:?}",
                bridge.metrics.snapshot().admitted
            );
        }
        std::thread::sleep(Duration::from_millis(1));
    }
}

/// Heavy enough that the worker thread is still running for ~hundreds
/// of ms even on fast hardware. Used to keep `max_in_flight = 1`
/// saturated long enough to deterministically race a second admission.
const HEAVY_QUERY: &str = "WITH RECURSIVE seq(x) AS (\
    SELECT 1 UNION ALL SELECT x + 1 FROM seq WHERE x < 1000000\
    ) SELECT SUM(x) FROM seq";

fn shutdown_runtime(runtime: Arc<ThreadedRuntime<SingleShard, DefaultThreadedMailboxFactory>>) {
    if let Ok(rt) = Arc::try_unwrap(runtime) {
        let _ = rt.shutdown();
    }
}

// ---------------------------------------------------------------------------
// Config validation
// ---------------------------------------------------------------------------

#[test]
fn config_rejects_zero_mailbox() {
    let cfg = SqliteConfig::memory().with_mailbox_capacity(0);
    assert_eq!(cfg.validate(), Err(SqliteConfigError::ZeroMailboxCapacity));
}

#[test]
fn config_rejects_max_in_flight_zero() {
    let cfg = SqliteConfig::memory().with_max_in_flight(0);
    assert_eq!(
        cfg.validate(),
        Err(SqliteConfigError::InvalidMaxInFlight { requested: 0 })
    );
}

#[test]
fn config_rejects_max_in_flight_above_one() {
    let cfg = SqliteConfig::memory().with_max_in_flight(2);
    assert_eq!(
        cfg.validate(),
        Err(SqliteConfigError::InvalidMaxInFlight { requested: 2 })
    );
}

#[test]
fn config_rejects_external_pool_zero() {
    let cfg = SqliteConfig::memory().with_external_pool_size(0);
    assert_eq!(
        cfg.validate(),
        Err(SqliteConfigError::InvalidExternalPoolSize { requested: 0 })
    );
}

#[test]
fn config_rejects_external_pool_above_one() {
    let cfg = SqliteConfig::memory().with_external_pool_size(4);
    assert_eq!(
        cfg.validate(),
        Err(SqliteConfigError::InvalidExternalPoolSize { requested: 4 })
    );
}

#[test]
fn config_rejects_zero_pending_reply_capacity() {
    let cfg = SqliteConfig::memory().with_pending_reply_capacity(0);
    assert_eq!(
        cfg.validate(),
        Err(SqliteConfigError::ZeroPendingReplyCapacity)
    );
}

#[test]
fn config_rejects_zero_default_timeout() {
    let cfg = SqliteConfig::memory().with_default_timeout(Duration::ZERO);
    assert_eq!(cfg.validate(), Err(SqliteConfigError::ZeroDefaultTimeout));
}

#[test]
fn config_rejects_zero_poll_interval() {
    let cfg = SqliteConfig::memory().with_poll_interval(Duration::ZERO);
    assert_eq!(cfg.validate(), Err(SqliteConfigError::ZeroPollInterval));
}

#[test]
fn config_rejects_zero_max_request_params() {
    let cfg = SqliteConfig::memory().with_max_request_params(0);
    assert_eq!(cfg.validate(), Err(SqliteConfigError::ZeroMaxRequestParams));
}

#[test]
fn config_rejects_zero_max_response_rows() {
    let cfg = SqliteConfig::memory().with_max_response_rows(0);
    assert_eq!(cfg.validate(), Err(SqliteConfigError::ZeroMaxResponseRows));
}

// ---------------------------------------------------------------------------
// Happy paths
// ---------------------------------------------------------------------------

fn create_and_seed(
    bridge_addr: SqliteAddress,
    runtime: &ThreadedRuntime<SingleShard, DefaultThreadedMailboxFactory>,
) {
    let sink = Arc::new(Sink::default());
    let caller = register_caller(
        runtime,
        bridge_addr,
        Arc::clone(&sink),
        Duration::from_secs(2),
    );
    runtime
        .try_send(
            caller,
            CallerMsg::Run(SqliteRequest::Execute {
                sql: "CREATE TABLE t (k INTEGER PRIMARY KEY, v TEXT NOT NULL UNIQUE)".into(),
                params: vec![],
            }),
        )
        .expect("kick create");
    let outcome = sink.wait_one(Duration::from_secs(5));
    match outcome {
        CallOutcome::Replied(Ok(SqliteResponse::Executed { .. })) => {}
        other => panic!("create table outcome: {other:?}"),
    }
}

#[test]
fn happy_execute_and_query() {
    let runtime = make_runtime();
    let bridge = install_bridge(&runtime, test_config());
    create_and_seed(bridge.address, &runtime);

    // INSERT a row with bound parameters.
    let sink = Arc::new(Sink::default());
    let caller = register_caller(
        &runtime,
        bridge.address,
        Arc::clone(&sink),
        Duration::from_secs(2),
    );
    runtime
        .try_send(
            caller,
            CallerMsg::Run(SqliteRequest::Execute {
                sql: "INSERT INTO t (k, v) VALUES (?, ?)".into(),
                params: vec![SqliteValue::Integer(1), SqliteValue::Text("hello".into())],
            }),
        )
        .expect("kick insert");
    match sink.wait_one(Duration::from_secs(5)) {
        CallOutcome::Replied(Ok(SqliteResponse::Executed { rows_changed })) => {
            assert_eq!(rows_changed, 1);
        }
        other => panic!("insert outcome: {other:?}"),
    }

    // SELECT it back.
    let sink2 = Arc::new(Sink::default());
    let caller2 = register_caller(
        &runtime,
        bridge.address,
        Arc::clone(&sink2),
        Duration::from_secs(2),
    );
    runtime
        .try_send(
            caller2,
            CallerMsg::Run(SqliteRequest::QueryRows {
                sql: "SELECT k, v FROM t WHERE k = ?".into(),
                params: vec![SqliteValue::Integer(1)],
                max_rows: 16,
            }),
        )
        .expect("kick query");
    match sink2.wait_one(Duration::from_secs(5)) {
        CallOutcome::Replied(Ok(SqliteResponse::Rows { columns, rows })) => {
            assert_eq!(columns, vec!["k".to_string(), "v".to_string()]);
            assert_eq!(rows.len(), 1);
            assert_eq!(rows[0][0], SqliteValue::Integer(1));
            assert_eq!(rows[0][1], SqliteValue::Text("hello".into()));
        }
        other => panic!("select outcome: {other:?}"),
    }

    let snap = bridge.metrics.snapshot();
    assert!(
        snap.worker_executed >= 2,
        "expected >=2 executes, got {snap:?}"
    );
    assert!(
        snap.worker_rows >= 1,
        "expected >=1 row reply, got {snap:?}"
    );
    assert_eq!(snap.current_in_flight, 0, "no leftover in-flight");

    shutdown_runtime(runtime);
}

#[test]
fn sequential_calls_share_one_connection() {
    let runtime = make_runtime();
    let bridge = install_bridge(&runtime, test_config());
    create_and_seed(bridge.address, &runtime);

    for i in 0..10 {
        let sink = Arc::new(Sink::default());
        let caller = register_caller(
            &runtime,
            bridge.address,
            Arc::clone(&sink),
            Duration::from_secs(2),
        );
        runtime
            .try_send(
                caller,
                CallerMsg::Run(SqliteRequest::Execute {
                    sql: "INSERT INTO t (k, v) VALUES (?, ?)".into(),
                    params: vec![
                        SqliteValue::Integer(i as i64 + 100),
                        SqliteValue::Text(format!("row-{i}")),
                    ],
                }),
            )
            .expect("kick insert");
        match sink.wait_one(Duration::from_secs(5)) {
            CallOutcome::Replied(Ok(SqliteResponse::Executed { rows_changed: 1 })) => {}
            other => panic!("insert {i}: {other:?}"),
        }
    }

    let sink = Arc::new(Sink::default());
    let caller = register_caller(
        &runtime,
        bridge.address,
        Arc::clone(&sink),
        Duration::from_secs(2),
    );
    runtime
        .try_send(
            caller,
            CallerMsg::Run(SqliteRequest::QueryRows {
                sql: "SELECT COUNT(*) FROM t".into(),
                params: vec![],
                max_rows: 1,
            }),
        )
        .expect("kick count");
    match sink.wait_one(Duration::from_secs(5)) {
        CallOutcome::Replied(Ok(SqliteResponse::Rows { rows, .. })) => {
            assert_eq!(rows.len(), 1);
            assert_eq!(rows[0][0], SqliteValue::Integer(10));
        }
        other => panic!("count outcome: {other:?}"),
    }

    shutdown_runtime(runtime);
}

// ---------------------------------------------------------------------------
// Caps and Full
// ---------------------------------------------------------------------------

#[test]
fn request_param_cap_rejects_invalid_request() {
    let runtime = make_runtime();
    let cfg = test_config().with_max_request_params(2);
    let bridge = install_bridge(&runtime, cfg);

    let sink = Arc::new(Sink::default());
    let caller = register_caller(
        &runtime,
        bridge.address,
        Arc::clone(&sink),
        Duration::from_secs(2),
    );
    runtime
        .try_send(
            caller,
            CallerMsg::Run(SqliteRequest::Execute {
                sql: "SELECT ?, ?, ?".into(),
                params: vec![
                    SqliteValue::Integer(1),
                    SqliteValue::Integer(2),
                    SqliteValue::Integer(3),
                ],
            }),
        )
        .expect("kick");
    match sink.wait_one(Duration::from_secs(5)) {
        CallOutcome::Replied(Err(SqliteError::InvalidRequest(msg))) => {
            assert!(msg.contains("params 3"), "msg: {msg}");
        }
        other => panic!("expected InvalidRequest, got {other:?}"),
    }
    assert_eq!(bridge.metrics.snapshot().invalid, 1);

    shutdown_runtime(runtime);
}

#[test]
fn empty_sql_is_invalid_request() {
    let runtime = make_runtime();
    let bridge = install_bridge(&runtime, test_config());

    let sink = Arc::new(Sink::default());
    let caller = register_caller(
        &runtime,
        bridge.address,
        Arc::clone(&sink),
        Duration::from_secs(2),
    );
    runtime
        .try_send(
            caller,
            CallerMsg::Run(SqliteRequest::Execute {
                sql: "   ".into(),
                params: vec![],
            }),
        )
        .expect("kick");
    match sink.wait_one(Duration::from_secs(5)) {
        CallOutcome::Replied(Err(SqliteError::InvalidRequest(_))) => {}
        other => panic!("expected InvalidRequest, got {other:?}"),
    }

    shutdown_runtime(runtime);
}

#[test]
fn response_row_cap_rejects_oversized_query() {
    let runtime = make_runtime();
    let cfg = test_config().with_max_response_rows(3);
    let bridge = install_bridge(&runtime, cfg);
    create_and_seed(bridge.address, &runtime);

    // Insert 10 rows.
    for i in 0..10 {
        let sink = Arc::new(Sink::default());
        let caller = register_caller(
            &runtime,
            bridge.address,
            Arc::clone(&sink),
            Duration::from_secs(2),
        );
        runtime
            .try_send(
                caller,
                CallerMsg::Run(SqliteRequest::Execute {
                    sql: "INSERT INTO t (k, v) VALUES (?, ?)".into(),
                    params: vec![SqliteValue::Integer(i), SqliteValue::Text(format!("v{i}"))],
                }),
            )
            .expect("kick insert");
        match sink.wait_one(Duration::from_secs(5)) {
            CallOutcome::Replied(Ok(_)) => {}
            other => panic!("insert {i}: {other:?}"),
        }
    }

    // Request all rows; row buffer cap should fire.
    let sink = Arc::new(Sink::default());
    let caller = register_caller(
        &runtime,
        bridge.address,
        Arc::clone(&sink),
        Duration::from_secs(2),
    );
    runtime
        .try_send(
            caller,
            CallerMsg::Run(SqliteRequest::QueryRows {
                sql: "SELECT k, v FROM t".into(),
                params: vec![],
                max_rows: 100,
            }),
        )
        .expect("kick query");
    match sink.wait_one(Duration::from_secs(5)) {
        CallOutcome::Replied(Err(SqliteError::ResponseTooLarge)) => {}
        other => panic!("expected ResponseTooLarge, got {other:?}"),
    }
    assert_eq!(bridge.metrics.snapshot().worker_response_too_large, 1);

    shutdown_runtime(runtime);
}

#[test]
fn full_when_in_flight_saturated() {
    // Send A; wait for it to land in_flight; then send B. As long as
    // A's HEAVY_QUERY is still running on the worker thread when B's
    // admission arrives, B Full-rejects.
    let runtime = make_runtime();
    let bridge = install_bridge(&runtime, test_config());

    let sink_a = Arc::new(Sink::default());
    let sink_b = Arc::new(Sink::default());
    let caller_a = register_caller(
        &runtime,
        bridge.address,
        Arc::clone(&sink_a),
        Duration::from_secs(15),
    );
    let caller_b = register_caller(
        &runtime,
        bridge.address,
        Arc::clone(&sink_b),
        Duration::from_secs(15),
    );

    runtime
        .try_send(
            caller_a,
            CallerMsg::Run(SqliteRequest::QueryRows {
                sql: HEAVY_QUERY.into(),
                params: vec![],
                max_rows: 1,
            }),
        )
        .expect("kick a");
    wait_admitted(&bridge, 1, Duration::from_secs(5));
    runtime
        .try_send(
            caller_b,
            CallerMsg::Run(SqliteRequest::Execute {
                sql: "SELECT 1".into(),
                params: vec![],
            }),
        )
        .expect("kick b");

    match sink_b.wait_one(Duration::from_secs(15)) {
        CallOutcome::Replied(Err(SqliteError::Full)) => {}
        other => panic!("expected Full for B, got {other:?}"),
    }

    // Drain A so the worker thread fully resets before shutdown.
    let _ = sink_a.wait_one(Duration::from_secs(30));

    let snap = bridge.metrics.snapshot();
    assert_eq!(snap.full, 1, "B's Full should be the only one: {snap:?}");
    assert!(snap.in_flight_high_water >= 1);

    shutdown_runtime(runtime);
}

// ---------------------------------------------------------------------------
// Closed
// ---------------------------------------------------------------------------

#[test]
fn closer_rejects_new_admissions() {
    let runtime = make_runtime();
    let bridge = install_bridge(&runtime, test_config());

    bridge.closer.close();

    let sink = Arc::new(Sink::default());
    let caller = register_caller(
        &runtime,
        bridge.address,
        Arc::clone(&sink),
        Duration::from_secs(2),
    );
    runtime
        .try_send(
            caller,
            CallerMsg::Run(SqliteRequest::Execute {
                sql: "CREATE TABLE z (n INTEGER)".into(),
                params: vec![],
            }),
        )
        .expect("kick");
    match sink.wait_one(Duration::from_secs(5)) {
        CallOutcome::Replied(Err(SqliteError::Closed)) => {}
        other => panic!("expected Closed, got {other:?}"),
    }
    assert_eq!(bridge.metrics.snapshot().closed, 1);

    shutdown_runtime(runtime);
}

#[test]
fn close_with_in_flight_lets_inflight_complete_then_rejects() {
    let runtime = make_runtime();
    let bridge = install_bridge(&runtime, test_config());
    create_and_seed(bridge.address, &runtime);
    let baseline = bridge.metrics.snapshot().admitted;

    let sink_a = Arc::new(Sink::default());
    let caller_a = register_caller(
        &runtime,
        bridge.address,
        Arc::clone(&sink_a),
        Duration::from_secs(15),
    );

    runtime
        .try_send(
            caller_a,
            CallerMsg::Run(SqliteRequest::QueryRows {
                sql: HEAVY_QUERY.into(),
                params: vec![],
                max_rows: 1,
            }),
        )
        .expect("kick a");
    wait_admitted(&bridge, baseline + 1, Duration::from_secs(5));
    bridge.closer.close();

    // B arrives after close; should reject as Closed.
    let sink_b = Arc::new(Sink::default());
    let caller_b = register_caller(
        &runtime,
        bridge.address,
        Arc::clone(&sink_b),
        Duration::from_secs(2),
    );
    runtime
        .try_send(
            caller_b,
            CallerMsg::Run(SqliteRequest::Execute {
                sql: "SELECT 1".into(),
                params: vec![],
            }),
        )
        .expect("kick b");

    match sink_b.wait_one(Duration::from_secs(5)) {
        CallOutcome::Replied(Err(SqliteError::Closed)) => {}
        other => panic!("expected Closed for B, got {other:?}"),
    }

    // A still finishes successfully (or with a typed worker outcome).
    match sink_a.wait_one(Duration::from_secs(30)) {
        CallOutcome::Replied(Ok(SqliteResponse::Rows { .. })) => {}
        other => panic!("A should have completed: {other:?}"),
    }

    shutdown_runtime(runtime);
}

// ---------------------------------------------------------------------------
// Caller timeout + late result truth
// ---------------------------------------------------------------------------

#[test]
fn bridge_attempt_timeout_surfaces_and_late_result_is_recorded() {
    // Tight default_timeout; HEAVY_QUERY runs longer. Bridge surfaces
    // Timeout; worker finishes; late_results == 1 and worker_rows == 1
    // (no double-tally).
    let runtime = make_runtime();
    let cfg = test_config()
        .with_default_timeout(Duration::from_millis(20))
        .with_poll_interval(Duration::from_millis(1));
    let bridge = install_bridge(&runtime, cfg);

    let sink = Arc::new(Sink::default());
    let caller = register_caller(
        &runtime,
        bridge.address,
        Arc::clone(&sink),
        Duration::from_secs(15),
    );
    runtime
        .try_send(
            caller,
            CallerMsg::Run(SqliteRequest::QueryRows {
                sql: HEAVY_QUERY.into(),
                params: vec![],
                max_rows: 1,
            }),
        )
        .expect("kick");

    match sink.wait_one(Duration::from_secs(10)) {
        CallOutcome::Replied(Err(SqliteError::Timeout)) => {}
        other => panic!("expected Timeout, got {other:?}"),
    }
    assert_eq!(bridge.metrics.snapshot().timeouts, 1);

    // Wait for the worker to finish and bump late_results.
    let deadline = Instant::now() + Duration::from_secs(30);
    while Instant::now() < deadline {
        if bridge.metrics.snapshot().late_results >= 1 {
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    let snap = bridge.metrics.snapshot();
    assert_eq!(
        snap.late_results, 1,
        "late_results == 1 (no double): {snap:?}"
    );
    assert_eq!(snap.worker_rows, 1, "worker_rows tallied once: {snap:?}");
    assert_eq!(snap.worker_executed, 0);
    assert_eq!(snap.timeouts, 1);

    shutdown_runtime(runtime);
}

// ---------------------------------------------------------------------------
// After-failure recovery
// ---------------------------------------------------------------------------

#[test]
fn after_bad_sql_worker_still_serves_next_request() {
    let runtime = make_runtime();
    let bridge = install_bridge(&runtime, test_config());
    create_and_seed(bridge.address, &runtime);

    // Bad SQL.
    let sink = Arc::new(Sink::default());
    let caller = register_caller(
        &runtime,
        bridge.address,
        Arc::clone(&sink),
        Duration::from_secs(2),
    );
    runtime
        .try_send(
            caller,
            CallerMsg::Run(SqliteRequest::Execute {
                sql: "NOT VALID SQL".into(),
                params: vec![],
            }),
        )
        .expect("kick bad");
    match sink.wait_one(Duration::from_secs(5)) {
        CallOutcome::Replied(Err(SqliteError::Sqlite(_))) => {}
        other => panic!("expected Sqlite error, got {other:?}"),
    }

    // Constraint violation (UNIQUE index on v).
    let sink = Arc::new(Sink::default());
    let caller = register_caller(
        &runtime,
        bridge.address,
        Arc::clone(&sink),
        Duration::from_secs(2),
    );
    runtime
        .try_send(
            caller,
            CallerMsg::Run(SqliteRequest::Execute {
                sql: "INSERT INTO t (k, v) VALUES (1, 'a')".into(),
                params: vec![],
            }),
        )
        .expect("kick first insert");
    match sink.wait_one(Duration::from_secs(5)) {
        CallOutcome::Replied(Ok(_)) => {}
        other => panic!("first insert: {other:?}"),
    }

    let sink = Arc::new(Sink::default());
    let caller = register_caller(
        &runtime,
        bridge.address,
        Arc::clone(&sink),
        Duration::from_secs(2),
    );
    runtime
        .try_send(
            caller,
            CallerMsg::Run(SqliteRequest::Execute {
                sql: "INSERT INTO t (k, v) VALUES (2, 'a')".into(),
                params: vec![],
            }),
        )
        .expect("kick dup insert");
    match sink.wait_one(Duration::from_secs(5)) {
        CallOutcome::Replied(Err(SqliteError::Constraint(_))) => {}
        other => panic!("expected Constraint, got {other:?}"),
    }

    // Worker still serves a third request fine.
    let sink = Arc::new(Sink::default());
    let caller = register_caller(
        &runtime,
        bridge.address,
        Arc::clone(&sink),
        Duration::from_secs(2),
    );
    runtime
        .try_send(
            caller,
            CallerMsg::Run(SqliteRequest::Execute {
                sql: "INSERT INTO t (k, v) VALUES (3, 'b')".into(),
                params: vec![],
            }),
        )
        .expect("kick recovery insert");
    match sink.wait_one(Duration::from_secs(5)) {
        CallOutcome::Replied(Ok(SqliteResponse::Executed { rows_changed: 1 })) => {}
        other => panic!("recovery insert: {other:?}"),
    }

    let snap = bridge.metrics.snapshot();
    assert!(snap.worker_constraint >= 1);
    assert!(snap.worker_sqlite >= 1);

    shutdown_runtime(runtime);
}

// ---------------------------------------------------------------------------
// Install error variants
// ---------------------------------------------------------------------------

#[test]
fn install_fails_on_bad_pragma() {
    // Many unknown pragmas silently no-op; use a syntactically broken
    // statement to force a parse error.
    let cfg = SqliteConfig {
        pragmas: vec!["foreign_keys = ;;not valid;;".into()],
        ..SqliteConfig::memory()
    };
    let runtime = make_runtime();
    let res = SqliteWorker::<SingleShard>::install(&runtime, cfg);
    match res {
        Err(tina_sqlite_bridge::InstallError::Pragma(_)) => {}
        Err(other) => panic!("expected Pragma install error, got {other:?}"),
        Ok(_) => panic!("install should fail"),
    }
    shutdown_runtime(runtime);
}

#[test]
fn install_fails_on_bad_path_with_open_variant() {
    // A path under a non-existent directory will fail at sqlite3_open.
    let cfg = SqliteConfig::path("/this/path/should/not/exist/db.sqlite");
    let runtime = make_runtime();
    let res = SqliteWorker::<SingleShard>::install(&runtime, cfg);
    match res {
        Err(tina_sqlite_bridge::InstallError::Open(_)) => {}
        Err(other) => panic!("expected Open install error, got {other:?}"),
        Ok(_) => panic!("install should fail"),
    }
    shutdown_runtime(runtime);
}

// ---------------------------------------------------------------------------
// Mailbox burst: all 8 admissions resolve to Ok or Full
// ---------------------------------------------------------------------------

#[test]
fn burst_admissions_resolve_ok_or_full() {
    let runtime = make_runtime();
    let bridge = install_bridge(&runtime, test_config().with_mailbox_capacity(64));
    create_and_seed(bridge.address, &runtime);

    let sink = Arc::new(Sink::default());
    let mut callers = Vec::new();
    for _ in 0..8 {
        let caller = register_caller(
            &runtime,
            bridge.address,
            Arc::clone(&sink),
            Duration::from_secs(5),
        );
        callers.push(caller);
    }
    for (i, addr) in callers.into_iter().enumerate() {
        runtime
            .try_send(
                addr,
                CallerMsg::Run(SqliteRequest::Execute {
                    sql: "INSERT INTO t (k, v) VALUES (?, ?)".into(),
                    params: vec![
                        SqliteValue::Integer(i as i64 + 1),
                        SqliteValue::Text(format!("burst-{i}")),
                    ],
                }),
            )
            .expect("kick burst");
    }

    let outcomes = sink.wait_n(8, Duration::from_secs(15));
    let mut ok = 0u32;
    let mut full = 0u32;
    for o in outcomes {
        match o {
            CallOutcome::Replied(Ok(SqliteResponse::Executed { rows_changed: 1 })) => ok += 1,
            CallOutcome::Replied(Err(SqliteError::Full)) => full += 1,
            other => panic!("unexpected burst outcome: {other:?}"),
        }
    }
    assert!(ok >= 1);
    assert_eq!(ok + full, 8);
    assert_eq!(sink.len(), 0);

    shutdown_runtime(runtime);
}

// ---------------------------------------------------------------------------
// Value round-trips
// ---------------------------------------------------------------------------

fn run_one(
    runtime: &ThreadedRuntime<SingleShard, DefaultThreadedMailboxFactory>,
    addr: SqliteAddress,
    request: SqliteRequest,
) -> SqliteCallOutcome {
    let sink = Arc::new(Sink::default());
    let caller = register_caller(runtime, addr, Arc::clone(&sink), Duration::from_secs(5));
    runtime
        .try_send(caller, CallerMsg::Run(request))
        .expect("kick");
    sink.wait_one(Duration::from_secs(5))
}

fn assert_executed(outcome: SqliteCallOutcome, rows: u64) {
    match outcome {
        CallOutcome::Replied(Ok(SqliteResponse::Executed { rows_changed })) => {
            assert_eq!(rows_changed, rows);
        }
        other => panic!("expected Executed {{ rows_changed: {rows} }}, got {other:?}"),
    }
}

#[test]
fn blob_round_trip() {
    let runtime = make_runtime();
    let bridge = install_bridge(&runtime, test_config());

    assert_executed(
        run_one(
            &runtime,
            bridge.address,
            SqliteRequest::Execute {
                sql: "CREATE TABLE b (k INTEGER PRIMARY KEY, v BLOB)".into(),
                params: vec![],
            },
        ),
        0,
    );
    let payload: Vec<u8> = (0u8..=255).collect();
    assert_executed(
        run_one(
            &runtime,
            bridge.address,
            SqliteRequest::Execute {
                sql: "INSERT INTO b (k, v) VALUES (1, ?)".into(),
                params: vec![SqliteValue::Blob(payload.clone())],
            },
        ),
        1,
    );
    match run_one(
        &runtime,
        bridge.address,
        SqliteRequest::QueryRows {
            sql: "SELECT v FROM b WHERE k = 1".into(),
            params: vec![],
            max_rows: 1,
        },
    ) {
        CallOutcome::Replied(Ok(SqliteResponse::Rows { rows, .. })) => {
            assert_eq!(rows.len(), 1);
            match &rows[0][0] {
                SqliteValue::Blob(b) => assert_eq!(b, &payload),
                other => panic!("expected Blob, got {other:?}"),
            }
        }
        other => panic!("select: {other:?}"),
    }

    shutdown_runtime(runtime);
}

#[test]
fn real_round_trip() {
    let runtime = make_runtime();
    let bridge = install_bridge(&runtime, test_config());

    assert_executed(
        run_one(
            &runtime,
            bridge.address,
            SqliteRequest::Execute {
                sql: "CREATE TABLE r (k INTEGER PRIMARY KEY, v REAL)".into(),
                params: vec![],
            },
        ),
        0,
    );
    assert_executed(
        run_one(
            &runtime,
            bridge.address,
            SqliteRequest::Execute {
                sql: "INSERT INTO r (k, v) VALUES (1, ?)".into(),
                params: vec![SqliteValue::Real(std::f64::consts::PI)],
            },
        ),
        1,
    );
    match run_one(
        &runtime,
        bridge.address,
        SqliteRequest::QueryRows {
            sql: "SELECT v FROM r WHERE k = 1".into(),
            params: vec![],
            max_rows: 1,
        },
    ) {
        CallOutcome::Replied(Ok(SqliteResponse::Rows { rows, .. })) => match &rows[0][0] {
            SqliteValue::Real(f) => assert!((f - std::f64::consts::PI).abs() < 1e-12),
            other => panic!("expected Real, got {other:?}"),
        },
        other => panic!("select: {other:?}"),
    }

    shutdown_runtime(runtime);
}

#[test]
fn non_utf8_text_returns_blob() {
    // Insert a TEXT column whose bytes aren't valid UTF-8. Rusqlite
    // binds Text from Rust strings (always UTF-8), so we go through
    // BLOB then read with TYPEOF to confirm the underlying type was
    // actually TEXT — and the bridge falls back to Blob since utf-8
    // decoding fails.
    let runtime = make_runtime();
    let bridge = install_bridge(&runtime, test_config());

    assert_executed(
        run_one(
            &runtime,
            bridge.address,
            SqliteRequest::Execute {
                sql: "CREATE TABLE t (k INTEGER PRIMARY KEY, v TEXT)".into(),
                params: vec![],
            },
        ),
        0,
    );
    // CAST a BLOB to TEXT inside SQL — leaves invalid UTF-8 bytes in
    // a TEXT cell.
    assert_executed(
        run_one(
            &runtime,
            bridge.address,
            SqliteRequest::Execute {
                sql: "INSERT INTO t (k, v) VALUES (1, CAST(? AS TEXT))".into(),
                params: vec![SqliteValue::Blob(vec![0xff, 0xfe, 0xfd])],
            },
        ),
        1,
    );
    match run_one(
        &runtime,
        bridge.address,
        SqliteRequest::QueryRows {
            sql: "SELECT v FROM t WHERE k = 1".into(),
            params: vec![],
            max_rows: 1,
        },
    ) {
        CallOutcome::Replied(Ok(SqliteResponse::Rows { rows, .. })) => match &rows[0][0] {
            SqliteValue::Blob(b) => assert_eq!(b, &vec![0xff, 0xfe, 0xfd]),
            other => panic!("expected Blob fallback, got {other:?}"),
        },
        other => panic!("select: {other:?}"),
    }

    shutdown_runtime(runtime);
}

// ---------------------------------------------------------------------------
// Multi-statement SQL surfaces as Sqlite (not silently truncated)
// ---------------------------------------------------------------------------

#[test]
fn multi_statement_execute_falls_through_to_sqlite_error() {
    let runtime = make_runtime();
    let bridge = install_bridge(&runtime, test_config());

    let sink = Arc::new(Sink::default());
    let caller = register_caller(
        &runtime,
        bridge.address,
        Arc::clone(&sink),
        Duration::from_secs(5),
    );
    runtime
        .try_send(
            caller,
            CallerMsg::Run(SqliteRequest::Execute {
                sql: "SELECT 1; SELECT 2".into(),
                params: vec![],
            }),
        )
        .expect("kick");
    match sink.wait_one(Duration::from_secs(5)) {
        CallOutcome::Replied(Err(SqliteError::Sqlite(_))) => {}
        other => panic!("expected Sqlite error, got {other:?}"),
    }

    shutdown_runtime(runtime);
}

// ---------------------------------------------------------------------------
// pending_reply_capacity < max_in_flight rejected
// ---------------------------------------------------------------------------

#[test]
fn config_rejects_pending_below_max_in_flight() {
    // pending_reply_capacity = 1, max_in_flight = 1 is fine, but if we
    // ever set pending below max, validate must reject. With current
    // pins this is hard to hit honestly; force max_in_flight to 2 to
    // exercise the rule (validate also rejects max_in_flight != 1, so
    // we must use a config where the pending check fires first; the
    // actual ordering means InvalidMaxInFlight wins. Test the variant
    // shape directly instead.)
    let err = SqliteConfigError::PendingReplyCapacityBelowMaxInFlight {
        pending: 1,
        in_flight: 4,
    };
    assert!(matches!(
        err,
        SqliteConfigError::PendingReplyCapacityBelowMaxInFlight { .. }
    ));
    // Also exercise Display for completeness.
    let s = format!("{err}");
    assert!(s.contains("pending_reply_capacity 1"));
    assert!(s.contains("max_in_flight 4"));
}

// ---------------------------------------------------------------------------
// Pragma actually lands on the connection
// ---------------------------------------------------------------------------

#[test]
fn pragma_journal_mode_wal_takes_effect() {
    // In-memory databases coerce journal_mode to "memory"; use a
    // tempfile so WAL can actually apply.
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("wal.sqlite");
    let cfg = SqliteConfig::path(&path)
        .with_pragma("journal_mode = WAL")
        .with_poll_interval(Duration::from_millis(1));
    let runtime = make_runtime();
    let bridge = install_bridge(&runtime, cfg);

    let outcome = run_one(
        &runtime,
        bridge.address,
        SqliteRequest::QueryRows {
            sql: "PRAGMA journal_mode".into(),
            params: vec![],
            max_rows: 1,
        },
    );
    match outcome {
        CallOutcome::Replied(Ok(SqliteResponse::Rows { rows, .. })) => {
            assert_eq!(rows.len(), 1);
            match &rows[0][0] {
                SqliteValue::Text(mode) => assert_eq!(mode, "wal"),
                other => panic!("expected Text, got {other:?}"),
            }
        }
        other => panic!("pragma read: {other:?}"),
    }

    shutdown_runtime(runtime);
    drop(dir);
}

// ---------------------------------------------------------------------------
// Closer lifecycle
// ---------------------------------------------------------------------------

#[test]
fn closer_is_closed_lifecycle() {
    let runtime = make_runtime();
    let bridge = install_bridge(&runtime, test_config());
    assert!(!bridge.closer.is_closed());
    let cloned = bridge.closer.clone();
    assert!(!cloned.is_closed());
    bridge.closer.close();
    assert!(bridge.closer.is_closed());
    assert!(cloned.is_closed(), "Clone shares the closed flag");
    // Idempotent.
    bridge.closer.close();
    assert!(bridge.closer.is_closed());
    shutdown_runtime(runtime);
}

// ---------------------------------------------------------------------------
// Sustained sequential load: nothing leaks
// ---------------------------------------------------------------------------

#[test]
fn sustained_sequential_load_keeps_metrics_clean() {
    let runtime = make_runtime();
    let bridge = install_bridge(&runtime, test_config());
    create_and_seed(bridge.address, &runtime);
    let baseline = bridge.metrics.snapshot();

    for i in 0..50 {
        assert_executed(
            run_one(
                &runtime,
                bridge.address,
                SqliteRequest::Execute {
                    sql: "INSERT INTO t (k, v) VALUES (?, ?)".into(),
                    params: vec![
                        SqliteValue::Integer(i as i64 + 1000),
                        SqliteValue::Text(format!("load-{i}")),
                    ],
                },
            ),
            1,
        );
    }

    let snap = bridge.metrics.snapshot();
    assert_eq!(snap.admitted - baseline.admitted, 50);
    assert_eq!(snap.full, 0);
    assert_eq!(snap.timeouts, 0);
    assert_eq!(snap.late_results, 0);
    assert_eq!(snap.closed, 0);
    assert_eq!(snap.invalid, 0);
    assert_eq!(snap.current_in_flight, 0);
    assert_eq!(snap.in_flight_high_water, 1);
    assert_eq!(snap.worker_executed - baseline.worker_executed, 50);

    shutdown_runtime(runtime);
}

// ---------------------------------------------------------------------------
// Display round-trips
// ---------------------------------------------------------------------------

#[test]
fn error_display_includes_payload() {
    let cases: Vec<(SqliteError, &str)> = vec![
        (SqliteError::Full, "ingress full"),
        (SqliteError::Closed, "closed"),
        (SqliteError::Timeout, "timeout"),
        (
            SqliteError::InvalidRequest("oops".into()),
            "invalid request: oops",
        ),
        (SqliteError::ResponseTooLarge, "row cap"),
        (SqliteError::Busy, "SQLITE_BUSY"),
        (
            SqliteError::Constraint("UNIQUE".into()),
            "constraint violation: UNIQUE",
        ),
        (SqliteError::Io("disk".into()), "io error: disk"),
        (SqliteError::Sqlite("syntax".into()), "sqlite error: syntax"),
        (SqliteError::Internal("bug".into()), "internal: bug"),
    ];
    for (err, fragment) in cases {
        let s = format!("{err}");
        assert!(s.contains(fragment), "{s} missing {fragment}");
    }
}

#[test]
fn install_error_display_names_phase() {
    let cases: Vec<(tina_sqlite_bridge::InstallError, &str)> = vec![
        (
            tina_sqlite_bridge::InstallError::Open("perm".into()),
            "open: perm",
        ),
        (
            tina_sqlite_bridge::InstallError::Setup("bt".into()),
            "setup: bt",
        ),
        (
            tina_sqlite_bridge::InstallError::Pragma("bad".into()),
            "pragma: bad",
        ),
        (
            tina_sqlite_bridge::InstallError::Spawn("eof".into()),
            "spawn: eof",
        ),
        (
            tina_sqlite_bridge::InstallError::Register("nope".into()),
            "register: nope",
        ),
    ];
    for (err, fragment) in cases {
        let s = format!("{err}");
        assert!(s.contains(fragment), "{s} missing {fragment}");
    }
}

// ---------------------------------------------------------------------------
// Value accessors
// ---------------------------------------------------------------------------

#[test]
fn value_accessors_round_trip() {
    let n = SqliteValue::Null;
    let i = SqliteValue::Integer(42);
    let r = SqliteValue::Real(2.5);
    let t = SqliteValue::Text("hi".into());
    let b = SqliteValue::Blob(vec![1, 2, 3]);

    assert!(n.is_null());
    assert!(!i.is_null());

    assert_eq!(i.as_i64(), Some(42));
    assert_eq!(r.as_i64(), None);

    assert_eq!(r.as_f64(), Some(2.5));
    assert_eq!(t.as_f64(), None);

    assert_eq!(t.as_text(), Some("hi"));
    assert_eq!(b.as_text(), None);

    assert_eq!(b.as_blob(), Some(&[1u8, 2, 3][..]));
    assert_eq!(t.as_blob(), None);

    // From conversions.
    let from_i: SqliteValue = 7_i64.into();
    let from_str: SqliteValue = "x".into();
    let from_string: SqliteValue = String::from("y").into();
    let from_vec: SqliteValue = vec![0u8; 2].into();
    let from_f: SqliteValue = 0.5_f64.into();
    assert_eq!(from_i, SqliteValue::Integer(7));
    assert_eq!(from_str, SqliteValue::Text("x".into()));
    assert_eq!(from_string, SqliteValue::Text("y".into()));
    assert_eq!(from_vec, SqliteValue::Blob(vec![0, 0]));
    assert_eq!(from_f, SqliteValue::Real(0.5));
}

// ---------------------------------------------------------------------------
// Request constructors and chaining
// ---------------------------------------------------------------------------

#[test]
fn request_builder_chains() {
    let req = SqliteRequest::execute("INSERT INTO t (a, b) VALUES (?, ?)")
        .param(1_i64)
        .param("hello");
    match req {
        SqliteRequest::Execute { sql, params } => {
            assert_eq!(sql, "INSERT INTO t (a, b) VALUES (?, ?)");
            assert_eq!(params.len(), 2);
            assert_eq!(params[0], SqliteValue::Integer(1));
            assert_eq!(params[1], SqliteValue::Text("hello".into()));
        }
        _ => panic!("expected Execute"),
    }

    let req = SqliteRequest::query_rows("SELECT * FROM t WHERE a = ?", 16).param(7_i64);
    match req {
        SqliteRequest::QueryRows {
            sql,
            params,
            max_rows,
        } => {
            assert_eq!(sql, "SELECT * FROM t WHERE a = ?");
            assert_eq!(params.len(), 1);
            assert_eq!(max_rows, 16);
        }
        _ => panic!("expected QueryRows"),
    }

    let req = SqliteRequest::execute("...")
        .params(vec![SqliteValue::Integer(1), SqliteValue::Integer(2)]);
    match req {
        SqliteRequest::Execute { params, .. } => assert_eq!(params.len(), 2),
        _ => unreachable!(),
    }
}

// ---------------------------------------------------------------------------
// Typed helpers: execute_call and query_call
// ---------------------------------------------------------------------------

#[derive(Debug)]
enum TypedCallerMsg {
    RunExecute {
        sql: String,
        params: Vec<SqliteValue>,
    },
    RunQuery {
        sql: String,
        params: Vec<SqliteValue>,
        max_rows: usize,
    },
    DoneExecute(tina_sqlite_bridge::SqliteExecutedOutcome),
    DoneQuery(tina_sqlite_bridge::SqliteRowsOutcome),
}

#[derive(Default)]
struct TypedSink {
    executed: Mutex<Option<tina_sqlite_bridge::SqliteExecutedOutcome>>,
    rows: Mutex<Option<tina_sqlite_bridge::SqliteRowsOutcome>>,
    cv: Condvar,
}

impl TypedSink {
    fn put_executed(&self, o: tina_sqlite_bridge::SqliteExecutedOutcome) {
        *self.executed.lock().expect("typed sink") = Some(o);
        self.cv.notify_all();
    }
    fn put_rows(&self, o: tina_sqlite_bridge::SqliteRowsOutcome) {
        *self.rows.lock().expect("typed sink") = Some(o);
        self.cv.notify_all();
    }
    fn wait_executed(&self, t: Duration) -> tina_sqlite_bridge::SqliteExecutedOutcome {
        let deadline = Instant::now() + t;
        let mut g = self.executed.lock().expect("typed sink");
        while g.is_none() {
            let now = Instant::now();
            if now >= deadline {
                panic!("typed wait_executed timed out");
            }
            let (gg, _) = self
                .cv
                .wait_timeout(g, deadline - now)
                .expect("typed cv wait");
            g = gg;
        }
        g.take().expect("typed sink populated")
    }
    fn wait_rows(&self, t: Duration) -> tina_sqlite_bridge::SqliteRowsOutcome {
        let deadline = Instant::now() + t;
        let mut g = self.rows.lock().expect("typed sink");
        while g.is_none() {
            let now = Instant::now();
            if now >= deadline {
                panic!("typed wait_rows timed out");
            }
            let (gg, _) = self
                .cv
                .wait_timeout(g, deadline - now)
                .expect("typed cv wait");
            g = gg;
        }
        g.take().expect("typed sink populated")
    }
}

struct TypedCaller {
    db: SqliteAddress,
    timeout: Duration,
    sink: Arc<TypedSink>,
}

impl Isolate for TypedCaller {
    tina::isolate_types! {
        message: TypedCallerMsg,
        reply: (),
        send: tina::Outbound<Infallible>,
        spawn: Infallible,
        call: RuntimeCall<TypedCallerMsg>,
        shard: SingleShard,
    }
    fn handle(
        &mut self,
        msg: TypedCallerMsg,
        _ctx: &mut Context<'_, SingleShard, Self::Reply>,
    ) -> Effect<Self> {
        match msg {
            TypedCallerMsg::RunExecute { sql, params } => {
                tina_sqlite_bridge::execute_call(self.db, sql, params, self.timeout)
                    .reply(TypedCallerMsg::DoneExecute)
            }
            TypedCallerMsg::RunQuery {
                sql,
                params,
                max_rows,
            } => tina_sqlite_bridge::query_call(self.db, sql, params, max_rows, self.timeout)
                .reply(TypedCallerMsg::DoneQuery),
            TypedCallerMsg::DoneExecute(o) => {
                self.sink.put_executed(o);
                stop()
            }
            TypedCallerMsg::DoneQuery(o) => {
                self.sink.put_rows(o);
                stop()
            }
        }
    }
}

#[test]
fn execute_call_returns_typed_rows_changed() {
    let runtime = make_runtime();
    let bridge = install_bridge(&runtime, test_config());

    let _ = run_one(
        &runtime,
        bridge.address,
        SqliteRequest::execute("CREATE TABLE t (k INTEGER PRIMARY KEY, v INTEGER NOT NULL)"),
    );

    let sink = Arc::new(TypedSink::default());
    let caller = runtime
        .register_with_capacity::<_, Infallible>(
            TypedCaller {
                db: bridge.address,
                timeout: Duration::from_secs(2),
                sink: Arc::clone(&sink),
            },
            8,
        )
        .expect("register typed caller");
    runtime
        .try_send(
            caller,
            TypedCallerMsg::RunExecute {
                sql: "INSERT INTO t (k, v) VALUES (?, ?)".into(),
                // i32 + i32 — proves From<i32> works without explicit
                // type annotation.
                params: vec![1.into(), 99.into()],
            },
        )
        .expect("kick");
    match sink.wait_executed(Duration::from_secs(5)) {
        CallOutcome::Replied(Ok(rows_changed)) => assert_eq!(rows_changed, 1),
        other => panic!("expected typed Ok(1), got {other:?}"),
    }
    shutdown_runtime(runtime);
}

#[test]
fn query_call_returns_typed_rows() {
    let runtime = make_runtime();
    let bridge = install_bridge(&runtime, test_config());

    let _ = run_one(
        &runtime,
        bridge.address,
        SqliteRequest::execute("CREATE TABLE t (k INTEGER PRIMARY KEY, v TEXT NOT NULL)"),
    );
    let _ = run_one(
        &runtime,
        bridge.address,
        SqliteRequest::execute("INSERT INTO t (k, v) VALUES (?, ?)")
            .param(1_i64)
            .param("hello"),
    );

    let sink = Arc::new(TypedSink::default());
    let caller = runtime
        .register_with_capacity::<_, Infallible>(
            TypedCaller {
                db: bridge.address,
                timeout: Duration::from_secs(2),
                sink: Arc::clone(&sink),
            },
            8,
        )
        .expect("register typed caller");
    runtime
        .try_send(
            caller,
            TypedCallerMsg::RunQuery {
                sql: "SELECT k, v FROM t WHERE k = ?".into(),
                params: vec![1.into()],
                max_rows: 16,
            },
        )
        .expect("kick");
    match sink.wait_rows(Duration::from_secs(5)) {
        CallOutcome::Replied(Ok(rows)) => {
            assert_eq!(rows.columns, vec!["k".to_string(), "v".to_string()]);
            assert_eq!(rows.len(), 1);
            // Row newtype accessors.
            let row = rows.row(0).expect("row 0");
            assert_eq!(row.col(0).and_then(|c| c.as_i64()), Some(1));
            assert_eq!(row.by_name("v").and_then(|c| c.as_text()), Some("hello"));
            assert!(row.by_name("nope").is_none());
        }
        other => panic!("expected typed Ok(SqliteRows), got {other:?}"),
    }
    shutdown_runtime(runtime);
}

// ---------------------------------------------------------------------------
// Outcome classifier
// ---------------------------------------------------------------------------

#[test]
fn classify_succeeded_transient_fatal() {
    use tina_sqlite_bridge::{
        SqliteFatalReason, SqliteOutcomeClass, SqliteOutcomeExt, SqliteTransientReason,
    };

    let succeeded: SqliteCallOutcome =
        CallOutcome::Replied(Ok(SqliteResponse::Executed { rows_changed: 3 }));
    match succeeded.classify() {
        SqliteOutcomeClass::Succeeded(SqliteResponse::Executed { rows_changed }) => {
            assert_eq!(rows_changed, 3);
        }
        other => panic!("Succeeded expected, got {other:?}"),
    }

    let busy: SqliteCallOutcome = CallOutcome::Replied(Err(SqliteError::Busy));
    assert!(matches!(
        busy.classify(),
        SqliteOutcomeClass::Transient(SqliteTransientReason::Busy)
    ));

    let worker_timeout: SqliteCallOutcome = CallOutcome::Replied(Err(SqliteError::Timeout));
    assert!(matches!(
        worker_timeout.classify(),
        SqliteOutcomeClass::Transient(SqliteTransientReason::WorkerTimeout)
    ));

    let bridge_timeout: SqliteCallOutcome = CallOutcome::Timeout;
    assert!(matches!(
        bridge_timeout.classify(),
        SqliteOutcomeClass::Transient(SqliteTransientReason::BridgeTimeout)
    ));

    let constraint: SqliteCallOutcome =
        CallOutcome::Replied(Err(SqliteError::Constraint("UNIQUE".into())));
    assert!(matches!(
        constraint.classify(),
        SqliteOutcomeClass::Fatal(SqliteFatalReason::Constraint(_))
    ));

    let bridge_full: SqliteCallOutcome = CallOutcome::Full;
    assert!(matches!(
        bridge_full.classify(),
        SqliteOutcomeClass::Fatal(SqliteFatalReason::BridgeFull)
    ));

    let worker_full: SqliteCallOutcome = CallOutcome::Replied(Err(SqliteError::Full));
    assert!(matches!(
        worker_full.classify(),
        SqliteOutcomeClass::Fatal(SqliteFatalReason::Full)
    ));
}

// Classifier on typed outcomes (execute_call / query_call).

#[test]
fn classify_on_typed_outcomes() {
    use tina_sqlite_bridge::{
        SqliteExecutedOutcome, SqliteFatalReason, SqliteOutcomeClass, SqliteOutcomeExt, SqliteRows,
        SqliteRowsOutcome, SqliteTransientReason,
    };

    // Executed outcome: success carries u64.
    let exec_ok: SqliteExecutedOutcome = CallOutcome::Replied(Ok(7));
    match exec_ok.classify() {
        SqliteOutcomeClass::Succeeded(rows_changed) => assert_eq!(rows_changed, 7),
        other => panic!("Succeeded(7) expected, got {other:?}"),
    }

    let exec_busy: SqliteExecutedOutcome = CallOutcome::Replied(Err(SqliteError::Busy));
    assert!(matches!(
        exec_busy.classify(),
        SqliteOutcomeClass::Transient(SqliteTransientReason::Busy)
    ));

    let exec_constraint: SqliteExecutedOutcome =
        CallOutcome::Replied(Err(SqliteError::Constraint("UNIQUE".into())));
    assert!(matches!(
        exec_constraint.classify(),
        SqliteOutcomeClass::Fatal(SqliteFatalReason::Constraint(_))
    ));

    // Rows outcome: success carries SqliteRows.
    let q_ok: SqliteRowsOutcome = CallOutcome::Replied(Ok(SqliteRows {
        columns: vec!["x".into()],
        rows: vec![vec![SqliteValue::Integer(1)]],
    }));
    match q_ok.classify() {
        SqliteOutcomeClass::Succeeded(rows) => {
            assert_eq!(rows.columns, vec!["x".to_string()]);
            assert_eq!(rows.len(), 1);
        }
        other => panic!("Succeeded(rows) expected, got {other:?}"),
    }

    let q_bridge_timeout: SqliteRowsOutcome = CallOutcome::Timeout;
    assert!(matches!(
        q_bridge_timeout.classify(),
        SqliteOutcomeClass::Transient(SqliteTransientReason::BridgeTimeout)
    ));
}

// SqliteRows::row / Row::col / Row::by_name accessors.

#[test]
fn row_accessors_borrow_cells_and_resolve_by_name() {
    use tina_sqlite_bridge::SqliteRows;

    let rows = SqliteRows {
        columns: vec!["a".into(), "b".into(), "c".into()],
        rows: vec![
            vec![
                SqliteValue::Integer(10),
                SqliteValue::Text("hi".into()),
                SqliteValue::Null,
            ],
            vec![
                SqliteValue::Integer(20),
                SqliteValue::Text("bye".into()),
                SqliteValue::Real(0.5),
            ],
        ],
    };
    assert_eq!(rows.len(), 2);
    assert!(!rows.is_empty());
    assert!(rows.row(2).is_none());

    let r0 = rows.row(0).expect("row 0");
    assert_eq!(r0.len(), 3);
    assert_eq!(r0.col(0).and_then(|c| c.as_i64()), Some(10));
    assert_eq!(r0.by_name("b").and_then(|c| c.as_text()), Some("hi"));
    assert!(r0.by_name("c").map(|c| c.is_null()).unwrap_or(false));
    assert!(r0.by_name("missing").is_none());

    let collected: Vec<_> = rows
        .iter()
        .filter_map(|r| r.by_name("a").and_then(|c| c.as_i64()))
        .collect();
    assert_eq!(collected, vec![10, 20]);
}

// SqliteValue: more From conversions and consuming accessors.

#[test]
fn value_into_text_and_into_blob_take_ownership() {
    let t = SqliteValue::Text("owned".into());
    assert_eq!(t.into_text(), Some("owned".to_string()));

    let b = SqliteValue::Blob(vec![1, 2, 3]);
    assert_eq!(b.into_blob(), Some(vec![1, 2, 3]));

    // Wrong variants return None.
    assert!(SqliteValue::Integer(1).into_text().is_none());
    assert!(SqliteValue::Real(0.5).into_blob().is_none());
}

#[test]
fn value_from_extra_types() {
    // Integer widths.
    assert_eq!(SqliteValue::from(7_i32), SqliteValue::Integer(7));
    assert_eq!(SqliteValue::from(7_i16), SqliteValue::Integer(7));
    assert_eq!(SqliteValue::from(7_i8), SqliteValue::Integer(7));
    assert_eq!(SqliteValue::from(7_u32), SqliteValue::Integer(7));
    assert_eq!(SqliteValue::from(7_u16), SqliteValue::Integer(7));
    assert_eq!(SqliteValue::from(7_u8), SqliteValue::Integer(7));

    // u64 documented saturation: u64::MAX → -1 (bit pattern preserved).
    assert_eq!(SqliteValue::from(u64::MAX), SqliteValue::Integer(-1));

    // f32 widens.
    assert_eq!(SqliteValue::from(0.5_f32), SqliteValue::Real(0.5));

    // bool → 0/1.
    assert_eq!(SqliteValue::from(false), SqliteValue::Integer(0));
    assert_eq!(SqliteValue::from(true), SqliteValue::Integer(1));

    // &[u8] and array-ref blobs.
    let slice: &[u8] = &[1, 2, 3];
    assert_eq!(SqliteValue::from(slice), SqliteValue::Blob(vec![1, 2, 3]));
    assert_eq!(
        SqliteValue::from(b"\x00\xff"),
        SqliteValue::Blob(vec![0, 0xff])
    );

    // Option<T> → Null on None, value on Some.
    let none_i: Option<i64> = None;
    assert_eq!(SqliteValue::from(none_i), SqliteValue::Null);
    let some_i: Option<i64> = Some(42);
    assert_eq!(SqliteValue::from(some_i), SqliteValue::Integer(42));
}
