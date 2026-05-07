//! Tina: a supervised worker. The `Parent` isolate spawns a
//! `Worker` as a `RestartableChildDefinition`; the runtime
//! supervisor restarts it on panic, charged against a typed
//! `RestartBudget`. Each restart's correctness comes from
//! `runtime.observe_child_restarted(parent).wait(...)` — no manual
//! generation counter, no trace polling.

use std::convert::Infallible;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use tina::{RestartBudget, RestartPolicy, prelude::*};
use tina_runtime::{DefaultThreadedMailboxFactory, ThreadedRuntime, ThreadedTrySendError};
use tina_supervisor::SupervisorConfig;

use crate::{Job, Report, job_script};

type WorkerAddr = Address<WorkerMsg>;

/// One-shot publish slot for the worker's address. Each restarted
/// worker overwrites it on `Boot`. Until Tina ships an
/// observe-child-spawned waiter, this is the documented pattern for
/// "host needs to know the fresh worker's address" (FINDINGS.md).
#[derive(Default)]
struct WorkerSlot {
    inner: Mutex<Option<WorkerAddr>>,
}

impl WorkerSlot {
    fn current(&self) -> Option<WorkerAddr> {
        self.inner
            .lock()
            .expect("worker slot mutex")
            .as_ref()
            .copied()
    }
    fn set(&self, addr: WorkerAddr) {
        *self.inner.lock().expect("worker slot mutex") = Some(addr);
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

#[tina_runtime::isolate(message = WorkerMsg)]
impl Worker {
    fn handle(&mut self, msg: WorkerMsg, ctx: &mut Context<'_, SingleShard>) -> Effect<Self> {
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
    worker_capacity: usize,
}

#[tina_runtime::isolate(
    message = ParentMsg,
    spawn = RestartableChildDefinition<Worker>,
)]
impl Parent {
    fn handle(&mut self, msg: ParentMsg, _ctx: &mut Context<'_, SingleShard>) -> Effect<Self> {
        match msg {
            ParentMsg::Spawn => {
                let slot = Arc::clone(&self.slot);
                let capacity = self.worker_capacity;
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

pub fn run() -> anyhow::Result<Report> {
    let script = job_script();
    let poison_count = script.iter().filter(|j| matches!(j, Job::Poison)).count() as u32;
    let work_count = script.iter().filter(|j| matches!(j, Job::Work(_))).count() as u32;

    let runtime = ThreadedRuntime::new(SingleShard, DefaultThreadedMailboxFactory);
    let slot = Arc::new(WorkerSlot::default());

    let parent = runtime
        .register_with_capacity::<_, Infallible>(
            Parent {
                slot: Arc::clone(&slot),
                worker_capacity: 8,
            },
            8,
        )
        .map_err(|e| anyhow::anyhow!("register parent: {e:?}"))?;

    runtime
        .try_supervise(
            parent,
            SupervisorConfig::new(
                RestartPolicy::OneForOne,
                RestartBudget::new(poison_count + 2),
            ),
        )
        .map_err(|e| anyhow::anyhow!("supervise parent: {e:?}"))?
        .map_err(|e| anyhow::anyhow!("supervise: {e:?}"))?;

    runtime
        .try_send(parent, ParentMsg::Spawn)
        .map_err(|e| anyhow::anyhow!("send spawn: {e:?}"))?;

    // Wait for the worker's first `Boot` to publish its address.
    wait_until(Duration::from_secs(2), "first worker boot", || {
        slot.current().is_some()
    })?;

    let mut restarts: u32 = 0;
    for job in &script {
        let addr = slot
            .current()
            .ok_or_else(|| anyhow::anyhow!("worker addr missing"))?;
        match job {
            Job::Poison => {
                let restart_waiter = runtime.observe_child_restarted(parent);
                send_until_accepted(
                    &runtime,
                    addr,
                    WorkerMsg::Process(*job),
                    Duration::from_secs(2),
                )?;
                restart_waiter
                    .wait(Duration::from_secs(2))
                    .map_err(|e| anyhow::anyhow!("supervisor restart: {e:?}"))?;
                restarts += 1;
                // Wait for the fresh incarnation to publish its address.
                wait_until(Duration::from_secs(2), "next worker boot", || {
                    slot.current().map(|a| a != addr).unwrap_or(false)
                })?;
            }
            Job::Work(_) => {
                send_until_accepted(
                    &runtime,
                    addr,
                    WorkerMsg::Process(*job),
                    Duration::from_secs(2),
                )?;
            }
        }
    }

    let _ = runtime.shutdown();

    Ok(Report {
        processed: work_count,
        poisoned: poison_count,
        restarts,
        exit_clean: true,
    })
}

fn send_until_accepted(
    runtime: &ThreadedRuntime<SingleShard, DefaultThreadedMailboxFactory>,
    addr: WorkerAddr,
    msg: WorkerMsg,
    timeout: Duration,
) -> anyhow::Result<()> {
    let deadline = Instant::now() + timeout;
    loop {
        match runtime.try_send(addr, msg) {
            Ok(()) => return Ok(()),
            Err(ThreadedTrySendError::IngressFull) => {
                if Instant::now() > deadline {
                    anyhow::bail!("send_until_accepted: ingress full timeout");
                }
                thread::yield_now();
            }
            Err(ThreadedTrySendError::WorkerStopped) => {
                anyhow::bail!("send_until_accepted: runtime worker stopped");
            }
        }
    }
}

fn wait_until<F>(timeout: Duration, label: &str, mut predicate: F) -> anyhow::Result<()>
where
    F: FnMut() -> bool,
{
    let deadline = Instant::now() + timeout;
    while !predicate() {
        if Instant::now() > deadline {
            anyhow::bail!("wait_until({label}) timed out");
        }
        thread::yield_now();
    }
    Ok(())
}
