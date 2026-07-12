//! Tina side. Request-only shard services live behind a typed placement table;
//! one bounded scatter/gather owner drives the fanout and returns the complete
//! ordered report to the host caller.

use std::convert::Infallible;
use std::time::Duration;

use tina::prelude::*;
use tina_runtime::sharded::{
    ScatterGatherConfig, ScatterGatherReport, ScatterGatherTargetOutcome, ShardPlacement,
    ShardRequestServiceTable,
};
use tina_runtime::{
    BoundedItems, CallOutcome, DefaultThreadedMailboxFactory, ScatterGatherEvent,
    ScatterGatherOperations, ScatterGatherOperationsStart, ScatterGatherStartError,
    ThreadedMultiShardRuntime, call_cancelable_request,
};

use crate::{Report, SEED_VALUES, SHARD_RAW_IDS};

const TARGET_TIMEOUT: Duration = Duration::from_millis(200);
const AGGREGATE_TIMEOUT: Duration = Duration::from_millis(500);

#[derive(Debug, Clone, Copy)]
struct AppShard(u32);

impl Shard for AppShard {
    fn id(&self) -> ShardId {
        ShardId::new(self.0)
    }
}

// ---------- Per-shard counter --------------------------------------------

#[derive(Debug, Clone, Copy)]
enum ShardCounterRequest {
    Get,
}

#[derive(Debug, Clone, Copy)]
struct ShardCounterReply {
    shard: ShardId,
    value: u64,
}

struct ShardCounter {
    shard: ShardId,
    value: u64,
}

#[tina_runtime::isolate(
    request = ShardCounterRequest,
    reply = ShardCounterReply,
    shard = AppShard
)]
impl ShardCounter {
    fn handle_request(
        &mut self,
        request: ShardCounterRequest,
        call: RequestCall<'_, Self>,
    ) -> RequestEffect<Self> {
        match request {
            ShardCounterRequest::Get => call.reply(ShardCounterReply {
                shard: self.shard,
                value: self.value,
            }),
        }
    }
}

// ---------- Coordinator ---------------------------------------------------

#[derive(Debug)]
enum CoordEvent {
    Scatter(ScatterGatherEvent<ShardId, ShardCounterReply>),
}

#[derive(Debug, Clone, Copy)]
enum CoordRequest {
    ReadAll,
}

#[derive(Debug)]
enum AggregateReply {
    Complete(ScatterGatherReport<ShardCounterReply, ShardId>),
    Full,
    StartRejected(ScatterGatherStartError<ShardId>),
}

struct ScatterCoord {
    table: ShardRequestServiceTable<ShardCounterRequest, ShardCounterReply>,
    config: ScatterGatherConfig,
    operations: ScatterGatherOperations<ShardId, ShardCounterReply, AggregateReply>,
}

#[tina_runtime::isolate(event = CoordEvent, request = CoordRequest, reply = AggregateReply, shard = AppShard)]
impl ScatterCoord {
    fn handle_event(
        &mut self,
        event: CoordEvent,
        _ctx: &mut Context<'_, AppShard, Self::Reply>,
    ) -> Effect<Self> {
        let CoordEvent::Scatter(event) = event;
        let Some(advance) = self
            .operations
            .advance_service::<Self, _, _, _>(event, CoordEvent::Scatter)
            .unwrap_or_else(|error| panic!("scatter continuation violated authority: {error:?}"))
        else {
            return noop();
        };
        match advance.completed {
            Some(completed) => Effect::Batch(vec![
                advance.effect,
                reply_to(
                    completed.request,
                    AggregateReply::Complete(completed.report),
                ),
            ]),
            None => advance.effect,
        }
    }

    fn handle_request(
        &mut self,
        request: CoordRequest,
        call: RequestCall<'_, Self>,
    ) -> RequestEffect<Self> {
        match request {
            CoordRequest::ReadAll => call.capture(|request| {
                let targets = BoundedItems::try_from_iter(
                    self.config.max_targets,
                    self.table
                        .placement()
                        .shards()
                        .iter()
                        .copied()
                        .map(|shard| {
                            let address = self
                                .table
                                .address_for(shard)
                                .expect("placement shard has a request capability");
                            (shard, Some(address))
                        }),
                )
                .expect("placement target list is bounded by scatter config");

                match self.operations.start_service::<Self, _, _, _, _, _, _>(
                    request,
                    self.config,
                    targets,
                    |address, timeout| {
                        call_cancelable_request(address, ShardCounterRequest::Get, timeout)
                    },
                    CoordEvent::Scatter,
                ) {
                    Ok(ScatterGatherOperationsStart::Running(effect)) => effect,
                    Ok(ScatterGatherOperationsStart::Ready(completed)) => reply_to(
                        completed.request,
                        AggregateReply::Complete(completed.report),
                    ),
                    Err(failure) => match failure.error {
                        ScatterGatherStartError::OperationsFull { .. } => {
                            reply_to(failure.request, AggregateReply::Full)
                        }
                        error => reply_to(failure.request, AggregateReply::StartRejected(error)),
                    },
                }
            }),
        }
    }
}

// ---------- Run -----------------------------------------------------------

pub fn run() -> anyhow::Result<Report> {
    let runtime = ThreadedMultiShardRuntime::try_new(
        SHARD_RAW_IDS.iter().copied().map(AppShard),
        DefaultThreadedMailboxFactory,
    )?;

    let placement = ShardPlacement::new(
        "specimen-sharded-fanout-read",
        SHARD_RAW_IDS.iter().copied().map(ShardId::new).collect(),
    )
    .map_err(|error| anyhow::anyhow!("placement: {error}"))?;

    let table = ShardRequestServiceTable::try_from_placement(placement.clone(), |shard| {
        let value = SEED_VALUES[placement
            .shards()
            .iter()
            .position(|candidate| *candidate == shard)
            .expect("registration shard came from placement")];
        runtime.register_request_service_on(shard, ShardCounter { shard, value }, 8)
    })
    .map_err(|error| anyhow::anyhow!("register shard counters: {error}"))?;

    let config = ScatterGatherConfig {
        max_targets: SHARD_RAW_IDS.len(),
        collector_capacity: SHARD_RAW_IDS.len(),
        per_target_timeout: TARGET_TIMEOUT,
        aggregate_timeout: AGGREGATE_TIMEOUT,
    };
    config
        .validate()
        .map_err(|error| anyhow::anyhow!("scatter/gather config: {error}"))?;

    let coord = runtime
        .register_split_service_on::<ScatterCoord, CoordEvent, CoordRequest, Infallible>(
            ShardId::new(SHARD_RAW_IDS[0]),
            ScatterCoord {
                table,
                config,
                operations: ScatterGatherOperations::with_capacity(1),
            },
            config.collector_capacity + 2,
        )
        .map_err(|error| anyhow::anyhow!("register coordinator: {error:?}"))?;

    let outcome = runtime
        .call_blocking_request(
            coord.requests,
            CoordRequest::ReadAll,
            Duration::from_secs(2),
        )
        .map_err(|error| anyhow::anyhow!("drive scatter request: {error}"))?;

    let report = match outcome {
        CallOutcome::Replied(AggregateReply::Complete(report)) => report,
        CallOutcome::Replied(AggregateReply::Full) => {
            anyhow::bail!("single scatter operation was unexpectedly full")
        }
        CallOutcome::Replied(AggregateReply::StartRejected(error)) => {
            anyhow::bail!("scatter start rejected: {error:?}")
        }
        CallOutcome::Full => anyhow::bail!("coordinator mailbox was full"),
        CallOutcome::Closed => anyhow::bail!("coordinator closed before replying"),
        CallOutcome::Timeout => anyhow::bail!("coordinator call timed out"),
        CallOutcome::Rejected(reason) => anyhow::bail!("coordinator rejected request: {reason:?}"),
    };

    let total_sum = report
        .outcomes
        .iter()
        .try_fold(0u64, |sum, (target, outcome)| match outcome {
            ScatterGatherTargetOutcome::Replied(reply) if reply.shard == *target => {
                Some(sum + reply.value)
            }
            ScatterGatherTargetOutcome::Replied(_)
            | ScatterGatherTargetOutcome::Full
            | ScatterGatherTargetOutcome::Closed
            | ScatterGatherTargetOutcome::Timeout
            | ScatterGatherTargetOutcome::Rejected(_)
            | ScatterGatherTargetOutcome::AggregateTimeout
            | ScatterGatherTargetOutcome::MissingShard => None,
        })
        .ok_or_else(|| anyhow::anyhow!("scatter report was partial or misrouted: {report:?}"))?;
    let shards_replied = report.replied_count() as u32;

    runtime.shutdown_report().ensure_clean()?;
    Ok(Report {
        total_sum,
        shards_replied,
        exit_clean: true,
    })
}
