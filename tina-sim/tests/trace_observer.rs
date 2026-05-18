//! Sim parity for the live trace observer hook.
//!
//! Pins the same contract as `tina-runtime`:
//!
//! - one callback per recorded event, in recording order;
//! - attaching a noop observer leaves the deterministic event record
//!   byte-identical (DST replay safeguard);
//! - the observer sees what the in-memory trace contains.

use std::convert::Infallible;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use tina::{Context, Effect, Isolate, Outbound, Shard, ShardId, stop};
use tina_runtime::{RuntimeCall, RuntimeEvent, TraceObserver, stable_trace_hash};
use tina_sim::{Simulator, SimulatorConfig};

#[derive(Debug, Default)]
struct ParityShard;

impl Shard for ParityShard {
    fn id(&self) -> ShardId {
        ShardId::new(0)
    }
}

#[derive(Debug)]
enum Msg {
    Stop,
}

#[derive(Debug)]
struct Tiny;

impl Isolate for Tiny {
    type Message = Msg;
    type Reply = ();
    type Send = Outbound<Infallible>;
    type Spawn = Infallible;
    type SpawnObserved = std::convert::Infallible;
    type Call = RuntimeCall<Msg>;
    type Fact = ::std::convert::Infallible;
    type Shard = ParityShard;

    fn handle(
        &mut self,
        _msg: Self::Message,
        _ctx: &mut Context<'_, Self::Shard, Self::Reply>,
    ) -> Effect<Self> {
        stop()
    }
}

fn run(observer: Option<Arc<dyn TraceObserver>>) -> Vec<RuntimeEvent> {
    let mut sim = Simulator::new(ParityShard, SimulatorConfig::default());
    sim.set_trace_observer(observer);
    let address = sim.register::<Tiny, Msg, Infallible>(Tiny);
    sim.try_send(address, Msg::Stop).expect("kick tiny");
    for _ in 0..32 {
        let delivered = sim.step();
        if delivered == 0 {
            break;
        }
    }
    sim.trace().to_vec()
}

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

#[test]
fn sim_observer_sees_every_recorded_event_in_order() {
    let collector = Arc::new(CollectingObserver::default());
    let observer: Arc<dyn TraceObserver> = collector.clone();
    let trace = run(Some(observer));
    let observed = collector.0.lock().unwrap().clone();
    assert_eq!(observed, trace, "observer events match the in-memory trace");
}

#[test]
fn sim_trace_is_byte_identical_with_and_without_noop_observer() {
    let baseline = run(None);
    let counter = Arc::new(CountingObserver::default());
    let observer: Arc<dyn TraceObserver> = counter.clone();
    let with_obs = run(Some(observer));
    assert_eq!(
        baseline, with_obs,
        "noop observer must not perturb sim trace"
    );
    assert_eq!(
        stable_trace_hash(baseline.iter()),
        stable_trace_hash(with_obs.iter()),
        "stable hash must match too",
    );
    assert!(counter.0.load(Ordering::Relaxed) > 0);
}

#[test]
fn sim_closure_observer_works_via_blanket_impl() {
    let count = Arc::new(AtomicUsize::new(0));
    let count_for_closure = Arc::clone(&count);
    let observer: Arc<dyn TraceObserver> = Arc::new(move |_event: &RuntimeEvent| {
        count_for_closure.fetch_add(1, Ordering::Relaxed);
    });
    let _ = run(Some(observer));
    assert!(count.load(Ordering::Relaxed) > 0);
}
