//! Multi-shard explicit-step coordinator extracted from lib.rs (phase 055).
//!
//! Houses `MultiShardRuntime`, `MultiShardRuntimeConfig`, and the remote-queue
//! plumbing helpers (`RemoteQueueIndexes`, `RemoteQueues`,
//! `build_remote_queues`, `build_remote_queue_storage`) that drive deterministic
//! cross-shard delivery.

use std::collections::{BTreeMap, VecDeque};

use tina::{Address, Isolate, Mailbox, Outbound as TinaOutbound, Shard, ShardId, TrySendError};
use tina_supervisor::SupervisorConfig;

use crate::call::{IntoErasedCall, RuntimeCall};
use crate::clock::MonotonicClock;
use crate::mailbox::MailboxFactory;
use crate::sharded::ReplyAdapter;
use crate::trace::{RuntimeEvent, SendRejectedReason};
use crate::{
    ChildRestartedWaiter, IdSource, IntoErasedSpawn, IntoErasedSpawnObserved,
    IntoSendErasedSpawnObserved, QueuedRemoteEnvelope, Runtime,
};

type RemoteQueueIndexes = BTreeMap<(ShardId, ShardId), usize>;
type RemoteQueues = Vec<VecDeque<QueuedRemoteEnvelope>>;

/// Deterministic explicit-step coordinator over a fixed set of shard runtimes.
///
/// This additive shell preserves the existing single-shard [`Runtime`] API
/// while giving Galileo one honest place to define global ingress, global
/// stepping order, and explicit root placement by shard.
pub struct MultiShardRuntime<S, F>
where
    S: Shard + 'static,
    F: MailboxFactory + 'static,
{
    runtimes: Vec<Runtime<S, F>>,
    shard_ids: Vec<ShardId>,
    shard_indexes: BTreeMap<ShardId, usize>,
    remote_queue_indexes: RemoteQueueIndexes,
    config: MultiShardRuntimeConfig,
    remote_queues: RemoteQueues,
    next_remote_queues: RemoteQueues,
    terminal_remote_queues: RemoteQueues,
    next_terminal_remote_queues: RemoteQueues,
    terminal_overflow_queues: RemoteQueues,
}

/// Bounded coordinator config for additive multi-shard runtime shells.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MultiShardRuntimeConfig {
    /// Capacity of each source-shard -> destination-shard queue.
    pub shard_pair_capacity: usize,
}

impl Default for MultiShardRuntimeConfig {
    fn default() -> Self {
        Self {
            shard_pair_capacity: 64,
        }
    }
}

impl<S, F> MultiShardRuntime<S, F>
where
    S: Shard + 'static,
    F: MailboxFactory + Clone + 'static,
{
    /// Creates one additive multi-shard coordinator over the provided shards.
    ///
    /// Shards are stepped in ascending [`ShardId`] order, regardless of input
    /// order. Empty shard sets and duplicate shard ids are programmer errors
    /// and panic.
    pub fn new<I>(shards: I, mailbox_factory: F) -> Self
    where
        I: IntoIterator<Item = S>,
    {
        Self::with_config(shards, mailbox_factory, MultiShardRuntimeConfig::default())
    }

    /// Creates one additive multi-shard coordinator with explicit shard-pair
    /// queue boundedness.
    pub fn with_config<I>(shards: I, mailbox_factory: F, config: MultiShardRuntimeConfig) -> Self
    where
        I: IntoIterator<Item = S>,
    {
        let mut shards: Vec<S> = shards.into_iter().collect();
        if shards.is_empty() {
            panic!("multi-shard runtime requires at least one shard");
        }
        if config.shard_pair_capacity == 0 {
            panic!("multi-shard runtime requires shard-pair capacity > 0");
        }

        shards.sort_by_key(Shard::id);
        for pair in shards.windows(2) {
            if pair[0].id() == pair[1].id() {
                panic!(
                    "multi-shard runtime received duplicate shard id {}",
                    pair[0].id().get()
                );
            }
        }

        let ids = IdSource::new();
        let mut runtimes = Vec::with_capacity(shards.len());
        let mut shard_ids = Vec::with_capacity(shards.len());
        let mut shard_indexes = BTreeMap::new();
        for shard in shards {
            let shard_id = shard.id();
            shard_indexes.insert(shard_id, runtimes.len());
            shard_ids.push(shard_id);
            let mut runtime = Runtime::with_clock_and_ids(
                shard,
                mailbox_factory.clone(),
                Box::new(MonotonicClock),
                ids.clone(),
            );
            runtime.remote_child_control_capacity = config.shard_pair_capacity;
            runtimes.push(runtime);
        }
        let (remote_queue_indexes, remote_queues) =
            build_remote_queues(&shard_ids, config.shard_pair_capacity);
        let next_remote_queues = build_remote_queue_storage(&shard_ids, config.shard_pair_capacity);
        let terminal_remote_queues =
            build_remote_queue_storage(&shard_ids, config.shard_pair_capacity);
        let next_terminal_remote_queues =
            build_remote_queue_storage(&shard_ids, config.shard_pair_capacity);
        let terminal_overflow_queues =
            build_remote_queue_storage(&shard_ids, config.shard_pair_capacity);

        Self {
            runtimes,
            shard_ids,
            shard_indexes,
            remote_queue_indexes,
            config,
            remote_queues,
            next_remote_queues,
            terminal_remote_queues,
            next_terminal_remote_queues,
            terminal_overflow_queues,
        }
    }

    /// Returns the shard ids owned by this coordinator in global step order.
    pub fn shard_ids(&self) -> Vec<ShardId> {
        self.shard_ids.clone()
    }

    /// Returns the merged deterministic event record in global event-id order.
    pub fn trace(&self) -> Vec<RuntimeEvent> {
        let mut events: Vec<_> = self
            .runtimes
            .iter()
            .flat_map(|runtime| runtime.trace().iter().copied())
            .collect();
        events.sort_by_key(|event| event.id());
        events
    }

    /// Returns whether any owned shard still has in-flight runtime-owned work.
    pub fn has_in_flight_calls(&self) -> bool {
        self.runtimes.iter().any(Runtime::has_in_flight_calls)
    }

    /// Registers one root isolate on the requested owning shard.
    #[allow(private_bounds)]
    pub fn register_on<I, M, Outbound>(
        &mut self,
        shard: ShardId,
        isolate: I,
        mailbox: M,
    ) -> Address<I::Message, I::Reply>
    where
        I: Isolate<Shard = S, Send = TinaOutbound<Outbound>> + 'static,
        I::Message: 'static,
        I::Reply: 'static,
        I::Spawn: IntoErasedSpawn<S, F> + 'static,
        I::SpawnObserved: IntoErasedSpawnObserved<S, F, I::Message> + 'static,
        I::SpawnObservedRemote: IntoSendErasedSpawnObserved<S, F, I::Message> + 'static,
        I::Io: IntoErasedCall<I::Message> + 'static,
        I::Fact: crate::fact::IntoRuntimeFact + 'static,
        Outbound: 'static,
        M: Mailbox<I::Message> + 'static,
    {
        self.runtime_mut(shard)
            .register::<I, M, Outbound>(isolate, mailbox)
    }

    /// Registers one root isolate on the requested shard and lets that shard
    /// runtime allocate the mailbox.
    #[allow(private_bounds)]
    pub fn register_with_capacity_on<I, Outbound>(
        &mut self,
        shard: ShardId,
        isolate: I,
        mailbox_capacity: usize,
    ) -> Address<I::Message, I::Reply>
    where
        I: Isolate<Shard = S, Send = TinaOutbound<Outbound>> + 'static,
        I::Message: 'static,
        I::Reply: 'static,
        I::Spawn: IntoErasedSpawn<S, F> + 'static,
        I::SpawnObserved: IntoErasedSpawnObserved<S, F, I::Message> + 'static,
        I::SpawnObservedRemote: IntoSendErasedSpawnObserved<S, F, I::Message> + 'static,
        I::Io: IntoErasedCall<I::Message> + 'static,
        I::Fact: crate::fact::IntoRuntimeFact + 'static,
        Outbound: 'static,
    {
        self.runtime_mut(shard)
            .register_with_capacity::<I, Outbound>(isolate, mailbox_capacity)
    }

    /// Multi-shard mirror of [`Runtime::register_with_capacity_and_bootstrap`].
    #[allow(private_bounds, clippy::type_complexity)]
    pub fn register_with_capacity_and_bootstrap_on<I, Outbound>(
        &mut self,
        shard: ShardId,
        isolate: I,
        mailbox_capacity: usize,
        bootstrap: I::Message,
    ) -> Result<Address<I::Message, I::Reply>, crate::errors::RegisterBootstrapError<I::Message>>
    where
        I: Isolate<Shard = S, Send = TinaOutbound<Outbound>> + 'static,
        I::Message: 'static,
        I::Reply: 'static,
        I::Spawn: IntoErasedSpawn<S, F> + 'static,
        I::SpawnObserved: IntoErasedSpawnObserved<S, F, I::Message> + 'static,
        I::SpawnObservedRemote: IntoSendErasedSpawnObserved<S, F, I::Message> + 'static,
        I::Io: IntoErasedCall<I::Message> + 'static,
        I::Fact: crate::fact::IntoRuntimeFact + 'static,
        Outbound: 'static,
    {
        self.runtime_mut(shard)
            .register_with_capacity_and_bootstrap::<I, Outbound>(
                isolate,
                mailbox_capacity,
                bootstrap,
            )
    }

    /// Register a [`ReplyAdapter<M, T, S>`] on a chosen shard.
    ///
    /// Translates inbound `M` to outbound `T` via the user-provided
    /// `From<M> for T` and forwards to `target`. Returns the bridge
    /// `Address<M>` callers send to.
    ///
    /// Equivalent to
    /// `register_with_capacity_on::<ReplyAdapter<M, T, S>, T>(shard, ReplyAdapter::new(target), capacity)`
    /// but does not require restating the adapter's full type. The
    /// adapter has the same per-isolate state any registered isolate
    /// has (one entry, one bounded mailbox, one handler); no extra
    /// queue, no target clone, no scatter/gather policy.
    ///
    /// Caller picks `shard`. Co-locating with the coordinator keeps
    /// reply translation local; pinning on the target's shard moves
    /// it off the caller's hot path. The helper does not pick.
    ///
    /// Mirrors
    /// [`crate::ThreadedMultiShardRuntime::register_reply_adapter_on`]
    /// (threaded) and `MultiShardSimulator::register_reply_adapter_on`
    /// (in `tina-sim`). Bound lists are matched to each runtime's
    /// lower-level `register_with_capacity_on`; mirror changes across
    /// all three.
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
        std::convert::Infallible: IntoErasedSpawn<S, F>,
        RuntimeCall<M>: IntoErasedCall<M>,
    {
        self.register_with_capacity_on::<ReplyAdapter<M, T, S>, T>(
            shard,
            ReplyAdapter::new(target),
            mailbox_capacity,
        )
    }

    /// Configures a registered isolate as supervisor on its owning shard.
    pub fn supervise<M: 'static, R>(&mut self, parent: Address<M, R>, config: SupervisorConfig) {
        self.runtime_mut(parent.shard()).supervise(parent, config);
    }

    /// Returns the live runtime-owned lifecycle report for direct children of
    /// `parent`.
    pub fn child_lifecycle_report<M: 'static, R>(
        &self,
        parent: Address<M, R>,
    ) -> Result<crate::ChildLifecycleReport, crate::ChildLifecycleReportError> {
        let Some(index) = self.shard_indexes.get(&parent.shard()).copied() else {
            return Err(crate::ChildLifecycleReportError::ParentShardUnavailable(
                parent.shard(),
            ));
        };
        self.runtimes[index].child_lifecycle_report(parent)
    }

    /// Registers a typed waiter for the next child restart reported on the
    /// parent's owning shard. Cross-shard child restarts resolve here with the
    /// replacement shard/isolate/generation, so callers do not need to mine the
    /// trace for replacement addresses.
    pub fn observe_child_restarted<M: 'static, R>(
        &mut self,
        parent: Address<M, R>,
    ) -> ChildRestartedWaiter {
        self.runtime_mut(parent.shard())
            .observe_child_restarted(parent)
    }

    /// Attempts one typed global ingress send routed strictly by target shard.
    pub fn try_send<M: 'static, R>(
        &self,
        address: Address<M, R>,
        message: M,
    ) -> Result<(), TrySendError<M>> {
        self.runtime(address.shard()).try_send(address, message)
    }

    /// Runs one global deterministic round in ascending shard-id order.
    pub fn step(&mut self) -> usize {
        std::mem::swap(&mut self.remote_queues, &mut self.next_remote_queues);
        std::mem::swap(
            &mut self.terminal_remote_queues,
            &mut self.next_terminal_remote_queues,
        );
        let mut delivered = 0;
        let config = self.config;
        let shard_ids = &self.shard_ids;
        let shard_indexes = &self.shard_indexes;
        let remote_queue_indexes = &self.remote_queue_indexes;
        let remote_queues = &mut self.remote_queues;
        let next_remote_queues = &mut self.next_remote_queues;
        let terminal_remote_queues = &mut self.terminal_remote_queues;
        let next_terminal_remote_queues = &mut self.next_terminal_remote_queues;
        let terminal_overflow_queues = &mut self.terminal_overflow_queues;
        let runtimes = &mut self.runtimes;

        flush_terminal_overflow_queues(
            terminal_overflow_queues,
            next_terminal_remote_queues,
            config.shard_pair_capacity,
        );
        let mut remote_buffers = RemoteQueueBuffers {
            indexes: remote_queue_indexes,
            next_remote: next_remote_queues,
            next_terminal: next_terminal_remote_queues,
            terminal_overflow: terminal_overflow_queues,
            shard_pair_capacity: config.shard_pair_capacity,
            label: "multi-shard runtime",
        };

        for destination in shard_ids.iter().copied() {
            let index = shard_indexes.get(&destination).copied().unwrap_or_else(|| {
                panic!(
                    "multi-shard runtime targeted unknown shard {}",
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
                        "multi-shard runtime missing queue from shard {} to shard {}",
                        source.get(),
                        destination.get()
                    )
                });
                while let Some(queued) = terminal_remote_queues[queue_index].pop_front() {
                    if let Some(outbound) = runtimes[index].harvest_remote_envelope(queued) {
                        let _ = enqueue_remote_envelope_preserving_terminal(
                            destination,
                            outbound,
                            &mut remote_buffers,
                        );
                    }
                }
                while let Some(queued) = remote_queues[queue_index].pop_front() {
                    if let Some(outbound) = runtimes[index].harvest_remote_envelope(queued) {
                        let _ = enqueue_remote_envelope_preserving_terminal(
                            destination,
                            outbound,
                            &mut remote_buffers,
                        );
                    }
                }
            }
            delivered += runtimes[index].step_with_remote(&mut |source_shard, envelope| {
                let target_shard = envelope.target_shard();
                if !shard_indexes.contains_key(&target_shard) {
                    panic!(
                        "multi-shard runtime targeted unknown destination shard {}",
                        target_shard.get()
                    );
                }

                enqueue_remote_envelope_preserving_terminal(
                    source_shard,
                    envelope,
                    &mut remote_buffers,
                )
            });
        }

        delivered
    }

    fn runtime(&self, shard: ShardId) -> &Runtime<S, F> {
        &self.runtimes[self.checked_shard_index(shard)]
    }

    fn runtime_mut(&mut self, shard: ShardId) -> &mut Runtime<S, F> {
        let index = self.checked_shard_index(shard);
        &mut self.runtimes[index]
    }

    fn checked_shard_index(&self, shard: ShardId) -> usize {
        self.shard_indexes
            .get(&shard)
            .copied()
            .unwrap_or_else(|| panic!("multi-shard runtime targeted unknown shard {}", shard.get()))
    }
}

struct RemoteQueueBuffers<'a> {
    indexes: &'a RemoteQueueIndexes,
    next_remote: &'a mut RemoteQueues,
    next_terminal: &'a mut RemoteQueues,
    terminal_overflow: &'a mut RemoteQueues,
    shard_pair_capacity: usize,
    label: &'static str,
}

fn enqueue_remote_envelope_preserving_terminal(
    source_shard: ShardId,
    envelope: QueuedRemoteEnvelope,
    buffers: &mut RemoteQueueBuffers<'_>,
) -> Result<(), SendRejectedReason> {
    let target_shard = envelope.target_shard();
    let key = (source_shard, target_shard);
    let queue_index = buffers.indexes.get(&key).copied().unwrap_or_else(|| {
        panic!(
            "{} missing queue from shard {} to shard {}",
            buffers.label,
            source_shard.get(),
            target_shard.get()
        )
    });
    let terminal = matches!(
        envelope,
        QueuedRemoteEnvelope::CallReply(_)
            | QueuedRemoteEnvelope::SpawnReply(_)
            | QueuedRemoteEnvelope::SpawnCancel(_)
            | QueuedRemoteEnvelope::ChildStop(_)
            | QueuedRemoteEnvelope::ChildStopped(_)
            | QueuedRemoteEnvelope::ChildRestart(_)
            | QueuedRemoteEnvelope::ChildRestarted(_)
    );
    let queue = if terminal {
        &mut buffers.next_terminal[queue_index]
    } else {
        &mut buffers.next_remote[queue_index]
    };
    if queue.len() < buffers.shard_pair_capacity {
        queue.push_back(envelope);
        return Ok(());
    }
    if terminal {
        buffers.terminal_overflow[queue_index].push_back(envelope);
        Ok(())
    } else {
        Err(SendRejectedReason::Full)
    }
}

fn flush_terminal_overflow_queues(
    terminal_overflow_queues: &mut RemoteQueues,
    next_terminal_remote_queues: &mut RemoteQueues,
    shard_pair_capacity: usize,
) {
    for (overflow, next_terminal) in terminal_overflow_queues
        .iter_mut()
        .zip(next_terminal_remote_queues.iter_mut())
    {
        while next_terminal.len() < shard_pair_capacity {
            let Some(envelope) = overflow.pop_front() else {
                break;
            };
            next_terminal.push_back(envelope);
        }
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
