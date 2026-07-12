#![feature(allocator_api)]

use std::alloc::Global;
use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::convert::Infallible;
use std::io;
use std::net::SocketAddr;
use std::path::Path;
use std::sync::{Arc, Mutex, mpsc};
use std::thread;
use std::time::{Duration, Instant};

use betelgeuse::{
    AcceptCompletion, AcceptOp, ConnectCompletion, IO, IOFile, IOLoop, IOLoopHandle, IOSocket,
    OpenOptions, Operation, RecvBufCompletion, RecvCompletion, SendCompletion, SendOwnedCompletion,
    io::simulated::SimulatedIO,
};
use tina::{CallContext, Mailbox, TrySendError, prelude::*};
use tina_runtime::{
    CallCompletionRejectedReason, CallInput, CallKind, CallOutcome, CallOutput,
    DriverRuntimeRequirement, ListenerId, MailboxFactory, RuntimeCall, RuntimeEvent,
    RuntimeEventKind, SendRejectedReason, TINA_DRIVER_RUNTIME_CONTRACT, ThreadedMultiShardRuntime,
    ThreadedRuntime, ThreadedRuntimeConfig, ThreadedRuntimeConfigError, ThreadedRuntimeError,
    TraceRetention, call, sleep,
};

#[derive(Debug, Default)]
struct TestShard;

impl Shard for TestShard {
    fn id(&self) -> ShardId {
        ShardId::new(61)
    }
}

struct TestMailbox<T> {
    capacity: usize,
    queue: RefCell<VecDeque<T>>,
    closed: Cell<bool>,
}

impl<T> TestMailbox<T> {
    fn new(capacity: usize) -> Self {
        Self {
            capacity,
            queue: RefCell::new(VecDeque::new()),
            closed: Cell::new(false),
        }
    }
}

impl<T> Mailbox<T> for TestMailbox<T> {
    fn capacity(&self) -> usize {
        self.capacity
    }

    fn try_send(&self, message: T) -> Result<(), TrySendError<T>> {
        if self.closed.get() {
            return Err(TrySendError::Closed(message));
        }
        let mut queue = self.queue.borrow_mut();
        if queue.len() >= self.capacity {
            return Err(TrySendError::Full(message));
        }
        queue.push_back(message);
        Ok(())
    }

    fn recv(&self) -> Option<T> {
        self.queue.borrow_mut().pop_front()
    }
    fn is_empty(&self) -> bool {
        self.queue.borrow().is_empty()
    }

    fn close(&self) {
        self.closed.set(true);
    }
}

#[derive(Debug, Clone, Copy)]
struct TestMailboxFactory;

impl MailboxFactory for TestMailboxFactory {
    fn create<T: 'static>(&self, capacity: usize) -> Box<dyn Mailbox<T>> {
        Box::new(TestMailbox::new(capacity))
    }
}

#[test]
fn threaded_runtime_rejects_zero_remote_inbound_drain_budget() {
    let error = ThreadedRuntime::try_with_config(
        TestShard,
        TestMailboxFactory,
        ThreadedRuntimeConfig {
            remote_inbound_drain_budget: 0,
            ..ThreadedRuntimeConfig::default()
        },
    )
    .err()
    .expect("zero budget must fail");
    assert!(matches!(
        error,
        tina_runtime::StartupError::InvalidThreadedConfig(
            ThreadedRuntimeConfigError::ZeroRemoteInboundDrainBudget
        )
    ));
}

#[test]
fn threaded_runtime_rejects_zero_driver_lane_capacities() {
    macro_rules! assert_zero_capacity_error {
        ($field:ident, $expected:pat) => {{
            let error = ThreadedRuntime::try_with_config_and_io_loop_factory(
                TestShard,
                TestMailboxFactory,
                ThreadedRuntimeConfig {
                    $field: 0,
                    ..ThreadedRuntimeConfig::default()
                },
                || panic!("I/O loop factory should not run after invalid config"),
            )
            .err()
            .expect("zero capacity must fail before worker start");
            assert!(matches!(
                error,
                tina_runtime::StartupError::InvalidThreadedConfig($expected)
            ));
        }};
    }

    assert_zero_capacity_error!(
        dns_lane_capacity,
        ThreadedRuntimeConfigError::ZeroDnsLaneCapacity
    );
    assert_zero_capacity_error!(
        tls_lane_capacity,
        ThreadedRuntimeConfigError::ZeroTlsLaneCapacity
    );
    assert_zero_capacity_error!(
        process_lane_capacity,
        ThreadedRuntimeConfigError::ZeroProcessLaneCapacity
    );
    assert_zero_capacity_error!(
        signal_capacity,
        ThreadedRuntimeConfigError::ZeroSignalCapacity
    );
}

fn wait_until<F>(timeout: Duration, label: &str, mut predicate: F)
where
    F: FnMut() -> bool,
{
    let deadline = Instant::now() + timeout;
    while !predicate() {
        if Instant::now() > deadline {
            panic!("wait_until({label}): predicate not satisfied within timeout");
        }
        thread::yield_now();
    }
}

fn count_event(trace: &[RuntimeEvent], predicate: impl Fn(&RuntimeEventKind) -> bool) -> usize {
    trace
        .iter()
        .filter(|event| predicate(&event.kind()))
        .count()
}

#[test]
fn tina_driver_runtime_contract_names_core_substrate_without_general_runtime_claim() {
    let contract = TINA_DRIVER_RUNTIME_CONTRACT;

    assert_eq!(
        contract.completion_based_io,
        DriverRuntimeRequirement::Required
    );
    assert_eq!(
        contract.bounded_runtime_commands,
        DriverRuntimeRequirement::Required
    );
    assert_eq!(
        contract.explicit_cancellation,
        DriverRuntimeRequirement::Required
    );
    assert_eq!(contract.owned_shutdown, DriverRuntimeRequirement::Required);
    assert_eq!(
        contract.explicit_progress,
        DriverRuntimeRequirement::Required
    );
    assert_eq!(
        contract.deterministic_simulation,
        DriverRuntimeRequirement::Required
    );
    assert_eq!(
        contract.hidden_executor_tasks,
        DriverRuntimeRequirement::Forbidden
    );
    assert_eq!(
        contract.general_async_executor,
        DriverRuntimeRequirement::NotClaimed
    );
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RetryMsg {
    TryWork,
    RetryNow,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RetryObservation {
    Attempted(usize),
    Failed(usize),
    BackoffElapsed,
    Succeeded(usize),
}

#[derive(Debug)]
struct RetryWorker {
    backoff: Duration,
    attempts: usize,
    observations: Arc<Mutex<Vec<RetryObservation>>>,
}

impl Isolate for RetryWorker {
    tina::isolate_types! {
        message: RetryMsg,
        reply: (),
        send: Outbound<RetryMsg>,
        spawn: Infallible,
        io: RuntimeCall<RetryMsg>,
        shard: TestShard,
    }

    fn handle(
        &mut self,
        msg: Self::Message,
        ctx: &mut Context<'_, Self::Shard, Self::Reply>,
    ) -> Effect<Self> {
        match msg {
            RetryMsg::TryWork => {
                self.attempts += 1;
                self.observations
                    .lock()
                    .expect("observations mutex")
                    .push(RetryObservation::Attempted(self.attempts));
                if self.attempts == 1 {
                    self.observations
                        .lock()
                        .expect("observations mutex")
                        .push(RetryObservation::Failed(self.attempts));
                    sleep(self.backoff).then(|_| RetryMsg::RetryNow)
                } else {
                    self.observations
                        .lock()
                        .expect("observations mutex")
                        .push(RetryObservation::Succeeded(self.attempts));
                    noop()
                }
            }
            RetryMsg::RetryNow => {
                self.observations
                    .lock()
                    .expect("observations mutex")
                    .push(RetryObservation::BackoffElapsed);
                ctx.send_self(RetryMsg::TryWork)
            }
        }
    }
}

#[test]
fn threaded_runtime_timer_retry_runs_without_manual_stepping() {
    let observations = Arc::new(Mutex::new(Vec::new()));
    let runtime = ThreadedRuntime::with_config(
        TestShard,
        TestMailboxFactory,
        ThreadedRuntimeConfig {
            command_capacity: 8,
            idle_wait: Duration::from_millis(1),
            ..Default::default()
        },
    );
    let worker = runtime
        .register_with_capacity::<RetryWorker, _>(
            RetryWorker {
                backoff: Duration::from_millis(5),
                attempts: 0,
                observations: Arc::clone(&observations),
            },
            8,
        )
        .expect("Betelgeuse register accepts");

    runtime
        .try_send(worker, RetryMsg::TryWork)
        .expect("retry handoff accepted");

    wait_until(Duration::from_secs(2), "Betelgeuse retry", || {
        observations.lock().expect("observations mutex").as_slice()
            == [
                RetryObservation::Attempted(1),
                RetryObservation::Failed(1),
                RetryObservation::BackoffElapsed,
                RetryObservation::Attempted(2),
                RetryObservation::Succeeded(2),
            ]
    });

    let trace = runtime.shutdown().expect("Betelgeuse shutdown");
    assert_eq!(
        count_event(&trace, |kind| matches!(
            kind,
            RuntimeEventKind::CallCompleted {
                call_kind: CallKind::Sleep,
                ..
            }
        )),
        1
    );
}

#[test]
fn threaded_runtime_honors_bounded_trace_retention() {
    let observations = Arc::new(Mutex::new(Vec::new()));
    let runtime = ThreadedRuntime::with_config(
        TestShard,
        TestMailboxFactory,
        ThreadedRuntimeConfig {
            command_capacity: 8,
            trace_retention: TraceRetention::Bounded(5),
            idle_wait: Duration::from_millis(1),
            ..Default::default()
        },
    );
    let worker = runtime
        .register_with_capacity::<RetryWorker, _>(
            RetryWorker {
                backoff: Duration::from_millis(1),
                attempts: 0,
                observations: Arc::clone(&observations),
            },
            8,
        )
        .expect("worker register accepts");

    runtime
        .try_send(worker, RetryMsg::TryWork)
        .expect("worker ingress accepts");
    wait_until(Duration::from_secs(1), "retry completed", || {
        observations
            .lock()
            .expect("observations mutex")
            .contains(&RetryObservation::Succeeded(2))
    });

    let trace = runtime.trace();
    assert!(trace.is_partial());
    assert!(!trace.is_complete());
    assert!(trace.dropped_events() > 0);
    assert_eq!(
        trace.clone().complete_events(),
        Err(ThreadedRuntimeError::WorkerStopped)
    );
    let events = trace.events();
    assert_eq!(events.len(), 5);
    assert!(events.first().expect("retained first event").id().get() > 1);
    assert!(events.windows(2).all(|pair| pair[0].id() < pair[1].id()));
    let _ = runtime.shutdown().expect("runtime shutdown");
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LongTimerMsg {
    Start,
    Finished,
}

#[derive(Debug)]
struct LongTimer;

impl Isolate for LongTimer {
    tina::isolate_types! {
        message: LongTimerMsg,
        reply: (),
        send: Outbound<Infallible>,
        spawn: Infallible,
        io: RuntimeCall<LongTimerMsg>,
        shard: TestShard,
    }

    fn handle(
        &mut self,
        msg: Self::Message,
        _ctx: &mut Context<'_, Self::Shard, Self::Reply>,
    ) -> Effect<Self> {
        match msg {
            LongTimerMsg::Start => sleep(Duration::from_secs(60)).then(|_| LongTimerMsg::Finished),
            LongTimerMsg::Finished => noop(),
        }
    }
}

#[test]
fn threaded_runtime_shutdown_rejects_outstanding_timer_completion() {
    let runtime = ThreadedRuntime::with_config(
        TestShard,
        TestMailboxFactory,
        ThreadedRuntimeConfig {
            command_capacity: 8,
            idle_wait: Duration::from_millis(1),
            ..Default::default()
        },
    );
    let timer = runtime
        .register_with_capacity::<LongTimer, _>(LongTimer, 8)
        .expect("timer register accepts");

    runtime
        .try_send(timer, LongTimerMsg::Start)
        .expect("timer start handoff accepted");

    wait_until(Duration::from_secs(2), "Betelgeuse timer pending", || {
        runtime
            .has_in_flight_calls()
            .expect("in-flight query succeeds")
    });

    let trace = runtime.shutdown().expect("Betelgeuse shutdown");
    assert!(trace.iter().any(|event| {
        event.isolate() == timer.isolate()
            && matches!(
                event.kind(),
                RuntimeEventKind::CallCompletionRejected {
                    call_kind: CallKind::Sleep,
                    reason: CallCompletionRejectedReason::RequesterClosed,
                    ..
                }
            )
    }));
}

#[derive(Debug, Clone)]
enum TcpAcceptMsg {
    Bind,
    Bound {
        listener: ListenerId,
        addr: SocketAddr,
    },
    StartAccept,
    Accepted,
    Failed,
}

#[derive(Debug)]
struct TcpAcceptWorker {
    bind_addr: SocketAddr,
    listener: Option<ListenerId>,
    published: Arc<Mutex<Option<SocketAddr>>>,
    observed: Arc<Mutex<Vec<TcpAcceptMsg>>>,
}

impl Isolate for TcpAcceptWorker {
    tina::isolate_types! {
        message: TcpAcceptMsg,
        reply: (),
        send: Outbound<Infallible>,
        spawn: Infallible,
        io: RuntimeCall<TcpAcceptMsg>,
        shard: TestShard,
    }

    fn handle(
        &mut self,
        msg: Self::Message,
        _ctx: &mut Context<'_, Self::Shard, Self::Reply>,
    ) -> Effect<Self> {
        match msg {
            TcpAcceptMsg::Bind => Effect::Io(RuntimeCall::new(
                CallInput::TcpBind {
                    addr: self.bind_addr,
                },
                |result| match result {
                    CallOutput::TcpBound {
                        listener,
                        local_addr,
                    } => TcpAcceptMsg::Bound {
                        listener,
                        addr: local_addr,
                    },
                    other => panic!("expected TcpBound, got {other:?}"),
                },
            )),
            TcpAcceptMsg::Bound { listener, addr } => {
                self.listener = Some(listener);
                *self.published.lock().expect("published addr mutex") = Some(addr);
                noop()
            }
            TcpAcceptMsg::StartAccept => Effect::Io(RuntimeCall::new(
                CallInput::TcpAccept {
                    listener: self.listener.expect("listener bound before accept"),
                },
                |result| match result {
                    CallOutput::TcpAccepted { .. } => TcpAcceptMsg::Accepted,
                    CallOutput::Failed(_) => TcpAcceptMsg::Failed,
                    other => panic!("unexpected accept result {other:?}"),
                },
            )),
            TcpAcceptMsg::Accepted | TcpAcceptMsg::Failed => {
                self.observed.lock().expect("observed mutex").push(msg);
                noop()
            }
        }
    }
}

#[test]
fn threaded_runtime_shutdown_rejects_outstanding_tcp_accept_completion() {
    let simulated_io = SimulatedIO::new();
    let published = Arc::new(Mutex::new(None));
    let observed = Arc::new(Mutex::new(Vec::new()));
    let runtime = {
        let simulated_io = simulated_io.clone();
        ThreadedRuntime::with_config_and_io_loop_factory(
            TestShard,
            TestMailboxFactory,
            ThreadedRuntimeConfig {
                command_capacity: 8,
                idle_wait: Duration::from_millis(1),
                ..Default::default()
            },
            move || simulated_io.loop_handle(Global),
        )
    };
    let worker = runtime
        .register_with_capacity::<TcpAcceptWorker, _>(
            TcpAcceptWorker {
                bind_addr: "127.0.0.1:0".parse().expect("bind addr"),
                listener: None,
                published: Arc::clone(&published),
                observed: Arc::clone(&observed),
            },
            8,
        )
        .expect("tcp worker register accepts");

    runtime
        .try_send(worker, TcpAcceptMsg::Bind)
        .expect("bind handoff accepted");
    wait_until(Duration::from_secs(2), "tcp bind published", || {
        published.lock().expect("published addr mutex").is_some()
    });

    runtime
        .try_send(worker, TcpAcceptMsg::StartAccept)
        .expect("accept handoff accepted");
    wait_until(Duration::from_secs(2), "tcp accept pending", || {
        runtime
            .has_in_flight_calls()
            .expect("in-flight query succeeds")
    });

    let trace = runtime.shutdown().expect("Betelgeuse shutdown");
    for _ in 0..3 {
        simulated_io
            .step()
            .expect("external simulated I/O step after shutdown stays safe");
    }
    assert!(
        observed.lock().expect("observed mutex").is_empty(),
        "shutdown must not deliver translated accept completion"
    );
    assert!(trace.iter().any(|event| {
        event.isolate() == worker.isolate()
            && matches!(
                event.kind(),
                RuntimeEventKind::CallCompletionRejected {
                    call_kind: CallKind::TcpAccept,
                    reason: CallCompletionRejectedReason::RequesterClosed,
                    ..
                }
            )
    }));
}

#[test]
fn native_threaded_zero_drain_shutdown_is_safe_and_bounded() {
    let published = Arc::new(Mutex::new(None));
    let observed = Arc::new(Mutex::new(Vec::new()));
    let runtime = ThreadedRuntime::with_config(
        TestShard,
        TestMailboxFactory,
        ThreadedRuntimeConfig {
            command_capacity: 8,
            idle_wait: Duration::from_millis(1),
            shutdown_lane_drain_timeout: Duration::ZERO,
            ..Default::default()
        },
    );
    let worker = runtime
        .register_with_capacity::<TcpAcceptWorker, _>(
            TcpAcceptWorker {
                bind_addr: "127.0.0.1:0".parse().expect("bind addr"),
                listener: None,
                published: Arc::clone(&published),
                observed: Arc::clone(&observed),
            },
            8,
        )
        .expect("tcp worker register accepts");

    runtime
        .try_send(worker, TcpAcceptMsg::Bind)
        .expect("bind handoff accepted");
    wait_until(Duration::from_secs(2), "native tcp bind published", || {
        published.lock().expect("published addr mutex").is_some()
    });
    runtime
        .try_send(worker, TcpAcceptMsg::StartAccept)
        .expect("accept handoff accepted");
    wait_until(Duration::from_secs(2), "native tcp accept pending", || {
        runtime
            .has_in_flight_calls()
            .expect("in-flight query succeeds")
    });

    let shutdown_started = Instant::now();
    let report = runtime.shutdown_report();
    let shutdown_elapsed = shutdown_started.elapsed();
    assert!(
        shutdown_elapsed < Duration::from_secs(2),
        "zero-budget shutdown took {shutdown_elapsed:?}"
    );
    #[cfg(target_os = "linux")]
    // A locally queued accept cancels synchronously; an io_uring-submitted
    // accept may require the bounded quarantine path.
    assert!(
        matches!(
            report.error(),
            None | Some(ThreadedRuntimeError::DriverShutdownFailed)
        ),
        "zero-budget Linux shutdown returned unexpected error: {:?}",
        report.error()
    );
    #[cfg(target_os = "macos")]
    assert_eq!(
        report.error(),
        None,
        "kqueue deletes the watch synchronously, so zero-budget shutdown should reclaim immediately"
    );
    assert!(
        observed.lock().expect("observed mutex").is_empty(),
        "shutdown must not translate the cancelled accept"
    );
}

#[derive(Clone)]
struct StuckReleaseIo {
    state: Arc<Mutex<StuckReleaseState>>,
}

impl Default for StuckReleaseIo {
    fn default() -> Self {
        Self {
            state: Arc::new(Mutex::new(StuckReleaseState::default())),
        }
    }
}

#[derive(Default)]
struct StuckReleaseState {
    local_addr: Option<SocketAddr>,
    pending_completion_count: usize,
    close_count: usize,
    cancel_error: bool,
}

impl StuckReleaseIo {
    fn loop_handle(&self) -> IOLoopHandle<Global> {
        IOLoopHandle::new(std::rc::Rc::new(self.clone()), Global)
    }
}

#[derive(Clone)]
struct StuckReleaseSocket {
    state: Arc<Mutex<StuckReleaseState>>,
}

impl IOSocket for StuckReleaseSocket {
    fn bind(&self, mut addr: SocketAddr) -> io::Result<()> {
        if addr.port() == 0 {
            addr.set_port(41_911);
        }
        self.state.lock().expect("stuck io mutex").local_addr = Some(addr);
        Ok(())
    }

    fn local_addr(&self) -> io::Result<SocketAddr> {
        self.state
            .lock()
            .expect("stuck io mutex")
            .local_addr
            .ok_or_else(|| io::Error::new(io::ErrorKind::AddrNotAvailable, "not bound"))
    }

    fn peer_addr(&self) -> io::Result<SocketAddr> {
        Err(io::Error::new(io::ErrorKind::NotConnected, "no peer"))
    }

    fn accept(&self, c: &mut AcceptCompletion) -> io::Result<()> {
        c.inner_mut()
            .prepare(Operation::Accept(AcceptOp { fd: 41_911 }));
        self.state
            .lock()
            .expect("stuck io mutex")
            .pending_completion_count += 1;
        Ok(())
    }

    fn connect(&self, _c: &mut ConnectCompletion, _addr: SocketAddr) -> io::Result<()> {
        Err(io::Error::new(io::ErrorKind::Unsupported, "stuck connect"))
    }

    fn bind_unix(&self, _path: &std::path::Path) -> io::Result<()> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "stuck unix bind",
        ))
    }

    fn connect_unix(&self, _c: &mut ConnectCompletion, _path: &std::path::Path) -> io::Result<()> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "stuck unix connect",
        ))
    }

    fn recv(&self, _c: &mut RecvCompletion, _len: usize) -> io::Result<()> {
        Err(io::Error::new(io::ErrorKind::Unsupported, "stuck recv"))
    }

    fn recv_buf(
        &self,
        _c: &mut RecvBufCompletion,
        buffer: Vec<u8>,
        _max_len: usize,
    ) -> Result<(), (io::Error, Vec<u8>)> {
        Err((
            io::Error::new(io::ErrorKind::Unsupported, "stuck recv_buf"),
            buffer,
        ))
    }

    fn send(&self, _c: &mut SendCompletion, _buf: Vec<u8>) -> io::Result<()> {
        Err(io::Error::new(io::ErrorKind::Unsupported, "stuck send"))
    }

    fn send_owned(
        &self,
        _c: &mut SendOwnedCompletion,
        buf: Vec<u8>,
    ) -> Result<(), (io::Error, Vec<u8>)> {
        Err((
            io::Error::new(io::ErrorKind::Unsupported, "stuck send_owned"),
            buf,
        ))
    }

    fn send_owned_from(
        &self,
        _c: &mut SendOwnedCompletion,
        buf: Vec<u8>,
        _start: usize,
    ) -> Result<(), (io::Error, Vec<u8>)> {
        Err((
            io::Error::new(io::ErrorKind::Unsupported, "stuck send_owned_from"),
            buf,
        ))
    }

    fn set_nodelay(&self, _on: bool) -> io::Result<()> {
        Ok(())
    }

    fn close(&self) {
        self.state.lock().expect("stuck io mutex").close_count += 1;
    }
}

impl IO for StuckReleaseIo {
    fn open(&self, _path: &Path, _options: OpenOptions) -> io::Result<Box<dyn IOFile>> {
        Err(io::Error::new(io::ErrorKind::Unsupported, "stuck open"))
    }

    fn socket(&self) -> io::Result<Box<dyn IOSocket>> {
        Ok(Box::new(StuckReleaseSocket {
            state: Arc::clone(&self.state),
        }))
    }

    fn mkdir(
        &self,
        _c: &mut betelgeuse::MkdirCompletion,
        _path: &Path,
        _mode: u32,
    ) -> io::Result<()> {
        Err(io::Error::new(io::ErrorKind::Unsupported, "stuck mkdir"))
    }

    fn backend_name(&self) -> &'static str {
        "stuck-release-test"
    }
}

impl IOLoop for StuckReleaseIo {
    fn step(&self) -> io::Result<bool> {
        Ok(false)
    }

    fn pending_completion_count(&self) -> usize {
        self.state
            .lock()
            .expect("stuck io mutex")
            .pending_completion_count
    }

    fn cancel_pending_completions(&self) -> io::Result<()> {
        if self.state.lock().expect("stuck io mutex").cancel_error {
            return Err(io::Error::other("test backend refused cancellation"));
        }
        Ok(())
    }
}

#[test]
fn threaded_runtime_shutdown_reports_driver_release_failure() {
    let stuck_io = StuckReleaseIo::default();
    let io_for_worker = stuck_io.clone();
    let runtime = ThreadedRuntime::with_config_and_io_loop_factory(
        TestShard,
        TestMailboxFactory,
        ThreadedRuntimeConfig {
            command_capacity: 8,
            idle_wait: Duration::from_millis(1),
            ..Default::default()
        },
        move || io_for_worker.loop_handle(),
    );
    let published = Arc::new(Mutex::new(None));
    let observed = Arc::new(Mutex::new(Vec::new()));
    let worker = runtime
        .register_with_capacity::<TcpAcceptWorker, _>(
            TcpAcceptWorker {
                bind_addr: "127.0.0.1:0".parse().expect("loopback parse"),
                published: Arc::clone(&published),
                listener: None,
                observed,
            },
            8,
        )
        .expect("Betelgeuse register accepts");

    runtime
        .try_send(worker, TcpAcceptMsg::Bind)
        .expect("bind handoff accepted");
    wait_until(Duration::from_secs(2), "stuck bind published", || {
        published.lock().expect("published addr mutex").is_some()
    });
    runtime
        .try_send(worker, TcpAcceptMsg::StartAccept)
        .expect("accept handoff accepted");
    wait_until(Duration::from_secs(2), "stuck accept pending", || {
        runtime
            .has_in_flight_calls()
            .expect("in-flight query succeeds")
    });

    assert_eq!(
        runtime.shutdown(),
        Err(ThreadedRuntimeError::DriverShutdownFailed)
    );
}

#[test]
fn threaded_runtime_shutdown_report_keeps_trace_on_driver_release_failure() {
    let stuck_io = StuckReleaseIo::default();
    let io_for_worker = stuck_io.clone();
    let runtime = ThreadedRuntime::with_config_and_io_loop_factory(
        TestShard,
        TestMailboxFactory,
        ThreadedRuntimeConfig {
            command_capacity: 8,
            idle_wait: Duration::from_millis(1),
            ..Default::default()
        },
        move || io_for_worker.loop_handle(),
    );
    let published = Arc::new(Mutex::new(None));
    let observed = Arc::new(Mutex::new(Vec::new()));
    let worker = runtime
        .register_with_capacity::<TcpAcceptWorker, _>(
            TcpAcceptWorker {
                bind_addr: "127.0.0.1:0".parse().expect("loopback parse"),
                published: Arc::clone(&published),
                listener: None,
                observed,
            },
            8,
        )
        .expect("Betelgeuse register accepts");

    runtime
        .try_send(worker, TcpAcceptMsg::Bind)
        .expect("bind handoff accepted");
    wait_until(Duration::from_secs(2), "stuck bind published", || {
        published.lock().expect("published addr mutex").is_some()
    });
    runtime
        .try_send(worker, TcpAcceptMsg::StartAccept)
        .expect("accept handoff accepted");
    wait_until(Duration::from_secs(2), "stuck accept pending", || {
        runtime
            .has_in_flight_calls()
            .expect("in-flight query succeeds")
    });

    let report = runtime.shutdown_report();
    assert_eq!(
        report.error(),
        Some(ThreadedRuntimeError::DriverShutdownFailed)
    );
    assert!(
        !report.trace().is_empty(),
        "failed low-level shutdown report must retain trace collected before driver failure"
    );
    assert!(
        report.shutdown_report().unclean_reason().is_some(),
        "failed low-level shutdown report must keep unclean accounting"
    );
}

#[test]
fn threaded_runtime_shutdown_still_closes_resources_when_backend_cancel_fails() {
    let stuck_io = StuckReleaseIo::default();
    stuck_io.state.lock().expect("stuck io mutex").cancel_error = true;
    let io_for_worker = stuck_io.clone();
    let runtime = ThreadedRuntime::with_config_and_io_loop_factory(
        TestShard,
        TestMailboxFactory,
        ThreadedRuntimeConfig {
            command_capacity: 8,
            idle_wait: Duration::from_millis(1),
            ..Default::default()
        },
        move || io_for_worker.loop_handle(),
    );
    let published = Arc::new(Mutex::new(None));
    let observed = Arc::new(Mutex::new(Vec::new()));
    let worker = runtime
        .register_with_capacity::<TcpAcceptWorker, _>(
            TcpAcceptWorker {
                bind_addr: "127.0.0.1:0".parse().expect("loopback parse"),
                published: Arc::clone(&published),
                listener: None,
                observed,
            },
            8,
        )
        .expect("Betelgeuse register accepts");

    runtime
        .try_send(worker, TcpAcceptMsg::Bind)
        .expect("bind handoff accepted");
    wait_until(Duration::from_secs(2), "stuck bind published", || {
        published.lock().expect("published addr mutex").is_some()
    });
    runtime
        .try_send(worker, TcpAcceptMsg::StartAccept)
        .expect("accept handoff accepted");
    wait_until(Duration::from_secs(2), "stuck accept pending", || {
        runtime
            .has_in_flight_calls()
            .expect("in-flight query succeeds")
    });

    assert_eq!(
        runtime.shutdown(),
        Err(ThreadedRuntimeError::DriverShutdownFailed)
    );
    assert!(
        stuck_io.state.lock().expect("stuck io mutex").close_count > 0,
        "resource close must run even when backend cancel reports failure"
    );
}

#[derive(Debug, Clone, Copy)]
struct PanickingMailboxFactory;

impl MailboxFactory for PanickingMailboxFactory {
    fn create<T: 'static>(&self, _capacity: usize) -> Box<dyn Mailbox<T>> {
        panic!("test mailbox factory panic")
    }
}

#[derive(Debug, Clone, Copy)]
struct CapacityPanicMailboxFactory {
    panic_capacity: usize,
}

impl MailboxFactory for CapacityPanicMailboxFactory {
    fn create<T: 'static>(&self, capacity: usize) -> Box<dyn Mailbox<T>> {
        if capacity == self.panic_capacity {
            panic!("test mailbox factory panic for capacity {capacity}");
        }
        Box::new(TestMailbox::new(capacity))
    }
}

#[test]
fn threaded_runtime_worker_panic_is_a_startup_error() {
    let error = ThreadedRuntime::try_new(TestShard, PanickingMailboxFactory)
        .err()
        .expect("panicking mailbox factory must fail startup");

    assert!(matches!(
        error,
        tina_runtime::StartupError::WorkerStartupPanicked { shard, ref message }
            if shard == ShardId::new(61) && message.contains("test mailbox factory panic")
    ));
}

#[derive(Debug, Clone, Copy)]
enum DriverMsg {
    FillTwice,
}

#[derive(Debug, Clone, Copy)]
enum SinkMsg {
    Hit,
}

#[derive(Debug)]
struct Driver {
    sink: Address<SinkMsg>,
}

impl Isolate for Driver {
    tina::isolate_types! {
        message: DriverMsg,
        reply: (),
        send: Outbound<SinkMsg>,
        spawn: Infallible,
        io: RuntimeCall<DriverMsg>,
        shard: TestShard,
    }

    fn handle(
        &mut self,
        msg: Self::Message,
        _ctx: &mut Context<'_, Self::Shard, Self::Reply>,
    ) -> Effect<Self> {
        match msg {
            DriverMsg::FillTwice => {
                batch([send(self.sink, SinkMsg::Hit), send(self.sink, SinkMsg::Hit)])
            }
        }
    }
}

#[derive(Debug)]
struct Sink;

impl Isolate for Sink {
    tina::isolate_types! {
        message: SinkMsg,
        reply: (),
        send: Outbound<Infallible>,
        spawn: Infallible,
        io: RuntimeCall<SinkMsg>,
        shard: TestShard,
    }

    fn handle(
        &mut self,
        _msg: Self::Message,
        _ctx: &mut Context<'_, Self::Shard, Self::Reply>,
    ) -> Effect<Self> {
        noop()
    }
}

#[test]
fn threaded_runtime_local_mailbox_full_is_visible_in_trace() {
    let runtime = ThreadedRuntime::new(TestShard, TestMailboxFactory);
    let sink = runtime
        .register_with_capacity::<Sink, _>(Sink, 1)
        .expect("sink register accepts");
    let driver = runtime
        .register_with_capacity::<Driver, _>(Driver { sink }, 8)
        .expect("driver register accepts");

    runtime
        .try_send(driver, DriverMsg::FillTwice)
        .expect("driver handoff accepted");

    wait_until(Duration::from_secs(2), "Betelgeuse local full", || {
        let trace = runtime.trace();
        assert!(trace.is_complete());
        trace.events().iter().any(|event| {
            matches!(
                event.kind(),
                RuntimeEventKind::SendRejected {
                    reason: SendRejectedReason::Full,
                    ..
                }
            )
        })
    });

    let trace = runtime.shutdown().expect("Betelgeuse shutdown");
    assert_eq!(
        count_event(&trace, |kind| matches!(
            kind,
            RuntimeEventKind::SendRejected {
                reason: SendRejectedReason::Full,
                ..
            }
        )),
        1
    );
}

#[derive(Debug, Clone, Copy)]
struct WorkShard(u32);

impl Shard for WorkShard {
    fn id(&self) -> ShardId {
        ShardId::new(self.0)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CoordinatorMsg {
    Submit { job_id: u64, value: u64 },
    SubmitAfterBadRemote { job_id: u64, value: u64 },
    Completed { job_id: u64, doubled: u64 },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WorkerMsg {
    Run {
        job_id: u64,
        value: u64,
        reply_to: Address<CoordinatorMsg>,
    },
}

#[derive(Debug)]
struct Coordinator {
    worker: Address<WorkerMsg>,
    bad_worker: Option<Address<WorkerMsg>>,
    completed: Arc<Mutex<Vec<(u64, u64)>>>,
}

impl Isolate for Coordinator {
    tina::isolate_types! {
        message: CoordinatorMsg,
        reply: (),
        send: Outbound<WorkerMsg>,
        spawn: Infallible,
        io: RuntimeCall<CoordinatorMsg>,
        shard: WorkShard,
    }

    fn handle(
        &mut self,
        msg: Self::Message,
        ctx: &mut Context<'_, Self::Shard, Self::Reply>,
    ) -> Effect<Self> {
        match msg {
            CoordinatorMsg::Submit { job_id, value } => send(
                self.worker,
                WorkerMsg::Run {
                    job_id,
                    value,
                    reply_to: ctx.me(),
                },
            ),
            CoordinatorMsg::SubmitAfterBadRemote { job_id, value } => batch([
                send(
                    self.bad_worker.expect("bad worker address configured"),
                    WorkerMsg::Run {
                        job_id: 0,
                        value: 0,
                        reply_to: ctx.me(),
                    },
                ),
                send(
                    self.worker,
                    WorkerMsg::Run {
                        job_id,
                        value,
                        reply_to: ctx.me(),
                    },
                ),
            ]),
            CoordinatorMsg::Completed { job_id, doubled } => {
                self.completed
                    .lock()
                    .expect("completed mutex")
                    .push((job_id, doubled));
                noop()
            }
        }
    }
}

#[derive(Debug)]
struct Worker;

impl Isolate for Worker {
    tina::isolate_types! {
        message: WorkerMsg,
        reply: (),
        send: Outbound<CoordinatorMsg>,
        spawn: Infallible,
        io: RuntimeCall<WorkerMsg>,
        shard: WorkShard,
    }

    fn handle(
        &mut self,
        msg: Self::Message,
        _ctx: &mut Context<'_, Self::Shard, Self::Reply>,
    ) -> Effect<Self> {
        match msg {
            WorkerMsg::Run {
                job_id,
                value,
                reply_to,
            } => send(
                reply_to,
                CoordinatorMsg::Completed {
                    job_id,
                    doubled: value * 2,
                },
            ),
        }
    }
}

#[derive(Debug)]
struct WorkSink;

impl Isolate for WorkSink {
    tina::isolate_types! {
        message: SinkMsg,
        reply: (),
        send: Outbound<Infallible>,
        spawn: Infallible,
        io: RuntimeCall<SinkMsg>,
        shard: WorkShard,
    }

    fn handle(
        &mut self,
        _msg: Self::Message,
        _ctx: &mut Context<'_, Self::Shard, Self::Reply>,
    ) -> Effect<Self> {
        noop()
    }
}

fn count_send_rejected_full(trace: &[RuntimeEvent]) -> usize {
    count_event(trace, |kind| {
        matches!(
            kind,
            RuntimeEventKind::SendRejected {
                reason: SendRejectedReason::Full,
                ..
            }
        )
    })
}

fn has_send_accepted_between(trace: &[RuntimeEvent], from: u32, to: u32) -> bool {
    trace.iter().any(|event| {
        event.shard() == ShardId::new(from)
            && matches!(
                event.kind(),
                RuntimeEventKind::SendAccepted { target_shard, .. }
                    if target_shard == ShardId::new(to)
            )
    })
}

#[test]
fn threaded_multishard_dispatcher_round_trips_between_worker_threads() {
    let runtime = ThreadedMultiShardRuntime::with_config(
        [WorkShard(1), WorkShard(2)],
        TestMailboxFactory,
        ThreadedRuntimeConfig {
            command_capacity: 8,
            idle_wait: Duration::from_millis(1),
            ..Default::default()
        },
    );
    let completed = Arc::new(Mutex::new(Vec::new()));
    let worker = runtime
        .register_with_capacity_on::<Worker, _>(ShardId::new(2), Worker, 8)
        .expect("worker register accepts");
    let coordinator = runtime
        .register_with_capacity_on::<Coordinator, _>(
            ShardId::new(1),
            Coordinator {
                worker,
                bad_worker: None,
                completed: Arc::clone(&completed),
            },
            8,
        )
        .expect("coordinator register accepts");

    runtime
        .try_send(
            coordinator,
            CoordinatorMsg::Submit {
                job_id: 7,
                value: 21,
            },
        )
        .expect("submit handoff accepted");

    wait_until(
        Duration::from_secs(2),
        "Betelgeuse multishard dispatch",
        || completed.lock().expect("completed mutex").as_slice() == [(7, 42)],
    );

    let trace = runtime.shutdown().expect("Betelgeuse multishard shutdown");
    assert!(has_send_accepted_between(&trace, 1, 2));
    assert!(has_send_accepted_between(&trace, 2, 1));
    assert_eq!(count_send_rejected_full(&trace), 0);
}

#[test]
fn threaded_multishard_rejects_zero_storage_lane_capacity() {
    let error = ThreadedMultiShardRuntime::try_with_config(
        [WorkShard(1), WorkShard(2)],
        TestMailboxFactory,
        ThreadedRuntimeConfig {
            storage_lane_capacity: 0,
            ..Default::default()
        },
    )
    .err()
    .expect("zero capacity must fail");
    assert!(matches!(
        error,
        tina_runtime::StartupError::InvalidThreadedConfig(
            ThreadedRuntimeConfigError::ZeroStorageLaneCapacity
        )
    ));
}

#[test]
fn threaded_multishard_rejects_zero_driver_lane_capacities() {
    macro_rules! assert_zero_capacity_error {
        ($field:ident, $expected:pat) => {{
            let error = ThreadedMultiShardRuntime::try_with_config(
                [WorkShard(1), WorkShard(2)],
                TestMailboxFactory,
                ThreadedRuntimeConfig {
                    $field: 0,
                    ..ThreadedRuntimeConfig::default()
                },
            )
            .err()
            .expect("zero capacity must fail before worker start");
            assert!(matches!(
                error,
                tina_runtime::StartupError::InvalidThreadedConfig($expected)
            ));
        }};
    }

    assert_zero_capacity_error!(
        dns_lane_capacity,
        ThreadedRuntimeConfigError::ZeroDnsLaneCapacity
    );
    assert_zero_capacity_error!(
        tls_lane_capacity,
        ThreadedRuntimeConfigError::ZeroTlsLaneCapacity
    );
    assert_zero_capacity_error!(
        process_lane_capacity,
        ThreadedRuntimeConfigError::ZeroProcessLaneCapacity
    );
    assert_zero_capacity_error!(
        signal_capacity,
        ThreadedRuntimeConfigError::ZeroSignalCapacity
    );
}

#[test]
fn threaded_multishard_bad_remote_does_not_poison_good_remote_work() {
    let runtime = ThreadedMultiShardRuntime::new([WorkShard(1), WorkShard(2)], TestMailboxFactory);
    let completed = Arc::new(Mutex::new(Vec::new()));
    let worker = runtime
        .register_with_capacity_on::<Worker, _>(ShardId::new(2), Worker, 8)
        .expect("worker register accepts");
    let bad_worker = Address::new(ShardId::new(2), IsolateId::new(99));
    let coordinator = runtime
        .register_with_capacity_on::<Coordinator, _>(
            ShardId::new(1),
            Coordinator {
                worker,
                bad_worker: Some(bad_worker),
                completed: Arc::clone(&completed),
            },
            8,
        )
        .expect("coordinator register accepts");

    runtime
        .try_send(
            coordinator,
            CoordinatorMsg::SubmitAfterBadRemote {
                job_id: 11,
                value: 5,
            },
        )
        .expect("submit handoff accepted");

    wait_until(
        Duration::from_secs(2),
        "Betelgeuse bad then good remote",
        || completed.lock().expect("completed mutex").as_slice() == [(11, 10)],
    );

    let trace = runtime.shutdown().expect("Betelgeuse multishard shutdown");
    assert!(trace.iter().any(|event| {
        event.isolate() == IsolateId::new(99)
            && matches!(
                event.kind(),
                RuntimeEventKind::SendRejected {
                    reason: SendRejectedReason::Closed,
                    ..
                }
            )
    }));
    assert!(has_send_accepted_between(&trace, 2, 1));
}

#[test]
fn threaded_multishard_shutdown_report_keeps_trace_after_one_worker_fails() {
    let runtime = ThreadedMultiShardRuntime::with_config(
        [WorkShard(1), WorkShard(2)],
        CapacityPanicMailboxFactory { panic_capacity: 13 },
        ThreadedRuntimeConfig {
            command_capacity: 8,
            idle_wait: Duration::from_millis(1),
            ..Default::default()
        },
    );
    let sink = runtime
        .register_with_capacity_on::<WorkSink, _>(ShardId::new(2), WorkSink, 8)
        .expect("healthy shard register accepts");
    runtime
        .try_send(sink, SinkMsg::Hit)
        .expect("healthy shard handoff accepted");
    wait_until(
        Duration::from_secs(2),
        "healthy shard records trace before sibling failure",
        || {
            runtime
                .trace_on(ShardId::new(2))
                .expect("healthy shard trace")
                .iter()
                .any(|event| {
                    event.shard() == ShardId::new(2)
                        && matches!(event.kind(), RuntimeEventKind::HandlerFinished { .. })
                })
        },
    );

    assert!(matches!(
        runtime.register_with_capacity_on::<WorkSink, _>(ShardId::new(1), WorkSink, 13),
        Err(ThreadedRuntimeError::WorkerStopped)
    ));
    assert_eq!(
        runtime.complete_trace(),
        Err(ThreadedRuntimeError::WorkerStopped)
    );
    let trace = runtime.trace();
    assert!(trace.is_partial());
    assert!(
        trace
            .events()
            .iter()
            .any(|event| event.shard() == ShardId::new(2)),
        "best-effort trace should retain healthy shard events after sibling failure"
    );
    assert_eq!(trace.missing_shards(), &[ShardId::new(1)]);

    let report = runtime.shutdown_report();
    assert_eq!(report.error(), Some(ThreadedRuntimeError::WorkerStopped));
    assert!(
        report
            .trace()
            .iter()
            .any(|event| event.shard() == ShardId::new(2)),
        "failed low-level multishard report must retain healthy shard trace"
    );
    let topology = report.topology().expect("terminal topology");
    assert_eq!(
        topology
            .shard(ShardId::new(1))
            .expect("failed shard")
            .state(),
        tina_runtime::LiveShardState::Failed
    );
    assert_eq!(
        topology
            .shard(ShardId::new(2))
            .expect("healthy shard")
            .state(),
        tina_runtime::LiveShardState::Stopped
    );
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CallTargetMsg {
    Ask,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct CallReply;

#[derive(Debug)]
struct CallTarget {
    hits: Arc<Mutex<usize>>,
}

impl Isolate for CallTarget {
    tina::isolate_types! {
        message: CallTargetMsg,
        reply: CallReply,
        send: Outbound<Infallible>,
        spawn: Infallible,
        io: RuntimeCall<CallTargetMsg>,
        shard: WorkShard,
    }

    fn handle(
        &mut self,
        msg: Self::Message,
        _ctx: &mut Context<'_, Self::Shard, Self::Reply>,
    ) -> Effect<Self> {
        match msg {
            CallTargetMsg::Ask => {
                *self.hits.lock().expect("hits mutex") += 1;
                reply(CallReply)
            }
        }
    }

    fn handle_call(&mut self, msg: Self::Message, call: CallContext<'_, Self>) -> Effect<Self> {
        match msg {
            CallTargetMsg::Ask => {
                *self.hits.lock().expect("hits mutex") += 1;
                call.reply(CallReply)
            }
        }
    }
}

#[derive(Debug)]
enum CallClientMsg {
    Start,
    Returned(CallOutcome<CallReply>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CallObservation {
    Replied,
    Full,
    Closed,
    Rejected,
    Timeout,
}

#[derive(Debug)]
struct CallClient {
    target: Address<CallTargetMsg, CallReply>,
    observations: Arc<Mutex<Vec<CallObservation>>>,
}

impl Isolate for CallClient {
    tina::isolate_types! {
        message: CallClientMsg,
        reply: (),
        send: Outbound<Infallible>,
        spawn: Infallible,
        io: RuntimeCall<CallClientMsg>,
        shard: WorkShard,
    }

    fn handle(
        &mut self,
        msg: Self::Message,
        _ctx: &mut Context<'_, Self::Shard, Self::Reply>,
    ) -> Effect<Self> {
        match msg {
            CallClientMsg::Start => call(self.target, CallTargetMsg::Ask, Duration::from_secs(1))
                .then(CallClientMsg::Returned),
            CallClientMsg::Returned(outcome) => {
                let observation = match outcome {
                    CallOutcome::Replied(_) => CallObservation::Replied,
                    CallOutcome::Full => CallObservation::Full,
                    CallOutcome::Closed => CallObservation::Closed,
                    CallOutcome::Rejected(_) => CallObservation::Rejected,
                    CallOutcome::Timeout => CallObservation::Timeout,
                };
                self.observations
                    .lock()
                    .expect("observations mutex")
                    .push(observation);
                noop()
            }
        }
    }
}

#[test]
fn threaded_multishard_isolate_call_round_trips_cross_shard_with_typed_outcome() {
    let runtime = ThreadedMultiShardRuntime::new([WorkShard(1), WorkShard(2)], TestMailboxFactory);
    let target_hits = Arc::new(Mutex::new(0));
    let observations = Arc::new(Mutex::new(Vec::new()));
    let target = runtime
        .register_with_capacity_on::<CallTarget, _>(
            ShardId::new(2),
            CallTarget {
                hits: Arc::clone(&target_hits),
            },
            8,
        )
        .expect("target register accepts");
    let client = runtime
        .register_with_capacity_on::<CallClient, _>(
            ShardId::new(1),
            CallClient {
                target,
                observations: Arc::clone(&observations),
            },
            8,
        )
        .expect("client register accepts");

    runtime
        .try_send(client, CallClientMsg::Start)
        .expect("call start handoff accepted");

    wait_until(
        Duration::from_secs(2),
        "Betelgeuse cross-shard call reply",
        || {
            observations.lock().expect("observations mutex").as_slice()
                == [CallObservation::Replied]
        },
    );

    let trace = runtime.shutdown().expect("Betelgeuse multishard shutdown");
    assert_eq!(*target_hits.lock().expect("hits mutex"), 1);
    assert!(trace.iter().any(|event| {
        event.shard() == ShardId::new(1)
            && matches!(
                event.kind(),
                RuntimeEventKind::CallCompleted {
                    call_kind: CallKind::IsolateCall,
                    ..
                }
            )
    }));
    assert!(!trace.iter().any(|event| {
        matches!(
            event.kind(),
            RuntimeEventKind::CallReplyRejected { .. }
                | RuntimeEventKind::CallFailed {
                    call_kind: CallKind::IsolateCall,
                    ..
                }
        )
    }));
}

#[derive(Debug, Clone, Copy)]
enum ParkMsg {
    Park,
    Wake,
}

#[derive(Debug)]
struct ParkWorker {
    parked_tx: Option<mpsc::SyncSender<()>>,
    wake_rx: mpsc::Receiver<()>,
}

impl Isolate for ParkWorker {
    tina::isolate_types! {
        message: ParkMsg,
        reply: (),
        send: Outbound<Infallible>,
        spawn: Infallible,
        io: RuntimeCall<ParkMsg>,
        shard: WorkShard,
    }

    fn handle(
        &mut self,
        msg: Self::Message,
        _ctx: &mut Context<'_, Self::Shard, Self::Reply>,
    ) -> Effect<Self> {
        match msg {
            ParkMsg::Park => {
                if let Some(parked_tx) = self.parked_tx.take() {
                    parked_tx.send(()).expect("test observes parked handler");
                }
                self.wake_rx.recv().expect("test releases parked handler");
                noop()
            }
            ParkMsg::Wake => noop(),
        }
    }
}

#[derive(Debug, Clone, Copy)]
enum BurstMsg {
    Burst,
}

#[derive(Debug)]
struct RemoteBurst {
    sink: Address<SinkMsg>,
}

impl Isolate for RemoteBurst {
    tina::isolate_types! {
        message: BurstMsg,
        reply: (),
        send: Outbound<SinkMsg>,
        spawn: Infallible,
        io: RuntimeCall<BurstMsg>,
        shard: WorkShard,
    }

    fn handle(
        &mut self,
        msg: Self::Message,
        _ctx: &mut Context<'_, Self::Shard, Self::Reply>,
    ) -> Effect<Self> {
        match msg {
            BurstMsg::Burst => {
                batch([send(self.sink, SinkMsg::Hit), send(self.sink, SinkMsg::Hit)])
            }
        }
    }
}

#[test]
fn threaded_multishard_remote_queue_full_is_visible_at_source() {
    let runtime = ThreadedMultiShardRuntime::with_config(
        [WorkShard(1), WorkShard(2)],
        TestMailboxFactory,
        ThreadedRuntimeConfig {
            command_capacity: 1,
            shard_pair_capacity: 1,
            idle_wait: Duration::from_millis(1),
            ..Default::default()
        },
    );
    let (parked_tx, parked_rx) = mpsc::sync_channel(0);
    let (wake_tx, wake_rx) = mpsc::sync_channel(0);
    let parker = runtime
        .register_with_capacity_on::<ParkWorker, _>(
            ShardId::new(2),
            ParkWorker {
                parked_tx: Some(parked_tx),
                wake_rx,
            },
            8,
        )
        .expect("parker register accepts");
    let sink = runtime
        .register_with_capacity_on::<WorkSink, _>(ShardId::new(2), WorkSink, 8)
        .expect("sink register accepts");
    let burst = runtime
        .register_with_capacity_on::<RemoteBurst, _>(ShardId::new(1), RemoteBurst { sink }, 8)
        .expect("burst register accepts");

    runtime
        .try_send(parker, ParkMsg::Park)
        .expect("park handoff accepted");
    parked_rx.recv().expect("worker reached parked handler");
    runtime
        .try_send(parker, ParkMsg::Wake)
        .expect("wake command fills target queue");
    runtime
        .try_send(burst, BurstMsg::Burst)
        .expect("burst handoff accepted");

    wait_until(
        Duration::from_secs(2),
        "Betelgeuse remote full",
        || match runtime.trace_on(ShardId::new(1)) {
            Ok(trace) => count_send_rejected_full(&trace) >= 1,
            Err(ThreadedRuntimeError::CommandFull) => false,
            Err(error) => panic!("source shard trace: {error}"),
        },
    );
    wake_tx.send(()).expect("release parked target worker");

    let trace = runtime.shutdown().expect("Betelgeuse multishard shutdown");
    assert!(trace.iter().any(|event| {
        event.shard() == ShardId::new(1)
            && matches!(
                event.kind(),
                RuntimeEventKind::SendRejected {
                    target_shard,
                    reason: SendRejectedReason::Full,
                    ..
                } if target_shard == ShardId::new(2)
            )
    }));
}
