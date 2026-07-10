//! Local admin sidecar over a simulator Unix-domain socket pair using
//! `tina_codec::LineFramer`. The client connects, sends a few line-
//! delimited commands, and the server echoes responses.

use std::convert::Infallible;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use std::time::Duration;

use tina::{Address, Context, Effect, Isolate, Outbound, Shard, ShardId};
use tina_codec::{DecodeStatus, LineFramer, decode_chunk};
use tina_runtime::{
    RuntimeCall, UnixAcceptReply, UnixBindReply, UnixConnectReply, UnixListenerId, UnixReadReply,
    UnixStreamId, UnixWriteReply, sleep, unix_accept, unix_bind, unix_close_stream, unix_connect,
    unix_read, unix_write,
};
use tina_sim::{Simulator, SimulatorConfig};

use crate::SpecimenReport;

#[derive(Debug, Default)]
pub struct AdminShard;

impl Shard for AdminShard {
    fn id(&self) -> ShardId {
        ShardId::new(102)
    }
}

// ---------- Server ---------------------------------------------------------

/// Max consecutive zero-progress writes the server tolerates before it
/// gives up on a wedged peer. Bounds the back-off retry loop so a peer
/// that never drains its inbound cannot pin the server forever.
const MAX_WRITE_BACKOFFS: u32 = 16;

#[derive(Debug)]
enum ServerMsg {
    Start,
    Bound(UnixBindReply),
    Accepted(UnixAcceptReply),
    Read(UnixReadReply),
    Wrote(UnixWriteReply),
    /// Back-off tick: the previous write made zero progress (peer
    /// inbound full). Retry after a short pause.
    RetryWrite,
    Done,
}

struct AdminServer {
    path: PathBuf,
    listener: Option<UnixListenerId>,
    stream: Option<UnixStreamId>,
    framer: LineFramer,
    write_pending: Vec<u8>,
    /// Consecutive zero-progress writes since the last byte landed.
    write_backoffs: u32,
    /// Records every command line the server saw, in order.
    seen: Arc<Mutex<Vec<Vec<u8>>>>,
    malformed: Arc<Mutex<bool>>,
}

impl Isolate for AdminServer {
    type Message = ServerMsg;
    type Reply = ();
    type Send = Outbound<Infallible>;
    type Spawn = Infallible;
    type SpawnObserved = Infallible;
    type Io = RuntimeCall<ServerMsg>;
    type Fact = Infallible;
    type Shard = AdminShard;

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
                    // Peer closed.
                    if let Some(stream) = self.stream.take() {
                        unix_close_stream(stream).then(|_| ServerMsg::Done)
                    } else {
                        Effect::Stop
                    }
                } else {
                    let mut reply_buffer = Vec::new();
                    let mut should_close = false;
                    let seen = &self.seen;
                    let status = decode_chunk(&mut self.framer, &bytes, |line| {
                        if !should_close {
                            seen.lock().unwrap().push(line.clone());
                            if line == b"shutdown" {
                                should_close = true;
                            } else {
                                let mut response = b"ok ".to_vec();
                                response.extend_from_slice(&line);
                                response.push(b'\n');
                                reply_buffer.extend_from_slice(&response);
                            }
                        }
                    });
                    if !should_close
                        && matches!(status, DecodeStatus::Malformed(_) | DecodeStatus::Full)
                    {
                        *self.malformed.lock().unwrap() = true;
                        should_close = true;
                    }
                    if should_close {
                        if let Some(stream) = self.stream.take() {
                            return unix_close_stream(stream).then(|_| ServerMsg::Done);
                        }
                        return Effect::Stop;
                    }
                    if reply_buffer.is_empty() {
                        unix_read(self.stream.expect("stream"), 64).then(ServerMsg::Read)
                    } else {
                        self.write_pending = reply_buffer.clone();
                        unix_write(self.stream.expect("stream"), reply_buffer)
                            .then(ServerMsg::Wrote)
                    }
                }
            }
            ServerMsg::Read(Err(_)) => Effect::Stop,
            ServerMsg::Wrote(Ok(count)) => {
                if count == 0 {
                    // Peer inbound full: zero-progress write. Back off
                    // instead of hot-spinning; give up after a bounded
                    // number of attempts so a wedged peer cannot pin us.
                    self.write_backoffs += 1;
                    if self.write_backoffs >= MAX_WRITE_BACKOFFS {
                        if let Some(stream) = self.stream.take() {
                            return unix_close_stream(stream).then(|_| ServerMsg::Done);
                        }
                        return Effect::Stop;
                    }
                    return sleep(Duration::from_millis(1)).then(|_| ServerMsg::RetryWrite);
                }
                self.write_backoffs = 0;
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
            ServerMsg::RetryWrite => {
                let pending = self.write_pending.clone();
                unix_write(self.stream.expect("stream"), pending).then(ServerMsg::Wrote)
            }
            ServerMsg::Done => Effect::Stop,
        }
    }
}

// ---------- Client ---------------------------------------------------------

#[derive(Debug)]
enum ClientMsg {
    Start,
    Connected(UnixConnectReply),
    Wrote(UnixWriteReply),
    Read(UnixReadReply),
    Done,
}

struct AdminClient {
    path: PathBuf,
    stream: Option<UnixStreamId>,
    outbound: Vec<u8>,
    write_pending: Vec<u8>,
    /// Bytes the client received from the server.
    received: Arc<Mutex<Vec<u8>>>,
}

impl Isolate for AdminClient {
    type Message = ClientMsg;
    type Reply = ();
    type Send = Outbound<Infallible>;
    type Spawn = Infallible;
    type SpawnObserved = Infallible;
    type Io = RuntimeCall<ClientMsg>;
    type Fact = Infallible;
    type Shard = AdminShard;

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
                    unix_read(self.stream.expect("stream"), 64).then(ClientMsg::Read)
                } else {
                    let pending = self.write_pending.clone();
                    unix_write(self.stream.expect("stream"), pending).then(ClientMsg::Wrote)
                }
            }
            ClientMsg::Wrote(Err(_)) => Effect::Stop,
            ClientMsg::Read(Ok(bytes)) => {
                if bytes.is_empty() {
                    // EOF
                    if let Some(stream) = self.stream.take() {
                        return unix_close_stream(stream).then(|_| ClientMsg::Done);
                    }
                    return Effect::Stop;
                }
                self.received.lock().unwrap().extend_from_slice(&bytes);
                unix_read(self.stream.expect("stream"), 64).then(ClientMsg::Read)
            }
            ClientMsg::Read(Err(_)) => Effect::Stop,
            ClientMsg::Done => Effect::Stop,
        }
    }
}

/// Result of one admin-socket exchange.
#[derive(Debug, Clone)]
pub struct AdminRun {
    /// Lines the server saw (in order).
    pub server_saw: Vec<Vec<u8>>,
    /// Bytes the client received from the server.
    pub client_received: Vec<u8>,
    /// True if the server's `LineFramer` rejected the input as full or
    /// malformed.
    pub server_saw_malformed_or_full: bool,
}

/// Run one client/server exchange over the simulator Unix rails.
pub fn run_admin_socket(path: PathBuf, commands: Vec<&[u8]>, max_line_len: usize) -> AdminRun {
    let mut sim = Simulator::new(AdminShard, SimulatorConfig::default());
    let server_seen = Arc::new(Mutex::new(Vec::new()));
    let server_malformed = Arc::new(Mutex::new(false));
    let received = Arc::new(Mutex::new(Vec::new()));

    let server = AdminServer {
        path: path.clone(),
        listener: None,
        stream: None,
        framer: LineFramer::new(max_line_len),
        write_pending: Vec::new(),
        write_backoffs: 0,
        seen: Arc::clone(&server_seen),
        malformed: Arc::clone(&server_malformed),
    };
    let server_addr: Address<ServerMsg, ()> = sim.register(server);
    let mut outbound = Vec::new();
    for cmd in commands {
        outbound.extend_from_slice(cmd);
    }
    let client = AdminClient {
        path,
        stream: None,
        outbound,
        write_pending: Vec::new(),
        received: Arc::clone(&received),
    };
    let client_addr: Address<ClientMsg, ()> = sim.register(client);
    sim.try_send(server_addr, ServerMsg::Start).unwrap();
    sim.try_send(client_addr, ClientMsg::Start).unwrap();
    sim.run_until_quiescent();

    AdminRun {
        server_saw: server_seen.lock().unwrap().clone(),
        client_received: received.lock().unwrap().clone(),
        server_saw_malformed_or_full: *server_malformed.lock().unwrap(),
    }
}

/// Smoke command: drives a handful of commands and a graceful shutdown.
pub fn smoke() -> SpecimenReport {
    let result = run_admin_socket(
        PathBuf::from("/tmp/specimen_admin.sock"),
        vec![b"ping\n", b"status\n", b"shutdown\n"],
        64,
    );
    let frames = result.server_saw.len() as u64;
    let bytes = result.client_received.len() as u64;
    SpecimenReport {
        name: "admin_socket",
        bytes,
        frames,
        ok: !result.server_saw_malformed_or_full && frames >= 2,
        note: format!(
            "server_saw_lines={} bytes_received_by_client={}",
            frames, bytes,
        ),
    }
}

/// Bad-input proof: a single line longer than the configured cap must
/// surface `DecodeStatus::Full` and shut the connection down without
/// growing the framer past the cap.
pub fn bad_input_line_too_long() -> SpecimenReport {
    let mut huge = vec![b'X'; 64];
    huge.push(b'\n');
    let result = run_admin_socket(
        PathBuf::from("/tmp/specimen_admin_bad.sock"),
        vec![&huge],
        8,
    );
    SpecimenReport {
        name: "admin_socket:line_too_long",
        bytes: result.client_received.len() as u64,
        frames: result.server_saw.len() as u64,
        ok: result.server_saw_malformed_or_full,
        note: format!(
            "rejected_oversize_line={}",
            result.server_saw_malformed_or_full
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn simulator_delivers_coalesced_maximum_line_and_following_line() {
        let result = run_admin_socket(
            PathBuf::from("/tmp/specimen_admin_coalesced.sock"),
            vec![b"abcdefgh\n", b"x\n", b"shutdown\n"],
            8,
        );
        assert_eq!(
            result.server_saw,
            [b"abcdefgh".to_vec(), b"x".to_vec(), b"shutdown".to_vec()]
        );
        assert!(!result.server_saw_malformed_or_full);
    }

    #[test]
    fn shutdown_ignores_an_oversize_suffix_in_the_same_transport_chunk() {
        let result = run_admin_socket(
            PathBuf::from("/tmp/specimen_admin_shutdown_suffix.sock"),
            vec![b"shutdown\n", b"xxxxxxxxxxxxxxxx\n"],
            8,
        );
        assert_eq!(result.server_saw, [b"shutdown".to_vec()]);
        assert!(!result.server_saw_malformed_or_full);
    }
}
