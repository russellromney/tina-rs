//! Multi-shard explicit-step simulator extracted from lib.rs (phase 055).
//!
//! Houses `MultiShardSimulator`, `MultiShardSimulatorConfig` impls, the
//! `RemoteQueueIndexes` / `RemoteQueues` type aliases, and the
//! `build_remote_queues` / `build_remote_queue_storage` helpers used to set
//! up deterministic cross-shard envelopes for whole-system runs.

use std::collections::{BTreeMap, VecDeque};
use std::time::Duration;

use tina::{Address, Isolate, Outbound as TinaOutbound, Shard, ShardId, TrySendError};
use tina_runtime::sharded::ReplyAdapter;
use tina_runtime::{RuntimeCall, RuntimeCallable, RuntimeEvent, SendRejectedReason};
use tina_supervisor::SupervisorConfig;

use crate::config::{
    Checker, CheckerDecision, CheckerFailure, DurableImage, MultiShardReplayArtifact,
    SimulatorConfig,
};
use crate::{IdSource, IntoErasedSpawn, QueuedRemoteEnvelope, Simulator};

type RemoteQueueIndexes = BTreeMap<(ShardId, ShardId), usize>;
type RemoteQueues = Vec<VecDeque<QueuedRemoteEnvelope>>;

/// Deterministic explicit-step coordinator over a fixed set of shard simulators.
///
/// This additive shell preserves the existing single-shard [`Simulator`] API
/// while giving Galileo one honest place to define global ingress, global
/// stepping order, and explicit root placement by shard in virtual time.
pub struct MultiShardSimulator<S>
where
    S: Shard + 'static,
{
    simulators: Vec<Simulator<S>>,
    shard_ids: Vec<ShardId>,
    shard_indexes: BTreeMap<ShardId, usize>,
    remote_queue_indexes: RemoteQueueIndexes,
    config: MultiShardSimulatorConfig,
    remote_queues: RemoteQueues,
    next_remote_queues: RemoteQueues,
    last_checker_failure: Option<CheckerFailure>,
}

/// Bounded coordinator config for additive multi-shard simulator shells.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MultiShardSimulatorConfig {
    /// Capacity of each source-shard -> destination-shard queue.
    pub shard_pair_capacity: usize,
}

impl Default for MultiShardSimulatorConfig {
    fn default() -> Self {
        Self {
            shard_pair_capacity: 64,
        }
    }
}

impl<S> MultiShardSimulator<S>
where
    S: Shard + 'static,
{
    /// Creates one additive multi-shard simulator over the provided shards.
    ///
    /// Shards are stepped in ascending [`ShardId`] order, regardless of input
    /// order. Empty shard sets and duplicate shard ids are programmer errors
    /// and panic.
    pub fn new<I>(shards: I, config: SimulatorConfig) -> Self
    where
        I: IntoIterator<Item = S>,
    {
        Self::with_config(shards, config, MultiShardSimulatorConfig::default())
    }

    /// Creates one additive multi-shard simulator with explicit shard-pair
    /// queue boundedness.
    pub fn with_config<I>(
        shards: I,
        config: SimulatorConfig,
        multishard: MultiShardSimulatorConfig,
    ) -> Self
    where
        I: IntoIterator<Item = S>,
    {
        let mut shards: Vec<S> = shards.into_iter().collect();
        if shards.is_empty() {
            panic!("multi-shard simulator requires at least one shard");
        }
        if multishard.shard_pair_capacity == 0 {
            panic!("multi-shard simulator requires shard-pair capacity > 0");
        }

        shards.sort_by_key(Shard::id);
        for pair in shards.windows(2) {
            if pair[0].id() == pair[1].id() {
                panic!(
                    "multi-shard simulator received duplicate shard id {}",
                    pair[0].id().get()
                );
            }
        }

        let ids = IdSource::new();
        let mut simulators = Vec::with_capacity(shards.len());
        let mut shard_ids = Vec::with_capacity(shards.len());
        let mut shard_indexes = BTreeMap::new();
        for shard in shards {
            let shard_id = shard.id();
            shard_indexes.insert(shard_id, simulators.len());
            shard_ids.push(shard_id);
            simulators.push(Simulator::with_ids(shard, config.clone(), ids.clone()));
        }
        let (remote_queue_indexes, remote_queues) =
            build_remote_queues(&shard_ids, multishard.shard_pair_capacity);
        let next_remote_queues =
            build_remote_queue_storage(&shard_ids, multishard.shard_pair_capacity);

        Self {
            simulators,
            shard_ids,
            shard_indexes,
            remote_queue_indexes,
            config: multishard,
            remote_queues,
            next_remote_queues,
            last_checker_failure: None,
        }
    }

    /// Returns the shard ids owned by this coordinator in global step order.
    pub fn shard_ids(&self) -> Vec<ShardId> {
        self.shard_ids.clone()
    }

    /// Returns the current shared virtual time.
    pub fn now(&self) -> Duration {
        self.simulators
            .first()
            .map(|simulator| simulator.virtual_now)
            .unwrap_or(Duration::ZERO)
    }

    /// Returns the merged deterministic event record in global event-id order.
    pub fn trace(&self) -> Vec<RuntimeEvent> {
        let mut events: Vec<_> = self
            .simulators
            .iter()
            .flat_map(|simulator| simulator.trace().iter().copied())
            .collect();
        events.sort_by_key(|event| event.id());
        events
    }

    /// Returns whether any owned shard still has pending timers or undelivered
    /// runtime-owned completions.
    pub fn has_in_flight_calls(&self) -> bool {
        self.simulators.iter().any(Simulator::has_in_flight_calls)
    }

    /// Registers one root isolate on the requested owning shard.
    #[allow(private_bounds)]
    pub fn register_on<I, Msg, Outbound>(
        &mut self,
        shard: ShardId,
        isolate: I,
    ) -> Address<Msg, I::Reply>
    where
        I: Isolate<
                Message = Msg,
                Shard = S,
                Send = TinaOutbound<Outbound>,
                Call = RuntimeCall<Msg>,
            > + 'static,
        I::Call: RuntimeCallable,
        I::Spawn: IntoErasedSpawn<S> + 'static,
        I::Reply: 'static,
        Msg: 'static,
        Outbound: 'static,
    {
        self.simulator_mut(shard)
            .register::<I, Msg, Outbound>(isolate)
    }

    /// Registers one root isolate on the requested shard with an explicit
    /// mailbox capacity.
    #[allow(private_bounds)]
    pub fn register_with_capacity_on<I, Msg, Outbound>(
        &mut self,
        shard: ShardId,
        isolate: I,
        mailbox_capacity: usize,
    ) -> Address<Msg, I::Reply>
    where
        I: Isolate<
                Message = Msg,
                Shard = S,
                Send = TinaOutbound<Outbound>,
                Call = RuntimeCall<Msg>,
            > + 'static,
        I::Call: RuntimeCallable,
        I::Spawn: IntoErasedSpawn<S> + 'static,
        I::Reply: 'static,
        Msg: 'static,
        Outbound: 'static,
    {
        self.simulator_mut(shard)
            .register_with_mailbox_capacity::<I, Msg, Outbound>(isolate, mailbox_capacity)
    }

    /// Register a [`ReplyAdapter<M, T, S>`] on a chosen shard.
    /// Simulator parity for the multi-shard runtime forms.
    ///
    /// Translates inbound `M` to outbound `T` via the user-provided
    /// `From<M> for T` and forwards to `target`. Returns the bridge
    /// `Address<M>` callers send to.
    ///
    /// Mirrors `MultiShardRuntime::register_reply_adapter_on` and
    /// `ThreadedMultiShardRuntime::register_reply_adapter_on` (in
    /// `tina-runtime`). Bound lists are matched to each runtime's
    /// lower-level `register_with_capacity_on`. Mirror changes
    /// across all three.
    #[allow(private_bounds)]
    pub fn register_reply_adapter_on<M, T>(
        &mut self,
        shard: ShardId,
        target: Address<T>,
        mailbox_capacity: usize,
    ) -> Address<M>
    where
        M: 'static,
        T: 'static + From<M>,
        std::convert::Infallible: IntoErasedSpawn<S>,
        RuntimeCall<M>: RuntimeCallable,
    {
        self.register_with_capacity_on::<ReplyAdapter<M, T, S>, M, T>(
            shard,
            ReplyAdapter::new(target),
            mailbox_capacity,
        )
    }

    /// Configures a registered isolate as supervisor on its owning shard.
    pub fn supervise<M: 'static, R>(&mut self, parent: Address<M, R>, config: SupervisorConfig) {
        self.simulator_mut(parent.shard()).supervise(parent, config);
    }

    /// Attempts one typed global ingress send routed strictly by target shard.
    pub fn try_send<M: 'static, R>(
        &self,
        address: Address<M, R>,
        message: M,
    ) -> Result<(), TrySendError<M>> {
        self.simulator(address.shard()).try_send(address, message)
    }

    /// Advances the shared virtual monotonic time by `by`.
    pub fn advance_time(&mut self, by: Duration) {
        for simulator in &mut self.simulators {
            simulator.advance_time(by);
        }
    }

    /// Advances shared virtual time to the next due timer on any shard.
    pub fn advance_to_next_timer(&mut self) -> bool {
        let Some(next_deadline) = self
            .simulators
            .iter()
            .flat_map(|simulator| {
                simulator.timers.iter().map(|entry| entry.deadline).chain(
                    simulator
                        .pending_isolate_calls
                        .iter()
                        .map(|entry| entry.deadline),
                )
            })
            .min()
        else {
            return false;
        };

        for simulator in &mut self.simulators {
            if next_deadline > simulator.virtual_now {
                simulator.virtual_now = next_deadline;
            }
        }

        true
    }

    /// Runs one global deterministic round in ascending shard-id order.
    pub fn step(&mut self) -> usize {
        std::mem::swap(&mut self.remote_queues, &mut self.next_remote_queues);
        let mut delivered = 0;
        let config = self.config;
        let shard_ids = &self.shard_ids;
        let shard_indexes = &self.shard_indexes;
        let remote_queue_indexes = &self.remote_queue_indexes;
        let remote_queues = &mut self.remote_queues;
        let next_remote_queues = &mut self.next_remote_queues;
        let simulators = &mut self.simulators;

        for destination in shard_ids.iter().copied() {
            let index = shard_indexes.get(&destination).copied().unwrap_or_else(|| {
                panic!(
                    "multi-shard simulator targeted unknown shard {}",
                    destination.get()
                )
            });
            for source in shard_ids.iter().copied() {
                if source == destination {
                    continue;
                }
                let key = (source, destination);
                let queue_index = remote_queue_indexes.get(&key).copied().unwrap_or_else(|| {
                    panic!(
                        "multi-shard simulator missing queue from shard {} to shard {}",
                        source.get(),
                        destination.get()
                    )
                });
                while let Some(queued) = remote_queues[queue_index].pop_front() {
                    if let Some(outbound) = simulators[index].harvest_remote_envelope(queued) {
                        let target_shard = outbound.target_shard();
                        let key = (destination, target_shard);
                        let queue_index =
                            remote_queue_indexes.get(&key).copied().unwrap_or_else(|| {
                                panic!(
                                    "multi-shard simulator missing queue from shard {} to shard {}",
                                    destination.get(),
                                    target_shard.get()
                                )
                            });
                        let queue = &mut next_remote_queues[queue_index];
                        if queue.len() < config.shard_pair_capacity {
                            queue.push_back(outbound);
                        }
                    }
                }
            }
            delivered += simulators[index].step_with_remote(&mut |source_shard, envelope| {
                let target_shard = envelope.target_shard();
                if !shard_indexes.contains_key(&target_shard) {
                    panic!(
                        "multi-shard simulator targeted unknown destination shard {}",
                        target_shard.get()
                    );
                }

                let key = (source_shard, target_shard);
                let queue_index = remote_queue_indexes.get(&key).copied().unwrap_or_else(|| {
                    panic!(
                        "multi-shard simulator missing queue from shard {} to shard {}",
                        source_shard.get(),
                        target_shard.get()
                    )
                });
                let queue = &mut next_remote_queues[queue_index];
                if queue.len() >= config.shard_pair_capacity {
                    return Err(SendRejectedReason::Full);
                }
                queue.push_back(envelope);
                Ok(())
            });
        }

        delivered
    }

    /// Continues running until every shard and every cross-shard queue is
    /// quiescent.
    pub fn run_until_quiescent(&mut self) -> usize {
        let mut total = 0;
        loop {
            let delivered = self.step();
            total += delivered;
            if delivered > 0 {
                continue;
            }
            if self.advance_to_next_timer() {
                continue;
            }
            if self.has_in_flight_calls() {
                continue;
            }
            if self.remote_queues.iter().any(|queue| !queue.is_empty())
                || self
                    .next_remote_queues
                    .iter()
                    .any(|queue| !queue.is_empty())
            {
                continue;
            }
            if self.simulators.iter().any(Simulator::has_pending_messages) {
                continue;
            }
            break total;
        }
    }

    /// Runs until every shard and every cross-shard queue is quiescent, or
    /// until a checker halts the whole multi-shard run.
    pub fn run_until_quiescent_checked<C: Checker>(
        &mut self,
        checker: &mut C,
    ) -> Option<CheckerFailure> {
        self.last_checker_failure = None;
        let mut observed_len = 0;
        loop {
            let delivered = self.step();
            if let Some(failure) = self.observe_new_events(checker, &mut observed_len) {
                self.last_checker_failure = Some(failure.clone());
                return Some(failure);
            }
            if delivered > 0 {
                continue;
            }
            if self.advance_to_next_timer() {
                continue;
            }
            if self.has_in_flight_calls() {
                continue;
            }
            if self.remote_queues.iter().any(|queue| !queue.is_empty())
                || self
                    .next_remote_queues
                    .iter()
                    .any(|queue| !queue.is_empty())
            {
                continue;
            }
            if self.simulators.iter().any(Simulator::has_pending_messages) {
                continue;
            }
            return None;
        }
    }

    /// Captures a deterministic replay artifact for the current whole-run
    /// multi-shard state.
    pub fn replay_artifact(&self) -> MultiShardReplayArtifact {
        MultiShardReplayArtifact {
            simulator_config: self
                .simulators
                .first()
                .map(|simulator| simulator.config().clone())
                .unwrap_or_default(),
            multishard_config: self.config,
            final_time: self.now(),
            event_record: self.trace(),
            checker_failure: self.last_checker_failure.clone(),
            observed_peer_output: self
                .simulators
                .iter()
                .flat_map(Simulator::observed_peer_output)
                .collect(),
            durable_image: DurableImage {
                files: self
                    .simulators
                    .iter()
                    .flat_map(|simulator| simulator.durable_image().files.into_iter())
                    .collect(),
            },
        }
    }

    fn simulator(&self, shard: ShardId) -> &Simulator<S> {
        &self.simulators[self.checked_shard_index(shard)]
    }

    fn simulator_mut(&mut self, shard: ShardId) -> &mut Simulator<S> {
        let index = self.checked_shard_index(shard);
        &mut self.simulators[index]
    }

    fn checked_shard_index(&self, shard: ShardId) -> usize {
        self.shard_indexes.get(&shard).copied().unwrap_or_else(|| {
            panic!(
                "multi-shard simulator targeted unknown shard {}",
                shard.get()
            )
        })
    }

    fn observe_new_events<C: Checker>(
        &self,
        checker: &mut C,
        observed_len: &mut usize,
    ) -> Option<CheckerFailure> {
        let trace = self.trace();
        while *observed_len < trace.len() {
            let event = trace[*observed_len];
            *observed_len += 1;
            match checker.on_event(&event) {
                CheckerDecision::Continue => {}
                CheckerDecision::Fail(reason) => {
                    return Some(CheckerFailure {
                        checker_name: checker.name(),
                        event_id: event.id(),
                        reason,
                    });
                }
            }
        }
        None
    }
}

fn build_remote_queues(
    shard_ids: &[ShardId],
    shard_pair_capacity: usize,
) -> (RemoteQueueIndexes, RemoteQueues) {
    let queue_count = shard_ids
        .len()
        .saturating_mul(shard_ids.len().saturating_sub(1));
    let mut indexes = BTreeMap::new();
    let mut queues = Vec::with_capacity(queue_count);
    for source in shard_ids.iter().copied() {
        for destination in shard_ids.iter().copied() {
            if source == destination {
                continue;
            }
            indexes.insert((source, destination), queues.len());
            queues.push(VecDeque::with_capacity(shard_pair_capacity));
        }
    }
    (indexes, queues)
}

fn build_remote_queue_storage(shard_ids: &[ShardId], shard_pair_capacity: usize) -> RemoteQueues {
    let queue_count = shard_ids
        .len()
        .saturating_mul(shard_ids.len().saturating_sub(1));
    let mut queues = Vec::with_capacity(queue_count);
    for _ in 0..queue_count {
        queues.push(VecDeque::with_capacity(shard_pair_capacity));
    }
    queues
}
