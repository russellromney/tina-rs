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
            .acquire_timeout(Duration::from_millis(50))
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
