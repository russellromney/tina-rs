//! Multi-shard explicit-step simulator extracted from lib.rs.
//!
//! Houses `MultiShardSimulator`, `MultiShardSimulatorConfig` impls, the
//! `RemoteQueueIndexes` / `RemoteQueues` type aliases, and the
//! `build_remote_queues` / `build_remote_queue_storage` helpers used to set
//! up deterministic cross-shard envelopes for whole-system runs.

use std::collections::{BTreeMap, VecDeque};
use std::time::Duration;

use tina::{Address, Isolate, Outbound as TinaOutbound, Shard, ShardId};
use tina_runtime::sharded::ReplyAdapter;
use tina_runtime::{
    RegisterBootstrapError, RuntimeCall, RuntimeCallable, RuntimeEvent, SendRejectedReason,
};
use tina_supervisor::SupervisorConfig;

use crate::config::{
    Checker, CheckerDecision, CheckerFailure, DurableImage, MultiShardReplayArtifact,
    SimulatorConfig,
};
use crate::{
    IdSource, IntoErasedSpawn, IntoErasedSpawnObserved, IntoSimRemoteSpawnObserved,
    QueuedRemoteEnvelope, Simulator,
};

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
    terminal_remote_queues: RemoteQueues,
    next_terminal_remote_queues: RemoteQueues,
    terminal_overflow_queues: RemoteQueues,
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
    /// Returns the provenance shared by every owned shard simulator.
    pub fn system_incarnation(&self) -> tina::SystemIncarnation {
        self.simulators[0].system_incarnation
    }

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
        assert!(
            !config
                .system_incarnation
                .is_some_and(tina::SystemIncarnation::is_unscoped),
            "multi-shard simulator system incarnation must be nonzero"
        );
        let system_incarnation = config
            .system_incarnation
            .unwrap_or_else(tina_runtime::fresh_system_incarnation);
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
            simulators.push(Simulator::with_ids_and_system(
                shard,
                config.clone(),
                ids.clone(),
                system_incarnation,
            ));
        }
        let (remote_queue_indexes, remote_queues) =
            build_remote_queues(&shard_ids, multishard.shard_pair_capacity);
        let next_remote_queues =
            build_remote_queue_storage(&shard_ids, multishard.shard_pair_capacity);
        let terminal_remote_queues =
            build_remote_queue_storage(&shard_ids, multishard.shard_pair_capacity);
        let next_terminal_remote_queues =
            build_remote_queue_storage(&shard_ids, multishard.shard_pair_capacity);
        let terminal_overflow_queues =
            build_remote_queue_storage(&shard_ids, multishard.shard_pair_capacity);

        Self {
            simulators,
            shard_ids,
            shard_indexes,
            remote_queue_indexes,
            config: multishard,
            remote_queues,
            next_remote_queues,
            terminal_remote_queues,
            next_terminal_remote_queues,
            terminal_overflow_queues,
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
    ///
    /// The simulator's shared global event counter assigns ids in fixed
    /// step order, so a global id sort is the deterministic cross-shard
    /// emission order — the order the DST checkers expect.
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
        I: Isolate<Message = Msg, Shard = S, Send = TinaOutbound<Outbound>, Io = RuntimeCall<Msg>>
            + 'static,
        I::Io: RuntimeCallable,
        I::Spawn: IntoErasedSpawn<S> + 'static,
        I::SpawnObserved: IntoErasedSpawnObserved<S, I::Message> + 'static,
        I::SpawnObservedRemote: IntoSimRemoteSpawnObserved<S, I::Message> + 'static,
        I::Reply: 'static,
        I::Fact: tina_runtime::IntoRuntimeFact + 'static,
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
        I: Isolate<Message = Msg, Shard = S, Send = TinaOutbound<Outbound>, Io = RuntimeCall<Msg>>
            + 'static,
        I::Io: RuntimeCallable,
        I::Spawn: IntoErasedSpawn<S> + 'static,
        I::SpawnObserved: IntoErasedSpawnObserved<S, I::Message> + 'static,
        I::SpawnObservedRemote: IntoSimRemoteSpawnObserved<S, I::Message> + 'static,
        I::Reply: 'static,
        I::Fact: tina_runtime::IntoRuntimeFact + 'static,
        Msg: 'static,
        Outbound: 'static,
    {
        self.simulator_mut(shard)
            .register_with_mailbox_capacity::<I, Msg, Outbound>(isolate, mailbox_capacity)
    }

    /// Registers one root isolate on `shard` and atomically prefills its
    /// bounded mailbox with `bootstrap`.
    ///
    /// This mirrors
    /// [`tina_runtime::MultiShardRuntime::register_with_capacity_and_bootstrap_on`].
    /// Mailbox refusal returns the bootstrap message and publishes no isolate
    /// entry or address.
    ///
    /// # Panics
    ///
    /// Panics when `shard` is not owned by this simulator, matching the other
    /// simulator registration APIs.
    #[allow(private_bounds, clippy::type_complexity)]
    pub fn register_with_capacity_and_bootstrap_on<I, Msg, Outbound>(
        &mut self,
        shard: ShardId,
        isolate: I,
        mailbox_capacity: usize,
        bootstrap: Msg,
    ) -> Result<Address<Msg, I::Reply>, RegisterBootstrapError<Msg>>
    where
        I: Isolate<Message = Msg, Shard = S, Send = TinaOutbound<Outbound>, Io = RuntimeCall<Msg>>
            + 'static,
        I::Io: RuntimeCallable,
        I::Spawn: IntoErasedSpawn<S> + 'static,
        I::SpawnObserved: IntoErasedSpawnObserved<S, I::Message> + 'static,
        I::SpawnObservedRemote: IntoSimRemoteSpawnObserved<S, I::Message> + 'static,
        I::Reply: 'static,
        I::Fact: tina_runtime::IntoRuntimeFact + 'static,
        Msg: 'static,
        Outbound: 'static,
    {
        self.simulator_mut(shard)
            .register_with_capacity_and_bootstrap::<I, Msg, Outbound>(
                isolate,
                mailbox_capacity,
                bootstrap,
            )
    }

    /// Registers one root on the requested shard whose constructor receives
    /// its final typed address before the entry is published.
    ///
    /// # Panics
    ///
    /// Panics when `shard` is not owned or when `construct` panics. Constructor
    /// panic consumes the shard-local deterministic id without adding an entry.
    /// Sending through an address leaked by a panicking constructor follows the
    /// simulator's unknown-isolate contract and panics; the id is never reused.
    #[allow(private_bounds)]
    pub fn register_with_capacity_using_on<I, Msg, Outbound, Ctor>(
        &mut self,
        shard: ShardId,
        mailbox_capacity: usize,
        construct: Ctor,
    ) -> Address<Msg, I::Reply>
    where
        I: Isolate<Message = Msg, Shard = S, Send = TinaOutbound<Outbound>, Io = RuntimeCall<Msg>>
            + 'static,
        I::Io: RuntimeCallable,
        I::Spawn: IntoErasedSpawn<S> + 'static,
        I::SpawnObserved: IntoErasedSpawnObserved<S, I::Message> + 'static,
        I::SpawnObservedRemote: IntoSimRemoteSpawnObserved<S, I::Message> + 'static,
        I::Reply: 'static,
        I::Fact: tina_runtime::IntoRuntimeFact + 'static,
        Msg: 'static,
        Outbound: 'static,
        Ctor: FnOnce(Address<Msg, I::Reply>) -> I,
    {
        self.simulator_mut(shard)
            .register_with_capacity_using::<I, Msg, Outbound, _>(mailbox_capacity, construct)
    }

    /// Registers one split event/request service on the requested shard.
    ///
    /// # Panics
    ///
    /// Panics when `shard` is not owned by this simulator.
    #[allow(private_bounds)]
    pub fn register_split_service_on<I, Event, Request, Outbound>(
        &mut self,
        shard: ShardId,
        isolate: I,
        mailbox_capacity: usize,
    ) -> tina_runtime::SplitServiceHandle<Event, Request, I::Reply>
    where
        I: Isolate<
                Message = tina::ServiceMessage<Event, Request>,
                Shard = S,
                Send = TinaOutbound<Outbound>,
                Io = RuntimeCall<tina::ServiceMessage<Event, Request>>,
            > + tina::CallableIsolate
            + 'static,
        I::Io: RuntimeCallable,
        I::Spawn: IntoErasedSpawn<S> + 'static,
        I::SpawnObserved: IntoErasedSpawnObserved<S, I::Message> + 'static,
        I::SpawnObservedRemote: IntoSimRemoteSpawnObserved<S, I::Message> + 'static,
        I::Reply: 'static,
        I::Fact: tina_runtime::IntoRuntimeFact + 'static,
        Event: 'static,
        Request: 'static,
        Outbound: 'static,
    {
        tina_runtime::SplitServiceHandle::from_address(
            self.register_with_capacity_on::<I, tina::ServiceMessage<Event, Request>, Outbound>(
                shard,
                isolate,
                mailbox_capacity,
            ),
        )
    }

    /// Registers one split service on `shard` and atomically prefills its
    /// first event.
    #[allow(private_bounds, clippy::type_complexity)]
    pub fn register_split_service_with_bootstrap_on<I, Event, Request, Outbound>(
        &mut self,
        shard: ShardId,
        isolate: I,
        mailbox_capacity: usize,
        bootstrap: Event,
    ) -> Result<
        tina_runtime::SplitServiceHandle<Event, Request, I::Reply>,
        tina_runtime::RegisterBootstrapError<Event>,
    >
    where
        I: Isolate<
                Message = tina::ServiceMessage<Event, Request>,
                Shard = S,
                Send = TinaOutbound<Outbound>,
                Io = RuntimeCall<tina::ServiceMessage<Event, Request>>,
            > + tina::CallableIsolate
            + 'static,
        I::Io: RuntimeCallable,
        I::Spawn: IntoErasedSpawn<S> + 'static,
        I::SpawnObserved: IntoErasedSpawnObserved<S, I::Message> + 'static,
        I::SpawnObservedRemote: IntoSimRemoteSpawnObserved<S, I::Message> + 'static,
        I::Reply: 'static,
        I::Fact: tina_runtime::IntoRuntimeFact + 'static,
        Event: 'static,
        Request: 'static,
        Outbound: 'static,
    {
        self.register_with_capacity_and_bootstrap_on::<
            I,
            tina::ServiceMessage<Event, Request>,
            Outbound,
        >(
            shard,
            isolate,
            mailbox_capacity,
            tina::ServiceMessage::Event(bootstrap),
        )
        .map(tina_runtime::SplitServiceHandle::from_address)
        .map_err(|error| {
            error.map_message(|message| match message {
                tina::ServiceMessage::Event(event) => event,
                tina::ServiceMessage::Request(_) => {
                    unreachable!("split-service bootstrap was constructed as an event")
                }
            })
        })
    }

    /// Registers one event-only service on the requested shard.
    ///
    /// # Panics
    ///
    /// Panics when `shard` is not owned by this simulator.
    #[allow(private_bounds)]
    pub fn register_event_service_on<I, Event, Outbound>(
        &mut self,
        shard: ShardId,
        isolate: I,
        mailbox_capacity: usize,
    ) -> tina_runtime::EventServiceHandle<Event>
    where
        I: Isolate<
                Message = tina::ServiceMessage<Event, std::convert::Infallible>,
                Reply = (),
                Shard = S,
                Send = TinaOutbound<Outbound>,
                Io = RuntimeCall<tina::ServiceMessage<Event, std::convert::Infallible>>,
            > + 'static,
        I::Io: RuntimeCallable,
        I::Spawn: IntoErasedSpawn<S> + 'static,
        I::SpawnObserved: IntoErasedSpawnObserved<S, I::Message> + 'static,
        I::SpawnObservedRemote: IntoSimRemoteSpawnObserved<S, I::Message> + 'static,
        I::Fact: tina_runtime::IntoRuntimeFact + 'static,
        Event: 'static,
        Outbound: 'static,
    {
        tina_runtime::SplitServiceHandle::from_address(self.register_with_capacity_on::<
            I,
            tina::ServiceMessage<Event, std::convert::Infallible>,
            Outbound,
        >(shard, isolate, mailbox_capacity))
        .events
    }

    /// Registers one request-only service on the requested shard.
    ///
    /// # Panics
    ///
    /// Panics when `shard` is not owned by this simulator.
    #[allow(private_bounds)]
    pub fn register_request_service_on<I, Request, Outbound>(
        &mut self,
        shard: ShardId,
        isolate: I,
        mailbox_capacity: usize,
    ) -> tina_runtime::RequestServiceHandle<Request, I::Reply>
    where
        I: Isolate<
                Message = tina::ServiceMessage<std::convert::Infallible, Request>,
                Shard = S,
                Send = TinaOutbound<Outbound>,
                Io = RuntimeCall<tina::ServiceMessage<std::convert::Infallible, Request>>,
            > + tina::CallableIsolate
            + 'static,
        I::Io: RuntimeCallable,
        I::Spawn: IntoErasedSpawn<S> + 'static,
        I::SpawnObserved: IntoErasedSpawnObserved<S, I::Message> + 'static,
        I::SpawnObservedRemote: IntoSimRemoteSpawnObserved<S, I::Message> + 'static,
        I::Reply: 'static,
        I::Fact: tina_runtime::IntoRuntimeFact + 'static,
        Request: 'static,
        Outbound: 'static,
    {
        self.register_split_service_on::<I, std::convert::Infallible, Request, Outbound>(
            shard,
            isolate,
            mailbox_capacity,
        )
        .requests
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
    ) -> Result<(), tina_runtime::IngressSendError<M>> {
        let expected = self.simulators[0].system_incarnation;
        if address.system() != expected {
            return Err(tina_runtime::IngressSendError::ForeignSystem {
                expected,
                actual: address.system(),
                message,
            });
        }
        self.simulator(address.shard()).try_send(address, message)
    }

    /// Attempts one event send through a service event capability.
    ///
    /// # Panics
    ///
    /// Panics when the address targets a shard not owned by this simulator.
    pub fn try_send_event<Event: 'static, Request: 'static>(
        &self,
        address: tina::ServiceEventAddress<Event, Request>,
        event: Event,
    ) -> Result<(), tina_runtime::IngressSendError<Event>> {
        match self.try_send(
            address.address().address(),
            tina::ServiceMessage::Event(event),
        ) {
            Ok(()) => Ok(()),
            Err(tina_runtime::IngressSendError::Full(tina::ServiceMessage::Event(event))) => {
                Err(tina_runtime::IngressSendError::Full(event))
            }
            Err(tina_runtime::IngressSendError::Closed(tina::ServiceMessage::Event(event))) => {
                Err(tina_runtime::IngressSendError::Closed(event))
            }
            Err(tina_runtime::IngressSendError::ForeignSystem {
                expected,
                actual,
                message: tina::ServiceMessage::Event(event),
            }) => Err(tina_runtime::IngressSendError::ForeignSystem {
                expected,
                actual,
                message: event,
            }),
            Err(tina_runtime::IngressSendError::Full(tina::ServiceMessage::Request(_)))
            | Err(tina_runtime::IngressSendError::Closed(tina::ServiceMessage::Request(_)))
            | Err(tina_runtime::IngressSendError::ForeignSystem {
                message: tina::ServiceMessage::Request(_),
                ..
            }) => {
                unreachable!("simulator returned a different service payload than it received")
            }
        }
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
                simulator
                    .timers
                    .iter()
                    .map(|entry| entry.deadline)
                    .chain(simulator.call_table.min_isolate_deadline())
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
        let simulators = &mut self.simulators;

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
            label: "multi-shard simulator",
        };

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
                while let Some(queued) = terminal_remote_queues[queue_index].pop_front() {
                    if let Some(outbound) = simulators[index].harvest_remote_envelope(queued) {
                        let _ = enqueue_remote_envelope_preserving_terminal(
                            destination,
                            outbound,
                            &mut remote_buffers,
                        );
                    }
                }
                while let Some(queued) = remote_queues[queue_index].pop_front() {
                    if let Some(outbound) = simulators[index].harvest_remote_envelope(queued) {
                        let _ = enqueue_remote_envelope_preserving_terminal(
                            destination,
                            outbound,
                            &mut remote_buffers,
                        );
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

                enqueue_remote_envelope_preserving_terminal(
                    source_shard,
                    envelope,
                    &mut remote_buffers,
                )
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
                || self
                    .terminal_remote_queues
                    .iter()
                    .any(|queue| !queue.is_empty())
                || self
                    .next_terminal_remote_queues
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
                || self
                    .terminal_remote_queues
                    .iter()
                    .any(|queue| !queue.is_empty())
                || self
                    .next_terminal_remote_queues
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
        QueuedRemoteEnvelope::CallReply(_) | QueuedRemoteEnvelope::SpawnReply(_)
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
