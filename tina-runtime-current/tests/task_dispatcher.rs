//! End-to-end task-dispatcher proof for `CurrentRuntime`.
//!
//! This integration test is the user-facing payoff for slices 005-010. It
//! demonstrates that the single-shard runtime can keep useful work moving
//! after a supervised worker panics, across all three restart policies plus
//! a budget-exhaustion case, and that two identical scripted runs produce
//! identical traces and identical completed-work logs.
//!
//! Pattern note for future readers:
//!
//! - The `Dispatcher` isolate is the supervised parent. Clients submit tasks
//!   to the dispatcher, and the dispatcher delegates slot resolution to the
//!   registry isolate before work reaches the current worker incarnation.
//! - The `Registry` isolate is a tiny user-space "name server" that holds
//!   current worker addresses keyed by stable slot. The dispatcher sends
//!   `Forward` requests to the registry and the registry forwards to the
//!   current incarnation by `Effect::Send`.
//! - After a supervised restart, the test reads the runtime trace, finds
//!   the `RestartChildCompleted` event for each replaced worker, and sends
//!   `Register` messages to refresh the registry.
//!
//! Production code should follow this shape: a registry isolate, refreshed
//! through messages, not by parsing trace events. The test parses the trace
//! only because it has no other way to discover replacement addresses (the
//! runtime intentionally exposes none — that is the slice 007 "logical
//! naming is user-space" commitment).
//!
//! What this test is not:
//!
//! - It is not the deterministic simulator (`tina-sim`); it drives the live
//!   `CurrentRuntime` and asserts on real runtime state.
//! - It is not a production routing pattern; the registry-isolate shape is
//!   the reference, but real apps may want richer name resolution.
//! - It is not the only proof of supervision correctness. Focused unit
//!   tests in `tina-runtime-current/src/lib.rs` and the generated-history
//!   property tests in `runtime_properties.rs` remain the primary semantic
//!   surface. This test proves the user story end-to-end.

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, VecDeque};
use std::convert::Infallible;
use std::rc::Rc;

use tina::{
    Address, AddressGeneration, Context, Effect, Isolate, IsolateId, Mailbox, RestartBudget,
    RestartPolicy, RestartableSpawnSpec, SendMessage, Shard, ShardId, TrySendError,
};
use tina_runtime_current::{
    CurrentRuntime, MailboxFactory, RuntimeEvent, RuntimeEventKind, SendRejectedReason,
};
use tina_supervisor::SupervisorConfig;

// ---------------------------------------------------------------------------
// Test infrastructure: shard, mailbox, mailbox factory.
// ---------------------------------------------------------------------------

#[derive(Debug, Default)]
struct TestShard;

impl Shard for TestShard {
    fn id(&self) -> ShardId {
        ShardId::new(7)
    }
}

struct TestMailbox<T> {
    capacity: usize,
    queue: Rc<RefCell<VecDeque<T>>>,
    closed: Rc<Cell<bool>>,
}

impl<T> Clone for TestMailbox<T> {
    fn clone(&self) -> Self {
        Self {
            capacity: self.capacity,
            queue: Rc::clone(&self.queue),
            closed: Rc::clone(&self.closed),
        }
    }
}

impl<T> TestMailbox<T> {
    fn new(capacity: usize) -> Self {
        Self {
            capacity,
            queue: Rc::new(RefCell::new(VecDeque::new())),
            closed: Rc::new(Cell::new(false)),
        }
    }
}

impl<T> Mailbox<T> for TestMailbox<T> {
    fn capacity(&self) -> usize {
        self.capacity
    }

    fn try_send(&self, message: T) -> Result<(), TrySendError<T>> {
        if self.closed.get() {
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

    fn close(&self) {
        self.closed.set(true);
    }
}

#[derive(Debug, Clone, Copy)]
struct TestMailboxFactory;

impl MailboxFactory for TestMailboxFactory {
    fn create<T: 'static>(&self, capacity: usize) -> Box<dyn Mailbox<T>> {
        Box::new(TestMailbox::new(capacity))
    }
}

// ---------------------------------------------------------------------------
// Workload messages and tasks.
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum NeverOutbound {}

/// A unit of work. `Normal(value)` records `(worker_id, value)` into the
/// shared completed-work log; `Poison` panics inside the worker handler.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Task {
    Normal(u32),
    Poison,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WorkerMsg {
    Run(Task),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DispatcherMsg {
    SpawnWorker,
    Submit { slot: u32, task: Task },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RegistryMsg {
    Register {
        slot: u32,
        address: Address<WorkerMsg>,
    },
    Forward {
        slot: u32,
        task: Task,
    },
}

// ---------------------------------------------------------------------------
// Worker isolate.
//
// Effect contract (per slice 011 plan):
//   - normal task -> Effect::Noop
//   - poison task -> panic
//   - no Reply, no Send
// ---------------------------------------------------------------------------

#[derive(Debug)]
struct Worker {
    completed: Rc<RefCell<Vec<(IsolateId, u32)>>>,
}

impl Isolate for Worker {
    type Message = WorkerMsg;
    type Reply = ();
    type Send = SendMessage<NeverOutbound>;
    type Spawn = Infallible;
    type Call = Infallible;
    type Shard = TestShard;

    fn handle(&mut self, msg: Self::Message, ctx: &mut Context<'_, Self::Shard>) -> Effect<Self> {
        match msg {
            WorkerMsg::Run(Task::Normal(value)) => {
                self.completed.borrow_mut().push((ctx.isolate_id(), value));
                Effect::Noop
            }
            WorkerMsg::Run(Task::Poison) => {
                panic!("poison task in worker handler");
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Dispatcher isolate.
//
// The dispatcher is the supervised parent and the user-facing ingress for
// task submission. It does not own a routing table; instead it delegates
// slot resolution to the registry isolate.
// ---------------------------------------------------------------------------

#[derive(Debug)]
struct Dispatcher {
    registry: Address<RegistryMsg>,
    worker_capacity: usize,
    completed: Rc<RefCell<Vec<(IsolateId, u32)>>>,
}

impl Isolate for Dispatcher {
    type Message = DispatcherMsg;
    type Reply = ();
    type Send = SendMessage<RegistryMsg>;
    type Spawn = RestartableSpawnSpec<Worker>;
    type Call = Infallible;
    type Shard = TestShard;

    fn handle(&mut self, msg: Self::Message, _ctx: &mut Context<'_, Self::Shard>) -> Effect<Self> {
        match msg {
            DispatcherMsg::SpawnWorker => {
                let completed = Rc::clone(&self.completed);
                Effect::Spawn(RestartableSpawnSpec::new(
                    move || Worker {
                        completed: Rc::clone(&completed),
                    },
                    self.worker_capacity,
                ))
            }
            DispatcherMsg::Submit { slot, task } => Effect::Send(SendMessage::new(
                self.registry,
                RegistryMsg::Forward { slot, task },
            )),
        }
    }
}

// ---------------------------------------------------------------------------
// Registry isolate.
//
// Tiny user-space name server. Holds `slot -> Address<WorkerMsg>` and
// forwards `Run(task)` messages to the current incarnation. Missing slots
// are a loud programmer error in this reference workload: the registry
// panics instead of silently dropping work. Updates happen through
// `Register` messages from the test after observing a restart.
// ---------------------------------------------------------------------------

#[derive(Debug)]
struct Registry {
    addresses: HashMap<u32, Address<WorkerMsg>>,
}

impl Registry {
    fn new() -> Self {
        Self {
            addresses: HashMap::new(),
        }
    }
}

impl Isolate for Registry {
    type Message = RegistryMsg;
    type Reply = ();
    type Send = SendMessage<WorkerMsg>;
    type Spawn = Infallible;
    type Call = Infallible;
    type Shard = TestShard;

    fn handle(&mut self, msg: Self::Message, _ctx: &mut Context<'_, Self::Shard>) -> Effect<Self> {
        match msg {
            RegistryMsg::Register { slot, address } => {
                self.addresses.insert(slot, address);
                Effect::Noop
            }
            RegistryMsg::Forward { slot, task } => {
                let address = self
                    .addresses
                    .get(&slot)
                    .copied()
                    .unwrap_or_else(|| panic!("registry slot {slot} is not registered"));
                Effect::Send(SendMessage::new(address, WorkerMsg::Run(task)))
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Trace helpers.
// ---------------------------------------------------------------------------

/// Returns the most recent replacement address that the runtime created for
/// `failed_isolate`, derived from the trace's `RestartChildCompleted` event.
///
/// Test-only. Real applications discover replacement addresses through a
/// registry isolate refreshed by the supervisor (a future slice), not by
/// parsing trace events. Trace event variants and ids are not stable across
/// runtime versions.
fn replacement_address_for(
    failed_isolate: IsolateId,
    trace: &[RuntimeEvent],
) -> Option<Address<WorkerMsg>> {
    for event in trace.iter().rev() {
        if let RuntimeEventKind::RestartChildCompleted {
            old_isolate,
            new_isolate,
            new_generation,
            ..
        } = event.kind()
        {
            if old_isolate == failed_isolate {
                return Some(Address::new_with_generation(
                    event.shard(),
                    new_isolate,
                    new_generation,
                ));
            }
        }
    }
    None
}

/// Returns all event kinds in trace order, useful for filtered subsequence
/// assertions. Whole-trace contents are not asserted directly because the
/// runtime trace will grow as more slices land.
fn event_kinds(trace: &[RuntimeEvent]) -> Vec<RuntimeEventKind> {
    trace.iter().map(|event| event.kind()).collect()
}

// ---------------------------------------------------------------------------
// Harness.
// ---------------------------------------------------------------------------

struct Harness {
    runtime: CurrentRuntime<TestShard, TestMailboxFactory>,
    completed: Rc<RefCell<Vec<(IsolateId, u32)>>>,
    dispatcher: Address<DispatcherMsg>,
    registry: Address<RegistryMsg>,
}

impl Harness {
    fn new(policy: RestartPolicy, budget: RestartBudget, worker_capacity: usize) -> Self {
        let mut runtime = CurrentRuntime::new(TestShard, TestMailboxFactory);
        let completed = Rc::new(RefCell::new(Vec::new()));

        let registry = runtime.register(Registry::new(), TestMailbox::new(8));
        let dispatcher = runtime.register(
            Dispatcher {
                registry,
                worker_capacity,
                completed: Rc::clone(&completed),
            },
            TestMailbox::new(8),
        );

        runtime.supervise(dispatcher, SupervisorConfig::new(policy, budget));

        Self {
            runtime,
            completed,
            dispatcher,
            registry,
        }
    }

    /// Spawns `count` workers via the dispatcher. After the runtime has
    /// stepped enough times for spawn execution, returns the new worker
    /// addresses in spawn order.
    fn spawn_workers(&mut self, count: usize) -> Vec<Address<WorkerMsg>> {
        let trace_len_before = self.runtime.trace().len();

        for _ in 0..count {
            self.runtime
                .try_send(self.dispatcher, DispatcherMsg::SpawnWorker)
                .expect("dispatcher mailbox accepts SpawnWorker");
            // One step delivers the SpawnWorker message; the dispatcher
            // returns Effect::Spawn which the runtime executes immediately,
            // but the new worker child only runs on a later step.
            assert_eq!(self.runtime.step(), 1);
        }

        // Each spawn produced one new isolate id; collect them from the
        // trace events emitted since the last call.
        let spawned_ids: Vec<IsolateId> = self.runtime.trace()[trace_len_before..]
            .iter()
            .filter_map(|event| match event.kind() {
                RuntimeEventKind::Spawned { child_isolate } => Some(child_isolate),
                _ => None,
            })
            .collect();
        assert_eq!(spawned_ids.len(), count, "expected {count} spawned workers");

        let shard = TestShard.id();
        spawned_ids
            .into_iter()
            .map(|isolate| Address::new_with_generation(shard, isolate, AddressGeneration::new(0)))
            .collect()
    }

    /// Registers a worker address in the registry under `slot`. Steps the
    /// runtime once so the registry handler observes the registration.
    fn register_worker(&mut self, slot: u32, address: Address<WorkerMsg>) {
        self.runtime
            .try_send(self.registry, RegistryMsg::Register { slot, address })
            .expect("registry mailbox accepts Register");
        assert_eq!(self.runtime.step(), 1);
    }

    /// Submits a task to the dispatcher. The dispatcher delegates lookup to
    /// the registry isolate, and the registry forwards to the current worker
    /// address. Three steps are enough in this registration order:
    /// - step 1: dispatcher handles `Submit` and enqueues `Forward`
    /// - step 2: registry handles `Forward` and enqueues `Run`
    /// - step 3: worker handles `Run`
    fn submit(&mut self, slot: u32, task: Task) {
        self.runtime
            .try_send(self.dispatcher, DispatcherMsg::Submit { slot, task })
            .expect("dispatcher mailbox accepts Submit");
        let mut total = 0;
        for _ in 0..3 {
            total += self.runtime.step();
        }
        assert!(
            total >= 1,
            "submit must produce at least the dispatcher's handler invocation"
        );
    }
}

// ---------------------------------------------------------------------------
// Tests.
// ---------------------------------------------------------------------------

/// Plenty-of-headroom budget for policy-shape proofs where budget exhaustion
/// would obscure the policy outcome.
fn generous_budget() -> RestartBudget {
    RestartBudget::new(64)
}

#[test]
fn one_for_one_restarts_only_failed_worker_and_keeps_siblings_running() {
    let mut h = Harness::new(RestartPolicy::OneForOne, generous_budget(), 4);

    let workers = h.spawn_workers(2);
    let worker_a = workers[0];
    let worker_b = workers[1];
    h.register_worker(0, worker_a);
    h.register_worker(1, worker_b);

    // Both workers handle one normal task before any failure.
    h.submit(0, Task::Normal(10));
    h.submit(1, Task::Normal(20));

    let pre_panic_completed: Vec<(IsolateId, u32)> = h.completed.borrow().clone();
    assert_eq!(
        pre_panic_completed,
        vec![(worker_a.isolate(), 10), (worker_b.isolate(), 20)]
    );

    // Inject the poison task that makes worker A panic.
    h.submit(0, Task::Poison);

    // Sanity: trace should now contain the panic and its supervised
    // restart subtree.
    let kinds = event_kinds(h.runtime.trace());
    assert!(
        kinds
            .iter()
            .any(|k| matches!(k, RuntimeEventKind::HandlerPanicked))
    );
    assert!(
        kinds
            .iter()
            .any(|k| matches!(k, RuntimeEventKind::SupervisorRestartTriggered { .. }))
    );
    assert!(
        kinds
            .iter()
            .any(|k| matches!(k, RuntimeEventKind::RestartChildCompleted { .. }))
    );

    // The old worker A address now fails closed.
    assert!(matches!(
        h.runtime
            .try_send(worker_a, WorkerMsg::Run(Task::Normal(99))),
        Err(TrySendError::Closed(WorkerMsg::Run(Task::Normal(99))))
    ));

    // Discover the replacement worker A and refresh the registry.
    let new_worker_a = replacement_address_for(worker_a.isolate(), h.runtime.trace())
        .expect("replacement worker A address");
    assert_ne!(new_worker_a.isolate(), worker_a.isolate());
    h.register_worker(0, new_worker_a);

    // Worker B's identity is unchanged (OneForOne does not restart siblings).
    assert!(
        replacement_address_for(worker_b.isolate(), h.runtime.trace()).is_none(),
        "worker B should not be restarted under OneForOne"
    );

    // Submit further work to both slots.
    h.submit(0, Task::Normal(11));
    h.submit(1, Task::Normal(21));

    let final_completed: Vec<(IsolateId, u32)> = h.completed.borrow().clone();
    assert_eq!(
        final_completed,
        vec![
            (worker_a.isolate(), 10),
            (worker_b.isolate(), 20),
            (new_worker_a.isolate(), 11),
            (worker_b.isolate(), 21),
        ]
    );
}

#[test]
fn one_for_all_restarts_every_worker_after_one_panic() {
    let mut h = Harness::new(RestartPolicy::OneForAll, generous_budget(), 4);

    let workers = h.spawn_workers(3);
    let original_ids: Vec<IsolateId> = workers.iter().map(|a| a.isolate()).collect();
    for (slot, address) in workers.iter().enumerate() {
        h.register_worker(slot as u32, *address);
    }

    // Pre-panic work: each worker handles one task. Order matters because
    // OneForAll will abandon any messages buffered when the panic fires.
    for (slot, _) in workers.iter().enumerate() {
        h.submit(slot as u32, Task::Normal(slot as u32 * 10));
    }
    let pre_panic = h.completed.borrow().clone();
    assert_eq!(
        pre_panic,
        vec![
            (original_ids[0], 0),
            (original_ids[1], 10),
            (original_ids[2], 20),
        ]
    );

    // Worker 1 panics. Under OneForAll, all three workers are replaced.
    h.submit(1, Task::Poison);

    // Every old address now fails closed.
    for address in &workers {
        assert!(matches!(
            h.runtime
                .try_send(*address, WorkerMsg::Run(Task::Normal(99))),
            Err(TrySendError::Closed(_))
        ));
    }

    // Refresh the registry from trace-derived replacements for every slot.
    let replacements: Vec<Address<WorkerMsg>> = original_ids
        .iter()
        .map(|old| {
            replacement_address_for(*old, h.runtime.trace())
                .expect("replacement for OneForAll worker")
        })
        .collect();
    let replacement_ids: Vec<IsolateId> = replacements.iter().map(|a| a.isolate()).collect();

    // All three replacements must be fresh ids distinct from originals.
    for (old, new) in original_ids.iter().zip(replacement_ids.iter()) {
        assert_ne!(old, new);
    }

    for (slot, address) in replacements.iter().enumerate() {
        h.register_worker(slot as u32, *address);
    }

    // Later work routes to every replacement.
    for (slot, _) in replacements.iter().enumerate() {
        h.submit(slot as u32, Task::Normal(100 + slot as u32));
    }

    let final_completed = h.completed.borrow().clone();
    let expected_tail: Vec<(IsolateId, u32)> = replacement_ids
        .iter()
        .enumerate()
        .map(|(slot, id)| (*id, 100 + slot as u32))
        .collect();
    assert_eq!(final_completed[..3], pre_panic[..]);
    assert_eq!(final_completed[3..], expected_tail);
}

#[test]
fn rest_for_one_keeps_older_siblings_and_restarts_failed_and_younger() {
    let mut h = Harness::new(RestartPolicy::RestForOne, generous_budget(), 4);

    let workers = h.spawn_workers(3);
    let original_ids: Vec<IsolateId> = workers.iter().map(|a| a.isolate()).collect();
    for (slot, address) in workers.iter().enumerate() {
        h.register_worker(slot as u32, *address);
    }

    // Each worker handles one task before the panic.
    for (slot, _) in workers.iter().enumerate() {
        h.submit(slot as u32, Task::Normal(slot as u32 + 1));
    }

    // Middle worker panics. RestForOne restarts the failed worker and any
    // younger siblings; the older sibling (slot 0) keeps its identity.
    h.submit(1, Task::Poison);

    // Slot 0 (older) keeps its identity.
    assert!(
        replacement_address_for(original_ids[0], h.runtime.trace()).is_none(),
        "older sibling should not be restarted under RestForOne"
    );

    // Slot 1 (failed) and slot 2 (younger) get replacements.
    let new_1 = replacement_address_for(original_ids[1], h.runtime.trace())
        .expect("replacement for failed middle worker");
    let new_2 = replacement_address_for(original_ids[2], h.runtime.trace())
        .expect("replacement for younger worker");
    assert_ne!(new_1.isolate(), original_ids[1]);
    assert_ne!(new_2.isolate(), original_ids[2]);

    // Old failed/younger addresses fail closed; older worker's address still
    // accepts work (proved by the routing step below, since registry already
    // has the original entry for slot 0).
    assert!(matches!(
        h.runtime
            .try_send(workers[1], WorkerMsg::Run(Task::Normal(99))),
        Err(TrySendError::Closed(_))
    ));
    assert!(matches!(
        h.runtime
            .try_send(workers[2], WorkerMsg::Run(Task::Normal(99))),
        Err(TrySendError::Closed(_))
    ));

    // Refresh registry for restarted slots only.
    h.register_worker(1, new_1);
    h.register_worker(2, new_2);

    // Later work goes to all three slots and lands on the surviving older
    // worker plus the replacements.
    h.submit(0, Task::Normal(40));
    h.submit(1, Task::Normal(50));
    h.submit(2, Task::Normal(60));

    let final_completed = h.completed.borrow().clone();
    // Final tail contains the post-restart trio in scripted order.
    let tail = &final_completed[final_completed.len() - 3..];
    assert_eq!(
        tail,
        &[
            (original_ids[0], 40),
            (new_1.isolate(), 50),
            (new_2.isolate(), 60),
        ]
    );
}

#[test]
fn budget_exhaustion_emits_visible_rejection_and_creates_no_replacement() {
    let mut h = Harness::new(RestartPolicy::OneForOne, RestartBudget::new(1), 4);

    let workers = h.spawn_workers(1);
    let worker_a = workers[0];
    h.register_worker(0, worker_a);

    h.submit(0, Task::Normal(1));
    assert_eq!(h.completed.borrow().clone(), vec![(worker_a.isolate(), 1)]);

    // First panic consumes the only budget unit and produces a replacement.
    h.submit(0, Task::Poison);
    let new_worker_a = replacement_address_for(worker_a.isolate(), h.runtime.trace())
        .expect("first restart should succeed");
    h.register_worker(0, new_worker_a);
    h.submit(0, Task::Normal(2));
    assert_eq!(
        h.completed.borrow().clone(),
        vec![(worker_a.isolate(), 1), (new_worker_a.isolate(), 2)]
    );

    // Second panic exceeds the budget. The runtime emits a rejection event
    // and creates no further replacement.
    let trace_len_before_second_panic = h.runtime.trace().len();
    h.submit(0, Task::Poison);

    let post_kinds = event_kinds(&h.runtime.trace()[trace_len_before_second_panic..]);
    let rejected = post_kinds
        .iter()
        .find(|k| matches!(k, RuntimeEventKind::SupervisorRestartRejected { .. }))
        .expect("budget exhaustion should produce SupervisorRestartRejected");
    match rejected {
        RuntimeEventKind::SupervisorRestartRejected { reason, .. } => {
            assert!(
                matches!(
                    reason,
                    tina_runtime_current::SupervisionRejectedReason::BudgetExceeded { .. }
                ),
                "rejection reason should be BudgetExceeded, got {reason:?}"
            );
        }
        _ => unreachable!(),
    }
    assert!(
        !post_kinds
            .iter()
            .any(|k| matches!(k, RuntimeEventKind::RestartChildCompleted { .. })),
        "budget-exhausted restart should not complete any child"
    );

    // The rejected worker's stale address is closed (panic-capture stopped it
    // even though the supervisor did not replace it).
    assert!(matches!(
        h.runtime
            .try_send(new_worker_a, WorkerMsg::Run(Task::Normal(99))),
        Err(TrySendError::Closed(_))
    ));
}

#[test]
fn stale_address_send_through_runtime_returns_closed_without_auto_routing() {
    // The runtime does not auto-route stale-address sends to the
    // replacement; user-space refresh is required. This guarantees the
    // registry-isolate pattern remains the only refresh path.
    let mut h = Harness::new(RestartPolicy::OneForOne, generous_budget(), 4);

    let workers = h.spawn_workers(1);
    let worker_a = workers[0];
    h.register_worker(0, worker_a);
    h.submit(0, Task::Poison);

    // Without refreshing the registry, submitting through the dispatcher will
    // still have the registry try to send to the stale slot-0 address. Verify
    // the trace shape directly: the next Submit produces a SendRejected{Closed}
    // event from the registry's dispatch attempt.
    let trace_len_before = h.runtime.trace().len();
    h.submit(0, Task::Normal(100));

    let post_kinds = event_kinds(&h.runtime.trace()[trace_len_before..]);
    assert!(
        post_kinds.iter().any(|k| matches!(
            k,
            RuntimeEventKind::SendRejected {
                reason: SendRejectedReason::Closed,
                ..
            }
        )),
        "stale-address forward should emit SendRejected{{Closed}}, got: {post_kinds:?}"
    );
    // No worker handler ran because the rejected send did not deliver.
    let trailing_handler_starts = post_kinds
        .iter()
        .filter(|k| matches!(k, RuntimeEventKind::HandlerStarted))
        .count();
    // `Submit` runs the dispatcher first, then the registry. No worker handler
    // should run because the rejected send did not deliver.
    assert_eq!(
        trailing_handler_starts, 2,
        "stale submit should run dispatcher + registry, not the worker"
    );
}

#[test]
fn missing_registry_slot_panics_instead_of_silently_dropping_work() {
    let mut h = Harness::new(RestartPolicy::OneForOne, generous_budget(), 4);

    let trace_len_before = h.runtime.trace().len();
    h.submit(99, Task::Normal(123));

    let post_kinds = event_kinds(&h.runtime.trace()[trace_len_before..]);
    assert!(
        post_kinds
            .iter()
            .any(|k| matches!(k, RuntimeEventKind::HandlerPanicked)),
        "missing registry slot should panic visibly, got: {post_kinds:?}"
    );
    assert!(
        h.completed.borrow().is_empty(),
        "missing registry slot must not complete any work"
    );
}

#[test]
fn repeated_runs_produce_identical_traces_and_completed_logs() {
    fn run_once() -> (Vec<(IsolateId, u32)>, Vec<RuntimeEvent>) {
        let mut h = Harness::new(RestartPolicy::OneForOne, generous_budget(), 4);

        let workers = h.spawn_workers(2);
        let worker_a = workers[0];
        let worker_b = workers[1];
        h.register_worker(0, worker_a);
        h.register_worker(1, worker_b);

        h.submit(0, Task::Normal(1));
        h.submit(1, Task::Normal(2));
        h.submit(0, Task::Poison);

        let new_worker_a = replacement_address_for(worker_a.isolate(), h.runtime.trace())
            .expect("replacement worker A");
        h.register_worker(0, new_worker_a);

        h.submit(0, Task::Normal(3));
        h.submit(1, Task::Normal(4));

        (h.completed.borrow().clone(), h.runtime.trace().to_vec())
    }

    let first = run_once();
    let second = run_once();
    assert_eq!(first.0, second.0, "completed-work logs must match");
    assert_eq!(first.1, second.1, "runtime traces must match");
}
