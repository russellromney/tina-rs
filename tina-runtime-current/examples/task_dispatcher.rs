//! Runnable task-dispatcher demonstration.
//!
//! Shows the reference shape for supervised work in the single-shard
//! runtime: a `Dispatcher` isolate that supervises restartable `Worker`
//! children and owns task ingress, plus a tiny `Registry` "name server"
//! isolate that holds current worker addresses and forwards work to them.
//!
//! When a worker panics on a poison task, the supervisor's `OneForOne`
//! policy replaces only the failed worker. The replacement gets a fresh
//! isolate identity. The old worker address fails closed; the test
//! refreshes the registry from the runtime trace, then later work
//! resumes through the new incarnation.
//!
//! Run with:
//! ```bash
//! cargo run -p tina-runtime-current --example task_dispatcher
//! ```
//!
//! The example asserts its own outcomes so it doubles as a smoke test.

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, VecDeque};
use std::convert::Infallible;
use std::rc::Rc;

use tina::{
    Address, AddressGeneration, Context, Effect, Isolate, IsolateId, Mailbox, RestartBudget,
    RestartPolicy, RestartableSpawnSpec, SendMessage, Shard, ShardId, TrySendError,
};
use tina_runtime_current::{CurrentRuntime, MailboxFactory, RuntimeEvent, RuntimeEventKind};
use tina_supervisor::SupervisorConfig;

// ---------------------------------------------------------------------------
// Test infrastructure: a deterministic mailbox and shard.
// ---------------------------------------------------------------------------

#[derive(Debug, Default)]
struct DemoShard;

impl Shard for DemoShard {
    fn id(&self) -> ShardId {
        ShardId::new(1)
    }
}

struct DemoMailbox<T> {
    capacity: usize,
    queue: Rc<RefCell<VecDeque<T>>>,
    closed: Rc<Cell<bool>>,
}

impl<T> Clone for DemoMailbox<T> {
    fn clone(&self) -> Self {
        Self {
            capacity: self.capacity,
            queue: Rc::clone(&self.queue),
            closed: Rc::clone(&self.closed),
        }
    }
}

impl<T> DemoMailbox<T> {
    fn new(capacity: usize) -> Self {
        Self {
            capacity,
            queue: Rc::new(RefCell::new(VecDeque::new())),
            closed: Rc::new(Cell::new(false)),
        }
    }
}

impl<T> Mailbox<T> for DemoMailbox<T> {
    fn capacity(&self) -> usize {
        self.capacity
    }
    fn try_send(&self, message: T) -> Result<(), TrySendError<T>> {
        if self.closed.get() {
            return Err(TrySendError::Closed(message));
        }
        let mut q = self.queue.borrow_mut();
        if q.len() >= self.capacity {
            return Err(TrySendError::Full(message));
        }
        q.push_back(message);
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
struct DemoMailboxFactory;

impl MailboxFactory for DemoMailboxFactory {
    fn create<T: 'static>(&self, capacity: usize) -> Box<dyn Mailbox<T>> {
        Box::new(DemoMailbox::new(capacity))
    }
}

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
// Worker, Dispatcher, Registry.
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
    type Shard = DemoShard;

    fn handle(&mut self, msg: Self::Message, ctx: &mut Context<'_, Self::Shard>) -> Effect<Self> {
        match msg {
            WorkerMsg::Run(Task::Normal(value)) => {
                self.completed.borrow_mut().push((ctx.isolate_id(), value));
                Effect::Noop
            }
            WorkerMsg::Run(Task::Poison) => panic!("poison task"),
        }
    }
}

#[derive(Debug)]
struct Dispatcher {
    registry: Address<RegistryMsg>,
    completed: Rc<RefCell<Vec<(IsolateId, u32)>>>,
}

impl Isolate for Dispatcher {
    type Message = DispatcherMsg;
    type Reply = ();
    type Send = SendMessage<RegistryMsg>;
    type Spawn = RestartableSpawnSpec<Worker>;
    type Shard = DemoShard;

    fn handle(&mut self, msg: Self::Message, _ctx: &mut Context<'_, Self::Shard>) -> Effect<Self> {
        match msg {
            DispatcherMsg::SpawnWorker => {
                let completed = Rc::clone(&self.completed);
                Effect::Spawn(RestartableSpawnSpec::new(
                    move || Worker {
                        completed: Rc::clone(&completed),
                    },
                    4,
                ))
            }
            DispatcherMsg::Submit { slot, task } => Effect::Send(SendMessage::new(
                self.registry,
                RegistryMsg::Forward { slot, task },
            )),
        }
    }
}

#[derive(Debug)]
struct Registry {
    addresses: HashMap<u32, Address<WorkerMsg>>,
}

impl Isolate for Registry {
    type Message = RegistryMsg;
    type Reply = ();
    type Send = SendMessage<WorkerMsg>;
    type Spawn = Infallible;
    type Shard = DemoShard;

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

/// Test-only helper for discovering replacement worker addresses by walking
/// the runtime trace. Production code should refresh registry state through
/// messages from the supervisor, not by parsing trace events.
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

// ---------------------------------------------------------------------------
// Demonstration script.
// ---------------------------------------------------------------------------

fn main() {
    let mut runtime = CurrentRuntime::new(DemoShard, DemoMailboxFactory);
    let completed: Rc<RefCell<Vec<(IsolateId, u32)>>> = Rc::new(RefCell::new(Vec::new()));

    let registry = runtime.register(
        Registry {
            addresses: HashMap::new(),
        },
        DemoMailbox::new(8),
    );
    let dispatcher = runtime.register(
        Dispatcher {
            registry,
            completed: Rc::clone(&completed),
        },
        DemoMailbox::new(8),
    );

    runtime.supervise(
        dispatcher,
        SupervisorConfig::new(RestartPolicy::OneForOne, RestartBudget::new(8)),
    );

    // Spawn one worker through the dispatcher.
    runtime
        .try_send(dispatcher, DispatcherMsg::SpawnWorker)
        .unwrap();
    runtime.step();
    let worker_id = runtime
        .trace()
        .iter()
        .rev()
        .find_map(|event| match event.kind() {
            RuntimeEventKind::Spawned { child_isolate } => Some(child_isolate),
            _ => None,
        })
        .expect("worker spawned");
    let worker = Address::new_with_generation(DemoShard.id(), worker_id, AddressGeneration::new(0));
    println!("spawned worker {worker_id:?}");

    // Register the worker under slot 0.
    runtime
        .try_send(
            registry,
            RegistryMsg::Register {
                slot: 0,
                address: worker,
            },
        )
        .unwrap();
    runtime.step();

    // Submit a normal task through the dispatcher. The dispatcher delegates to
    // the registry, which resolves slot 0 to the current worker address.
    runtime
        .try_send(
            dispatcher,
            DispatcherMsg::Submit {
                slot: 0,
                task: Task::Normal(42),
            },
        )
        .unwrap();
    runtime.step();
    runtime.step();
    runtime.step();
    println!("after normal task: completed = {:?}", completed.borrow());

    // Submit a poison task through the dispatcher. Worker panics; supervisor
    // replaces it.
    runtime
        .try_send(
            dispatcher,
            DispatcherMsg::Submit {
                slot: 0,
                task: Task::Poison,
            },
        )
        .unwrap();
    runtime.step();
    runtime.step();
    runtime.step();

    // The old worker address now fails closed.
    let stale_send = runtime.try_send(worker, WorkerMsg::Run(Task::Normal(99)));
    assert!(matches!(stale_send, Err(TrySendError::Closed(_))));
    println!("stale address rejected as expected: {stale_send:?}");

    // Refresh the registry with the replacement address.
    let replacement =
        replacement_address_for(worker_id, runtime.trace()).expect("replacement worker address");
    println!("replacement worker {:?}", replacement.isolate());
    runtime
        .try_send(
            registry,
            RegistryMsg::Register {
                slot: 0,
                address: replacement,
            },
        )
        .unwrap();
    runtime.step();

    // Submit another normal task through the dispatcher. It now lands on the
    // replacement worker.
    runtime
        .try_send(
            dispatcher,
            DispatcherMsg::Submit {
                slot: 0,
                task: Task::Normal(43),
            },
        )
        .unwrap();
    runtime.step();
    runtime.step();
    runtime.step();
    println!("after restart: completed = {:?}", completed.borrow());

    let final_log = completed.borrow().clone();
    assert_eq!(
        final_log,
        vec![(worker_id, 42), (replacement.isolate(), 43)],
        "worker should record original then replacement completion"
    );

    println!("dead worker is not a dead system. ✅");
}
