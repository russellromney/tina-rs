//! Runnable task-dispatcher demonstration.
//!
//! Supervised work on the threaded single-shard runtime: a `Dispatcher`
//! isolate supervises restartable `Worker` children and owns task ingress,
//! plus a tiny `Registry` "name server" isolate that holds current worker
//! addresses and forwards work to them.
//!
//! When a worker panics on a poison task, the dispatcher's `OneForOne` policy
//! replaces only the failed worker. The replacement gets a fresh isolate
//! identity; the old address fails closed. The host learns the replacement
//! from a typed restart waiter and refreshes the registry.
//!
//! The dispatcher never scrapes the runtime trace to find a child. It spawns
//! with `spawn_observed(...).then(...)`, so the runtime hands it the typed
//! child address back as an ordinary message, which it forwards to the
//! registry.
//!
//! Run with:
//! ```bash
//! cargo run -p tina-runtime --example task_dispatcher
//! ```
//!
//! The example asserts its own outcomes so it doubles as a smoke test.

use std::collections::HashMap;
use std::panic;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use tina::{Address, ChildRef, IsolateId, SpawnObservedError, prelude::*};
use tina::{RestartBudget, RestartPolicy};
use tina_runtime::{DefaultThreadedMailboxFactory, ThreadedRuntime, ThreadedSendObservedError};
use tina_supervisor::SupervisorConfig;

// ---------------------------------------------------------------------------
// Workload types.
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum NeverOutbound {}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Task {
    Normal(u32),
    Poison,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WorkerEvent {
    Run(Task),
}

#[derive(Debug, Clone, Copy)]
enum DispatcherEvent {
    SpawnWorker,
    WorkerReady(Result<ChildRef<WorkerEvent>, SpawnObservedError>),
    Submit { slot: u32, task: Task },
}

#[derive(Debug, Clone, Copy)]
enum RegistryEvent {
    Register {
        slot: u32,
        address: Address<WorkerEvent>,
    },
    Forward {
        slot: u32,
        task: Task,
    },
}

// Shared, host-visible state. Isolates run on the worker thread, so these are
// `Arc<Mutex<_>>`, not `Rc<RefCell<_>>`.
type CompletedLog = Arc<Mutex<Vec<(IsolateId, u32)>>>;
type PublishedWorker = Arc<Mutex<Option<Address<WorkerEvent>>>>;

// ---------------------------------------------------------------------------
// Worker, Dispatcher, Registry.
// ---------------------------------------------------------------------------

#[derive(Debug)]
struct Worker {
    completed: CompletedLog,
}

#[tina::isolate(message = WorkerEvent, send = Outbound<NeverOutbound>)]
impl Worker {
    fn handle(
        &mut self,
        msg: WorkerEvent,
        ctx: &mut Context<'_, SingleShard, Self::Reply>,
    ) -> Effect<Self> {
        match msg {
            WorkerEvent::Run(Task::Normal(value)) => {
                self.completed
                    .lock()
                    .expect("completed log never poisoned")
                    .push((ctx.isolate_id(), value));
                noop()
            }
            WorkerEvent::Run(Task::Poison) => panic!("poison task"),
        }
    }
}

#[derive(Debug)]
struct Dispatcher {
    registry: Address<RegistryEvent>,
    completed: CompletedLog,
}

#[tina::isolate(
    message = DispatcherEvent,
    send = Outbound<RegistryEvent>,
    spawn = RestartableChildDefinition<Worker>,
    spawn_observed = tina::SpawnObserved<RestartableChildDefinition<Worker>, DispatcherEvent, WorkerEvent>,
)]
impl Dispatcher {
    fn handle(
        &mut self,
        msg: DispatcherEvent,
        _ctx: &mut Context<'_, SingleShard, Self::Reply>,
    ) -> Effect<Self> {
        match msg {
            DispatcherEvent::SpawnWorker => {
                let completed = Arc::clone(&self.completed);
                // Observed spawn: the runtime delivers the typed child address
                // back as `WorkerReady`, no trace scraping.
                spawn_observed(RestartableChildDefinition::new(
                    move || Worker {
                        completed: Arc::clone(&completed),
                    },
                    4,
                ))
                .then(DispatcherEvent::WorkerReady)
            }
            DispatcherEvent::WorkerReady(Ok(child)) => {
                // Register the fresh worker under slot 0.
                send(
                    self.registry,
                    RegistryEvent::Register {
                        slot: 0,
                        address: child.address,
                    },
                )
            }
            DispatcherEvent::WorkerReady(Err(_)) => noop(),
            DispatcherEvent::Submit { slot, task } => {
                send(self.registry, RegistryEvent::Forward { slot, task })
            }
        }
    }
}

#[derive(Debug)]
struct Registry {
    addresses: HashMap<u32, Address<WorkerEvent>>,
    // Published so the host can see slot 0 is live before it submits work.
    published: PublishedWorker,
}

#[tina::isolate(message = RegistryEvent, send = Outbound<WorkerEvent>)]
impl Registry {
    fn handle(
        &mut self,
        msg: RegistryEvent,
        _ctx: &mut Context<'_, SingleShard, Self::Reply>,
    ) -> Effect<Self> {
        match msg {
            RegistryEvent::Register { slot, address } => {
                self.addresses.insert(slot, address);
                if slot == 0 {
                    *self
                        .published
                        .lock()
                        .expect("published slot never poisoned") = Some(address);
                }
                noop()
            }
            RegistryEvent::Forward { slot, task } => {
                let address = self
                    .addresses
                    .get(&slot)
                    .copied()
                    .unwrap_or_else(|| panic!("registry slot {slot} is not registered"));
                send(address, WorkerEvent::Run(task))
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Demonstration script.
// ---------------------------------------------------------------------------

/// Polls `probe` until it yields a value or the timeout elapses.
fn wait_for<T>(timeout: Duration, mut probe: impl FnMut() -> Option<T>) -> Option<T> {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(value) = probe() {
            return Some(value);
        }
        if Instant::now() > deadline {
            return None;
        }
        thread::sleep(Duration::from_millis(2));
    }
}

fn main() {
    // Keep the poison-task panic from spamming stderr.
    let previous_hook = panic::take_hook();
    panic::set_hook(Box::new(|_| {}));

    let completed: CompletedLog = Arc::new(Mutex::new(Vec::new()));
    let published: PublishedWorker = Arc::new(Mutex::new(None));

    let runtime = ThreadedRuntime::new(SingleShard, DefaultThreadedMailboxFactory);

    let registry = runtime
        .register_with_capacity::<Registry, WorkerEvent>(
            Registry {
                addresses: HashMap::new(),
                published: Arc::clone(&published),
            },
            8,
        )
        .expect("register registry");
    let dispatcher = runtime
        .register_with_capacity::<Dispatcher, RegistryEvent>(
            Dispatcher {
                registry,
                completed: Arc::clone(&completed),
            },
            8,
        )
        .expect("register dispatcher");

    runtime
        .supervise(
            dispatcher,
            SupervisorConfig::new(RestartPolicy::OneForOne, RestartBudget::new(8)),
        )
        .expect("supervise dispatcher");

    // Spawn one worker through the dispatcher. The dispatcher learns the child
    // address as a message and registers it; the registry publishes slot 0.
    runtime
        .try_send(dispatcher, DispatcherEvent::SpawnWorker)
        .expect("ask dispatcher to spawn");
    let worker = wait_for(Duration::from_secs(2), || {
        *published.lock().expect("published lock")
    })
    .expect("worker registered under slot 0");
    println!("spawned worker {:?}", worker.isolate());

    // Submit a normal task. Dispatcher -> registry -> worker.
    runtime
        .try_send(
            dispatcher,
            DispatcherEvent::Submit {
                slot: 0,
                task: Task::Normal(42),
            },
        )
        .expect("submit normal task");
    wait_for(Duration::from_secs(2), || {
        (completed.lock().expect("completed lock").len() == 1).then_some(())
    })
    .expect("normal task completed");
    println!(
        "after normal task: completed = {:?}",
        completed.lock().expect("completed lock")
    );

    // Watch for the supervised restart before poisoning the worker.
    let restart = runtime
        .observe_child_restarted(dispatcher)
        .expect("register restart observer");

    // Poison task: the worker panics; OneForOne replaces it.
    runtime
        .try_send(
            dispatcher,
            DispatcherEvent::Submit {
                slot: 0,
                task: Task::Poison,
            },
        )
        .expect("submit poison task");
    let restarted = restart
        .wait(Duration::from_secs(3))
        .expect("worker restarted");

    // The old worker address now fails closed.
    let stale = runtime.send_and_observe(worker, WorkerEvent::Run(Task::Normal(99)));
    assert!(matches!(
        stale,
        Err(ThreadedSendObservedError::MailboxClosed)
    ));
    println!("stale address rejected as expected: {stale:?}");

    // Refresh the registry with the replacement address. The host learned the
    // new incarnation from the typed restart waiter, not from the trace.
    let replacement = Address::<WorkerEvent>::new_with_generation(
        restarted.new_shard,
        restarted.new_isolate,
        restarted.new_generation,
    );
    println!("replacement worker {:?}", replacement.isolate());
    runtime
        .send_and_observe(
            registry,
            RegistryEvent::Register {
                slot: 0,
                address: replacement,
            },
        )
        .expect("re-register replacement");

    // Submit another normal task. It now lands on the replacement worker.
    runtime
        .try_send(
            dispatcher,
            DispatcherEvent::Submit {
                slot: 0,
                task: Task::Normal(43),
            },
        )
        .expect("submit second normal task");
    wait_for(Duration::from_secs(2), || {
        (completed.lock().expect("completed lock").len() == 2).then_some(())
    })
    .expect("replacement completed");
    println!(
        "after restart: completed = {:?}",
        completed.lock().expect("completed lock")
    );

    let final_log = completed.lock().expect("completed lock").clone();
    assert_eq!(
        final_log,
        vec![(worker.isolate(), 42), (replacement.isolate(), 43)],
        "worker should record original then replacement completion"
    );

    println!("dead worker is not a dead system.");

    runtime.shutdown().expect("clean shutdown");
    panic::set_hook(previous_hook);
}
