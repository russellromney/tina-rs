//! Focused simulator coverage for the Unix-domain rails: connect/accept
//! parking symmetry, wrong-resource typed errors, peer-close-while-read
//! settling, and listener-close refusing a parked connect.

use std::convert::Infallible;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tina::{Context, Effect, Isolate, Outbound, Shard, ShardId};
use tina_runtime::{
    CallCompletionRejectedReason, CallError, CallKind, LoopStep, RuntimeCall, RuntimeEventKind,
    UnixAcceptReply, UnixBindReply, UnixConnectReply, UnixReadReply, UnixStreamId, UnixWriteAll,
    UnixWriteOwnedReply, UnixWriteReply, sleep, unix_accept, unix_bind, unix_close_listener,
    unix_close_stream, unix_connect, unix_read, unix_write,
};
use tina_sim::{ReplayArtifact, Simulator, SimulatorConfig, dst::InvariantSuite};

#[derive(Debug, Default)]
struct UnixShard;

impl Shard for UnixShard {
    fn id(&self) -> ShardId {
        ShardId::new(117)
    }
}

fn sock(name: &str) -> PathBuf {
    PathBuf::from(format!("/tmp/tina-sim-unix-{name}.sock"))
}

// ---------------------------------------------------------------------------
// 1. connect parks until accept, then the pair resolves.
//
// The server binds, then waits before accepting. The client connects
// during that window, so the connect must park and later resolve
// against the arriving accept. The echo round-trip proves the
// connect-park path works.
// ---------------------------------------------------------------------------

#[allow(dead_code)]
#[derive(Debug)]
enum EchoServerMsg {
    Start,
    Bound(UnixBindReply),
    DelayDone(Result<(), CallError>),
    Accepted(UnixAcceptReply),
    Read(UnixReadReply),
    Wrote(UnixWriteOwnedReply),
    Done,
}

struct EchoServer {
    path: PathBuf,
    accept_delay: Duration,
    listener: Option<tina_runtime::UnixListenerId>,
    stream: Option<UnixStreamId>,
    write_all: Option<UnixWriteAll>,
}

impl Isolate for EchoServer {
    type Message = EchoServerMsg;
    type Reply = ();
    type Send = Outbound<Infallible>;
    type Spawn = Infallible;
    type SpawnObserved = Infallible;
    type Io = RuntimeCall<EchoServerMsg>;
    type Fact = Infallible;
    type Shard = UnixShard;

    fn handle(
        &mut self,
        msg: Self::Message,
        _ctx: &mut Context<'_, Self::Shard, Self::Reply>,
    ) -> Effect<Self> {
        match msg {
            EchoServerMsg::Start => unix_bind(self.path.clone()).then(EchoServerMsg::Bound),
            EchoServerMsg::Bound(Ok((listener, _))) => {
                self.listener = Some(listener);
                sleep(self.accept_delay).then(EchoServerMsg::DelayDone)
            }
            EchoServerMsg::Bound(Err(_)) => Effect::Stop,
            EchoServerMsg::DelayDone(_) => {
                unix_accept(self.listener.expect("listener")).then(EchoServerMsg::Accepted)
            }
            EchoServerMsg::Accepted(Ok(stream)) => {
                self.stream = Some(stream);
                unix_read(stream, 64).then(EchoServerMsg::Read)
            }
            EchoServerMsg::Accepted(Err(_)) => Effect::Stop,
            EchoServerMsg::Read(Ok(bytes)) => {
                if bytes.is_empty() {
                    match self.stream.take() {
                        Some(stream) => unix_close_stream(stream).then(|_| EchoServerMsg::Done),
                        None => Effect::Stop,
                    }
                } else {
                    let mut write_all = UnixWriteAll::new(self.stream.expect("stream"), bytes);
                    let effect = write_all
                        .next_effect(EchoServerMsg::Wrote)
                        .expect("echo payload is non-empty");
                    self.write_all = Some(write_all);
                    effect
                }
            }
            EchoServerMsg::Read(Err(_)) => Effect::Stop,
            EchoServerMsg::Wrote(reply) => {
                let write_all = self.write_all.as_mut().expect("write helper armed");
                match write_all.advance::<Self, _, _>(reply, EchoServerMsg::Wrote) {
                    LoopStep::Pending(effect) => effect,
                    LoopStep::Done(_) => {
                        self.write_all = None;
                        unix_read(self.stream.expect("stream"), 64).then(EchoServerMsg::Read)
                    }
                    LoopStep::Failed(_) => Effect::Stop,
                }
            }
            EchoServerMsg::Done => Effect::Stop,
        }
    }
}

#[allow(dead_code)]
#[derive(Debug)]
enum EchoClientMsg {
    Start,
    ConnectDelayDone(Result<(), CallError>),
    Connected(UnixConnectReply),
    Wrote(UnixWriteOwnedReply),
    Read(UnixReadReply),
    Done,
}

struct EchoClient {
    path: PathBuf,
    connect_delay: Duration,
    stream: Option<UnixStreamId>,
    write_all: Option<UnixWriteAll>,
    received: Arc<Mutex<Vec<u8>>>,
    connect_error: Arc<Mutex<Option<CallError>>>,
}

impl Isolate for EchoClient {
    type Message = EchoClientMsg;
    type Reply = ();
    type Send = Outbound<Infallible>;
    type Spawn = Infallible;
    type SpawnObserved = Infallible;
    type Io = RuntimeCall<EchoClientMsg>;
    type Fact = Infallible;
    type Shard = UnixShard;

    fn handle(
        &mut self,
        msg: Self::Message,
        _ctx: &mut Context<'_, Self::Shard, Self::Reply>,
    ) -> Effect<Self> {
        match msg {
            EchoClientMsg::Start => sleep(self.connect_delay).then(EchoClientMsg::ConnectDelayDone),
            EchoClientMsg::ConnectDelayDone(_) => {
                unix_connect(self.path.clone()).then(EchoClientMsg::Connected)
            }
            EchoClientMsg::Connected(Ok(stream)) => {
                self.stream = Some(stream);
                let mut write_all = UnixWriteAll::new(stream, b"ping".to_vec());
                let effect = write_all
                    .next_effect(EchoClientMsg::Wrote)
                    .expect("client payload is non-empty");
                self.write_all = Some(write_all);
                effect
            }
            EchoClientMsg::Connected(Err(error)) => {
                *self.connect_error.lock().unwrap() = Some(error);
                Effect::Stop
            }
            EchoClientMsg::Wrote(reply) => {
                let write_all = self.write_all.as_mut().expect("write helper armed");
                match write_all.advance::<Self, _, _>(reply, EchoClientMsg::Wrote) {
                    LoopStep::Pending(effect) => effect,
                    LoopStep::Done(_) => {
                        self.write_all = None;
                        unix_read(self.stream.expect("stream"), 64).then(EchoClientMsg::Read)
                    }
                    LoopStep::Failed(_) => Effect::Stop,
                }
            }
            EchoClientMsg::Read(Ok(bytes)) => {
                if !bytes.is_empty() {
                    self.received.lock().unwrap().extend_from_slice(&bytes);
                }
                if self.received.lock().unwrap().len() < 4 {
                    unix_read(self.stream.expect("stream"), 64).then(EchoClientMsg::Read)
                } else {
                    match self.stream.take() {
                        Some(stream) => unix_close_stream(stream).then(|_| EchoClientMsg::Done),
                        None => Effect::Stop,
                    }
                }
            }
            EchoClientMsg::Read(Err(_)) => Effect::Stop,
            EchoClientMsg::Done => Effect::Stop,
        }
    }
}

#[test]
fn connect_parks_until_accept_then_pairs() {
    let mut config = SimulatorConfig::default();
    config.unix.default_write_cap = 1;
    let mut sim = Simulator::new(UnixShard, config);
    let received = Arc::new(Mutex::new(Vec::new()));
    let connect_error = Arc::new(Mutex::new(None));

    // Server accepts only after 5ms; the client connects at 1ms, so the
    // connect arrives while no accept is parked and must park itself.
    let server = sim.register(EchoServer {
        path: sock("connect-parks"),
        accept_delay: Duration::from_millis(5),
        listener: None,
        stream: None,
        write_all: None,
    });
    let client = sim.register(EchoClient {
        path: sock("connect-parks"),
        connect_delay: Duration::from_millis(1),
        stream: None,
        write_all: None,
        received: Arc::clone(&received),
        connect_error: Arc::clone(&connect_error),
    });
    sim.try_send(server, EchoServerMsg::Start).unwrap();
    sim.try_send(client, EchoClientMsg::Start).unwrap();
    sim.run_until_quiescent();

    assert_eq!(*connect_error.lock().unwrap(), None, "connect must succeed");
    assert_eq!(received.lock().unwrap().as_slice(), b"ping");
    InvariantSuite::standard().assert(sim.trace());
}

// ---------------------------------------------------------------------------
// 2. wrong-resource: read/write/accept on an id the runtime never opened
//    returns InvalidResource, not TCP-shaped accidental success.
// ---------------------------------------------------------------------------

#[allow(dead_code)]
#[derive(Debug)]
enum WrongResourceMsg {
    Start,
    Read(UnixReadReply),
}

type ReadObservation = Arc<Mutex<Option<Result<Vec<u8>, CallError>>>>;

struct WrongResource {
    observed: ReadObservation,
}

impl Isolate for WrongResource {
    type Message = WrongResourceMsg;
    type Reply = ();
    type Send = Outbound<Infallible>;
    type Spawn = Infallible;
    type SpawnObserved = Infallible;
    type Io = RuntimeCall<WrongResourceMsg>;
    type Fact = Infallible;
    type Shard = UnixShard;

    fn handle(
        &mut self,
        msg: Self::Message,
        _ctx: &mut Context<'_, Self::Shard, Self::Reply>,
    ) -> Effect<Self> {
        match msg {
            // A stream id the runtime never handed out.
            WrongResourceMsg::Start => {
                unix_read(UnixStreamId::new(9999), 16).then(WrongResourceMsg::Read)
            }
            WrongResourceMsg::Read(result) => {
                *self.observed.lock().unwrap() = Some(result);
                Effect::Stop
            }
        }
    }
}

#[test]
fn read_on_unknown_stream_is_invalid_resource() {
    let mut sim = Simulator::new(UnixShard, SimulatorConfig::default());
    let observed = Arc::new(Mutex::new(None));
    let actor = sim.register(WrongResource {
        observed: Arc::clone(&observed),
    });
    sim.try_send(actor, WrongResourceMsg::Start).unwrap();
    sim.run_until_quiescent();

    assert_eq!(
        *observed.lock().unwrap(),
        Some(Err(CallError::InvalidResource)),
        "reading an unopened Unix stream must be InvalidResource",
    );
}

// ---------------------------------------------------------------------------
// 3. peer close while a read is pending settles the read as EOF.
//
// The server parks a read with no bytes available; the client connects
// and immediately closes without writing. The server's pending read
// must settle with empty bytes (EOF), not hang.
// ---------------------------------------------------------------------------

#[allow(dead_code)]
#[derive(Debug)]
enum PeerCloseServerMsg {
    Start,
    Bound(UnixBindReply),
    Accepted(UnixAcceptReply),
    Read(UnixReadReply),
}

struct PeerCloseServer {
    path: PathBuf,
    listener: Option<tina_runtime::UnixListenerId>,
    read_outcome: Arc<Mutex<Option<Result<usize, CallError>>>>,
}

impl Isolate for PeerCloseServer {
    type Message = PeerCloseServerMsg;
    type Reply = ();
    type Send = Outbound<Infallible>;
    type Spawn = Infallible;
    type SpawnObserved = Infallible;
    type Io = RuntimeCall<PeerCloseServerMsg>;
    type Fact = Infallible;
    type Shard = UnixShard;

    fn handle(
        &mut self,
        msg: Self::Message,
        _ctx: &mut Context<'_, Self::Shard, Self::Reply>,
    ) -> Effect<Self> {
        match msg {
            PeerCloseServerMsg::Start => {
                unix_bind(self.path.clone()).then(PeerCloseServerMsg::Bound)
            }
            PeerCloseServerMsg::Bound(Ok((listener, _))) => {
                self.listener = Some(listener);
                unix_accept(listener).then(PeerCloseServerMsg::Accepted)
            }
            PeerCloseServerMsg::Bound(Err(_)) => Effect::Stop,
            PeerCloseServerMsg::Accepted(Ok(stream)) => {
                unix_read(stream, 64).then(PeerCloseServerMsg::Read)
            }
            PeerCloseServerMsg::Accepted(Err(_)) => Effect::Stop,
            PeerCloseServerMsg::Read(result) => {
                *self.read_outcome.lock().unwrap() = Some(result.map(|bytes| bytes.len()));
                Effect::Stop
            }
        }
    }
}

#[allow(dead_code)]
#[derive(Debug)]
enum CloserClientMsg {
    Start,
    Delay(Result<(), CallError>),
    Connected(UnixConnectReply),
    Closed,
}

struct CloserClient {
    path: PathBuf,
}

impl Isolate for CloserClient {
    type Message = CloserClientMsg;
    type Reply = ();
    type Send = Outbound<Infallible>;
    type Spawn = Infallible;
    type SpawnObserved = Infallible;
    type Io = RuntimeCall<CloserClientMsg>;
    type Fact = Infallible;
    type Shard = UnixShard;

    fn handle(
        &mut self,
        msg: Self::Message,
        _ctx: &mut Context<'_, Self::Shard, Self::Reply>,
    ) -> Effect<Self> {
        match msg {
            // Delay so the server's accept+read are parked first.
            CloserClientMsg::Start => sleep(Duration::from_millis(2)).then(CloserClientMsg::Delay),
            CloserClientMsg::Delay(_) => {
                unix_connect(self.path.clone()).then(CloserClientMsg::Connected)
            }
            CloserClientMsg::Connected(Ok(stream)) => {
                unix_close_stream(stream).then(|_| CloserClientMsg::Closed)
            }
            CloserClientMsg::Connected(Err(_)) => Effect::Stop,
            CloserClientMsg::Closed => Effect::Stop,
        }
    }
}

#[test]
fn peer_close_while_read_pending_settles_as_eof() {
    let mut sim = Simulator::new(UnixShard, SimulatorConfig::default());
    let read_outcome = Arc::new(Mutex::new(None));
    let server = sim.register(PeerCloseServer {
        path: sock("peer-close"),
        listener: None,
        read_outcome: Arc::clone(&read_outcome),
    });
    let client = sim.register(CloserClient {
        path: sock("peer-close"),
    });
    sim.try_send(server, PeerCloseServerMsg::Start).unwrap();
    sim.try_send(client, CloserClientMsg::Start).unwrap();
    sim.run_until_quiescent();

    assert_eq!(
        *read_outcome.lock().unwrap(),
        Some(Ok(0)),
        "a pending read must settle as EOF (empty bytes) when the peer closes",
    );
}

// ---------------------------------------------------------------------------
// 4. listener close refuses a parked connect with a typed error.
// ---------------------------------------------------------------------------

#[allow(dead_code)]
#[derive(Debug)]
enum RefuseServerMsg {
    Start,
    Bound(UnixBindReply),
    DelayDone(Result<(), CallError>),
    Closed(Result<(), CallError>),
}

struct RefuseServer {
    path: PathBuf,
    listener: Option<tina_runtime::UnixListenerId>,
}

impl Isolate for RefuseServer {
    type Message = RefuseServerMsg;
    type Reply = ();
    type Send = Outbound<Infallible>;
    type Spawn = Infallible;
    type SpawnObserved = Infallible;
    type Io = RuntimeCall<RefuseServerMsg>;
    type Fact = Infallible;
    type Shard = UnixShard;

    fn handle(
        &mut self,
        msg: Self::Message,
        _ctx: &mut Context<'_, Self::Shard, Self::Reply>,
    ) -> Effect<Self> {
        match msg {
            RefuseServerMsg::Start => unix_bind(self.path.clone()).then(RefuseServerMsg::Bound),
            RefuseServerMsg::Bound(Ok((listener, _))) => {
                self.listener = Some(listener);
                // Never accept; close after a delay while a connect is parked.
                sleep(Duration::from_millis(5)).then(RefuseServerMsg::DelayDone)
            }
            RefuseServerMsg::Bound(Err(_)) => Effect::Stop,
            RefuseServerMsg::DelayDone(_) => {
                unix_close_listener(self.listener.expect("listener")).then(RefuseServerMsg::Closed)
            }
            RefuseServerMsg::Closed(_) => Effect::Stop,
        }
    }
}

#[test]
fn listener_close_refuses_parked_connect() {
    let mut sim = Simulator::new(UnixShard, SimulatorConfig::default());
    let connect_error = Arc::new(Mutex::new(None));
    let received = Arc::new(Mutex::new(Vec::new()));
    let server = sim.register(RefuseServer {
        path: sock("refuse"),
        listener: None,
    });
    // Client connects at 1ms (parks, since the server never accepts),
    // and the server closes the listener at 5ms.
    let client = sim.register(EchoClient {
        path: sock("refuse"),
        connect_delay: Duration::from_millis(1),
        stream: None,
        write_all: None,
        received: Arc::clone(&received),
        connect_error: Arc::clone(&connect_error),
    });
    sim.try_send(server, RefuseServerMsg::Start).unwrap();
    sim.try_send(client, EchoClientMsg::Start).unwrap();
    sim.run_until_quiescent();

    assert_eq!(
        *connect_error.lock().unwrap(),
        Some(CallError::Io),
        "closing a listener must refuse a parked connect with a typed error",
    );
    assert!(received.lock().unwrap().is_empty());
}

// ---------------------------------------------------------------------------
// 5. full peer inbound parks writes until reads free capacity.
// ---------------------------------------------------------------------------

#[derive(Debug)]
enum PressureServerMsg {
    Start,
    Bound(UnixBindReply),
    Accepted(UnixAcceptReply),
    DelayDone(Result<(), CallError>),
    FillerWrote(UnixWriteReply),
    Read(UnixReadReply),
    Closed,
}

struct PressureServer {
    path: PathBuf,
    listener: Option<tina_runtime::UnixListenerId>,
    stream: Option<UnixStreamId>,
    received: Arc<Mutex<Vec<u8>>>,
    target_len: usize,
    close_after_delay: bool,
    fill_completion_queue_on_read: bool,
}

impl Isolate for PressureServer {
    type Message = PressureServerMsg;
    type Reply = ();
    type Send = Outbound<Infallible>;
    type Spawn = Infallible;
    type SpawnObserved = Infallible;
    type Io = RuntimeCall<PressureServerMsg>;
    type Fact = Infallible;
    type Shard = UnixShard;

    fn handle(
        &mut self,
        msg: Self::Message,
        _ctx: &mut Context<'_, Self::Shard, Self::Reply>,
    ) -> Effect<Self> {
        match msg {
            PressureServerMsg::Start => unix_bind(self.path.clone()).then(PressureServerMsg::Bound),
            PressureServerMsg::Bound(Ok((listener, _))) => {
                self.listener = Some(listener);
                unix_accept(listener).then(PressureServerMsg::Accepted)
            }
            PressureServerMsg::Accepted(Ok(stream)) => {
                self.stream = Some(stream);
                sleep(Duration::from_millis(5)).then(PressureServerMsg::DelayDone)
            }
            PressureServerMsg::DelayDone(Ok(())) => {
                if self.close_after_delay {
                    unix_close_stream(self.stream.expect("accepted"))
                        .then(|_| PressureServerMsg::Closed)
                } else if self.fill_completion_queue_on_read {
                    let stream = self.stream.expect("accepted");
                    tina::batch([
                        unix_write(stream, b"z".to_vec()).then(PressureServerMsg::FillerWrote),
                        unix_read(stream, 2).then(PressureServerMsg::Read),
                    ])
                } else {
                    unix_read(self.stream.expect("accepted"), 2).then(PressureServerMsg::Read)
                }
            }
            PressureServerMsg::FillerWrote(result) => {
                let _ = result;
                tina::noop()
            }
            PressureServerMsg::Read(Ok(bytes)) => {
                let mut received = self.received.lock().expect("pressure receive buffer");
                received.extend_from_slice(&bytes);
                let done = received.len() == self.target_len;
                drop(received);
                if done {
                    Effect::Stop
                } else {
                    unix_read(self.stream.expect("accepted"), 2).then(PressureServerMsg::Read)
                }
            }
            PressureServerMsg::Closed => Effect::Stop,
            PressureServerMsg::Bound(Err(_))
            | PressureServerMsg::Accepted(Err(_))
            | PressureServerMsg::DelayDone(Err(_))
            | PressureServerMsg::Read(Err(_)) => Effect::Stop,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PressureClientReport {
    raw_counts: Vec<usize>,
    owned_result: Result<usize, CallError>,
    allocation_preserved: bool,
}

#[derive(Debug)]
enum PressureClientMsg {
    Start,
    DelayDone(Result<(), CallError>),
    Connected(UnixConnectReply),
    RawSeeded(UnixWriteReply),
    RawAfterPressure(UnixWriteReply),
    OwnedWrote(UnixWriteOwnedReply),
    StopNow,
}

struct PressureClient {
    path: PathBuf,
    stream: Option<UnixStreamId>,
    write_all: Option<UnixWriteAll>,
    allocation: Option<usize>,
    allocation_preserved: bool,
    raw_counts: Vec<usize>,
    report: Arc<Mutex<Option<PressureClientReport>>>,
    raw_after_pressure: bool,
    owned_payload: Vec<u8>,
    cancel_pending: bool,
}

impl PressureClient {
    fn finish(&mut self, owned_result: Result<usize, CallError>) -> Effect<Self> {
        *self.report.lock().expect("pressure client report") = Some(PressureClientReport {
            raw_counts: std::mem::take(&mut self.raw_counts),
            owned_result,
            allocation_preserved: self.allocation_preserved,
        });
        Effect::Stop
    }

    fn begin_owned(&mut self) -> Effect<Self> {
        let bytes = std::mem::take(&mut self.owned_payload);
        self.allocation = Some(bytes.as_ptr() as usize);
        let mut write_all = UnixWriteAll::new(self.stream.expect("connected"), bytes);
        let effect = write_all
            .next_effect(PressureClientMsg::OwnedWrote)
            .expect("owned payload is non-empty");
        self.write_all = Some(write_all);
        effect
    }
}

impl Isolate for PressureClient {
    type Message = PressureClientMsg;
    type Reply = ();
    type Send = Outbound<PressureClientMsg>;
    type Spawn = Infallible;
    type SpawnObserved = Infallible;
    type Io = RuntimeCall<PressureClientMsg>;
    type Fact = Infallible;
    type Shard = UnixShard;

    fn handle(
        &mut self,
        msg: Self::Message,
        ctx: &mut Context<'_, Self::Shard, Self::Reply>,
    ) -> Effect<Self> {
        match msg {
            PressureClientMsg::Start => {
                sleep(Duration::from_millis(1)).then(PressureClientMsg::DelayDone)
            }
            PressureClientMsg::DelayDone(Ok(())) => {
                unix_connect(self.path.clone()).then(PressureClientMsg::Connected)
            }
            PressureClientMsg::Connected(Ok(stream)) => {
                self.stream = Some(stream);
                unix_write(stream, b"ab".to_vec()).then(PressureClientMsg::RawSeeded)
            }
            PressureClientMsg::RawSeeded(Ok(count)) => {
                self.raw_counts.push(count);
                if self.raw_after_pressure {
                    unix_write(self.stream.expect("connected"), b"cd".to_vec())
                        .then(PressureClientMsg::RawAfterPressure)
                } else {
                    let effect = self.begin_owned();
                    if self.cancel_pending {
                        tina::batch([effect, tina::send(ctx.me(), PressureClientMsg::StopNow)])
                    } else {
                        effect
                    }
                }
            }
            PressureClientMsg::RawAfterPressure(Ok(count)) => {
                self.raw_counts.push(count);
                self.begin_owned()
            }
            PressureClientMsg::OwnedWrote(reply) => {
                let returned_allocation = match &reply {
                    Ok(reply) => reply.bytes.as_ptr() as usize,
                    Err(error) => error.bytes.as_ptr() as usize,
                };
                self.allocation_preserved &= self.allocation == Some(returned_allocation);
                let write_all = self.write_all.as_mut().expect("write helper armed");
                match write_all.advance::<Self, _, _>(reply, PressureClientMsg::OwnedWrote) {
                    LoopStep::Pending(effect) => effect,
                    LoopStep::Done(written) => self.finish(Ok(written)),
                    LoopStep::Failed(error) => self.finish(Err(error)),
                }
            }
            PressureClientMsg::DelayDone(Err(error))
            | PressureClientMsg::Connected(Err(error))
            | PressureClientMsg::RawSeeded(Err(error))
            | PressureClientMsg::RawAfterPressure(Err(error)) => self.finish(Err(error)),
            PressureClientMsg::StopNow => Effect::Stop,
        }
    }
}

fn run_unix_backpressure_scenario() -> (Vec<u8>, PressureClientReport, ReplayArtifact) {
    let mut config = SimulatorConfig::default();
    config.unix.default_inbound_capacity = 2;
    config.unix.default_write_cap = 2;
    let mut sim = Simulator::new(UnixShard, config);
    let received = Arc::new(Mutex::new(Vec::new()));
    let report = Arc::new(Mutex::new(None));
    let path = sock("backpressure");
    let server = sim.register(PressureServer {
        path: path.clone(),
        listener: None,
        stream: None,
        received: Arc::clone(&received),
        target_len: 10,
        close_after_delay: false,
        fill_completion_queue_on_read: false,
    });
    let client = sim.register(PressureClient {
        path,
        stream: None,
        write_all: None,
        allocation: None,
        allocation_preserved: true,
        raw_counts: Vec::new(),
        report: Arc::clone(&report),
        raw_after_pressure: true,
        owned_payload: b"efghij".to_vec(),
        cancel_pending: false,
    });
    sim.try_send(server, PressureServerMsg::Start).unwrap();
    sim.try_send(client, PressureClientMsg::Start).unwrap();
    sim.run_until_quiescent();
    let received = received.lock().expect("pressure receive buffer").clone();
    let report = report
        .lock()
        .expect("pressure client report")
        .clone()
        .expect("client completed");
    (received, report, sim.replay_artifact())
}

#[test]
fn full_peer_inbound_parks_raw_and_owned_writes_until_reads_drain() {
    let (received, report, artifact) = run_unix_backpressure_scenario();
    assert_eq!(received, b"abcdefghij");
    assert_eq!(
        report,
        PressureClientReport {
            raw_counts: vec![2, 2],
            owned_result: Ok(6),
            allocation_preserved: true,
        }
    );
    InvariantSuite::standard().assert(artifact.event_record());

    let (replayed_received, replayed_report, replayed_artifact) = run_unix_backpressure_scenario();
    assert_eq!(replayed_received, received);
    assert_eq!(replayed_report, report);
    assert_eq!(replayed_artifact, artifact);
}

fn run_unix_pending_write_close_scenario(
    cancel_requester: bool,
) -> (Option<PressureClientReport>, ReplayArtifact) {
    let mut config = SimulatorConfig::default();
    config.unix.default_inbound_capacity = 2;
    config.unix.default_write_cap = 2;
    let mut sim = Simulator::new(UnixShard, config);
    let received = Arc::new(Mutex::new(Vec::new()));
    let report = Arc::new(Mutex::new(None));
    let path = sock(if cancel_requester {
        "cancel-pending-write"
    } else {
        "close-pending-write"
    });
    let server = sim.register(PressureServer {
        path: path.clone(),
        listener: None,
        stream: None,
        received,
        target_len: 0,
        close_after_delay: true,
        fill_completion_queue_on_read: false,
    });
    let client = sim.register(PressureClient {
        path,
        stream: None,
        write_all: None,
        allocation: None,
        allocation_preserved: true,
        raw_counts: Vec::new(),
        report: Arc::clone(&report),
        raw_after_pressure: false,
        owned_payload: b"cd".to_vec(),
        cancel_pending: cancel_requester,
    });
    sim.try_send(server, PressureServerMsg::Start).unwrap();
    sim.try_send(client, PressureClientMsg::Start).unwrap();
    sim.run_until_quiescent();
    let report = report.lock().expect("pressure client report").clone();
    assert!(!sim.has_in_flight_calls());
    (report, sim.replay_artifact())
}

#[test]
fn closing_peer_fails_parked_owned_write_and_returns_allocation() {
    let (report, artifact) = run_unix_pending_write_close_scenario(false);
    assert_eq!(
        report,
        Some(PressureClientReport {
            raw_counts: vec![2],
            owned_result: Err(CallError::Io),
            allocation_preserved: true,
        })
    );
    InvariantSuite::standard().assert(artifact.event_record());
}

#[test]
fn requester_stop_cancels_parked_owned_write_without_lingering_work() {
    let (report, artifact) = run_unix_pending_write_close_scenario(true);
    assert_eq!(
        report, None,
        "stopped requester must not run its continuation"
    );
    assert!(artifact.event_record().iter().any(|event| {
        matches!(
            event.kind(),
            RuntimeEventKind::CallCompletionRejected {
                call_kind: CallKind::UnixWrite,
                reason: CallCompletionRejectedReason::RequesterClosed,
                ..
            }
        )
    }));
    InvariantSuite::standard().assert(artifact.event_record());
}

#[test]
fn completion_capacity_failure_returns_the_parked_owned_buffer() {
    let mut config = SimulatorConfig::default();
    config.unix.default_inbound_capacity = 2;
    config.unix.default_write_cap = 2;
    config.tcp.pending_completion_capacity = 2;
    let mut sim = Simulator::new(UnixShard, config);
    let received = Arc::new(Mutex::new(Vec::new()));
    let report = Arc::new(Mutex::new(None));
    let path = sock("completion-pressure");
    let server = sim.register(PressureServer {
        path: path.clone(),
        listener: None,
        stream: None,
        received,
        target_len: 2,
        close_after_delay: false,
        fill_completion_queue_on_read: true,
    });
    let client = sim.register(PressureClient {
        path,
        stream: None,
        write_all: None,
        allocation: None,
        allocation_preserved: true,
        raw_counts: Vec::new(),
        report: Arc::clone(&report),
        raw_after_pressure: false,
        owned_payload: b"cd".to_vec(),
        cancel_pending: false,
    });
    sim.try_send(server, PressureServerMsg::Start).unwrap();
    sim.try_send(client, PressureClientMsg::Start).unwrap();
    sim.run_until_quiescent();

    assert_eq!(
        report.lock().expect("pressure client report").as_ref(),
        Some(&PressureClientReport {
            raw_counts: vec![2],
            owned_result: Err(CallError::Io),
            allocation_preserved: true,
        })
    );
    assert!(!sim.has_in_flight_calls());
    InvariantSuite::standard().assert(sim.trace());
}
