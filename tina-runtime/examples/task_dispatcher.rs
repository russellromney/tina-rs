//! Runnable task-dispatcher demonstration.
//!
//! Supervised work on the threaded single-shard runtime: a `Dispatcher`
//! isolate supervises restartable `Worker` children and owns task ingress,
//! plus a tiny `Registry` "name server" isolate that holds current worker
//! addresses and forwards work to them.
//!
//! When a worker panics on a poison task, the dispatcher's `OneForOne` policy
//! replaces only the failed worker. The replacement gets a fresh isolate
//! identity; the old address fails closed. The dispatcher receives the typed
//! replacement and refreshes the registry before the host restart waiter wakes.
//!
//! The dispatcher never scrapes the runtime trace to find a child. It spawns
//! with `spawn_observed(...).then_with_restarts(...)`, so the runtime hands it
//! every typed child incarnation as an ordinary message, which it forwards to
//! the registry.
//!
//! Run with:
//! ```bash
//! cargo run -p tina-runtime --example task_dispatcher
//! ```
//!
//! The example asserts its own outcomes so it doubles as a smoke test.

use std::collections::HashMap;
use std::panic;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use tina::{Address, ChildRef, IsolateId, SpawnObservedError, prelude::*};
use tina::{RestartBudget, RestartPolicy};
use tina_runtime::{DefaultThreadedMailboxFactory, ThreadedRuntime};
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

// Shared completed-work log for the executable's final assertions.
type CompletedLog = Arc<Mutex<Vec<(IsolateId, u32)>>>;

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
                .then_with_restarts(DispatcherEvent::WorkerReady, |child| {
                    DispatcherEvent::WorkerReady(Ok(child))
                })
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
    ready: Arc<AtomicBool>,
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
                    self.ready.store(true, Ordering::Release);
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
    let registry_ready = Arc::new(AtomicBool::new(false));
    let runtime = ThreadedRuntime::new(SingleShard, DefaultThreadedMailboxFactory);

    let registry = runtime
        .register_with_capacity::<Registry, WorkerEvent>(
            Registry {
                addresses: HashMap::new(),
                ready: Arc::clone(&registry_ready),
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
    // address as a message and registers it before work is submitted.
    runtime
        .try_send(dispatcher, DispatcherEvent::SpawnWorker)
        .expect("ask dispatcher to spawn");
    wait_for(Duration::from_secs(2), || {
        registry_ready.load(Ordering::Acquire).then_some(())
    })
    .expect("worker registered under slot 0");
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
    let first_worker = completed.lock().expect("completed lock")[0].0;
    println!("spawned worker {first_worker:?}");

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
    restart
        .wait(Duration::from_secs(3))
        .expect("worker restarted");

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
    assert_eq!(final_log[0], (first_worker, 42));
    assert_eq!(final_log[1].1, 43);
    assert_ne!(final_log[1].0, first_worker);

    println!("dead worker is not a dead system.");

    runtime.shutdown().expect("clean shutdown");
    panic::set_hook(previous_hook);
}
