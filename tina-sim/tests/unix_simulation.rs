//! Focused simulator coverage for the Unix-domain rails: connect/accept
//! parking symmetry, wrong-resource typed errors, peer-close-while-read
//! settling, and listener-close refusing a parked connect.

use std::convert::Infallible;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tina::{Context, Effect, Isolate, Outbound, Shard, ShardId};
use tina_runtime::{
    CallError, RuntimeCall, UnixAcceptReply, UnixBindReply, UnixConnectReply, UnixReadReply,
    UnixStreamId, UnixWriteReply, sleep, unix_accept, unix_bind, unix_close_listener,
    unix_close_stream, unix_connect, unix_read, unix_write,
};
use tina_sim::{Simulator, SimulatorConfig};

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
    Wrote(UnixWriteReply),
    Done,
}

struct EchoServer {
    path: PathBuf,
    accept_delay: Duration,
    listener: Option<tina_runtime::UnixListenerId>,
    stream: Option<UnixStreamId>,
    pending: Vec<u8>,
}

impl Isolate for EchoServer {
    type Message = EchoServerMsg;
    type Reply = ();
    type Send = Outbound<Infallible>;
    type Spawn = Infallible;
    type SpawnObserved = Infallible;
    type Call = RuntimeCall<EchoServerMsg>;
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
                    self.pending = bytes;
                    unix_write(self.stream.expect("stream"), self.pending.clone())
                        .then(EchoServerMsg::Wrote)
                }
            }
            EchoServerMsg::Read(Err(_)) => Effect::Stop,
            EchoServerMsg::Wrote(Ok(count)) => {
                let drained = count.min(self.pending.len());
                self.pending.drain(..drained);
                if self.pending.is_empty() {
                    unix_read(self.stream.expect("stream"), 64).then(EchoServerMsg::Read)
                } else {
                    unix_write(self.stream.expect("stream"), self.pending.clone())
                        .then(EchoServerMsg::Wrote)
                }
            }
            EchoServerMsg::Wrote(Err(_)) => Effect::Stop,
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
    Wrote(UnixWriteReply),
    Read(UnixReadReply),
    Done,
}

struct EchoClient {
    path: PathBuf,
    connect_delay: Duration,
    stream: Option<UnixStreamId>,
    pending: Vec<u8>,
    received: Arc<Mutex<Vec<u8>>>,
    connect_error: Arc<Mutex<Option<CallError>>>,
}

impl Isolate for EchoClient {
    type Message = EchoClientMsg;
    type Reply = ();
    type Send = Outbound<Infallible>;
    type Spawn = Infallible;
    type SpawnObserved = Infallible;
    type Call = RuntimeCall<EchoClientMsg>;
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
                self.pending = b"ping".to_vec();
                unix_write(stream, self.pending.clone()).then(EchoClientMsg::Wrote)
            }
            EchoClientMsg::Connected(Err(error)) => {
                *self.connect_error.lock().unwrap() = Some(error);
                Effect::Stop
            }
            EchoClientMsg::Wrote(Ok(count)) => {
                let drained = count.min(self.pending.len());
                self.pending.drain(..drained);
                if self.pending.is_empty() {
                    unix_read(self.stream.expect("stream"), 64).then(EchoClientMsg::Read)
                } else {
                    unix_write(self.stream.expect("stream"), self.pending.clone())
                        .then(EchoClientMsg::Wrote)
                }
            }
            EchoClientMsg::Wrote(Err(_)) => Effect::Stop,
            EchoClientMsg::Read(Ok(bytes)) => {
                if !bytes.is_empty() {
                    self.received.lock().unwrap().extend_from_slice(&bytes);
                }
                match self.stream.take() {
                    Some(stream) => unix_close_stream(stream).then(|_| EchoClientMsg::Done),
                    None => Effect::Stop,
                }
            }
            EchoClientMsg::Read(Err(_)) => Effect::Stop,
            EchoClientMsg::Done => Effect::Stop,
        }
    }
}

#[test]
fn connect_parks_until_accept_then_pairs() {
    let mut sim = Simulator::new(UnixShard, SimulatorConfig::default());
    let received = Arc::new(Mutex::new(Vec::new()));
    let connect_error = Arc::new(Mutex::new(None));

    // Server accepts only after 5ms; the client connects at 1ms, so the
    // connect arrives while no accept is parked and must park itself.
    let server = sim.register(EchoServer {
        path: sock("connect-parks"),
        accept_delay: Duration::from_millis(5),
        listener: None,
        stream: None,
        pending: Vec::new(),
    });
    let client = sim.register(EchoClient {
        path: sock("connect-parks"),
        connect_delay: Duration::from_millis(1),
        stream: None,
        pending: Vec::new(),
        received: Arc::clone(&received),
        connect_error: Arc::clone(&connect_error),
    });
    sim.try_send(server, EchoServerMsg::Start).unwrap();
    sim.try_send(client, EchoClientMsg::Start).unwrap();
    sim.run_until_quiescent();

    assert_eq!(*connect_error.lock().unwrap(), None, "connect must succeed");
    assert_eq!(received.lock().unwrap().as_slice(), b"ping");
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
    type Call = RuntimeCall<WrongResourceMsg>;
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
    type Call = RuntimeCall<PeerCloseServerMsg>;
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
    type Call = RuntimeCall<CloserClientMsg>;
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
    type Call = RuntimeCall<RefuseServerMsg>;
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
        pending: Vec::new(),
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
