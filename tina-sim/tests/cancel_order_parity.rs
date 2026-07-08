//! Requester-stop cancel ordering, pinned on both engines.
//!
//! When a requester with several in-flight driver calls stops, the runtime
//! cancels them in ascending `CallId` (== insertion) order. #255 replaced a
//! `swap_remove` sweep — whose emission order was neither insertion nor sorted
//! once three or more calls were live — with a `BTreeMap` sweep that is always
//! ascending, and mirrored the change into the simulator to remove a latent
//! runtime/sim asymmetry.
//!
//! These tests are the load-bearing pin: they create a requester with four
//! concurrent in-flight `sleep` calls, stop it, and assert the emitted
//! `CallCompletionRejected{RequesterClosed}` events come out strictly
//! ascending — on the live `Runtime` and on the `Simulator` — and that the two
//! engines agree. A revert to `swap_remove` breaks the strictly-ascending
//! assertion (the sweep would emit 1, 4, 3, 2 for four contiguous ids).

use std::cell::RefCell;
use std::collections::VecDeque;
use std::convert::Infallible;
use std::time::Duration;

use tina::{Mailbox, TrySendError, batch, prelude::*};
use tina_runtime::{
    CallCompletionRejectedReason, MailboxFactory, Runtime, RuntimeEvent, RuntimeEventKind, sleep,
};
use tina_sim::{Simulator, SimulatorConfig};

#[derive(Debug, Clone, Copy)]
struct TestShard;

impl Shard for TestShard {
    fn id(&self) -> ShardId {
        ShardId::new(5)
    }
}

struct TestMailbox<T> {
    capacity: usize,
    queue: RefCell<VecDeque<T>>,
    closed: RefCell<bool>,
}

impl<T> TestMailbox<T> {
    fn new(capacity: usize) -> Self {
        Self {
            capacity,
            queue: RefCell::new(VecDeque::new()),
            closed: RefCell::new(false),
        }
    }
}

impl<T> Mailbox<T> for TestMailbox<T> {
    fn capacity(&self) -> usize {
        self.capacity
    }
    fn try_send(&self, message: T) -> Result<(), TrySendError<T>> {
        if *self.closed.borrow() {
            return Err(TrySendError::Closed(message));
        }
        let mut queue = self.queue.borrow_mut();
        if queue.len() >= self.capacity {
            return Err(TrySendError::Full(message));
        }
        queue.push_back(message);
        Ok(())
    }
    fn recv(&self) -> Option<T> {
        self.queue.borrow_mut().pop_front()
    }
    fn is_empty(&self) -> bool {
        self.queue.borrow().is_empty()
    }
    fn close(&self) {
        *self.closed.borrow_mut() = true;
    }
}

#[derive(Debug, Clone, Copy)]
struct TestMailboxFactory;

impl MailboxFactory for TestMailboxFactory {
    fn create<T: 'static>(&self, capacity: usize) -> Box<dyn Mailbox<T>> {
        Box::new(TestMailbox::new(capacity))
    }
}

/// Number of concurrent in-flight driver calls. Three or more is required to
/// distinguish ascending emission from a `swap_remove` sweep.
const IN_FLIGHT: usize = 4;

#[derive(Debug)]
enum FleetMsg {
    Setup,
    Stop,
    Woke,
}

/// Issues `IN_FLIGHT` long sleeps on `Setup` (all stay pending), stops on
/// `Stop`. The sleeps never fire during the test, so the only way they leave
/// the table is the requester-stop cancel sweep.
struct SleepFleet;

#[tina_runtime::isolate(message = FleetMsg, shard = TestShard)]
impl SleepFleet {
    fn handle(
        &mut self,
        msg: FleetMsg,
        _ctx: &mut Context<'_, TestShard, Self::Reply>,
    ) -> Effect<Self> {
        match msg {
            FleetMsg::Setup => batch(
                (0..IN_FLIGHT)
                    .map(|_| sleep(Duration::from_secs(3600)).then_event(|| FleetMsg::Woke))
                    .collect::<Vec<_>>(),
            ),
            FleetMsg::Stop => stop(),
            FleetMsg::Woke => noop(),
        }
    }
}

/// Call ids of `CallCompletionRejected{RequesterClosed}` events, in trace order.
fn requester_closed_call_ids(trace: &[RuntimeEvent]) -> Vec<u64> {
    trace
        .iter()
        .filter_map(|event| match event.kind() {
            RuntimeEventKind::CallCompletionRejected {
                call_id,
                reason: CallCompletionRejectedReason::RequesterClosed,
                ..
            } => Some(call_id.get()),
            _ => None,
        })
        .collect()
}

fn run_oracle() -> Vec<u64> {
    let mut runtime = Runtime::new(TestShard, TestMailboxFactory);
    let fleet = runtime.register_with_capacity::<SleepFleet, Infallible>(SleepFleet, 8);
    runtime.try_send(fleet, FleetMsg::Setup).expect("setup");
    runtime.step();
    runtime.try_send(fleet, FleetMsg::Stop).expect("stop");
    runtime.step();
    requester_closed_call_ids(runtime.trace())
}

fn run_simulator() -> Vec<u64> {
    let mut sim = Simulator::new(TestShard, SimulatorConfig::default());
    let fleet =
        sim.register_with_mailbox_capacity::<SleepFleet, FleetMsg, Infallible>(SleepFleet, 8);
    sim.try_send(fleet, FleetMsg::Setup).expect("setup");
    sim.step();
    sim.try_send(fleet, FleetMsg::Stop).expect("stop");
    sim.step();
    requester_closed_call_ids(sim.trace())
}

fn assert_ascending(label: &str, ids: &[u64]) {
    assert_eq!(
        ids.len(),
        IN_FLIGHT,
        "{label}: every in-flight call must settle as RequesterClosed, got {ids:?}"
    );
    assert!(
        ids.windows(2).all(|w| w[0] < w[1]),
        "{label}: requester-stop cancellations must be strictly ascending by call id, got {ids:?}"
    );
}

#[test]
fn requester_stop_cancels_in_ascending_call_id_order_on_runtime() {
    assert_ascending("runtime", &run_oracle());
}

#[test]
fn requester_stop_cancels_in_ascending_call_id_order_on_simulator() {
    assert_ascending("simulator", &run_simulator());
}

fn quarantine_count(trace: &[RuntimeEvent]) -> usize {
    trace
        .iter()
        .filter(|event| {
            matches!(
                event.kind(),
                RuntimeEventKind::DriverCompletionQuarantined { .. }
            )
        })
        .count()
}

#[test]
fn simulator_purges_pending_completions_for_stopped_requester_without_quarantine() {
    // Sim analogue of the runtime's `carried_completion_for_stopped_requester_is
    // _dropped_not_quarantined`: a backend completion (here a pending timer)
    // whose requester stops is a NORMAL race, not a driver bug. The stop must
    // purge the pending timer so a later time advance cannot fire it against a
    // call whose entry is gone (which would quarantine and falsely accuse the
    // driver). It settles as RequesterClosed instead.
    let mut sim = Simulator::new(TestShard, SimulatorConfig::default());
    let fleet =
        sim.register_with_mailbox_capacity::<SleepFleet, FleetMsg, Infallible>(SleepFleet, 8);
    sim.try_send(fleet, FleetMsg::Setup).expect("setup");
    sim.step();
    sim.try_send(fleet, FleetMsg::Stop).expect("stop");
    sim.step();

    // Advance well past every sleep deadline and keep stepping: a purged timer
    // must never fire.
    sim.advance_time(Duration::from_secs(7200));
    for _ in 0..4 {
        sim.step();
    }

    assert_eq!(
        quarantine_count(sim.trace()),
        0,
        "a purged pending completion for a stopped requester must not quarantine"
    );
    assert_ascending("simulator", &requester_closed_call_ids(sim.trace()));
}

#[test]
fn runtime_and_simulator_agree_on_cancel_ordering() {
    let oracle = run_oracle();
    let sim = run_simulator();
    assert_ascending("runtime", &oracle);
    assert_ascending("simulator", &sim);
    // Both engines mint ascending ids from the same starting point for this
    // single isolate, so the exact id sequences match — the parity the #255
    // mirror is meant to guarantee.
    assert_eq!(
        oracle, sim,
        "runtime and simulator must emit the same cancel ordering"
    );
}

/// Trace-shape differential across both engines for the requester-stop
/// workload: cancel ordering plus the carried-completion-purge property (no
/// quarantine). A stopped requester's in-flight driver calls are exactly the
/// "carried completions" the #255 self-review worried about; neither engine may
/// let one leak into `deliver_completion`'s quarantine.
#[test]
fn runtime_and_simulator_agree_on_cancel_shape_without_quarantine() {
    let oracle_trace = oracle_trace();
    let sim_trace = simulator_trace();

    // Same ordered outcome shape.
    assert_eq!(
        requester_closed_call_ids(&oracle_trace),
        requester_closed_call_ids(&sim_trace),
        "runtime and simulator must emit the same requester-stop cancel shape"
    );
    // Neither engine quarantines a stopped requester's pending completion.
    assert_eq!(
        quarantine_count(&oracle_trace),
        0,
        "runtime must not quarantine a stopped requester's carried completions"
    );
    assert_eq!(
        quarantine_count(&sim_trace),
        0,
        "simulator must not quarantine a stopped requester's carried completions"
    );
}

fn oracle_trace() -> Vec<RuntimeEvent> {
    let mut runtime = Runtime::new(TestShard, TestMailboxFactory);
    let fleet = runtime.register_with_capacity::<SleepFleet, Infallible>(SleepFleet, 8);
    runtime.try_send(fleet, FleetMsg::Setup).expect("setup");
    runtime.step();
    runtime.try_send(fleet, FleetMsg::Stop).expect("stop");
    runtime.step();
    runtime.trace().to_vec()
}

fn simulator_trace() -> Vec<RuntimeEvent> {
    let mut sim = Simulator::new(TestShard, SimulatorConfig::default());
    let fleet =
        sim.register_with_mailbox_capacity::<SleepFleet, FleetMsg, Infallible>(SleepFleet, 8);
    sim.try_send(fleet, FleetMsg::Setup).expect("setup");
    sim.step();
    sim.try_send(fleet, FleetMsg::Stop).expect("stop");
    sim.step();
    sim.trace().to_vec()
}
