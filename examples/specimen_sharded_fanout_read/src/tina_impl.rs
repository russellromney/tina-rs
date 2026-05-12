//! Tina side. One `ShardCounter` isolate per shard owns its own
//! `u64`. A `ScatterCoord` on shard 0 fans `Get` out to every shard
//! via `call`, collects `CallOutcome<ShardCounterReply>`, and builds
//! a `ScatterGatherReport<u64>`.
//!
//! What this teaches:
//!
//! - **Placement is structural.** `ShardPlacement` and
//!   `ShardServiceTable` make "which shard owns what" a typed thing.
//!   The coord does not see raw indices.
//! - **`call` replaces send + reply adapter.** `ShardCounter` replies
//!   to the caller directly via `reply(...)`. The coord fans out with
//!   `call(address, ShardCounterMsg::Get, timeout).reply(...)`. No
//!   `ReplyAdapter`, no `Bind` message, no chicken-and-egg.
//! - **Per-target outcomes are typed.** `CallOutcome::Replied`,
//!   `Timeout`, `Full`, `Closed` surface directly in the coord's
//!   `CallResult` handler. No translation through an adapter.

use std::convert::Infallible;
use std::time::Duration;

use tina::prelude::*;
use tina_runtime::sharded::{
    ScatterGatherConfig, ScatterGatherReport, ScatterGatherTargetOutcome, ShardPlacement,
    ShardServiceTable,
};
use tina_runtime::{CallOutcome, DefaultThreadedMailboxFactory, RuntimeCall, ThreadedMultiShardRuntime, call};

use crate::{Report, SEED_VALUES, SHARD_RAW_IDS};

#[derive(Debug, Clone, Copy)]
struct AppShard(u32);

impl Shard for AppShard {
    fn id(&self) -> ShardId {
        ShardId::new(self.0)
    }
}

// ---------- Per-shard counter ----------

#[derive(Debug, Clone)]
enum ShardCounterMsg {
    Get,
}

#[derive(Debug, Clone)]
struct ShardCounterReply {
    shard: ShardId,
    value: u64,
}

struct ShardCounter {
    value: u64,
}

impl Isolate for ShardCounter {
    tina::isolate_types! {
        message: ShardCounterMsg,
        reply: ShardCounterReply,
        send: Outbound<Infallible>,
        spawn: Infallible,
        call: RuntimeCall<ShardCounterMsg>,
        shard: AppShard,
    }

    fn handle(&mut self, msg: ShardCounterMsg, ctx: &mut Context<'_, AppShard, Self::Reply>) -> Effect<Self> {
        match msg {
            ShardCounterMsg::Get => reply(ShardCounterReply {
                shard: ctx.shard_id(),
                value: self.value,
            }),
        }
    }
}

// ---------- ScatterCoord (lives on the first shard) ----------

#[derive(Debug, Clone)]
enum ScatterCoordMsg {
    Start,
    CallResult {
        shard: ShardId,
        outcome: CallOutcome<ShardCounterReply>,
    },
}

struct ScatterCoord {
    table: ShardServiceTable<ShardCounterMsg, ShardCounterReply>,
    config: ScatterGatherConfig,
    targets_in_order: Vec<ShardId>,
    pending: Vec<ShardId>,
    outcomes: Vec<(ShardId, ScatterGatherTargetOutcome<u64>)>,
}

impl Isolate for ScatterCoord {
    tina::isolate_types! {
        message: ScatterCoordMsg,
        reply: (),
        send: Outbound<()>,
        spawn: Infallible,
        call: RuntimeCall<ScatterCoordMsg>,
        shard: AppShard,
    }

    fn handle(
        &mut self,
        msg: ScatterCoordMsg,
        _ctx: &mut Context<'_, AppShard, Self::Reply>,
    ) -> Effect<Self> {
        match msg {
            ScatterCoordMsg::Start => {
                let mut effects = Vec::new();
                self.targets_in_order = self.table.placement().shards().to_vec();
                let timeout = self.config.per_target_timeout;
                for &shard in &self.targets_in_order {
                    self.pending.push(shard);
                    let address = self
                        .table
                        .address_for(shard)
                        .expect("shard came from placement.shards()");
                    effects.push(
                        call(address, ShardCounterMsg::Get, timeout)
                            .reply(move |outcome| ScatterCoordMsg::CallResult {
                                shard,
                                outcome,
                            }),
                    );
                }
                batch(effects)
            }
            ScatterCoordMsg::CallResult { shard, outcome } => {
                if let Some(pos) = self.pending.iter().position(|s| *s == shard) {
                    self.pending.swap_remove(pos);
                }
                let sg_outcome = match outcome {
                    CallOutcome::Replied(ShardCounterReply { value, .. }) => {
                        ScatterGatherTargetOutcome::Replied(value)
                    }
                    CallOutcome::Timeout => ScatterGatherTargetOutcome::Timeout,
                    CallOutcome::Full => ScatterGatherTargetOutcome::Full,
                    CallOutcome::Closed => ScatterGatherTargetOutcome::Closed,
                };
                self.outcomes.push((shard, sg_outcome));
                if self.pending.is_empty() {
                    let outcomes = sort_outcomes_by_target_list(
                        std::mem::take(&mut self.outcomes),
                        &self.targets_in_order,
                    );
                    let report = ScatterGatherReport {
                        config: self.config,
                        outcomes,
                    };
                    stop_with(report)
                } else {
                    noop()
                }
            }
        }
    }
}

fn sort_outcomes_by_target_list<T>(
    mut outcomes: Vec<(ShardId, ScatterGatherTargetOutcome<T>)>,
    targets: &[ShardId],
) -> Vec<(ShardId, ScatterGatherTargetOutcome<T>)> {
    outcomes.sort_by_key(|(s, _)| {
        targets
            .iter()
            .position(|t| *t == *s)
            .unwrap_or(usize::MAX)
    });
    outcomes
}

// ---------- Run ----------

pub fn run() -> anyhow::Result<Report> {
    let runtime = ThreadedMultiShardRuntime::new(
        SHARD_RAW_IDS.iter().copied().map(AppShard),
        DefaultThreadedMailboxFactory,
    );

    let placement = ShardPlacement::new(
        "specimen-sharded-fanout-read",
        SHARD_RAW_IDS.iter().copied().map(ShardId::new).collect(),
    )
    .map_err(|e| anyhow::anyhow!("placement: {e}"))?;

    // One ShardCounter per shard, seeded in placement order.
    let table = ShardServiceTable::try_from_placement(placement.clone(), |shard| {
        let value = SEED_VALUES[placement
            .shards()
            .iter()
            .position(|s| *s == shard)
            .expect("shard came from placement.shards()")];
        runtime
            .register_with_capacity_on::<ShardCounter, _>(shard, ShardCounter { value }, 8)
    })
    .map_err(|e| anyhow::anyhow!("register shard counters: {e}"))?;

    let coord_shard = ShardId::new(SHARD_RAW_IDS[0]);
    let config = ScatterGatherConfig {
        max_targets: SHARD_RAW_IDS.len(),
        collector_capacity: SHARD_RAW_IDS.len(),
        per_target_timeout: Duration::from_millis(200),
        aggregate_timeout: Duration::from_millis(500),
    };
    config
        .validate()
        .map_err(|e| anyhow::anyhow!("scatter/gather config: {e}"))?;

    let coord = runtime
        .register_with_capacity_on::<ScatterCoord, _>(
            coord_shard,
            ScatterCoord {
                table,
                config,
                targets_in_order: Vec::new(),
                pending: Vec::new(),
                outcomes: Vec::with_capacity(config.max_targets),
            },
            config.collector_capacity,
        )
        .map_err(|e| anyhow::anyhow!("register coord: {e:?}"))?;

    // Phase-062 Rock 1: typed result waiter on the multi-shard runtime.
    // The coord publishes its `ScatterGatherReport<u64>` via
    // `stop_with(report)`; no shared mutex.
    let waiter = runtime
        .observe_result::<ScatterGatherReport<u64>, _, _>(coord)
        .map_err(|e| anyhow::anyhow!("observe_result: {e:?}"))?;

    runtime
        .try_send(coord, ScatterCoordMsg::Start)
        .map_err(|e| anyhow::anyhow!("send Start: {e:?}"))?;

    let report = waiter
        .wait(Duration::from_secs(2))
        .map_err(|e| anyhow::anyhow!("scatter coord did not produce a report: {e:?}"))?;

    let total_sum: u64 = report.replied().map(|(_, v)| *v).sum();
    let shards_replied = report.replied_count() as u32;

    let _ = runtime.shutdown();
    Ok(Report {
        total_sum,
        shards_replied,
        exit_clean: true,
    })
}
