use std::convert::Infallible;
use std::sync::Arc;
use std::sync::Mutex;
use std::thread;
use std::time::{Duration, Instant};

use tina::{RestartBudget, RestartPolicy, prelude::*};
use tina_runtime::{
    DefaultThreadedMailboxFactory, RuntimeEventKind, ThreadedRuntime, ThreadedRuntimeConfig,
    ThreadedTrySendError,
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

// Phase 047 Rock 4: the host now learns about each restart via a typed
// `ChildRestartedWaiter` instead of an `AtomicU64` generation counter. The
// initial address still comes from the worker self-publishing on its first
// `Boot` because the runtime does not (yet) expose an
// `observe_next_child_spawned` waiter; the slot's mutex stays for that
// one-shot publish only.
#[derive(Default)]
struct WorkerSlot {
    inner: Mutex<Option<WorkerAddr>>,
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
        let mut guard = self.inner.lock().expect("worker slot mutex");
        *guard = Some(addr);
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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
        DefaultThreadedMailboxFactory,
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

    // Wait for the worker's first Boot to publish its address. The first
    // address still arrives via the worker self-publishing through `slot`
    // (a child-spawned waiter is future runtime work; the slot is a
    // one-shot mutex now, no `AtomicU64` generation counter).
    wait_until(Duration::from_secs(2), "first worker boot", || {
        slot.current().is_some()
    });

    for job in script {
        let addr = wait_for_addr(&slot, Duration::from_secs(2));
        if matches!(job, Job::Poison) {
            // Phase 047 Rock 4: register the typed restart waiter BEFORE
            // sending the poison job, so the host is observing when the
            // panic-induced restart fires. This deletes the
            // `AtomicU64` generation counter and the spin-loop predicate.
            let restart_waiter = runtime.observe_child_restarted(parent);
            send_until_accepted(
                &runtime,
                addr,
                WorkerMsg::Process(job),
                Duration::from_secs(2),
            );
            restart_waiter
                .wait(Duration::from_secs(2))
                .expect("supervisor restart resolves");
            // Wait for the fresh incarnation to publish its address.
            wait_until(Duration::from_secs(2), "next worker boot", || {
                slot.current().map(|a| a != addr).unwrap_or(false)
            });
        } else {
            send_until_accepted(
                &runtime,
                addr,
                WorkerMsg::Process(job),
                Duration::from_secs(2),
            );
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
    runtime: &ThreadedRuntime<WorkerShard, DefaultThreadedMailboxFactory>,
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
