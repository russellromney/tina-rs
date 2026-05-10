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
    PgResponse, PgWorker, send_request,
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
