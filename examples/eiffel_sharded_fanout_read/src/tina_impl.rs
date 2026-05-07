//! Tina side. One `ShardCounter` isolate per shard owns its own
//! `u64`. A `ScatterCoord` on shard 0 fans `Get` out to every shard
//! through the `ShardServiceTable`. Replies flow through a
//! `ReplyAdapter` that translates each shard's typed
//! `ShardCounterReply` into the coord's own `ScatterCoordMsg::Reply`
//! variant. When every target has either replied or been ruled out,
//! the coord builds a `ScatterGatherReport<u64>` and publishes it.
//!
//! What this teaches:
//!
//! - **Placement is structural.** `ShardPlacement` and
//!   `ShardServiceTable` make "which shard owns what" a typed thing.
//!   The coord does not see raw indices.
//! - **Per-target outcomes are typed.** Even when every reply lands
//!   on the happy path, the report names the four overload outcomes
//!   the runtime *would* surface
//!   (`Full` / `Closed` / `Timeout` / `AggregateTimeout`) and the one
//!   placement outcome (`MissingShard`). The richer pressure forms
//!   live in `tina-runtime/tests/sharded_primitives.rs`.
//! - **`ReplyAdapter` is shipped.** No hand-written translator
//!   isolate. The user provides `impl From<ShardCounterReply> for
//!   ScatterCoordMsg` and the adapter does the routing.

use std::convert::Infallible;
use std::time::Duration;

use tina::prelude::*;
use tina_runtime::sharded::{
    ReplyAdapter, ScatterGatherConfig, ScatterGatherReport, ScatterGatherTargetOutcome,
    ShardPlacement, ShardServiceTable,
};
use tina_runtime::{DefaultThreadedMailboxFactory, RuntimeCall, ThreadedMultiShardRuntime};

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
    Get { reply_to: Address<ShardCounterReply> },
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
        reply: (),
        send: Outbound<ShardCounterReply>,
        spawn: Infallible,
        call: RuntimeCall<ShardCounterMsg>,
        shard: AppShard,
    }

    fn handle(&mut self, msg: ShardCounterMsg, ctx: &mut Context<'_, AppShard, Self::Reply>) -> Effect<Self> {
        match msg {
            ShardCounterMsg::Get { reply_to } => send(
                reply_to,
                ShardCounterReply {
                    shard: ctx.shard_id(),
                    value: self.value,
                },
            ),
        }
    }
}

// ---------- ScatterCoord (lives on the first shard) ----------

#[derive(Debug, Clone)]
enum ScatterCoordMsg {
    Bind {
        bridge: Address<ShardCounterReply>,
    },
    Start,
    Reply(ShardCounterReply),
}

// `ReplyAdapter<ShardCounterReply, ScatterCoordMsg, AppShard>` does
// the translation; the user only provides this `From` impl.
impl From<ShardCounterReply> for ScatterCoordMsg {
    fn from(msg: ShardCounterReply) -> Self {
        ScatterCoordMsg::Reply(msg)
    }
}

struct ScatterCoord {
    table: ShardServiceTable<ShardCounterMsg>,
    bridge: Option<Address<ShardCounterReply>>,
    config: ScatterGatherConfig,
    targets_in_order: Vec<ShardId>,
    pending_targets: Vec<ShardId>,
    outcomes: Vec<(ShardId, ScatterGatherTargetOutcome<u64>)>,
}

impl Isolate for ScatterCoord {
    tina::isolate_types! {
        message: ScatterCoordMsg,
        reply: (),
        send: Outbound<ShardCounterMsg>,
        spawn: Infallible,
        call: Infallible,
        shard: AppShard,
    }

    fn handle(
        &mut self,
        msg: ScatterCoordMsg,
        _ctx: &mut Context<'_, AppShard, Self::Reply>,
    ) -> Effect<Self> {
        match msg {
            ScatterCoordMsg::Bind { bridge } => {
                self.bridge = Some(bridge);
                noop()
            }
            ScatterCoordMsg::Start => {
                let bridge = self.bridge.expect("Bind must arrive before Start");
                let mut effects = Vec::new();
                self.targets_in_order = self.table.placement().shards().to_vec();
                for shard in self.targets_in_order.iter().copied() {
                    self.pending_targets.push(shard);
                    let address = self
                        .table
                        .address_for(shard)
                        .expect("shard came from placement.shards()");
                    effects.push(send(
                        address,
                        ShardCounterMsg::Get { reply_to: bridge },
                    ));
                }
                batch(effects)
            }
            ScatterCoordMsg::Reply(reply) => {
                let ShardCounterReply { shard, value } = reply;
                if let Some(pos) = self.pending_targets.iter().position(|s| *s == shard) {
                    self.pending_targets.swap_remove(pos);
                    self.outcomes
                        .push((shard, ScatterGatherTargetOutcome::Replied(value)));
                }
                if self.pending_targets.is_empty() {
                    let outcomes = sort_outcomes_by_target_list(
                        std::mem::take(&mut self.outcomes),
                        &self.targets_in_order,
                    );
                    let report = ScatterGatherReport {
                        config: self.config,
                        outcomes,
                    };
                    // Phase-062 Rock 1: the host reads the typed
                    // `ScatterGatherReport<u64>` via
                    // `runtime.observe_result::<...>` instead of polling
                    // an `Arc<Mutex<Option<_>>>` slot.
                    return stop_with(report);
                }
                noop()
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
        "eiffel-sharded-fanout-read",
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
            .register_with_capacity_on::<ShardCounter, ShardCounterReply>(shard, ShardCounter { value }, 8)
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
        .register_with_capacity_on::<ScatterCoord, ShardCounterMsg>(
            coord_shard,
            ScatterCoord {
                table: table.clone(),
                bridge: None,
                config,
                targets_in_order: Vec::new(),
                pending_targets: Vec::new(),
                outcomes: Vec::with_capacity(config.max_targets),
            },
            config.collector_capacity,
        )
        .map_err(|e| anyhow::anyhow!("register coord: {e:?}"))?;

    let bridge = runtime
        .register_with_capacity_on::<ReplyAdapter<ShardCounterReply, ScatterCoordMsg, AppShard>, ScatterCoordMsg>(
            coord_shard,
            ReplyAdapter::new(coord),
            config.collector_capacity,
        )
        .map_err(|e| anyhow::anyhow!("register reply adapter: {e:?}"))?;

    // Phase-062 Rock 1: typed result waiter on the multi-shard runtime.
    // The coord publishes its `ScatterGatherReport<u64>` via
    // `stop_with(report)`; no shared mutex.
    let waiter = runtime
        .observe_result::<ScatterGatherReport<u64>, _, _>(coord)
        .map_err(|e| anyhow::anyhow!("observe_result: {e:?}"))?;

    runtime
        .try_send(coord, ScatterCoordMsg::Bind { bridge })
        .map_err(|e| anyhow::anyhow!("send Bind: {e:?}"))?;
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
