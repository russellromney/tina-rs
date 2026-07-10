//! Mini-keyspace protocol with length-prefixed frames over a simulator
//! Unix-domain socket pair. Demonstrates how
//! `tina_codec::LengthDelimitedFramer` slots beside Tina-owned I/O.

use std::convert::Infallible;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use tina::{Address, Context, Effect, Isolate, Outbound, Shard, ShardId};
use tina_codec::{DecodeStatus, LengthDelimitedFramer, LengthPrefix, decode_chunk, encode_into};
use tina_runtime::{
    LoopStep, RuntimeCall, UnixAcceptReply, UnixBindReply, UnixConnectReply, UnixListenerId,
    UnixReadReply, UnixStreamId, UnixWriteAll, UnixWriteOwnedReply, unix_accept, unix_bind,
    unix_close_stream, unix_connect, unix_read,
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
    Wrote(UnixWriteOwnedReply),
    Done,
}

struct KeyspaceServer {
    path: PathBuf,
    listener: Option<UnixListenerId>,
    stream: Option<UnixStreamId>,
    framer: LengthDelimitedFramer,
    write_all: Option<UnixWriteAll>,
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
    type Io = RuntimeCall<ServerMsg>;
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
                    let mut response_buf = Vec::new();
                    let mut shutdown = false;
                    let received_frames = &self.received_frames;
                    let status = decode_chunk(&mut self.framer, &bytes, |frame| {
                        received_frames.lock().unwrap().push(frame.clone());
                        let mut payload = b"ack:".to_vec();
                        payload.extend_from_slice(&frame);
                        let _ = encode_into(LengthPrefix::U16, &payload, &mut response_buf);
                    });
                    if matches!(status, DecodeStatus::Malformed(_) | DecodeStatus::Full) {
                        *self.saw_full.lock().unwrap() = true;
                        shutdown = true;
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
                        let mut write_all =
                            UnixWriteAll::new(self.stream.expect("stream"), response_buf);
                        let effect = write_all
                            .next_effect(ServerMsg::Wrote)
                            .expect("response buffer is non-empty");
                        self.write_all = Some(write_all);
                        effect
                    }
                }
            }
            ServerMsg::Read(Err(_)) => Effect::Stop,
            ServerMsg::Wrote(reply) => {
                let write_all = self.write_all.as_mut().expect("write helper armed");
                match write_all.advance::<Self, _, _>(reply, ServerMsg::Wrote) {
                    LoopStep::Pending(effect) => effect,
                    LoopStep::Done(_) => {
                        self.write_all = None;
                        unix_read(self.stream.expect("stream"), 64).then(ServerMsg::Read)
                    }
                    LoopStep::Failed(_) => Effect::Stop,
                }
            }
            ServerMsg::Done => Effect::Stop,
        }
    }
}

#[derive(Debug)]
enum ClientMsg {
    Start,
    Connected(UnixConnectReply),
    Wrote(UnixWriteOwnedReply),
    Read(UnixReadReply),
    Done,
}

struct KeyspaceClient {
    path: PathBuf,
    stream: Option<UnixStreamId>,
    outbound: Vec<u8>,
    write_all: Option<UnixWriteAll>,
    received: Arc<Mutex<Vec<u8>>>,
}

impl Isolate for KeyspaceClient {
    type Message = ClientMsg;
    type Reply = ();
    type Send = Outbound<Infallible>;
    type Spawn = Infallible;
    type SpawnObserved = Infallible;
    type Io = RuntimeCall<ClientMsg>;
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
                let bytes = std::mem::take(&mut self.outbound);
                if bytes.is_empty() {
                    self.stream = None;
                    return unix_close_stream(stream).then(|_| ClientMsg::Done);
                }
                let mut write_all = UnixWriteAll::new(stream, bytes);
                let effect = write_all
                    .next_effect(ClientMsg::Wrote)
                    .expect("non-empty client payload has a write step");
                self.write_all = Some(write_all);
                effect
            }
            ClientMsg::Connected(Err(_)) => Effect::Stop,
            ClientMsg::Wrote(reply) => {
                let write_all = self.write_all.as_mut().expect("write helper armed");
                match write_all.advance::<Self, _, _>(reply, ClientMsg::Wrote) {
                    LoopStep::Pending(effect) => effect,
                    LoopStep::Done(_) => {
                        self.write_all = None;
                        // One read, then close. A second parked read never
                        // wakes and deadlocks the smoke; the server sees EOF
                        // and exits. Assertions use `server_frames`, filled
                        // synchronously per parsed frame.
                        unix_read(self.stream.expect("stream"), 256).then(ClientMsg::Read)
                    }
                    LoopStep::Failed(_) => Effect::Stop,
                }
            }
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
        write_all: None,
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
        write_all: None,
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
        write_all: None,
        received_frames: Arc::clone(&received_frames),
        saw_full: Arc::clone(&saw_full),
    };
    let server_addr: Address<ServerMsg, ()> = sim.register(server);
    let client = KeyspaceClient {
        path,
        stream: None,
        outbound: frames[0].to_vec(),
        write_all: None,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn simulator_delivers_coalesced_maximum_frame_and_following_frame() {
        let result = run_framed_keyspace(
            PathBuf::from("/tmp/specimen_keyspace_coalesced.sock"),
            &[b"abcd", b"x"],
            4,
        );
        assert_eq!(result.server_frames, [b"abcd".to_vec(), b"x".to_vec()]);
        assert!(!result.server_saw_full_or_malformed);
    }

    #[test]
    fn empty_frame_batch_does_not_arm_an_empty_write() {
        let result =
            run_framed_keyspace(PathBuf::from("/tmp/specimen_keyspace_empty.sock"), &[], 4);
        assert!(result.server_frames.is_empty());
        assert!(result.client_bytes.is_empty());
    }
}
