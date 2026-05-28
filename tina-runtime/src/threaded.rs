//! Threaded single-shard runtime extracted from lib.rs (phase 055).
//!
//! Houses `ThreadedRuntimeConfig`, the `ThreadedRuntime` worker handle,
//! `ThreadedCommand`, `threaded_worker_loop`,
//! `deliver_shutdown_signal_and_drain`, and the
//! `DEFAULT_SHUTDOWN_LANE_DRAIN_TIMEOUT` constant. The worker thread owns one
//! `Runtime` and processes a bounded `mpsc` command queue.

use std::alloc::Global;
use std::convert::Infallible;
use std::marker::PhantomData;
use std::sync::{Arc, mpsc};
use std::thread;
use std::time::{Duration, Instant};

use betelgeuse::{IOLoopHandle, io_loop};
use tina::{Address, Isolate, Outbound as TinaOutbound, Shard, TrySendError};
use tina_supervisor::SupervisorConfig;

use crate::call::{CallOutcome, IntoErasedCall};
use crate::capabilities::RuntimeCapabilities;
use crate::clock::MonotonicClock;
use crate::driver::{self, BetelgeuseDriver};
use crate::errors::{
    SendObservedUntilError, ShutdownWaitError, SuperviseError, ThreadedRegisterBootstrapError,
    ThreadedRuntimeError, ThreadedSendObservedError, ThreadedTrySendError,
};
use crate::host_burst::HostBurstOutcomes;
use crate::host_call_dispatcher::{
    ConcreteHostCallBegin, DispatcherMsg, HostCallDispatcher, HostCallTaskBegin,
};
use crate::live_report::{LiveShardMetrics, LiveShardState, LiveTopologyReport};
use crate::local_system::{
    LocalSystemTerminalReport, ThreadedCommandFn, ThreadedIoLoopFactory, ThreadedWorkerExit,
    TraceSnapshot,
};
use crate::mailbox::MailboxFactory;
use crate::observation::{self, BoundAddressWaiter};
use crate::observer::TraceObserver;
use crate::shutdown::{SharedShutdownState, ShutdownWorker, ThreadedShutdownHandle, handle_for};
use crate::trace::{CallKind, RuntimeEvent};
use crate::{
    ChildLifecycleReport, IdSource, IntoErasedSpawn, IntoErasedSpawnObserved,
    IntoSendErasedSpawnObserved, PreallocationConfig, Runtime, TraceRetention,
};

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

    /// Capacity of the bounded TLS lane. Each in-flight TLS operation owns
    /// one worker thread up to this cap.
    pub tls_lane_capacity: usize,

    /// Capacity of the bounded process lane.
    pub process_lane_capacity: usize,

    /// Capacity of runtime-owned signal waits.
    pub signal_capacity: usize,

    /// OS CPU id to hard-pin this shard worker to.
    ///
    /// `Some(n)` means "pin this worker to OS CPU id `n` if the platform can."
    /// `n` is an OS CPU id checked against the process's allowed affinity mask,
    /// not an index into `0..num_cpus`. On Linux the worker pins with
    /// `sched_setaffinity` and reports [`crate::AffinityStatus::Applied`]; a
    /// core outside the allowed mask reports [`crate::AffinityStatus::Failed`]
    /// and the worker runs unpinned. Platforms without a hard pin (e.g. macOS)
    /// report [`crate::AffinityStatus::Unsupported`]. Helper-lane threads are
    /// never pinned. Default `None` makes no affinity call
    /// ([`crate::AffinityStatus::NotRequested`]).
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

/// Number of persistent host-call dispatcher isolates registered per worker.
///
/// One dispatcher would serialize all concurrent host calls through a single
/// mailbox (and a single isolate, which gets one `step()` delivery per turn).
/// A small pool restores the parallelism the old per-call `HostCallDriver`
/// path got "for free" from having a fresh isolate per call, without paying
/// the per-call registration cost.
///
/// Sized to cover typical concurrent host workloads; concurrency beyond this
/// will round-robin onto already-busy dispatchers and partially serialize,
/// which is the same backpressure shape Tina applies elsewhere.
pub(crate) const HOST_CALL_DISPATCHER_POOL_SIZE: usize = 8;

/// Extra host-side delivery grace used by the legacy one-timeout
/// `call_blocking` wrapper so a target call timeout can be delivered as
/// `CallOutcome::Timeout` instead of racing the host wait timer.
pub const DEFAULT_HOST_CALL_DELIVERY_GRACE: Duration = Duration::from_millis(100);

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
///
/// Lifetime: the runtime value is the owner. Dropping it requests shutdown
/// and joins the worker. Cloneable [`ThreadedShutdownHandle`]s obtained via
/// [`Self::shutdown_handle`] do not extend the runtime's lifetime; they
/// only let host threads request shutdown and observe the cached terminal
/// report without `Arc::try_unwrap(runtime)`.
pub struct ThreadedRuntime<S, F>
where
    S: Shard + Send + 'static,
    F: MailboxFactory + Send + 'static,
{
    commands: std::sync::mpsc::SyncSender<ThreadedCommand<S, F>>,
    /// Pool of persistent host-call dispatchers. Registered once at worker
    /// startup and reused for every `call_blocking`. A single dispatcher
    /// would serialize concurrent host calls (one Begin per isolate per
    /// `step()` turn); a pool of size K lets up to K calls execute their
    /// Begin / Returned handlers in the same turn — same parallelism the
    /// old per-call `HostCallDriver` enjoyed — without the per-call mailbox /
    /// isolate-entry / handler-box allocations.
    ///
    /// Empty means the worker thread died before publishing addresses (e.g.
    /// a panicking mailbox factory): `call_blocking` surfaces `WorkerStopped`
    /// instead of hanging or panicking the constructor, matching the
    /// deferred-failure model the rest of `ThreadedRuntime`'s API uses.
    dispatchers: Arc<Vec<Address<DispatcherMsg<S>, ()>>>,
    /// Round-robin selector for the dispatcher pool. Wrapping atomic add is
    /// cheap and stays correct under concurrent host-thread access; modulo
    /// pool size at read time.
    dispatcher_next: Arc<std::sync::atomic::AtomicUsize>,
    metrics: Arc<LiveShardMetrics>,
    shutdown: Arc<SharedShutdownState<S, F>>,
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

    /// Starts one worker with a live trace observer wired before the
    /// first event. Observer stays out of [`ThreadedRuntimeConfig`] —
    /// config is pure data.
    pub fn with_config_and_trace_observer(
        shard: S,
        mailbox_factory: F,
        config: ThreadedRuntimeConfig,
        observer: Arc<dyn TraceObserver>,
    ) -> Self {
        Self::with_config_observer_and_io_loop_factory(
            shard,
            mailbox_factory,
            config,
            Some(observer),
            || io_loop(Global).expect("failed to initialise Betelgeuse IO loop for tina-runtime"),
        )
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
        Self::with_config_observer_and_io_loop_factory(
            shard,
            mailbox_factory,
            config,
            None,
            io_loop_factory,
        )
    }

    /// [`Self::with_config_and_io_loop_factory`] plus a trace observer.
    /// Reach for this when both seams matter.
    pub fn with_config_observer_and_io_loop_factory<G>(
        shard: S,
        mailbox_factory: F,
        config: ThreadedRuntimeConfig,
        observer: Option<Arc<dyn TraceObserver>>,
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
        let worker_observer = observer;
        // One-shot channel for the worker to publish the persistent host-call
        // dispatcher pool's addresses back to the host once the runtime has
        // registered them. We block briefly on construction so `call_blocking`
        // can use the addresses immediately.
        let (dispatcher_tx, dispatcher_rx) =
            std::sync::mpsc::channel::<Vec<Address<DispatcherMsg<S>, ()>>>();
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
                    worker_observer,
                    dispatcher_tx,
                )
            })
            .expect("failed to spawn Tina threaded worker");
        // `Disconnected` means the worker thread died before it could
        // register the dispatcher pool — surface that to subsequent ops as
        // `WorkerStopped`, don't panic the constructor. `Timeout` means the
        // worker is stuck (registration normally takes microseconds); 5s is
        // far past any plausible setup time, so panic in that case.
        let dispatchers = match dispatcher_rx.recv_timeout(Duration::from_secs(5)) {
            Ok(addrs) => Arc::new(addrs),
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => Arc::new(Vec::new()),
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                panic!("worker failed to publish host-call dispatcher pool in 5s");
            }
        };
        let dispatcher_next = Arc::new(std::sync::atomic::AtomicUsize::new(0));

        let shutdown = Arc::new(SharedShutdownState::single_shard(ShutdownWorker {
            shard: shard_id,
            commands: commands.clone(),
            handle: Some(handle),
            metrics: Arc::clone(&metrics),
            signaled: false,
        }));

        Self {
            commands,
            dispatchers,
            dispatcher_next,
            metrics,
            shutdown,
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
        I::SpawnObserved: IntoErasedSpawnObserved<S, F, I::Message> + 'static,
        I::SpawnObservedRemote: IntoSendErasedSpawnObserved<S, F, I::Message> + 'static,
        I::Call: IntoErasedCall<I::Message> + 'static,
        I::Fact: crate::fact::IntoRuntimeFact + 'static,
        Outbound: 'static,
    {
        self.call(move |runtime| {
            runtime.register_with_capacity::<I, Outbound>(isolate, mailbox_capacity)
        })
    }

    /// Threaded mirror of [`Runtime::register_service`](crate::Runtime::register_service).
    ///
    /// Returns a capability-typed [`ServiceHandle`](crate::ServiceHandle) so
    /// callers see the `.send` and `.call` lanes split at the type boundary.
    /// Requires `I: tina::CallableIsolate`, which the
    /// `#[tina_runtime::isolate]` macro emits automatically when the impl
    /// block defines `fn handle_call(...)`.
    #[allow(private_bounds)]
    pub fn register_service<I, Outbound>(
        &self,
        isolate: I,
        mailbox_capacity: usize,
    ) -> Result<crate::ServiceHandle<I::Message, I::Reply>, ThreadedRuntimeError>
    where
        I: Isolate<Shard = S, Send = TinaOutbound<Outbound>>
            + tina::CallableIsolate
            + Send
            + 'static,
        I::Message: 'static,
        I::Reply: 'static,
        I::Spawn: IntoErasedSpawn<S, F> + 'static,
        I::SpawnObserved: IntoErasedSpawnObserved<S, F, I::Message> + 'static,
        I::SpawnObservedRemote: IntoSendErasedSpawnObserved<S, F, I::Message> + 'static,
        I::Call: IntoErasedCall<I::Message> + 'static,
        I::Fact: crate::fact::IntoRuntimeFact + 'static,
        Outbound: 'static,
    {
        self.register_with_capacity::<I, Outbound>(isolate, mailbox_capacity)
            .map(crate::ServiceHandle::from_address)
    }

    /// Threaded mirror of
    /// [`Runtime::register_service_send_only`](crate::Runtime::register_service_send_only).
    ///
    /// Returns a [`SendOnlyServiceHandle`](crate::SendOnlyServiceHandle) with
    /// only the `.send` lane. The isolate must have `Reply = ()` so no caller
    /// can construct a callable handle in the first place.
    #[allow(private_bounds)]
    pub fn register_service_send_only<I, Outbound>(
        &self,
        isolate: I,
        mailbox_capacity: usize,
    ) -> Result<crate::SendOnlyServiceHandle<I::Message>, ThreadedRuntimeError>
    where
        I: Isolate<Shard = S, Send = TinaOutbound<Outbound>, Reply = ()> + Send + 'static,
        I::Message: 'static,
        I::Spawn: IntoErasedSpawn<S, F> + 'static,
        I::SpawnObserved: IntoErasedSpawnObserved<S, F, I::Message> + 'static,
        I::SpawnObservedRemote: IntoSendErasedSpawnObserved<S, F, I::Message> + 'static,
        I::Call: IntoErasedCall<I::Message> + 'static,
        I::Fact: crate::fact::IntoRuntimeFact + 'static,
        Outbound: 'static,
    {
        self.register_with_capacity::<I, Outbound>(isolate, mailbox_capacity)
            .map(|address| crate::SendOnlyServiceHandle {
                send: address.send_only(),
            })
    }

    /// Threaded mirror of
    /// [`Runtime::register_split_service`](crate::Runtime::register_split_service).
    #[allow(private_bounds)]
    pub fn register_split_service<I, Event, Request, Outbound>(
        &self,
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
        I::Reply: 'static,
        I::Spawn: IntoErasedSpawn<S, F> + 'static,
        I::SpawnObserved: IntoErasedSpawnObserved<S, F, I::Message> + 'static,
        I::SpawnObservedRemote: IntoSendErasedSpawnObserved<S, F, I::Message> + 'static,
        I::Call: IntoErasedCall<I::Message> + 'static,
        I::Fact: crate::fact::IntoRuntimeFact + 'static,
        Outbound: 'static,
    {
        self.register_with_capacity::<I, Outbound>(isolate, mailbox_capacity)
            .map(crate::SplitServiceHandle::from_address)
    }

    /// Threaded mirror of [`Runtime::register_with_capacity_and_bootstrap`].
    ///
    /// The mailbox is allocated and the bootstrap message is prefilled before
    /// the isolate entry is inserted. The returned address points at an
    /// isolate whose first delivered message is `bootstrap`. Sends issued
    /// immediately after this call can still see `Full` until the bootstrap
    /// message is consumed; that is honest pressure, not a bug.
    #[allow(private_bounds, clippy::type_complexity)]
    pub fn register_with_capacity_and_bootstrap<I, Outbound>(
        &self,
        isolate: I,
        mailbox_capacity: usize,
        bootstrap: I::Message,
    ) -> Result<Address<I::Message, I::Reply>, ThreadedRegisterBootstrapError<I::Message>>
    where
        I: Isolate<Shard = S, Send = TinaOutbound<Outbound>> + Send + 'static,
        I::Message: Send + 'static,
        I::Reply: 'static,
        I::Spawn: IntoErasedSpawn<S, F> + 'static,
        I::SpawnObserved: IntoErasedSpawnObserved<S, F, I::Message> + 'static,
        I::SpawnObservedRemote: IntoSendErasedSpawnObserved<S, F, I::Message> + 'static,
        I::Call: IntoErasedCall<I::Message> + 'static,
        I::Fact: crate::fact::IntoRuntimeFact + 'static,
        Outbound: 'static,
    {
        match self.call(move |runtime| {
            runtime.register_with_capacity_and_bootstrap::<I, Outbound>(
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
            Err(ThreadedRuntimeError::UnknownShard(shard)) => {
                Err(ThreadedRegisterBootstrapError::UnknownShard(shard))
            }
            Err(ThreadedRuntimeError::DriverShutdownFailed)
            | Err(ThreadedRuntimeError::CommandFull)
            | Err(ThreadedRuntimeError::HostWaitTimeout) => {
                // `call` is blocking-admission, so `CommandFull` is
                // unreachable today. Map defensively in case the inner
                // helper is ever migrated.
                Err(ThreadedRegisterBootstrapError::WorkerStopped)
            }
        }
    }

    /// Threaded mirror of [`Runtime::register_with_capacity_using`].
    ///
    /// Constructor runs on the worker thread; caller blocks on the
    /// worker's reply. Heavy work in `construct` blocks every other
    /// isolate on the shard for the duration — build the value before
    /// calling. `Ctor: Send + 'static` so the closure ships across the
    /// worker command channel.
    #[allow(private_bounds)]
    pub fn register_with_capacity_using<I, Outbound, Ctor>(
        &self,
        mailbox_capacity: usize,
        construct: Ctor,
    ) -> Result<Address<I::Message, I::Reply>, ThreadedRuntimeError>
    where
        I: Isolate<Shard = S, Send = TinaOutbound<Outbound>> + Send + 'static,
        I::Message: 'static,
        I::Reply: 'static,
        I::Spawn: IntoErasedSpawn<S, F> + 'static,
        I::SpawnObserved: IntoErasedSpawnObserved<S, F, I::Message> + 'static,
        I::SpawnObservedRemote: IntoSendErasedSpawnObserved<S, F, I::Message> + 'static,
        I::Call: IntoErasedCall<I::Message> + 'static,
        I::Fact: crate::fact::IntoRuntimeFact + 'static,
        Outbound: 'static,
        Ctor: FnOnce(Address<I::Message, I::Reply>) -> I + Send + 'static,
    {
        self.call(move |runtime| {
            runtime.register_with_capacity_using::<I, Outbound, _>(mailbox_capacity, construct)
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

    /// Mirrors [`Self::observe_next_bound`] for `tls_bind`.
    pub fn observe_next_tls_bound(&self) -> BoundAddressWaiter {
        match self.call(|runtime| runtime.observe_next_tls_bound()) {
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

    /// Returns a live child lifecycle report from the worker shard.
    pub fn child_lifecycle_report<M: 'static, R: 'static>(
        &self,
        parent_address: Address<M, R>,
    ) -> Result<ChildLifecycleReport, ThreadedRuntimeError> {
        self.call(move |runtime| runtime.child_lifecycle_report(parent_address))
            .and_then(|report| report.map_err(|_| ThreadedRuntimeError::WorkerStopped))
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

    /// Attempts one public event send through a split-service event
    /// capability.
    ///
    /// This is the threaded host companion to [`tina::send_event`]. It avoids
    /// the raw `ServiceMessage<Event, Request>` escape hatch in copied tests
    /// and setup code.
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

    /// Sends one public event through a split-service event capability and
    /// waits for the worker to observe the target mailbox outcome.
    ///
    /// This is the split-service companion to
    /// [`send_and_observe`](Self::send_and_observe). Use it in host-driven
    /// setup/tests when `Full` / `Closed` must stay visible.
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

    /// Retries public event admission through a split-service event capability
    /// until the event lands or the deadline elapses.
    ///
    /// This is the split-service companion to
    /// [`send_observed_until`](Self::send_observed_until).
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
    /// roundtrip (same shape as [`Self::send_and_observe`]); the
    /// helper does not pin a CPU. A `backoff` of `Duration::ZERO`
    /// degenerates to "back-to-back attempts as fast as the worker can
    /// drain commands" — rarely what you want, since it churns command
    /// queue capacity that other ingress could use. Pick a backoff that
    /// reflects how fast the data mailbox actually drains.
    ///
    /// **Worker-observation latency is bounded by the deadline.** Each
    /// attempt waits for the worker to admit the command using
    /// `recv_timeout(remaining)`, so a stuck or slow worker cannot
    /// extend the call past `deadline`. A worker that accepts the
    /// command but does not observe the mailbox outcome before the
    /// deadline elapses surfaces as
    /// [`SendObservedUntilError::Timeout`].
    ///
    /// **Past deadline.** If `deadline <= Instant::now()` at entry, the
    /// helper returns `Timeout` immediately without enqueueing a
    /// command. That avoids the race where a "must finish by" deadline
    /// causes the helper to deliver the message *and* report `Timeout`,
    /// leaving the caller unsure whether the side effect happened.
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
            // Past-deadline check up-front: don't enqueue a command we
            // can't bound. Avoids the "delivered but reported Timeout"
            // race that an unbounded `recv` would mask.
            let now = Instant::now();
            if now >= deadline {
                return Err(SendObservedUntilError::Timeout);
            }
            let remaining = deadline.saturating_duration_since(now);

            // Worker rejects ingress to a quarantined shard immediately,
            // matching `try_send` / `send_and_observe`.
            if self.metrics.state() == LiveShardState::Failed {
                self.metrics.ingress.rejected_closed();
                return Err(SendObservedUntilError::WorkerStopped);
            }

            let (reply_tx, reply_rx) = std::sync::mpsc::channel();
            let message = make_message();
            let command = ThreadedCommand::Run(Box::new(move |runtime| {
                let result = runtime
                    .try_send(address, message)
                    .map_err(|error| match error {
                        TrySendError::Full(_) => ThreadedSendObservedError::MailboxFull,
                        TrySendError::Closed(_) => ThreadedSendObservedError::MailboxClosed,
                    });
                let _ = reply_tx.send(result);
            }));

            let outcome = match self.commands.try_send(command) {
                Ok(()) => {
                    self.metrics.ingress.accepted();
                    match reply_rx.recv_timeout(remaining) {
                        Ok(result) => result,
                        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                            // Worker accepted the command but didn't
                            // observe the mailbox outcome in time.
                            return Err(SendObservedUntilError::Timeout);
                        }
                        Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                            self.metrics.set_state(LiveShardState::Failed);
                            return Err(SendObservedUntilError::WorkerStopped);
                        }
                    }
                }
                Err(std::sync::mpsc::TrySendError::Full(_)) => {
                    self.metrics.ingress.rejected_full();
                    Err(ThreadedSendObservedError::IngressFull)
                }
                Err(std::sync::mpsc::TrySendError::Disconnected(_)) => {
                    self.metrics.ingress.rejected_closed();
                    self.metrics.set_state(LiveShardState::Failed);
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
                    // Don't oversleep past the deadline: the next
                    // attempt should still get at least one shot.
                    let remaining = deadline.saturating_duration_since(now);
                    thread::sleep(backoff.min(remaining));
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

    /// Performs one typed isolate call from the host thread and waits for its
    /// ordinary [`CallOutcome`].
    ///
    /// This is a host convenience for tests, specimens, and setup code that
    /// would otherwise need a one-off driver isolate just to issue
    /// `call(address, message, timeout).then(...)`. It still uses the normal
    /// Tina call path internally: `Full`, `Closed`, and `Timeout` stay visible
    /// as [`CallOutcome`] values, and accepted work is not cancelled by
    /// dropping the host-side wait.
    ///
    /// # When to use
    ///
    /// - **Tests and specimens** that need to drive one call and inspect the
    ///   result.
    /// - **Setup code** that registers a service and then sends an initial
    ///   message before handing control to a larger system.
    ///
    /// **Do not call from inside an isolate handler.** Handlers must stay
    /// synchronous and non-blocking; use `call(...).then(...)` instead.
    ///
    /// # Example
    ///
    /// ```rust,no_run
    /// use std::time::Duration;
    /// use tina::prelude::*;
    /// use tina_runtime::{CallOutcome, ThreadedRuntime, ThreadedRuntimeConfig};
    ///
    /// # fn example<M, R>(runtime: &ThreadedRuntime<SingleShard, tina_runtime::DefaultThreadedMailboxFactory>, addr: Address<M, R>, msg: M)
    /// # where M: Send + 'static, R: Send + 'static,
    /// # {
    /// let outcome = runtime.call_blocking(addr, msg, Duration::from_secs(2));
    /// match outcome {
    ///     Ok(CallOutcome::Replied(reply)) => { /* use reply */ }
    ///     Ok(CallOutcome::Full) => { /* target mailbox was full */ }
    ///     Ok(CallOutcome::Closed) => { /* target isolate had stopped */ }
    ///     Ok(CallOutcome::Timeout) => { /* call deadline fired */ }
    ///     Ok(CallOutcome::Rejected(_)) => { /* target rejected this call shape */ }
    ///     Err(_) => { /* worker thread stopped or command queue was full */ }
    /// }
    /// # }
    /// ```
    ///
    /// The `timeout` argument is the *call* timeout (how long the target
    /// isolate has to reply). The legacy host-side wait adds a short delivery
    /// grace so target timeouts can arrive as `CallOutcome::Timeout`. Use
    /// [`call_blocking_with_host_timeout`](Self::call_blocking_with_host_timeout)
    /// when host wait budget and target deadline must be distinct.
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
            .checked_add(DEFAULT_HOST_CALL_DELIVERY_GRACE)
            .unwrap_or(timeout);
        self.call_blocking_with_host_timeout(address, message, timeout, host_wait_timeout)
    }

    /// Like [`call_blocking`](Self::call_blocking), but gives the host wait its
    /// own budget separate from the target call deadline.
    ///
    /// `target_timeout` is delivered into Tina and controls when the call
    /// becomes `CallOutcome::Timeout`. `host_wait_timeout` controls how long
    /// this OS thread waits for the driver result. If the host wait expires
    /// first, the target call is still governed by `target_timeout` and this
    /// method returns [`ThreadedRuntimeError::HostWaitTimeout`].
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
        // If the worker died before publishing the dispatcher pool (e.g. a
        // panicking mailbox factory blew up registration), the runtime has
        // no usable host-call path. Surface that as `WorkerStopped` instead of
        // dispatching into the void.
        if self.dispatchers.is_empty() {
            self.metrics.set_state(LiveShardState::Failed);
            return Err(ThreadedRuntimeError::WorkerStopped);
        }
        // Round-robin across the dispatcher pool. Wrapping atomic add stays
        // correct under concurrent host-thread access; modulo at read time.
        let idx = self
            .dispatcher_next
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            % self.dispatchers.len();
        let dispatcher_addr = self.dispatchers[idx];
        let (reply_tx, reply_rx) = mpsc::channel();
        // Type-erase the per-call task and hand it to the persistent dispatcher
        // — no per-call isolate registration, no per-call mailbox/handler box.
        let begin: Box<dyn HostCallTaskBegin<S>> = Box::new(ConcreteHostCallBegin::<S, M, R> {
            target: address,
            message,
            timeout: target_timeout,
            sender: reply_tx,
            _marker: PhantomData,
        });
        let command = ThreadedCommand::Run(Box::new(move |runtime| {
            // The dispatcher mailbox is sized to `command_capacity`, matching
            // the command-queue bound; in normal use it cannot be full while
            // the command queue had room. A full or closed dispatcher mailbox
            // drops `begin` here, which drops the sender, which surfaces to
            // the host as `Disconnected` -> `WorkerStopped` below.
            let _ = runtime.try_send(dispatcher_addr, DispatcherMsg::Begin(begin));
        }));
        match self.commands.try_send(command) {
            Ok(()) => {}
            Err(std::sync::mpsc::TrySendError::Full(_)) => {
                return Err(ThreadedRuntimeError::CommandFull);
            }
            Err(std::sync::mpsc::TrySendError::Disconnected(_)) => {
                self.metrics.set_state(LiveShardState::Failed);
                return Err(ThreadedRuntimeError::WorkerStopped);
            }
        }

        match reply_rx.recv_timeout(host_wait_timeout) {
            Ok(outcome) => Ok(outcome),
            Err(mpsc::RecvTimeoutError::Timeout) => Err(ThreadedRuntimeError::HostWaitTimeout),
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                self.metrics.set_state(LiveShardState::Failed);
                Err(ThreadedRuntimeError::WorkerStopped)
            }
        }
    }

    /// Capability-typed [`call_blocking`](Self::call_blocking) that accepts only
    /// a [`tina::CallAddress`].
    ///
    /// Passing a [`tina::SendAddress`] or the `.send` lane of a
    /// [`ServiceHandle`](crate::ServiceHandle) is a compile error.
    pub fn call_blocking_typed<M, R>(
        &self,
        address: tina::CallAddress<M, R>,
        message: M,
        timeout: Duration,
    ) -> Result<CallOutcome<R>, ThreadedRuntimeError>
    where
        M: Send + 'static,
        R: Send + 'static,
    {
        self.call_blocking(address.address(), message, timeout)
    }

    /// Blocking host call through a split-service request capability.
    ///
    /// This is the threaded host companion to [`crate::call_request`]. It
    /// wraps the request in [`tina::ServiceMessage::Request`] and keeps host
    /// code from reaching for the raw split envelope address.
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

    /// Returns a cloneable handle that controls runtime-level shutdown
    /// without consuming the runtime value.
    ///
    /// Host threads can call [`ThreadedShutdownHandle::request_shutdown`]
    /// and [`ThreadedShutdownHandle::wait_report`] from any thread without
    /// the `Arc::try_unwrap(runtime)` ceremony that consuming
    /// [`Self::shutdown_report`] requires when the runtime is shared
    /// behind an `Arc`.
    ///
    /// Dropping the handle does **not** trigger shutdown. The runtime
    /// owner still controls lifetime. Service-level drain (admit/quiesce
    /// on app-level `Stop`/`Drain` messages) stays the service's
    /// responsibility; this handle only asks the runtime/control plane
    /// to begin shutdown.
    pub fn shutdown_handle(&self) -> ThreadedShutdownHandle {
        handle_for(&self.shutdown)
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
}

impl<S, F> Drop for ThreadedRuntime<S, F>
where
    S: Shard + Send + 'static,
    F: MailboxFactory + Send + 'static,
{
    fn drop(&mut self) {
        self.shutdown.shutdown_blocking();
        let _ = self
            .shutdown
            .wait_report_for_owner_with_timeout(DEFAULT_SHUTDOWN_LANE_DRAIN_TIMEOUT);
    }
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn threaded_worker_loop<S, F>(
    shard: S,
    mailbox_factory: F,
    receiver: std::sync::mpsc::Receiver<ThreadedCommand<S, F>>,
    config: ThreadedRuntimeConfig,
    io_loop_factory: ThreadedIoLoopFactory,
    metrics: Arc<LiveShardMetrics>,
    observer: Option<Arc<dyn TraceObserver>>,
    dispatcher_tx: std::sync::mpsc::Sender<Vec<Address<DispatcherMsg<S>, ()>>>,
) -> ThreadedWorkerExit
where
    S: Shard + Send + 'static,
    F: MailboxFactory + 'static,
{
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
    // Wire the observer before any event records.
    runtime.set_trace_observer(observer);

    // Register the persistent host-call dispatcher pool and publish their
    // addresses back to the host so `call_blocking` can target them without
    // a per-call isolate registration (phase 145 Rock 5). Each dispatcher's
    // mailbox is sized to `command_capacity`: at peak, an in-flight host
    // call routed to a given dispatcher occupies at most one slot in that
    // dispatcher's mailbox (Begin pending or Returned pending, never both),
    // matching the command-queue backpressure bound.
    let mut dispatcher_addrs = Vec::with_capacity(HOST_CALL_DISPATCHER_POOL_SIZE);
    for _ in 0..HOST_CALL_DISPATCHER_POOL_SIZE {
        let addr = runtime.register_with_capacity::<HostCallDispatcher<S>, Infallible>(
            HostCallDispatcher::new(),
            config.command_capacity,
        );
        dispatcher_addrs.push(addr);
    }
    // If the host hung up between spawn and registration we just drop the
    // addresses; the worker continues to run normally and the host will
    // surface its own panic via the recv_timeout on its side.
    let _ = dispatcher_tx.send(dispatcher_addrs);
    drop(dispatcher_tx);

    // Pin this worker (if requested and the platform can) only after the driver
    // has spawned its helper lanes above, so those lanes inherit the unpinned
    // mask and float onto spare cores. Pin before the loop so a report that
    // names the worker carries its proven pin outcome.
    let affinity = crate::affinity::apply(config.configured_core);
    metrics.publish_worker_start(format!("{:?}", thread::current().id()), affinity);

    // Refresh the live resource snapshot on idle and command turns, but not
    // after a fast delivery turn: recomputing the O(pending) resource report on
    // every hot turn is the per-op tax this phase removes. Counts refresh again
    // as soon as the worker parks or runs a command (phase 145).
    let mut refresh_metrics = true;
    loop {
        if refresh_metrics {
            metrics.set_resource_counts(runtime.resource_report());
        }
        refresh_metrics = true;
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

        // Drain everything the shard can make progress on right now. A step
        // that delivered handlers loops back immediately and steps again —
        // no per-turn sleep tax. A tiny local call needs several runtime
        // turns; throttling each turn by 1ms turned microseconds of work into
        // milliseconds of latency (phase 145).
        let delivered = runtime.step();
        if delivered > 0 {
            refresh_metrics = false;
            continue;
        }

        // Nothing was deliverable this turn. Park on the command queue so a
        // new command (including shutdown) wakes the worker immediately. The
        // bounded `idle_wait` timeout makes the worker re-poll the driver for
        // runtime-owned work (timers, lane I/O completions, listeners) it
        // cannot be signalled about, without hot-spinning the CPU. This single
        // policy covers both "fully idle" and "waiting on a pending timer/I/O":
        // in both cases the only honest move is to sleep until either a command
        // arrives or it is time to re-poll the driver.
        match receiver.recv_timeout(config.idle_wait) {
            Ok(ThreadedCommand::Run(command)) => command(&mut runtime),
            Ok(ThreadedCommand::Shutdown) => {
                deliver_shutdown_signal_and_drain(&mut runtime);
                break;
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
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
    S: Shard + 'static,
    F: MailboxFactory + 'static,
{
    runtime.notify_signal("shutdown");
    for _ in 0..1024 {
        if runtime.step() == 0 {
            break;
        }
    }
}
