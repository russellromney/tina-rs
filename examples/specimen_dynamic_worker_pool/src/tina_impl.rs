//! Tina side. The `Coordinator` isolate dynamically spawns
//! `WORKER_COUNT` `Worker` children via `spawn(ChildDefinition::new(
//! ..., capacity).with_initial_message(WorkerMsg::Compute))`. Each
//! worker computes the sum of its slice and `send`s the partial back
//! to the coordinator's address. The coordinator collects partials
//! and `stop_with(report)` when every child has replied.

use std::time::Duration;

use tina::ChildDefinition;
use tina::prelude::*;
use tina_runtime::{BoundedItems, DefaultThreadedMailboxFactory, LocalSystem, bounded_batch};

use crate::{Report, WORK_VALUES, WORKER_COUNT};

// ---------- Worker isolate ----------

#[derive(Debug, Clone)]
enum WorkerMsg {
    /// Initial message: compute the partial and send it back.
    Compute,
}

struct Worker {
    parent: Address<CoordMsg>,
    chunk: Vec<u64>,
}

#[tina::isolate(message = WorkerMsg, send = tina::Outbound<CoordMsg>)]
impl Worker {
    fn handle(&mut self, msg: WorkerMsg, _ctx: &mut Context<'_, SingleShard>) -> Effect<Self> {
        match msg {
            WorkerMsg::Compute => {
                let partial: u64 = self.chunk.iter().copied().sum();
                send(self.parent, CoordMsg::WorkerDone(partial))
            }
        }
    }
}

// ---------- Coordinator isolate ----------

#[derive(Debug, Clone)]
pub enum CoordMsg {
    /// Host kicks off the fanout. No `Begin { self_addr }` ceremony:
    /// the coord learns its own address at registration via
    /// `register_root_using`.
    Start,
    /// One child finished. Carries its partial sum.
    WorkerDone(u64),
}

struct Coordinator {
    self_addr: Address<CoordMsg>,
    expected: u32,
    received: u32,
    sum: u64,
    chunks: Vec<Vec<u64>>,
    report: Report,
}

#[tina_runtime::isolate(
    message = CoordMsg,
    spawn = ChildDefinition<Worker>,
)]
impl Coordinator {
    fn handle(&mut self, msg: CoordMsg, _ctx: &mut Context<'_, SingleShard>) -> Effect<Self> {
        match msg {
            CoordMsg::Start => {
                let parent = self.self_addr;
                let chunks =
                    BoundedItems::try_from_iter(self.expected as usize, self.chunks.drain(..))
                        .expect("worker chunks are capped by WORKER_COUNT");
                let effects = chunks.map_effects(|chunk| {
                    spawn(
                        ChildDefinition::new(Worker { parent, chunk }, 4)
                            .with_initial_message(WorkerMsg::Compute),
                    )
                });
                bounded_batch(effects)
            }
            CoordMsg::WorkerDone(partial) => {
                self.received += 1;
                self.sum += partial;
                if self.received >= self.expected {
                    self.report.results_collected = self.received;
                    self.report.total_sum = self.sum;
                    self.report.exit_clean = true;
                    stop_with(self.report)
                } else {
                    noop()
                }
            }
        }
    }
}

// ---------- Run ----------

pub fn run() -> anyhow::Result<Report> {
    let app = LocalSystem::single_shard(SingleShard, DefaultThreadedMailboxFactory).try_build()?;

    let chunk_size = WORK_VALUES.len() / WORKER_COUNT as usize;
    let chunks: Vec<Vec<u64>> = (0..WORKER_COUNT as usize)
        .map(|i| WORK_VALUES[i * chunk_size..(i + 1) * chunk_size].to_vec())
        .collect();

    Ok(app.run_to_shutdown_reported(Duration::from_secs(5), move |app| {
        let coord_addr = app
            .register_root_using(
            // Mailbox sized for `Start` + every worker's `WorkerDone`.
            (WORKER_COUNT + 4) as usize,
            move |self_addr| Coordinator {
                self_addr,
                expected: WORKER_COUNT,
                received: 0,
                sum: 0,
                chunks,
                report: Report::default(),
            },
        )
        .map_err(|e| anyhow::anyhow!("register coordinator: {e:?}"))?;

        let waiter = app
            .observe_result::<Report, _, _>(coord_addr)
            .map_err(|e| anyhow::anyhow!("observe_result: {e:?}"))?;

        app.try_send(coord_addr, CoordMsg::Start)
            .map_err(|e| anyhow::anyhow!("send Start: {e:?}"))?;

        waiter
            .wait(Duration::from_secs(5))
            .map_err(|e| anyhow::anyhow!("coord did not finish: {e:?}"))
    })?)
}
