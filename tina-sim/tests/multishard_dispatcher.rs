use std::cell::RefCell;
use std::convert::Infallible;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::rc::Rc;
use std::time::Duration;

use tina::{Address, AddressGeneration, RestartBudget, RestartPolicy, prelude::*};
use tina_runtime::{
    CallError, CallInput, CallKind, CallOutcome, CallOutput, ListenerId, RuntimeCall, RuntimeEvent,
    RuntimeEventKind, SendRejectedReason, StreamId, call, journal_append, sleep,
};
use tina_sim::{
    Checker, CheckerDecision, FaultConfig, FaultMode, MultiShardReplayArtifact,
    MultiShardSimulator, MultiShardSimulatorConfig, ObservedPeerOutput, ScriptedListenerConfig,
    ScriptedPeerConfig, ScriptedTcpConfig, SimulatorConfig, TcpCompletionFaultMode,
};
use tina_supervisor::SupervisorConfig;

#[derive(Debug, Clone, Copy)]
struct WorkShard(u32);

impl Shard for WorkShard {
    fn id(&self) -> ShardId {
        ShardId::new(self.0)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CoordinatorEvent {
    Submit {
        job_id: u64,
        value: u64,
    },
    SubmitAfterBadRemote {
        job_id: u64,
        value: u64,
    },
    SubmitPair {
        first_job: u64,
        first_value: u64,
        second_job: u64,
        second_value: u64,
    },
    JobCompleted {
        job_id: u64,
        doubled: u64,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WorkerEvent {
    Run {
        job_id: u64,
        value: u64,
        reply_to: Address<CoordinatorEvent>,
    },
}

#[derive(Debug)]
struct Coordinator {
    worker: Address<WorkerEvent>,
    bad_worker: Option<Address<WorkerEvent>>,
    completed: Rc<RefCell<Vec<(u64, u64)>>>,
}

impl Isolate for Coordinator {
    tina::isolate_types! {
        message: CoordinatorEvent,
        reply: (),
        send: Outbound<WorkerEvent>,
        spawn: Infallible,
        io: RuntimeCall<CoordinatorEvent>,
        shard: WorkShard,
    }

    fn handle(
        &mut self,
        msg: Self::Message,
        ctx: &mut Context<'_, Self::Shard, Self::Reply>,
    ) -> Effect<Self> {
        match msg {
            CoordinatorEvent::Submit { job_id, value } => send(
                self.worker,
                WorkerEvent::Run {
                    job_id,
                    value,
                    reply_to: ctx.me(),
                },
            ),
            CoordinatorEvent::SubmitAfterBadRemote { job_id, value } => {
                let bad_worker = self
                    .bad_worker
                    .expect("SubmitAfterBadRemote requires a bad worker address");
                batch([
                    send(
                        bad_worker,
                        WorkerEvent::Run {
                            job_id: 0,
                            value: 0,
                            reply_to: ctx.me(),
                        },
                    ),
                    send(
                        self.worker,
                        WorkerEvent::Run {
                            job_id,
                            value,
                            reply_to: ctx.me(),
                        },
                    ),
                ])
            }
            CoordinatorEvent::SubmitPair {
                first_job,
                first_value,
                second_job,
                second_value,
            } => batch([
                send(
                    self.worker,
                    WorkerEvent::Run {
                        job_id: first_job,
                        value: first_value,
                        reply_to: ctx.me(),
                    },
                ),
                send(
                    self.worker,
                    WorkerEvent::Run {
                        job_id: second_job,
                        value: second_value,
                        reply_to: ctx.me(),
                    },
                ),
            ]),
            CoordinatorEvent::JobCompleted { job_id, doubled } => {
                self.completed.borrow_mut().push((job_id, doubled));
                noop()
            }
        }
    }
}

#[derive(Debug)]
struct Worker;

impl Isolate for Worker {
    tina::isolate_types! {
        message: WorkerEvent,
        reply: (),
        send: Outbound<CoordinatorEvent>,
        spawn: Infallible,
        io: RuntimeCall<WorkerEvent>,
        shard: WorkShard,
    }

    fn handle(
        &mut self,
        msg: Self::Message,
        _ctx: &mut Context<'_, Self::Shard, Self::Reply>,
    ) -> Effect<Self> {
        match msg {
            WorkerEvent::Run {
                job_id,
                value,
                reply_to,
            } => send(
                reply_to,
                CoordinatorEvent::JobCompleted {
                    job_id,
                    doubled: value * 2,
                },
            ),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum TerminalLaneCallerMsg {
    Start,
    Noise,
    Returned(CallOutcome<TerminalLaneReply>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct TerminalLaneReply(u64);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TerminalLaneWorkerMsg {
    Work {
        reply_to: Address<TerminalLaneCallerMsg>,
    },
}

#[derive(Debug)]
struct TerminalLaneCaller {
    worker: Address<TerminalLaneWorkerMsg, TerminalLaneReply>,
    outcomes: Rc<RefCell<Vec<CallOutcome<TerminalLaneReply>>>>,
    noise: Rc<RefCell<usize>>,
    order: Rc<RefCell<Vec<&'static str>>>,
}

#[tina_runtime::isolate(
    message = TerminalLaneCallerMsg,
    send = Outbound<TerminalLaneWorkerMsg>,
    shard = WorkShard
)]
impl TerminalLaneCaller {
    fn handle(
        &mut self,
        msg: TerminalLaneCallerMsg,
        ctx: &mut Context<'_, WorkShard, Self::Reply>,
    ) -> Effect<Self> {
        match msg {
            TerminalLaneCallerMsg::Start => call(
                self.worker,
                TerminalLaneWorkerMsg::Work { reply_to: ctx.me() },
                Duration::from_secs(1),
            )
            .then(TerminalLaneCallerMsg::Returned),
            TerminalLaneCallerMsg::Noise => {
                *self.noise.borrow_mut() += 1;
                self.order.borrow_mut().push("ordinary-noise");
                noop()
            }
            TerminalLaneCallerMsg::Returned(outcome) => {
                self.order.borrow_mut().push("terminal-reply");
                self.outcomes.borrow_mut().push(outcome);
                noop()
            }
        }
    }
}

#[derive(Debug)]
struct TerminalLaneWorker;

#[tina_runtime::isolate(
    message = TerminalLaneWorkerMsg,
    reply = TerminalLaneReply,
    send = Outbound<TerminalLaneCallerMsg>,
    shard = WorkShard
)]
impl TerminalLaneWorker {
    fn handle(
        &mut self,
        _msg: TerminalLaneWorkerMsg,
        _ctx: &mut Context<'_, WorkShard, Self::Reply>,
    ) -> Effect<Self> {
        noop()
    }

    fn handle_call(
        &mut self,
        msg: TerminalLaneWorkerMsg,
        call: tina::CallContext<'_, Self>,
    ) -> Effect<Self> {
        match msg {
            TerminalLaneWorkerMsg::Work { reply_to } => batch([
                send(reply_to, TerminalLaneCallerMsg::Noise),
                call.reply(TerminalLaneReply(42)),
            ]),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TimedCoordinatorEvent {
    Start,
    DelayElapsed,
    JobCompleted { job_id: u64, doubled: u64 },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TimedWorkerEvent {
    Run {
        job_id: u64,
        value: u64,
        reply_to: Address<TimedCoordinatorEvent>,
    },
}

#[derive(Debug)]
struct TimedCoordinator {
    worker: Address<TimedWorkerEvent>,
    job_id: u64,
    value: u64,
    backoff: Duration,
    completed: Rc<RefCell<Vec<(u64, u64)>>>,
}

impl Isolate for TimedCoordinator {
    tina::isolate_types! {
        message: TimedCoordinatorEvent,
        reply: (),
        send: Outbound<TimedWorkerEvent>,
        spawn: Infallible,
        io: RuntimeCall<TimedCoordinatorEvent>,
        shard: WorkShard,
    }

    fn handle(
        &mut self,
        msg: Self::Message,
        ctx: &mut Context<'_, Self::Shard, Self::Reply>,
    ) -> Effect<Self> {
        match msg {
            TimedCoordinatorEvent::Start => {
                sleep(self.backoff).then(|_| TimedCoordinatorEvent::DelayElapsed)
            }
            TimedCoordinatorEvent::DelayElapsed => send(
                self.worker,
                TimedWorkerEvent::Run {
                    job_id: self.job_id,
                    value: self.value,
                    reply_to: ctx.me(),
                },
            ),
            TimedCoordinatorEvent::JobCompleted { job_id, doubled } => {
                self.completed.borrow_mut().push((job_id, doubled));
                noop()
            }
        }
    }
}

#[derive(Debug)]
struct TimedWorker;

impl Isolate for TimedWorker {
    tina::isolate_types! {
        message: TimedWorkerEvent,
        reply: (),
        send: Outbound<TimedCoordinatorEvent>,
        spawn: Infallible,
        io: RuntimeCall<TimedWorkerEvent>,
        shard: WorkShard,
    }

    fn handle(
        &mut self,
        msg: Self::Message,
        _ctx: &mut Context<'_, Self::Shard, Self::Reply>,
    ) -> Effect<Self> {
        match msg {
            TimedWorkerEvent::Run {
                job_id,
                value,
                reply_to,
            } => send(
                reply_to,
                TimedCoordinatorEvent::JobCompleted {
                    job_id,
                    doubled: value * 2,
                },
            ),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SupervisorObservation {
    Booted(IsolateId),
    Worked(IsolateId, u32),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SupervisorEvent {
    SpawnOne,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RestartableWorkerEvent {
    Boot,
    Work(u32),
    Poison,
}

#[derive(Debug)]
struct SupervisorObserver {
    log: Rc<RefCell<Vec<SupervisorObservation>>>,
}

impl Isolate for SupervisorObserver {
    tina::isolate_types! {
        message: SupervisorObservation,
        reply: (),
        send: Outbound<Infallible>,
        spawn: Infallible,
        io: RuntimeCall<SupervisorObservation>,
        shard: WorkShard,
    }

    fn handle(
        &mut self,
        msg: Self::Message,
        _ctx: &mut Context<'_, Self::Shard, Self::Reply>,
    ) -> Effect<Self> {
        self.log.borrow_mut().push(msg);
        noop()
    }
}

#[derive(Debug)]
struct RestartableWorker {
    observer: Address<SupervisorObservation>,
}

impl Isolate for RestartableWorker {
    tina::isolate_types! {
        message: RestartableWorkerEvent,
        reply: (),
        send: Outbound<SupervisorObservation>,
        spawn: Infallible,
        io: RuntimeCall<RestartableWorkerEvent>,
        shard: WorkShard,
    }

    fn handle(
        &mut self,
        msg: Self::Message,
        ctx: &mut Context<'_, Self::Shard, Self::Reply>,
    ) -> Effect<Self> {
        match msg {
            RestartableWorkerEvent::Boot => send(
                self.observer,
                SupervisorObservation::Booted(ctx.isolate_id()),
            ),
            RestartableWorkerEvent::Work(value) => send(
                self.observer,
                SupervisorObservation::Worked(ctx.isolate_id(), value),
            ),
            RestartableWorkerEvent::Poison => panic!("simulated multi-shard worker panic"),
        }
    }
}

#[derive(Debug)]
struct SupervisedParent {
    observer: Address<SupervisorObservation>,
}

impl Isolate for SupervisedParent {
    tina::isolate_types! {
        message: SupervisorEvent,
        reply: (),
        send: Outbound<Infallible>,
        spawn: RestartableChildDefinition<RestartableWorker>,
        io: RuntimeCall<SupervisorEvent>,
        shard: WorkShard,
    }

    fn handle(
        &mut self,
        msg: Self::Message,
        _ctx: &mut Context<'_, Self::Shard, Self::Reply>,
    ) -> Effect<Self> {
        match msg {
            SupervisorEvent::SpawnOne => spawn(
                RestartableChildDefinition::new(
                    {
                        let observer = self.observer;
                        move || RestartableWorker { observer }
                    },
                    8,
                )
                .with_initial_message(|| RestartableWorkerEvent::Boot),
            ),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum TcpConnectionEvent {
    Start,
    ReadCompleted(Vec<u8>),
    WriteCompleted { count: usize },
    StreamClosed,
    Failed,
}

#[derive(Debug)]
struct TcpEchoConnection {
    stream: StreamId,
    pending_write: Vec<u8>,
}

impl Isolate for TcpEchoConnection {
    tina::isolate_types! {
        message: TcpConnectionEvent,
        reply: (),
        send: Outbound<Infallible>,
        spawn: Infallible,
        io: RuntimeCall<TcpConnectionEvent>,
        shard: WorkShard,
    }

    fn handle(
        &mut self,
        msg: Self::Message,
        _ctx: &mut Context<'_, Self::Shard, Self::Reply>,
    ) -> Effect<Self> {
        match msg {
            TcpConnectionEvent::Start => tcp_read_call(self.stream),
            TcpConnectionEvent::ReadCompleted(bytes) => {
                if bytes.is_empty() {
                    tcp_close_stream_call(self.stream)
                } else {
                    self.pending_write = bytes;
                    tcp_write_call(self.stream, self.pending_write.clone())
                }
            }
            TcpConnectionEvent::WriteCompleted { count } => {
                if count >= self.pending_write.len() {
                    self.pending_write.clear();
                    tcp_read_call(self.stream)
                } else {
                    self.pending_write.drain(..count);
                    tcp_write_call(self.stream, self.pending_write.clone())
                }
            }
            TcpConnectionEvent::StreamClosed | TcpConnectionEvent::Failed => stop(),
        }
    }
}

fn tcp_read_call(stream: StreamId) -> Effect<TcpEchoConnection> {
    Effect::Io(RuntimeCall::new(
        CallInput::TcpRead {
            stream,
            max_len: 64,
        },
        |result| match result {
            CallOutput::TcpRead { bytes } => TcpConnectionEvent::ReadCompleted(bytes),
            CallOutput::Failed(_) => TcpConnectionEvent::Failed,
            other => panic!("unexpected read result {other:?}"),
        },
    ))
}

fn tcp_write_call(stream: StreamId, bytes: Vec<u8>) -> Effect<TcpEchoConnection> {
    Effect::Io(RuntimeCall::new(
        CallInput::TcpWrite { stream, bytes },
        |result| match result {
            CallOutput::TcpWrote { count } => TcpConnectionEvent::WriteCompleted { count },
            CallOutput::Failed(_) => TcpConnectionEvent::Failed,
            other => panic!("unexpected write result {other:?}"),
        },
    ))
}

fn tcp_close_stream_call(stream: StreamId) -> Effect<TcpEchoConnection> {
    Effect::Io(RuntimeCall::new(
        CallInput::TcpStreamClose { stream },
        |result| match result {
            CallOutput::TcpStreamClosed => TcpConnectionEvent::StreamClosed,
            CallOutput::Failed(_) => TcpConnectionEvent::Failed,
            other => panic!("unexpected stream close result {other:?}"),
        },
    ))
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum TcpControlEvent {
    Bootstrap,
    Bound { listener: ListenerId },
    ReArmAccept,
    Accepted { stream: StreamId },
    CloseListener,
    ListenerClosed,
    ListenerFinished,
    Failed,
}

#[derive(Debug)]
struct TcpEchoListener {
    bind_addr: SocketAddr,
    target_accepts: usize,
    accepted: usize,
    listener: Option<ListenerId>,
    report_to: Address<TcpControlEvent>,
}

impl Isolate for TcpEchoListener {
    tina::isolate_types! {
        message: TcpControlEvent,
        reply: (),
        send: Outbound<TcpControlEvent>,
        spawn: RestartableChildDefinition<TcpEchoConnection>,
        io: RuntimeCall<TcpControlEvent>,
        shard: WorkShard,
    }

    fn handle(
        &mut self,
        msg: Self::Message,
        ctx: &mut Context<'_, Self::Shard, Self::Reply>,
    ) -> Effect<Self> {
        match msg {
            TcpControlEvent::Bootstrap => {
                let addr = self.bind_addr;
                Effect::Io(RuntimeCall::new(
                    CallInput::TcpBind { addr },
                    |result| match result {
                        CallOutput::TcpBound { listener, .. } => {
                            TcpControlEvent::Bound { listener }
                        }
                        CallOutput::Failed(_) => TcpControlEvent::Failed,
                        other => panic!("unexpected bind result {other:?}"),
                    },
                ))
            }
            TcpControlEvent::Bound { listener } => {
                self.listener = Some(listener);
                Effect::Io(RuntimeCall::new(
                    CallInput::TcpAccept { listener },
                    |result| match result {
                        CallOutput::TcpAccepted { stream, .. } => {
                            TcpControlEvent::Accepted { stream }
                        }
                        CallOutput::Failed(_) => TcpControlEvent::Failed,
                        other => panic!("unexpected accept result {other:?}"),
                    },
                ))
            }
            TcpControlEvent::ReArmAccept => {
                let listener = self.listener.expect("listener stored before re-arm");
                Effect::Io(RuntimeCall::new(
                    CallInput::TcpAccept { listener },
                    |result| match result {
                        CallOutput::TcpAccepted { stream, .. } => {
                            TcpControlEvent::Accepted { stream }
                        }
                        CallOutput::Failed(_) => TcpControlEvent::Failed,
                        other => panic!("unexpected accept result {other:?}"),
                    },
                ))
            }
            TcpControlEvent::Accepted { stream } => {
                self.accepted += 1;
                let spawn_effect = spawn(
                    RestartableChildDefinition::new(
                        move || TcpEchoConnection {
                            stream,
                            pending_write: Vec::new(),
                        },
                        8,
                    )
                    .with_initial_message(|| TcpConnectionEvent::Start),
                );
                let follow_up = if self.accepted < self.target_accepts {
                    TcpControlEvent::ReArmAccept
                } else {
                    TcpControlEvent::CloseListener
                };
                batch([spawn_effect, ctx.send_self(follow_up)])
            }
            TcpControlEvent::CloseListener => {
                let listener = self.listener.expect("listener stored before close");
                Effect::Io(RuntimeCall::new(
                    CallInput::TcpListenerClose { listener },
                    |result| match result {
                        CallOutput::TcpListenerClosed => TcpControlEvent::ListenerClosed,
                        CallOutput::Failed(_) => TcpControlEvent::Failed,
                        other => panic!("unexpected listener close result {other:?}"),
                    },
                ))
            }
            TcpControlEvent::ListenerClosed => batch([
                send(self.report_to, TcpControlEvent::ListenerFinished),
                stop(),
            ]),
            TcpControlEvent::ListenerFinished | TcpControlEvent::Failed => stop(),
        }
    }
}

#[derive(Debug)]
struct TcpCoordinator {
    done: Rc<RefCell<usize>>,
}

impl Isolate for TcpCoordinator {
    tina::isolate_types! {
        message: TcpControlEvent,
        reply: (),
        send: Outbound<Infallible>,
        spawn: Infallible,
        io: RuntimeCall<TcpControlEvent>,
        shard: WorkShard,
    }

    fn handle(
        &mut self,
        msg: Self::Message,
        _ctx: &mut Context<'_, Self::Shard, Self::Reply>,
    ) -> Effect<Self> {
        match msg {
            TcpControlEvent::ListenerFinished => {
                *self.done.borrow_mut() += 1;
                noop()
            }
            _ => stop(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum DurableTcpFrontendMsg {
    Start,
    Bound { listener: ListenerId },
    Accepted { stream: StreamId },
    Read(Result<Vec<u8>, CallError>, StreamId),
    Persisted(Result<(), CallError>, Vec<u8>),
    Wrote(Result<usize, CallError>, StreamId),
    StreamClosed(Result<(), CallError>),
    ListenerClosed(Result<(), CallError>),
    Failed,
}

#[derive(Debug)]
struct DurableTcpFrontend {
    bind_addr: SocketAddr,
    worker: Address<DurableStoreMsg>,
    listener: Option<ListenerId>,
    active_stream: Option<StreamId>,
}

impl Isolate for DurableTcpFrontend {
    tina::isolate_types! {
        message: DurableTcpFrontendMsg,
        reply: (),
        send: Outbound<DurableStoreMsg>,
        spawn: Infallible,
        io: RuntimeCall<DurableTcpFrontendMsg>,
        shard: WorkShard,
    }

    fn handle(
        &mut self,
        msg: Self::Message,
        ctx: &mut Context<'_, Self::Shard, Self::Reply>,
    ) -> Effect<Self> {
        match msg {
            DurableTcpFrontendMsg::Start => Effect::Io(RuntimeCall::new(
                CallInput::TcpBind {
                    addr: self.bind_addr,
                },
                |result| match result {
                    CallOutput::TcpBound { listener, .. } => {
                        DurableTcpFrontendMsg::Bound { listener }
                    }
                    CallOutput::Failed(_) => DurableTcpFrontendMsg::Failed,
                    other => panic!("unexpected bind result {other:?}"),
                },
            )),
            DurableTcpFrontendMsg::Bound { listener } => {
                self.listener = Some(listener);
                Effect::Io(RuntimeCall::new(
                    CallInput::TcpAccept { listener },
                    |result| match result {
                        CallOutput::TcpAccepted { stream, .. } => {
                            DurableTcpFrontendMsg::Accepted { stream }
                        }
                        CallOutput::Failed(_) => DurableTcpFrontendMsg::Failed,
                        other => panic!("unexpected accept result {other:?}"),
                    },
                ))
            }
            DurableTcpFrontendMsg::Accepted { stream } => {
                self.active_stream = Some(stream);
                Effect::Io(RuntimeCall::new(
                    CallInput::TcpRead {
                        stream,
                        max_len: 64,
                    },
                    move |result| match result {
                        CallOutput::TcpRead { bytes } => {
                            DurableTcpFrontendMsg::Read(Ok(bytes), stream)
                        }
                        CallOutput::Failed(error) => {
                            DurableTcpFrontendMsg::Read(Err(error), stream)
                        }
                        other => panic!("unexpected read result {other:?}"),
                    },
                ))
            }
            DurableTcpFrontendMsg::Read(Ok(bytes), stream) if bytes.is_empty() => {
                Effect::Io(RuntimeCall::new(
                    CallInput::TcpStreamClose { stream },
                    |result| match result {
                        CallOutput::TcpStreamClosed => DurableTcpFrontendMsg::StreamClosed(Ok(())),
                        CallOutput::Failed(error) => {
                            DurableTcpFrontendMsg::StreamClosed(Err(error))
                        }
                        other => panic!("unexpected stream close result {other:?}"),
                    },
                ))
            }
            DurableTcpFrontendMsg::Read(Ok(bytes), stream) => {
                self.active_stream = Some(stream);
                send(
                    self.worker,
                    DurableStoreMsg::Append {
                        index: 1,
                        bytes,
                        reply_to: ctx.me(),
                    },
                )
            }
            DurableTcpFrontendMsg::Persisted(Ok(()), bytes) => {
                let stream = self
                    .active_stream
                    .expect("stream stored before durable ack");
                let mut reply = b"stored:".to_vec();
                reply.extend_from_slice(&bytes);
                Effect::Io(RuntimeCall::new(
                    CallInput::TcpWrite {
                        stream,
                        bytes: reply,
                    },
                    move |result| match result {
                        CallOutput::TcpWrote { count } => {
                            DurableTcpFrontendMsg::Wrote(Ok(count), stream)
                        }
                        CallOutput::Failed(error) => {
                            DurableTcpFrontendMsg::Wrote(Err(error), stream)
                        }
                        other => panic!("unexpected write result {other:?}"),
                    },
                ))
            }
            DurableTcpFrontendMsg::Wrote(Ok(_), stream) => Effect::Io(RuntimeCall::new(
                CallInput::TcpStreamClose { stream },
                |result| match result {
                    CallOutput::TcpStreamClosed => DurableTcpFrontendMsg::StreamClosed(Ok(())),
                    CallOutput::Failed(error) => DurableTcpFrontendMsg::StreamClosed(Err(error)),
                    other => panic!("unexpected stream close result {other:?}"),
                },
            )),
            DurableTcpFrontendMsg::StreamClosed(Ok(())) => {
                let listener = self.listener.expect("listener stored before close");
                Effect::Io(RuntimeCall::new(
                    CallInput::TcpListenerClose { listener },
                    |result| match result {
                        CallOutput::TcpListenerClosed => {
                            DurableTcpFrontendMsg::ListenerClosed(Ok(()))
                        }
                        CallOutput::Failed(error) => {
                            DurableTcpFrontendMsg::ListenerClosed(Err(error))
                        }
                        other => panic!("unexpected listener close result {other:?}"),
                    },
                ))
            }
            DurableTcpFrontendMsg::ListenerClosed(Ok(())) => stop(),
            DurableTcpFrontendMsg::Read(Err(_), _)
            | DurableTcpFrontendMsg::Persisted(Err(_), _)
            | DurableTcpFrontendMsg::Wrote(Err(_), _)
            | DurableTcpFrontendMsg::StreamClosed(Err(_))
            | DurableTcpFrontendMsg::ListenerClosed(Err(_))
            | DurableTcpFrontendMsg::Failed => stop(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum DurableStoreMsg {
    Append {
        index: u64,
        bytes: Vec<u8>,
        reply_to: Address<DurableTcpFrontendMsg>,
    },
    Appended(
        Result<(), CallError>,
        Vec<u8>,
        Address<DurableTcpFrontendMsg>,
    ),
}

#[derive(Debug)]
struct DurableStore {
    journal_path: PathBuf,
}

impl Isolate for DurableStore {
    tina::isolate_types! {
        message: DurableStoreMsg,
        reply: (),
        send: Outbound<DurableTcpFrontendMsg>,
        spawn: Infallible,
        io: RuntimeCall<DurableStoreMsg>,
        shard: WorkShard,
    }

    fn handle(
        &mut self,
        msg: Self::Message,
        _ctx: &mut Context<'_, Self::Shard, Self::Reply>,
    ) -> Effect<Self> {
        match msg {
            DurableStoreMsg::Append {
                index,
                bytes,
                reply_to,
            } => journal_append(self.journal_path.clone(), index, bytes.clone())
                .then(move |result| DurableStoreMsg::Appended(result, bytes, reply_to)),
            DurableStoreMsg::Appended(result, bytes, reply_to) => {
                send(reply_to, DurableTcpFrontendMsg::Persisted(result, bytes))
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum DurableBatchListenerMsg {
    Start,
    Bound { listener: ListenerId },
    Accepted { stream: StreamId },
    ReArmAccept,
    CloseListener,
    ListenerClosed(Result<(), CallError>),
    Failed,
}

#[derive(Debug)]
struct DurableBatchListener {
    bind_addr: SocketAddr,
    worker: Address<DurableBatchStoreMsg>,
    target_accepts: usize,
    accepted: usize,
    listener: Option<ListenerId>,
}

impl Isolate for DurableBatchListener {
    tina::isolate_types! {
        message: DurableBatchListenerMsg,
        reply: (),
        send: Outbound<DurableBatchListenerMsg>,
        spawn: RestartableChildDefinition<DurableBatchConnection>,
        io: RuntimeCall<DurableBatchListenerMsg>,
        shard: WorkShard,
    }

    fn handle(
        &mut self,
        msg: Self::Message,
        ctx: &mut Context<'_, Self::Shard, Self::Reply>,
    ) -> Effect<Self> {
        match msg {
            DurableBatchListenerMsg::Start => Effect::Io(RuntimeCall::new(
                CallInput::TcpBind {
                    addr: self.bind_addr,
                },
                |result| match result {
                    CallOutput::TcpBound { listener, .. } => {
                        DurableBatchListenerMsg::Bound { listener }
                    }
                    CallOutput::Failed(_) => DurableBatchListenerMsg::Failed,
                    other => panic!("unexpected bind result {other:?}"),
                },
            )),
            DurableBatchListenerMsg::Bound { listener } => {
                self.listener = Some(listener);
                tcp_accept_batch_call(listener)
            }
            DurableBatchListenerMsg::Accepted { stream } => {
                self.accepted += 1;
                let worker = self.worker;
                let spawn_effect = spawn(
                    RestartableChildDefinition::new(
                        move || DurableBatchConnection {
                            stream,
                            worker,
                            pending_write: Vec::new(),
                        },
                        8,
                    )
                    .with_initial_message(|| DurableBatchConnectionMsg::Start),
                );
                let follow_up = if self.accepted < self.target_accepts {
                    DurableBatchListenerMsg::ReArmAccept
                } else {
                    DurableBatchListenerMsg::CloseListener
                };
                batch([spawn_effect, ctx.send_self(follow_up)])
            }
            DurableBatchListenerMsg::ReArmAccept => {
                tcp_accept_batch_call(self.listener.expect("listener stored before re-arm"))
            }
            DurableBatchListenerMsg::CloseListener => {
                let listener = self.listener.expect("listener stored before close");
                Effect::Io(RuntimeCall::new(
                    CallInput::TcpListenerClose { listener },
                    |result| match result {
                        CallOutput::TcpListenerClosed => {
                            DurableBatchListenerMsg::ListenerClosed(Ok(()))
                        }
                        CallOutput::Failed(error) => {
                            DurableBatchListenerMsg::ListenerClosed(Err(error))
                        }
                        other => panic!("unexpected listener close result {other:?}"),
                    },
                ))
            }
            DurableBatchListenerMsg::ListenerClosed(Ok(())) => stop(),
            DurableBatchListenerMsg::ListenerClosed(Err(_)) | DurableBatchListenerMsg::Failed => {
                stop()
            }
        }
    }
}

fn tcp_accept_batch_call(listener: ListenerId) -> Effect<DurableBatchListener> {
    Effect::Io(RuntimeCall::new(
        CallInput::TcpAccept { listener },
        |result| match result {
            CallOutput::TcpAccepted { stream, .. } => DurableBatchListenerMsg::Accepted { stream },
            CallOutput::Failed(_) => DurableBatchListenerMsg::Failed,
            other => panic!("unexpected accept result {other:?}"),
        },
    ))
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum DurableBatchConnectionMsg {
    Start,
    Read(Result<Vec<u8>, CallError>),
    Persisted(Result<(), CallError>, Vec<u8>),
    Wrote(Result<usize, CallError>),
    StreamClosed(Result<(), CallError>),
}

#[derive(Debug)]
struct DurableBatchConnection {
    stream: StreamId,
    worker: Address<DurableBatchStoreMsg>,
    pending_write: Vec<u8>,
}

impl Isolate for DurableBatchConnection {
    tina::isolate_types! {
        message: DurableBatchConnectionMsg,
        reply: (),
        send: Outbound<DurableBatchStoreMsg>,
        spawn: Infallible,
        io: RuntimeCall<DurableBatchConnectionMsg>,
        shard: WorkShard,
    }

    fn handle(
        &mut self,
        msg: Self::Message,
        ctx: &mut Context<'_, Self::Shard, Self::Reply>,
    ) -> Effect<Self> {
        match msg {
            DurableBatchConnectionMsg::Start => durable_batch_read_call(self.stream),
            DurableBatchConnectionMsg::Read(Ok(bytes)) if bytes.is_empty() => {
                durable_batch_close_call(self.stream)
            }
            DurableBatchConnectionMsg::Read(Ok(bytes)) => send(
                self.worker,
                DurableBatchStoreMsg::Append {
                    bytes,
                    reply_to: ctx.me(),
                },
            ),
            DurableBatchConnectionMsg::Persisted(Ok(()), bytes) => {
                self.pending_write = b"stored:".to_vec();
                self.pending_write.extend_from_slice(&bytes);
                durable_batch_write_call(self.stream, self.pending_write.clone())
            }
            DurableBatchConnectionMsg::Wrote(Ok(count)) => {
                if count >= self.pending_write.len() {
                    self.pending_write.clear();
                    durable_batch_read_call(self.stream)
                } else {
                    self.pending_write.drain(..count);
                    durable_batch_write_call(self.stream, self.pending_write.clone())
                }
            }
            DurableBatchConnectionMsg::StreamClosed(Ok(())) => stop(),
            DurableBatchConnectionMsg::Read(Err(_))
            | DurableBatchConnectionMsg::Persisted(Err(_), _)
            | DurableBatchConnectionMsg::Wrote(Err(_))
            | DurableBatchConnectionMsg::StreamClosed(Err(_)) => stop(),
        }
    }
}

fn durable_batch_read_call(stream: StreamId) -> Effect<DurableBatchConnection> {
    Effect::Io(RuntimeCall::new(
        CallInput::TcpRead {
            stream,
            max_len: 64,
        },
        |result| match result {
            CallOutput::TcpRead { bytes } => DurableBatchConnectionMsg::Read(Ok(bytes)),
            CallOutput::Failed(error) => DurableBatchConnectionMsg::Read(Err(error)),
            other => panic!("unexpected read result {other:?}"),
        },
    ))
}

fn durable_batch_write_call(stream: StreamId, bytes: Vec<u8>) -> Effect<DurableBatchConnection> {
    Effect::Io(RuntimeCall::new(
        CallInput::TcpWrite { stream, bytes },
        |result| match result {
            CallOutput::TcpWrote { count } => DurableBatchConnectionMsg::Wrote(Ok(count)),
            CallOutput::Failed(error) => DurableBatchConnectionMsg::Wrote(Err(error)),
            other => panic!("unexpected write result {other:?}"),
        },
    ))
}

fn durable_batch_close_call(stream: StreamId) -> Effect<DurableBatchConnection> {
    Effect::Io(RuntimeCall::new(
        CallInput::TcpStreamClose { stream },
        |result| match result {
            CallOutput::TcpStreamClosed => DurableBatchConnectionMsg::StreamClosed(Ok(())),
            CallOutput::Failed(error) => DurableBatchConnectionMsg::StreamClosed(Err(error)),
            other => panic!("unexpected stream close result {other:?}"),
        },
    ))
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum DurableBatchStoreMsg {
    Append {
        bytes: Vec<u8>,
        reply_to: Address<DurableBatchConnectionMsg>,
    },
    Appended(
        Result<(), CallError>,
        Vec<u8>,
        Address<DurableBatchConnectionMsg>,
    ),
}

#[derive(Debug)]
struct DurableBatchStore {
    journal_path: PathBuf,
    next_index: u64,
}

impl Isolate for DurableBatchStore {
    tina::isolate_types! {
        message: DurableBatchStoreMsg,
        reply: (),
        send: Outbound<DurableBatchConnectionMsg>,
        spawn: Infallible,
        io: RuntimeCall<DurableBatchStoreMsg>,
        shard: WorkShard,
    }

    fn handle(
        &mut self,
        msg: Self::Message,
        _ctx: &mut Context<'_, Self::Shard, Self::Reply>,
    ) -> Effect<Self> {
        match msg {
            DurableBatchStoreMsg::Append { bytes, reply_to } => {
                self.next_index += 1;
                journal_append(self.journal_path.clone(), self.next_index, bytes.clone())
                    .then(move |result| DurableBatchStoreMsg::Appended(result, bytes, reply_to))
            }
            DurableBatchStoreMsg::Appended(result, bytes, reply_to) => send(
                reply_to,
                DurableBatchConnectionMsg::Persisted(result, bytes),
            ),
        }
    }
}

#[derive(Debug, Clone)]
struct SavedMultiShardRun {
    artifact: MultiShardReplayArtifact,
    completed: Vec<(u64, u64)>,
}

#[derive(Debug, Clone)]
struct SavedDurableTcpRun {
    artifact: MultiShardReplayArtifact,
}

fn event_id(trace: &[RuntimeEvent], predicate: impl Fn(&RuntimeEvent) -> bool) -> u64 {
    trace
        .iter()
        .find(|event| predicate(event))
        .unwrap_or_else(|| panic!("expected matching event in trace"))
        .id()
        .get()
}

fn run_timed_dispatcher_workload(
    simulator_config: SimulatorConfig,
    multishard_config: MultiShardSimulatorConfig,
) -> SavedMultiShardRun {
    let mut sim = MultiShardSimulator::with_config(
        [WorkShard(11), WorkShard(22)],
        simulator_config,
        multishard_config,
    );
    let completed = Rc::new(RefCell::new(Vec::new()));

    let worker = sim
        .register_with_capacity_on::<TimedWorker, TimedWorkerEvent, TimedCoordinatorEvent>(
            ShardId::new(22),
            TimedWorker,
            4,
        );
    let coordinator = sim
        .register_with_capacity_on::<TimedCoordinator, TimedCoordinatorEvent, TimedWorkerEvent>(
            ShardId::new(11),
            TimedCoordinator {
                worker,
                job_id: 9,
                value: 7,
                backoff: Duration::from_millis(25),
                completed: Rc::clone(&completed),
            },
            4,
        );

    sim.try_send(coordinator, TimedCoordinatorEvent::Start)
        .unwrap();
    sim.run_until_quiescent();

    SavedMultiShardRun {
        artifact: sim.replay_artifact(),
        completed: completed.borrow().clone(),
    }
}

fn bind_addr() -> SocketAddr {
    "127.0.0.1:0".parse().expect("loopback bind addr")
}

fn local_addr(port: u16) -> SocketAddr {
    format!("127.0.0.1:{port}")
        .parse()
        .expect("loopback local addr")
}

fn peer_addr(port: u16) -> SocketAddr {
    format!("127.0.0.1:{port}")
        .parse()
        .expect("loopback peer addr")
}

fn peer_script(
    accept_after_step: u64,
    peer_addr: SocketAddr,
    inbound_chunks: Vec<Vec<u8>>,
    read_chunk_cap: Option<usize>,
    write_cap: usize,
) -> ScriptedPeerConfig {
    ScriptedPeerConfig {
        accept_after_step,
        peer_addr,
        inbound_capacity: inbound_chunks.iter().map(Vec::len).sum(),
        inbound_chunks,
        read_chunk_cap,
        write_cap,
        output_capacity: 1024,
    }
}

fn durable_journal_path() -> PathBuf {
    PathBuf::from("/tmp/tina-sim-durable-cross-shard-journal")
}

fn durable_batch_journal_path(seed: u64) -> PathBuf {
    PathBuf::from(format!("/tmp/tina-sim-durable-batch-journal-{seed}"))
}

fn expected_stored_output(payload: &[u8], chunk_cap: usize) -> Vec<u8> {
    let mut output = Vec::new();
    for chunk in payload.chunks(chunk_cap) {
        output.extend_from_slice(b"stored:");
        output.extend_from_slice(chunk);
    }
    output
}

fn expected_journal_chunks(payloads: &[Vec<u8>], chunk_cap: usize) -> Vec<Vec<u8>> {
    payloads
        .iter()
        .flat_map(|payload| payload.chunks(chunk_cap).map(<[u8]>::to_vec))
        .collect()
}

fn run_durable_tcp_multishard_workload(config: SimulatorConfig) -> SavedDurableTcpRun {
    let mut sim = MultiShardSimulator::with_config(
        [WorkShard(11), WorkShard(22)],
        config,
        MultiShardSimulatorConfig {
            shard_pair_capacity: 8,
        },
    );
    let worker = sim
        .register_with_capacity_on::<DurableStore, DurableStoreMsg, DurableTcpFrontendMsg>(
            ShardId::new(22),
            DurableStore {
                journal_path: durable_journal_path(),
            },
            8,
        );
    let frontend = sim
        .register_with_capacity_on::<DurableTcpFrontend, DurableTcpFrontendMsg, DurableStoreMsg>(
            ShardId::new(11),
            DurableTcpFrontend {
                bind_addr: bind_addr(),
                worker,
                listener: None,
                active_stream: None,
            },
            8,
        );
    let mut checker = PersistBeforeTcpWriteChecker::default();

    sim.try_send(frontend, DurableTcpFrontendMsg::Start)
        .unwrap();
    let failure = sim.run_until_quiescent_checked(&mut checker);
    assert_eq!(failure, None);

    SavedDurableTcpRun {
        artifact: sim.replay_artifact(),
    }
}

fn run_durable_batch_tcp_multishard_workload(
    seed: u64,
    payloads: &[Vec<u8>],
) -> SavedDurableTcpRun {
    let mut sim = MultiShardSimulator::with_config(
        [WorkShard(11), WorkShard(22)],
        SimulatorConfig {
            seed,
            faults: FaultConfig {
                local_send: tina_sim::LocalSendFaultMode::DelayByRounds {
                    one_in: 2,
                    rounds: 1,
                },
                tcp_completion: TcpCompletionFaultMode::ReorderReady { one_in: 2 },
                ..Default::default()
            },
            tcp: ScriptedTcpConfig {
                pending_completion_capacity: 128,
                listeners: vec![ScriptedListenerConfig {
                    bind_addr: bind_addr(),
                    local_addr: local_addr(50200),
                    backlog_capacity: payloads.len(),
                    peers: payloads
                        .iter()
                        .enumerate()
                        .map(|(index, payload)| {
                            peer_script(
                                1,
                                peer_addr(61200 + index as u16),
                                vec![payload.clone()],
                                Some(2),
                                3,
                            )
                        })
                        .collect(),
                }],
            },
            storage: Default::default(),
            ..Default::default()
        },
        MultiShardSimulatorConfig {
            shard_pair_capacity: 16,
        },
    );
    let worker = sim.register_with_capacity_on::<
        DurableBatchStore,
        DurableBatchStoreMsg,
        DurableBatchConnectionMsg,
    >(
        ShardId::new(22),
        DurableBatchStore {
            journal_path: durable_batch_journal_path(seed),
            next_index: 0,
        },
        32,
    );
    let listener = sim.register_with_capacity_on::<
        DurableBatchListener,
        DurableBatchListenerMsg,
        DurableBatchListenerMsg,
    >(
        ShardId::new(11),
        DurableBatchListener {
            bind_addr: bind_addr(),
            worker,
            target_accepts: payloads.len(),
            accepted: 0,
            listener: None,
        },
        16,
    );
    let mut checker = PersistBeforeTcpWriteChecker::default();

    sim.try_send(listener, DurableBatchListenerMsg::Start)
        .unwrap();
    let failure = sim.run_until_quiescent_checked(&mut checker);
    assert_eq!(failure, None);

    SavedDurableTcpRun {
        artifact: sim.replay_artifact(),
    }
}

#[derive(Debug, Default)]
struct PersistBeforeTcpWriteChecker {
    journal_appended: bool,
}

impl Checker for PersistBeforeTcpWriteChecker {
    fn name(&self) -> &'static str {
        "persist-before-tcp-write"
    }

    fn on_event(&mut self, event: &RuntimeEvent) -> CheckerDecision {
        match event.kind() {
            RuntimeEventKind::JournalAppended { .. } => {
                self.journal_appended = true;
                CheckerDecision::Continue
            }
            RuntimeEventKind::CallCompleted {
                call_kind: CallKind::TcpWrite,
                ..
            } if !self.journal_appended => {
                CheckerDecision::Fail("peer reply became visible before durable append".into())
            }
            _ => CheckerDecision::Continue,
        }
    }
}

fn spawned_children(trace: &[RuntimeEvent]) -> Vec<IsolateId> {
    trace
        .iter()
        .filter_map(|event| match event.kind() {
            RuntimeEventKind::Spawned { child_isolate, .. } => Some(child_isolate),
            _ => None,
        })
        .collect()
}

fn completed_restarts(trace: &[RuntimeEvent]) -> Vec<(IsolateId, IsolateId)> {
    trace
        .iter()
        .filter_map(|event| match event.kind() {
            RuntimeEventKind::RestartChildCompleted {
                old_isolate,
                new_isolate,
                ..
            } => Some((old_isolate, new_isolate)),
            _ => None,
        })
        .collect()
}

fn run_dispatcher_workload(
    simulator_config: SimulatorConfig,
    multishard_config: MultiShardSimulatorConfig,
    event: CoordinatorEvent,
) -> SavedMultiShardRun {
    let mut sim = MultiShardSimulator::with_config(
        [WorkShard(11), WorkShard(22)],
        simulator_config.clone(),
        multishard_config,
    );
    let completed = Rc::new(RefCell::new(Vec::new()));

    let worker = sim.register_with_capacity_on::<Worker, WorkerEvent, CoordinatorEvent>(
        ShardId::new(22),
        Worker,
        4,
    );
    let coordinator = sim.register_with_capacity_on::<Coordinator, CoordinatorEvent, WorkerEvent>(
        ShardId::new(11),
        Coordinator {
            worker,
            bad_worker: None,
            completed: Rc::clone(&completed),
        },
        4,
    );

    sim.try_send(coordinator, event).unwrap();
    sim.run_until_quiescent();

    SavedMultiShardRun {
        artifact: sim.replay_artifact(),
        completed: completed.borrow().clone(),
    }
}

#[test]
fn multishard_dispatcher_workload_preserves_request_reply_causality() {
    let run = run_dispatcher_workload(
        SimulatorConfig::default(),
        MultiShardSimulatorConfig {
            shard_pair_capacity: 4,
        },
        CoordinatorEvent::Submit {
            job_id: 7,
            value: 21,
        },
    );

    assert_eq!(run.completed, vec![(7, 42)]);

    let request_attempt = event_id(run.artifact.event_record(), |event| {
        event.shard() == ShardId::new(11)
            && matches!(
                event.kind(),
                RuntimeEventKind::SendDispatchAttempted {
                    target_shard,
                    ..
                } if target_shard == ShardId::new(22)
            )
    });
    let request_accept = event_id(run.artifact.event_record(), |event| {
        event.shard() == ShardId::new(11)
            && matches!(
                event.kind(),
                RuntimeEventKind::SendAccepted {
                    target_shard,
                    ..
                } if target_shard == ShardId::new(22)
            )
    });
    let worker_mailbox = event_id(run.artifact.event_record(), |event| {
        event.shard() == ShardId::new(22)
            && matches!(event.kind(), RuntimeEventKind::MailboxAccepted)
            && event
                .cause()
                .is_some_and(|cause| cause.event().get() == request_attempt)
    });
    let reply_attempt = event_id(run.artifact.event_record(), |event| {
        event.shard() == ShardId::new(22)
            && matches!(
                event.kind(),
                RuntimeEventKind::SendDispatchAttempted {
                    target_shard,
                    ..
                } if target_shard == ShardId::new(11)
            )
    });
    let coordinator_mailbox = event_id(run.artifact.event_record(), |event| {
        event.shard() == ShardId::new(11)
            && matches!(event.kind(), RuntimeEventKind::MailboxAccepted)
            && event
                .cause()
                .is_some_and(|cause| cause.event().get() == reply_attempt)
    });

    assert!(request_attempt < request_accept);
    assert!(request_accept < worker_mailbox);
    assert!(worker_mailbox < reply_attempt);
    assert!(reply_attempt < coordinator_mailbox);
}

#[test]
fn multishard_dispatcher_workload_continues_after_bad_remote_address_on_same_shard() {
    let mut sim =
        MultiShardSimulator::new([WorkShard(11), WorkShard(22)], SimulatorConfig::default());
    let completed = Rc::new(RefCell::new(Vec::new()));

    let worker = sim.register_with_capacity_on::<Worker, WorkerEvent, CoordinatorEvent>(
        ShardId::new(22),
        Worker,
        4,
    );
    let bad_worker = Address::new_with_generation(
        ShardId::new(22),
        IsolateId::new(999),
        AddressGeneration::new(0),
    );
    let coordinator = sim.register_with_capacity_on::<Coordinator, CoordinatorEvent, WorkerEvent>(
        ShardId::new(11),
        Coordinator {
            worker,
            bad_worker: Some(bad_worker),
            completed: Rc::clone(&completed),
        },
        4,
    );

    sim.try_send(
        coordinator,
        CoordinatorEvent::SubmitAfterBadRemote {
            job_id: 31,
            value: 4,
        },
    )
    .unwrap();

    assert!(sim.run_until_quiescent() > 0);
    assert_eq!(&*completed.borrow(), &[(31, 8)]);

    let trace = sim.trace();
    let bad_rejection = event_id(&trace, |event| {
        event.shard() == ShardId::new(22)
            && matches!(
                event.kind(),
                RuntimeEventKind::SendRejected {
                    target_isolate,
                    reason: SendRejectedReason::Closed,
                    ..
                } if target_isolate == bad_worker.isolate()
            )
    });
    let good_accept = event_id(&trace, |event| {
        event.shard() == ShardId::new(22)
            && event.isolate() == worker.isolate()
            && matches!(event.kind(), RuntimeEventKind::MailboxAccepted)
    });
    let worker_handler = event_id(&trace, |event| {
        event.shard() == ShardId::new(22)
            && event.isolate() == worker.isolate()
            && matches!(event.kind(), RuntimeEventKind::HandlerStarted)
    });

    assert!(bad_rejection < good_accept);
    assert!(good_accept < worker_handler);
}

#[test]
fn multishard_dispatcher_workload_surfaces_source_time_full_rejection() {
    let run = run_dispatcher_workload(
        SimulatorConfig::default(),
        MultiShardSimulatorConfig {
            shard_pair_capacity: 1,
        },
        CoordinatorEvent::SubmitPair {
            first_job: 1,
            first_value: 10,
            second_job: 2,
            second_value: 20,
        },
    );

    assert_eq!(run.completed, vec![(1, 20)]);

    let full_rejections = run
        .artifact
        .event_record()
        .iter()
        .filter(|event| {
            event.shard() == ShardId::new(11)
                && matches!(
                    event.kind(),
                    RuntimeEventKind::SendRejected {
                        reason: SendRejectedReason::Full,
                        ..
                    }
                )
        })
        .count();
    assert_eq!(full_rejections, 1);
}

#[test]
fn terminal_reply_lane_bypasses_saturated_ordinary_remote_queue() {
    let mut sim = MultiShardSimulator::with_config(
        [WorkShard(11), WorkShard(22)],
        SimulatorConfig::default(),
        MultiShardSimulatorConfig {
            shard_pair_capacity: 1,
        },
    );
    let outcomes = Rc::new(RefCell::new(Vec::new()));
    let noise = Rc::new(RefCell::new(0usize));
    let order = Rc::new(RefCell::new(Vec::new()));
    let worker = sim.register_with_capacity_on::<
        TerminalLaneWorker,
        TerminalLaneWorkerMsg,
        TerminalLaneCallerMsg,
    >(ShardId::new(22), TerminalLaneWorker, 4);
    let caller = sim.register_with_capacity_on::<
        TerminalLaneCaller,
        TerminalLaneCallerMsg,
        TerminalLaneWorkerMsg,
    >(
        ShardId::new(11),
        TerminalLaneCaller {
            worker,
            outcomes: Rc::clone(&outcomes),
            noise: Rc::clone(&noise),
            order: Rc::clone(&order),
        },
        4,
    );

    sim.try_send(caller, TerminalLaneCallerMsg::Start).unwrap();
    assert!(sim.run_until_quiescent() > 0);

    assert_eq!(*noise.borrow(), 1);
    assert_eq!(
        &*outcomes.borrow(),
        &[CallOutcome::Replied(TerminalLaneReply(42))]
    );
    assert_eq!(
        &*order.borrow(),
        &["terminal-reply", "ordinary-noise"],
        "simulator terminal replies must drain before ordinary remote traffic visible to the same caller"
    );
    assert!(
        sim.trace().iter().any(|event| {
            matches!(
                event.kind(),
                RuntimeEventKind::CallCompleted {
                    call_kind: CallKind::IsolateCall,
                    ..
                }
            )
        }),
        "simulator must record the user-visible reply as a real call completion"
    );
    assert!(
        !sim.trace().iter().any(|event| {
            matches!(
                event.kind(),
                RuntimeEventKind::CallReplyRejected { .. }
                    | RuntimeEventKind::CallReplyAbandoned { .. }
            )
        }),
        "simulator terminal lane should not convert a deliverable reply into a rejected or abandoned trace"
    );
}

#[test]
fn terminal_reply_lane_records_one_terminal_call_fact_for_user_call() {
    let mut sim = MultiShardSimulator::with_config(
        [WorkShard(11), WorkShard(22)],
        SimulatorConfig::default(),
        MultiShardSimulatorConfig {
            shard_pair_capacity: 1,
        },
    );
    let outcomes = Rc::new(RefCell::new(Vec::new()));
    let noise = Rc::new(RefCell::new(0usize));
    let order = Rc::new(RefCell::new(Vec::new()));
    let worker = sim.register_with_capacity_on::<
        TerminalLaneWorker,
        TerminalLaneWorkerMsg,
        TerminalLaneCallerMsg,
    >(ShardId::new(22), TerminalLaneWorker, 4);
    let caller = sim.register_with_capacity_on::<
        TerminalLaneCaller,
        TerminalLaneCallerMsg,
        TerminalLaneWorkerMsg,
    >(
        ShardId::new(11),
        TerminalLaneCaller {
            worker,
            outcomes: Rc::clone(&outcomes),
            noise: Rc::clone(&noise),
            order,
        },
        4,
    );

    sim.try_send(caller, TerminalLaneCallerMsg::Start).unwrap();
    assert!(sim.run_until_quiescent() > 0);

    assert_eq!(
        &*outcomes.borrow(),
        &[CallOutcome::Replied(TerminalLaneReply(42))]
    );
    let call_completed = sim
        .trace()
        .iter()
        .filter(|event| {
            matches!(
                event.kind(),
                RuntimeEventKind::CallCompleted {
                    call_kind: CallKind::IsolateCall,
                    ..
                }
            )
        })
        .count();
    let reply_terminal_failures = sim
        .trace()
        .iter()
        .filter(|event| {
            matches!(
                event.kind(),
                RuntimeEventKind::CallReplyRejected { .. }
                    | RuntimeEventKind::CallReplyAbandoned { .. }
            )
        })
        .count();
    assert_eq!(call_completed, 1);
    assert_eq!(reply_terminal_failures, 0);
}

#[test]
fn multishard_dispatcher_workload_replays_from_saved_config() {
    let saved = run_dispatcher_workload(
        SimulatorConfig::default(),
        MultiShardSimulatorConfig {
            shard_pair_capacity: 4,
        },
        CoordinatorEvent::Submit {
            job_id: 11,
            value: 5,
        },
    );

    let replayed = run_dispatcher_workload(
        saved.artifact.simulator_config().clone(),
        saved.artifact.multishard_config(),
        CoordinatorEvent::Submit {
            job_id: 11,
            value: 5,
        },
    );

    assert_eq!(replayed.completed, saved.completed);
    assert_eq!(replayed.artifact, saved.artifact);
}

#[test]
fn multishard_dispatcher_composes_with_seeded_timer_faults() {
    let run = run_timed_dispatcher_workload(
        SimulatorConfig {
            seed: 17,
            faults: FaultConfig {
                timer_wake: FaultMode::DelayBy {
                    one_in: 2,
                    by: Duration::from_millis(7),
                },
                ..Default::default()
            },
            ..Default::default()
        },
        MultiShardSimulatorConfig {
            shard_pair_capacity: 4,
        },
    );

    assert_eq!(run.completed, vec![(9, 14)]);
    assert!(
        run.artifact.event_record().iter().any(|event| {
            matches!(
                event.kind(),
                RuntimeEventKind::CallCompleted {
                    call_kind: CallKind::Sleep,
                    ..
                }
            )
        }),
        "timer-gated multi-shard workload should still prove that remote work started from a runtime-owned sleep"
    );
}

#[test]
fn same_non_default_seed_faulted_multishard_dispatcher_replays_same_artifact() {
    let config = SimulatorConfig {
        seed: 17,
        faults: FaultConfig {
            timer_wake: FaultMode::DelayBy {
                one_in: 2,
                by: Duration::from_millis(7),
            },
            ..Default::default()
        },
        ..Default::default()
    };

    let first = run_timed_dispatcher_workload(
        config.clone(),
        MultiShardSimulatorConfig {
            shard_pair_capacity: 4,
        },
    );
    let second = run_timed_dispatcher_workload(
        config,
        MultiShardSimulatorConfig {
            shard_pair_capacity: 4,
        },
    );

    assert_eq!(first.completed, second.completed);
    assert_eq!(first.artifact, second.artifact);
}

#[test]
fn different_seeds_can_diverge_in_faulted_multishard_dispatcher_replay() {
    // Under the splitmix64-mixed selector (G2), seed 19's ordinal-0
    // timer-wake decision fires the delay and seed 17's does not, so
    // their final times diverge. Pre-G2 this was seeds 17 vs 18, which
    // diverged only via the `ordinal == 0 -> seed % modulus`
    // short-circuit the fix removed (both now resolve identically).
    let delayed = SimulatorConfig {
        seed: 19,
        faults: FaultConfig {
            timer_wake: FaultMode::DelayBy {
                one_in: 2,
                by: Duration::from_millis(7),
            },
            ..Default::default()
        },
        ..Default::default()
    };
    let baseline = SimulatorConfig {
        seed: 17,
        faults: delayed.faults,
        ..Default::default()
    };

    let delayed_run = run_timed_dispatcher_workload(
        delayed,
        MultiShardSimulatorConfig {
            shard_pair_capacity: 4,
        },
    );
    let baseline_run = run_timed_dispatcher_workload(
        baseline,
        MultiShardSimulatorConfig {
            shard_pair_capacity: 4,
        },
    );

    assert_eq!(delayed_run.completed, baseline_run.completed);
    assert_ne!(
        delayed_run.artifact.final_time(),
        baseline_run.artifact.final_time(),
        "different non-default seeds should be able to perturb timer-gated multi-shard timing"
    );
}

#[test]
fn multishard_tcp_workload_composes_with_seeded_tcp_completion_faults() {
    let mut sim = MultiShardSimulator::with_config(
        [WorkShard(11), WorkShard(22)],
        SimulatorConfig {
            seed: 33,
            faults: FaultConfig {
                tcp_completion: TcpCompletionFaultMode::DelayBySteps {
                    one_in: 1,
                    steps: 2,
                },
                ..Default::default()
            },
            tcp: ScriptedTcpConfig {
                pending_completion_capacity: 16,
                listeners: vec![ScriptedListenerConfig {
                    bind_addr: bind_addr(),
                    local_addr: local_addr(50000),
                    backlog_capacity: 2,
                    peers: vec![
                        peer_script(1, peer_addr(61001), vec![b"alpha".to_vec()], Some(2), 2),
                        peer_script(1, peer_addr(61002), vec![b"beta".to_vec()], Some(2), 2),
                    ],
                }],
            },
            storage: Default::default(),
            ..Default::default()
        },
        MultiShardSimulatorConfig {
            shard_pair_capacity: 8,
        },
    );
    let done = Rc::new(RefCell::new(0usize));

    let coordinator = sim.register_with_capacity_on::<TcpCoordinator, TcpControlEvent, Infallible>(
        ShardId::new(11),
        TcpCoordinator {
            done: Rc::clone(&done),
        },
        4,
    );
    let listener = sim
        .register_with_capacity_on::<TcpEchoListener, TcpControlEvent, TcpControlEvent>(
            ShardId::new(22),
            TcpEchoListener {
                bind_addr: bind_addr(),
                target_accepts: 2,
                accepted: 0,
                listener: None,
                report_to: coordinator,
            },
            8,
        );

    sim.try_send(listener, TcpControlEvent::Bootstrap).unwrap();
    sim.run_until_quiescent();

    assert_eq!(*done.borrow(), 1);
    assert_eq!(
        sim.replay_artifact()
            .observed_peer_output()
            .iter()
            .map(ObservedPeerOutput::bytes)
            .collect::<Vec<_>>(),
        vec![b"alpha".as_slice(), b"beta".as_slice()]
    );
    assert!(
        sim.trace().iter().any(|event| {
            event.shard() == ShardId::new(11)
                && matches!(event.kind(), RuntimeEventKind::MailboxAccepted)
        }),
        "listener completion should cross shards and become visible to the coordinator"
    );
}

#[test]
fn multishard_tcp_persistence_service_replays_under_seeded_dst_faults() {
    let config = SimulatorConfig {
        seed: 91,
        faults: FaultConfig {
            local_send: tina_sim::LocalSendFaultMode::DelayByRounds {
                one_in: 2,
                rounds: 1,
            },
            tcp_completion: TcpCompletionFaultMode::DelayBySteps {
                one_in: 1,
                steps: 2,
            },
            ..Default::default()
        },
        tcp: ScriptedTcpConfig {
            pending_completion_capacity: 16,
            listeners: vec![ScriptedListenerConfig {
                bind_addr: bind_addr(),
                local_addr: local_addr(50100),
                backlog_capacity: 1,
                peers: vec![peer_script(
                    1,
                    peer_addr(61100),
                    vec![b"grain".to_vec()],
                    None,
                    64,
                )],
            }],
        },
        storage: Default::default(),
        ..Default::default()
    };

    let first = run_durable_tcp_multishard_workload(config.clone());
    let second = run_durable_tcp_multishard_workload(config);

    assert_eq!(first.artifact, second.artifact);
    assert_eq!(
        first
            .artifact
            .observed_peer_output()
            .iter()
            .map(ObservedPeerOutput::bytes)
            .collect::<Vec<_>>(),
        vec![b"stored:grain".as_slice()]
    );

    let replay = tina_runtime::persistence::replay_journal_bytes(
        first
            .artifact
            .durable_image()
            .get(durable_journal_path())
            .expect("durable journal exists in replay image"),
    )
    .expect("journal image replays");
    assert_eq!(replay.records.len(), 1);
    assert_eq!(replay.records[0].index, 1);
    assert_eq!(replay.records[0].bytes, b"grain");

    let trace = first.artifact.event_record();
    let tcp_read = event_id(trace, |event| {
        event.shard() == ShardId::new(11)
            && matches!(
                event.kind(),
                RuntimeEventKind::CallCompleted {
                    call_kind: CallKind::TcpRead,
                    ..
                }
            )
    });
    let request_attempt = event_id(trace, |event| {
        event.shard() == ShardId::new(11)
            && matches!(
                event.kind(),
                RuntimeEventKind::SendDispatchAttempted {
                    target_shard,
                    ..
                } if target_shard == ShardId::new(22)
            )
    });
    let journal_appended = event_id(trace, |event| {
        event.shard() == ShardId::new(22)
            && matches!(event.kind(), RuntimeEventKind::JournalAppended { .. })
    });
    let ack_attempt = event_id(trace, |event| {
        event.shard() == ShardId::new(22)
            && matches!(
                event.kind(),
                RuntimeEventKind::SendDispatchAttempted {
                    target_shard,
                    ..
                } if target_shard == ShardId::new(11)
            )
    });
    let tcp_write = event_id(trace, |event| {
        event.shard() == ShardId::new(11)
            && matches!(
                event.kind(),
                RuntimeEventKind::CallCompleted {
                    call_kind: CallKind::TcpWrite,
                    ..
                }
            )
    });

    assert!(tcp_read < request_attempt);
    assert!(request_attempt < journal_appended);
    assert!(journal_appended < ack_attempt);
    assert!(ack_attempt < tcp_write);
}

#[test]
fn multishard_tcp_persistence_service_handles_overlap_partial_io_and_seed_sweep() {
    let payloads = vec![b"alpha".to_vec(), b"beta".to_vec(), b"gamma".to_vec()];
    let expected_chunks = expected_journal_chunks(&payloads, 2);
    let mut expected_outputs = payloads
        .iter()
        .map(|payload| expected_stored_output(payload, 2))
        .collect::<Vec<_>>();
    expected_outputs.sort();

    for seed in [0, 1, 7, 19, 91] {
        let first = run_durable_batch_tcp_multishard_workload(seed, &payloads);
        let second = run_durable_batch_tcp_multishard_workload(seed, &payloads);

        assert_eq!(
            first.artifact, second.artifact,
            "same seed should replay exactly for seed {seed}"
        );

        let mut observed_outputs = first
            .artifact
            .observed_peer_output()
            .iter()
            .map(|output| output.bytes().to_vec())
            .collect::<Vec<_>>();
        observed_outputs.sort();
        assert_eq!(observed_outputs, expected_outputs);

        let replay = tina_runtime::persistence::replay_journal_bytes(
            first
                .artifact
                .durable_image()
                .get(durable_batch_journal_path(seed))
                .expect("durable batch journal exists in replay image"),
        )
        .expect("batch journal image replays");
        assert_eq!(replay.records.len(), expected_chunks.len());
        assert_eq!(
            replay
                .records
                .iter()
                .map(|record| record.index)
                .collect::<Vec<_>>(),
            (1..=expected_chunks.len() as u64).collect::<Vec<_>>()
        );

        let mut observed_chunks = replay
            .records
            .iter()
            .map(|record| record.bytes.clone())
            .collect::<Vec<_>>();
        observed_chunks.sort();
        let mut sorted_expected_chunks = expected_chunks.clone();
        sorted_expected_chunks.sort();
        assert_eq!(observed_chunks, sorted_expected_chunks);

        let trace = first.artifact.event_record();
        let journal_appended_count = trace
            .iter()
            .filter(|event| matches!(event.kind(), RuntimeEventKind::JournalAppended { .. }))
            .count();
        let tcp_write_count = trace
            .iter()
            .filter(|event| {
                matches!(
                    event.kind(),
                    RuntimeEventKind::CallCompleted {
                        call_kind: CallKind::TcpWrite,
                        ..
                    }
                )
            })
            .count();
        let request_attempts = trace
            .iter()
            .filter(|event| {
                event.shard() == ShardId::new(11)
                    && matches!(
                        event.kind(),
                        RuntimeEventKind::SendDispatchAttempted {
                            target_shard,
                            ..
                        } if target_shard == ShardId::new(22)
                    )
            })
            .count();
        let ack_attempts = trace
            .iter()
            .filter(|event| {
                event.shard() == ShardId::new(22)
                    && matches!(
                        event.kind(),
                        RuntimeEventKind::SendDispatchAttempted {
                            target_shard,
                            ..
                        } if target_shard == ShardId::new(11)
                    )
            })
            .count();

        assert_eq!(journal_appended_count, expected_chunks.len());
        assert_eq!(request_attempts, expected_chunks.len());
        assert_eq!(ack_attempts, expected_chunks.len());
        assert!(
            tcp_write_count > expected_chunks.len(),
            "write cap should force partial write completions for seed {seed}"
        );
    }
}

#[test]
fn multishard_supervision_workload_composes_with_seeded_local_send_delay() {
    let mut sim = MultiShardSimulator::with_config(
        [WorkShard(11), WorkShard(22)],
        SimulatorConfig {
            seed: 5,
            faults: FaultConfig {
                local_send: tina_sim::LocalSendFaultMode::DelayByRounds {
                    one_in: 1,
                    rounds: 2,
                },
                ..Default::default()
            },
            ..Default::default()
        },
        MultiShardSimulatorConfig {
            shard_pair_capacity: 8,
        },
    );
    let log = Rc::new(RefCell::new(Vec::new()));

    let observer = sim
        .register_with_capacity_on::<SupervisorObserver, SupervisorObservation, Infallible>(
            ShardId::new(11),
            SupervisorObserver {
                log: Rc::clone(&log),
            },
            16,
        );
    let parent = sim.register_with_capacity_on::<SupervisedParent, SupervisorEvent, Infallible>(
        ShardId::new(22),
        SupervisedParent { observer },
        8,
    );
    sim.supervise(
        parent,
        SupervisorConfig::new(RestartPolicy::OneForOne, RestartBudget::new(2)),
    );

    sim.try_send(parent, SupervisorEvent::SpawnOne).unwrap();
    sim.run_until_quiescent();

    let first = spawned_children(&sim.trace())[0];
    sim.try_send(
        Address::new(ShardId::new(22), first),
        RestartableWorkerEvent::Poison,
    )
    .unwrap();
    sim.run_until_quiescent();

    let replacement = completed_restarts(&sim.trace())[0].1;
    sim.try_send(
        Address::new(ShardId::new(22), replacement),
        RestartableWorkerEvent::Work(99),
    )
    .unwrap();
    sim.run_until_quiescent();

    let observed = log.borrow().clone();
    assert!(observed.contains(&SupervisorObservation::Booted(first)));
    assert!(observed.contains(&SupervisorObservation::Booted(replacement)));
    assert!(observed.contains(&SupervisorObservation::Worked(replacement, 99)));
}
