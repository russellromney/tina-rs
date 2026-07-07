use std::collections::VecDeque;
use std::convert::Infallible;
use std::fs;
use std::io::{Read, Write};
use std::net::{Shutdown, SocketAddr, TcpStream};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use tina::{Address, Mailbox, TrySendError, prelude::*};
use tina_runtime::{
    CallError, CallKind, CallOutcome, FileId, ListenerId, LiveShardState, LocalSystem,
    LocalSystemState, MailboxFactory, ProcessRunResult, RuntimeEventKind, SendOutcome, StreamId,
    ThreadedRuntimeError, TraceRetention, call, dns_lookup, file_create, file_read, file_write,
    journal_append, process_run, send_observed, sleep_then, tcp_accept, tcp_bind,
    tcp_close_listener, tcp_close_stream, tcp_read, tcp_write,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct ServiceShard(u32);

impl Shard for ServiceShard {
    fn id(&self) -> ShardId {
        ShardId::new(self.0)
    }
}

struct ServiceMailbox<T> {
    capacity: usize,
    queue: Mutex<VecDeque<T>>,
    closed: Mutex<bool>,
}

impl<T> ServiceMailbox<T> {
    fn new(capacity: usize) -> Self {
        Self {
            capacity,
            queue: Mutex::new(VecDeque::new()),
            closed: Mutex::new(false),
        }
    }
}

impl<T> Mailbox<T> for ServiceMailbox<T> {
    fn capacity(&self) -> usize {
        self.capacity
    }

    fn try_send(&self, message: T) -> Result<(), TrySendError<T>> {
        if *self.closed.lock().expect("closed lock") {
            return Err(TrySendError::Closed(message));
        }
        let mut queue = self.queue.lock().expect("queue lock");
        if queue.len() >= self.capacity {
            return Err(TrySendError::Full(message));
        }
        queue.push_back(message);
        Ok(())
    }

    fn recv(&self) -> Option<T> {
        self.queue.lock().expect("queue lock").pop_front()
    }
    fn is_empty(&self) -> bool {
        self.queue.lock().expect("queue lock").is_empty()
    }

    fn close(&self) {
        *self.closed.lock().expect("closed lock") = true;
    }
}

#[derive(Debug, Clone, Copy)]
struct ServiceMailboxFactory;

impl MailboxFactory for ServiceMailboxFactory {
    fn create<T: 'static>(&self, capacity: usize) -> Box<dyn Mailbox<T>> {
        Box::new(ServiceMailbox::new(capacity))
    }
}

#[derive(Debug, Clone, Copy)]
struct CapacityPanicMailboxFactory {
    panic_capacity: usize,
}

impl MailboxFactory for CapacityPanicMailboxFactory {
    fn create<T: 'static>(&self, capacity: usize) -> Box<dyn Mailbox<T>> {
        if capacity == self.panic_capacity {
            panic!("Baobab test mailbox factory panic for capacity {capacity}");
        }
        Box::new(ServiceMailbox::new(capacity))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ServiceRequest {
    key: u64,
    body: String,
}

#[derive(Debug)]
enum WorkerMsg {
    Work(ServiceRequest),
    Journaled(Result<(), CallError>, ServiceRequest, u64),
    JournaledForCall(
        tina::RequestContext<WorkerReply>,
        Result<(), CallError>,
        ServiceRequest,
        u64,
    ),
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum WorkerReply {
    Stored { key: u64, body: String },
    DurableFailure(CallError),
}

#[derive(Debug)]
struct DurableWorker {
    journal_path: PathBuf,
    next_index: u64,
    observed: Arc<Mutex<Vec<ServiceObservation>>>,
}

#[tina_runtime::isolate(
    message = WorkerMsg,
    reply = WorkerReply,
    io = tina_runtime::RuntimeCall<WorkerMsg>,
    shard = ServiceShard
)]
impl DurableWorker {
    fn handle(
        &mut self,
        msg: WorkerMsg,
        _ctx: &mut Context<'_, ServiceShard, Self::Reply>,
    ) -> Effect<Self> {
        match msg {
            WorkerMsg::Work(request) => {
                let index = self.next_index;
                journal_append(
                    self.journal_path.clone(),
                    index,
                    format!("{}:{}", request.key, request.body).into_bytes(),
                )
                .then(move |result| WorkerMsg::Journaled(result, request, index))
            }
            WorkerMsg::Journaled(Ok(()), request, index) => {
                self.next_index = index + 1;
                self.observed
                    .lock()
                    .expect("worker observed lock")
                    .push(ServiceObservation::WorkerStored(request.key));
                reply(WorkerReply::Stored {
                    key: request.key,
                    body: request.body,
                })
            }
            WorkerMsg::Journaled(Err(error), _, _) => reply(WorkerReply::DurableFailure(error)),
            WorkerMsg::JournaledForCall(req, Ok(()), request, index) => {
                self.next_index = index + 1;
                self.observed
                    .lock()
                    .expect("worker observed lock")
                    .push(ServiceObservation::WorkerStored(request.key));
                tina::reply_to_request(
                    req,
                    WorkerReply::Stored {
                        key: request.key,
                        body: request.body,
                    },
                )
            }
            WorkerMsg::JournaledForCall(req, Err(error), _, _) => {
                tina::reply_to_request(req, WorkerReply::DurableFailure(error))
            }
        }
    }

    fn handle_call(&mut self, msg: WorkerMsg, call: tina::CallContext<'_, Self>) -> Effect<Self> {
        match msg {
            WorkerMsg::Work(request) => {
                let req = call.into_request_context();
                let index = self.next_index;
                journal_append(
                    self.journal_path.clone(),
                    index,
                    format!("{}:{}", request.key, request.body).into_bytes(),
                )
                .then(move |result| WorkerMsg::JournaledForCall(req, result, request, index))
            }
            WorkerMsg::Journaled(_, _, _) | WorkerMsg::JournaledForCall(_, _, _, _) => {
                call.reject(tina::CallRejectedReason::UnsupportedMessage)
            }
        }
    }
}

#[derive(Debug, Clone)]
enum RouterMsg {
    Submit(ServiceRequest),
    Retry(ServiceRequest, usize),
    Routed(ServiceRequest, usize, CallOutcome<WorkerReply>),
    RouteToWrongShard(ServiceRequest, Address<WorkerMsg, WorkerReply>),
}

#[derive(Debug)]
struct Router {
    even_worker: Address<WorkerMsg, WorkerReply>,
    odd_worker: Address<WorkerMsg, WorkerReply>,
    max_retries: usize,
    observed: Arc<Mutex<Vec<ServiceObservation>>>,
}

#[tina_runtime::isolate(
    message = RouterMsg,
    io = tina_runtime::RuntimeCall<RouterMsg>,
    shard = ServiceShard
)]
impl Router {
    fn handle(
        &mut self,
        msg: RouterMsg,
        _ctx: &mut Context<'_, ServiceShard, Self::Reply>,
    ) -> Effect<Self> {
        match msg {
            RouterMsg::Submit(request) => self.route(request, 0),
            RouterMsg::Retry(request, attempt) => {
                self.observed.lock().expect("router observed lock").push(
                    ServiceObservation::RetryWoke {
                        key: request.key,
                        attempt,
                    },
                );
                self.route(request, attempt)
            }
            RouterMsg::Routed(request, attempt, outcome) => match outcome {
                CallOutcome::Replied(WorkerReply::Stored { body, .. }) => {
                    self.observed.lock().expect("router observed lock").push(
                        ServiceObservation::Accepted {
                            key: request.key,
                            body,
                        },
                    );
                    noop()
                }
                CallOutcome::Replied(WorkerReply::DurableFailure(error)) => {
                    self.observed.lock().expect("router observed lock").push(
                        ServiceObservation::Rejected {
                            key: request.key,
                            reason: format!("durable:{error:?}"),
                        },
                    );
                    noop()
                }
                CallOutcome::Full if attempt < self.max_retries => {
                    self.observed.lock().expect("router observed lock").push(
                        ServiceObservation::BackoffScheduled {
                            key: request.key,
                            attempt: attempt + 1,
                        },
                    );
                    sleep_then(
                        Duration::from_millis(1),
                        RouterMsg::Retry(request, attempt + 1),
                    )
                }
                CallOutcome::Full => {
                    self.observed.lock().expect("router observed lock").push(
                        ServiceObservation::Rejected {
                            key: request.key,
                            reason: "busy".to_string(),
                        },
                    );
                    noop()
                }
                CallOutcome::Closed => {
                    self.observed.lock().expect("router observed lock").push(
                        ServiceObservation::Rejected {
                            key: request.key,
                            reason: "closed".to_string(),
                        },
                    );
                    noop()
                }
                CallOutcome::Rejected(reason) => {
                    self.observed.lock().expect("router observed lock").push(
                        ServiceObservation::Rejected {
                            key: request.key,
                            reason: format!("rejected:{reason:?}"),
                        },
                    );
                    noop()
                }
                CallOutcome::Timeout => {
                    self.observed.lock().expect("router observed lock").push(
                        ServiceObservation::Rejected {
                            key: request.key,
                            reason: "timeout".to_string(),
                        },
                    );
                    noop()
                }
            },
            RouterMsg::RouteToWrongShard(request, target) => {
                let expected = self.expected_worker(request.key);
                if target.shard() != expected.shard() {
                    self.observed.lock().expect("router observed lock").push(
                        ServiceObservation::Rejected {
                            key: request.key,
                            reason: "wrong-shard".to_string(),
                        },
                    );
                    noop()
                } else {
                    call(
                        target,
                        WorkerMsg::Work(request.clone()),
                        Duration::from_secs(1),
                    )
                    .then(move |outcome| RouterMsg::Routed(request, 0, outcome))
                }
            }
        }
    }
}

impl Router {
    fn route(&self, request: ServiceRequest, attempt: usize) -> Effect<Self> {
        let target = self.expected_worker(request.key);
        call(
            target,
            WorkerMsg::Work(request.clone()),
            Duration::from_secs(1),
        )
        .then(move |outcome| RouterMsg::Routed(request, attempt, outcome))
    }

    fn expected_worker(&self, key: u64) -> Address<WorkerMsg, WorkerReply> {
        if key % 2 == 0 {
            self.even_worker
        } else {
            self.odd_worker
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum ServiceObservation {
    WorkerStored(u64),
    AuditStored(u64),
    Accepted { key: u64, body: String },
    BackoffScheduled { key: u64, attempt: usize },
    RetryWoke { key: u64, attempt: usize },
    Rejected { key: u64, reason: String },
}

#[derive(Debug, Clone)]
enum ReadinessMsg {
    Start,
    TcpBound(Result<(ListenerId, SocketAddr), CallError>),
    TcpAccepted(Result<(StreamId, SocketAddr), CallError>),
    TcpRead(Result<Vec<u8>, CallError>, StreamId),
    TcpWrote(Result<usize, CallError>, StreamId),
    TcpStreamClosed(Result<(), CallError>),
    TcpListenerClosed(Result<(), CallError>),
    TcpAbortStreamClosed(Result<(), CallError>),
    TcpAbortListenerClosed(Result<(), CallError>),
    TimerReady,
    Dns(Result<Vec<std::net::SocketAddr>, CallError>),
    Process(Result<ProcessRunResult, CallError>),
    FileOpened(Result<FileId, CallError>),
    FileWritten(FileId, Result<usize, CallError>),
    FileRead(Result<Vec<u8>, CallError>),
    Journaled(Result<(), CallError>),
    Worker(CallOutcome<WorkerReply>),
}

#[derive(Debug)]
struct ReadinessService {
    file_path: PathBuf,
    journal_path: PathBuf,
    worker: Address<WorkerMsg, WorkerReply>,
    published: Arc<Mutex<Option<SocketAddr>>>,
    observed: Arc<Mutex<Vec<ServiceObservation>>>,
    listener: Option<ListenerId>,
    tcp_buffer: Vec<u8>,
}

const READINESS_MAX_FRAME: usize = 64;

#[tina_runtime::isolate(
    message = ReadinessMsg,
    io = tina_runtime::RuntimeCall<ReadinessMsg>,
    shard = ServiceShard
)]
impl ReadinessService {
    fn handle(
        &mut self,
        msg: ReadinessMsg,
        _ctx: &mut Context<'_, ServiceShard, Self::Reply>,
    ) -> Effect<Self> {
        match msg {
            ReadinessMsg::Start => tcp_bind("127.0.0.1:0".parse().expect("readiness bind addr"))
                .then(ReadinessMsg::TcpBound),
            ReadinessMsg::TcpBound(Ok((listener, addr))) => {
                self.listener = Some(listener);
                *self.published.lock().expect("readiness published lock") = Some(addr);
                tcp_accept(listener).then(ReadinessMsg::TcpAccepted)
            }
            ReadinessMsg::TcpBound(Err(error)) => {
                self.record_rejection(50, format!("tcp-bind:{error:?}"));
                noop()
            }
            ReadinessMsg::TcpAccepted(Ok((stream, _peer))) => {
                self.tcp_buffer.clear();
                tcp_read(stream, 4).then(move |result| ReadinessMsg::TcpRead(result, stream))
            }
            ReadinessMsg::TcpAccepted(Err(error)) => {
                self.record_rejection(50, format!("tcp-accept:{error:?}"));
                noop()
            }
            ReadinessMsg::TcpRead(Ok(bytes), stream) if bytes.is_empty() => {
                self.record_rejection(50, "tcp-eof-before-frame");
                tcp_close_stream(stream).then(ReadinessMsg::TcpAbortStreamClosed)
            }
            ReadinessMsg::TcpRead(Ok(bytes), stream) => {
                self.tcp_buffer.extend_from_slice(&bytes);
                match self.tcp_buffer.iter().position(|byte| *byte == b'\n') {
                    Some(newline)
                        if newline <= READINESS_MAX_FRAME
                            && &self.tcp_buffer[..newline] == b"baobab-ready" =>
                    {
                        tcp_write(stream, b"readiness-ok\n".to_vec())
                            .then(move |result| ReadinessMsg::TcpWrote(result, stream))
                    }
                    Some(_) => {
                        self.record_rejection(50, "tcp-frame");
                        tcp_close_stream(stream).then(ReadinessMsg::TcpAbortStreamClosed)
                    }
                    None if self.tcp_buffer.len() > READINESS_MAX_FRAME => {
                        self.record_rejection(50, "tcp-frame-too-large");
                        tcp_close_stream(stream).then(ReadinessMsg::TcpAbortStreamClosed)
                    }
                    None => tcp_read(stream, 4)
                        .then(move |result| ReadinessMsg::TcpRead(result, stream)),
                }
            }
            ReadinessMsg::TcpRead(Err(error), stream) => {
                self.record_rejection(50, format!("tcp-read:{error:?}"));
                tcp_close_stream(stream).then(ReadinessMsg::TcpAbortStreamClosed)
            }
            ReadinessMsg::TcpWrote(Ok(_), stream) => {
                tcp_close_stream(stream).then(ReadinessMsg::TcpStreamClosed)
            }
            ReadinessMsg::TcpWrote(Err(error), stream) => {
                self.record_rejection(50, format!("tcp-write:{error:?}"));
                tcp_close_stream(stream).then(ReadinessMsg::TcpAbortStreamClosed)
            }
            ReadinessMsg::TcpStreamClosed(Ok(())) => match self.listener.take() {
                Some(listener) => {
                    tcp_close_listener(listener).then(ReadinessMsg::TcpListenerClosed)
                }
                None => {
                    self.record_rejection(50, "tcp-listener-missing");
                    noop()
                }
            },
            ReadinessMsg::TcpStreamClosed(Err(error)) => {
                self.record_rejection(50, format!("tcp-close-stream:{error:?}"));
                match self.listener.take() {
                    Some(listener) => {
                        tcp_close_listener(listener).then(ReadinessMsg::TcpAbortListenerClosed)
                    }
                    None => noop(),
                }
            }
            ReadinessMsg::TcpListenerClosed(Ok(())) => {
                sleep_then(Duration::from_millis(1), ReadinessMsg::TimerReady)
            }
            ReadinessMsg::TcpListenerClosed(Err(error)) => {
                self.record_rejection(50, format!("tcp-close-listener:{error:?}"));
                noop()
            }
            ReadinessMsg::TcpAbortStreamClosed(result) => {
                if let Err(error) = result {
                    self.record_rejection(50, format!("tcp-abort-close-stream:{error:?}"));
                }
                match self.listener.take() {
                    Some(listener) => {
                        tcp_close_listener(listener).then(ReadinessMsg::TcpAbortListenerClosed)
                    }
                    None => noop(),
                }
            }
            ReadinessMsg::TcpAbortListenerClosed(result) => {
                if let Err(error) = result {
                    self.record_rejection(50, format!("tcp-abort-close-listener:{error:?}"));
                }
                noop()
            }
            ReadinessMsg::TimerReady => {
                dns_lookup("localhost", 80, Duration::from_secs(1)).then(ReadinessMsg::Dns)
            }
            ReadinessMsg::Dns(Ok(addresses)) if !addresses.is_empty() => process_run(
                "/bin/echo",
                vec!["baobab-ready".to_string()],
                Duration::from_secs(1),
                128,
                128,
            )
            .then(ReadinessMsg::Process),
            ReadinessMsg::Dns(Ok(_)) => {
                self.record_rejection(50, "dns-empty");
                noop()
            }
            ReadinessMsg::Dns(Err(error)) => {
                self.record_rejection(50, format!("dns:{error:?}"));
                noop()
            }
            ReadinessMsg::Process(Ok(result))
                if result.status.code == Some(0) && result.stdout.starts_with(b"baobab-ready") =>
            {
                file_create(self.file_path.clone()).then(ReadinessMsg::FileOpened)
            }
            ReadinessMsg::Process(Ok(_)) => {
                self.record_rejection(50, "process-output");
                noop()
            }
            ReadinessMsg::Process(Err(error)) => {
                self.record_rejection(50, format!("process:{error:?}"));
                noop()
            }
            ReadinessMsg::FileOpened(Ok(file)) => file_write(file, b"baobab".to_vec())
                .then(move |result| ReadinessMsg::FileWritten(file, result)),
            ReadinessMsg::FileOpened(Err(error)) => {
                self.record_rejection(50, format!("file-open:{error:?}"));
                noop()
            }
            ReadinessMsg::FileWritten(file, Ok(6)) => {
                file_read(file, 6).then(ReadinessMsg::FileRead)
            }
            ReadinessMsg::FileWritten(_, Ok(_)) => {
                self.record_rejection(50, "short-write");
                noop()
            }
            ReadinessMsg::FileWritten(_, Err(error)) => {
                self.record_rejection(50, format!("file-write:{error:?}"));
                noop()
            }
            ReadinessMsg::FileRead(Ok(bytes)) if bytes == b"baobab" => {
                journal_append(self.journal_path.clone(), 1, bytes).then(ReadinessMsg::Journaled)
            }
            ReadinessMsg::FileRead(Ok(_)) => {
                self.record_rejection(50, "file-read");
                noop()
            }
            ReadinessMsg::FileRead(Err(error)) => {
                self.record_rejection(50, format!("file-read:{error:?}"));
                noop()
            }
            ReadinessMsg::Journaled(Ok(())) => call(
                self.worker,
                WorkerMsg::Work(ServiceRequest {
                    key: 51,
                    body: "cross-shard-gauntlet".to_string(),
                }),
                Duration::from_secs(1),
            )
            .then(ReadinessMsg::Worker),
            ReadinessMsg::Journaled(Err(error)) => {
                self.record_rejection(50, format!("journal:{error:?}"));
                noop()
            }
            ReadinessMsg::Worker(CallOutcome::Replied(WorkerReply::Stored { .. })) => {
                self.observed.lock().expect("readiness observed lock").push(
                    ServiceObservation::Accepted {
                        key: 50,
                        body: "tcp-timer-dns-process-file-journal-cross-shard-call".to_string(),
                    },
                );
                noop()
            }
            ReadinessMsg::Worker(CallOutcome::Replied(WorkerReply::DurableFailure(error))) => {
                self.record_rejection(50, format!("worker-durable:{error:?}"));
                noop()
            }
            ReadinessMsg::Worker(CallOutcome::Full) => {
                self.record_rejection(50, "worker-full");
                noop()
            }
            ReadinessMsg::Worker(CallOutcome::Closed) => {
                self.record_rejection(50, "worker-closed");
                noop()
            }
            ReadinessMsg::Worker(CallOutcome::Rejected(reason)) => {
                self.record_rejection(50, format!("worker-rejected:{reason:?}"));
                noop()
            }
            ReadinessMsg::Worker(CallOutcome::Timeout) => {
                self.record_rejection(50, "worker-timeout");
                noop()
            }
        }
    }
}

impl ReadinessService {
    fn record_rejection(&self, key: u64, reason: impl Into<String>) {
        self.observed
            .lock()
            .expect("readiness observed lock")
            .push(ServiceObservation::Rejected {
                key,
                reason: reason.into(),
            });
    }
}

#[derive(Debug, Clone)]
enum AuditMsg {
    Record(u64),
}

#[derive(Debug)]
struct AuditSink {
    observed: Arc<Mutex<Vec<ServiceObservation>>>,
}

#[tina_runtime::isolate(message = AuditMsg, shard = ServiceShard)]
impl AuditSink {
    fn handle(
        &mut self,
        msg: AuditMsg,
        _ctx: &mut Context<'_, ServiceShard, Self::Reply>,
    ) -> Effect<Self> {
        match msg {
            AuditMsg::Record(key) => {
                self.observed
                    .lock()
                    .expect("audit observed lock")
                    .push(ServiceObservation::AuditStored(key));
                noop()
            }
        }
    }
}

#[derive(Debug)]
enum AuditedWorkerMsg {
    Work(ServiceRequest),
    AuditResult(ServiceRequest, SendOutcome),
    AuditResultForCall(
        tina::RequestContext<WorkerReply>,
        ServiceRequest,
        SendOutcome,
    ),
}

#[derive(Debug)]
struct AuditedWorker {
    audit: Address<AuditMsg>,
    observed: Arc<Mutex<Vec<ServiceObservation>>>,
}

#[tina_runtime::isolate(
    message = AuditedWorkerMsg,
    reply = WorkerReply,
    io = tina_runtime::RuntimeCall<AuditedWorkerMsg>,
    shard = ServiceShard
)]
impl AuditedWorker {
    fn handle(
        &mut self,
        msg: AuditedWorkerMsg,
        _ctx: &mut Context<'_, ServiceShard, Self::Reply>,
    ) -> Effect<Self> {
        match msg {
            AuditedWorkerMsg::Work(request) => {
                send_observed(self.audit, AuditMsg::Record(request.key))
                    .then(move |outcome| AuditedWorkerMsg::AuditResult(request, outcome))
            }
            AuditedWorkerMsg::AuditResult(request, SendOutcome::Accepted) => {
                self.observed
                    .lock()
                    .expect("audited worker observed lock")
                    .push(ServiceObservation::WorkerStored(request.key));
                reply(WorkerReply::Stored {
                    key: request.key,
                    body: request.body,
                })
            }
            AuditedWorkerMsg::AuditResult(_, SendOutcome::Full) => {
                reply(WorkerReply::DurableFailure(CallError::TargetFull))
            }
            AuditedWorkerMsg::AuditResult(_, SendOutcome::Closed) => {
                reply(WorkerReply::DurableFailure(CallError::TargetClosed))
            }
            AuditedWorkerMsg::AuditResultForCall(req, request, SendOutcome::Accepted) => {
                self.observed
                    .lock()
                    .expect("audited worker observed lock")
                    .push(ServiceObservation::WorkerStored(request.key));
                tina::reply_to_request(
                    req,
                    WorkerReply::Stored {
                        key: request.key,
                        body: request.body,
                    },
                )
            }
            AuditedWorkerMsg::AuditResultForCall(req, _, SendOutcome::Full) => {
                tina::reply_to_request(req, WorkerReply::DurableFailure(CallError::TargetFull))
            }
            AuditedWorkerMsg::AuditResultForCall(req, _, SendOutcome::Closed) => {
                tina::reply_to_request(req, WorkerReply::DurableFailure(CallError::TargetClosed))
            }
        }
    }

    fn handle_call(
        &mut self,
        msg: AuditedWorkerMsg,
        call: tina::CallContext<'_, Self>,
    ) -> Effect<Self> {
        match msg {
            AuditedWorkerMsg::Work(request) => {
                let req = call.into_request_context();
                send_observed(self.audit, AuditMsg::Record(request.key)).then(move |outcome| {
                    AuditedWorkerMsg::AuditResultForCall(req, request, outcome)
                })
            }
            AuditedWorkerMsg::AuditResult(_, _) | AuditedWorkerMsg::AuditResultForCall(_, _, _) => {
                call.reject(tina::CallRejectedReason::UnsupportedMessage)
            }
        }
    }
}

#[derive(Debug, Clone)]
enum AuditedClientMsg {
    Start(ServiceRequest),
    Returned(ServiceRequest, CallOutcome<WorkerReply>),
}

#[derive(Debug)]
struct AuditedClient {
    worker: Address<AuditedWorkerMsg, WorkerReply>,
    observed: Arc<Mutex<Vec<ServiceObservation>>>,
}

#[tina_runtime::isolate(
    message = AuditedClientMsg,
    io = tina_runtime::RuntimeCall<AuditedClientMsg>,
    shard = ServiceShard
)]
impl AuditedClient {
    fn handle(
        &mut self,
        msg: AuditedClientMsg,
        _ctx: &mut Context<'_, ServiceShard, Self::Reply>,
    ) -> Effect<Self> {
        match msg {
            AuditedClientMsg::Start(request) => call(
                self.worker,
                AuditedWorkerMsg::Work(request.clone()),
                Duration::from_secs(1),
            )
            .then(move |outcome| AuditedClientMsg::Returned(request, outcome)),
            AuditedClientMsg::Returned(request, outcome) => {
                let observation = match outcome {
                    CallOutcome::Replied(WorkerReply::Stored { body, .. }) => {
                        ServiceObservation::Accepted {
                            key: request.key,
                            body,
                        }
                    }
                    CallOutcome::Replied(WorkerReply::DurableFailure(error)) => {
                        ServiceObservation::Rejected {
                            key: request.key,
                            reason: format!("audit:{error:?}"),
                        }
                    }
                    CallOutcome::Full => ServiceObservation::Rejected {
                        key: request.key,
                        reason: "worker-full".to_string(),
                    },
                    CallOutcome::Closed => ServiceObservation::Rejected {
                        key: request.key,
                        reason: "worker-closed".to_string(),
                    },
                    CallOutcome::Rejected(reason) => ServiceObservation::Rejected {
                        key: request.key,
                        reason: format!("worker-rejected:{reason:?}"),
                    },
                    CallOutcome::Timeout => ServiceObservation::Rejected {
                        key: request.key,
                        reason: "worker-timeout".to_string(),
                    },
                };
                self.observed
                    .lock()
                    .expect("audited client observed lock")
                    .push(observation);
                noop()
            }
        }
    }
}

fn wait_until<F>(label: &str, predicate: F)
where
    F: Fn() -> bool,
{
    let deadline = Instant::now() + Duration::from_secs(5);
    while !predicate() {
        if Instant::now() > deadline {
            panic!("wait_until({label}) timed out");
        }
        std::thread::yield_now();
    }
}

fn wait_for_socket_addr(published: &Arc<Mutex<Option<SocketAddr>>>) -> SocketAddr {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if let Some(addr) = *published.lock().expect("published addr lock") {
            return addr;
        }
        assert!(Instant::now() <= deadline, "socket addr was not published");
        std::thread::yield_now();
    }
}

fn temp_dir(prefix: &str) -> PathBuf {
    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock after epoch")
        .as_nanos();
    std::env::temp_dir().join(format!("tina-{prefix}-{}-{unique}", std::process::id()))
}

fn new_worker(
    dir: &std::path::Path,
    name: &str,
    observed: &Arc<Mutex<Vec<ServiceObservation>>>,
) -> DurableWorker {
    DurableWorker {
        journal_path: dir.join(format!("{name}.journal")),
        next_index: 1,
        observed: Arc::clone(observed),
    }
}

#[test]
fn portable_service_harness_routes_persists_and_reports_terminal_truth() {
    let dir = temp_dir("portable-service-happy");
    fs::create_dir_all(&dir).expect("create portable service dir");
    let observed = Arc::new(Mutex::new(Vec::new()));

    let app =
        LocalSystem::<ServiceShard, ServiceMailboxFactory>::multi_shard(ServiceMailboxFactory)
            .shard(ServiceShard(10))
            .shard(ServiceShard(11))
            .shard(ServiceShard(12))
            .ingress_capacity(16)
            .shard_pair_capacity(16)
            .storage_lane_capacity(4)
            .remote_inbound_drain_budget(2)
            .trace_retention(TraceRetention::Bounded(1024))
            .build();

    let even = app
        .register_root_on::<DurableWorker, Infallible>(
            ShardId::new(11),
            new_worker(&dir, "even", &observed),
            8,
        )
        .expect("register even worker");
    let odd = app
        .register_root_on::<DurableWorker, Infallible>(
            ShardId::new(12),
            new_worker(&dir, "odd", &observed),
            8,
        )
        .expect("register odd worker");
    let router = app
        .register_root_on::<Router, Infallible>(
            ShardId::new(10),
            Router {
                even_worker: even,
                odd_worker: odd,
                max_retries: 0,
                observed: Arc::clone(&observed),
            },
            8,
        )
        .expect("register router");

    app.try_send(
        router,
        RouterMsg::Submit(ServiceRequest {
            key: 2,
            body: "llama-feed".to_string(),
        }),
    )
    .expect("submit even request");
    app.try_send(
        router,
        RouterMsg::Submit(ServiceRequest {
            key: 3,
            body: "hay-stack".to_string(),
        }),
    )
    .expect("submit odd request");

    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let current = observed.lock().expect("observed lock").clone();
        if current.contains(&ServiceObservation::Accepted {
            key: 2,
            body: "llama-feed".to_string(),
        }) && current.contains(&ServiceObservation::Accepted {
            key: 3,
            body: "hay-stack".to_string(),
        }) {
            break;
        }
        assert!(
            Instant::now() <= deadline,
            "portable service accepted both requests; observed = {current:?}; trace = {:#?}",
            app.trace()
        );
        std::thread::yield_now();
    }

    let terminal = app
        .shutdown()
        .drain()
        .join()
        .expect("portable service shutdown");
    assert_eq!(terminal.state(), LocalSystemState::Closed);
    assert!(terminal.shutdown_report().clean());
    assert_eq!(terminal.summary().journal_appended, 2);
    assert_eq!(terminal.summary().call_failed, 0);
    assert_eq!(terminal.summary().send_rejected, 0);
    let topology = terminal.topology().expect("terminal topology exists");
    assert!(topology.shard(ShardId::new(10)).is_some());
    assert!(topology.shard(ShardId::new(11)).is_some());
    assert!(topology.shard(ShardId::new(12)).is_some());
    assert!(
        terminal.trace().iter().any(|event| {
            event.shard() == ShardId::new(10)
                && matches!(
                    event.kind(),
                    RuntimeEventKind::SendAccepted {
                        target_shard,
                        ..
                    } if target_shard == ShardId::new(11)
                )
        }),
        "router should visibly send/call to even worker shard"
    );
    assert!(
        terminal.trace().iter().any(|event| {
            matches!(
                event.kind(),
                RuntimeEventKind::CallCompleted {
                    call_kind: CallKind::JournalAppend,
                    ..
                }
            )
        }),
        "journal append should be visible in terminal trace"
    );

    let even_replay = tina_runtime::persistence::replay_journal(&dir.join("even.journal"))
        .expect("even journal replays");
    assert_eq!(even_replay.records.len(), 1);
    assert_eq!(even_replay.records[0].bytes, b"2:llama-feed");
    let odd_replay = tina_runtime::persistence::replay_journal(&dir.join("odd.journal"))
        .expect("odd journal replays");
    assert_eq!(odd_replay.records.len(), 1);
    assert_eq!(odd_replay.records[0].bytes, b"3:hay-stack");
    let _ = fs::remove_dir_all(dir);
}

#[test]
fn baobab_service_gauntlet_composes_tcp_timer_dns_process_file_persistence_cross_shard_and_shutdown()
 {
    let dir = temp_dir("baobab-service-gauntlet");
    fs::create_dir_all(&dir).expect("create baobab service dir");
    let observed = Arc::new(Mutex::new(Vec::new()));
    let published = Arc::new(Mutex::new(None));

    let app =
        LocalSystem::<ServiceShard, ServiceMailboxFactory>::multi_shard(ServiceMailboxFactory)
            .shard(ServiceShard(13))
            .shard(ServiceShard(17))
            .ingress_capacity(8)
            .shard_pair_capacity(8)
            .dns_lane_capacity(2)
            .process_lane_capacity(2)
            .storage_lane_capacity(4)
            .trace_retention(TraceRetention::Bounded(1024))
            .build();
    let worker = app
        .register_root_on::<DurableWorker, Infallible>(
            ShardId::new(17),
            new_worker(&dir, "readiness-worker", &observed),
            8,
        )
        .expect("register readiness worker");
    let service = app
        .register_root_on::<ReadinessService, Infallible>(
            ShardId::new(13),
            ReadinessService {
                file_path: dir.join("config.txt"),
                journal_path: dir.join("checkpoint.journal"),
                worker,
                published: Arc::clone(&published),
                observed: Arc::clone(&observed),
                listener: None,
                tcp_buffer: Vec::new(),
            },
            8,
        )
        .expect("register readiness service");

    app.try_send(service, ReadinessMsg::Start)
        .expect("start readiness service");
    let addr = wait_for_socket_addr(&published);
    let client = thread::spawn(move || {
        let mut stream = TcpStream::connect(addr).expect("connect readiness tcp service");
        stream
            .write_all(b"baobab-")
            .expect("write readiness request prefix");
        stream
            .write_all(b"ready\n")
            .expect("write readiness request suffix");
        stream.shutdown(Shutdown::Write).expect("finish request");
        let mut reply = String::new();
        stream
            .read_to_string(&mut reply)
            .expect("read readiness reply");
        reply
    });
    wait_until("baobab service gauntlet completes", || {
        observed
            .lock()
            .expect("observed lock")
            .contains(&ServiceObservation::Accepted {
                key: 50,
                body: "tcp-timer-dns-process-file-journal-cross-shard-call".to_string(),
            })
    });
    assert_eq!(
        client.join().expect("readiness TCP client joins"),
        "readiness-ok\n"
    );

    let terminal = app
        .shutdown()
        .drain()
        .join()
        .expect("baobab service shutdown");
    assert!(terminal.shutdown_report().clean());
    for expected in [
        CallKind::TcpBind,
        CallKind::TcpAccept,
        CallKind::TcpRead,
        CallKind::TcpWrite,
        CallKind::TcpStreamClose,
        CallKind::TcpListenerClose,
        CallKind::Sleep,
        CallKind::DnsLookup,
        CallKind::ProcessRun,
        CallKind::FileOpen,
        CallKind::FileWriteAt,
        CallKind::FileReadAt,
        CallKind::JournalAppend,
        CallKind::IsolateCall,
    ] {
        assert!(
            terminal.trace().iter().any(|event| {
                matches!(
                    event.kind(),
                    RuntimeEventKind::CallCompleted { call_kind, .. } if call_kind == expected
                )
            }),
            "expected {expected:?} completion in Baobab service trace"
        );
    }
    let tcp_reads = terminal
        .trace()
        .iter()
        .filter(|event| {
            matches!(
                event.kind(),
                RuntimeEventKind::CallCompleted {
                    call_kind: CallKind::TcpRead,
                    ..
                }
            )
        })
        .count();
    assert!(
        tcp_reads >= 4,
        "Baobab service should prove framed TCP across multiple reads, saw {tcp_reads}"
    );
    assert_eq!(terminal.summary().call_failed, 0);
    assert_eq!(terminal.summary().journal_appended, 2);
    assert_eq!(
        fs::read(dir.join("config.txt")).expect("config file survives"),
        b"baobab"
    );
    let replay = tina_runtime::persistence::replay_journal(&dir.join("checkpoint.journal"))
        .expect("checkpoint journal replays");
    assert_eq!(replay.records.len(), 1);
    assert_eq!(replay.records[0].bytes, b"baobab");
    let worker_replay =
        tina_runtime::persistence::replay_journal(&dir.join("readiness-worker.journal"))
            .expect("readiness worker journal replays");
    assert_eq!(worker_replay.records.len(), 1);
    assert_eq!(worker_replay.records[0].bytes, b"51:cross-shard-gauntlet");
    let _ = fs::remove_dir_all(dir);
}

#[test]
fn baobab_live_multishard_service_survives_peer_shard_failure() {
    let dir = temp_dir("baobab-live-multishard-failure");
    fs::create_dir_all(&dir).expect("create Baobab live multishard dir");
    let observed = Arc::new(Mutex::new(Vec::new()));

    let app = LocalSystem::<ServiceShard, CapacityPanicMailboxFactory>::multi_shard(
        CapacityPanicMailboxFactory { panic_capacity: 13 },
    )
    .shard(ServiceShard(14))
    .shard(ServiceShard(15))
    .shard(ServiceShard(16))
    .ingress_capacity(16)
    .shard_pair_capacity(2)
    .storage_lane_capacity(2)
    .trace_retention(TraceRetention::Bounded(1024))
    .build();
    let even = app
        .register_root_on::<DurableWorker, Infallible>(
            ShardId::new(15),
            new_worker(&dir, "even", &observed),
            8,
        )
        .expect("register even worker");
    let odd = app
        .register_root_on::<DurableWorker, Infallible>(
            ShardId::new(16),
            new_worker(&dir, "odd", &observed),
            8,
        )
        .expect("register odd worker");
    let router = app
        .register_root_on::<Router, Infallible>(
            ShardId::new(14),
            Router {
                even_worker: even,
                odd_worker: odd,
                max_retries: 0,
                observed: Arc::clone(&observed),
            },
            8,
        )
        .expect("register Baobab router");

    assert!(matches!(
        app.register_root_on::<DurableWorker, Infallible>(
            ShardId::new(15),
            new_worker(&dir, "poison", &observed),
            13,
        ),
        Err(ThreadedRuntimeError::WorkerStopped)
    ));
    assert_eq!(
        app.try_send(
            even,
            WorkerMsg::Work(ServiceRequest {
                key: 100,
                body: "probe-failed-shard".to_string(),
            }),
        ),
        Err(tina_runtime::ThreadedTrySendError::WorkerStopped)
    );

    app.try_send(
        router,
        RouterMsg::Submit(ServiceRequest {
            key: 3,
            body: "healthy-shard-work".to_string(),
        }),
    )
    .expect("healthy shard request accepted after peer failure");
    app.try_send(
        router,
        RouterMsg::Submit(ServiceRequest {
            key: 2,
            body: "failed-shard-work".to_string(),
        }),
    )
    .expect("failed shard request reaches router");

    wait_until("healthy sibling and failed target outcomes settle", || {
        let observed = observed.lock().expect("observed lock");
        observed.contains(&ServiceObservation::Accepted {
            key: 3,
            body: "healthy-shard-work".to_string(),
        }) && observed.contains(&ServiceObservation::Rejected {
            key: 2,
            reason: "closed".to_string(),
        })
    });

    let terminal = app.shutdown().drain().join_report();
    assert_eq!(terminal.state(), LocalSystemState::Failed);
    assert_eq!(terminal.error(), Some(ThreadedRuntimeError::WorkerStopped));
    assert!(
        terminal
            .shutdown_report()
            .unclean_reasons()
            .iter()
            .any(|reason| matches!(reason, tina_runtime::ShutdownUncleanReason::RuntimeError(_)))
    );
    assert_eq!(terminal.summary().journal_appended, 1);
    let topology = terminal.topology().expect("terminal topology");
    assert_eq!(
        topology
            .shard(ShardId::new(15))
            .expect("failed shard topology")
            .state(),
        LiveShardState::Failed
    );
    assert!(terminal.trace().iter().any(|event| {
        event.shard() == ShardId::new(16)
            && matches!(
                event.kind(),
                RuntimeEventKind::JournalAppended { record_index: 1 }
            )
    }));
    assert!(terminal.trace().iter().any(|event| {
        event.shard() == ShardId::new(14)
            && matches!(
                event.kind(),
                RuntimeEventKind::CallFailed {
                    call_kind: CallKind::IsolateCall,
                    reason: CallError::TargetClosed,
                    ..
                }
            )
    }));

    let odd_replay = tina_runtime::persistence::replay_journal(&dir.join("odd.journal"))
        .expect("healthy sibling journal replays");
    assert_eq!(odd_replay.records.len(), 1);
    assert_eq!(odd_replay.records[0].bytes, b"3:healthy-shard-work");
    let _ = fs::remove_dir_all(dir);
}

#[test]
fn portable_service_wrong_shard_placement_rejects_before_work_runs() {
    let dir = temp_dir("portable-service-wrong-shard");
    fs::create_dir_all(&dir).expect("create portable service dir");
    let observed = Arc::new(Mutex::new(Vec::new()));

    let app =
        LocalSystem::<ServiceShard, ServiceMailboxFactory>::multi_shard(ServiceMailboxFactory)
            .shard(ServiceShard(20))
            .shard(ServiceShard(21))
            .shard(ServiceShard(22))
            .ingress_capacity(8)
            .shard_pair_capacity(8)
            .storage_lane_capacity(2)
            .trace_retention(TraceRetention::Bounded(256))
            .build();
    let even = app
        .register_root_on::<DurableWorker, Infallible>(
            ShardId::new(21),
            new_worker(&dir, "even", &observed),
            8,
        )
        .expect("register even worker");
    let odd = app
        .register_root_on::<DurableWorker, Infallible>(
            ShardId::new(22),
            new_worker(&dir, "odd", &observed),
            8,
        )
        .expect("register odd worker");
    let router = app
        .register_root_on::<Router, Infallible>(
            ShardId::new(20),
            Router {
                even_worker: even,
                odd_worker: odd,
                max_retries: 0,
                observed: Arc::clone(&observed),
            },
            8,
        )
        .expect("register router");

    app.try_send(
        router,
        RouterMsg::RouteToWrongShard(
            ServiceRequest {
                key: 2,
                body: "should-not-store".to_string(),
            },
            odd,
        ),
    )
    .expect("send bad placement request");

    wait_until("wrong shard rejected", || {
        observed
            .lock()
            .expect("observed lock")
            .contains(&ServiceObservation::Rejected {
                key: 2,
                reason: "wrong-shard".to_string(),
            })
    });

    let terminal = app
        .shutdown()
        .drain()
        .join()
        .expect("wrong-shard app shutdown");
    assert_eq!(terminal.summary().journal_appended, 0);
    assert!(terminal.shutdown_report().clean());
    assert!(!dir.join("even.journal").exists());
    assert!(!dir.join("odd.journal").exists());
    let _ = fs::remove_dir_all(dir);
}

#[test]
fn portable_service_busy_policy_rejects_after_explicit_timer_retry() {
    let dir = temp_dir("portable-service-busy-retry");
    fs::create_dir_all(&dir).expect("create portable service dir");
    let observed = Arc::new(Mutex::new(Vec::new()));

    let app =
        LocalSystem::<ServiceShard, ServiceMailboxFactory>::multi_shard(ServiceMailboxFactory)
            .shard(ServiceShard(30))
            .shard(ServiceShard(31))
            .shard(ServiceShard(32))
            .ingress_capacity(16)
            .shard_pair_capacity(16)
            .storage_lane_capacity(2)
            .trace_retention(TraceRetention::Bounded(512))
            .build();
    let even = app
        .register_root_on::<DurableWorker, Infallible>(
            ShardId::new(31),
            new_worker(&dir, "even", &observed),
            0,
        )
        .expect("register even worker");
    let odd = app
        .register_root_on::<DurableWorker, Infallible>(
            ShardId::new(32),
            new_worker(&dir, "odd", &observed),
            8,
        )
        .expect("register odd worker");
    let router = app
        .register_root_on::<Router, Infallible>(
            ShardId::new(30),
            Router {
                even_worker: even,
                odd_worker: odd,
                max_retries: 1,
                observed: Arc::clone(&observed),
            },
            8,
        )
        .expect("register router");

    app.try_send(
        router,
        RouterMsg::Submit(ServiceRequest {
            key: 2,
            body: "busy".to_string(),
        }),
    )
    .expect("send busy request");

    wait_until("busy request exhausted after retry", || {
        let observed = observed.lock().expect("observed lock");
        observed.contains(&ServiceObservation::BackoffScheduled { key: 2, attempt: 1 })
            && observed.contains(&ServiceObservation::RetryWoke { key: 2, attempt: 1 })
            && observed.contains(&ServiceObservation::Rejected {
                key: 2,
                reason: "busy".to_string(),
            })
    });

    let terminal = app
        .shutdown()
        .drain()
        .join()
        .expect("busy-retry app shutdown");
    assert_eq!(terminal.summary().journal_appended, 0);
    assert!(terminal.trace().iter().any(|event| {
        matches!(
            event.kind(),
            RuntimeEventKind::CallFailed {
                call_kind: CallKind::IsolateCall,
                reason: CallError::TargetFull,
                ..
            }
        )
    }));
    assert!(terminal.trace().iter().any(|event| {
        matches!(
            event.kind(),
            RuntimeEventKind::CallCompleted {
                call_kind: CallKind::Sleep,
                ..
            }
        )
    }));
    let _ = fs::remove_dir_all(dir);
}

#[test]
fn portable_service_unknown_shard_registration_fails_before_start() {
    let app =
        LocalSystem::<ServiceShard, ServiceMailboxFactory>::multi_shard(ServiceMailboxFactory)
            .shard(ServiceShard(40))
            .ingress_capacity(8)
            .shard_pair_capacity(8)
            .trace_retention(TraceRetention::Bounded(64))
            .build();
    let observed = Arc::new(Mutex::new(Vec::new()));

    let result = app.register_root_on::<DurableWorker, Infallible>(
        ShardId::new(41),
        DurableWorker {
            journal_path: PathBuf::from("unused.journal"),
            next_index: 1,
            observed,
        },
        8,
    );
    assert!(matches!(
        result,
        Err(ThreadedRuntimeError::UnknownShard(shard)) if shard == ShardId::new(41)
    ));

    let terminal = app
        .shutdown()
        .drain()
        .join()
        .expect("unknown-shard app shutdown");
    assert!(terminal.shutdown_report().clean());
}

#[test]
fn portable_service_observed_send_accepted_preserves_original_call() {
    let observed = Arc::new(Mutex::new(Vec::new()));
    let app =
        LocalSystem::<ServiceShard, ServiceMailboxFactory>::multi_shard(ServiceMailboxFactory)
            .shard(ServiceShard(60))
            .shard(ServiceShard(61))
            .shard(ServiceShard(62))
            .ingress_capacity(8)
            .shard_pair_capacity(8)
            .trace_retention(TraceRetention::Bounded(512))
            .build();

    let audit = app
        .register_root_on::<AuditSink, Infallible>(
            ShardId::new(61),
            AuditSink {
                observed: Arc::clone(&observed),
            },
            8,
        )
        .expect("register audit sink");
    let worker = app
        .register_root_on::<AuditedWorker, Infallible>(
            ShardId::new(61),
            AuditedWorker {
                audit,
                observed: Arc::clone(&observed),
            },
            8,
        )
        .expect("register audited worker");
    let client = app
        .register_root_on::<AuditedClient, Infallible>(
            ShardId::new(60),
            AuditedClient {
                worker,
                observed: Arc::clone(&observed),
            },
            8,
        )
        .expect("register audited client");

    app.try_send(
        client,
        AuditedClientMsg::Start(ServiceRequest {
            key: 9,
            body: "audit-first".to_string(),
        }),
    )
    .expect("start audited request");
    wait_until(
        "audited observed-send accepted continuation replies",
        || {
            observed
                .lock()
                .expect("observed lock")
                .contains(&ServiceObservation::Accepted {
                    key: 9,
                    body: "audit-first".to_string(),
                })
        },
    );
    wait_until("audit target eventually handles accepted send", || {
        observed
            .lock()
            .expect("observed lock")
            .contains(&ServiceObservation::AuditStored(9))
    });
    let seen = observed.lock().expect("observed lock").clone();
    assert!(
        seen.contains(&ServiceObservation::AuditStored(9)),
        "accepted audit send should eventually run the target side effect: {seen:?}"
    );
    assert!(
        seen.contains(&ServiceObservation::WorkerStored(9)),
        "worker should record stored after observed-send completion: {seen:?}"
    );

    let terminal = app
        .shutdown()
        .drain()
        .join()
        .expect("audited service shutdown");
    assert!(terminal.shutdown_report().clean());
    assert!(
        terminal.trace().iter().any(|event| {
            matches!(
                event.kind(),
                RuntimeEventKind::CallCompleted {
                    call_kind: CallKind::ObservedSend,
                    ..
                }
            )
        }),
        "observed send completion should be visible in terminal trace"
    );
    assert_eq!(terminal.summary().call_failed, 0);
    assert_eq!(terminal.summary().call_reply_rejected, 0);
}

#[test]
fn portable_service_observed_send_full_still_replies_to_original_call() {
    let observed = Arc::new(Mutex::new(Vec::new()));
    let app =
        LocalSystem::<ServiceShard, ServiceMailboxFactory>::multi_shard(ServiceMailboxFactory)
            .shard(ServiceShard(70))
            .shard(ServiceShard(71))
            .shard(ServiceShard(72))
            .ingress_capacity(8)
            .shard_pair_capacity(8)
            .trace_retention(TraceRetention::Bounded(512))
            .build();

    let audit = app
        .register_root_on::<AuditSink, Infallible>(
            ShardId::new(71),
            AuditSink {
                observed: Arc::clone(&observed),
            },
            0,
        )
        .expect("register full audit sink");
    let worker = app
        .register_root_on::<AuditedWorker, Infallible>(
            ShardId::new(71),
            AuditedWorker {
                audit,
                observed: Arc::clone(&observed),
            },
            8,
        )
        .expect("register audited worker");
    let client = app
        .register_root_on::<AuditedClient, Infallible>(
            ShardId::new(70),
            AuditedClient {
                worker,
                observed: Arc::clone(&observed),
            },
            8,
        )
        .expect("register full-audit client");

    app.try_send(
        client,
        AuditedClientMsg::Start(ServiceRequest {
            key: 10,
            body: "audit-full".to_string(),
        }),
    )
    .expect("start full-audit request");
    wait_until("full observed-send continuation replies", || {
        observed
            .lock()
            .expect("observed lock")
            .contains(&ServiceObservation::Rejected {
                key: 10,
                reason: "audit:TargetFull".to_string(),
            })
    });
    assert!(
        !observed
            .lock()
            .expect("observed lock")
            .contains(&ServiceObservation::AuditStored(10)),
        "full audit mailbox must not run the side effect"
    );

    let terminal = app
        .shutdown()
        .drain()
        .join()
        .expect("full-audit service shutdown");
    assert!(terminal.shutdown_report().clean());
    assert!(terminal.trace().iter().any(|event| {
        matches!(
            event.kind(),
            RuntimeEventKind::SendRejected {
                reason: tina_runtime::SendRejectedReason::Full,
                ..
            }
        )
    }));
    assert_eq!(terminal.summary().call_reply_rejected, 0);
}
