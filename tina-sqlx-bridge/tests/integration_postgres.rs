//! Integration tests against a real Postgres.
//!
//! Each test is `#[ignore]` and reads `DATABASE_URL` from the
//! environment. CI does not need real credentials. Run locally with:
//!
//! ```sh
//! DATABASE_URL=postgres://postgres@127.0.0.1:5432/postgres \
//!     cargo test -p tina-sqlx-bridge --test integration_postgres -- --ignored
//! ```
//!
//! Each test creates and drops its own `bridge_test_<id>` table to
//! avoid conflicts when the suite runs against a live database with
//! other consumers.

use std::convert::Infallible;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use tina::prelude::*;
use tina_runtime::{
    CallOutcome, DefaultThreadedMailboxFactory, RuntimeCall, ThreadedRuntime, ThreadedRuntimeConfig,
};
use tina_sqlx_bridge::{
    InstalledPgBridge, PgAddress, PgCallOutcome, PgConfig, PgError, PgPoolConfig, PgRequest,
    PgResponse, PgRows, PgStep, PgStepOk, PgTransactionOutcome, PgValue, PgWorker, send_request,
};

fn database_url() -> Option<String> {
    std::env::var("DATABASE_URL").ok()
}

#[derive(Default)]
struct Sink {
    state: Mutex<Vec<PgCallOutcome>>,
    cv: Condvar,
}

impl Sink {
    fn put(&self, outcome: PgCallOutcome) {
        self.state.lock().expect("sink lock").push(outcome);
        self.cv.notify_all();
    }

    fn wait_one(&self, timeout: Duration) -> PgCallOutcome {
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

    fn wait_n(&self, n: usize, timeout: Duration) -> Vec<PgCallOutcome> {
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
}

#[derive(Debug)]
enum CallerMsg {
    Run(PgRequest),
    Done(PgCallOutcome),
}

struct CallerIsolate {
    worker: PgAddress,
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
                send_request(self.worker, request, self.timeout).then(CallerMsg::Done)
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

fn shutdown(runtime: Arc<ThreadedRuntime<SingleShard, DefaultThreadedMailboxFactory>>) {
    if let Ok(rt) = Arc::try_unwrap(runtime) {
        let _ = rt.shutdown();
    }
}

static TABLE_SEQ: AtomicU64 = AtomicU64::new(0);

fn unique_table() -> String {
    let n = TABLE_SEQ.fetch_add(1, Ordering::Relaxed);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("bridge_test_{n}_{nanos}")
}

fn install(
    runtime: &ThreadedRuntime<SingleShard, DefaultThreadedMailboxFactory>,
    url: &str,
) -> InstalledPgBridge<SingleShard> {
    let cfg = PgConfig::new()
        .with_pool(
            PgPoolConfig::new(url)
                .with_max_connections(2)
                .with_acquire_timeout(Duration::from_secs(2)),
        )
        .with_default_timeout(Duration::from_secs(3))
        .with_poll_interval(Duration::from_millis(2))
        .with_max_in_flight(2);
    PgWorker::<SingleShard>::install(runtime, cfg).expect("install bridge")
}

fn register_caller(
    runtime: &ThreadedRuntime<SingleShard, DefaultThreadedMailboxFactory>,
    worker: PgAddress,
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

fn one(
    runtime: &ThreadedRuntime<SingleShard, DefaultThreadedMailboxFactory>,
    bridge: &InstalledPgBridge<SingleShard>,
    request: PgRequest,
    call_timeout: Duration,
    wait: Duration,
) -> PgCallOutcome {
    let sink = Arc::new(Sink::default());
    let caller = register_caller(runtime, bridge.address, Arc::clone(&sink), call_timeout);
    runtime
        .try_send(caller, CallerMsg::Run(request))
        .expect("send");
    sink.wait_one(wait)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[test]
#[ignore = "needs DATABASE_URL pointing at a real Postgres"]
fn happy_execute_and_fetch_one() {
    let url = match database_url() {
        Some(u) => u,
        None => return,
    };
    let runtime = make_runtime();
    let bridge = install(&runtime, &url);
    let table = unique_table();

    let create = one(
        &runtime,
        &bridge,
        PgRequest::execute(format!(
            "CREATE TABLE {table} (id INT8 PRIMARY KEY, name TEXT NOT NULL)"
        )),
        Duration::from_secs(5),
        Duration::from_secs(10),
    );
    assert!(
        matches!(
            create,
            CallOutcome::Replied(Ok(PgResponse::Executed { .. }))
        ),
        "create: {create:?}",
    );

    let insert = one(
        &runtime,
        &bridge,
        PgRequest::execute(format!("INSERT INTO {table} (id, name) VALUES ($1, $2)"))
            .param(1_i64)
            .param("ada"),
        Duration::from_secs(5),
        Duration::from_secs(10),
    );
    assert!(
        matches!(
            insert,
            CallOutcome::Replied(Ok(PgResponse::Executed { rows_affected: 1 }))
        ),
        "insert: {insert:?}",
    );

    let fetch = one(
        &runtime,
        &bridge,
        PgRequest::fetch_one(format!("SELECT id, name FROM {table} WHERE id = $1")).param(1_i64),
        Duration::from_secs(5),
        Duration::from_secs(10),
    );
    match fetch {
        CallOutcome::Replied(Ok(PgResponse::Row(row))) => {
            assert_eq!(row.get_i64(0), Some(1));
            assert_eq!(row.get_text(1), Some("ada"));
        }
        other => panic!("fetch: {other:?}"),
    }

    let _drop = one(
        &runtime,
        &bridge,
        PgRequest::execute(format!("DROP TABLE {table}")),
        Duration::from_secs(5),
        Duration::from_secs(10),
    );

    let m = bridge.metrics.snapshot();
    assert!(m.responses_executed >= 3);
    assert_eq!(m.responses_row, 1);
    assert_eq!(m.sqlx_errors, 0);
    assert_eq!(m.timeouts, 0);

    shutdown(runtime);
}

#[test]
#[ignore = "needs DATABASE_URL pointing at a real Postgres"]
fn fetch_one_no_rows_is_no_rows() {
    let url = match database_url() {
        Some(u) => u,
        None => return,
    };
    let runtime = make_runtime();
    let bridge = install(&runtime, &url);
    let outcome = one(
        &runtime,
        &bridge,
        PgRequest::fetch_one("SELECT 1 WHERE false"),
        Duration::from_secs(5),
        Duration::from_secs(10),
    );
    assert!(
        matches!(outcome, CallOutcome::Replied(Ok(PgResponse::NoRows))),
        "outcome: {outcome:?}",
    );
    let m = bridge.metrics.snapshot();
    assert_eq!(m.responses_no_rows, 1);
    shutdown(runtime);
}

#[test]
#[ignore = "needs DATABASE_URL pointing at a real Postgres"]
fn fetch_one_too_many_rows_is_too_many_rows() {
    let url = match database_url() {
        Some(u) => u,
        None => return,
    };
    let runtime = make_runtime();
    let bridge = install(&runtime, &url);
    let outcome = one(
        &runtime,
        &bridge,
        PgRequest::fetch_one("SELECT generate_series(1, 5)"),
        Duration::from_secs(5),
        Duration::from_secs(10),
    );
    assert!(
        matches!(outcome, CallOutcome::Replied(Err(PgError::TooManyRows))),
        "outcome: {outcome:?}",
    );
    let m = bridge.metrics.snapshot();
    assert_eq!(m.too_many_rows, 1);
    shutdown(runtime);
}

#[test]
#[ignore = "needs DATABASE_URL pointing at a real Postgres"]
fn fetch_many_under_cap_returns_all_rows_not_truncated() {
    let url = match database_url() {
        Some(u) => u,
        None => return,
    };
    let runtime = make_runtime();
    let bridge = install(&runtime, &url);
    let outcome = one(
        &runtime,
        &bridge,
        PgRequest::fetch_many("SELECT generate_series(1, 5)::INT8", 100),
        Duration::from_secs(5),
        Duration::from_secs(10),
    );
    match outcome {
        CallOutcome::Replied(Ok(PgResponse::Rows { rows, truncated })) => {
            assert_eq!(rows.len(), 5);
            assert!(!truncated, "5 rows under a cap of 100 must not truncate");
            for (i, row) in rows.iter().enumerate() {
                assert_eq!(row.get_i64(0), Some((i as i64) + 1));
            }
        }
        other => panic!("fetch_many: {other:?}"),
    }
    let m = bridge.metrics.snapshot();
    assert_eq!(m.responses_rows, 1);
    assert_eq!(m.responses_truncated, 0);
    assert_eq!(m.rows_returned, 5);
    shutdown(runtime);
}

#[test]
#[ignore = "needs DATABASE_URL pointing at a real Postgres"]
fn fetch_many_at_cap_truncates_without_buffering_extras() {
    let url = match database_url() {
        Some(u) => u,
        None => return,
    };
    let runtime = make_runtime();
    let bridge = install(&runtime, &url);
    // Cap at 3, query produces 100. Bridge must stop after 4 pulls
    // (3 kept + 1 to discriminate "more on the wire") and never grow
    // its buffer past 3.
    let outcome = one(
        &runtime,
        &bridge,
        PgRequest::fetch_many("SELECT generate_series(1, 100)::INT8", 3),
        Duration::from_secs(5),
        Duration::from_secs(10),
    );
    match outcome {
        CallOutcome::Replied(Ok(PgResponse::Rows { rows, truncated })) => {
            assert_eq!(rows.len(), 3, "rows beyond cap must not land in the buffer");
            assert!(truncated, "must signal truncation when more rows existed");
            assert_eq!(rows[0].get_i64(0), Some(1));
            assert_eq!(rows[2].get_i64(0), Some(3));
        }
        other => panic!("fetch_many: {other:?}"),
    }
    let m = bridge.metrics.snapshot();
    assert_eq!(m.responses_rows, 1);
    assert_eq!(m.responses_truncated, 1);
    assert_eq!(m.rows_returned, 3, "rows_returned reflects buffered count");
    shutdown(runtime);
}

#[test]
#[ignore = "needs DATABASE_URL pointing at a real Postgres"]
fn fetch_many_zero_rows_returns_empty_not_truncated() {
    let url = match database_url() {
        Some(u) => u,
        None => return,
    };
    let runtime = make_runtime();
    let bridge = install(&runtime, &url);
    let outcome = one(
        &runtime,
        &bridge,
        PgRequest::fetch_many("SELECT 1::INT8 WHERE false", 10),
        Duration::from_secs(5),
        Duration::from_secs(10),
    );
    match outcome {
        CallOutcome::Replied(Ok(PgResponse::Rows { rows, truncated })) => {
            assert!(rows.is_empty());
            assert!(!truncated);
        }
        other => panic!("fetch_many: {other:?}"),
    }
    shutdown(runtime);
}

#[test]
#[ignore = "needs DATABASE_URL pointing at a real Postgres"]
fn fetch_many_caller_max_rows_is_capped_by_config_ceiling() {
    let url = match database_url() {
        Some(u) => u,
        None => return,
    };
    let runtime = make_runtime();
    // Config ceiling: 5. Caller asks for 1000. Effective cap: 5.
    let cfg = PgConfig::new()
        .with_pool(
            PgPoolConfig::new(&url)
                .with_max_connections(2)
                .with_acquire_timeout(Duration::from_secs(2)),
        )
        .with_default_timeout(Duration::from_secs(5))
        .with_poll_interval(Duration::from_millis(2))
        .with_max_in_flight(2)
        .with_max_response_rows(5);
    let bridge = PgWorker::<SingleShard>::install(&runtime, cfg).expect("install");
    let outcome = one(
        &runtime,
        &bridge,
        PgRequest::fetch_many("SELECT generate_series(1, 100)::INT8", 1000),
        Duration::from_secs(5),
        Duration::from_secs(10),
    );
    match outcome {
        CallOutcome::Replied(Ok(PgResponse::Rows { rows, truncated })) => {
            assert_eq!(rows.len(), 5, "config ceiling caps caller's request");
            assert!(truncated);
        }
        other => panic!("fetch_many: {other:?}"),
    }
    shutdown(runtime);
}

#[test]
#[ignore = "needs DATABASE_URL pointing at a real Postgres"]
fn fetch_many_typed_helper_returns_pgrows() {
    use tina_sqlx_bridge::fetch_many_call;

    let url = match database_url() {
        Some(u) => u,
        None => return,
    };
    let runtime = make_runtime();
    let bridge = install(&runtime, &url);

    let sink = Arc::new(Sink::default());

    #[derive(Debug)]
    enum HelperMsg {
        Run,
        Done(tina_sqlx_bridge::PgFetchManyOutcome),
    }
    struct HelperIsolate {
        worker: PgAddress,
        sink: Arc<Mutex<Option<Result<PgRows, PgError>>>>,
        done: Arc<std::sync::Condvar>,
    }
    impl Isolate for HelperIsolate {
        tina::isolate_types! {
            message: HelperMsg,
            reply: (),
            send: tina::Outbound<Infallible>,
            spawn: Infallible,
            call: RuntimeCall<HelperMsg>,
            shard: SingleShard,
        }
        fn handle(
            &mut self,
            msg: HelperMsg,
            _ctx: &mut Context<'_, SingleShard, Self::Reply>,
        ) -> Effect<Self> {
            match msg {
                HelperMsg::Run => fetch_many_call(
                    self.worker,
                    "SELECT generate_series(1, 4)::INT8",
                    vec![],
                    10,
                    Duration::from_secs(5),
                )
                .then(HelperMsg::Done),
                HelperMsg::Done(outcome) => {
                    let stored = match outcome {
                        CallOutcome::Replied(r) => Some(r),
                        _ => None,
                    };
                    *self.sink.lock().unwrap() = stored;
                    self.done.notify_all();
                    stop()
                }
            }
        }
    }
    let _ = sink; // silence the wider Sink in this scope.

    let result_slot: Arc<Mutex<Option<Result<PgRows, PgError>>>> = Arc::new(Mutex::new(None));
    let done = Arc::new(std::sync::Condvar::new());
    let isolate = HelperIsolate {
        worker: bridge.address,
        sink: Arc::clone(&result_slot),
        done: Arc::clone(&done),
    };
    let addr = runtime
        .register_with_capacity::<_, Infallible>(isolate, 4)
        .expect("register");
    runtime.try_send(addr, HelperMsg::Run).expect("send");

    let deadline = Instant::now() + Duration::from_secs(10);
    let mut guard = result_slot.lock().unwrap();
    while guard.is_none() {
        let now = Instant::now();
        if now >= deadline {
            panic!("helper never finished");
        }
        let (g, _) = done.wait_timeout(guard, deadline - now).unwrap();
        guard = g;
    }
    let result = guard.take().unwrap();
    drop(guard);

    match result {
        Ok(rows) => {
            assert_eq!(rows.len(), 4);
            assert!(!rows.truncated);
        }
        Err(e) => panic!("typed helper failed: {e}"),
    }
    shutdown(runtime);
}

#[test]
#[ignore = "needs DATABASE_URL pointing at a real Postgres"]
fn fetch_one_does_not_buffer_huge_result_sets() {
    // Pre-fix this query buffered ~10M rows in bridge memory before
    // discovering the row count and surfacing TooManyRows. Post-fix
    // the streaming peek aborts after the second row, so this
    // returns quickly and never grows the bridge's resident set.
    let url = match database_url() {
        Some(u) => u,
        None => return,
    };
    let runtime = make_runtime();
    let bridge = install(&runtime, &url);
    let started = Instant::now();
    let outcome = one(
        &runtime,
        &bridge,
        PgRequest::fetch_one("SELECT generate_series(1, 10000000)::INT8"),
        Duration::from_secs(10),
        Duration::from_secs(15),
    );
    let elapsed = started.elapsed();
    assert!(
        matches!(outcome, CallOutcome::Replied(Err(PgError::TooManyRows))),
        "outcome: {outcome:?}",
    );
    // Two rows worth of round-trip is comfortably under one second
    // even on slow CI; pre-fix this took multiple seconds and
    // allocated several hundred MB.
    assert!(
        elapsed < Duration::from_secs(2),
        "fetch_one returned in {elapsed:?}; suspect we're buffering",
    );
    shutdown(runtime);
}

#[test]
#[ignore = "needs DATABASE_URL pointing at a real Postgres"]
fn sql_error_then_recovery() {
    let url = match database_url() {
        Some(u) => u,
        None => return,
    };
    let runtime = make_runtime();
    let bridge = install(&runtime, &url);

    // Bad SQL syntax → Sqlx error.
    let bad = one(
        &runtime,
        &bridge,
        PgRequest::execute("THIS IS NOT VALID SQL"),
        Duration::from_secs(5),
        Duration::from_secs(10),
    );
    match bad {
        CallOutcome::Replied(Err(PgError::Sqlx(_))) => {}
        other => panic!("bad sql: {other:?}"),
    }

    // Same bridge still serves a real query afterwards.
    let good = one(
        &runtime,
        &bridge,
        PgRequest::fetch_one("SELECT 1::INT8"),
        Duration::from_secs(5),
        Duration::from_secs(10),
    );
    match good {
        CallOutcome::Replied(Ok(PgResponse::Row(row))) => {
            assert_eq!(row.get_i64(0), Some(1));
        }
        other => panic!("recovery: {other:?}"),
    }

    let m = bridge.metrics.snapshot();
    assert_eq!(m.sqlx_errors, 1);
    assert_eq!(m.responses_row, 1);
    shutdown(runtime);
}

#[test]
#[ignore = "needs DATABASE_URL pointing at a real Postgres"]
fn per_attempt_bridge_timeout_surfaces_timeout_and_records_late_result() {
    let url = match database_url() {
        Some(u) => u,
        None => return,
    };
    let runtime = make_runtime();
    // Bridge per-attempt timeout much shorter than the query.
    let cfg = PgConfig::from_url(&url)
        .with_default_timeout(Duration::from_millis(50))
        .with_poll_interval(Duration::from_millis(2))
        .with_max_in_flight(2);
    let bridge = PgWorker::<SingleShard>::install(&runtime, cfg).expect("install");

    // `pg_sleep(...)` returns a VOID column (which the bridge's
    // narrow decoder rejects). Project a decodable INT8 from it so
    // late completion tallies as `responses_row` instead of
    // `decode_errors`, keeping the success-path assertion below
    // meaningful.
    let outcome = one(
        &runtime,
        &bridge,
        PgRequest::fetch_one("SELECT 1::INT8 FROM pg_sleep(1)"),
        Duration::from_secs(10),
        Duration::from_secs(15),
    );
    assert!(
        matches!(outcome, CallOutcome::Replied(Err(PgError::Timeout))),
        "outcome: {outcome:?}",
    );
    // Caller saw Timeout at ~50ms. The detached SQLx future keeps
    // running until pg_sleep(1) returns ~1s later; at that point
    // tally + late_results fire. Poll briefly so the assertion
    // doesn't race the detached task.
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut snap = bridge.metrics.snapshot();
    while snap.late_results == 0 && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
        snap = bridge.metrics.snapshot();
    }
    assert!(snap.timeouts >= 1, "timeouts: {}", snap.timeouts);
    assert!(
        snap.late_results >= 1,
        "late_results never incremented after detached task: {snap:?}",
    );
    // The detached future is no longer aborted, so the actual
    // worker-terminal must have been recorded too. pg_sleep
    // returns one row → responses_row should be 1.
    assert!(
        snap.responses_row >= 1,
        "detached task should have completed and tallied: {snap:?}",
    );
    shutdown(runtime);
}

#[test]
#[ignore = "needs DATABASE_URL pointing at a real Postgres"]
fn pool_acquire_timeout_distinct_from_tina_full() {
    let url = match database_url() {
        Some(u) => u,
        None => return,
    };
    let runtime = make_runtime();

    // Pool: 1 connection, 200ms acquire timeout. Bridge: max_in_flight
    // = 4 so Tina admission never trips. With one slow query holding
    // the only connection, a second admitted call must wait for the
    // pool — past 200ms it surfaces PoolAcquireTimeout, NOT Full.
    let cfg = PgConfig::new()
        .with_pool(
            PgPoolConfig::new(&url)
                .with_max_connections(1)
                .with_acquire_timeout(Duration::from_millis(200)),
        )
        .with_default_timeout(Duration::from_secs(5))
        .with_poll_interval(Duration::from_millis(2))
        .with_max_in_flight(4);
    let bridge = PgWorker::<SingleShard>::install(&runtime, cfg).expect("install");

    let sink_a = Arc::new(Sink::default());
    let sink_b = Arc::new(Sink::default());
    let caller_a = register_caller(
        &runtime,
        bridge.address,
        Arc::clone(&sink_a),
        Duration::from_secs(10),
    );
    let caller_b = register_caller(
        &runtime,
        bridge.address,
        Arc::clone(&sink_b),
        Duration::from_secs(10),
    );

    runtime
        .try_send(
            caller_a,
            CallerMsg::Run(PgRequest::execute("SELECT pg_sleep(2)")),
        )
        .expect("send a");
    // Make sure A reaches Postgres before B asks for a connection.
    std::thread::sleep(Duration::from_millis(200));
    runtime
        .try_send(caller_b, CallerMsg::Run(PgRequest::execute("SELECT 1")))
        .expect("send b");

    let outcome_b = sink_b.wait_one(Duration::from_secs(5));
    assert!(
        matches!(
            outcome_b,
            CallOutcome::Replied(Err(PgError::PoolAcquireTimeout))
        ),
        "B should see PoolAcquireTimeout, not Full: {outcome_b:?}",
    );
    let m = bridge.metrics.snapshot();
    assert!(m.pool_acquire_timeouts >= 1);
    assert_eq!(m.full, 0, "Tina admission must not have tripped");

    // Drain A so the runtime can shut down cleanly.
    let _ = sink_a.wait_one(Duration::from_secs(10));
    shutdown(runtime);
}

#[test]
#[ignore = "needs DATABASE_URL pointing at a real Postgres"]
fn transaction_commits_when_all_steps_succeed() {
    let url = match database_url() {
        Some(u) => u,
        None => return,
    };
    let runtime = make_runtime();
    let bridge = install(&runtime, &url);
    let table = unique_table();

    // Create the table outside the transaction.
    let _ = one(
        &runtime,
        &bridge,
        PgRequest::execute(format!(
            "CREATE TABLE {table} (id INT8 PRIMARY KEY, value INT8 NOT NULL)"
        )),
        Duration::from_secs(5),
        Duration::from_secs(10),
    );

    // Atomic script: insert two rows and read one back.
    let outcome = one(
        &runtime,
        &bridge,
        PgRequest::transaction(vec![
            PgStep::execute(format!("INSERT INTO {table} (id, value) VALUES (1, 100)")),
            PgStep::execute(format!("INSERT INTO {table} (id, value) VALUES (2, 200)")),
            PgStep::fetch_one(format!("SELECT value FROM {table} WHERE id = 1")),
        ]),
        Duration::from_secs(5),
        Duration::from_secs(10),
    );
    match outcome {
        CallOutcome::Replied(Ok(PgResponse::Transaction(PgTransactionOutcome::Committed {
            steps,
        }))) => {
            assert_eq!(steps.len(), 3);
            assert!(matches!(steps[0], PgStepOk::Executed { rows_affected: 1 }));
            assert!(matches!(steps[1], PgStepOk::Executed { rows_affected: 1 }));
            match &steps[2] {
                PgStepOk::Row(row) => assert_eq!(row.get_i64(0), Some(100)),
                other => panic!("step 2: expected Row, got {other:?}"),
            }
        }
        other => panic!("transaction: {other:?}"),
    }

    // Verify both rows are visible after commit.
    let count = one(
        &runtime,
        &bridge,
        PgRequest::fetch_one(format!("SELECT count(*)::INT8 FROM {table}")),
        Duration::from_secs(5),
        Duration::from_secs(10),
    );
    match count {
        CallOutcome::Replied(Ok(PgResponse::Row(row))) => {
            assert_eq!(row.get_i64(0), Some(2));
        }
        other => panic!("count: {other:?}"),
    }

    let _ = one(
        &runtime,
        &bridge,
        PgRequest::execute(format!("DROP TABLE {table}")),
        Duration::from_secs(5),
        Duration::from_secs(10),
    );

    let m = bridge.metrics.snapshot();
    assert_eq!(m.transactions_committed, 1);
    assert_eq!(m.transactions_rolled_back, 0);
    shutdown(runtime);
}

#[test]
#[ignore = "needs DATABASE_URL pointing at a real Postgres"]
fn transaction_rolls_back_on_step_failure() {
    let url = match database_url() {
        Some(u) => u,
        None => return,
    };
    let runtime = make_runtime();
    let bridge = install(&runtime, &url);
    let table = unique_table();

    let _ = one(
        &runtime,
        &bridge,
        PgRequest::execute(format!(
            "CREATE TABLE {table} (id INT8 PRIMARY KEY, value INT8 NOT NULL)"
        )),
        Duration::from_secs(5),
        Duration::from_secs(10),
    );

    // Step 0 succeeds, step 1 violates the PK (id = 1 again),
    // step 2 never runs. Whole script must roll back.
    let outcome = one(
        &runtime,
        &bridge,
        PgRequest::transaction(vec![
            PgStep::execute(format!("INSERT INTO {table} (id, value) VALUES (1, 100)")),
            PgStep::execute(format!("INSERT INTO {table} (id, value) VALUES (1, 200)")),
            PgStep::execute(format!("INSERT INTO {table} (id, value) VALUES (2, 300)")),
        ]),
        Duration::from_secs(5),
        Duration::from_secs(10),
    );
    match outcome {
        CallOutcome::Replied(Ok(PgResponse::Transaction(PgTransactionOutcome::RolledBack {
            completed,
            failed_at,
            error,
        }))) => {
            assert_eq!(completed.len(), 1, "only step 0 ran before failure");
            assert_eq!(failed_at, 1);
            assert!(matches!(error, PgError::Sqlx(_)), "got {error:?}");
        }
        other => panic!("transaction: {other:?}"),
    }

    // Confirm rollback: no rows visible.
    let count = one(
        &runtime,
        &bridge,
        PgRequest::fetch_one(format!("SELECT count(*)::INT8 FROM {table}")),
        Duration::from_secs(5),
        Duration::from_secs(10),
    );
    match count {
        CallOutcome::Replied(Ok(PgResponse::Row(row))) => {
            assert_eq!(row.get_i64(0), Some(0), "rollback must hide all rows");
        }
        other => panic!("count: {other:?}"),
    }

    let _ = one(
        &runtime,
        &bridge,
        PgRequest::execute(format!("DROP TABLE {table}")),
        Duration::from_secs(5),
        Duration::from_secs(10),
    );

    let m = bridge.metrics.snapshot();
    assert_eq!(m.transactions_committed, 0);
    assert_eq!(m.transactions_rolled_back, 1);
    shutdown(runtime);
}

#[test]
#[ignore = "needs DATABASE_URL pointing at a real Postgres"]
fn transaction_step_can_be_fetch_many_with_truncation() {
    let url = match database_url() {
        Some(u) => u,
        None => return,
    };
    let runtime = make_runtime();
    let bridge = install(&runtime, &url);

    let outcome = one(
        &runtime,
        &bridge,
        PgRequest::transaction(vec![PgStep::fetch_many(
            "SELECT generate_series(1, 50)::INT8",
            3,
        )]),
        Duration::from_secs(5),
        Duration::from_secs(10),
    );
    match outcome {
        CallOutcome::Replied(Ok(PgResponse::Transaction(PgTransactionOutcome::Committed {
            steps,
        }))) => match &steps[0] {
            PgStepOk::Rows { rows, truncated } => {
                assert_eq!(rows.len(), 3);
                assert!(truncated);
            }
            other => panic!("step 0: {other:?}"),
        },
        other => panic!("transaction: {other:?}"),
    }
    shutdown(runtime);
}

#[test]
#[ignore = "needs DATABASE_URL pointing at a real Postgres"]
fn transaction_typed_helper_returns_outcome() {
    use tina_sqlx_bridge::transaction_call;

    let url = match database_url() {
        Some(u) => u,
        None => return,
    };
    let runtime = make_runtime();
    let bridge = install(&runtime, &url);

    let result_slot: Arc<Mutex<Option<Result<PgTransactionOutcome, PgError>>>> =
        Arc::new(Mutex::new(None));
    let done = Arc::new(std::sync::Condvar::new());

    #[derive(Debug)]
    enum HelperMsg {
        Run,
        Done(tina_sqlx_bridge::PgTransactionCallOutcome),
    }
    struct HelperIsolate {
        worker: PgAddress,
        slot: Arc<Mutex<Option<Result<PgTransactionOutcome, PgError>>>>,
        done: Arc<std::sync::Condvar>,
    }
    impl Isolate for HelperIsolate {
        tina::isolate_types! {
            message: HelperMsg,
            reply: (),
            send: tina::Outbound<Infallible>,
            spawn: Infallible,
            call: RuntimeCall<HelperMsg>,
            shard: SingleShard,
        }
        fn handle(
            &mut self,
            msg: HelperMsg,
            _ctx: &mut Context<'_, SingleShard, Self::Reply>,
        ) -> Effect<Self> {
            match msg {
                HelperMsg::Run => transaction_call(
                    self.worker,
                    vec![
                        PgStep::execute("SELECT 1::INT8"),
                        PgStep::fetch_one("SELECT 2::INT8"),
                    ],
                    Duration::from_secs(5),
                )
                .then(HelperMsg::Done),
                HelperMsg::Done(outcome) => {
                    let stored = match outcome {
                        CallOutcome::Replied(r) => Some(r),
                        _ => None,
                    };
                    *self.slot.lock().unwrap() = stored;
                    self.done.notify_all();
                    stop()
                }
            }
        }
    }
    let isolate = HelperIsolate {
        worker: bridge.address,
        slot: Arc::clone(&result_slot),
        done: Arc::clone(&done),
    };
    let addr = runtime
        .register_with_capacity::<_, Infallible>(isolate, 4)
        .expect("register");
    runtime.try_send(addr, HelperMsg::Run).expect("send");

    let deadline = Instant::now() + Duration::from_secs(10);
    let mut guard = result_slot.lock().unwrap();
    while guard.is_none() {
        let now = Instant::now();
        if now >= deadline {
            panic!("helper never finished");
        }
        let (g, _) = done.wait_timeout(guard, deadline - now).unwrap();
        guard = g;
    }
    let result = guard.take().unwrap();
    drop(guard);

    match result {
        Ok(PgTransactionOutcome::Committed { steps }) => assert_eq!(steps.len(), 2),
        other => panic!("typed helper: {other:?}"),
    }
    shutdown(runtime);
}

#[test]
#[ignore = "needs DATABASE_URL pointing at a real Postgres"]
fn cancel_on_timeout_config_is_noop_until_connection_quarantine_exists() {
    let url = match database_url() {
        Some(u) => u,
        None => return,
    };
    let runtime = make_runtime();
    // Phase 123 deliberately disables DB-side pg_cancel_backend(pid):
    // the old sidecar could race connection reuse and cancel a later
    // query on the same backend. The config remains accepted for
    // compatibility, but timeout means "Tina stopped waiting."
    let cfg = PgConfig::new()
        .with_pool(
            PgPoolConfig::new(&url)
                .with_max_connections(2)
                .with_acquire_timeout(Duration::from_secs(2)),
        )
        .with_default_timeout(Duration::from_millis(100))
        .with_poll_interval(Duration::from_millis(2))
        .with_max_in_flight(2)
        .with_cancel_on_timeout(1);
    let bridge = PgWorker::<SingleShard>::install(&runtime, cfg).expect("install");

    let started = Instant::now();
    let outcome = one(
        &runtime,
        &bridge,
        PgRequest::execute("SELECT pg_sleep(0.5)"),
        Duration::from_secs(15),
        Duration::from_secs(20),
    );
    assert!(
        matches!(outcome, CallOutcome::Replied(Err(PgError::Timeout))),
        "outcome: {outcome:?}",
    );

    // Wait for the natural late tally. A sidecar cancel would usually
    // produce an earlier Sqlx error; the fixed behavior is a late
    // success and zero cancel attempts.
    let late_deadline = Instant::now() + Duration::from_secs(2);
    while bridge.metrics.snapshot().late_results == 0 {
        if Instant::now() >= late_deadline {
            panic!(
                "late_results never incremented: {:?}",
                bridge.metrics.snapshot()
            );
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    let total = started.elapsed();
    let snap = bridge.metrics.snapshot();

    assert!(snap.timeouts >= 1, "{snap:?}");
    assert!(snap.late_results >= 1, "{snap:?}");
    assert_eq!(
        snap.db_cancels_sent, 0,
        "cancel config must not fire unsafe pid-sidecar cancel: {snap:?}",
    );
    assert!(
        total >= Duration::from_millis(400),
        "query should finish naturally instead of sidecar-canceling; got {total:?}",
    );
    assert!(
        snap.responses_executed >= 1,
        "late natural completion should tally worker success: {snap:?}",
    );
    shutdown(runtime);
}

#[test]
#[ignore = "needs DATABASE_URL pointing at a real Postgres"]
fn cancel_on_timeout_disabled_by_default_no_cancels_sent() {
    let url = match database_url() {
        Some(u) => u,
        None => return,
    };
    let runtime = make_runtime();
    // No `.with_cancel_on_timeout(...)` — default is off.
    let cfg = PgConfig::new()
        .with_pool(
            PgPoolConfig::new(&url)
                .with_max_connections(2)
                .with_acquire_timeout(Duration::from_secs(2)),
        )
        .with_default_timeout(Duration::from_millis(100))
        .with_poll_interval(Duration::from_millis(2))
        .with_max_in_flight(2);
    let bridge = PgWorker::<SingleShard>::install(&runtime, cfg).expect("install");

    let outcome = one(
        &runtime,
        &bridge,
        // Short sleep so the test does not hang on the late tally.
        PgRequest::execute("SELECT pg_sleep(0.5)"),
        Duration::from_secs(5),
        Duration::from_secs(10),
    );
    assert!(matches!(
        outcome,
        CallOutcome::Replied(Err(PgError::Timeout))
    ));

    // Wait for late tally so the assertion is stable.
    let deadline = Instant::now() + Duration::from_secs(2);
    while bridge.metrics.snapshot().late_results == 0 {
        if Instant::now() >= deadline {
            panic!("late_results never incremented");
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    let snap = bridge.metrics.snapshot();
    assert_eq!(
        snap.db_cancels_sent, 0,
        "cancel must stay off by default: {snap:?}",
    );
    shutdown(runtime);
}

#[test]
#[ignore = "needs DATABASE_URL pointing at a real Postgres"]
fn typed_null_text_round_trips_via_positional_bind() {
    // A positional NULL bind into a TEXT column would need a SQL
    // cast under PgValue::Null (which sends INT8 NULL). With
    // PgValue::null_text() the wire-level NULL is typed as TEXT
    // and Postgres accepts it without a cast.
    use tina_sqlx_bridge::PgType;
    let url = match database_url() {
        Some(u) => u,
        None => return,
    };
    let runtime = make_runtime();
    let bridge = install(&runtime, &url);
    let table = unique_table();

    let _ = one(
        &runtime,
        &bridge,
        PgRequest::execute(format!(
            "CREATE TABLE {table} (id INT8 PRIMARY KEY, name TEXT)"
        )),
        Duration::from_secs(5),
        Duration::from_secs(10),
    );
    let insert = one(
        &runtime,
        &bridge,
        PgRequest::execute(format!("INSERT INTO {table} (id, name) VALUES ($1, $2)"))
            .param(1_i64)
            .param(PgValue::null_text()),
        Duration::from_secs(5),
        Duration::from_secs(10),
    );
    assert!(
        matches!(
            insert,
            CallOutcome::Replied(Ok(PgResponse::Executed { rows_affected: 1 }))
        ),
        "typed null insert: {insert:?}",
    );

    let fetch = one(
        &runtime,
        &bridge,
        PgRequest::fetch_one(format!("SELECT name FROM {table} WHERE id = $1")).param(1_i64),
        Duration::from_secs(5),
        Duration::from_secs(10),
    );
    match fetch {
        CallOutcome::Replied(Ok(PgResponse::Row(row))) => {
            assert!(row.col(0).map(PgValue::is_null).unwrap_or(false));
        }
        other => panic!("fetch: {other:?}"),
    }

    // Sanity: PgType::into() builds the same TypedNull.
    let pre_built: PgValue = PgType::Text.into();
    assert_eq!(pre_built, PgValue::TypedNull(PgType::Text));

    let _ = one(
        &runtime,
        &bridge,
        PgRequest::execute(format!("DROP TABLE {table}")),
        Duration::from_secs(5),
        Duration::from_secs(10),
    );
    shutdown(runtime);
}

#[cfg(feature = "uuid")]
#[test]
#[ignore = "needs DATABASE_URL pointing at a real Postgres"]
fn typed_null_uuid_works_where_untyped_null_would_fail() {
    // The whole point of typed nulls: a positional NULL into a UUID
    // column without a SQL cast. Untyped Null sends INT8 NULL,
    // which Postgres rejects with a type-mismatch. This test
    // exercises the success path; the failure path (untyped Null)
    // is covered by the `untyped_null_into_uuid_column_errors` test
    // below as a contrast.
    let url = match database_url() {
        Some(u) => u,
        None => return,
    };
    let runtime = make_runtime();
    let bridge = install(&runtime, &url);
    let table = unique_table();

    let _ = one(
        &runtime,
        &bridge,
        PgRequest::execute(format!(
            "CREATE TABLE {table} (id INT8 PRIMARY KEY, ref UUID)"
        )),
        Duration::from_secs(5),
        Duration::from_secs(10),
    );

    let insert = one(
        &runtime,
        &bridge,
        PgRequest::execute(format!("INSERT INTO {table} (id, ref) VALUES ($1, $2)"))
            .param(1_i64)
            .param(PgValue::null_uuid()),
        Duration::from_secs(5),
        Duration::from_secs(10),
    );
    assert!(
        matches!(
            insert,
            CallOutcome::Replied(Ok(PgResponse::Executed { rows_affected: 1 }))
        ),
        "typed null uuid insert: {insert:?}",
    );

    let fetch = one(
        &runtime,
        &bridge,
        PgRequest::fetch_one(format!("SELECT ref FROM {table} WHERE id = $1")).param(1_i64),
        Duration::from_secs(5),
        Duration::from_secs(10),
    );
    match fetch {
        CallOutcome::Replied(Ok(PgResponse::Row(row))) => {
            assert!(row.col(0).map(PgValue::is_null).unwrap_or(false));
        }
        other => panic!("fetch: {other:?}"),
    }

    let _ = one(
        &runtime,
        &bridge,
        PgRequest::execute(format!("DROP TABLE {table}")),
        Duration::from_secs(5),
        Duration::from_secs(10),
    );
    shutdown(runtime);
}

#[cfg(feature = "uuid")]
#[test]
#[ignore = "needs DATABASE_URL pointing at a real Postgres"]
fn untyped_null_into_uuid_column_errors() {
    // Contrast for the typed-null test above: PgValue::Null is sent
    // as INT8 NULL, so a positional bind into a UUID column trips
    // a Postgres type-mismatch error. This test locks the failure
    // shape so a regression in the bind code is loud.
    let url = match database_url() {
        Some(u) => u,
        None => return,
    };
    let runtime = make_runtime();
    let bridge = install(&runtime, &url);
    let table = unique_table();

    let _ = one(
        &runtime,
        &bridge,
        PgRequest::execute(format!(
            "CREATE TABLE {table} (id INT8 PRIMARY KEY, ref UUID)"
        )),
        Duration::from_secs(5),
        Duration::from_secs(10),
    );

    let outcome = one(
        &runtime,
        &bridge,
        PgRequest::execute(format!("INSERT INTO {table} (id, ref) VALUES ($1, $2)"))
            .param(1_i64)
            .param(PgValue::Null),
        Duration::from_secs(5),
        Duration::from_secs(10),
    );
    assert!(
        matches!(outcome, CallOutcome::Replied(Err(PgError::Sqlx(_)))),
        "expected Sqlx type-mismatch error, got {outcome:?}",
    );

    let _ = one(
        &runtime,
        &bridge,
        PgRequest::execute(format!("DROP TABLE {table}")),
        Duration::from_secs(5),
        Duration::from_secs(10),
    );
    shutdown(runtime);
}

#[test]
#[ignore = "needs DATABASE_URL pointing at a real Postgres"]
fn unsupported_type_decode_hints_at_feature() {
    // Without the `time` feature, a TIMESTAMPTZ column decodes to
    // PgError::Decode with a hint pointing at the cargo feature
    // that fixes it. This test runs whether or not the feature is
    // enabled — when it is, the column decodes successfully and
    // we skip the assertion. The point is locking the
    // turn-this-feature-on hint for the unfeatured build.
    let url = match database_url() {
        Some(u) => u,
        None => return,
    };
    let runtime = make_runtime();
    let bridge = install(&runtime, &url);
    let outcome = one(
        &runtime,
        &bridge,
        PgRequest::fetch_one("SELECT now()::TIMESTAMPTZ"),
        Duration::from_secs(5),
        Duration::from_secs(10),
    );
    match outcome {
        // `time` feature off path: hint must call out the feature.
        #[cfg(not(feature = "time"))]
        CallOutcome::Replied(Err(PgError::Decode(msg))) => {
            assert!(
                msg.contains("\"time\" feature"),
                "expected hint about the time feature, got {msg}",
            );
        }
        // `time` feature on path: column decodes cleanly.
        #[cfg(feature = "time")]
        CallOutcome::Replied(Ok(PgResponse::Row(_))) => {}
        other => panic!("unexpected: {other:?}"),
    }
    shutdown(runtime);
}

#[cfg(feature = "uuid")]
#[test]
#[ignore = "needs DATABASE_URL pointing at a real Postgres"]
fn uuid_round_trip() {
    let url = match database_url() {
        Some(u) => u,
        None => return,
    };
    let runtime = make_runtime();
    let bridge = install(&runtime, &url);
    let id = uuid::Uuid::from_u128(0x1111_2222_3333_4444_5555_6666_7777_8888);

    let outcome = one(
        &runtime,
        &bridge,
        PgRequest::fetch_one("SELECT $1::UUID").param(PgValue::from(id)),
        Duration::from_secs(5),
        Duration::from_secs(10),
    );
    match outcome {
        CallOutcome::Replied(Ok(PgResponse::Row(row))) => {
            assert_eq!(row.col(0).and_then(PgValue::as_uuid), Some(id));
        }
        other => panic!("uuid: {other:?}"),
    }
    shutdown(runtime);
}

#[cfg(feature = "json")]
#[test]
#[ignore = "needs DATABASE_URL pointing at a real Postgres"]
fn json_and_jsonb_round_trip() {
    let url = match database_url() {
        Some(u) => u,
        None => return,
    };
    let runtime = make_runtime();
    let bridge = install(&runtime, &url);
    let payload = serde_json::json!({ "k": "v", "n": 7, "list": [1, 2] });

    // JSONB
    let outcome = one(
        &runtime,
        &bridge,
        PgRequest::fetch_one("SELECT $1::JSONB").param(PgValue::from(payload.clone())),
        Duration::from_secs(5),
        Duration::from_secs(10),
    );
    match outcome {
        CallOutcome::Replied(Ok(PgResponse::Row(row))) => {
            assert_eq!(row.col(0).and_then(PgValue::as_json), Some(&payload));
        }
        other => panic!("jsonb: {other:?}"),
    }

    // JSON (not JSONB) — same Rust type, different Postgres column.
    let outcome = one(
        &runtime,
        &bridge,
        PgRequest::fetch_one("SELECT '{\"k\":\"v\"}'::JSON"),
        Duration::from_secs(5),
        Duration::from_secs(10),
    );
    match outcome {
        CallOutcome::Replied(Ok(PgResponse::Row(row))) => {
            let v = row.col(0).and_then(PgValue::as_json).expect("json cell");
            assert_eq!(v, &serde_json::json!({ "k": "v" }));
        }
        other => panic!("json: {other:?}"),
    }
    shutdown(runtime);
}

#[cfg(feature = "numeric")]
#[test]
#[ignore = "needs DATABASE_URL pointing at a real Postgres"]
fn numeric_round_trip() {
    use core::str::FromStr;

    let url = match database_url() {
        Some(u) => u,
        None => return,
    };
    let runtime = make_runtime();
    let bridge = install(&runtime, &url);
    let d = rust_decimal::Decimal::from_str("12345.678901234").unwrap();

    let outcome = one(
        &runtime,
        &bridge,
        PgRequest::fetch_one("SELECT $1::NUMERIC").param(PgValue::from(d)),
        Duration::from_secs(5),
        Duration::from_secs(10),
    );
    match outcome {
        CallOutcome::Replied(Ok(PgResponse::Row(row))) => {
            assert_eq!(row.col(0).and_then(PgValue::as_numeric), Some(d));
        }
        other => panic!("numeric: {other:?}"),
    }
    shutdown(runtime);
}

#[cfg(feature = "time")]
#[test]
#[ignore = "needs DATABASE_URL pointing at a real Postgres"]
fn temporal_round_trip() {
    use time::macros::{date, datetime, time as t};

    let url = match database_url() {
        Some(u) => u,
        None => return,
    };
    let runtime = make_runtime();
    let bridge = install(&runtime, &url);

    let d = date!(2024 - 06 - 15);
    let dt = time::PrimitiveDateTime::new(d, t!(12:34:56));
    let dtz = datetime!(2024-06-15 12:34:56 UTC);

    // DATE
    let outcome = one(
        &runtime,
        &bridge,
        PgRequest::fetch_one("SELECT $1::DATE").param(PgValue::from(d)),
        Duration::from_secs(5),
        Duration::from_secs(10),
    );
    match outcome {
        CallOutcome::Replied(Ok(PgResponse::Row(row))) => {
            assert_eq!(row.col(0).and_then(PgValue::as_date), Some(d));
        }
        other => panic!("date: {other:?}"),
    }

    // TIMESTAMP
    let outcome = one(
        &runtime,
        &bridge,
        PgRequest::fetch_one("SELECT $1::TIMESTAMP").param(PgValue::from(dt)),
        Duration::from_secs(5),
        Duration::from_secs(10),
    );
    match outcome {
        CallOutcome::Replied(Ok(PgResponse::Row(row))) => {
            assert_eq!(row.col(0).and_then(PgValue::as_timestamp), Some(dt));
        }
        other => panic!("timestamp: {other:?}"),
    }

    // TIMESTAMPTZ
    let outcome = one(
        &runtime,
        &bridge,
        PgRequest::fetch_one("SELECT $1::TIMESTAMPTZ").param(PgValue::from(dtz)),
        Duration::from_secs(5),
        Duration::from_secs(10),
    );
    match outcome {
        CallOutcome::Replied(Ok(PgResponse::Row(row))) => {
            assert_eq!(row.col(0).and_then(PgValue::as_timestamptz), Some(dtz));
        }
        other => panic!("timestamptz: {other:?}"),
    }
    shutdown(runtime);
}

#[test]
#[ignore = "needs DATABASE_URL pointing at a real Postgres"]
fn close_with_in_flight_runs_to_completion_then_closes() {
    let url = match database_url() {
        Some(u) => u,
        None => return,
    };
    let runtime = make_runtime();
    let bridge = install(&runtime, &url);

    let sink = Arc::new(Sink::default());
    let caller_a = register_caller(
        &runtime,
        bridge.address,
        Arc::clone(&sink),
        Duration::from_secs(10),
    );
    let caller_b = register_caller(
        &runtime,
        bridge.address,
        Arc::clone(&sink),
        Duration::from_secs(10),
    );

    runtime
        .try_send(
            caller_a,
            CallerMsg::Run(PgRequest::execute("SELECT pg_sleep(0.5)")),
        )
        .expect("send a");
    // Let A reach the worker.
    std::thread::sleep(Duration::from_millis(100));
    bridge.closer.close();
    runtime
        .try_send(caller_b, CallerMsg::Run(PgRequest::execute("SELECT 1")))
        .expect("send b");

    let outcomes = sink.wait_n(2, Duration::from_secs(10));
    let mut saw_executed = false;
    let mut saw_closed = false;
    for o in outcomes {
        match o {
            CallOutcome::Replied(Ok(_)) => saw_executed = true,
            CallOutcome::Replied(Err(PgError::Closed)) => saw_closed = true,
            other => panic!("unexpected: {other:?}"),
        }
    }
    assert!(saw_executed, "A's in-flight work must complete");
    assert!(saw_closed, "B sent after close must see Closed");
    shutdown(runtime);
}
