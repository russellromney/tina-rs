#![feature(allocator_api)]

//! Focused tests for the runtime-owned call effect path.
//!
//! These tests deliberately exercise non-network call semantics through
//! the runtime's call-dispatch machinery: invalid resource ids, what
//! happens when a requesting isolate stops before its completion arrives,
//! and call-id monotonicity.
//!
//! They are the primary semantic surface for slice 012's call dispatch
//! contract; the live TCP echo integration test in `tcp_echo.rs` is a
//! higher-level proof built on the same machinery.
//!
//! Why this lives in its own file: per the package's "do not let echo
//! become the only proof" trap, the call-dispatch behavior must be
//! provable without depending on a successful real socket round-trip.
//! Phase 012 is TCP-first, so we drive these scenarios through the same
//! TCP call vocabulary the echo test uses.

use std::alloc::Global;
use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::convert::Infallible;
use std::net::{SocketAddr, TcpStream};
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use betelgeuse::io::simulated::{SimulatedConfig, SimulatedDelay, SimulatedIO};
use tina::{Address, Context, Effect, Isolate, Mailbox, Outbound, Shard, ShardId, TrySendError};
use tina_runtime::{
    CallCompletionRejectedReason, CallError, CallId, CallInput, CallKind, CallOutput, ListenerId,
    MailboxFactory, Runtime, RuntimeCall, RuntimeEvent, RuntimeEventKind, StreamId,
};

#[derive(Debug, Default)]
struct TestShard;

impl Shard for TestShard {
    fn id(&self) -> ShardId {
        ShardId::new(2)
    }
}

struct TestMailbox<T> {
    capacity: usize,
    queue: Rc<RefCell<VecDeque<T>>>,
    closed: Rc<Cell<bool>>,
}

impl<T> TestMailbox<T> {
    fn new(capacity: usize) -> Self {
        Self {
            capacity,
            queue: Rc::new(RefCell::new(VecDeque::new())),
            closed: Rc::new(Cell::new(false)),
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum NeverOutbound {}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProbeMsg {
    StartInvalidAccept,
    StartInvalidRead,
    InvalidResourceObserved,
}

#[derive(Debug, Clone)]
enum BinderMsg {
    StartBind,
    Bound {
        listener: ListenerId,
        addr: SocketAddr,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WaiterMsg {
    StartAccept,
    StopNow,
    AcceptedObserved(SocketAddr),
    FailedObserved,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AcceptStreamMsg {
    StartAccept,
    Accepted(StreamId),
    Failed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReaderMsg {
    StartRead,
    StopNow,
    ReadObserved,
    FailedObserved,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WriterMsg {
    StartWrite,
    StopNow,
    WroteObserved,
    FailedObserved,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CloserMsg {
    CloseStream(StreamId),
    ClosedObserved,
    FailedObserved,
}

/// A small isolate that issues TCP call requests against runtime-owned ids
/// it knows are invalid, so we exercise the call-dispatch path without
/// depending on real socket behavior.
#[derive(Debug)]
struct Probe {
    log: Rc<RefCell<Vec<ProbeMsg>>>,
}

impl Isolate for Probe {
    type Message = ProbeMsg;
    type Reply = ();
    type Send = Outbound<NeverOutbound>;
    type Spawn = Infallible;
    type SpawnObserved = std::convert::Infallible;
    type Call = RuntimeCall<ProbeMsg>;
    type Fact = ::std::convert::Infallible;
    type Shard = TestShard;

    fn handle(
        &mut self,
        msg: Self::Message,
        _ctx: &mut Context<'_, Self::Shard, Self::Reply>,
    ) -> Effect<Self> {
        match msg {
            ProbeMsg::StartInvalidAccept => Effect::Call(RuntimeCall::new(
                CallInput::TcpAccept {
                    listener: ListenerId::new(9999),
                },
                |result| match result {
                    CallOutput::Failed(_) => ProbeMsg::InvalidResourceObserved,
                    other => panic!("expected accept failure, got {other:?}"),
                },
            )),
            ProbeMsg::StartInvalidRead => Effect::Call(RuntimeCall::new(
                CallInput::TcpRead {
                    stream: StreamId::new(9999),
                    max_len: 64,
                },
                |result| match result {
                    CallOutput::Failed(_) => ProbeMsg::InvalidResourceObserved,
                    other => panic!("expected read failure, got {other:?}"),
                },
            )),
            ProbeMsg::InvalidResourceObserved => {
                self.log
                    .borrow_mut()
                    .push(ProbeMsg::InvalidResourceObserved);
                Effect::Noop
            }
        }
    }
}

#[derive(Debug)]
struct Binder {
    bind_addr: SocketAddr,
    listener_slot: Arc<Mutex<Option<ListenerId>>>,
    addr_slot: Arc<Mutex<Option<SocketAddr>>>,
}

impl Isolate for Binder {
    type Message = BinderMsg;
    type Reply = ();
    type Send = Outbound<NeverOutbound>;
    type Spawn = Infallible;
    type SpawnObserved = std::convert::Infallible;
    type Call = RuntimeCall<BinderMsg>;
    type Fact = ::std::convert::Infallible;
    type Shard = TestShard;

    fn handle(
        &mut self,
        msg: Self::Message,
        _ctx: &mut Context<'_, Self::Shard, Self::Reply>,
    ) -> Effect<Self> {
        match msg {
            BinderMsg::StartBind => {
                let addr = self.bind_addr;
                Effect::Call(RuntimeCall::new(
                    CallInput::TcpBind { addr },
                    move |result| match result {
                        CallOutput::TcpBound {
                            listener,
                            local_addr,
                        } => BinderMsg::Bound {
                            listener,
                            addr: local_addr,
                        },
                        other => panic!("expected successful bind result, got {other:?}"),
                    },
                ))
            }
            BinderMsg::Bound { listener, addr } => {
                *self.listener_slot.lock().expect("listener mutex") = Some(listener);
                *self.addr_slot.lock().expect("addr mutex") = Some(addr);
                Effect::Noop
            }
        }
    }
}

#[derive(Debug)]
struct Waiter {
    listener: ListenerId,
    log: Rc<RefCell<Vec<WaiterMsg>>>,
    peer_slot: Arc<Mutex<Option<SocketAddr>>>,
}

#[derive(Debug)]
struct StreamAcceptor {
    listener: ListenerId,
    stream_slot: Rc<Cell<Option<StreamId>>>,
}

#[derive(Debug)]
struct Reader {
    stream: StreamId,
    log: Rc<RefCell<Vec<ReaderMsg>>>,
}

#[derive(Debug)]
struct Writer {
    stream: StreamId,
    log: Rc<RefCell<Vec<WriterMsg>>>,
}

#[derive(Debug)]
struct Closer {
    log: Rc<RefCell<Vec<CloserMsg>>>,
}

impl Isolate for Waiter {
    type Message = WaiterMsg;
    type Reply = ();
    type Send = Outbound<NeverOutbound>;
    type Spawn = Infallible;
    type SpawnObserved = std::convert::Infallible;
    type Call = RuntimeCall<WaiterMsg>;
    type Fact = ::std::convert::Infallible;
    type Shard = TestShard;

    fn handle(
        &mut self,
        msg: Self::Message,
        _ctx: &mut Context<'_, Self::Shard, Self::Reply>,
    ) -> Effect<Self> {
        match msg {
            WaiterMsg::StartAccept => Effect::Call(RuntimeCall::new(
                CallInput::TcpAccept {
                    listener: self.listener,
                },
                |result| match result {
                    CallOutput::TcpAccepted { peer_addr, .. } => {
                        WaiterMsg::AcceptedObserved(peer_addr)
                    }
                    CallOutput::Failed(_) => WaiterMsg::FailedObserved,
                    other => panic!("unexpected accept result {other:?}"),
                },
            )),
            WaiterMsg::StopNow => Effect::Stop,
            WaiterMsg::AcceptedObserved(peer_addr) => {
                *self.peer_slot.lock().expect("peer mutex") = Some(peer_addr);
                self.log
                    .borrow_mut()
                    .push(WaiterMsg::AcceptedObserved(peer_addr));
                Effect::Noop
            }
            WaiterMsg::FailedObserved => {
                self.log.borrow_mut().push(WaiterMsg::FailedObserved);
                Effect::Noop
            }
        }
    }
}

impl Isolate for StreamAcceptor {
    type Message = AcceptStreamMsg;
    type Reply = ();
    type Send = Outbound<NeverOutbound>;
    type Spawn = Infallible;
    type SpawnObserved = std::convert::Infallible;
    type Call = RuntimeCall<AcceptStreamMsg>;
    type Fact = ::std::convert::Infallible;
    type Shard = TestShard;

    fn handle(
        &mut self,
        msg: Self::Message,
        _ctx: &mut Context<'_, Self::Shard, Self::Reply>,
    ) -> Effect<Self> {
        match msg {
            AcceptStreamMsg::StartAccept => Effect::Call(RuntimeCall::new(
                CallInput::TcpAccept {
                    listener: self.listener,
                },
                |result| match result {
                    CallOutput::TcpAccepted { stream, .. } => AcceptStreamMsg::Accepted(stream),
                    CallOutput::Failed(_) => AcceptStreamMsg::Failed,
                    other => panic!("unexpected accept result {other:?}"),
                },
            )),
            AcceptStreamMsg::Accepted(stream) => {
                self.stream_slot.set(Some(stream));
                Effect::Noop
            }
            AcceptStreamMsg::Failed => Effect::Noop,
        }
    }
}

impl Isolate for Reader {
    type Message = ReaderMsg;
    type Reply = ();
    type Send = Outbound<NeverOutbound>;
    type Spawn = Infallible;
    type SpawnObserved = std::convert::Infallible;
    type Call = RuntimeCall<ReaderMsg>;
    type Fact = ::std::convert::Infallible;
    type Shard = TestShard;

    fn handle(
        &mut self,
        msg: Self::Message,
        _ctx: &mut Context<'_, Self::Shard, Self::Reply>,
    ) -> Effect<Self> {
        match msg {
            ReaderMsg::StartRead => Effect::Call(RuntimeCall::new(
                CallInput::TcpRead {
                    stream: self.stream,
                    max_len: 64,
                },
                |result| match result {
                    CallOutput::TcpRead { .. } => ReaderMsg::ReadObserved,
                    CallOutput::Failed(_) => ReaderMsg::FailedObserved,
                    other => panic!("unexpected read result {other:?}"),
                },
            )),
            ReaderMsg::StopNow => Effect::Stop,
            ReaderMsg::ReadObserved => {
                self.log.borrow_mut().push(ReaderMsg::ReadObserved);
                Effect::Noop
            }
            ReaderMsg::FailedObserved => {
                self.log.borrow_mut().push(ReaderMsg::FailedObserved);
                Effect::Noop
            }
        }
    }
}

impl Isolate for Writer {
    type Message = WriterMsg;
    type Reply = ();
    type Send = Outbound<NeverOutbound>;
    type Spawn = Infallible;
    type SpawnObserved = std::convert::Infallible;
    type Call = RuntimeCall<WriterMsg>;
    type Fact = ::std::convert::Infallible;
    type Shard = TestShard;

    fn handle(
        &mut self,
        msg: Self::Message,
        _ctx: &mut Context<'_, Self::Shard, Self::Reply>,
    ) -> Effect<Self> {
        match msg {
            WriterMsg::StartWrite => Effect::Call(RuntimeCall::new(
                CallInput::TcpWrite {
                    stream: self.stream,
                    bytes: b"cancel me before completion".to_vec(),
                },
                |result| match result {
                    CallOutput::TcpWrote { .. } => WriterMsg::WroteObserved,
                    CallOutput::Failed(_) => WriterMsg::FailedObserved,
                    other => panic!("unexpected write result {other:?}"),
                },
            )),
            WriterMsg::StopNow => Effect::Stop,
            WriterMsg::WroteObserved => {
                self.log.borrow_mut().push(WriterMsg::WroteObserved);
                Effect::Noop
            }
            WriterMsg::FailedObserved => {
                self.log.borrow_mut().push(WriterMsg::FailedObserved);
                Effect::Noop
            }
        }
    }
}

impl Isolate for Closer {
    type Message = CloserMsg;
    type Reply = ();
    type Send = Outbound<NeverOutbound>;
    type Spawn = Infallible;
    type SpawnObserved = std::convert::Infallible;
    type Call = RuntimeCall<CloserMsg>;
    type Fact = ::std::convert::Infallible;
    type Shard = TestShard;

    fn handle(
        &mut self,
        msg: Self::Message,
        _ctx: &mut Context<'_, Self::Shard, Self::Reply>,
    ) -> Effect<Self> {
        match msg {
            CloserMsg::CloseStream(stream) => Effect::Call(RuntimeCall::new(
                CallInput::TcpStreamClose { stream },
                |result| match result {
                    CallOutput::TcpStreamClosed => CloserMsg::ClosedObserved,
                    CallOutput::Failed(_) => CloserMsg::FailedObserved,
                    other => panic!("unexpected stream close result {other:?}"),
                },
            )),
            CloserMsg::ClosedObserved => {
                self.log.borrow_mut().push(CloserMsg::ClosedObserved);
                Effect::Noop
            }
            CloserMsg::FailedObserved => {
                self.log.borrow_mut().push(CloserMsg::FailedObserved);
                Effect::Noop
            }
        }
    }
}

fn call_event_kinds(trace: &[RuntimeEvent]) -> Vec<RuntimeEventKind> {
    trace
        .iter()
        .filter_map(|event| match event.kind() {
            kind @ (RuntimeEventKind::CallDispatchAttempted { .. }
            | RuntimeEventKind::CallCompleted { .. }
            | RuntimeEventKind::CallFailed { .. }
            | RuntimeEventKind::CallCompletionRejected { .. }) => Some(kind),
            _ => None,
        })
        .collect()
}

#[test]
fn invalid_listener_id_surfaces_failure_to_isolate_and_trace() {
    let log = Rc::new(RefCell::new(Vec::new()));
    let mut runtime = Runtime::new(TestShard, TestMailboxFactory);
    let probe = runtime.register(
        Probe {
            log: Rc::clone(&log),
        },
        TestMailbox::new(8),
    );

    runtime
        .try_send(probe, ProbeMsg::StartInvalidAccept)
        .expect("ingress accepts StartInvalidAccept");

    // Step once to dispatch the handler that returns Effect::Call. The
    // backend resolves invalid-resource synchronously, so the translator
    // runs on the same step and the next step delivers the message.
    runtime.step();
    runtime.step();

    assert_eq!(*log.borrow(), vec![ProbeMsg::InvalidResourceObserved]);

    let kinds = call_event_kinds(runtime.trace());
    assert!(
        kinds.iter().any(|k| matches!(
            k,
            RuntimeEventKind::CallFailed {
                call_kind: CallKind::TcpAccept,
                reason: tina_runtime::CallError::InvalidResource,
                ..
            }
        )),
        "trace must record CallFailed for accept against an invalid listener: {kinds:?}"
    );
}

#[test]
fn invalid_stream_id_surfaces_failure_to_isolate_and_trace() {
    let log = Rc::new(RefCell::new(Vec::new()));
    let mut runtime = Runtime::new(TestShard, TestMailboxFactory);
    let probe = runtime.register(
        Probe {
            log: Rc::clone(&log),
        },
        TestMailbox::new(8),
    );

    runtime
        .try_send(probe, ProbeMsg::StartInvalidRead)
        .expect("ingress accepts StartInvalidRead");

    runtime.step();
    runtime.step();

    assert_eq!(*log.borrow(), vec![ProbeMsg::InvalidResourceObserved]);

    let kinds = call_event_kinds(runtime.trace());
    assert!(
        kinds.iter().any(|k| matches!(
            k,
            RuntimeEventKind::CallFailed {
                call_kind: CallKind::TcpRead,
                reason: tina_runtime::CallError::InvalidResource,
                ..
            }
        )),
        "trace must record CallFailed for read against an invalid stream: {kinds:?}"
    );
}

#[test]
fn port_zero_bind_returns_real_local_addr_with_visible_trace() {
    let listener_slot = Arc::new(Mutex::new(None));
    let addr_slot = Arc::new(Mutex::new(None));
    let mut runtime = Runtime::new(TestShard, TestMailboxFactory);
    let binder = runtime.register(
        Binder {
            bind_addr: "127.0.0.1:0".parse().expect("loopback parse"),
            listener_slot: Arc::clone(&listener_slot),
            addr_slot: Arc::clone(&addr_slot),
        },
        TestMailbox::new(8),
    );

    runtime
        .try_send(binder, BinderMsg::StartBind)
        .expect("ingress accepts StartBind");

    runtime.step();
    runtime.step();

    let _listener = listener_slot
        .lock()
        .expect("listener mutex")
        .expect("listener published");
    let local_addr = addr_slot
        .lock()
        .expect("addr mutex")
        .expect("addr published");

    assert_eq!(local_addr.ip().to_string(), "127.0.0.1");
    assert_ne!(local_addr.port(), 0, "kernel must choose a real port");

    let kinds = call_event_kinds(runtime.trace());
    assert!(
        kinds.iter().any(|k| matches!(
            k,
            RuntimeEventKind::CallCompleted {
                call_kind: CallKind::TcpBind,
                ..
            }
        )),
        "trace must record CallCompleted for a successful port-0 bind: {kinds:?}"
    );
    assert!(
        !kinds.iter().any(|k| matches!(
            k,
            RuntimeEventKind::CallFailed {
                call_kind: CallKind::TcpBind,
                ..
            }
        )),
        "successful port-0 bind must not produce CallFailed: {kinds:?}"
    );
}

#[test]
fn pending_accept_completion_is_rejected_when_requester_stops_first() {
    let listener_slot = Arc::new(Mutex::new(None));
    let addr_slot = Arc::new(Mutex::new(None));
    let waiter_log = Rc::new(RefCell::new(Vec::new()));
    let peer_slot = Arc::new(Mutex::new(None));
    let bind_addr: SocketAddr = "127.0.0.1:0".parse().expect("loopback parse");

    let mut runtime = Runtime::new(TestShard, TestMailboxFactory);
    let binder = runtime.register(
        Binder {
            bind_addr,
            listener_slot: Arc::clone(&listener_slot),
            addr_slot: Arc::clone(&addr_slot),
        },
        TestMailbox::new(8),
    );

    runtime
        .try_send(binder, BinderMsg::StartBind)
        .expect("ingress accepts StartBind");
    runtime.step();
    runtime.step();

    let listener = listener_slot
        .lock()
        .expect("listener mutex")
        .expect("listener published");
    let _local_addr = addr_slot
        .lock()
        .expect("addr mutex")
        .expect("addr published");

    let waiter = runtime.register(
        Waiter {
            listener,
            log: Rc::clone(&waiter_log),
            peer_slot: Arc::clone(&peer_slot),
        },
        TestMailbox::new(8),
    );

    runtime
        .try_send(waiter, WaiterMsg::StartAccept)
        .expect("ingress accepts StartAccept");
    runtime.step();

    runtime
        .try_send(waiter, WaiterMsg::StopNow)
        .expect("ingress accepts StopNow");
    runtime.step();

    assert!(
        !runtime.has_in_flight_calls(),
        "stopped requester must cancel pending accept without needing a peer connection"
    );

    assert!(
        waiter_log.borrow().is_empty(),
        "stopped requester must not observe translated completion messages"
    );
    assert!(
        peer_slot.lock().expect("peer mutex").is_none(),
        "stopped requester must not publish a peer address"
    );

    let kinds = call_event_kinds(runtime.trace());
    assert!(
        kinds.iter().any(|k| matches!(
            k,
            RuntimeEventKind::CallCompletionRejected {
                call_kind: CallKind::TcpAccept,
                reason: CallCompletionRejectedReason::RequesterClosed,
                ..
            }
        )),
        "trace must record CallCompletionRejected{{RequesterClosed}} for pending accept: {kinds:?}"
    );
    assert!(
        !kinds.iter().any(|k| matches!(
            k,
            RuntimeEventKind::CallCompleted {
                call_kind: CallKind::TcpAccept,
                ..
            }
        )),
        "rejected completion must not produce CallCompleted: {kinds:?}"
    );
}

#[test]
fn pending_read_is_cancelled_when_requester_stops_without_peer_input() {
    let listener_slot = Arc::new(Mutex::new(None));
    let addr_slot = Arc::new(Mutex::new(None));
    let stream_slot = Rc::new(Cell::new(None));
    let reader_log = Rc::new(RefCell::new(Vec::new()));
    let bind_addr: SocketAddr = "127.0.0.1:0".parse().expect("loopback parse");

    let mut runtime = Runtime::new(TestShard, TestMailboxFactory);
    let binder = runtime.register(
        Binder {
            bind_addr,
            listener_slot: Arc::clone(&listener_slot),
            addr_slot: Arc::clone(&addr_slot),
        },
        TestMailbox::new(8),
    );

    runtime
        .try_send(binder, BinderMsg::StartBind)
        .expect("ingress accepts StartBind");
    runtime.step();
    runtime.step();

    let listener = listener_slot
        .lock()
        .expect("listener mutex")
        .expect("listener published");
    let local_addr = addr_slot
        .lock()
        .expect("addr mutex")
        .expect("addr published");

    let acceptor = runtime.register(
        StreamAcceptor {
            listener,
            stream_slot: Rc::clone(&stream_slot),
        },
        TestMailbox::new(8),
    );
    runtime
        .try_send(acceptor, AcceptStreamMsg::StartAccept)
        .expect("ingress accepts StartAccept");
    runtime.step();

    let _client = TcpStream::connect(local_addr).expect("connect to listener");
    let deadline = Instant::now() + Duration::from_secs(2);
    while stream_slot.get().is_none() {
        runtime.step();
        if Instant::now() > deadline {
            panic!(
                "timed out waiting for accepted stream; trace = {:#?}",
                runtime.trace()
            );
        }
    }

    let reader = runtime.register(
        Reader {
            stream: stream_slot.get().expect("accepted stream id"),
            log: Rc::clone(&reader_log),
        },
        TestMailbox::new(8),
    );
    runtime
        .try_send(reader, ReaderMsg::StartRead)
        .expect("ingress accepts StartRead");
    runtime.step();
    assert!(runtime.has_in_flight_calls());

    runtime
        .try_send(reader, ReaderMsg::StopNow)
        .expect("ingress accepts StopNow");
    runtime.step();

    assert!(
        !runtime.has_in_flight_calls(),
        "stopped requester must cancel pending read without peer input"
    );
    assert!(
        reader_log.borrow().is_empty(),
        "stopped requester must not observe translated read completion messages"
    );

    let kinds = call_event_kinds(runtime.trace());
    assert!(
        kinds.iter().any(|k| matches!(
            k,
            RuntimeEventKind::CallCompletionRejected {
                call_kind: CallKind::TcpRead,
                reason: CallCompletionRejectedReason::RequesterClosed,
                ..
            }
        )),
        "trace must record CallCompletionRejected{{RequesterClosed}} for pending read: {kinds:?}"
    );
    assert!(
        !kinds.iter().any(|k| matches!(
            k,
            RuntimeEventKind::CallCompleted {
                call_kind: CallKind::TcpRead,
                ..
            }
        )),
        "stopped requester must not observe TcpRead CallCompleted"
    );
}

#[test]
fn pending_write_is_cancelled_when_requester_stops_before_simulated_completion() {
    let listener_slot = Arc::new(Mutex::new(None));
    let addr_slot = Arc::new(Mutex::new(None));
    let stream_slot = Rc::new(Cell::new(None));
    let writer_log = Rc::new(RefCell::new(Vec::new()));
    let bind_addr: SocketAddr = "127.0.0.1:0".parse().expect("loopback parse");
    let simulated_io = SimulatedIO::with_config(SimulatedConfig {
        seed: 26,
        completion_delay: SimulatedDelay::Every {
            one_in: 1,
            steps: 3,
        },
        max_send_chunk: None,
    });

    let mut runtime = Runtime::with_betelgeuse_io_loop(
        TestShard,
        TestMailboxFactory,
        simulated_io.loop_handle(Global),
    );
    let binder = runtime.register(
        Binder {
            bind_addr,
            listener_slot: Arc::clone(&listener_slot),
            addr_slot: Arc::clone(&addr_slot),
        },
        TestMailbox::new(8),
    );

    runtime
        .try_send(binder, BinderMsg::StartBind)
        .expect("ingress accepts StartBind");
    runtime.step();
    runtime.step();

    let listener = listener_slot
        .lock()
        .expect("listener mutex")
        .expect("listener published");
    let local_addr = addr_slot
        .lock()
        .expect("addr mutex")
        .expect("addr published");

    let acceptor = runtime.register(
        StreamAcceptor {
            listener,
            stream_slot: Rc::clone(&stream_slot),
        },
        TestMailbox::new(8),
    );
    runtime
        .try_send(acceptor, AcceptStreamMsg::StartAccept)
        .expect("ingress accepts StartAccept");
    runtime.step();

    let peer = simulated_io
        .connect(local_addr, Vec::new())
        .expect("simulated peer connects");
    let deadline = Instant::now() + Duration::from_secs(2);
    while stream_slot.get().is_none() {
        runtime.step();
        if Instant::now() > deadline {
            panic!(
                "timed out waiting for accepted stream; trace = {:#?}",
                runtime.trace()
            );
        }
    }

    let writer = runtime.register(
        Writer {
            stream: stream_slot.get().expect("accepted stream id"),
            log: Rc::clone(&writer_log),
        },
        TestMailbox::new(8),
    );
    runtime
        .try_send(writer, WriterMsg::StartWrite)
        .expect("ingress accepts StartWrite");
    runtime.step();
    assert!(runtime.has_in_flight_calls());

    runtime
        .try_send(writer, WriterMsg::StopNow)
        .expect("ingress accepts StopNow");
    runtime.step();

    assert!(
        !runtime.has_in_flight_calls(),
        "stopped requester must cancel pending write before simulated completion"
    );
    assert!(
        writer_log.borrow().is_empty(),
        "stopped requester must not observe translated write completion messages"
    );
    assert!(
        peer.output().is_empty(),
        "cancelled write must not become visible to the simulated peer before completion"
    );

    let kinds = call_event_kinds(runtime.trace());
    assert!(
        kinds.iter().any(|k| matches!(
            k,
            RuntimeEventKind::CallCompletionRejected {
                call_kind: CallKind::TcpWrite,
                reason: CallCompletionRejectedReason::RequesterClosed,
                ..
            }
        )),
        "trace must record CallCompletionRejected{{RequesterClosed}} for pending write: {kinds:?}"
    );
    assert!(
        !kinds.iter().any(|k| matches!(
            k,
            RuntimeEventKind::CallCompleted {
                call_kind: CallKind::TcpWrite,
                ..
            }
        )),
        "stopped requester must not observe TcpWrite CallCompleted"
    );

    for _ in 0..5 {
        runtime.step();
    }
    let matured_kinds = call_event_kinds(runtime.trace());
    assert!(
        !peer.output().is_empty(),
        "submitted write side effects may still mature after requester cancellation"
    );
    assert!(
        writer_log.borrow().is_empty(),
        "late tombstoned completion must not deliver translated write messages"
    );
    assert!(
        !matured_kinds.iter().any(|k| matches!(
            k,
            RuntimeEventKind::CallCompleted {
                call_kind: CallKind::TcpWrite,
                ..
            }
        )),
        "late tombstoned write must not produce TcpWrite CallCompleted"
    );
}

#[test]
fn read_and_write_can_overlap_on_same_stream() {
    let listener_slot = Arc::new(Mutex::new(None));
    let addr_slot = Arc::new(Mutex::new(None));
    let stream_slot = Rc::new(Cell::new(None));
    let reader_log = Rc::new(RefCell::new(Vec::new()));
    let writer_log = Rc::new(RefCell::new(Vec::new()));
    let bind_addr: SocketAddr = "127.0.0.1:0".parse().expect("loopback parse");

    let mut runtime = Runtime::new(TestShard, TestMailboxFactory);
    let binder = runtime.register(
        Binder {
            bind_addr,
            listener_slot: Arc::clone(&listener_slot),
            addr_slot: Arc::clone(&addr_slot),
        },
        TestMailbox::new(8),
    );

    runtime
        .try_send(binder, BinderMsg::StartBind)
        .expect("ingress accepts StartBind");
    runtime.step();
    runtime.step();

    let listener = listener_slot
        .lock()
        .expect("listener mutex")
        .expect("listener published");
    let local_addr = addr_slot
        .lock()
        .expect("addr mutex")
        .expect("addr published");

    let acceptor = runtime.register(
        StreamAcceptor {
            listener,
            stream_slot: Rc::clone(&stream_slot),
        },
        TestMailbox::new(8),
    );
    runtime
        .try_send(acceptor, AcceptStreamMsg::StartAccept)
        .expect("ingress accepts StartAccept");
    runtime.step();

    let _client = TcpStream::connect(local_addr).expect("connect to listener");
    let deadline = Instant::now() + Duration::from_secs(2);
    while stream_slot.get().is_none() {
        runtime.step();
        if Instant::now() > deadline {
            panic!(
                "timed out waiting for accepted stream; trace = {:#?}",
                runtime.trace()
            );
        }
    }

    let stream = stream_slot.get().expect("accepted stream id");
    let reader = runtime.register(
        Reader {
            stream,
            log: Rc::clone(&reader_log),
        },
        TestMailbox::new(8),
    );
    let writer = runtime.register(
        Writer {
            stream,
            log: Rc::clone(&writer_log),
        },
        TestMailbox::new(8),
    );

    runtime
        .try_send(reader, ReaderMsg::StartRead)
        .expect("ingress accepts StartRead");
    runtime.step();
    assert!(runtime.has_in_flight_calls());

    runtime
        .try_send(writer, WriterMsg::StartWrite)
        .expect("ingress accepts StartWrite");
    let deadline = Instant::now() + Duration::from_secs(2);
    while writer_log.borrow().is_empty() {
        runtime.step();
        if Instant::now() > deadline {
            panic!(
                "timed out waiting for overlapping write completion; trace = {:#?}",
                runtime.trace()
            );
        }
    }

    assert!(
        writer_log.borrow().contains(&WriterMsg::WroteObserved),
        "write lane must complete while read lane remains pending"
    );
    assert!(
        reader_log.borrow().is_empty(),
        "the original pending read must not be completed by the write"
    );
    assert!(
        runtime.has_in_flight_calls(),
        "the original read remains pending after overlapping write completes"
    );

    let kinds = call_event_kinds(runtime.trace());
    assert!(
        kinds.iter().any(|k| matches!(
            k,
            RuntimeEventKind::CallCompleted {
                call_kind: CallKind::TcpWrite,
                ..
            }
        )),
        "trace must record successful overlapping TcpWrite completion: {kinds:?}"
    );
    assert!(
        !kinds.iter().any(|k| matches!(
            k,
            RuntimeEventKind::CallFailed {
                call_kind: CallKind::TcpWrite,
                reason: CallError::ResourceBusy,
                ..
            }
        )),
        "overlapping write must not fail as ResourceBusy"
    );
}

#[test]
fn second_pending_read_on_same_stream_fails_resource_busy() {
    let listener_slot = Arc::new(Mutex::new(None));
    let addr_slot = Arc::new(Mutex::new(None));
    let stream_slot = Rc::new(Cell::new(None));
    let first_log = Rc::new(RefCell::new(Vec::new()));
    let second_log = Rc::new(RefCell::new(Vec::new()));
    let bind_addr: SocketAddr = "127.0.0.1:0".parse().expect("loopback parse");

    let mut runtime = Runtime::new(TestShard, TestMailboxFactory);
    let binder = runtime.register(
        Binder {
            bind_addr,
            listener_slot: Arc::clone(&listener_slot),
            addr_slot: Arc::clone(&addr_slot),
        },
        TestMailbox::new(8),
    );

    runtime
        .try_send(binder, BinderMsg::StartBind)
        .expect("ingress accepts StartBind");
    runtime.step();
    runtime.step();

    let listener = listener_slot
        .lock()
        .expect("listener mutex")
        .expect("listener published");
    let local_addr = addr_slot
        .lock()
        .expect("addr mutex")
        .expect("addr published");

    let acceptor = runtime.register(
        StreamAcceptor {
            listener,
            stream_slot: Rc::clone(&stream_slot),
        },
        TestMailbox::new(8),
    );
    runtime
        .try_send(acceptor, AcceptStreamMsg::StartAccept)
        .expect("ingress accepts StartAccept");
    runtime.step();

    let _client = TcpStream::connect(local_addr).expect("connect to listener");
    let deadline = Instant::now() + Duration::from_secs(2);
    while stream_slot.get().is_none() {
        runtime.step();
        if Instant::now() > deadline {
            panic!(
                "timed out waiting for accepted stream; trace = {:#?}",
                runtime.trace()
            );
        }
    }

    let stream = stream_slot.get().expect("accepted stream id");
    let first_reader = runtime.register(
        Reader {
            stream,
            log: Rc::clone(&first_log),
        },
        TestMailbox::new(8),
    );
    let second_reader = runtime.register(
        Reader {
            stream,
            log: Rc::clone(&second_log),
        },
        TestMailbox::new(8),
    );

    runtime
        .try_send(first_reader, ReaderMsg::StartRead)
        .expect("ingress accepts first StartRead");
    runtime.step();
    assert!(runtime.has_in_flight_calls());

    runtime
        .try_send(second_reader, ReaderMsg::StartRead)
        .expect("ingress accepts second StartRead");
    runtime.step();
    runtime.step();

    assert!(
        first_log.borrow().is_empty(),
        "original read must remain pending"
    );
    assert_eq!(*second_log.borrow(), vec![ReaderMsg::FailedObserved]);

    let kinds = call_event_kinds(runtime.trace());
    assert!(
        kinds.iter().any(|k| matches!(
            k,
            RuntimeEventKind::CallFailed {
                call_kind: CallKind::TcpRead,
                reason: CallError::ResourceBusy,
                ..
            }
        )),
        "trace must record duplicate TcpRead ResourceBusy failure: {kinds:?}"
    );
}

#[test]
fn cancelling_pending_read_does_not_close_stream_for_later_write() {
    let listener_slot = Arc::new(Mutex::new(None));
    let addr_slot = Arc::new(Mutex::new(None));
    let stream_slot = Rc::new(Cell::new(None));
    let reader_log = Rc::new(RefCell::new(Vec::new()));
    let second_reader_log = Rc::new(RefCell::new(Vec::new()));
    let writer_log = Rc::new(RefCell::new(Vec::new()));
    let bind_addr: SocketAddr = "127.0.0.1:0".parse().expect("loopback parse");

    let mut runtime = Runtime::new(TestShard, TestMailboxFactory);
    let binder = runtime.register(
        Binder {
            bind_addr,
            listener_slot: Arc::clone(&listener_slot),
            addr_slot: Arc::clone(&addr_slot),
        },
        TestMailbox::new(8),
    );

    runtime
        .try_send(binder, BinderMsg::StartBind)
        .expect("ingress accepts StartBind");
    runtime.step();
    runtime.step();

    let listener = listener_slot
        .lock()
        .expect("listener mutex")
        .expect("listener published");
    let local_addr = addr_slot
        .lock()
        .expect("addr mutex")
        .expect("addr published");

    let acceptor = runtime.register(
        StreamAcceptor {
            listener,
            stream_slot: Rc::clone(&stream_slot),
        },
        TestMailbox::new(8),
    );
    runtime
        .try_send(acceptor, AcceptStreamMsg::StartAccept)
        .expect("ingress accepts StartAccept");
    runtime.step();

    let _client = TcpStream::connect(local_addr).expect("connect to listener");
    let deadline = Instant::now() + Duration::from_secs(2);
    while stream_slot.get().is_none() {
        runtime.step();
        if Instant::now() > deadline {
            panic!(
                "timed out waiting for accepted stream; trace = {:#?}",
                runtime.trace()
            );
        }
    }

    let stream = stream_slot.get().expect("accepted stream id");
    let reader = runtime.register(
        Reader {
            stream,
            log: Rc::clone(&reader_log),
        },
        TestMailbox::new(8),
    );
    let writer = runtime.register(
        Writer {
            stream,
            log: Rc::clone(&writer_log),
        },
        TestMailbox::new(8),
    );
    let second_reader = runtime.register(
        Reader {
            stream,
            log: Rc::clone(&second_reader_log),
        },
        TestMailbox::new(8),
    );

    runtime
        .try_send(reader, ReaderMsg::StartRead)
        .expect("ingress accepts StartRead");
    runtime.step();
    assert!(runtime.has_in_flight_calls());

    runtime
        .try_send(reader, ReaderMsg::StopNow)
        .expect("ingress accepts StopNow");
    runtime.step();
    assert!(!runtime.has_in_flight_calls());

    runtime
        .try_send(second_reader, ReaderMsg::StartRead)
        .expect("ingress accepts second StartRead");
    runtime.step();
    runtime.step();
    assert_eq!(
        *second_reader_log.borrow(),
        vec![ReaderMsg::FailedObserved],
        "canceled tombstone keeps the read lane reserved until substrate completion drains"
    );

    runtime
        .try_send(writer, WriterMsg::StartWrite)
        .expect("ingress accepts StartWrite");
    let deadline = Instant::now() + Duration::from_secs(2);
    while writer_log.borrow().is_empty() {
        runtime.step();
        if Instant::now() > deadline {
            panic!(
                "timed out waiting for write after read cancellation; trace = {:#?}",
                runtime.trace()
            );
        }
    }

    assert!(reader_log.borrow().is_empty());
    assert_eq!(*writer_log.borrow(), vec![WriterMsg::WroteObserved]);

    let kinds = call_event_kinds(runtime.trace());
    assert!(
        kinds.iter().any(|k| matches!(
            k,
            RuntimeEventKind::CallCompletionRejected {
                call_kind: CallKind::TcpRead,
                reason: CallCompletionRejectedReason::RequesterClosed,
                ..
            }
        )),
        "trace must record canceled read completion rejection: {kinds:?}"
    );
    assert!(
        kinds.iter().any(|k| matches!(
            k,
            RuntimeEventKind::CallCompleted {
                call_kind: CallKind::TcpWrite,
                ..
            }
        )),
        "stream must remain valid for later TcpWrite: {kinds:?}"
    );
}

#[test]
fn stream_close_while_read_pending_cancels_read_and_closes() {
    let listener_slot = Arc::new(Mutex::new(None));
    let addr_slot = Arc::new(Mutex::new(None));
    let stream_slot = Rc::new(Cell::new(None));
    let reader_log = Rc::new(RefCell::new(Vec::new()));
    let closer_log = Rc::new(RefCell::new(Vec::new()));
    let bind_addr: SocketAddr = "127.0.0.1:0".parse().expect("loopback parse");

    let mut runtime = Runtime::new(TestShard, TestMailboxFactory);
    let binder = runtime.register(
        Binder {
            bind_addr,
            listener_slot: Arc::clone(&listener_slot),
            addr_slot: Arc::clone(&addr_slot),
        },
        TestMailbox::new(8),
    );

    runtime
        .try_send(binder, BinderMsg::StartBind)
        .expect("ingress accepts StartBind");
    runtime.step();
    runtime.step();

    let listener = listener_slot
        .lock()
        .expect("listener mutex")
        .expect("listener published");
    let local_addr = addr_slot
        .lock()
        .expect("addr mutex")
        .expect("addr published");

    let acceptor = runtime.register(
        StreamAcceptor {
            listener,
            stream_slot: Rc::clone(&stream_slot),
        },
        TestMailbox::new(8),
    );
    runtime
        .try_send(acceptor, AcceptStreamMsg::StartAccept)
        .expect("ingress accepts StartAccept");
    runtime.step();

    let _client = TcpStream::connect(local_addr).expect("connect to listener");
    let deadline = Instant::now() + Duration::from_secs(2);
    while stream_slot.get().is_none() {
        runtime.step();
        if Instant::now() > deadline {
            panic!(
                "timed out waiting for accepted stream; trace = {:#?}",
                runtime.trace()
            );
        }
    }

    let stream = stream_slot.get().expect("accepted stream id");
    let reader = runtime.register(
        Reader {
            stream,
            log: Rc::clone(&reader_log),
        },
        TestMailbox::new(8),
    );
    let closer = runtime.register(
        Closer {
            log: Rc::clone(&closer_log),
        },
        TestMailbox::new(8),
    );

    runtime
        .try_send(reader, ReaderMsg::StartRead)
        .expect("ingress accepts StartRead");
    runtime.step();
    assert!(runtime.has_in_flight_calls());

    runtime
        .try_send(closer, CloserMsg::CloseStream(stream))
        .expect("ingress accepts CloseStream");
    runtime.step();
    runtime.step();

    // Close wins. The pending read continuation does not fire.
    assert_eq!(
        *closer_log.borrow(),
        vec![CloserMsg::ClosedObserved],
        "close must succeed when a read was pending on the same stream"
    );
    assert!(
        reader_log.borrow().is_empty(),
        "the pending read continuation must not fire after the stream was closed"
    );
    let kinds = call_event_kinds(runtime.trace());
    assert!(
        kinds.iter().any(|k| matches!(
            k,
            RuntimeEventKind::CallCompletionRejected {
                call_kind: CallKind::TcpRead,
                reason: CallCompletionRejectedReason::ResourceClosed,
                ..
            }
        )),
        "trace must record TcpRead ResourceClosed after close-cancel: {kinds:?}"
    );
    assert!(
        kinds.iter().any(|k| matches!(
            k,
            RuntimeEventKind::CallCompleted {
                call_kind: CallKind::TcpStreamClose,
                ..
            }
        )),
        "trace must record TcpStreamClose CallCompleted: {kinds:?}"
    );
    assert!(
        !kinds.iter().any(|k| matches!(
            k,
            RuntimeEventKind::CallFailed {
                call_kind: CallKind::TcpStreamClose,
                reason: CallError::ResourceBusy,
                ..
            }
        )),
        "trace must NOT record TcpStreamClose ResourceBusy under the new contract"
    );
}

#[test]
fn accepted_stream_reports_real_peer_addr() {
    let listener_slot = Arc::new(Mutex::new(None));
    let addr_slot = Arc::new(Mutex::new(None));
    let waiter_log = Rc::new(RefCell::new(Vec::new()));
    let peer_slot = Arc::new(Mutex::new(None));
    let bind_addr: SocketAddr = "127.0.0.1:0".parse().expect("loopback parse");

    let mut runtime = Runtime::new(TestShard, TestMailboxFactory);
    let binder = runtime.register(
        Binder {
            bind_addr,
            listener_slot: Arc::clone(&listener_slot),
            addr_slot: Arc::clone(&addr_slot),
        },
        TestMailbox::new(8),
    );

    runtime
        .try_send(binder, BinderMsg::StartBind)
        .expect("ingress accepts StartBind");
    runtime.step();
    runtime.step();

    let listener = listener_slot
        .lock()
        .expect("listener mutex")
        .expect("listener published");
    let local_addr = addr_slot
        .lock()
        .expect("addr mutex")
        .expect("addr published");

    let waiter = runtime.register(
        Waiter {
            listener,
            log: Rc::clone(&waiter_log),
            peer_slot: Arc::clone(&peer_slot),
        },
        TestMailbox::new(8),
    );

    runtime
        .try_send(waiter, WaiterMsg::StartAccept)
        .expect("ingress accepts StartAccept");
    runtime.step();

    let client = TcpStream::connect(local_addr).expect("connect to listener");
    let client_local_addr = client.local_addr().expect("client local_addr");

    let deadline = Instant::now() + Duration::from_secs(2);
    while peer_slot.lock().expect("peer mutex").is_none() {
        runtime.step();
        if Instant::now() > deadline {
            panic!(
                "timed out waiting for accepted peer address; trace = {:#?}",
                runtime.trace()
            );
        }
    }

    assert_eq!(
        *peer_slot.lock().expect("peer mutex"),
        Some(client_local_addr),
        "accepted peer_addr must match the connecting client's local address"
    );
    assert_eq!(
        waiter_log.borrow().as_slice(),
        &[WaiterMsg::AcceptedObserved(client_local_addr)],
    );

    let kinds = call_event_kinds(runtime.trace());
    assert!(
        kinds.iter().any(|k| matches!(
            k,
            RuntimeEventKind::CallCompleted {
                call_kind: CallKind::TcpAccept,
                ..
            }
        )),
        "trace must record CallCompleted for accept: {kinds:?}"
    );

    drop(client);
}

#[test]
fn call_id_increments_in_submission_order() {
    let log = Rc::new(RefCell::new(Vec::new()));
    let mut runtime = Runtime::new(TestShard, TestMailboxFactory);
    let probe = runtime.register(
        Probe {
            log: Rc::clone(&log),
        },
        TestMailbox::new(8),
    );

    runtime
        .try_send(probe, ProbeMsg::StartInvalidAccept)
        .expect("first call");
    runtime.step();
    runtime
        .try_send(probe, ProbeMsg::StartInvalidAccept)
        .expect("second call");
    runtime.step();
    // A few more steps so failures complete and translators run.
    for _ in 0..3 {
        runtime.step();
    }

    let dispatch_ids: Vec<CallId> = runtime
        .trace()
        .iter()
        .filter_map(|event| match event.kind() {
            RuntimeEventKind::CallDispatchAttempted { call_id, .. } => Some(call_id),
            _ => None,
        })
        .collect();
    assert_eq!(dispatch_ids.len(), 2);
    assert!(
        dispatch_ids[0].get() < dispatch_ids[1].get(),
        "call ids must be monotonic: {dispatch_ids:?}"
    );
    assert_eq!(dispatch_ids[0].get(), 1, "first call id is 1");

    // The translator runs and produces InvalidResourceObserved for both
    // calls.
    assert_eq!(
        *log.borrow(),
        vec![
            ProbeMsg::InvalidResourceObserved,
            ProbeMsg::InvalidResourceObserved,
        ]
    );
}

#[test]
fn isolate_without_call_effects_compiles_with_infallible() {
    // Compile-only smoke: an isolate that never issues call effects keeps
    // the tina/tina-runtime pre-012 ergonomics by setting
    // `type Call = Infallible`.
    #[derive(Debug)]
    struct Quiet;

    impl Isolate for Quiet {
        type Message = ();
        type Reply = ();
        type Send = Outbound<NeverOutbound>;
        type Spawn = Infallible;
        type SpawnObserved = std::convert::Infallible;
        type Call = Infallible;
        type Fact = ::std::convert::Infallible;
        type Shard = TestShard;

        fn handle(
            &mut self,
            _msg: (),
            _ctx: &mut Context<'_, Self::Shard, Self::Reply>,
        ) -> Effect<Self> {
            Effect::Noop
        }
    }

    let mut runtime = Runtime::new(TestShard, TestMailboxFactory);
    let quiet: Address<()> = runtime.register(Quiet, TestMailbox::new(2));
    runtime.try_send(quiet, ()).expect("send");
    runtime.step();
}
