//! Threaded multi-shard runtime extracted from lib.rs (phase 055).
//!
//! Houses `ThreadedMultiShardRuntime`, the cross-shard worker loop
//! `threaded_worker_loop_with_remote`, and the remote-inbound drain helper
//! `drain_remote_inbound`. Each owned shard runs its own worker thread; this
//! type coordinates ingress, trace, capability, supervise, and shutdown across
//! the set.

use std::alloc::Global;
use std::collections::{BTreeMap, VecDeque};
use std::convert::Infallible;
use std::marker::PhantomData;
use std::sync::{Arc, mpsc};
use std::thread;
use std::time::{Duration, Instant};

use betelgeuse::io_loop;
use tina::{
    Address, Context, Effect, Isolate, Outbound as TinaOutbound, Shard, ShardId,
    TrySendError as TinaTrySendError,
};
use tina_supervisor::SupervisorConfig;

use crate::call::{CallOutcome, IntoErasedCall, RuntimeCall, call};
use crate::capabilities::RuntimeCapabilities;
use crate::clock::MonotonicClock;
use crate::driver::BetelgeuseDriver;
use crate::errors::{
    ShutdownWaitError, ThreadedRegisterBootstrapError, ThreadedRuntimeError, ThreadedTrySendError,
};
use crate::live_report::{
    LiveQueueMetrics, LiveRemoteQueueReport, LiveShardMetrics, LiveShardState, LiveTopologyReport,
};
use crate::local_system::{LocalSystemTerminalReport, ThreadedWorkerExit, TraceSnapshot};
use crate::mailbox::MailboxFactory;
use crate::observation;
use crate::observer::TraceObserver;
use crate::sharded::ReplyAdapter;
use crate::shutdown::{SharedShutdownState, ShutdownWorker, ThreadedShutdownHandle, handle_for};
use crate::threaded::{ThreadedCommand, ThreadedRuntimeConfig, deliver_shutdown_signal_and_drain};
use crate::trace::{RuntimeEvent, SendRejectedReason};
use crate::{
    ChildLifecycleReport, IdSource, IntoErasedSpawn, IntoErasedSpawnObserved,
    IntoSendErasedSpawnObserved, QueuedRemoteEnvelope, Runtime, SendableQueuedRemoteEnvelope,
};

/// One live worker-per-shard runtime over a fixed shard set.
///
/// This is the Betelgeuse live multi-shard substrate. It keeps each shard
/// runtime owned by one OS thread, routes cross-shard effects through bounded
/// worker queues, and preserves the explicit-step runtime/simulator as the
/// semantic oracle. Live cross-shard payloads must be `Send` because they move
/// between worker threads.
pub struct ThreadedMultiShardRuntime<S, F>
where
    S: Shard + Send + 'static,
    F: MailboxFactory + Send + Clone + 'static,
{
    commands: BTreeMap<ShardId, std::sync::mpsc::SyncSender<ThreadedCommand<S, F>>>,
    shard_metrics: BTreeMap<ShardId, Arc<LiveShardMetrics>>,
    remote_metrics: BTreeMap<(ShardId, ShardId), Arc<LiveQueueMetrics>>,
    shutdown: Arc<SharedShutdownState<S, F>>,
}

struct ThreadedRemoteWiring {
    senders:
        BTreeMap<(ShardId, ShardId), std::sync::mpsc::SyncSender<SendableQueuedRemoteEnvelope>>,
    terminal_senders:
        BTreeMap<(ShardId, ShardId), std::sync::mpsc::SyncSender<SendableQueuedRemoteEnvelope>>,
    receivers: Vec<(
        ShardId,
        std::sync::mpsc::Receiver<SendableQueuedRemoteEnvelope>,
    )>,
    terminal_receivers: Vec<(
        ShardId,
        std::sync::mpsc::Receiver<SendableQueuedRemoteEnvelope>,
    )>,
    queue_metrics: BTreeMap<(ShardId, ShardId), Arc<LiveQueueMetrics>>,
    shard_metrics: BTreeMap<ShardId, Arc<LiveShardMetrics>>,
}

impl<S, F> ThreadedMultiShardRuntime<S, F>
where
    S: Shard + Send + 'static,
    F: MailboxFactory + Send + Clone + 'static,
{
    /// Starts one live worker thread per shard.
    pub fn new<I>(shards: I, mailbox_factory: F) -> Self
    where
        I: IntoIterator<Item = S>,
    {
        Self::with_config(shards, mailbox_factory, ThreadedRuntimeConfig::default())
    }

    /// Starts one live worker thread per shard with explicit queue config.
    pub fn with_config<I>(shards: I, mailbox_factory: F, config: ThreadedRuntimeConfig) -> Self
    where
        I: IntoIterator<Item = S>,
    {
        Self::with_config_and_optional_trace_observer(shards, mailbox_factory, config, None)
    }

    /// Like [`Self::with_config`] but wires one trace observer on
    /// every shard before the first event records. Observer stays out
    /// of [`ThreadedRuntimeConfig`] — config is pure data. Per-shard
    /// order preserved; events across shards interleave freely.
    pub fn with_config_and_trace_observer<I>(
        shards: I,
        mailbox_factory: F,
        config: ThreadedRuntimeConfig,
        observer: Arc<dyn TraceObserver>,
    ) -> Self
    where
        I: IntoIterator<Item = S>,
    {
        Self::with_config_and_optional_trace_observer(
            shards,
            mailbox_factory,
            config,
            Some(observer),
        )
    }

    fn with_config_and_optional_trace_observer<I>(
        shards: I,
        mailbox_factory: F,
        config: ThreadedRuntimeConfig,
        observer: Option<Arc<dyn TraceObserver>>,
    ) -> Self
    where
        I: IntoIterator<Item = S>,
    {
        if config.command_capacity == 0 {
            panic!("ThreadedMultiShardRuntime requires command capacity > 0");
        }
        if config.storage_lane_capacity == 0 {
            panic!("ThreadedMultiShardRuntime requires storage lane capacity > 0");
        }
        if config.dns_lane_capacity == 0 {
            panic!("ThreadedMultiShardRuntime requires DNS lane capacity > 0");
        }
        if config.tls_lane_capacity == 0 {
            panic!("ThreadedMultiShardRuntime requires TLS lane capacity > 0");
        }
        if config.process_lane_capacity == 0 {
            panic!("ThreadedMultiShardRuntime requires process lane capacity > 0");
        }
        if config.signal_capacity == 0 {
            panic!("ThreadedMultiShardRuntime requires signal capacity > 0");
        }
        if config.shard_pair_capacity == 0 {
            panic!("ThreadedMultiShardRuntime requires shard-pair capacity > 0");
        }
        if config.remote_inbound_drain_budget == 0 {
            panic!("ThreadedMultiShardRuntime requires remote inbound drain budget > 0");
        }

        let mut shards: Vec<S> = shards.into_iter().collect();
        if shards.is_empty() {
            panic!("ThreadedMultiShardRuntime requires at least one shard");
        }
        shards.sort_by_key(Shard::id);
        for pair in shards.windows(2) {
            if pair[0].id() == pair[1].id() {
                panic!(
                    "ThreadedMultiShardRuntime received duplicate shard id {}",
                    pair[0].id().get()
                );
            }
        }

        let mut commands = BTreeMap::new();
        let mut shard_metrics = BTreeMap::new();
        let mut receivers = Vec::with_capacity(shards.len());
        for (ordinal, shard) in shards.iter().enumerate() {
            let worker_config = ThreadedRuntimeConfig {
                configured_core: config.configured_core.map(|core| core + ordinal),
                ..config
            };
            let (sender, receiver) = std::sync::mpsc::sync_channel(config.command_capacity);
            commands.insert(shard.id(), sender);
            shard_metrics.insert(
                shard.id(),
                Arc::new(LiveShardMetrics::new(
                    shard.id(),
                    Some(format!("tina-shard-{}", shard.id().get())),
                    worker_config,
                )),
            );
            receivers.push((shard.id(), receiver));
        }
        let mut remote_metrics = BTreeMap::new();
        let mut remote_senders = BTreeMap::new();
        let mut terminal_remote_senders = BTreeMap::new();
        let mut remote_receivers: BTreeMap<
            ShardId,
            Vec<(
                ShardId,
                std::sync::mpsc::Receiver<SendableQueuedRemoteEnvelope>,
            )>,
        > = BTreeMap::new();
        let mut terminal_remote_receivers: BTreeMap<
            ShardId,
            Vec<(
                ShardId,
                std::sync::mpsc::Receiver<SendableQueuedRemoteEnvelope>,
            )>,
        > = BTreeMap::new();
        for source in &shards {
            for target in &shards {
                if source.id() != target.id() {
                    let (sender, receiver) =
                        std::sync::mpsc::sync_channel(config.shard_pair_capacity);
                    let (terminal_sender, terminal_receiver) =
                        std::sync::mpsc::sync_channel(config.shard_pair_capacity);
                    remote_senders.insert((source.id(), target.id()), sender);
                    terminal_remote_senders.insert((source.id(), target.id()), terminal_sender);
                    remote_receivers
                        .entry(target.id())
                        .or_default()
                        .push((source.id(), receiver));
                    terminal_remote_receivers
                        .entry(target.id())
                        .or_default()
                        .push((source.id(), terminal_receiver));
                    remote_metrics.insert(
                        (source.id(), target.id()),
                        Arc::new(LiveQueueMetrics::new(config.shard_pair_capacity)),
                    );
                }
            }
        }

        let ids = IdSource::new();
        let mut handles = Vec::with_capacity(shards.len());
        for (ordinal, (shard, (_shard_id, receiver))) in
            shards.into_iter().zip(receivers).enumerate()
        {
            let worker_config = ThreadedRuntimeConfig {
                configured_core: config.configured_core.map(|core| core + ordinal),
                ..config
            };
            let factory = mailbox_factory.clone();
            let ids = ids.clone();
            let shard_id = shard.id();
            let remote_wiring = ThreadedRemoteWiring {
                senders: remote_senders.clone(),
                terminal_senders: terminal_remote_senders.clone(),
                receivers: remote_receivers.remove(&shard_id).unwrap_or_default(),
                terminal_receivers: terminal_remote_receivers
                    .remove(&shard_id)
                    .unwrap_or_default(),
                queue_metrics: remote_metrics.clone(),
                shard_metrics: shard_metrics.clone(),
            };
            let shard_metrics_for_worker = Arc::clone(
                shard_metrics
                    .get(&shard_id)
                    .expect("shard metrics exist for worker"),
            );
            let worker_observer = observer.clone();
            handles.push((
                shard_id,
                thread::Builder::new()
                    .name(format!("tina-shard-{}", shard_id.get()))
                    .spawn(move || {
                        let io_loop = io_loop(Global).expect(
                            "failed to initialise Betelgeuse IO loop for tina-runtime shard",
                        );
                        let runtime = Runtime::with_clock_and_ids_and_driver_and_preallocation(
                            shard,
                            factory,
                            Box::new(MonotonicClock),
                            ids,
                            Box::new(BetelgeuseDriver::with_io_loop_and_capacities(
                                io_loop,
                                worker_config.storage_lane_capacity,
                                worker_config.dns_lane_capacity,
                                worker_config.tls_lane_capacity,
                                worker_config.process_lane_capacity,
                                worker_config.signal_capacity,
                            )),
                            worker_config.preallocation,
                        );
                        let mut runtime = runtime;
                        runtime.set_trace_retention(worker_config.trace_retention);
                        runtime.set_trace_observer(worker_observer);
                        threaded_worker_loop_with_remote(
                            runtime,
                            receiver,
                            worker_config,
                            remote_wiring,
                            shard_metrics_for_worker,
                        )
                    })
                    .expect("failed to spawn Tina threaded shard worker"),
            ));
        }

        let workers: Vec<ShutdownWorker<S, F>> = handles
            .into_iter()
            .map(|(shard_id, handle)| {
                let metrics = Arc::clone(
                    shard_metrics
                        .get(&shard_id)
                        .expect("shard metrics exist for worker"),
                );
                let commands_sender = commands
                    .get(&shard_id)
                    .expect("commands sender exists for worker")
                    .clone();
                ShutdownWorker {
                    shard: shard_id,
                    commands: commands_sender,
                    handle: Some(handle),
                    metrics,
                    signaled: false,
                }
            })
            .collect();
        let shutdown = Arc::new(SharedShutdownState::multi_shard(
            workers,
            remote_metrics.clone(),
        ));

        Self {
            commands,
            shard_metrics,
            remote_metrics,
            shutdown,
        }
    }

    /// Registers one root isolate on a chosen shard.
    #[allow(private_bounds)]
    pub fn register_with_capacity_on<I, Outbound>(
        &self,
        shard: ShardId,
        isolate: I,
        mailbox_capacity: usize,
    ) -> Result<Address<I::Message, I::Reply>, ThreadedRuntimeError>
    where
        I: Isolate<Shard = S, Send = TinaOutbound<Outbound>> + Send + 'static,
        I::Message: 'static,
        I::Reply: Send + 'static,
        I::Spawn: IntoErasedSpawn<S, F> + 'static,
        I::SpawnObserved: IntoErasedSpawnObserved<S, F, I::Message> + 'static,
        I::SpawnObservedRemote: IntoSendErasedSpawnObserved<S, F, I::Message> + 'static,
        I::Call: IntoErasedCall<I::Message> + 'static,
        I::Fact: crate::fact::IntoRuntimeFact + 'static,
        Outbound: Send + 'static,
    {
        self.call_on(shard, move |runtime| {
            runtime.register_sendable_with_capacity::<I, Outbound>(isolate, mailbox_capacity)
        })
    }

    /// Threaded multi-shard mirror of
    /// [`Runtime::register_with_capacity_and_bootstrap`].
    #[allow(private_bounds, clippy::type_complexity)]
    pub fn register_with_capacity_and_bootstrap_on<I, Outbound>(
        &self,
        shard: ShardId,
        isolate: I,
        mailbox_capacity: usize,
        bootstrap: I::Message,
    ) -> Result<Address<I::Message, I::Reply>, ThreadedRegisterBootstrapError<I::Message>>
    where
        I: Isolate<Shard = S, Send = TinaOutbound<Outbound>> + Send + 'static,
        I::Message: Send + 'static,
        I::Reply: Send + 'static,
        I::Spawn: IntoErasedSpawn<S, F> + 'static,
        I::SpawnObserved: IntoErasedSpawnObserved<S, F, I::Message> + 'static,
        I::SpawnObservedRemote: IntoSendErasedSpawnObserved<S, F, I::Message> + 'static,
        I::Call: IntoErasedCall<I::Message> + 'static,
        I::Fact: crate::fact::IntoRuntimeFact + 'static,
        Outbound: Send + 'static,
    {
        match self.call_on(shard, move |runtime| {
            runtime.register_sendable_with_capacity_and_bootstrap::<I, Outbound>(
                isolate,
                mailbox_capacity,
                bootstrap,
            )
        }) {
            Ok(Ok(address)) => Ok(address),
            Ok(Err(err)) => Err(ThreadedRegisterBootstrapError::from_register(err)),
            Err(ThreadedRuntimeError::WorkerStopped) => {
                Err(ThreadedRegisterBootstrapError::WorkerStopped)
            }
            Err(ThreadedRuntimeError::UnknownShard(s)) => {
                Err(ThreadedRegisterBootstrapError::UnknownShard(s))
            }
            Err(ThreadedRuntimeError::DriverShutdownFailed)
            | Err(ThreadedRuntimeError::CommandFull)
            | Err(ThreadedRuntimeError::HostWaitTimeout) => {
                // `call_on` is blocking-admission, so `CommandFull` is
                // unreachable today. Map defensively in case the inner
                // helper is ever migrated.
                Err(ThreadedRegisterBootstrapError::WorkerStopped)
            }
        }
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
    /// `M: 'static` is sufficient for registration. Sending to the
    /// returned address through `try_send` requires `M: Send` (the
    /// runtime's send surface enforces it independently).
    ///
    /// Mirrors [`crate::MultiShardRuntime::register_reply_adapter_on`]
    /// (explicit-step) and `MultiShardSimulator::register_reply_adapter_on`
    /// (in `tina-sim`). Bound lists are matched to each runtime's
    /// lower-level `register_with_capacity_on`; mirror changes across
    /// all three.
    #[allow(private_bounds)]
    pub fn register_reply_adapter_on<M, T>(
        &self,
        shard: ShardId,
        target: Address<T>,
        mailbox_capacity: usize,
    ) -> Result<Address<M>, ThreadedRuntimeError>
    where
        M: 'static,
        T: Send + 'static + From<M>,
        std::convert::Infallible: IntoErasedSpawn<S, F>,
        RuntimeCall<M>: IntoErasedCall<M>,
    {
        self.register_with_capacity_on::<ReplyAdapter<M, T, S>, T>(
            shard,
            ReplyAdapter::new(target),
            mailbox_capacity,
        )
    }

    /// Configures a registered root isolate as supervisor on its owning shard.
    pub fn supervise<M: 'static, R: 'static>(
        &self,
        parent: Address<M, R>,
        config: SupervisorConfig,
    ) -> Result<(), ThreadedRuntimeError> {
        self.call_on(parent.shard(), move |runtime| {
            runtime.supervise(parent, config)
        })
    }

    /// Returns a live child lifecycle report from the parent shard.
    pub fn child_lifecycle_report<M: 'static, R: 'static>(
        &self,
        parent: Address<M, R>,
    ) -> Result<ChildLifecycleReport, ThreadedRuntimeError> {
        self.call_on(parent.shard(), move |runtime| {
            runtime.child_lifecycle_report(parent)
        })
        .and_then(|report| report.map_err(|_| ThreadedRuntimeError::WorkerStopped))
    }

    /// Registers a typed waiter for the next child restart reported on the
    /// parent's owning shard. Remote child restarts resolve through the owner
    /// shard with the replacement address fields.
    pub fn observe_child_restarted<M: 'static, R: 'static>(
        &self,
        parent: Address<M, R>,
    ) -> Result<observation::ChildRestartedWaiter, ThreadedRuntimeError> {
        self.call_on(parent.shard(), move |runtime| {
            runtime.observe_child_restarted(parent)
        })
    }

    /// Attempts bounded ingress to the worker that owns `address`.
    pub fn try_send<M: Send + 'static, R: 'static>(
        &self,
        address: Address<M, R>,
        message: M,
    ) -> Result<(), ThreadedTrySendError> {
        let Some(sender) = self.commands.get(&address.shard()) else {
            panic!(
                "ThreadedMultiShardRuntime targeted unknown shard {}",
                address.shard().get()
            );
        };
        // Reject ingress to a quarantined shard
        // immediately, before the bounded sync_channel has observed
        // Disconnected. Cross-shard senders should not race with the
        // worker's natural exit window.
        if let Some(metrics) = self.shard_metrics.get(&address.shard()) {
            if metrics.state() == LiveShardState::Failed {
                metrics.ingress.rejected_closed();
                return Err(ThreadedTrySendError::WorkerStopped);
            }
        }
        let command = ThreadedCommand::Run(Box::new(move |runtime| {
            let _ = runtime.try_send(address, message);
        }));
        match sender.try_send(command) {
            Ok(()) => {
                if let Some(metrics) = self.shard_metrics.get(&address.shard()) {
                    metrics.ingress.accepted();
                }
                Ok(())
            }
            Err(std::sync::mpsc::TrySendError::Full(_)) => {
                if let Some(metrics) = self.shard_metrics.get(&address.shard()) {
                    metrics.ingress.rejected_full();
                }
                Err(ThreadedTrySendError::IngressFull)
            }
            Err(std::sync::mpsc::TrySendError::Disconnected(_)) => {
                if let Some(metrics) = self.shard_metrics.get(&address.shard()) {
                    metrics.ingress.rejected_closed();
                    metrics.set_state(LiveShardState::Failed);
                }
                Err(ThreadedTrySendError::WorkerStopped)
            }
        }
    }

    /// Registers a typed result waiter for the isolate at `address` on the
    /// shard that owns it.
    ///
    /// Routes the registration to the address's owning shard worker,
    /// matching the way [`Self::try_send`] routes ingress. Same vocabulary
    /// as [`crate::ThreadedRuntime::observe_result`]:
    ///
    /// - eager errors: `AlreadyStopped`, `AlreadyClaimed`, `ObservationFull`;
    /// - `wait` outcomes: `Timeout`, `RuntimeStopped`, `StoppedWithoutResult`,
    ///   `TypeMismatch`.
    ///
    /// Worker stopped -> `RuntimeStopped`.
    ///
    /// # Panics
    ///
    /// Panics if `address.shard()` is not owned by this runtime — same
    /// convention as [`Self::try_send`]. Passing an address from a
    /// different runtime is a programmer error, not a runtime fault.
    pub fn observe_result<T: Send + 'static, M: 'static, R: 'static>(
        &self,
        address: Address<M, R>,
    ) -> Result<observation::IsolateResultWaiter<T>, observation::ResultWaitError> {
        if !self.commands.contains_key(&address.shard()) {
            panic!(
                "ThreadedMultiShardRuntime::observe_result targeted unknown shard {}",
                address.shard().get()
            );
        }
        match self.call_on(address.shard(), move |runtime| {
            runtime.observe_result::<T, M, R>(address)
        }) {
            Ok(result) => result,
            Err(_) => Err(observation::ResultWaitError::RuntimeStopped),
        }
    }

    /// Returns retained trace from shards still able to report.
    pub fn trace(&self) -> TraceSnapshot {
        let mut events = Vec::new();
        let mut missing_shards = Vec::new();
        for shard in self.commands.keys() {
            match self.call_on(*shard, |runtime| runtime.trace().to_vec()) {
                Ok(trace) => events.extend(trace),
                Err(_) => missing_shards.push(*shard),
            }
        }
        events.sort_by_key(|event| event.id());
        TraceSnapshot::partial(events, missing_shards)
    }

    /// Returns complete trace, failing if any shard can no longer report.
    pub fn complete_trace(&self) -> Result<Vec<RuntimeEvent>, ThreadedRuntimeError> {
        let mut events = Vec::new();
        for shard in self.commands.keys() {
            events.extend(self.call_on(*shard, |runtime| runtime.trace().to_vec())?);
        }
        events.sort_by_key(|event| event.id());
        Ok(events)
    }

    /// Returns a trace snapshot from one worker shard.
    pub fn trace_on(&self, shard: ShardId) -> Result<Vec<RuntimeEvent>, ThreadedRuntimeError> {
        self.call_on(shard, |runtime| runtime.trace().to_vec())
    }

    /// Returns a handle-owned topology snapshot without probing workers.
    pub fn topology(&self) -> LiveTopologyReport {
        let shards = self
            .shard_metrics
            .values()
            .map(|metrics| metrics.report())
            .collect();
        let remote_queues = self
            .remote_metrics
            .iter()
            .map(|(&(source, target), metrics)| LiveRemoteQueueReport {
                source,
                target,
                queue: metrics.report(),
            })
            .collect();
        LiveTopologyReport::new(shards, remote_queues)
    }

    /// Returns the live runtime capability table shared by each worker.
    pub fn capabilities(&self) -> RuntimeCapabilities {
        let config = self
            .shard_metrics
            .values()
            .next()
            .map(|metrics| metrics.config)
            .unwrap_or_default();
        RuntimeCapabilities::threaded_with_capacities(
            config.storage_lane_capacity,
            config.dns_lane_capacity,
            config.tls_lane_capacity,
            config.process_lane_capacity,
            config.signal_capacity,
        )
    }

    /// Requests shutdown and joins every worker.
    pub fn shutdown(self) -> Result<Vec<RuntimeEvent>, ThreadedRuntimeError> {
        let report = self.shutdown_report();
        if let Some(error) = report.error() {
            Err(error)
        } else {
            Ok(report.into_trace())
        }
    }

    /// Requests shutdown and joins every worker, always returning terminal truth.
    ///
    /// Routes through the same shared shutdown state as
    /// [`Self::shutdown_handle`] and `Drop`; a handle that already
    /// requested shutdown or already waited the terminal report sees the
    /// same cached report when this consuming form is called next.
    pub fn shutdown_report(self) -> LocalSystemTerminalReport {
        let shared = Arc::clone(&self.shutdown);
        drop(self);
        shared.wait_report_blocking()
    }

    /// Requests shutdown and waits up to `timeout` for terminal truth.
    ///
    /// This is the explicit bounded form for hosts that cannot risk an
    /// unbounded join. A timeout returns [`ShutdownWaitError::Timeout`]
    /// while the background joiner may continue trying to finish.
    pub fn shutdown_with_timeout(
        self,
        timeout: Duration,
    ) -> Result<LocalSystemTerminalReport, ShutdownWaitError> {
        let shared = Arc::clone(&self.shutdown);
        drop(self);
        shared.wait_report_for_owner_with_timeout(timeout)
    }

    /// Returns a cloneable handle that controls runtime-level shutdown
    /// without consuming the runtime value.
    ///
    /// See [`crate::ThreadedRuntime::shutdown_handle`] for the contract.
    /// The multi-shard handle requests shutdown on every owned shard; a
    /// single shard's full command queue surfaces in
    /// [`crate::ShutdownRequestError::CommandFull`] with the offending
    /// shard id attached.
    pub fn shutdown_handle(&self) -> ThreadedShutdownHandle {
        handle_for(&self.shutdown)
    }

    /// Performs one typed isolate call from the host thread on the shard
    /// that owns `address` and waits for its ordinary [`CallOutcome`].
    ///
    /// Same host-convenience shape as
    /// [`crate::ThreadedRuntime::call_blocking`] but routed to the shard
    /// implied by the address. The host-call driver isolate is registered
    /// on that shard through bounded admission via `try_send` on the
    /// worker command queue.
    ///
    /// # Routing
    ///
    /// The call is driven from the shard that owns `address`, exactly
    /// matching how [`Self::try_send`] and [`Self::observe_result`] route
    /// by `address.shard()`. There is no explicit `*_on` variant in this
    /// phase: a future host-to-shard variant is only worth shipping with
    /// a real caller and a cross-shard remote-path proof.
    ///
    /// # Panics
    ///
    /// Panics if `address.shard()` is not owned by this runtime — same
    /// programmer-error convention as [`Self::try_send`] and
    /// [`Self::observe_result`].
    ///
    /// # Errors
    ///
    /// - [`ThreadedRuntimeError::CommandFull`] — the bounded worker
    ///   command queue could not accept the host-control admission
    ///   command immediately.
    /// - [`ThreadedRuntimeError::WorkerStopped`] — the target shard
    ///   worker is gone before the host call could be driven.
    pub fn call_blocking<M, R>(
        &self,
        address: Address<M, R>,
        message: M,
        timeout: Duration,
    ) -> Result<CallOutcome<R>, ThreadedRuntimeError>
    where
        M: Send + 'static,
        R: Send + 'static,
    {
        let host_wait_timeout = timeout
            .checked_add(crate::threaded::DEFAULT_HOST_CALL_DELIVERY_GRACE)
            .unwrap_or(timeout);
        self.call_blocking_with_host_timeout(address, message, timeout, host_wait_timeout)
    }

    /// Like [`call_blocking`](Self::call_blocking), but separates the target
    /// call deadline from the host-side wait budget.
    pub fn call_blocking_with_host_timeout<M, R>(
        &self,
        address: Address<M, R>,
        message: M,
        target_timeout: Duration,
        host_wait_timeout: Duration,
    ) -> Result<CallOutcome<R>, ThreadedRuntimeError>
    where
        M: Send + 'static,
        R: Send + 'static,
    {
        let shard = address.shard();
        if !self.commands.contains_key(&shard) {
            panic!(
                "ThreadedMultiShardRuntime::call_blocking targeted unknown shard {}",
                shard.get()
            );
        }
        let (reply_tx, reply_rx) = mpsc::channel();
        let driver = HostCallDriverMS::<S, M, R> {
            sender: reply_tx,
            _marker: PhantomData,
        };
        let Some(sender) = self.commands.get(&shard) else {
            panic!(
                "ThreadedMultiShardRuntime::call_blocking targeted unknown shard {}",
                shard.get()
            );
        };
        let command = ThreadedCommand::Run(Box::new(move |runtime| {
            let driver_addr = runtime
                .register_sendable_with_capacity::<HostCallDriverMS<S, M, R>, Infallible>(
                    driver, 2,
                );
            match runtime.try_send(
                driver_addr,
                HostCallMsg::Begin {
                    target: address,
                    message,
                    timeout: target_timeout,
                },
            ) {
                Ok(()) => {}
                Err(TinaTrySendError::Full(_)) => {
                    panic!("fresh host-call driver mailbox was unexpectedly full");
                }
                Err(TinaTrySendError::Closed(_)) => {
                    panic!("fresh host-call driver mailbox was unexpectedly closed");
                }
            }
        }));
        match sender.try_send(command) {
            Ok(()) => {}
            Err(std::sync::mpsc::TrySendError::Full(_)) => {
                return Err(ThreadedRuntimeError::CommandFull);
            }
            Err(std::sync::mpsc::TrySendError::Disconnected(_)) => {
                if let Some(metrics) = self.shard_metrics.get(&shard) {
                    metrics.set_state(LiveShardState::Failed);
                }
                return Err(ThreadedRuntimeError::WorkerStopped);
            }
        }

        match reply_rx.recv_timeout(host_wait_timeout) {
            Ok(outcome) => Ok(outcome),
            Err(mpsc::RecvTimeoutError::Timeout) => Err(ThreadedRuntimeError::HostWaitTimeout),
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                if let Some(metrics) = self.shard_metrics.get(&shard) {
                    metrics.set_state(LiveShardState::Failed);
                }
                Err(ThreadedRuntimeError::WorkerStopped)
            }
        }
    }

    fn call_on<R, C>(&self, shard: ShardId, command: C) -> Result<R, ThreadedRuntimeError>
    where
        R: Send + 'static,
        C: FnOnce(&mut Runtime<S, F>) -> R + Send + 'static,
    {
        let Some(sender) = self.commands.get(&shard) else {
            return Err(ThreadedRuntimeError::UnknownShard(shard));
        };
        let (reply_tx, reply_rx) = std::sync::mpsc::channel();
        sender
            .send(ThreadedCommand::Run(Box::new(move |runtime| {
                let _ = reply_tx.send(command(runtime));
            })))
            .map_err(|_| {
                if let Some(metrics) = self.shard_metrics.get(&shard) {
                    metrics.set_state(LiveShardState::Failed);
                }
                ThreadedRuntimeError::WorkerStopped
            })?;
        reply_rx.recv().map_err(|_| {
            if let Some(metrics) = self.shard_metrics.get(&shard) {
                metrics.set_state(LiveShardState::Failed);
            }
            ThreadedRuntimeError::WorkerStopped
        })
    }
}

impl<S, F> Drop for ThreadedMultiShardRuntime<S, F>
where
    S: Shard + Send + 'static,
    F: MailboxFactory + Send + Clone + 'static,
{
    fn drop(&mut self) {
        self.shutdown.shutdown_blocking();
        let _ = self.shutdown.wait_report_for_owner_with_timeout(
            crate::threaded::DEFAULT_SHUTDOWN_LANE_DRAIN_TIMEOUT,
        );
    }
}

// ---------- Multi-shard host-call driver isolate ----------

enum HostCallMsg<M, R> {
    Begin {
        target: Address<M, R>,
        message: M,
        timeout: Duration,
    },
    Returned(CallOutcome<R>),
}

struct HostCallDriverMS<S, M, R>
where
    S: Shard + 'static,
{
    sender: mpsc::Sender<CallOutcome<R>>,
    _marker: PhantomData<(S, M)>,
}

impl<S, M, R> Isolate for HostCallDriverMS<S, M, R>
where
    S: Shard + 'static,
    M: Send + 'static,
    R: Send + 'static,
{
    tina::isolate_types! {
        message: HostCallMsg<M, R>,
        reply: (),
        send: TinaOutbound<Infallible>,
        spawn: Infallible,
        call: RuntimeCall<HostCallMsg<M, R>>,
        shard: S,
    }

    fn handle(
        &mut self,
        msg: HostCallMsg<M, R>,
        _ctx: &mut Context<'_, S, Self::Reply>,
    ) -> Effect<Self> {
        match msg {
            HostCallMsg::Begin {
                target,
                message,
                timeout,
            } => call(target, message, timeout).then(HostCallMsg::Returned),
            HostCallMsg::Returned(outcome) => {
                let _ = self.sender.send(outcome);
                tina::stop()
            }
        }
    }
}

fn threaded_worker_loop_with_remote<S, F>(
    mut runtime: Runtime<S, F>,
    receiver: std::sync::mpsc::Receiver<ThreadedCommand<S, F>>,
    config: ThreadedRuntimeConfig,
    remote_wiring: ThreadedRemoteWiring,
    shard_metrics: Arc<LiveShardMetrics>,
) -> ThreadedWorkerExit
where
    S: Shard + 'static,
    F: MailboxFactory + 'static,
{
    runtime.remote_child_control_capacity = config.shard_pair_capacity;
    // Pin this shard worker (if requested and the platform can). The driver's
    // helper lanes were already spawned when `runtime` was built above, so they
    // inherit the unpinned mask; later per-op helper threads float off the pin.
    // Pin before recording the thread id so a report that names the worker
    // carries its proven pin outcome.
    let affinity = crate::affinity::apply(config.configured_core);
    shard_metrics.publish_worker_start(format!("{:?}", thread::current().id()), affinity);
    let source_shard = runtime.shard().id();
    let mut terminal_overflow = VecDeque::new();
    let mut terminal_remote_drain_start = 0;
    let mut ordinary_remote_drain_start = 0;
    // Terminal replies and ordinary sends are separate inbound classes.
    // Rotate sources within each class, and alternate which class gets first
    // chance so neither ordinary sends nor terminal replies can consume every
    // bounded drain pass forever.
    let mut drain_terminal_first = true;
    loop {
        shard_metrics.set_resource_counts(runtime.resource_report());
        let route_remote_lossless =
            |envelope: QueuedRemoteEnvelope| -> Result<(), Box<RemoteRouteFailure>> {
                let target_shard = envelope.target_shard();
                let terminal = matches!(
                    envelope,
                    QueuedRemoteEnvelope::CallReply(_)
                        | QueuedRemoteEnvelope::SpawnReply(_)
                        | QueuedRemoteEnvelope::ChildStopped(_)
                        | QueuedRemoteEnvelope::ChildRestarted(_)
                );
                let metrics = remote_wiring
                    .queue_metrics
                    .get(&(source_shard, target_shard));
                if remote_wiring
                    .shard_metrics
                    .get(&target_shard)
                    .is_some_and(|metrics| metrics.state() == LiveShardState::Failed)
                {
                    if let Some(metrics) = metrics {
                        metrics.rejected_closed();
                    }
                    return Err(Box::new(RemoteRouteFailure {
                        reason: SendRejectedReason::Closed,
                        envelope,
                    }));
                }
                let senders = if terminal {
                    &remote_wiring.terminal_senders
                } else {
                    &remote_wiring.senders
                };
                let Some(sender) = senders.get(&(source_shard, target_shard)) else {
                    panic!(
                        "ThreadedMultiShardRuntime targeted unknown destination shard {}",
                        target_shard.get()
                    );
                };
                let envelope = SendableQueuedRemoteEnvelope::new(envelope);
                match sender.try_send(envelope) {
                    Ok(()) => {
                        if let Some(metrics) = metrics {
                            metrics.accepted();
                        }
                        Ok(())
                    }
                    Err(std::sync::mpsc::TrySendError::Full(envelope)) => {
                        if let Some(metrics) = metrics {
                            metrics.rejected_full();
                        }
                        Err(Box::new(RemoteRouteFailure {
                            reason: SendRejectedReason::Full,
                            envelope: envelope.into_queued_remote_envelope(),
                        }))
                    }
                    Err(std::sync::mpsc::TrySendError::Disconnected(envelope)) => {
                        if let Some(metrics) = metrics {
                            metrics.rejected_closed();
                        }
                        Err(Box::new(RemoteRouteFailure {
                            reason: SendRejectedReason::Closed,
                            envelope: envelope.into_queued_remote_envelope(),
                        }))
                    }
                }
            };
        let overflow_delivered =
            drain_terminal_overflow(&mut terminal_overflow, &route_remote_lossless);
        let mut route_remote = |envelope: QueuedRemoteEnvelope| -> Result<(), SendRejectedReason> {
            route_remote_preserving_terminal(
                envelope,
                &mut terminal_overflow,
                &route_remote_lossless,
            )
        };
        let remote_delivered = overflow_delivered
            + if drain_terminal_first {
                let terminal_delivered = drain_remote_inbound(
                    &mut runtime,
                    &remote_wiring.terminal_receivers,
                    &mut route_remote,
                    config.remote_inbound_drain_budget,
                    &mut terminal_remote_drain_start,
                );
                let ordinary_budget = config
                    .remote_inbound_drain_budget
                    .saturating_sub(terminal_delivered);
                terminal_delivered
                    + drain_remote_inbound(
                        &mut runtime,
                        &remote_wiring.receivers,
                        &mut route_remote,
                        ordinary_budget,
                        &mut ordinary_remote_drain_start,
                    )
            } else {
                let ordinary_delivered = drain_remote_inbound(
                    &mut runtime,
                    &remote_wiring.receivers,
                    &mut route_remote,
                    config.remote_inbound_drain_budget,
                    &mut ordinary_remote_drain_start,
                );
                let terminal_budget = config
                    .remote_inbound_drain_budget
                    .saturating_sub(ordinary_delivered);
                ordinary_delivered
                    + drain_remote_inbound(
                        &mut runtime,
                        &remote_wiring.terminal_receivers,
                        &mut route_remote,
                        terminal_budget,
                        &mut terminal_remote_drain_start,
                    )
            };
        drain_terminal_first = !drain_terminal_first;
        // Fairness: poll the local command queue after every bounded
        // remote-drain pass, not only when the drain delivered zero
        // envelopes. A sustained remote inbound flood keeps
        // `remote_delivered > 0` indefinitely; without this check,
        // `Run` and `Shutdown` never get read.
        match receiver.try_recv() {
            Ok(ThreadedCommand::Run(command)) => {
                command(&mut runtime);
                continue;
            }
            Ok(ThreadedCommand::Shutdown) => {
                deliver_shutdown_signal_and_drain(&mut runtime);
                break;
            }
            Err(std::sync::mpsc::TryRecvError::Disconnected) => break,
            Err(std::sync::mpsc::TryRecvError::Empty) => {}
        }

        let delivered = runtime.step_with_remote(&mut |_, envelope| route_remote(envelope));

        if delivered > 0 || remote_delivered > 0 || !terminal_overflow.is_empty() {
            continue;
        }

        if !runtime.has_in_flight_calls() {
            match receiver.recv_timeout(config.idle_wait) {
                Ok(ThreadedCommand::Run(command)) => command(&mut runtime),
                Ok(ThreadedCommand::Shutdown) => {
                    deliver_shutdown_signal_and_drain(&mut runtime);
                    break;
                }
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
            }
        } else {
            thread::yield_now();
        }
    }

    let shutdown_deadline = Instant::now() + config.shutdown_lane_drain_timeout;
    let shutdown_result = runtime.cancel_in_flight_calls_for_shutdown(shutdown_deadline);
    shard_metrics.set_resource_counts(runtime.resource_report());
    let trace = runtime.trace().to_vec();
    if shutdown_result.is_err() {
        return ThreadedWorkerExit::failed(ThreadedRuntimeError::DriverShutdownFailed, trace);
    }
    ThreadedWorkerExit::clean(trace)
}

struct RemoteRouteFailure {
    reason: SendRejectedReason,
    envelope: QueuedRemoteEnvelope,
}

fn route_remote_preserving_terminal(
    envelope: QueuedRemoteEnvelope,
    terminal_overflow: &mut VecDeque<QueuedRemoteEnvelope>,
    route_remote: &impl Fn(QueuedRemoteEnvelope) -> Result<(), Box<RemoteRouteFailure>>,
) -> Result<(), SendRejectedReason> {
    match route_remote(envelope) {
        Ok(()) => Ok(()),
        Err(failure)
            if failure.reason == SendRejectedReason::Full
                && matches!(
                    failure.envelope,
                    QueuedRemoteEnvelope::CallReply(_)
                        | QueuedRemoteEnvelope::SpawnReply(_)
                        | QueuedRemoteEnvelope::ChildStopped(_)
                        | QueuedRemoteEnvelope::ChildRestarted(_)
                ) =>
        {
            terminal_overflow.push_back(failure.envelope);
            Ok(())
        }
        Err(failure) => Err(failure.reason),
    }
}

fn drain_terminal_overflow(
    terminal_overflow: &mut VecDeque<QueuedRemoteEnvelope>,
    route_remote: &impl Fn(QueuedRemoteEnvelope) -> Result<(), Box<RemoteRouteFailure>>,
) -> usize {
    let mut delivered = 0;
    while let Some(envelope) = terminal_overflow.pop_front() {
        match route_remote(envelope) {
            Ok(()) => delivered += 1,
            Err(failure)
                if failure.reason == SendRejectedReason::Full
                    && matches!(
                        failure.envelope,
                        QueuedRemoteEnvelope::CallReply(_)
                            | QueuedRemoteEnvelope::SpawnReply(_)
                            | QueuedRemoteEnvelope::ChildStopped(_)
                            | QueuedRemoteEnvelope::ChildRestarted(_)
                    ) =>
            {
                terminal_overflow.push_front(failure.envelope);
                break;
            }
            Err(_) => {
                delivered += 1;
            }
        }
    }
    delivered
}

fn drain_remote_inbound<S, F>(
    runtime: &mut Runtime<S, F>,
    remote_receivers: &[(
        ShardId,
        std::sync::mpsc::Receiver<SendableQueuedRemoteEnvelope>,
    )],
    route_remote: &mut impl FnMut(QueuedRemoteEnvelope) -> Result<(), SendRejectedReason>,
    budget: usize,
    next_start: &mut usize,
) -> usize
where
    S: Shard + 'static,
    F: MailboxFactory + 'static,
{
    if budget == 0 || remote_receivers.is_empty() {
        return 0;
    }
    *next_start %= remote_receivers.len();
    let mut delivered = 0;
    let mut last_delivered_index = None;
    for offset in 0..remote_receivers.len() {
        let index = (*next_start + offset) % remote_receivers.len();
        let (_, receiver) = &remote_receivers[index];
        loop {
            if delivered >= budget {
                *next_start = (index + 1) % remote_receivers.len();
                return delivered;
            }
            match receiver.try_recv() {
                Ok(envelope) => {
                    delivered += 1;
                    last_delivered_index = Some(index);
                    if let Some(outbound) =
                        runtime.harvest_remote_envelope(envelope.into_queued_remote_envelope())
                    {
                        let _ = route_remote(outbound);
                    }
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => break,
                Err(std::sync::mpsc::TryRecvError::Disconnected) => break,
            }
        }
    }
    *next_start = last_delivered_index
        .map(|index| (index + 1) % remote_receivers.len())
        .unwrap_or((*next_start + 1) % remote_receivers.len());
    delivered
}
