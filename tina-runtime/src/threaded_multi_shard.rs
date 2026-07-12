//! Threaded multi-shard runtime extracted from lib.rs.
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
    Address, Context, Effect, Isolate, Outbound as TinaOutbound, Shard, ShardId, SystemIncarnation,
};
use tina_supervisor::SupervisorConfig;

use crate::call::{CallOutcome, IntoErasedCall, RuntimeCall, call};
use crate::capabilities::RuntimeCapabilities;
use crate::clock::MonotonicClock;
use crate::driver::BetelgeuseDriver;
use crate::errors::{
    SendObservedUntilError, ShutdownWaitError, StartupError, ThreadedRegisterBootstrapError,
    ThreadedRuntimeError, ThreadedSendObservedError, ThreadedTrySendError,
};
use crate::host_burst::HostBurstOutcomes;
use crate::live_report::{
    LiveQueueMetrics, LiveRemoteQueueReport, LiveShardMetrics, LiveShardState, LiveTopologyReport,
};
use crate::local_system::{LocalSystemTerminalReport, ThreadedWorkerExit, TraceSnapshot};
use crate::mailbox::MailboxFactory;
use crate::observation;
use crate::observer::TraceObserver;
use crate::sharded::ReplyAdapter;
use crate::shutdown::{SharedShutdownState, ShutdownWorker, ThreadedShutdownHandle, handle_for};
use crate::threaded::{
    CommandSender, DEFAULT_STARTUP_HANDSHAKE_TIMEOUT, RecoverableControlCallError,
    STARTUP_CLEANUP_JOIN_TIMEOUT, ThreadedCommand, ThreadedRuntimeConfig,
    deadline_observed_attempt, deliver_shutdown_signal_and_drain_with_remote,
    observed_send_command, panic_payload_message, run_host_call,
};
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
    commands: BTreeMap<ShardId, CommandSender<S, F>>,
    shard_metrics: BTreeMap<ShardId, Arc<LiveShardMetrics>>,
    remote_metrics: BTreeMap<(ShardId, ShardId), Arc<LiveQueueMetrics>>,
    shutdown: Arc<SharedShutdownState<S, F>>,
    /// Upper bound on a per-shard host-control `call_on` awaiting the reply.
    control_call_timeout: Duration,
    system_incarnation: SystemIncarnation,
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

type StartingWorker = (
    ShardId,
    thread::JoinHandle<ThreadedWorkerExit>,
    mpsc::Receiver<Result<(), StartupError>>,
);

fn cleanup_startup_workers<S, F>(
    commands: &BTreeMap<ShardId, CommandSender<S, F>>,
    workers: Vec<(ShardId, thread::JoinHandle<ThreadedWorkerExit>)>,
) where
    S: Shard + Send + 'static,
    F: MailboxFactory + Send + Clone + 'static,
{
    for (shard, _) in &workers {
        if let Some(commands) = commands.get(shard) {
            let _ = commands.send(ThreadedCommand::Shutdown);
        }
    }
    let join_deadline = Instant::now() + STARTUP_CLEANUP_JOIN_TIMEOUT;
    while workers.iter().any(|(_, worker)| !worker.is_finished()) && Instant::now() < join_deadline
    {
        thread::sleep(Duration::from_millis(1));
    }
    for (_, worker) in workers {
        // A timed-out startup may be stuck in arbitrary user code. Rust cannot
        // cancel that thread, so detach it; dropping all command senders makes
        // it exit if startup eventually completes.
        if worker.is_finished() {
            let _ = worker.join();
        }
    }
}

fn shard_worker_config(
    config: ThreadedRuntimeConfig,
    ordinal: usize,
) -> Result<ThreadedRuntimeConfig, StartupError> {
    let configured_core = config
        .configured_core
        .map(|base| {
            base.checked_add(ordinal)
                .ok_or(StartupError::ConfiguredCoreOverflow { base, ordinal })
        })
        .transpose()?;
    Ok(ThreadedRuntimeConfig {
        configured_core,
        ..config
    })
}

impl<S, F> ThreadedMultiShardRuntime<S, F>
where
    S: Shard + Send + 'static,
    F: MailboxFactory + Send + Clone + 'static,
{
    /// Returns the provenance shared by every owned shard worker.
    pub const fn system_incarnation(&self) -> SystemIncarnation {
        self.system_incarnation
    }

    /// Starts one live worker thread per shard.
    pub fn new<I>(shards: I, mailbox_factory: F) -> Self
    where
        I: IntoIterator<Item = S>,
    {
        Self::try_new(shards, mailbox_factory)
            .expect("failed to start Tina multi-shard threaded runtime")
    }

    /// Fallible form of [`Self::new`].
    pub fn try_new<I>(shards: I, mailbox_factory: F) -> Result<Self, StartupError>
    where
        I: IntoIterator<Item = S>,
    {
        Self::try_with_config(shards, mailbox_factory, ThreadedRuntimeConfig::default())
    }

    /// Starts one live worker thread per shard with explicit queue config.
    pub fn with_config<I>(shards: I, mailbox_factory: F, config: ThreadedRuntimeConfig) -> Self
    where
        I: IntoIterator<Item = S>,
    {
        Self::try_with_config(shards, mailbox_factory, config)
            .expect("failed to start Tina multi-shard threaded runtime")
    }

    /// Fallible form of [`Self::with_config`].
    pub fn try_with_config<I>(
        shards: I,
        mailbox_factory: F,
        config: ThreadedRuntimeConfig,
    ) -> Result<Self, StartupError>
    where
        I: IntoIterator<Item = S>,
    {
        Self::try_with_config_and_optional_trace_observer(shards, mailbox_factory, config, None)
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
        Self::try_with_config_and_trace_observer(shards, mailbox_factory, config, observer)
            .expect("failed to start Tina multi-shard threaded runtime")
    }

    /// Fallible form of [`Self::with_config_and_trace_observer`].
    pub fn try_with_config_and_trace_observer<I>(
        shards: I,
        mailbox_factory: F,
        config: ThreadedRuntimeConfig,
        observer: Arc<dyn TraceObserver>,
    ) -> Result<Self, StartupError>
    where
        I: IntoIterator<Item = S>,
    {
        Self::try_with_config_and_optional_trace_observer(
            shards,
            mailbox_factory,
            config,
            Some(observer),
        )
    }

    fn try_with_config_and_optional_trace_observer<I>(
        shards: I,
        mailbox_factory: F,
        config: ThreadedRuntimeConfig,
        observer: Option<Arc<dyn TraceObserver>>,
    ) -> Result<Self, StartupError>
    where
        I: IntoIterator<Item = S>,
    {
        config.validate()?;
        let system_incarnation = config
            .system_incarnation
            .unwrap_or_else(crate::fresh_system_incarnation);
        let config = ThreadedRuntimeConfig {
            system_incarnation: Some(system_incarnation),
            ..config
        };

        let mut shards: Vec<S> = shards.into_iter().collect();
        if shards.is_empty() {
            return Err(StartupError::NoShards);
        }
        shards.sort_by_key(Shard::id);
        for pair in shards.windows(2) {
            if pair[0].id() == pair[1].id() {
                return Err(StartupError::DuplicateShard(pair[0].id()));
            }
        }

        let mut commands = BTreeMap::new();
        let mut shard_metrics = BTreeMap::new();
        let mut receivers = Vec::with_capacity(shards.len());
        for (ordinal, shard) in shards.iter().enumerate() {
            let worker_config = shard_worker_config(config, ordinal)?;
            let (sender, receiver) = std::sync::mpsc::sync_channel(config.command_capacity);
            commands.insert(shard.id(), CommandSender::new(sender));
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
        let mut handles: Vec<StartingWorker> = Vec::with_capacity(shards.len());
        for (ordinal, (shard, (_shard_id, receiver))) in
            shards.into_iter().zip(receivers).enumerate()
        {
            let worker_config = shard_worker_config(config, ordinal)?;
            let factory = match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                mailbox_factory.clone()
            })) {
                Ok(factory) => factory,
                Err(payload) => {
                    cleanup_startup_workers(
                        &commands,
                        handles
                            .into_iter()
                            .map(|(shard, handle, _)| (shard, handle))
                            .collect(),
                    );
                    return Err(StartupError::WorkerStartupPanicked {
                        shard: shard.id(),
                        message: panic_payload_message(&payload),
                    });
                }
            };
            let ids = ids.per_shard();
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
            let (startup_tx, startup_rx) = mpsc::channel::<Result<(), StartupError>>();
            let handle = match thread::Builder::new()
                .name(format!("tina-shard-{}", shard_id.get()))
                .spawn(move || {
                    let initialized =
                        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                            let io_loop = io_loop(Global).map_err(|source| {
                                StartupError::IoLoopInitialization {
                                    shard: shard_id,
                                    source,
                                }
                            })?;
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
                                    worker_config.timer_capacity,
                                )),
                                worker_config.preallocation,
                            )
                            .with_system_incarnation(system_incarnation);
                            let mut runtime = runtime;
                            runtime.set_trace_retention(worker_config.trace_retention);
                            runtime.set_driver_completion_drain_budget(
                                worker_config.driver_completion_drain_budget,
                            );
                            runtime.set_trace_observer(worker_observer);
                            Ok::<_, StartupError>(runtime)
                        }));
                    let runtime = match initialized {
                        Ok(Ok(runtime)) => runtime,
                        Ok(Err(error)) => {
                            shard_metrics_for_worker.set_state(LiveShardState::Failed);
                            let _ = startup_tx.send(Err(error));
                            return ThreadedWorkerExit::failed(
                                ThreadedRuntimeError::WorkerStopped,
                                Vec::new(),
                            );
                        }
                        Err(payload) => {
                            shard_metrics_for_worker.set_state(LiveShardState::Failed);
                            let _ = startup_tx.send(Err(StartupError::WorkerStartupPanicked {
                                shard: shard_id,
                                message: panic_payload_message(&payload),
                            }));
                            return ThreadedWorkerExit::failed(
                                ThreadedRuntimeError::WorkerStopped,
                                Vec::new(),
                            );
                        }
                    };
                    let _ = startup_tx.send(Ok(()));
                    drop(startup_tx);
                    threaded_worker_loop_with_remote(
                        runtime,
                        receiver,
                        worker_config,
                        remote_wiring,
                        shard_metrics_for_worker,
                    )
                }) {
                Ok(handle) => handle,
                Err(source) => {
                    cleanup_startup_workers(
                        &commands,
                        handles
                            .into_iter()
                            .map(|(shard, handle, _)| (shard, handle))
                            .collect(),
                    );
                    return Err(StartupError::ThreadSpawn {
                        shard: shard_id,
                        source,
                    });
                }
            };
            handles.push((shard_id, handle, startup_rx));
        }

        for (shard_id, _, startup_rx) in &handles {
            let failure = match startup_rx.recv_timeout(DEFAULT_STARTUP_HANDSHAKE_TIMEOUT) {
                Ok(Ok(())) => None,
                Ok(Err(error)) => Some(error),
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    Some(StartupError::WorkerHandshakeDisconnected(*shard_id))
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    Some(StartupError::WorkerHandshakeTimeout {
                        shard: *shard_id,
                        timeout: DEFAULT_STARTUP_HANDSHAKE_TIMEOUT,
                    })
                }
            };
            if let Some(error) = failure {
                let handles = handles
                    .into_iter()
                    .map(|(shard, handle, _)| (shard, handle))
                    .collect();
                cleanup_startup_workers(&commands, handles);
                return Err(error);
            }
        }

        let workers: Vec<ShutdownWorker<S, F>> = handles
            .into_iter()
            .map(|(shard_id, handle, _)| {
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

        Ok(Self {
            commands,
            shard_metrics,
            remote_metrics,
            shutdown,
            control_call_timeout: config.control_call_timeout,
            system_incarnation,
        })
    }

    /// Registers one root isolate on a chosen shard.
    /// Returns [`ThreadedRuntimeError::CommandFull`] without registering the
    /// isolate when that shard's bounded host-control queue is saturated.
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
        I::Io: IntoErasedCall<I::Message> + 'static,
        I::Fact: crate::fact::IntoRuntimeFact + 'static,
        Outbound: Send + 'static,
    {
        self.call_on(shard, move |runtime| {
            runtime.register_sendable_with_capacity::<I, Outbound>(isolate, mailbox_capacity)
        })
    }

    /// Registers one split event/request service on a chosen shard.
    ///
    /// Returns [`ThreadedRuntimeError::UnknownShard`] when `shard` is not
    /// owned by this runtime.
    #[allow(private_bounds)]
    pub fn register_split_service_on<I, Event, Request, Outbound>(
        &self,
        shard: ShardId,
        isolate: I,
        mailbox_capacity: usize,
    ) -> Result<crate::SplitServiceHandle<Event, Request, I::Reply>, ThreadedRuntimeError>
    where
        I: Isolate<
                Shard = S,
                Message = tina::ServiceMessage<Event, Request>,
                Send = TinaOutbound<Outbound>,
            > + tina::CallableIsolate
            + Send
            + 'static,
        Event: 'static,
        Request: 'static,
        I::Reply: Send + 'static,
        I::Spawn: IntoErasedSpawn<S, F> + 'static,
        I::SpawnObserved: IntoErasedSpawnObserved<S, F, I::Message> + 'static,
        I::SpawnObservedRemote: IntoSendErasedSpawnObserved<S, F, I::Message> + 'static,
        I::Io: IntoErasedCall<I::Message> + 'static,
        I::Fact: crate::fact::IntoRuntimeFact + 'static,
        Outbound: Send + 'static,
    {
        self.register_with_capacity_on::<I, Outbound>(shard, isolate, mailbox_capacity)
            .map(crate::SplitServiceHandle::from_address)
    }

    /// Registers one event-only service on a chosen shard.
    ///
    /// Returns [`ThreadedRuntimeError::UnknownShard`] when `shard` is not
    /// owned by this runtime.
    #[allow(private_bounds)]
    pub fn register_event_service_on<I, Event, Outbound>(
        &self,
        shard: ShardId,
        isolate: I,
        mailbox_capacity: usize,
    ) -> Result<crate::EventServiceHandle<Event>, ThreadedRuntimeError>
    where
        I: Isolate<
                Shard = S,
                Message = tina::ServiceMessage<Event, std::convert::Infallible>,
                Reply = (),
                Send = TinaOutbound<Outbound>,
            > + Send
            + 'static,
        Event: 'static,
        I::Spawn: IntoErasedSpawn<S, F> + 'static,
        I::SpawnObserved: IntoErasedSpawnObserved<S, F, I::Message> + 'static,
        I::SpawnObservedRemote: IntoSendErasedSpawnObserved<S, F, I::Message> + 'static,
        I::Io: IntoErasedCall<I::Message> + 'static,
        I::Fact: crate::fact::IntoRuntimeFact + 'static,
        Outbound: Send + 'static,
    {
        self.register_with_capacity_on::<I, Outbound>(shard, isolate, mailbox_capacity)
            .map(|address| crate::SplitServiceHandle::from_address(address).events)
    }

    /// Registers one request-only service on a chosen shard.
    ///
    /// Returns [`ThreadedRuntimeError::UnknownShard`] when `shard` is not
    /// owned by this runtime.
    #[allow(private_bounds)]
    pub fn register_request_service_on<I, Request, Outbound>(
        &self,
        shard: ShardId,
        isolate: I,
        mailbox_capacity: usize,
    ) -> Result<crate::RequestServiceHandle<Request, I::Reply>, ThreadedRuntimeError>
    where
        I: Isolate<
                Shard = S,
                Message = tina::ServiceMessage<std::convert::Infallible, Request>,
                Send = TinaOutbound<Outbound>,
            > + tina::CallableIsolate
            + Send
            + 'static,
        Request: 'static,
        I::Reply: Send + 'static,
        I::Spawn: IntoErasedSpawn<S, F> + 'static,
        I::SpawnObserved: IntoErasedSpawnObserved<S, F, I::Message> + 'static,
        I::SpawnObservedRemote: IntoSendErasedSpawnObserved<S, F, I::Message> + 'static,
        I::Io: IntoErasedCall<I::Message> + 'static,
        I::Fact: crate::fact::IntoRuntimeFact + 'static,
        Outbound: Send + 'static,
    {
        self.register_with_capacity_on::<I, Outbound>(shard, isolate, mailbox_capacity)
            .map(|address| crate::SplitServiceHandle::from_address(address).requests)
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
        I::Io: IntoErasedCall<I::Message> + 'static,
        I::Fact: crate::fact::IntoRuntimeFact + 'static,
        Outbound: Send + 'static,
    {
        match self.call_on_with_input(
            shard,
            (isolate, bootstrap),
            move |runtime, (isolate, bootstrap)| {
                runtime.register_sendable_with_capacity_and_bootstrap::<I, Outbound>(
                    isolate,
                    mailbox_capacity,
                    bootstrap,
                )
            },
        ) {
            Ok(Ok(address)) => Ok(address),
            Ok(Err(err)) => Err(ThreadedRegisterBootstrapError::from_register(err)),
            Err(RecoverableControlCallError::NotAdmitted {
                error: ThreadedRuntimeError::CommandFull,
                input: (_, bootstrap),
            }) => Err(ThreadedRegisterBootstrapError::CommandFull(bootstrap)),
            Err(RecoverableControlCallError::NotAdmitted {
                error: ThreadedRuntimeError::WorkerStopped,
                input: (_, bootstrap),
            }) => Err(ThreadedRegisterBootstrapError::CommandClosed(bootstrap)),
            Err(RecoverableControlCallError::NotAdmitted {
                error: ThreadedRuntimeError::UnknownShard(shard),
                input: (_, bootstrap),
            }) => Err(ThreadedRegisterBootstrapError::UnknownShard(
                shard, bootstrap,
            )),
            Err(RecoverableControlCallError::Accepted(ThreadedRuntimeError::WorkerStopped)) => {
                Err(ThreadedRegisterBootstrapError::WorkerStopped)
            }
            Err(RecoverableControlCallError::Accepted(_))
            | Err(RecoverableControlCallError::NotAdmitted { .. }) => {
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
        self.ensure_local_system(parent)?;
        self.call_on(parent.shard(), move |runtime| {
            runtime.supervise(parent, config)
        })
    }

    /// Returns a live child lifecycle report from the parent shard.
    pub fn child_lifecycle_report<M: 'static, R: 'static>(
        &self,
        parent: Address<M, R>,
    ) -> Result<ChildLifecycleReport, ThreadedRuntimeError> {
        self.ensure_local_system(parent)?;
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
        self.ensure_local_system(parent)?;
        self.call_on(parent.shard(), move |runtime| {
            runtime.observe_child_restarted(parent)
        })
    }

    /// Attempts bounded ingress to the worker that owns `address`.
    ///
    /// Returns [`ThreadedTrySendError::UnknownShard`] without consuming any
    /// runtime capacity when the address belongs to another shard topology.
    pub fn try_send<M: Send + 'static, R: 'static>(
        &self,
        address: Address<M, R>,
        message: M,
    ) -> Result<(), ThreadedTrySendError> {
        if address.system() != self.system_incarnation {
            return Err(ThreadedTrySendError::ForeignSystem {
                expected: self.system_incarnation,
                actual: address.system(),
            });
        }
        let Some(sender) = self.commands.get(&address.shard()) else {
            return Err(ThreadedTrySendError::UnknownShard(address.shard()));
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

    /// Attempts bounded ingress through a service event capability.
    ///
    /// Returns [`ThreadedTrySendError::UnknownShard`] when the event address
    /// targets a shard not owned by this runtime.
    pub fn try_send_event<Event, Request>(
        &self,
        address: tina::ServiceEventAddress<Event, Request>,
        event: Event,
    ) -> Result<(), ThreadedTrySendError>
    where
        Event: Send + 'static,
        Request: Send + 'static,
    {
        self.try_send(
            address.address().address(),
            tina::ServiceMessage::Event(event),
        )
    }

    /// Attempts one ingress send and waits for the owning worker to report the
    /// exact mailbox outcome.
    ///
    /// # Intentionally unbounded
    ///
    /// Like [`crate::ThreadedRuntime::send_and_observe`], this waits without a
    /// host timeout. A worker wedged in user code can block this host thread
    /// indefinitely. Use [`Self::send_observed_until`] when the host needs a
    /// deadline and a no-late-delivery guarantee.
    ///
    /// Returns [`ThreadedSendObservedError::UnknownShard`] without enqueueing
    /// the message when the address targets a shard not owned by this runtime.
    pub fn send_and_observe<M: Send + 'static, R: 'static>(
        &self,
        address: Address<M, R>,
        message: M,
    ) -> Result<(), ThreadedSendObservedError> {
        if address.system() != self.system_incarnation {
            return Err(ThreadedSendObservedError::ForeignSystem {
                expected: self.system_incarnation,
                actual: address.system(),
            });
        }
        let Some(sender) = self.commands.get(&address.shard()) else {
            return Err(ThreadedSendObservedError::UnknownShard(address.shard()));
        };
        let metrics = self
            .shard_metrics
            .get(&address.shard())
            .expect("owned shard has metrics");
        if metrics.state() == LiveShardState::Failed {
            metrics.ingress.rejected_closed();
            return Err(ThreadedSendObservedError::WorkerStopped);
        }
        let (reply_tx, reply_rx) = std::sync::mpsc::channel();
        let command = ThreadedCommand::Run(Box::new(move |runtime| {
            let outcome = runtime
                .try_send(address, message)
                .map_err(|error| match error {
                    crate::IngressSendError::ForeignSystem {
                        expected, actual, ..
                    } => ThreadedSendObservedError::ForeignSystem { expected, actual },
                    crate::IngressSendError::Full(_) => ThreadedSendObservedError::MailboxFull,
                    crate::IngressSendError::Closed(_) => ThreadedSendObservedError::MailboxClosed,
                });
            let _ = reply_tx.send(outcome);
        }));
        match sender.try_send(command) {
            Ok(()) => {
                metrics.ingress.accepted();
                reply_rx
                    .recv()
                    .unwrap_or(Err(ThreadedSendObservedError::WorkerStopped))
            }
            Err(std::sync::mpsc::TrySendError::Full(_)) => {
                metrics.ingress.rejected_full();
                Err(ThreadedSendObservedError::IngressFull)
            }
            Err(std::sync::mpsc::TrySendError::Disconnected(_)) => {
                metrics.ingress.rejected_closed();
                metrics.set_state(LiveShardState::Failed);
                Err(ThreadedSendObservedError::WorkerStopped)
            }
        }
    }

    /// Sends one split-service event and reports the exact target-mailbox
    /// outcome from the event address's owning shard.
    pub fn send_event_and_observe<Event, Request>(
        &self,
        address: tina::ServiceEventAddress<Event, Request>,
        event: Event,
    ) -> Result<(), ThreadedSendObservedError>
    where
        Event: Send + 'static,
        Request: Send + 'static,
    {
        self.send_and_observe(
            address.address().address(),
            tina::ServiceMessage::Event(event),
        )
    }

    /// Attempts one bounded observed send on the address's owning shard.
    /// Accepted observers settle exactly once, including worker-failure races.
    ///
    /// # Panics
    ///
    /// Panics when the address targets a shard not owned by this runtime.
    pub fn try_send_and_observe_with<M, R, O>(
        &self,
        address: Address<M, R>,
        message: M,
        observer: O,
    ) -> Result<(), ThreadedTrySendError>
    where
        M: Send + 'static,
        R: 'static,
        O: FnOnce(Result<(), ThreadedSendObservedError>) + Send + 'static,
    {
        self.try_send_and_observe_with_preflight(address, message, |_| None, observer)
    }

    /// Attempts one bounded observed send with a worker-side preflight on the
    /// address's owning shard.
    ///
    /// The preflight runs immediately before mailbox admission and must stay
    /// nonblocking. Accepted observers settle exactly once. If preflight or
    /// mailbox admission panics, worker unwind settles the retained observer
    /// with `WorkerStopped`; an observer callback panic is contained after
    /// settlement.
    ///
    /// Returns [`ThreadedTrySendError::UnknownShard`] without invoking the
    /// preflight or observer when the address is not owned by this runtime.
    pub fn try_send_and_observe_with_preflight<M, R, P, O>(
        &self,
        address: Address<M, R>,
        message: M,
        preflight: P,
        observer: O,
    ) -> Result<(), ThreadedTrySendError>
    where
        M: Send + 'static,
        R: 'static,
        P: FnOnce(&M) -> Option<ThreadedSendObservedError> + Send + 'static,
        O: FnOnce(Result<(), ThreadedSendObservedError>) + Send + 'static,
    {
        if address.system() != self.system_incarnation {
            return Err(ThreadedTrySendError::ForeignSystem {
                expected: self.system_incarnation,
                actual: address.system(),
            });
        }
        let Some(sender) = self.commands.get(&address.shard()) else {
            return Err(ThreadedTrySendError::UnknownShard(address.shard()));
        };
        let metrics = self
            .shard_metrics
            .get(&address.shard())
            .expect("owned shard has metrics");
        if metrics.state() == LiveShardState::Failed {
            metrics.ingress.rejected_closed();
            return Err(ThreadedTrySendError::WorkerStopped);
        }
        let command = observed_send_command(address, message, preflight, observer);
        match sender.try_send(command) {
            Ok(()) => {
                metrics.ingress.accepted();
                Ok(())
            }
            Err(std::sync::mpsc::TrySendError::Full(command)) => {
                let ThreadedCommand::RunObserved(command) = command else {
                    unreachable!("observed send enqueues an observed command")
                };
                command.disarm();
                metrics.ingress.rejected_full();
                Err(ThreadedTrySendError::IngressFull)
            }
            Err(std::sync::mpsc::TrySendError::Disconnected(command)) => {
                let ThreadedCommand::RunObserved(command) = command else {
                    unreachable!("observed send enqueues an observed command")
                };
                command.disarm();
                metrics.ingress.rejected_closed();
                metrics.set_state(LiveShardState::Failed);
                Err(ThreadedTrySendError::WorkerStopped)
            }
        }
    }

    /// Attempts one bounded observed send and records its eventual outcome.
    pub fn try_send_outcome<M, R>(
        &self,
        address: Address<M, R>,
        message: M,
        outcomes: &HostBurstOutcomes,
    ) -> Result<(), ThreadedTrySendError>
    where
        M: Send + 'static,
        R: 'static,
    {
        if address.system() != self.system_incarnation {
            return Err(ThreadedTrySendError::ForeignSystem {
                expected: self.system_incarnation,
                actual: address.system(),
            });
        }
        if !self.commands.contains_key(&address.shard()) {
            return Err(ThreadedTrySendError::UnknownShard(address.shard()));
        }
        outcomes.note_submitted();
        let observer = outcomes.observer();
        match self.try_send_and_observe_with(address, message, observer) {
            Ok(()) => Ok(()),
            Err(error) => {
                outcomes.note_host_side_error(error);
                Err(error)
            }
        }
    }

    /// Retries observed admission on the owning shard until it succeeds or the
    /// deadline expires. `Timeout` guarantees that no accepted attempt can
    /// deliver after this method returns.
    ///
    /// Returns [`SendObservedUntilError::UnknownShard`] before invoking
    /// `make_message` when the address is not owned by this runtime.
    pub fn send_observed_until<M, R, MakeMessage>(
        &self,
        address: Address<M, R>,
        deadline: Instant,
        backoff: Duration,
        mut make_message: MakeMessage,
    ) -> Result<(), SendObservedUntilError>
    where
        M: Send + 'static,
        R: 'static,
        MakeMessage: FnMut() -> M,
    {
        if address.system() != self.system_incarnation {
            return Err(SendObservedUntilError::ForeignSystem {
                expected: self.system_incarnation,
                actual: address.system(),
            });
        }
        let Some(sender) = self.commands.get(&address.shard()) else {
            return Err(SendObservedUntilError::UnknownShard(address.shard()));
        };
        let metrics = self
            .shard_metrics
            .get(&address.shard())
            .expect("owned shard has metrics");
        loop {
            let now = Instant::now();
            if now >= deadline {
                return Err(SendObservedUntilError::Timeout);
            }
            if metrics.state() == LiveShardState::Failed {
                metrics.ingress.rejected_closed();
                return Err(SendObservedUntilError::WorkerStopped);
            }

            let message = make_message();
            let now = Instant::now();
            if now >= deadline {
                return Err(SendObservedUntilError::Timeout);
            }
            let remaining = deadline.saturating_duration_since(now);
            let mut attempt = deadline_observed_attempt(address, message);
            let outcome = match sender.try_send(attempt.take_command()) {
                Ok(()) => {
                    metrics.ingress.accepted();
                    match attempt.wait_until(remaining) {
                        Ok(outcome) => outcome,
                        Err(SendObservedUntilError::WorkerStopped) => {
                            metrics.set_state(LiveShardState::Failed);
                            return Err(SendObservedUntilError::WorkerStopped);
                        }
                        Err(error) => return Err(error),
                    }
                }
                Err(std::sync::mpsc::TrySendError::Full(command)) => {
                    let ThreadedCommand::RunObserved(command) = command else {
                        unreachable!("deadline admission enqueues an observed command")
                    };
                    command.disarm();
                    metrics.ingress.rejected_full();
                    Err(ThreadedSendObservedError::IngressFull)
                }
                Err(std::sync::mpsc::TrySendError::Disconnected(command)) => {
                    let ThreadedCommand::RunObserved(command) = command else {
                        unreachable!("deadline admission enqueues an observed command")
                    };
                    command.disarm();
                    metrics.ingress.rejected_closed();
                    metrics.set_state(LiveShardState::Failed);
                    return Err(SendObservedUntilError::WorkerStopped);
                }
            };

            match outcome {
                Ok(()) => return Ok(()),
                Err(ThreadedSendObservedError::MailboxFull)
                | Err(ThreadedSendObservedError::IngressFull) => {
                    let now = Instant::now();
                    if now >= deadline {
                        return Err(SendObservedUntilError::Timeout);
                    }
                    thread::sleep(backoff.min(deadline.saturating_duration_since(now)));
                }
                Err(ThreadedSendObservedError::MailboxClosed) => {
                    return Err(SendObservedUntilError::Closed);
                }
                Err(ThreadedSendObservedError::WorkerStopped) => {
                    return Err(SendObservedUntilError::WorkerStopped);
                }
                Err(ThreadedSendObservedError::UnknownShard(shard)) => {
                    return Err(SendObservedUntilError::UnknownShard(shard));
                }
                Err(ThreadedSendObservedError::ForeignSystem { expected, actual }) => {
                    return Err(SendObservedUntilError::ForeignSystem { expected, actual });
                }
            }
        }
    }

    /// Retries split-service event admission on the owning shard until the
    /// event lands or the deadline expires.
    pub fn send_event_observed_until<Event, Request, MakeEvent>(
        &self,
        address: tina::ServiceEventAddress<Event, Request>,
        deadline: Instant,
        backoff: Duration,
        mut make_event: MakeEvent,
    ) -> Result<(), SendObservedUntilError>
    where
        Event: Send + 'static,
        Request: Send + 'static,
        MakeEvent: FnMut() -> Event,
    {
        self.send_observed_until(address.address().address(), deadline, backoff, move || {
            tina::ServiceMessage::Event(make_event())
        })
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
        if address.system() != self.system_incarnation {
            return Err(observation::ResultWaitError::ForeignSystem {
                expected: self.system_incarnation,
                actual: address.system(),
            });
        }
        if !self.commands.contains_key(&address.shard()) {
            return Err(observation::ResultWaitError::UnknownShard(address.shard()));
        }
        match self.call_on(address.shard(), move |runtime| {
            runtime.observe_result::<T, M, R>(address)
        }) {
            Ok(result) => result,
            Err(ThreadedRuntimeError::CommandFull) => {
                Err(observation::ResultWaitError::CommandFull)
            }
            Err(ThreadedRuntimeError::ForeignSystem { expected, actual }) => {
                Err(observation::ResultWaitError::ForeignSystem { expected, actual })
            }
            Err(ThreadedRuntimeError::UnknownShard(shard)) => {
                Err(observation::ResultWaitError::UnknownShard(shard))
            }
            Err(_) => Err(observation::ResultWaitError::RuntimeStopped),
        }
    }

    fn ensure_local_system<M, R>(
        &self,
        address: Address<M, R>,
    ) -> Result<(), ThreadedRuntimeError> {
        if address.system() == self.system_incarnation {
            Ok(())
        } else {
            Err(ThreadedRuntimeError::ForeignSystem {
                expected: self.system_incarnation,
                actual: address.system(),
            })
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

    /// Total blocking-park wakeups observed by one worker shard.
    pub fn park_wakeups_on(&self, shard: ShardId) -> Result<u64, ThreadedRuntimeError> {
        self.shard_metrics
            .get(&shard)
            .map(|metrics| metrics.park_wakeups())
            .ok_or(ThreadedRuntimeError::UnknownShard(shard))
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
            config.timer_capacity,
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
        if address.system() != self.system_incarnation {
            return Err(ThreadedRuntimeError::ForeignSystem {
                expected: self.system_incarnation,
                actual: address.system(),
            });
        }
        if !self.commands.contains_key(&shard) {
            return Err(ThreadedRuntimeError::UnknownShard(shard));
        }
        let (reply_tx, reply_rx) = mpsc::channel();
        let driver = HostCallDriverMS::<S, M, R> {
            sender: reply_tx,
            _marker: PhantomData,
        };
        let Some(sender) = self.commands.get(&shard) else {
            return Err(ThreadedRuntimeError::UnknownShard(shard));
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
                Err(crate::IngressSendError::Full(_)) => {
                    panic!("fresh host-call driver mailbox was unexpectedly full");
                }
                Err(crate::IngressSendError::Closed(_)) => {
                    panic!("fresh host-call driver mailbox was unexpectedly closed");
                }
                Err(crate::IngressSendError::ForeignSystem { .. }) => {
                    panic!("fresh host-call driver carried foreign provenance");
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

    /// Blocking host call through a split-service request capability,
    /// routed to the shard that owns `address`.
    ///
    /// This is the multi-shard companion to
    /// [`crate::ThreadedRuntime::call_blocking_request`]: it wraps the
    /// request in [`tina::ServiceMessage::Request`] and keeps host code
    /// from reaching for the raw split envelope address. See
    /// [`Self::call_blocking`] for the shard-routing and panic contract.
    pub fn call_blocking_request<Event, Request, Reply>(
        &self,
        address: tina::ServiceRequestAddress<Event, Request, Reply>,
        request: Request,
        timeout: Duration,
    ) -> Result<CallOutcome<Reply>, ThreadedRuntimeError>
    where
        Event: Send + 'static,
        Request: Send + 'static,
        Reply: Send + 'static,
    {
        self.call_blocking(
            address.address().address(),
            tina::ServiceMessage::Request(request),
            timeout,
        )
    }

    fn call_on_with_input<R, T, C>(
        &self,
        shard: ShardId,
        input: T,
        command: C,
    ) -> Result<R, RecoverableControlCallError<T>>
    where
        R: Send + 'static,
        T: Send + 'static,
        C: FnOnce(&mut Runtime<S, F>, T) -> R + Send + 'static,
    {
        let Some(sender) = self.commands.get(&shard) else {
            return Err(RecoverableControlCallError::NotAdmitted {
                error: ThreadedRuntimeError::UnknownShard(shard),
                input,
            });
        };
        let input = Arc::new(std::sync::Mutex::new(Some(input)));
        let worker_input = Arc::clone(&input);
        let (reply_tx, reply_rx) = std::sync::mpsc::channel();
        let threaded_command = ThreadedCommand::Run(Box::new(move |runtime| {
            let input = worker_input
                .lock()
                .expect("recoverable control-call input lock poisoned")
                .take()
                .expect("recoverable control-call input taken exactly once");
            let _ = reply_tx.send(command(runtime, input));
        }));
        match sender.try_send(threaded_command) {
            Ok(()) => {}
            Err(std::sync::mpsc::TrySendError::Full(command)) => {
                drop(command);
                let input = input
                    .lock()
                    .expect("recoverable control-call input lock poisoned")
                    .take()
                    .expect("unadmitted control call retains its input");
                return Err(RecoverableControlCallError::NotAdmitted {
                    error: ThreadedRuntimeError::CommandFull,
                    input,
                });
            }
            Err(std::sync::mpsc::TrySendError::Disconnected(command)) => {
                if let Some(metrics) = self.shard_metrics.get(&shard) {
                    metrics.set_state(LiveShardState::Failed);
                }
                drop(command);
                let input = input
                    .lock()
                    .expect("recoverable control-call input lock poisoned")
                    .take()
                    .expect("unadmitted control call retains its input");
                return Err(RecoverableControlCallError::NotAdmitted {
                    error: ThreadedRuntimeError::WorkerStopped,
                    input,
                });
            }
        }

        match reply_rx.recv_timeout(self.control_call_timeout) {
            Ok(reply) => Ok(reply),
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                if let Some(metrics) = self.shard_metrics.get(&shard) {
                    metrics.set_state(LiveShardState::Failed);
                }
                Err(RecoverableControlCallError::Accepted(
                    ThreadedRuntimeError::WorkerUnresponsive,
                ))
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                if let Some(metrics) = self.shard_metrics.get(&shard) {
                    metrics.set_state(LiveShardState::Failed);
                }
                Err(RecoverableControlCallError::Accepted(
                    ThreadedRuntimeError::WorkerStopped,
                ))
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
        match sender.try_send(ThreadedCommand::Run(Box::new(move |runtime| {
            let _ = reply_tx.send(command(runtime));
        }))) {
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
        // Bounded wait: a wedged handler on one shard must not hang the host.
        match reply_rx.recv_timeout(self.control_call_timeout) {
            Ok(reply) => Ok(reply),
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                if let Some(metrics) = self.shard_metrics.get(&shard) {
                    metrics.set_state(LiveShardState::Failed);
                }
                Err(ThreadedRuntimeError::WorkerUnresponsive)
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                if let Some(metrics) = self.shard_metrics.get(&shard) {
                    metrics.set_state(LiveShardState::Failed);
                }
                Err(ThreadedRuntimeError::WorkerStopped)
            }
        }
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
        io: RuntimeCall<HostCallMsg<M, R>>,
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
    // Refresh the live resource snapshot on idle and command turns, but not
    // after a fast delivery turn: recomputing the O(pending) resource report on
    // every hot turn is the per-op tax this phase removes. Counts refresh again
    // as soon as the worker parks or runs a command (phase 145).
    let mut refresh_metrics = true;
    loop {
        if refresh_metrics {
            shard_metrics.set_resource_counts(runtime.resource_report());
            shard_metrics.set_trace_dropped(runtime.trace_dropped());
        }
        refresh_metrics = true;
        let route_remote_lossless =
            |envelope: QueuedRemoteEnvelope| -> Result<(), Box<RemoteRouteFailure>> {
                let target_shard = envelope.target_shard();
                let terminal = is_terminal_remote_envelope(&envelope);
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
            Ok(ThreadedCommand::RunObserved(command)) => {
                command.run(&mut runtime);
                continue;
            }
            Ok(ThreadedCommand::HostCall { dispatcher, begin }) => {
                run_host_call(&mut runtime, dispatcher, begin);
                continue;
            }
            Ok(ThreadedCommand::Shutdown) => {
                deliver_shutdown_signal_and_drain_with_remote(&mut runtime, &mut |_, envelope| {
                    route_remote_preserving_terminal(
                        envelope,
                        &mut terminal_overflow,
                        &route_remote_lossless,
                    )
                });
                break;
            }
            Err(std::sync::mpsc::TryRecvError::Disconnected) => break,
            Err(std::sync::mpsc::TryRecvError::Empty) => {}
        }

        let delivered = runtime.step_with_remote(&mut |_, envelope| route_remote(envelope));

        if delivered > 0 || remote_delivered > 0 {
            refresh_metrics = false;
            continue;
        }

        // Nothing local, remote, or overflow was deliverable. Park on the
        // command queue, then explicitly re-poll remote inbound and step the
        // runtime. Runtime-owned work, pending cross-shard replies, and
        // terminal overflow do not arrive through this queue, so they use the
        // short bounded re-poll; a fully idle shard uses the longer idle wait.
        let park = if runtime.has_in_flight_calls()
            || runtime.has_pending_runtime_work()
            || !terminal_overflow.is_empty()
        {
            config.idle_repoll_interval.min(config.idle_wait)
        } else {
            config.idle_wait
        };
        let park_result = receiver.recv_timeout(park);
        shard_metrics.record_park_wakeup();
        match park_result {
            Ok(ThreadedCommand::Run(command)) => command(&mut runtime),
            Ok(ThreadedCommand::RunObserved(command)) => command.run(&mut runtime),
            Ok(ThreadedCommand::HostCall { dispatcher, begin }) => {
                run_host_call(&mut runtime, dispatcher, begin)
            }
            Ok(ThreadedCommand::Shutdown) => {
                deliver_shutdown_signal_and_drain_with_remote(&mut runtime, &mut |_, envelope| {
                    route_remote_preserving_terminal(
                        envelope,
                        &mut terminal_overflow,
                        &route_remote_lossless,
                    )
                });
                break;
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
        }
    }

    let shutdown_deadline =
        tina::Deadline::from_instant(Instant::now(), config.shutdown_lane_drain_timeout).instant();
    let shutdown_result = runtime.cancel_in_flight_calls_for_shutdown(shutdown_deadline);
    shard_metrics.set_resource_counts(runtime.resource_report());
    shard_metrics.set_trace_dropped(runtime.trace_dropped());
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
                && is_terminal_remote_envelope(&failure.envelope) =>
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
                    && is_terminal_remote_envelope(&failure.envelope) =>
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

fn is_terminal_remote_envelope(envelope: &QueuedRemoteEnvelope) -> bool {
    matches!(
        envelope,
        QueuedRemoteEnvelope::CallReply(_)
            | QueuedRemoteEnvelope::SpawnReply(_)
            | QueuedRemoteEnvelope::SpawnCancel(_)
            | QueuedRemoteEnvelope::ChildStop(_)
            | QueuedRemoteEnvelope::ChildStopped(_)
            | QueuedRemoteEnvelope::ChildRestart(_)
            | QueuedRemoteEnvelope::ChildRestarted(_)
    )
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
