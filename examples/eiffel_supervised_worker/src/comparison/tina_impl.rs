use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::convert::Infallible;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard};
use std::thread;
use std::time::{Duration, Instant};

use tina::{Mailbox, RestartBudget, RestartPolicy, TrySendError, prelude::*};
use tina_runtime::{
    MailboxFactory, RuntimeEventKind, ThreadedRuntime, ThreadedRuntimeConfig, ThreadedTrySendError,
};
use tina_supervisor::SupervisorConfig;

use super::{Job, SideReport, job_script};

#[derive(Debug, Default)]
struct WorkerShard;

impl Shard for WorkerShard {
    fn id(&self) -> ShardId {
        ShardId::new(91)
    }
}

struct WorkerMailbox<T> {
    capacity: usize,
    queue: Rc<RefCell<VecDeque<T>>>,
    closed: Rc<Cell<bool>>,
}

impl<T> WorkerMailbox<T> {
    fn new(capacity: usize) -> Self {
        Self {
            capacity,
            queue: Rc::new(RefCell::new(VecDeque::new())),
            closed: Rc::new(Cell::new(false)),
        }
    }
}

impl<T> Mailbox<T> for WorkerMailbox<T> {
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
struct WorkerMailboxFactory;

impl MailboxFactory for WorkerMailboxFactory {
    fn create<T: 'static>(&self, capacity: usize) -> Box<dyn Mailbox<T>> {
        Box::new(WorkerMailbox::new(capacity))
    }
}

// Shared state the live worker fills with its current address each time the
// supervisor restarts it. The driver thread reads this to send the next job
// to whatever incarnation is currently running. `generation` increments on
// every `Boot`, so the driver can wait for "the *next* incarnation" after a
// known-poison send.
#[derive(Default)]
struct WorkerSlot {
    inner: Mutex<Option<WorkerAddr>>,
    generation: AtomicU64,
}

type WorkerAddr = Address<WorkerMsg>;

impl WorkerSlot {
    fn current(&self) -> Option<WorkerAddr> {
        self.inner
            .lock()
            .expect("worker slot mutex")
            .as_ref()
            .copied()
    }

    fn set(&self, addr: WorkerAddr) {
        let mut guard: MutexGuard<'_, Option<WorkerAddr>> =
            self.inner.lock().expect("worker slot mutex");
        *guard = Some(addr);
        self.generation.fetch_add(1, Ordering::Release);
    }

    fn generation(&self) -> u64 {
        self.generation.load(Ordering::Acquire)
    }
}

#[derive(Debug, Clone, Copy)]
enum WorkerMsg {
    Boot,
    Process(Job),
}

struct Worker {
    slot: Arc<WorkerSlot>,
}

#[tina_runtime::isolate(message = WorkerMsg, shard = WorkerShard)]
impl Worker {
    fn handle(&mut self, msg: WorkerMsg, ctx: &mut Context<'_, WorkerShard>) -> Effect<Self> {
        match msg {
            WorkerMsg::Boot => {
                self.slot.set(ctx.me());
                noop()
            }
            WorkerMsg::Process(Job::Work(_)) => noop(),
            WorkerMsg::Process(Job::Poison) => panic!("supervised worker hit a poison job"),
        }
    }
}

#[derive(Debug, Clone, Copy)]
enum ParentMsg {
    Spawn,
}

struct Parent {
    slot: Arc<WorkerSlot>,
    capacity: usize,
}

#[tina_runtime::isolate(
    message = ParentMsg,
    spawn = RestartableChildDefinition<Worker>,
    shard = WorkerShard
)]
impl Parent {
    fn handle(&mut self, msg: ParentMsg, _ctx: &mut Context<'_, WorkerShard>) -> Effect<Self> {
        match msg {
            ParentMsg::Spawn => {
                let slot = Arc::clone(&self.slot);
                let capacity = self.capacity;
                spawn(
                    RestartableChildDefinition::new(
                        move || Worker {
                            slot: Arc::clone(&slot),
                        },
                        capacity,
                    )
                    .with_initial_message(|| WorkerMsg::Boot),
                )
            }
        }
    }
}

pub(crate) fn run() -> SideReport {
    let script = job_script();
    let poison_count = script.iter().filter(|j| matches!(j, Job::Poison)).count() as u32;

    let runtime = ThreadedRuntime::with_config(
        WorkerShard,
        WorkerMailboxFactory,
        ThreadedRuntimeConfig {
            command_capacity: 16,
            idle_wait: Duration::from_millis(1),
            ..Default::default()
        },
    );

    let slot = Arc::new(WorkerSlot::default());
    let parent = runtime
        .register_with_capacity::<Parent, Infallible>(
            Parent {
                slot: Arc::clone(&slot),
                capacity: 8,
            },
            8,
        )
        .expect("register parent");

    runtime
        .supervise(
            parent,
            SupervisorConfig::new(
                RestartPolicy::OneForOne,
                RestartBudget::new(poison_count + 2),
            ),
        )
        .expect("supervise parent");

    runtime
        .try_send(parent, ParentMsg::Spawn)
        .expect("send spawn");

    // Drive jobs from this thread. Each Poison advances the worker
    // generation; everything else uses the current incarnation.
    let mut last_seen_gen: u64 = 0;
    wait_until(Duration::from_secs(2), "first worker boot", || {
        slot.generation() > last_seen_gen
    });
    last_seen_gen = slot.generation();

    for job in script {
        let addr = wait_for_addr(&slot, Duration::from_secs(2));
        send_until_accepted(
            &runtime,
            addr,
            WorkerMsg::Process(job),
            Duration::from_secs(2),
        );
        if matches!(job, Job::Poison) {
            // Wait for the supervisor to restart and the new worker to Boot.
            let target = last_seen_gen + 1;
            wait_until(Duration::from_secs(2), "supervisor restart", || {
                slot.generation() >= target
            });
            last_seen_gen = slot.generation();
        }
    }

    // Drain any pending runtime work, then read the trace before shutdown.
    wait_until(Duration::from_secs(2), "no in-flight calls", || {
        runtime.has_in_flight_calls().map(|p| !p).unwrap_or(false)
    });
    let trace = runtime.complete_trace().expect("trace available");
    let restart_events = trace
        .iter()
        .filter(|event| {
            matches!(
                event.kind(),
                RuntimeEventKind::SupervisorRestartTriggered { .. }
            )
        })
        .count() as u32;
    let panic_events = trace
        .iter()
        .filter(|event| matches!(event.kind(), RuntimeEventKind::HandlerPanicked))
        .count() as u32;

    let _ = runtime.shutdown().expect("runtime shutdown");

    SideReport {
        processed: (job_script().len() as u32) - panic_events,
        poisoned: panic_events,
        restarts: restart_events,
        exit_clean: true,
    }
}

fn wait_for_addr(slot: &Arc<WorkerSlot>, timeout: Duration) -> WorkerAddr {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(addr) = slot.current() {
            return addr;
        }
        if Instant::now() > deadline {
            panic!("wait_for_addr timed out");
        }
        thread::yield_now();
    }
}

fn send_until_accepted(
    runtime: &ThreadedRuntime<WorkerShard, WorkerMailboxFactory>,
    addr: WorkerAddr,
    msg: WorkerMsg,
    timeout: Duration,
) {
    let deadline = Instant::now() + timeout;
    loop {
        match runtime.try_send(addr, msg) {
            Ok(()) => return,
            Err(ThreadedTrySendError::IngressFull) => {
                if Instant::now() > deadline {
                    panic!("send_until_accepted: ingress full timeout");
                }
                thread::yield_now();
            }
            Err(ThreadedTrySendError::WorkerStopped) => {
                panic!("send_until_accepted: runtime worker stopped");
            }
        }
    }
}

fn wait_until<F>(timeout: Duration, label: &str, mut predicate: F)
where
    F: FnMut() -> bool,
{
    let deadline = Instant::now() + timeout;
    while !predicate() {
        if Instant::now() > deadline {
            panic!("wait_until({label}) timed out");
        }
        thread::yield_now();
    }
}
