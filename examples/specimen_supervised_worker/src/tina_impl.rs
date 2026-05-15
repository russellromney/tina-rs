//! Tina: a supervised worker. The `Parent` isolate spawns a
//! `Worker` as a `RestartableChildDefinition`; the runtime
//! supervisor restarts it on panic, charged against a typed
//! `RestartBudget`. Each restart's correctness comes from
//! `spawn_observed(...).then(ParentMsg::ChildStarted)` for the initial
//! typed child reference plus
//! `runtime.observe_child_restarted(parent).wait(...)` for restart
//! generations — no manual generation counter, no trace polling.

use std::convert::Infallible;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use tina::{RestartBudget, RestartPolicy, prelude::*};
use tina_runtime::{DefaultThreadedMailboxFactory, ThreadedRuntime};
use tina_supervisor::SupervisorConfig;

use crate::{Job, Report, job_script};

type WorkerAddr = Address<WorkerMsg>;

/// Host-visible slot for the worker address used by this comparison harness.
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
    Process(Job),
}

struct Worker;

#[tina_runtime::isolate(message = WorkerMsg)]
impl Worker {
    fn handle(&mut self, msg: WorkerMsg, _ctx: &mut Context<'_, SingleShard, Self::Reply>) -> Effect<Self> {
        match msg {
            WorkerMsg::Process(Job::Work(_)) => noop(),
            WorkerMsg::Process(Job::Poison) => panic!("supervised worker hit a poison job"),
        }
    }
}

#[derive(Debug, Clone, Copy)]
enum ParentMsg {
    Spawn,
    ChildStarted(Result<ChildRef<WorkerMsg>, SpawnObservedError>),
}

struct Parent {
    slot: Arc<WorkerSlot>,
    worker_capacity: usize,
}

#[tina_runtime::isolate(
    message = ParentMsg,
    spawn = RestartableChildDefinition<Worker>,
    spawn_observed = tina::SpawnObserved<RestartableChildDefinition<Worker>, ParentMsg, WorkerMsg>,
)]
impl Parent {
    fn handle(&mut self, msg: ParentMsg, _ctx: &mut Context<'_, SingleShard, Self::Reply>) -> Effect<Self> {
        match msg {
            ParentMsg::Spawn => {
                let capacity = self.worker_capacity;
                spawn_observed(RestartableChildDefinition::new(move || Worker, capacity))
                    .then(ParentMsg::ChildStarted)
            }
            ParentMsg::ChildStarted(Ok(child)) => {
                self.slot.set(child.address);
                noop()
            }
            ParentMsg::ChildStarted(Err(_)) => {
                // The parent is still alive if this message was delivered;
                // keep the example honest and stop instead of hiding failure.
                stop()
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

    // Wait for the parent's spawn_observed continuation to publish its child ref.
    wait_until(Duration::from_secs(2), "first worker child ref", || {
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
                let deadline = Instant::now() + Duration::from_secs(2);
                runtime
                    .send_observed_until(addr, deadline, Duration::from_millis(1), || {
                        WorkerMsg::Process(*job)
                    })
                    .map_err(|e| anyhow::anyhow!("send poison job: {e:?}"))?;
                let restarted = restart_waiter
                    .wait(Duration::from_secs(2))
                    .map_err(|e| anyhow::anyhow!("supervisor restart: {e:?}"))?;
                restarts += 1;
                slot.set(Address::new_with_generation(
                    parent.shard(),
                    restarted.new_isolate,
                    restarted.new_generation,
                ));
            }
            Job::Work(_) => {
                let deadline = Instant::now() + Duration::from_secs(2);
                runtime
                    .send_observed_until(addr, deadline, Duration::from_millis(1), || {
                        WorkerMsg::Process(*job)
                    })
                    .map_err(|e| anyhow::anyhow!("send work job: {e:?}"))?;
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
