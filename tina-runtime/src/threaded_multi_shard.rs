//! Threaded multi-shard runtime extracted from lib.rs (phase 055).
//!
//! Houses `ThreadedMultiShardRuntime`, the cross-shard worker loop
//! `threaded_worker_loop_with_remote`, and the remote-inbound drain helper
//! `drain_remote_inbound`. Each owned shard runs its own worker thread; this
//! type coordinates ingress, trace, capability, supervise, and shutdown across
//! the set.

use std::alloc::Global;
use std::collections::BTreeMap;
use std::sync::Arc;
use std::thread;
use std::time::Instant;

use betelgeuse::io_loop;
use tina::{Address, Isolate, Outbound as TinaOutbound, Shard, ShardId};
use tina_supervisor::SupervisorConfig;

use crate::call::{IntoErasedCall, RuntimeCall};
use crate::sharded::ReplyAdapter;
use crate::capabilities::RuntimeCapabilities;
use crate::clock::MonotonicClock;
use crate::driver::BetelgeuseDriver;
use crate::errors::{ThreadedRuntimeError, ThreadedTrySendError};
use crate::live_report::{
    LiveQueueMetrics, LiveRemoteQueueReport, LiveShardMetrics, LiveShardState, LiveTopologyReport,
};
use crate::local_system::{
    LocalSystemState, LocalSystemTerminalReport, ThreadedWorkerExit, ThreadedWorkerJoin,
    TraceSnapshot,
};
use crate::mailbox::MailboxFactory;
use crate::observation;
use crate::observer::TraceObserver;
use crate::threaded::{ThreadedCommand, ThreadedRuntimeConfig, deliver_shutdown_signal_and_drain};
use crate::trace::{RuntimeEvent, SendRejectedReason};
use crate::{
    IdSource, IntoErasedSpawn, QueuedRemoteEnvelope, Runtime, SendableQueuedRemoteEnvelope,
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
    handles: Vec<(ShardId, ThreadedWorkerJoin)>,
    shard_metrics: BTreeMap<ShardId, Arc<LiveShardMetrics>>,
    remote_metrics: BTreeMap<(ShardId, ShardId), Arc<LiveQueueMetrics>>,
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
        let mut remote_receivers: BTreeMap<
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
                    remote_senders.insert((source.id(), target.id()), sender);
                    remote_receivers
                        .entry(target.id())
                        .or_default()
                        .push((source.id(), receiver));
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
            let remote_senders = remote_senders.clone();
            let shard_id = shard.id();
            let remote_receivers = remote_receivers.remove(&shard_id).unwrap_or_default();
            let remote_metrics_for_worker = remote_metrics.clone();
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
                            remote_senders,
                            remote_receivers,
                            remote_metrics_for_worker,
                            shard_metrics_for_worker,
                        )
                    })
                    .expect("failed to spawn Tina threaded shard worker"),
            ));
        }

        Self {
            commands,
            handles,
            shard_metrics,
            remote_metrics,
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
        I::Call: IntoErasedCall<I::Message> + 'static,
        Outbound: Send + 'static,
    {
        self.call_on(shard, move |runtime| {
            runtime.register_sendable_with_capacity::<I, Outbound>(isolate, mailbox_capacity)
        })
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
    /// Mirrors [`MultiShardRuntime::register_reply_adapter_on`]
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
    pub fn shutdown_report(mut self) -> LocalSystemTerminalReport {
        let (shutdown_result, trace) = self.shutdown_inner_with_available_trace();
        match shutdown_result {
            Ok(()) => LocalSystemTerminalReport::new_with_topology(
                LocalSystemState::Closed,
                trace,
                self.topology(),
            ),
            Err(error) => LocalSystemTerminalReport::failed_with_topology_and_trace(
                error,
                self.topology(),
                trace,
            ),
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

    fn shutdown_inner(&mut self) -> Result<Vec<RuntimeEvent>, ThreadedRuntimeError> {
        let (result, events) = self.shutdown_inner_with_available_trace();
        result.map(|()| events)
    }

    pub(crate) fn shutdown_inner_with_available_trace(
        &mut self,
    ) -> (Result<(), ThreadedRuntimeError>, Vec<RuntimeEvent>) {
        for sender in self.commands.values() {
            let _ = sender.send(ThreadedCommand::Shutdown);
        }

        let mut events = Vec::new();
        let mut failure = None;
        for (shard, handle) in std::mem::take(&mut self.handles) {
            match handle.join() {
                Ok(exit) => {
                    if let Some(error) = exit.error {
                        if let Some(metrics) = self.shard_metrics.get(&shard) {
                            metrics.set_state(LiveShardState::Failed);
                        }
                        failure = Some(error);
                    } else if let Some(metrics) = self.shard_metrics.get(&shard) {
                        metrics.set_state(LiveShardState::Stopped);
                    }
                    events.extend(exit.trace);
                }
                Err(_) => {
                    if let Some(metrics) = self.shard_metrics.get(&shard) {
                        metrics.set_state(LiveShardState::Failed);
                    }
                    failure = Some(ThreadedRuntimeError::WorkerStopped);
                }
            }
        }
        events.sort_by_key(|event| event.id());
        if let Some(error) = failure {
            return (Err(error), events);
        }
        (Ok(()), events)
    }
}

impl<S, F> Drop for ThreadedMultiShardRuntime<S, F>
where
    S: Shard + Send + 'static,
    F: MailboxFactory + Send + Clone + 'static,
{
    fn drop(&mut self) {
        let _ = self.shutdown_inner();
    }
}

fn threaded_worker_loop_with_remote<S, F>(
    mut runtime: Runtime<S, F>,
    receiver: std::sync::mpsc::Receiver<ThreadedCommand<S, F>>,
    config: ThreadedRuntimeConfig,
    remote_senders: BTreeMap<
        (ShardId, ShardId),
        std::sync::mpsc::SyncSender<SendableQueuedRemoteEnvelope>,
    >,
    remote_receivers: Vec<(
        ShardId,
        std::sync::mpsc::Receiver<SendableQueuedRemoteEnvelope>,
    )>,
    remote_metrics: BTreeMap<(ShardId, ShardId), Arc<LiveQueueMetrics>>,
    shard_metrics: Arc<LiveShardMetrics>,
) -> ThreadedWorkerExit
where
    S: Shard,
    F: MailboxFactory,
{
    shard_metrics.set_worker_thread_id(format!("{:?}", thread::current().id()));
    let source_shard = runtime.shard().id();
    loop {
        shard_metrics.set_resource_counts(runtime.resource_report());
        let route_remote = |envelope: QueuedRemoteEnvelope| -> Result<(), SendRejectedReason> {
            let target_shard = envelope.target_shard();
            let Some(sender) = remote_senders.get(&(source_shard, target_shard)) else {
                panic!(
                    "ThreadedMultiShardRuntime targeted unknown destination shard {}",
                    target_shard.get()
                );
            };
            let envelope = SendableQueuedRemoteEnvelope::new(envelope);
            let metrics = remote_metrics.get(&(source_shard, target_shard));
            match sender.try_send(envelope) {
                Ok(()) => {
                    if let Some(metrics) = metrics {
                        metrics.accepted();
                    }
                    Ok(())
                }
                Err(std::sync::mpsc::TrySendError::Full(_)) => {
                    if let Some(metrics) = metrics {
                        metrics.rejected_full();
                    }
                    Err(SendRejectedReason::Full)
                }
                Err(std::sync::mpsc::TrySendError::Disconnected(_)) => {
                    if let Some(metrics) = metrics {
                        metrics.rejected_closed();
                    }
                    Err(SendRejectedReason::Closed)
                }
            }
        };
        let remote_delivered = drain_remote_inbound(
            &mut runtime,
            &remote_receivers,
            &route_remote,
            config.remote_inbound_drain_budget,
        );
        if remote_delivered == 0 {
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
        }

        let delivered = runtime.step_with_remote(&mut |_, envelope| route_remote(envelope));

        if delivered == 0 && !runtime.has_in_flight_calls() {
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

fn drain_remote_inbound<S, F>(
    runtime: &mut Runtime<S, F>,
    remote_receivers: &[(
        ShardId,
        std::sync::mpsc::Receiver<SendableQueuedRemoteEnvelope>,
    )],
    route_remote: &impl Fn(QueuedRemoteEnvelope) -> Result<(), SendRejectedReason>,
    budget: usize,
) -> usize
where
    S: Shard,
    F: MailboxFactory,
{
    let mut delivered = 0;
    for (_, receiver) in remote_receivers {
        loop {
            if delivered >= budget {
                return delivered;
            }
            match receiver.try_recv() {
                Ok(envelope) => {
                    delivered += 1;
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
    delivered
}
