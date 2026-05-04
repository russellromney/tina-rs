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
    AcceptCompletion, AcceptOp, IO, IOFile, IOLoop, IOLoopHandle, IOSocket, OpenOptions, Operation,
    RecvCompletion, SendCompletion, io::simulated::SimulatedIO,
};
use tina::{Mailbox, TrySendError, prelude::*};
use tina_runtime::{
    BetelgeuseBackedControlError, BetelgeuseBackedMultiShardRuntime, BetelgeuseBackedRuntime,
    BetelgeuseBackedRuntimeConfig, CallCompletionRejectedReason, CallError, CallInput, CallKind,
    CallOutcome, CallOutput, ListenerId, MailboxFactory, RuntimeCall, RuntimeEvent,
    RuntimeEventKind, SendRejectedReason, TraceRetention, call, sleep,
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
        call: RuntimeCall<RetryMsg>,
        shard: TestShard,
    }

    fn handle(&mut self, msg: Self::Message, ctx: &mut Context<'_, Self::Shard>) -> Effect<Self> {
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
                    sleep(self.backoff).reply(|_| RetryMsg::RetryNow)
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
fn betelgeuse_runtime_timer_retry_runs_without_manual_stepping() {
    let observations = Arc::new(Mutex::new(Vec::new()));
    let runtime = BetelgeuseBackedRuntime::with_config(
        TestShard,
        TestMailboxFactory,
        BetelgeuseBackedRuntimeConfig {
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
fn betelgeuse_backed_runtime_honors_bounded_trace_retention() {
    let observations = Arc::new(Mutex::new(Vec::new()));
    let runtime = BetelgeuseBackedRuntime::with_config(
        TestShard,
        TestMailboxFactory,
        BetelgeuseBackedRuntimeConfig {
            command_capacity: 8,
            trace_retention: TraceRetention::Bounded(5),
            idle_wait: Duration::from_millis(1),
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

    let trace = runtime.trace().expect("trace snapshot");
    assert_eq!(trace.len(), 5);
    assert!(trace.first().expect("retained first event").id().get() > 1);
    assert!(trace.windows(2).all(|pair| pair[0].id() < pair[1].id()));
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
        call: RuntimeCall<LongTimerMsg>,
        shard: TestShard,
    }

    fn handle(&mut self, msg: Self::Message, _ctx: &mut Context<'_, Self::Shard>) -> Effect<Self> {
        match msg {
            LongTimerMsg::Start => sleep(Duration::from_secs(60)).reply(|_| LongTimerMsg::Finished),
            LongTimerMsg::Finished => noop(),
        }
    }
}

#[test]
fn betelgeuse_runtime_shutdown_rejects_outstanding_timer_completion() {
    let runtime = BetelgeuseBackedRuntime::with_config(
        TestShard,
        TestMailboxFactory,
        BetelgeuseBackedRuntimeConfig {
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
        call: RuntimeCall<TcpAcceptMsg>,
        shard: TestShard,
    }

    fn handle(&mut self, msg: Self::Message, _ctx: &mut Context<'_, Self::Shard>) -> Effect<Self> {
        match msg {
            TcpAcceptMsg::Bind => Effect::Call(RuntimeCall::new(
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
            TcpAcceptMsg::StartAccept => Effect::Call(RuntimeCall::new(
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
fn betelgeuse_runtime_shutdown_rejects_outstanding_tcp_accept_completion() {
    let simulated_io = SimulatedIO::new();
    let published = Arc::new(Mutex::new(None));
    let observed = Arc::new(Mutex::new(Vec::new()));
    let runtime = {
        let simulated_io = simulated_io.clone();
        BetelgeuseBackedRuntime::with_config_and_io_loop_factory(
            TestShard,
            TestMailboxFactory,
            BetelgeuseBackedRuntimeConfig {
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

#[derive(Clone, Default)]
struct StuckReleaseIo {
    state: Arc<Mutex<StuckReleaseState>>,
}

#[derive(Default)]
struct StuckReleaseState {
    local_addr: Option<SocketAddr>,
    pending_completion_count: usize,
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

    fn recv(&self, _c: &mut RecvCompletion, _len: usize) -> io::Result<()> {
        Err(io::Error::new(io::ErrorKind::Unsupported, "stuck recv"))
    }

    fn send(&self, _c: &mut SendCompletion, _buf: Vec<u8>) -> io::Result<()> {
        Err(io::Error::new(io::ErrorKind::Unsupported, "stuck send"))
    }

    fn set_nodelay(&self, _on: bool) -> io::Result<()> {
        Ok(())
    }

    fn close(&self) {}
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
        Ok(())
    }
}

#[test]
fn betelgeuse_runtime_shutdown_reports_driver_release_failure() {
    let stuck_io = StuckReleaseIo::default();
    let io_for_worker = stuck_io.clone();
    let runtime = BetelgeuseBackedRuntime::with_config_and_io_loop_factory(
        TestShard,
        TestMailboxFactory,
        BetelgeuseBackedRuntimeConfig {
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
        Err(BetelgeuseBackedControlError::DriverShutdownFailed)
    );
}

#[derive(Debug, Clone, Copy)]
struct PanickingMailboxFactory;

impl MailboxFactory for PanickingMailboxFactory {
    fn create<T: 'static>(&self, _capacity: usize) -> Box<dyn Mailbox<T>> {
        panic!("test mailbox factory panic")
    }
}

#[test]
fn betelgeuse_runtime_worker_panic_returns_typed_handle_error() {
    let runtime = BetelgeuseBackedRuntime::new(TestShard, PanickingMailboxFactory);

    assert_eq!(
        runtime.register_with_capacity::<LongTimer, _>(LongTimer, 8),
        Err(BetelgeuseBackedControlError::WorkerStopped)
    );
    assert_eq!(
        runtime.trace(),
        Err(BetelgeuseBackedControlError::WorkerStopped)
    );
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
        call: RuntimeCall<DriverMsg>,
        shard: TestShard,
    }

    fn handle(&mut self, msg: Self::Message, _ctx: &mut Context<'_, Self::Shard>) -> Effect<Self> {
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
        call: RuntimeCall<SinkMsg>,
        shard: TestShard,
    }

    fn handle(&mut self, _msg: Self::Message, _ctx: &mut Context<'_, Self::Shard>) -> Effect<Self> {
        noop()
    }
}

#[test]
fn betelgeuse_runtime_local_mailbox_full_is_visible_in_trace() {
    let runtime = BetelgeuseBackedRuntime::new(TestShard, TestMailboxFactory);
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
        let trace = runtime.trace().expect("Betelgeuse trace");
        trace.iter().any(|event| {
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
        call: RuntimeCall<CoordinatorMsg>,
        shard: WorkShard,
    }

    fn handle(&mut self, msg: Self::Message, ctx: &mut Context<'_, Self::Shard>) -> Effect<Self> {
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
        call: RuntimeCall<WorkerMsg>,
        shard: WorkShard,
    }

    fn handle(&mut self, msg: Self::Message, _ctx: &mut Context<'_, Self::Shard>) -> Effect<Self> {
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
        call: RuntimeCall<SinkMsg>,
        shard: WorkShard,
    }

    fn handle(&mut self, _msg: Self::Message, _ctx: &mut Context<'_, Self::Shard>) -> Effect<Self> {
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
fn betelgeuse_multishard_dispatcher_round_trips_between_worker_threads() {
    let runtime = BetelgeuseBackedMultiShardRuntime::with_config(
        [WorkShard(1), WorkShard(2)],
        TestMailboxFactory,
        BetelgeuseBackedRuntimeConfig {
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
fn betelgeuse_multishard_bad_remote_does_not_poison_good_remote_work() {
    let runtime =
        BetelgeuseBackedMultiShardRuntime::new([WorkShard(1), WorkShard(2)], TestMailboxFactory);
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
        call: RuntimeCall<CallTargetMsg>,
        shard: WorkShard,
    }

    fn handle(&mut self, msg: Self::Message, _ctx: &mut Context<'_, Self::Shard>) -> Effect<Self> {
        match msg {
            CallTargetMsg::Ask => {
                *self.hits.lock().expect("hits mutex") += 1;
                reply(CallReply)
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
        call: RuntimeCall<CallClientMsg>,
        shard: WorkShard,
    }

    fn handle(&mut self, msg: Self::Message, _ctx: &mut Context<'_, Self::Shard>) -> Effect<Self> {
        match msg {
            CallClientMsg::Start => call(self.target, CallTargetMsg::Ask, Duration::from_secs(1))
                .reply(CallClientMsg::Returned),
            CallClientMsg::Returned(outcome) => {
                let observation = match outcome {
                    CallOutcome::Replied(_) => CallObservation::Replied,
                    CallOutcome::Full => CallObservation::Full,
                    CallOutcome::Closed => CallObservation::Closed,
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
fn betelgeuse_multishard_isolate_call_rejects_cross_shard_with_typed_outcome() {
    let runtime =
        BetelgeuseBackedMultiShardRuntime::new([WorkShard(1), WorkShard(2)], TestMailboxFactory);
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
        "Betelgeuse cross-shard call reject",
        || observations.lock().expect("observations mutex").as_slice() == [CallObservation::Closed],
    );

    let trace = runtime.shutdown().expect("Betelgeuse multishard shutdown");
    assert_eq!(*target_hits.lock().expect("hits mutex"), 0);
    assert!(trace.iter().any(|event| {
        event.shard() == ShardId::new(1)
            && matches!(
                event.kind(),
                RuntimeEventKind::SendRejected {
                    target_shard,
                    reason: SendRejectedReason::Closed,
                    ..
                } if target_shard == ShardId::new(2)
            )
    }));
    assert!(trace.iter().any(|event| {
        event.shard() == ShardId::new(1)
            && matches!(
                event.kind(),
                RuntimeEventKind::CallFailed {
                    call_kind: CallKind::IsolateCall,
                    reason: CallError::TargetClosed,
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
        call: RuntimeCall<ParkMsg>,
        shard: WorkShard,
    }

    fn handle(&mut self, msg: Self::Message, _ctx: &mut Context<'_, Self::Shard>) -> Effect<Self> {
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
        call: RuntimeCall<BurstMsg>,
        shard: WorkShard,
    }

    fn handle(&mut self, msg: Self::Message, _ctx: &mut Context<'_, Self::Shard>) -> Effect<Self> {
        match msg {
            BurstMsg::Burst => {
                batch([send(self.sink, SinkMsg::Hit), send(self.sink, SinkMsg::Hit)])
            }
        }
    }
}

#[test]
fn betelgeuse_multishard_remote_queue_full_is_visible_at_source() {
    let runtime = BetelgeuseBackedMultiShardRuntime::with_config(
        [WorkShard(1), WorkShard(2)],
        TestMailboxFactory,
        BetelgeuseBackedRuntimeConfig {
            command_capacity: 1,
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

    wait_until(Duration::from_secs(2), "Betelgeuse remote full", || {
        let trace = runtime
            .trace_on(ShardId::new(1))
            .expect("source shard trace");
        count_send_rejected_full(&trace) >= 1
    });
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
