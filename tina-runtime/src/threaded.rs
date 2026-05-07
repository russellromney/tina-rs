//! Threaded single-shard runtime extracted from lib.rs (phase 055).
//!
//! Houses `ThreadedRuntimeConfig`, the `ThreadedRuntime` worker handle,
//! `ThreadedCommand`, `threaded_worker_loop`,
//! `deliver_shutdown_signal_and_drain`, and the
//! `DEFAULT_SHUTDOWN_LANE_DRAIN_TIMEOUT` constant. The worker thread owns one
//! `Runtime` and processes a bounded `mpsc` command queue.

use std::alloc::Global;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use betelgeuse::{IOLoopHandle, io_loop};
use tina::{Address, Isolate, Outbound as TinaOutbound, Shard, TrySendError};
use tina_supervisor::SupervisorConfig;

use crate::call::IntoErasedCall;
use crate::capabilities::RuntimeCapabilities;
use crate::clock::MonotonicClock;
use crate::driver::{self, BetelgeuseDriver};
use crate::errors::{
    SendObservedUntilError, SuperviseError, ThreadedRuntimeError, ThreadedSendObservedError,
    ThreadedTrySendError,
};
use crate::host_burst::HostBurstOutcomes;
use crate::live_report::{LiveShardMetrics, LiveShardState, LiveTopologyReport};
use crate::local_system::{
    LocalSystemState, LocalSystemTerminalReport, ThreadedCommandFn, ThreadedIoLoopFactory,
    ThreadedWorkerExit, ThreadedWorkerJoin, TraceSnapshot,
};
use crate::mailbox::MailboxFactory;
use crate::observation::{self, BoundAddressWaiter};
use crate::trace::{CallKind, RuntimeEvent};
use crate::{IdSource, IntoErasedSpawn, PreallocationConfig, Runtime, TraceRetention};

/// Configuration for [`ThreadedRuntime`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ThreadedRuntimeConfig {
    /// Capacity of the bounded control/ingress queue feeding the shard worker.
    pub command_capacity: usize,

    /// Capacity of each live source-shard -> destination-shard transport.
    pub shard_pair_capacity: usize,

    /// Maximum remote envelopes one live shard worker harvests before giving
    /// its local runtime a turn.
    pub remote_inbound_drain_budget: usize,

    /// Capacity of the bounded storage lane used for local persistence work.
    pub storage_lane_capacity: usize,

    /// Capacity of the bounded DNS lane.
    pub dns_lane_capacity: usize,

    /// Capacity of the bounded TLS lane.
    pub tls_lane_capacity: usize,

    /// Capacity of the bounded process lane.
    pub process_lane_capacity: usize,

    /// Capacity of runtime-owned signal waits.
    pub signal_capacity: usize,

    /// Desired OS core for this shard worker.
    ///
    /// The current portable backend reports this as advisory intent only. It
    /// does not hard-pin the worker without a platform-specific affinity
    /// implementation.
    pub configured_core: Option<usize>,

    /// Setup-time reserves for runtime-owned metadata.
    pub preallocation: PreallocationConfig,

    /// Trace retention for the worker-owned runtime.
    pub trace_retention: TraceRetention,

    /// How long an idle worker may park before checking runtime-owned work
    /// again.
    pub idle_wait: Duration,

    /// Per-shard budget for draining lane workers after cancellation
    /// during shutdown. When the budget elapses, shutdown returns even if
    /// some lane work could not finish.
    pub shutdown_lane_drain_timeout: Duration,
}

impl Default for ThreadedRuntimeConfig {
    fn default() -> Self {
        Self {
            command_capacity: 64,
            shard_pair_capacity: 64,
            remote_inbound_drain_budget: 64,
            storage_lane_capacity: driver::DEFAULT_STORAGE_LANE_CAPACITY,
            dns_lane_capacity: driver::DEFAULT_DNS_LANE_CAPACITY,
            tls_lane_capacity: driver::DEFAULT_TLS_LANE_CAPACITY,
            process_lane_capacity: driver::DEFAULT_PROCESS_LANE_CAPACITY,
            signal_capacity: driver::DEFAULT_SIGNAL_CAPACITY,
            configured_core: None,
            preallocation: PreallocationConfig::default(),
            trace_retention: TraceRetention::Full,
            idle_wait: Duration::from_millis(1),
            shutdown_lane_drain_timeout: DEFAULT_SHUTDOWN_LANE_DRAIN_TIMEOUT,
        }
    }
}

/// Per-shard default budget for draining lane workers after cancellation.
pub const DEFAULT_SHUTDOWN_LANE_DRAIN_TIMEOUT: Duration = Duration::from_millis(100);

pub(crate) enum ThreadedCommand<S, F>
where
    S: Shard,
    F: MailboxFactory,
{
    Run(ThreadedCommandFn<S, F>),
    Shutdown,
}

/// One live shard-owned runtime worker.
///
/// The worker constructs and owns a single [`Runtime`] on its OS thread. The
/// handle only communicates through a bounded command queue, so ingress
/// pressure remains visible instead of falling into an unbounded executor
/// backlog. This is the Betelgeuse live substrate shape; the
/// explicit-step [`crate::Runtime`] and [`crate::MultiShardRuntime`] remain the semantic
/// oracle.
pub struct ThreadedRuntime<S, F>
where
    S: Shard + Send + 'static,
    F: MailboxFactory + Send + 'static,
{
    commands: std::sync::mpsc::SyncSender<ThreadedCommand<S, F>>,
    handle: Option<ThreadedWorkerJoin>,
    metrics: Arc<LiveShardMetrics>,
}

impl<S, F> ThreadedRuntime<S, F>
where
    S: Shard + Send + 'static,
    F: MailboxFactory + Send + 'static,
{
    /// Starts one worker thread for one shard runtime.
    pub fn new(shard: S, mailbox_factory: F) -> Self {
        Self::with_config(shard, mailbox_factory, ThreadedRuntimeConfig::default())
    }

    /// Starts one worker thread with explicit bounded-command configuration.
    pub fn with_config(shard: S, mailbox_factory: F, config: ThreadedRuntimeConfig) -> Self {
        Self::with_config_and_io_loop_factory(shard, mailbox_factory, config, || {
            io_loop(Global).expect("failed to initialise Betelgeuse IO loop for tina-runtime")
        })
    }

    /// Starts one worker thread with an explicit Betelgeuse I/O loop factory.
    ///
    /// The factory runs on the worker thread so loop implementations that own
    /// thread-local state can still be used without making the runtime itself
    /// shared across threads.
    pub fn with_config_and_io_loop_factory<G>(
        shard: S,
        mailbox_factory: F,
        config: ThreadedRuntimeConfig,
        io_loop_factory: G,
    ) -> Self
    where
        G: FnOnce() -> IOLoopHandle<Global> + Send + 'static,
    {
        if config.command_capacity == 0 {
            panic!("ThreadedRuntime requires command capacity > 0");
        }
        if config.storage_lane_capacity == 0 {
            panic!("ThreadedRuntime requires storage lane capacity > 0");
        }
        if config.dns_lane_capacity == 0 {
            panic!("ThreadedRuntime requires DNS lane capacity > 0");
        }
        if config.tls_lane_capacity == 0 {
            panic!("ThreadedRuntime requires TLS lane capacity > 0");
        }
        if config.process_lane_capacity == 0 {
            panic!("ThreadedRuntime requires process lane capacity > 0");
        }
        if config.signal_capacity == 0 {
            panic!("ThreadedRuntime requires signal capacity > 0");
        }
        if config.remote_inbound_drain_budget == 0 {
            panic!("ThreadedRuntime requires remote inbound drain budget > 0");
        }

        let (commands, receiver) = std::sync::mpsc::sync_channel(config.command_capacity);
        let shard_id = shard.id();
        let worker_name = format!("tina-shard-{}", shard_id.get());
        let metrics = Arc::new(LiveShardMetrics::new(
            shard_id,
            Some(worker_name.clone()),
            config,
        ));
        let io_loop_factory: ThreadedIoLoopFactory = Box::new(io_loop_factory);
        let worker_metrics = Arc::clone(&metrics);
        let handle = thread::Builder::new()
            .name(worker_name)
            .spawn(move || {
                threaded_worker_loop(
                    shard,
                    mailbox_factory,
                    receiver,
                    config,
                    io_loop_factory,
                    worker_metrics,
                )
            })
            .expect("failed to spawn Tina threaded worker");

        Self {
            commands,
            handle: Some(handle),
            metrics,
        }
    }

    /// Registers one root isolate and lets the worker allocate its mailbox.
    #[allow(private_bounds)]
    pub fn register_with_capacity<I, Outbound>(
        &self,
        isolate: I,
        mailbox_capacity: usize,
    ) -> Result<Address<I::Message, I::Reply>, ThreadedRuntimeError>
    where
        I: Isolate<Shard = S, Send = TinaOutbound<Outbound>> + Send + 'static,
        I::Message: 'static,
        I::Reply: 'static,
        I::Spawn: IntoErasedSpawn<S, F> + 'static,
        I::Call: IntoErasedCall<I::Message> + 'static,
        Outbound: 'static,
    {
        self.call(move |runtime| {
            runtime.register_with_capacity::<I, Outbound>(isolate, mailbox_capacity)
        })
    }

    /// Configures a registered isolate as supervisor on the worker shard.
    ///
    /// This method panics on unknown parent (consistent
    /// with the explicit-step `Runtime::supervise`). Use
    /// [`try_supervise`](Self::try_supervise) for a non-panicking variant
    /// that surfaces unknown / stale parents as a typed
    /// [`SuperviseError::UnknownParent`] without crashing the worker.
    pub fn supervise<M: 'static, R: 'static>(
        &self,
        parent: Address<M, R>,
        config: SupervisorConfig,
    ) -> Result<(), ThreadedRuntimeError> {
        self.call(move |runtime| runtime.supervise(parent, config))
    }

    /// Configures a registered isolate as supervisor on the worker shard
    /// without panicking on unknown / stale parents.
    ///
    /// `Ok(Ok(()))` — registration succeeded. `Ok(Err(SuperviseError::UnknownParent))`
    /// — the address is not currently registered or its generation is
    /// stale. `Err(ThreadedRuntimeError)` — the worker thread had already
    /// stopped or the shutdown handshake could not be observed.
    pub fn try_supervise<M: 'static, R: 'static>(
        &self,
        parent: Address<M, R>,
        config: SupervisorConfig,
    ) -> Result<Result<(), SuperviseError>, ThreadedRuntimeError> {
        self.call(move |runtime| runtime.try_supervise(parent, config))
    }

    /// Registers a typed waiter for the next `tcp_bind` completion on the
    /// worker shard.
    ///
    /// Returns a [`BoundAddressWaiter`] the host can call `.wait(timeout)`
    /// on. Each call returns a fresh waiter.
    ///
    /// **Order matters.** Register the waiter *before* you trigger the bind
    /// (typically before the `try_send` that kicks the listener isolate). The
    /// command channel is FIFO, so a registration enqueued before the bind
    /// trigger always lands in the registry before the worker processes the
    /// trigger; a registration enqueued after the bind has already completed
    /// will wait for the *next* bind, not the one that just happened.
    ///
    /// If the worker is already stopped, the returned waiter resolves
    /// immediately to [`crate::WaitError::RuntimeStopped`] when `wait` is called —
    /// the waiter itself is the single source of truth for "did this bind
    /// happen", so no extra registration error is surfaced here.
    pub fn observe_next_bound(&self) -> BoundAddressWaiter {
        match self.call(|runtime| runtime.observe_next_bound()) {
            Ok(waiter) => waiter,
            Err(_) => observation::stopped_bound_waiter(),
        }
    }

    /// Registers a typed waiter for the targeted isolate's `IsolateStopped`
    /// event on the worker shard.
    ///
    /// See [`Runtime::observe_isolate_complete`] for semantics. If the worker
    /// is already stopped the returned waiter resolves immediately to
    /// [`crate::WaitError::RuntimeStopped`].
    pub fn observe_isolate_complete<M: 'static, R: 'static>(
        &self,
        address: Address<M, R>,
    ) -> observation::IsolateCompleteWaiter {
        match self.call(move |runtime| runtime.observe_isolate_complete(address)) {
            Ok(waiter) => waiter,
            Err(_) => observation::stopped_isolate_complete_waiter(),
        }
    }

    /// Registers a typed waiter for the next runtime call of `call_kind`
    /// issued by `address` that completes on the worker shard.
    ///
    /// See [`Runtime::observe_operation_done`] for semantics.
    pub fn observe_operation_done<M: 'static, R: 'static>(
        &self,
        address: Address<M, R>,
        call_kind: CallKind,
    ) -> observation::OperationDoneWaiter {
        match self.call(move |runtime| runtime.observe_operation_done(address, call_kind)) {
            Ok(waiter) => waiter,
            Err(_) => observation::stopped_operation_done_waiter(),
        }
    }

    /// Registers a typed waiter for the next supervised restart of any
    /// direct child of `parent_address` on the worker shard.
    ///
    /// See [`Runtime::observe_child_restarted`] for semantics.
    pub fn observe_child_restarted<M: 'static, R: 'static>(
        &self,
        parent_address: Address<M, R>,
    ) -> observation::ChildRestartedWaiter {
        match self.call(move |runtime| runtime.observe_child_restarted(parent_address)) {
            Ok(waiter) => waiter,
            Err(_) => observation::stopped_child_restarted_waiter(),
        }
    }

    /// Registers a typed result waiter for the isolate at `address` on the
    /// worker shard. See [`Runtime::observe_result`] for semantics.
    ///
    /// Worker stopped → `RuntimeStopped`.
    pub fn observe_result<T: Send + 'static, M: 'static, R: 'static>(
        &self,
        address: Address<M, R>,
    ) -> Result<observation::IsolateResultWaiter<T>, observation::ResultWaitError> {
        match self.call(move |runtime| runtime.observe_result::<T, M, R>(address)) {
            Ok(result) => result,
            Err(_) => Err(observation::ResultWaitError::RuntimeStopped),
        }
    }

    /// Attempts one typed ingress handoff through the bounded worker queue.
    ///
    /// Success means the worker accepted ownership of the message command. It
    /// does not mean the target mailbox has accepted the message yet. Mailbox
    /// `Full` / `Closed` outcomes are observed on the worker side through trace
    /// or through [`send_and_observe`](Self::send_and_observe).
    ///
    /// Porting note: this is the fast, fire-and-forget
    /// surface. Unlike [`Runtime::try_send`] (the explicit-step equivalent),
    /// `ThreadedRuntime::try_send`:
    ///
    /// - returns `ThreadedTrySendError`, not `TrySendError<M>`. The
    ///   message is consumed even on `IngressFull`; callers that need to
    ///   recover the message (or distinguish `MailboxFull` from
    ///   `MailboxClosed`) should use [`send_and_observe`](Self::send_and_observe),
    ///   which is the strict, message-recoverable equivalent.
    /// - silently drops messages addressed to a stale or unknown isolate
    ///   on the worker side once the command is accepted. Use
    ///   [`send_and_observe`](Self::send_and_observe) when the host must
    ///   learn that the target was already closed.
    pub fn try_send<M: Send + 'static, R: 'static>(
        &self,
        address: Address<M, R>,
        message: M,
    ) -> Result<(), ThreadedTrySendError> {
        // A Failed worker rejects ingress immediately
        // even before the bounded sync_channel has observed Disconnected,
        // so callers cannot enqueue work into a quarantined shard.
        if self.metrics.state() == LiveShardState::Failed {
            self.metrics.ingress.rejected_closed();
            return Err(ThreadedTrySendError::WorkerStopped);
        }
        let command = ThreadedCommand::Run(Box::new(move |runtime| {
            let _ = runtime.try_send(address, message);
        }));

        match self.commands.try_send(command) {
            Ok(()) => {
                self.metrics.ingress.accepted();
                Ok(())
            }
            Err(std::sync::mpsc::TrySendError::Full(_)) => {
                self.metrics.ingress.rejected_full();
                Err(ThreadedTrySendError::IngressFull)
            }
            Err(std::sync::mpsc::TrySendError::Disconnected(_)) => {
                self.metrics.ingress.rejected_closed();
                self.metrics.set_state(LiveShardState::Failed);
                Err(ThreadedTrySendError::WorkerStopped)
            }
        }
    }

    /// Attempts one typed ingress send and waits for the worker to observe the
    /// target mailbox outcome.
    ///
    /// This is a synchronous control path for tests and setup code that need to
    /// distinguish mailbox `Full` from `Closed`. Ordinary ingress should prefer
    /// [`try_send`](Self::try_send), which only proves bounded handoff.
    pub fn send_and_observe<M: Send + 'static, R: 'static>(
        &self,
        address: Address<M, R>,
        message: M,
    ) -> Result<(), ThreadedSendObservedError> {
        let (reply_tx, reply_rx) = std::sync::mpsc::channel();
        let command = ThreadedCommand::Run(Box::new(move |runtime| {
            let result = runtime
                .try_send(address, message)
                .map_err(|error| match error {
                    TrySendError::Full(_) => ThreadedSendObservedError::MailboxFull,
                    TrySendError::Closed(_) => ThreadedSendObservedError::MailboxClosed,
                });
            let _ = reply_tx.send(result);
        }));

        match self.commands.try_send(command) {
            Ok(()) => {
                self.metrics.ingress.accepted();
                reply_rx
                    .recv()
                    .unwrap_or(Err(ThreadedSendObservedError::WorkerStopped))
            }
            Err(std::sync::mpsc::TrySendError::Full(_)) => {
                self.metrics.ingress.rejected_full();
                Err(ThreadedSendObservedError::IngressFull)
            }
            Err(std::sync::mpsc::TrySendError::Disconnected(_)) => {
                self.metrics.ingress.rejected_closed();
                self.metrics.set_state(LiveShardState::Failed);
                Err(ThreadedSendObservedError::WorkerStopped)
            }
        }
    }

    /// Attempts one typed ingress send and reports the target mailbox outcome
    /// later from the worker thread.
    ///
    /// This preserves the nonblocking bounded-handoff behavior of
    /// [`try_send`](Self::try_send) while still letting bridge code surface
    /// target `Full` / `Closed` instead of degrading those failures into
    /// timeouts.
    ///
    /// The observer runs on the worker thread and must stay nonblocking.
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

    /// Retries [`send_and_observe`](Self::send_and_observe) on
    /// `MailboxFull` / `IngressFull` until the message lands or the
    /// deadline elapses (Phase 062 Rock 4).
    ///
    /// Convention: this is the host-side helper for "control" messages
    /// like `BurstClosed(n)` that travel through the same bounded
    /// mailbox as data. The helper does not introduce a second mailbox
    /// or hidden queue. If the data mailbox is full of admitted work,
    /// the helper waits — which is the honest backpressure shape.
    ///
    /// Outcomes:
    ///
    /// - `Ok(())` — the message was admitted into the mailbox.
    /// - [`SendObservedUntilError::Timeout`] — deadline elapsed while
    ///   still racing a `Full` mailbox / ingress.
    /// - [`SendObservedUntilError::Closed`] — the target isolate's
    ///   mailbox reported closed or stale; not retried.
    /// - [`SendObservedUntilError::WorkerStopped`] — worker thread is
    ///   gone; not retried.
    ///
    /// `make_message` is a closure called once per attempt so the helper
    /// can move ownership through `send_and_observe` without forcing a
    /// `Clone` constraint on `M`.
    ///
    /// **Backoff semantics.** `backoff` is the gap between admission
    /// attempts, not a CPU-spin guard. Each attempt is a worker-thread
    /// roundtrip (the same shape as [`Self::send_and_observe`]); the
    /// helper does not pin a CPU. A `backoff` of `Duration::ZERO`
    /// degenerates to "back-to-back attempts as fast as the worker can
    /// drain commands" — rarely what you want, since it churns command
    /// queue capacity that other ingress could use. Pick a backoff that
    /// reflects how fast the data mailbox actually drains. The helper
    /// always issues at least one attempt before returning `Timeout`,
    /// even if `deadline <= Instant::now()` at entry.
    pub fn send_observed_until<M, R, MakeMsg>(
        &self,
        address: Address<M, R>,
        deadline: Instant,
        backoff: Duration,
        mut make_message: MakeMsg,
    ) -> Result<(), SendObservedUntilError>
    where
        M: Send + 'static,
        R: 'static,
        MakeMsg: FnMut() -> M,
    {
        loop {
            match self.send_and_observe(address, make_message()) {
                Ok(()) => return Ok(()),
                Err(ThreadedSendObservedError::MailboxFull)
                | Err(ThreadedSendObservedError::IngressFull) => {
                    if Instant::now() >= deadline {
                        return Err(SendObservedUntilError::Timeout);
                    }
                    thread::sleep(backoff);
                }
                Err(ThreadedSendObservedError::MailboxClosed) => {
                    return Err(SendObservedUntilError::Closed);
                }
                Err(ThreadedSendObservedError::WorkerStopped) => {
                    return Err(SendObservedUntilError::WorkerStopped);
                }
            }
        }
    }

    /// Attempts one typed ingress send and accumulates the outcome into a
    /// shared [`HostBurstOutcomes`] counter (Phase 062 Rock 3).
    ///
    /// Convenience over [`try_send_and_observe_with`](Self::try_send_and_observe_with):
    /// hides the per-send observer closure, the Arc-cloned counters, and the
    /// manual "observed barrier" the caller used to spell out by hand. Every
    /// truth-typed outcome stays distinct in the snapshot — `MailboxFull`,
    /// `MailboxClosed`, `IngressFull`, `WorkerStopped`, and `admitted` are
    /// counted independently.
    ///
    /// Pattern:
    ///
    /// ```ignore
    /// let outcomes = HostBurstOutcomes::new();
    /// for n in 0..N {
    ///     let _ = runtime.try_send_outcome(addr, Msg::Submit(n), &outcomes);
    /// }
    /// outcomes.wait_complete(deadline)?;
    /// let snap = outcomes.snapshot();
    /// ```
    ///
    /// The return value mirrors `try_send_and_observe_with`: `Ok(())` means
    /// the bounded ingress queue accepted the command (the observer will
    /// fire later on the worker thread); `Err(IngressFull)` /
    /// `Err(WorkerStopped)` mean the host-side handoff failed and the
    /// observer will *not* fire — these errors are also folded into the
    /// shared counters so [`HostBurstOutcomes::wait_complete`] still drains
    /// cleanly.
    ///
    /// **Message ownership.** This helper consumes `message` and does not
    /// return it on rejection — same as the underlying
    /// `try_send_and_observe_with`. If the caller needs to reconstruct or
    /// re-route the message on `MailboxFull` / `IngressFull` / `Closed`,
    /// build it inside a `FnMut() -> M` closure outside this helper, or
    /// use [`Self::send_and_observe`] (which roundtrips through the
    /// worker per call but returns the typed outcome synchronously).
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

    /// Attempts one typed ingress send with a worker-side preflight check.
    ///
    /// The preflight runs on the worker thread immediately before mailbox
    /// admission. It is for already-queued commands that may have become stale
    /// before the worker could observe them; it must stay nonblocking.
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
        let command = ThreadedCommand::Run(Box::new(move |runtime| {
            if let Some(error) = preflight(&message) {
                observer(Err(error));
                return;
            }

            observer(
                runtime
                    .try_send(address, message)
                    .map_err(|error| match error {
                        TrySendError::Full(_) => ThreadedSendObservedError::MailboxFull,
                        TrySendError::Closed(_) => ThreadedSendObservedError::MailboxClosed,
                    }),
            );
        }));

        match self.commands.try_send(command) {
            Ok(()) => {
                self.metrics.ingress.accepted();
                Ok(())
            }
            Err(std::sync::mpsc::TrySendError::Full(_)) => {
                self.metrics.ingress.rejected_full();
                Err(ThreadedTrySendError::IngressFull)
            }
            Err(std::sync::mpsc::TrySendError::Disconnected(_)) => {
                self.metrics.ingress.rejected_closed();
                self.metrics.set_state(LiveShardState::Failed);
                Err(ThreadedTrySendError::WorkerStopped)
            }
        }
    }

    /// Returns retained trace without failing the observability path.
    pub fn trace(&self) -> TraceSnapshot {
        match self.complete_trace() {
            Ok(events) => TraceSnapshot::complete(events),
            Err(ThreadedRuntimeError::WorkerStopped) => TraceSnapshot::partial(
                Vec::new(),
                self.topology()
                    .shards()
                    .iter()
                    .map(|shard| shard.shard())
                    .collect(),
            ),
            Err(_) => TraceSnapshot::partial(
                Vec::new(),
                self.topology()
                    .shards()
                    .iter()
                    .map(|shard| shard.shard())
                    .collect(),
            ),
        }
    }

    /// Returns complete trace, failing if the worker can no longer report.
    pub fn complete_trace(&self) -> Result<Vec<RuntimeEvent>, ThreadedRuntimeError> {
        self.call(|runtime| runtime.trace().to_vec())
    }

    /// Returns a counted summary of pressure-shaped trace events.
    /// See [`Runtime::pressure_summary`].
    pub fn pressure_summary(
        &self,
    ) -> Result<crate::pressure::PressureSummary, ThreadedRuntimeError> {
        self.call(|runtime| runtime.pressure_summary())
    }

    /// Returns whether the worker still has runtime-owned work pending.
    pub fn has_in_flight_calls(&self) -> Result<bool, ThreadedRuntimeError> {
        self.call(|runtime| runtime.has_in_flight_calls())
    }

    /// Returns a handle-owned topology snapshot without probing the worker.
    pub fn topology(&self) -> LiveTopologyReport {
        LiveTopologyReport::single(self.metrics.report())
    }

    /// Returns the live runtime capability table for this worker.
    pub fn capabilities(&self) -> RuntimeCapabilities {
        RuntimeCapabilities::threaded_with_capacities(
            self.metrics.config.storage_lane_capacity,
            self.metrics.config.dns_lane_capacity,
            self.metrics.config.tls_lane_capacity,
            self.metrics.config.process_lane_capacity,
            self.metrics.config.signal_capacity,
        )
    }

    /// Requests shutdown and joins the worker, returning its final trace.
    pub fn shutdown(self) -> Result<Vec<RuntimeEvent>, ThreadedRuntimeError> {
        let report = self.shutdown_report();
        if let Some(error) = report.error() {
            Err(error)
        } else {
            Ok(report.into_trace())
        }
    }

    /// Requests shutdown and joins the worker, always returning terminal truth.
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

    fn call<R, C>(&self, command: C) -> Result<R, ThreadedRuntimeError>
    where
        R: Send + 'static,
        C: FnOnce(&mut Runtime<S, F>) -> R + Send + 'static,
    {
        let (reply_tx, reply_rx) = std::sync::mpsc::channel();
        self.commands
            .send(ThreadedCommand::Run(Box::new(move |runtime| {
                let _ = reply_tx.send(command(runtime));
            })))
            .map_err(|_| {
                self.metrics.set_state(LiveShardState::Failed);
                ThreadedRuntimeError::WorkerStopped
            })?;
        reply_rx.recv().map_err(|_| {
            self.metrics.set_state(LiveShardState::Failed);
            ThreadedRuntimeError::WorkerStopped
        })
    }

    fn shutdown_inner(&mut self) -> Result<Vec<RuntimeEvent>, ThreadedRuntimeError> {
        let (result, trace) = self.shutdown_inner_with_available_trace();
        result.map(|()| trace)
    }

    pub(crate) fn shutdown_inner_with_available_trace(
        &mut self,
    ) -> (Result<(), ThreadedRuntimeError>, Vec<RuntimeEvent>) {
        let Some(handle) = self.handle.take() else {
            return (Ok(()), Vec::new());
        };
        let _ = self.commands.send(ThreadedCommand::Shutdown);
        match handle.join() {
            Ok(exit) => {
                if let Some(error) = exit.error {
                    self.metrics.set_state(LiveShardState::Failed);
                    (Err(error), exit.trace)
                } else {
                    self.metrics.set_state(LiveShardState::Stopped);
                    (Ok(()), exit.trace)
                }
            }
            Err(_) => {
                self.metrics.set_state(LiveShardState::Failed);
                (Err(ThreadedRuntimeError::WorkerStopped), Vec::new())
            }
        }
    }
}

impl<S, F> Drop for ThreadedRuntime<S, F>
where
    S: Shard + Send + 'static,
    F: MailboxFactory + Send + 'static,
{
    fn drop(&mut self) {
        let _ = self.shutdown_inner();
    }
}

pub(crate) fn threaded_worker_loop<S, F>(
    shard: S,
    mailbox_factory: F,
    receiver: std::sync::mpsc::Receiver<ThreadedCommand<S, F>>,
    config: ThreadedRuntimeConfig,
    io_loop_factory: ThreadedIoLoopFactory,
    metrics: Arc<LiveShardMetrics>,
) -> ThreadedWorkerExit
where
    S: Shard,
    F: MailboxFactory,
{
    metrics.set_worker_thread_id(format!("{:?}", thread::current().id()));
    let mut runtime = Runtime::with_clock_and_ids_and_driver_and_preallocation(
        shard,
        mailbox_factory,
        Box::new(MonotonicClock),
        IdSource::new(),
        Box::new(BetelgeuseDriver::with_io_loop_and_capacities(
            io_loop_factory(),
            config.storage_lane_capacity,
            config.dns_lane_capacity,
            config.tls_lane_capacity,
            config.process_lane_capacity,
            config.signal_capacity,
        )),
        config.preallocation,
    );
    runtime.set_trace_retention(config.trace_retention);

    loop {
        metrics.set_resource_counts(runtime.resource_report());
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

        let delivered = runtime.step();
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
    metrics.set_resource_counts(runtime.resource_report());
    let trace = runtime.trace().to_vec();
    if shutdown_result.is_err() {
        return ThreadedWorkerExit::failed(ThreadedRuntimeError::DriverShutdownFailed, trace);
    }
    ThreadedWorkerExit::clean(trace)
}

pub(crate) fn deliver_shutdown_signal_and_drain<S, F>(runtime: &mut Runtime<S, F>)
where
    S: Shard,
    F: MailboxFactory,
{
    runtime.notify_signal("shutdown");
    for _ in 0..1024 {
        if runtime.step() == 0 {
            break;
        }
    }
}
