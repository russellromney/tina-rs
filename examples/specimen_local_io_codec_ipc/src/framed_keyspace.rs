//! Mini-keyspace protocol with length-prefixed frames over a simulator
//! Unix-domain socket pair. Demonstrates how
//! `tina_codec::LengthDelimitedFramer` slots beside Tina-owned I/O.

use std::convert::Infallible;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use tina::{Address, Context, Effect, Isolate, Outbound, Shard, ShardId};
use tina_codec::{FrameDecision, LengthDelimitedFramer, LengthPrefix, encode_into};
use tina_runtime::{
    RuntimeCall, UnixAcceptReply, UnixBindReply, UnixConnectReply, UnixListenerId, UnixReadReply,
    UnixStreamId, UnixWriteReply, unix_accept, unix_bind, unix_close_stream, unix_connect,
    unix_read, unix_write,
};
use tina_sim::{Simulator, SimulatorConfig};

use crate::SpecimenReport;

#[derive(Debug, Default)]
pub struct KeyspaceShard;

impl Shard for KeyspaceShard {
    fn id(&self) -> ShardId {
        ShardId::new(103)
    }
}

#[derive(Debug)]
enum ServerMsg {
    Start,
    Bound(UnixBindReply),
    Accepted(UnixAcceptReply),
    Read(UnixReadReply),
    Wrote(UnixWriteReply),
    Done,
}

struct KeyspaceServer {
    path: PathBuf,
    listener: Option<UnixListenerId>,
    stream: Option<UnixStreamId>,
    framer: LengthDelimitedFramer,
    write_pending: Vec<u8>,
    /// Echo store: tracks the bytes the server received frame-by-frame.
    received_frames: Arc<Mutex<Vec<Vec<u8>>>>,
    saw_full: Arc<Mutex<bool>>,
}

impl Isolate for KeyspaceServer {
    type Message = ServerMsg;
    type Reply = ();
    type Send = Outbound<Infallible>;
    type Spawn = Infallible;
    type SpawnObserved = Infallible;
    type Call = RuntimeCall<ServerMsg>;
    type Fact = Infallible;
    type Shard = KeyspaceShard;

    fn handle(
        &mut self,
        msg: Self::Message,
        _ctx: &mut Context<'_, Self::Shard, Self::Reply>,
    ) -> Effect<Self> {
        match msg {
            ServerMsg::Start => unix_bind(self.path.clone()).then(ServerMsg::Bound),
            ServerMsg::Bound(Ok((listener, _))) => {
                self.listener = Some(listener);
                unix_accept(listener).then(ServerMsg::Accepted)
            }
            ServerMsg::Bound(Err(_)) => Effect::Stop,
            ServerMsg::Accepted(Ok(stream)) => {
                self.stream = Some(stream);
                unix_read(stream, 64).then(ServerMsg::Read)
            }
            ServerMsg::Accepted(Err(_)) => Effect::Stop,
            ServerMsg::Read(Ok(bytes)) => {
                if bytes.is_empty() {
                    if let Some(stream) = self.stream.take() {
                        unix_close_stream(stream).then(|_| ServerMsg::Done)
                    } else {
                        Effect::Stop
                    }
                } else {
                    self.framer.feed(bytes);
                    let mut response_buf = Vec::new();
                    let mut shutdown = false;
                    loop {
                        match self.framer.next_frame() {
                            FrameDecision::NeedMore => break,
                            FrameDecision::Frame(frame) => {
                                self.received_frames.lock().unwrap().push(frame.clone());
                                // Echo back with `ack:` prefix.
                                let mut payload = b"ack:".to_vec();
                                payload.extend_from_slice(&frame);
                                let _ = encode_into(LengthPrefix::U16, &payload, &mut response_buf);
                            }
                            FrameDecision::Malformed(_) | FrameDecision::Full => {
                                *self.saw_full.lock().unwrap() = true;
                                shutdown = true;
                                break;
                            }
                        }
                    }
                    if shutdown {
                        if let Some(stream) = self.stream.take() {
                            return unix_close_stream(stream).then(|_| ServerMsg::Done);
                        }
                        return Effect::Stop;
                    }
                    if response_buf.is_empty() {
                        unix_read(self.stream.expect("stream"), 64).then(ServerMsg::Read)
                    } else {
                        self.write_pending = response_buf.clone();
                        unix_write(self.stream.expect("stream"), response_buf)
                            .then(ServerMsg::Wrote)
                    }
                }
            }
            ServerMsg::Read(Err(_)) => Effect::Stop,
            ServerMsg::Wrote(Ok(count)) => {
                let drained = count.min(self.write_pending.len());
                self.write_pending.drain(..drained);
                if self.write_pending.is_empty() {
                    unix_read(self.stream.expect("stream"), 64).then(ServerMsg::Read)
                } else {
                    let pending = self.write_pending.clone();
                    unix_write(self.stream.expect("stream"), pending).then(ServerMsg::Wrote)
                }
            }
            ServerMsg::Wrote(Err(_)) => Effect::Stop,
            ServerMsg::Done => Effect::Stop,
        }
    }
}

#[derive(Debug)]
enum ClientMsg {
    Start,
    Connected(UnixConnectReply),
    Wrote(UnixWriteReply),
    Read(UnixReadReply),
    Done,
}

struct KeyspaceClient {
    path: PathBuf,
    stream: Option<UnixStreamId>,
    outbound: Vec<u8>,
    write_pending: Vec<u8>,
    received: Arc<Mutex<Vec<u8>>>,
}

impl Isolate for KeyspaceClient {
    type Message = ClientMsg;
    type Reply = ();
    type Send = Outbound<Infallible>;
    type Spawn = Infallible;
    type SpawnObserved = Infallible;
    type Call = RuntimeCall<ClientMsg>;
    type Fact = Infallible;
    type Shard = KeyspaceShard;

    fn handle(
        &mut self,
        msg: Self::Message,
        _ctx: &mut Context<'_, Self::Shard, Self::Reply>,
    ) -> Effect<Self> {
        match msg {
            ClientMsg::Start => unix_connect(self.path.clone()).then(ClientMsg::Connected),
            ClientMsg::Connected(Ok(stream)) => {
                self.stream = Some(stream);
                self.write_pending = std::mem::take(&mut self.outbound);
                let bytes = self.write_pending.clone();
                unix_write(stream, bytes).then(ClientMsg::Wrote)
            }
            ClientMsg::Connected(Err(_)) => Effect::Stop,
            ClientMsg::Wrote(Ok(count)) => {
                let drained = count.min(self.write_pending.len());
                self.write_pending.drain(..drained);
                if self.write_pending.is_empty() {
                    // Drive a single read for the first batch of
                    // responses, then close. Closing after one read
                    // keeps the smoke from deadlocking on a second
                    // unwakeable read; the server sees EOF and exits
                    // cleanly, and the test asserts on
                    // `server_frames` (which the server populates
                    // synchronously per parsed frame).
                    unix_read(self.stream.expect("stream"), 256).then(ClientMsg::Read)
                } else {
                    let pending = self.write_pending.clone();
                    unix_write(self.stream.expect("stream"), pending).then(ClientMsg::Wrote)
                }
            }
            ClientMsg::Wrote(Err(_)) => Effect::Stop,
            ClientMsg::Read(Ok(bytes)) => {
                if !bytes.is_empty() {
                    self.received.lock().unwrap().extend_from_slice(&bytes);
                }
                if let Some(stream) = self.stream.take() {
                    return unix_close_stream(stream).then(|_| ClientMsg::Done);
                }
                Effect::Stop
            }
            ClientMsg::Read(Err(_)) => Effect::Stop,
            ClientMsg::Done => Effect::Stop,
        }
    }
}

#[derive(Debug, Clone)]
pub struct KeyspaceRun {
    pub server_frames: Vec<Vec<u8>>,
    pub client_bytes: Vec<u8>,
    pub server_saw_full_or_malformed: bool,
}

pub fn run_framed_keyspace(path: PathBuf, frames: &[&[u8]], max_body_len: usize) -> KeyspaceRun {
    let mut sim = Simulator::new(KeyspaceShard, SimulatorConfig::default());
    let received_frames = Arc::new(Mutex::new(Vec::new()));
    let saw_full = Arc::new(Mutex::new(false));
    let client_received = Arc::new(Mutex::new(Vec::new()));

    let server = KeyspaceServer {
        path: path.clone(),
        listener: None,
        stream: None,
        framer: LengthDelimitedFramer::new(LengthPrefix::U16, max_body_len),
        write_pending: Vec::new(),
        received_frames: Arc::clone(&received_frames),
        saw_full: Arc::clone(&saw_full),
    };
    let server_addr: Address<ServerMsg, ()> = sim.register(server);
    let mut outbound = Vec::new();
    for frame in frames {
        let _ = encode_into(LengthPrefix::U16, frame, &mut outbound);
    }
    let client = KeyspaceClient {
        path,
        stream: None,
        outbound,
        write_pending: Vec::new(),
        received: Arc::clone(&client_received),
    };
    let client_addr: Address<ClientMsg, ()> = sim.register(client);
    sim.try_send(server_addr, ServerMsg::Start).unwrap();
    sim.try_send(client_addr, ClientMsg::Start).unwrap();
    sim.run_until_quiescent();

    KeyspaceRun {
        server_frames: received_frames.lock().unwrap().clone(),
        client_bytes: client_received.lock().unwrap().clone(),
        server_saw_full_or_malformed: *saw_full.lock().unwrap(),
    }
}

pub fn smoke() -> SpecimenReport {
    let result = run_framed_keyspace(
        PathBuf::from("/tmp/specimen_framed_keyspace.sock"),
        &[b"set:a=1", b"set:b=2", b"get:a"],
        128,
    );
    SpecimenReport {
        name: "framed_keyspace",
        bytes: result.client_bytes.len() as u64,
        frames: result.server_frames.len() as u64,
        ok: !result.server_saw_full_or_malformed && result.server_frames.len() == 3,
        note: format!(
            "server_frames={} client_bytes={}",
            result.server_frames.len(),
            result.client_bytes.len()
        ),
    }
}

/// Bad-input proof: a frame whose declared length exceeds the body cap
/// is rejected by the framer before any body byte is allocated.
pub fn bad_input_frame_too_large() -> SpecimenReport {
    // Hand-craft an oversized frame: prefix announces 200, cap is 16.
    let mut oversized = Vec::new();
    oversized.extend_from_slice(&(200u16).to_be_bytes());
    oversized.extend_from_slice(&[b'A'; 200]);
    let frames: Vec<&[u8]> = vec![&oversized];
    // Bypass encode_into so we send the raw broken frame straight.
    let mut sim = Simulator::new(KeyspaceShard, SimulatorConfig::default());
    let received_frames = Arc::new(Mutex::new(Vec::new()));
    let saw_full = Arc::new(Mutex::new(false));
    let client_received = Arc::new(Mutex::new(Vec::new()));
    let path = PathBuf::from("/tmp/specimen_framed_keyspace_bad.sock");
    let server = KeyspaceServer {
        path: path.clone(),
        listener: None,
        stream: None,
        framer: LengthDelimitedFramer::new(LengthPrefix::U16, 16),
        write_pending: Vec::new(),
        received_frames: Arc::clone(&received_frames),
        saw_full: Arc::clone(&saw_full),
    };
    let server_addr: Address<ServerMsg, ()> = sim.register(server);
    let client = KeyspaceClient {
        path,
        stream: None,
        outbound: frames[0].to_vec(),
        write_pending: Vec::new(),
        received: Arc::clone(&client_received),
    };
    let client_addr: Address<ClientMsg, ()> = sim.register(client);
    sim.try_send(server_addr, ServerMsg::Start).unwrap();
    sim.try_send(client_addr, ClientMsg::Start).unwrap();
    sim.run_until_quiescent();

    let saw = *saw_full.lock().unwrap();
    SpecimenReport {
        name: "framed_keyspace:frame_too_large",
        bytes: 0,
        frames: 0,
        ok: saw,
        note: format!("framer_rejected_oversize_frame={}", saw),
    }
}
