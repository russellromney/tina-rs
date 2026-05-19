//! Admission-class tests that need the worker isolate to be alive but
//! never reach SQLx itself. Uses `PgPool::connect_lazy` so no real
//! Postgres is required.

use std::convert::Infallible;
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use sqlx::postgres::PgPoolOptions;
use tina::prelude::*;
use tina_runtime::{
    CallOutcome, DefaultThreadedMailboxFactory, RuntimeCall, ThreadedRuntime, ThreadedRuntimeConfig,
};
use tina_sqlx_bridge::{
    InstallError, InstalledPgBridge, PgAddress, PgCallOutcome, PgConfig, PgError, PgRequest,
    PgStep, PgWorker, send_request,
};

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

/// `PgPool::connect_lazy` parses the URL but never connects. Together
/// with a Tokio handle from the bridge's owned runtime, this lets us
/// test admission paths (Closed, InvalidRequest) that never reach SQLx.
fn install_lazy(
    runtime: &ThreadedRuntime<SingleShard, DefaultThreadedMailboxFactory>,
    config: PgConfig,
) -> (InstalledPgBridge<SingleShard>, tokio::runtime::Runtime) {
    let tokio_rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .worker_threads(1)
        .thread_name("tina-sqlx-bridge-test")
        .build()
        .expect("tokio runtime");
    // SQLx 0.8 spawns maintenance tasks at pool construction; that
    // requires an active Tokio context.
    let pool = {
        let _entered = tokio_rt.handle().enter();
        PgPoolOptions::new()
            .max_connections(2)
            .acquire_timeout(Duration::from_millis(500))
            .connect_lazy("postgres://test:test@127.0.0.1:1/test")
            .expect("lazy pool URL parse")
    };
    let bridge = PgWorker::<SingleShard>::install_with_pool(
        runtime,
        config,
        pool,
        tokio_rt.handle().clone(),
    )
    .expect("install_with_pool");
    (bridge, tokio_rt)
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

fn wait_for_external_idle(metrics: &tina_sqlx_bridge::PgMetricsHandle, timeout: Duration) {
    let deadline = Instant::now() + timeout;
    loop {
        if metrics.snapshot().external_in_flight_current == 0 {
            return;
        }
        if Instant::now() >= deadline {
            panic!(
                "external work did not settle before {timeout:?}: {:?}",
                metrics.snapshot()
            );
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn config_for_admission_tests() -> PgConfig {
    PgConfig::bridge_only()
        .with_default_timeout(Duration::from_millis(200))
        .with_poll_interval(Duration::from_millis(1))
        .with_max_in_flight(2)
        .with_max_request_params(4)
}

// ---------------------------------------------------------------------------
// install/install_with_pool ownership rules
// ---------------------------------------------------------------------------

#[test]
fn install_without_pool_config_errors_with_missing_pool_config() {
    let runtime = make_runtime();
    let cfg = PgConfig::bridge_only()
        .with_default_timeout(Duration::from_secs(1))
        .with_poll_interval(Duration::from_millis(1));
    let err = match PgWorker::<SingleShard>::install(&runtime, cfg) {
        Ok(_) => panic!("install should reject"),
        Err(e) => e,
    };
    assert!(
        matches!(err, InstallError::MissingPoolConfig),
        "got {err:?}",
    );
    shutdown(runtime);
}

#[test]
fn install_with_pool_silently_ignores_cancel_config() {
    // Caller may build a config with cancel set (e.g. from a shared
    // template) but install with a supplied pool. The bridge does
    // not error — it just builds without a sidecar. Cancel is a
    // no-op on this path. Lock that contract: install succeeds, no
    // panic, no cancel firing later.
    let runtime = make_runtime();
    let cfg = config_for_admission_tests().with_cancel_on_timeout(1);
    let (bridge, _tokio_rt) = install_lazy(&runtime, cfg);
    // The bridge is alive; closing exercises drop ordering.
    bridge.closer.close();
    assert_eq!(bridge.metrics.snapshot().db_cancels_sent, 0);
    shutdown(runtime);
}

#[test]
fn install_with_invalid_config_errors_with_config() {
    let runtime = make_runtime();
    let cfg = PgConfig::bridge_only().with_default_timeout(Duration::ZERO);
    let err = match PgWorker::<SingleShard>::install(&runtime, cfg) {
        Ok(_) => panic!("install should reject"),
        Err(e) => e,
    };
    assert!(matches!(err, InstallError::Config(_)), "got {err:?}");
    shutdown(runtime);
}

// ---------------------------------------------------------------------------
// Pool consumer shape: bridge-as-pool proofs
// ---------------------------------------------------------------------------

#[test]
fn full_is_distinct_from_pool_acquire_timeout() {
    let runtime = make_runtime();
    let cfg = PgConfig::bridge_only()
        .with_default_timeout(Duration::from_secs(2))
        .with_poll_interval(Duration::from_millis(1))
        .with_max_in_flight(1);
    let (bridge, _tokio_rt) = install_lazy(&runtime, cfg);

    let sink1 = Arc::new(Sink::default());
    let caller1 = register_caller(
        &runtime,
        bridge.address,
        Arc::clone(&sink1),
        Duration::from_secs(2),
    );
    runtime
        .try_send(caller1, CallerMsg::Run(PgRequest::execute("SELECT 1")))
        .expect("send req1");

    let sink2 = Arc::new(Sink::default());
    let caller2 = register_caller(
        &runtime,
        bridge.address,
        Arc::clone(&sink2),
        Duration::from_secs(2),
    );
    runtime
        .try_send(caller2, CallerMsg::Run(PgRequest::execute("SELECT 2")))
        .expect("send req2");

    let out2 = sink2.wait_one(Duration::from_secs(3));
    assert!(
        matches!(out2, CallOutcome::Replied(Err(PgError::Full))),
        "expected Full, got {out2:?}"
    );

    let out1 = sink1.wait_one(Duration::from_secs(3));
    assert!(
        matches!(out1, CallOutcome::Replied(Err(PgError::PoolAcquireTimeout))),
        "expected PoolAcquireTimeout, got {out1:?}"
    );

    let m = bridge.metrics.snapshot();
    assert_eq!(m.full, 1);
    assert_eq!(m.pool_acquire_timeouts, 1);
    shutdown(runtime);
}

#[test]
fn timeout_settles_caller_but_keeps_external_capacity_until_terminal() {
    let runtime = make_runtime();
    let cfg = PgConfig::bridge_only()
        .with_default_timeout(Duration::from_millis(30))
        .with_poll_interval(Duration::from_millis(1))
        .with_max_in_flight(1);
    let (bridge, _tokio_rt) = install_lazy(&runtime, cfg);

    let sink1 = Arc::new(Sink::default());
    let caller1 = register_caller(
        &runtime,
        bridge.address,
        Arc::clone(&sink1),
        Duration::from_secs(2),
    );
    runtime
        .try_send(caller1, CallerMsg::Run(PgRequest::execute("SELECT 1")))
        .expect("send req1");

    let out1 = sink1.wait_one(Duration::from_secs(3));
    assert!(
        matches!(out1, CallOutcome::Replied(Err(PgError::Timeout))),
        "expected Timeout, got {out1:?}"
    );

    let sink2 = Arc::new(Sink::default());
    let caller2 = register_caller(
        &runtime,
        bridge.address,
        Arc::clone(&sink2),
        Duration::from_secs(2),
    );
    runtime
        .try_send(caller2, CallerMsg::Run(PgRequest::execute("SELECT 2")))
        .expect("send req2");

    let out2 = sink2.wait_one(Duration::from_secs(3));
    assert!(
        matches!(out2, CallOutcome::Replied(Err(PgError::Full))),
        "expected Full while late external work still owns capacity, got {out2:?}"
    );

    let m = bridge.metrics.snapshot();
    assert_eq!(m.timeouts, 1);
    assert_eq!(m.admitted, 1, "timeout must not free external capacity");
    assert_eq!(m.full, 1);
    assert_eq!(m.caller_waiting_current, 0);
    assert_eq!(m.external_in_flight_current, 1);
    assert_eq!(m.late_in_flight_current, 1);
    let pressure = bridge.metrics.pressure_report();
    assert_eq!(pressure.leased, 1);
    assert_eq!(pressure.available, 0);
    assert_eq!(pressure.waiters, 0);

    wait_for_external_idle(&bridge.metrics, Duration::from_secs(2));
    let settled = bridge.metrics.snapshot();
    assert_eq!(settled.external_in_flight_current, 0);
    assert_eq!(settled.late_in_flight_current, 0);
    assert_eq!(settled.late_results, 1);
    shutdown(runtime);
}

#[test]
fn error_does_not_retire_lane() {
    let runtime = make_runtime();
    let cfg = PgConfig::bridge_only()
        .with_default_timeout(Duration::from_secs(2))
        .with_poll_interval(Duration::from_millis(1))
        .with_max_in_flight(1);
    let (bridge, _tokio_rt) = install_lazy(&runtime, cfg);

    let sink1 = Arc::new(Sink::default());
    let caller1 = register_caller(
        &runtime,
        bridge.address,
        Arc::clone(&sink1),
        Duration::from_secs(2),
    );
    runtime
        .try_send(caller1, CallerMsg::Run(PgRequest::execute("SELECT 1")))
        .expect("send req1");
    let out1 = sink1.wait_one(Duration::from_secs(3));
    assert!(
        matches!(out1, CallOutcome::Replied(Err(PgError::PoolAcquireTimeout))),
        "expected PoolAcquireTimeout, got {out1:?}"
    );

    let sink2 = Arc::new(Sink::default());
    let caller2 = register_caller(
        &runtime,
        bridge.address,
        Arc::clone(&sink2),
        Duration::from_secs(2),
    );
    runtime
        .try_send(caller2, CallerMsg::Run(PgRequest::execute("SELECT 2")))
        .expect("send req2");
    let out2 = sink2.wait_one(Duration::from_secs(3));
    assert!(
        matches!(out2, CallOutcome::Replied(Err(PgError::PoolAcquireTimeout))),
        "expected PoolAcquireTimeout, got {out2:?}"
    );

    let m = bridge.metrics.snapshot();
    assert_eq!(m.pool_acquire_timeouts, 2);
    assert_eq!(m.full, 0, "no Full because lane was reused after error");
    shutdown(runtime);
}

#[test]
fn pressure_report_reflects_capacity_and_high_water() {
    let runtime = make_runtime();
    let cfg = PgConfig::bridge_only()
        .with_default_timeout(Duration::from_secs(2))
        .with_poll_interval(Duration::from_millis(1))
        .with_max_in_flight(3);
    let (bridge, _tokio_rt) = install_lazy(&runtime, cfg.clone());

    let r0 = bridge.metrics.pressure_report();
    assert_eq!(r0.capacity, 3);
    assert_eq!(r0.available, 3);
    assert_eq!(r0.leased, 0);
    assert_eq!(r0.high_water, 0);
    assert!(!r0.is_full());

    // Send 3 requests. The lazy pool times out quickly (50 ms), so each
    // request cycles fast. We wait for completion and check accumulated
    // metrics — no race on concurrent state.
    let sinks: Vec<_> = (0..3)
        .map(|_| {
            let s = Arc::new(Sink::default());
            let c = register_caller(
                &runtime,
                bridge.address,
                Arc::clone(&s),
                Duration::from_secs(5),
            );
            runtime
                .try_send(c, CallerMsg::Run(PgRequest::execute("SELECT 1")))
                .expect("send");
            s
        })
        .collect();

    for s in &sinks {
        let _ = s.wait_one(Duration::from_secs(5));
    }

    let r1 = bridge.metrics.pressure_report();
    assert_eq!(r1.capacity, 3);
    assert_eq!(r1.leased, 0, "all done");
    assert_eq!(r1.available, 3);
    assert_eq!(r1.high_water, 3, "peak was 3 concurrent");
    assert!(!r1.is_full());

    // Send 4 more requests; at least one sees Full.
    let sinks2: Vec<_> = (0..4)
        .map(|_| {
            let s = Arc::new(Sink::default());
            let c = register_caller(
                &runtime,
                bridge.address,
                Arc::clone(&s),
                Duration::from_secs(5),
            );
            runtime
                .try_send(c, CallerMsg::Run(PgRequest::execute("SELECT 1")))
                .expect("send");
            s
        })
        .collect();

    for s in &sinks2 {
        let _ = s.wait_one(Duration::from_secs(5));
    }

    let r2 = bridge.metrics.pressure_report();
    assert_eq!(r2.high_water, 3, "peak stays at 3");
    assert_eq!(r2.full_count, 1, "one request was rejected");
    assert_eq!(r2.leased, 0);
    assert_eq!(r2.available, 3);
    assert!(!r2.is_full());

    shutdown(runtime);
}

// ---------------------------------------------------------------------------
// install/install_with_pool ownership rules
// ---------------------------------------------------------------------------
// Admission rejection
// ---------------------------------------------------------------------------

#[test]
fn closed_bridge_replies_closed() {
    let runtime = make_runtime();
    let (bridge, _tokio_rt) = install_lazy(&runtime, config_for_admission_tests());
    bridge.closer.close();
    assert!(bridge.closer.is_closed());

    let sink = Arc::new(Sink::default());
    let caller = register_caller(
        &runtime,
        bridge.address,
        Arc::clone(&sink),
        Duration::from_secs(2),
    );
    runtime
        .try_send(caller, CallerMsg::Run(PgRequest::execute("SELECT 1")))
        .expect("send");
    let outcome = sink.wait_one(Duration::from_secs(3));
    assert!(
        matches!(outcome, CallOutcome::Replied(Err(PgError::Closed))),
        "got {outcome:?}",
    );
    let m = bridge.metrics.snapshot();
    assert_eq!(m.closed, 1);
    assert_eq!(m.admitted, 0);
    shutdown(runtime);
}

#[test]
fn empty_sql_replies_invalid_request() {
    let runtime = make_runtime();
    let (bridge, _tokio_rt) = install_lazy(&runtime, config_for_admission_tests());

    let sink = Arc::new(Sink::default());
    let caller = register_caller(
        &runtime,
        bridge.address,
        Arc::clone(&sink),
        Duration::from_secs(2),
    );
    runtime
        .try_send(caller, CallerMsg::Run(PgRequest::execute("   ")))
        .expect("send");
    let outcome = sink.wait_one(Duration::from_secs(3));
    match outcome {
        CallOutcome::Replied(Err(PgError::InvalidRequest(msg))) => {
            assert!(msg.contains("empty"), "expected 'empty' detail, got {msg}");
        }
        other => panic!("expected InvalidRequest, got {other:?}"),
    }
    assert_eq!(bridge.metrics.snapshot().invalid, 1);
    shutdown(runtime);
}

#[test]
fn empty_transaction_replies_invalid_request() {
    let runtime = make_runtime();
    let (bridge, _tokio_rt) = install_lazy(&runtime, config_for_admission_tests());

    let sink = Arc::new(Sink::default());
    let caller = register_caller(
        &runtime,
        bridge.address,
        Arc::clone(&sink),
        Duration::from_secs(2),
    );
    runtime
        .try_send(caller, CallerMsg::Run(PgRequest::transaction(vec![])))
        .expect("send");
    let outcome = sink.wait_one(Duration::from_secs(3));
    match outcome {
        CallOutcome::Replied(Err(PgError::InvalidRequest(msg))) => {
            assert!(msg.contains("no steps"), "got {msg}");
        }
        other => panic!("expected InvalidRequest, got {other:?}"),
    }
    shutdown(runtime);
}

#[test]
fn transaction_with_empty_step_sql_replies_invalid_request_indexed() {
    let runtime = make_runtime();
    let (bridge, _tokio_rt) = install_lazy(&runtime, config_for_admission_tests());

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
            CallerMsg::Run(PgRequest::transaction(vec![
                PgStep::execute("SELECT 1"),
                PgStep::execute("   "),
            ])),
        )
        .expect("send");
    let outcome = sink.wait_one(Duration::from_secs(3));
    match outcome {
        CallOutcome::Replied(Err(PgError::InvalidRequest(msg))) => {
            assert!(
                msg.contains("step 1"),
                "expected indexed message, got {msg}"
            );
            assert!(msg.contains("empty"), "got {msg}");
        }
        other => panic!("expected InvalidRequest, got {other:?}"),
    }
    shutdown(runtime);
}

#[test]
fn too_many_params_replies_invalid_request() {
    let runtime = make_runtime();
    let (bridge, _tokio_rt) = install_lazy(&runtime, config_for_admission_tests());

    let sink = Arc::new(Sink::default());
    let caller = register_caller(
        &runtime,
        bridge.address,
        Arc::clone(&sink),
        Duration::from_secs(2),
    );

    let mut req = PgRequest::execute("SELECT 1");
    for i in 0..10 {
        req = req.param(i as i64);
    }
    runtime.try_send(caller, CallerMsg::Run(req)).expect("send");
    let outcome = sink.wait_one(Duration::from_secs(3));
    match outcome {
        CallOutcome::Replied(Err(PgError::InvalidRequest(msg))) => {
            assert!(
                msg.contains("params"),
                "expected 'params' detail, got {msg}"
            );
        }
        other => panic!("expected InvalidRequest, got {other:?}"),
    }
    shutdown(runtime);
}
