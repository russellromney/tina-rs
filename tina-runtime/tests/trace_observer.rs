//! Live trace observer contract.
//!
//! Pins the rules from `tina_runtime::TraceObserver`:
//!
//! - one callback per recorded event, in recording order;
//! - fires before retention, so `TraceRetention::Off` does not silence
//!   the observer;
//! - per-shard ordering; cross-shard order is not promised;
//! - attaching a noop observer leaves the deterministic trace record
//!   byte-identical (sim/live parity safeguard).

use std::convert::Infallible;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tina::prelude::*;
use tina_runtime::{
    DefaultMailboxFactory, DefaultThreadedMailboxFactory, Runtime, RuntimeEvent, RuntimeEventKind,
    ThreadedRuntime, ThreadedRuntimeConfig, TraceObserver, TraceRetention, sleep,
};

#[derive(Debug, Default)]
struct CountingObserver(AtomicUsize);

impl TraceObserver for CountingObserver {
    fn on_event(&self, _event: &RuntimeEvent) {
        self.0.fetch_add(1, Ordering::Relaxed);
    }
}

#[derive(Debug, Default)]
struct CollectingObserver(Mutex<Vec<RuntimeEvent>>);

impl TraceObserver for CollectingObserver {
    fn on_event(&self, event: &RuntimeEvent) {
        self.0.lock().expect("collector lock").push(*event);
    }
}

#[derive(Debug, Clone)]
enum Msg {
    Begin,
    Done,
}

struct Tiny;

#[tina_runtime::isolate(message = Msg)]
impl Tiny {
    fn handle(
        &mut self,
        msg: Msg,
        _ctx: &mut Context<'_, SingleShard, Self::Reply>,
    ) -> Effect<Self> {
        match msg {
            Msg::Begin => sleep(Duration::ZERO).then(|_| Msg::Done),
            Msg::Done => stop(),
        }
    }
}

fn run_explicit_step_with_observer(
    observer: Option<Arc<dyn TraceObserver>>,
    retention: TraceRetention,
) -> Vec<RuntimeEvent> {
    let mut runtime = Runtime::new(SingleShard, DefaultMailboxFactory);
    runtime.set_trace_retention(retention);
    runtime.set_trace_observer(observer);
    let addr = runtime.register_with_capacity::<Tiny, Infallible>(Tiny, 4);
    runtime.try_send(addr, Msg::Begin).expect("kick tiny");
    for _ in 0..64 {
        runtime.step();
        if !runtime.has_in_flight_calls() {
            break;
        }
    }
    runtime.trace().to_vec()
}

#[test]
fn explicit_step_observer_sees_every_recorded_event() {
    let collector = Arc::new(CollectingObserver::default());
    let observer: Arc<dyn TraceObserver> = collector.clone();
    let trace = run_explicit_step_with_observer(Some(observer), TraceRetention::Full);
    let observed = collector.0.lock().unwrap().clone();
    assert_eq!(observed, trace, "observer events match the in-memory trace");
}

#[test]
fn observer_fires_when_retention_is_off() {
    let counter = Arc::new(CountingObserver::default());
    let observer: Arc<dyn TraceObserver> = counter.clone();
    let trace = run_explicit_step_with_observer(Some(observer), TraceRetention::Off);
    assert!(trace.is_empty(), "Off keeps no in-memory trace");
    assert!(
        counter.0.load(Ordering::Relaxed) > 0,
        "observer fires anyway",
    );
}

#[test]
fn explicit_step_trace_is_byte_identical_with_and_without_noop_observer() {
    let baseline = run_explicit_step_with_observer(None, TraceRetention::Full);
    let counter = Arc::new(CountingObserver::default());
    let observer: Arc<dyn TraceObserver> = counter.clone();
    let with_obs = run_explicit_step_with_observer(Some(observer), TraceRetention::Full);
    assert_eq!(
        baseline, with_obs,
        "noop observer must not perturb the deterministic trace"
    );
    assert!(counter.0.load(Ordering::Relaxed) > 0);
}

#[test]
fn closure_observer_works_via_blanket_impl() {
    let count = Arc::new(AtomicUsize::new(0));
    let count_for_closure = Arc::clone(&count);
    let observer: Arc<dyn TraceObserver> = Arc::new(move |_event: &RuntimeEvent| {
        count_for_closure.fetch_add(1, Ordering::Relaxed);
    });
    let _ = run_explicit_step_with_observer(Some(observer), TraceRetention::Full);
    assert!(count.load(Ordering::Relaxed) > 0);
}

#[test]
fn threaded_runtime_observer_fires_on_worker_thread() {
    let collector = Arc::new(CollectingObserver::default());
    let observer: Arc<dyn TraceObserver> = collector.clone();
    let runtime = ThreadedRuntime::with_config_and_trace_observer(
        SingleShard,
        DefaultThreadedMailboxFactory,
        ThreadedRuntimeConfig::default(),
        observer,
    );
    let addr = runtime
        .register_with_capacity::<Tiny, Infallible>(Tiny, 4)
        .expect("register tiny");
    runtime.try_send(addr, Msg::Begin).expect("kick tiny");
    let _ = runtime.shutdown();
    let observed = collector.0.lock().unwrap().clone();
    assert!(
        observed
            .iter()
            .any(|e| matches!(e.kind(), RuntimeEventKind::IsolateStopped)),
        "observer saw isolate stop",
    );
    assert!(
        observed
            .iter()
            .any(|e| matches!(e.kind(), RuntimeEventKind::HandlerStarted)),
        "observer saw handler start",
    );
}
