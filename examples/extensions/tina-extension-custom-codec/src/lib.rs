//! Extension smoke crate: a **custom codec** built on the public
//! [`tina_codec::SyncCodec`] seam, driving a tiny TCP-shaped service.
//!
//! The codec — [`SemicolonCodec`] — frames on the `;` delimiter with an
//! explicit maximum frame length and rejects an embedded NUL as
//! malformed. It is not a built-in framer; it proves that a third-party
//! type can be a `SyncCodec` and be driven by ordinary service code.
//!
//! The contract the codec keeps:
//!
//! - **No I/O.** The codec is plain state on the server isolate. Tina
//!   owns the sockets, capacity, cancellation, and replay; the codec only
//!   turns bytes into frames.
//! - **Bounded.** When a stream exceeds `max_frame` before a delimiter,
//!   the codec returns [`FrameDecision::Full`] before allocating further,
//!   instead of buffering without limit.
//! - **Replayable.** `feed` + `next_frame` are pure over the bytes seen,
//!   so the service runs identically on the simulator's Unix-domain rails
//!   (used here) and on a live socket.
//!
//! The service runs over `tina_sim`'s deterministic Unix-domain socket
//! rails, so the smoke test is reproducible — no wall-clock timing.

use std::convert::Infallible;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tina::{Address, Context, Effect, Isolate, Outbound, Shard, ShardId};
use tina_codec::{FrameDecision, SyncCodec};
use tina_runtime::{
    RuntimeCall, UnixAcceptReply, UnixBindReply, UnixConnectReply, UnixListenerId, UnixReadReply,
    UnixStreamId, UnixWriteReply, sleep, unix_accept, unix_bind, unix_close_stream, unix_connect,
    unix_read, unix_write,
};
use tina_sim::{Simulator, SimulatorConfig};

/// Max consecutive zero-progress writes before the server gives up on a
/// wedged peer. Bounds the back-off loop so a peer that never drains its
/// inbound cannot pin the server forever.
const MAX_WRITE_BACKOFFS: u32 = 16;

// ---------- The custom codec ----------------------------------------------

/// Why a `SemicolonCodec` stream is unrecoverably malformed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SemicolonMalformed {
    /// A frame contained an embedded NUL byte.
    EmbeddedNul,
}

/// A `;`-delimited codec with an explicit maximum frame length.
///
/// Frames are the bytes between `;` delimiters. A frame longer than
/// `max_frame` (no delimiter within the cap) yields
/// [`FrameDecision::Full`]; a frame containing a NUL yields
/// [`FrameDecision::Malformed`].
pub struct SemicolonCodec {
    buf: Vec<u8>,
    max_frame: usize,
    full: bool,
}

impl SemicolonCodec {
    /// Build a codec that rejects unframed prefixes longer than `max_frame`.
    pub fn new(max_frame: usize) -> Self {
        Self {
            buf: Vec::new(),
            max_frame,
            full: false,
        }
    }
}

impl SyncCodec for SemicolonCodec {
    type Frame = Vec<u8>;
    type Malformed = SemicolonMalformed;

    fn feed(&mut self, bytes: &[u8]) {
        if self.full {
            return;
        }
        // Bounded per frame: append until the current unframed suffix
        // crosses the cap. A delimiter that arrives before the cap still
        // yields its frame, even if later bytes in the same read overflow
        // the next frame.
        for byte in bytes {
            self.buf.push(*byte);
            let unframed = self.buf.iter().rev().take_while(|b| **b != b';').count();
            if unframed > self.max_frame {
                self.full = true;
                return;
            }
        }
    }

    fn next_frame(&mut self) -> FrameDecision<Self::Frame, Self::Malformed> {
        if let Some(idx) = self.buf.iter().position(|b| *b == b';') {
            let mut frame = self.buf.split_off(idx + 1);
            std::mem::swap(&mut frame, &mut self.buf);
            frame.pop(); // drop the trailing ';'
            if frame.contains(&0) {
                return FrameDecision::Malformed(SemicolonMalformed::EmbeddedNul);
            }
            return FrameDecision::Frame(frame);
        }
        if self.full {
            return FrameDecision::Full;
        }
        FrameDecision::NeedMore
    }
}

// ---------- A tiny service that uses the codec ------------------------------

#[derive(Debug, Default)]
pub struct CodecShard;

impl Shard for CodecShard {
    fn id(&self) -> ShardId {
        ShardId::new(122)
    }
}

#[derive(Debug)]
enum ServerMsg {
    Start,
    Bound(UnixBindReply),
    Accepted(UnixAcceptReply),
    Read(UnixReadReply),
    Wrote(UnixWriteReply),
    /// Back-off tick: the previous write made zero progress (peer inbound
    /// full). Retry after a short pause instead of hot-spinning.
    RetryWrite,
    Done,
}

struct CodecServer {
    path: PathBuf,
    listener: Option<UnixListenerId>,
    stream: Option<UnixStreamId>,
    codec: SemicolonCodec,
    write_pending: Vec<u8>,
    /// Consecutive zero-progress writes since the last byte landed.
    write_backoffs: u32,
    /// After the pending reply flushes, close the connection (a `quit`
    /// frame was seen). Flush first, then close, so the echoes land.
    closing: bool,
    seen: Arc<Mutex<Vec<Vec<u8>>>>,
    rejected: Arc<Mutex<bool>>,
}

impl Isolate for CodecServer {
    type Message = ServerMsg;
    type Reply = ();
    type Send = Outbound<Infallible>;
    type Spawn = Infallible;
    type SpawnObserved = Infallible;
    type Call = RuntimeCall<ServerMsg>;
    type Fact = Infallible;
    type Shard = CodecShard;

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
                    return self.close();
                }
                self.codec.feed(&bytes);
                let mut reply = Vec::new();
                let mut tear_down = false;
                loop {
                    match self.codec.next_frame() {
                        FrameDecision::NeedMore => break,
                        FrameDecision::Frame(frame) => {
                            self.seen.lock().unwrap().push(frame.clone());
                            if frame == b"quit" {
                                // Flush whatever we already framed, then close.
                                self.closing = true;
                                break;
                            }
                            reply.extend_from_slice(b"ok:");
                            reply.extend_from_slice(&frame);
                            reply.push(b';');
                        }
                        FrameDecision::Malformed(_) | FrameDecision::Full => {
                            // Bad stream: tear down now, discard partial reply.
                            *self.rejected.lock().unwrap() = true;
                            tear_down = true;
                            break;
                        }
                    }
                }
                if tear_down {
                    return self.close();
                }
                if reply.is_empty() {
                    if self.closing {
                        return self.close();
                    }
                    unix_read(self.stream.expect("stream"), 64).then(ServerMsg::Read)
                } else {
                    self.write_pending = reply.clone();
                    unix_write(self.stream.expect("stream"), reply).then(ServerMsg::Wrote)
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
                        return self.close();
                    }
                    return sleep(Duration::from_millis(1)).then(|_| ServerMsg::RetryWrite);
                }
                self.write_backoffs = 0;
                let drained = count.min(self.write_pending.len());
                self.write_pending.drain(..drained);
                if self.write_pending.is_empty() {
                    if self.closing {
                        return self.close();
                    }
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

impl CodecServer {
    fn close(&mut self) -> Effect<Self> {
        if let Some(stream) = self.stream.take() {
            unix_close_stream(stream).then(|_| ServerMsg::Done)
        } else {
            Effect::Stop
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

struct CodecClient {
    path: PathBuf,
    stream: Option<UnixStreamId>,
    outbound: Vec<u8>,
    write_pending: Vec<u8>,
    received: Arc<Mutex<Vec<u8>>>,
}

impl Isolate for CodecClient {
    type Message = ClientMsg;
    type Reply = ();
    type Send = Outbound<Infallible>;
    type Spawn = Infallible;
    type SpawnObserved = Infallible;
    type Call = RuntimeCall<ClientMsg>;
    type Fact = Infallible;
    type Shard = CodecShard;

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

/// One exchange's observations.
#[derive(Debug, Clone)]
pub struct CodecRun {
    /// Frames the server decoded, in order.
    pub server_saw: Vec<Vec<u8>>,
    /// Bytes the client received.
    pub client_received: Vec<u8>,
    /// True if the codec rejected the stream (`Malformed` or `Full`).
    pub rejected: bool,
}

/// Run one client/server exchange over the simulator Unix rails.
pub fn run_codec_service(path: PathBuf, payload: Vec<u8>, max_frame: usize) -> CodecRun {
    let mut sim = Simulator::new(CodecShard, SimulatorConfig::default());
    let seen = Arc::new(Mutex::new(Vec::new()));
    let rejected = Arc::new(Mutex::new(false));
    let received = Arc::new(Mutex::new(Vec::new()));

    let server = CodecServer {
        path: path.clone(),
        listener: None,
        stream: None,
        codec: SemicolonCodec::new(max_frame),
        write_pending: Vec::new(),
        write_backoffs: 0,
        closing: false,
        seen: Arc::clone(&seen),
        rejected: Arc::clone(&rejected),
    };
    let server_addr: Address<ServerMsg, ()> = sim.register(server);

    let client = CodecClient {
        path,
        stream: None,
        outbound: payload,
        write_pending: Vec::new(),
        received: Arc::clone(&received),
    };
    let client_addr: Address<ClientMsg, ()> = sim.register(client);

    sim.try_send(server_addr, ServerMsg::Start).unwrap();
    sim.try_send(client_addr, ClientMsg::Start).unwrap();
    sim.run_until_quiescent();

    CodecRun {
        server_saw: seen.lock().unwrap().clone(),
        client_received: received.lock().unwrap().clone(),
        rejected: *rejected.lock().unwrap(),
    }
}

/// What the smoke run observed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Report {
    /// Frames the server decoded on the happy path.
    pub frames: u64,
    /// Bytes echoed back to the client.
    pub echoed_bytes: u64,
    /// An oversize frame surfaced `Full`.
    pub oversize_rejected: bool,
    /// An embedded-NUL frame surfaced `Malformed`.
    pub malformed_rejected: bool,
}

/// Drive the happy path plus the two bounded/malformed rejections.
pub fn run() -> Report {
    let happy = run_codec_service(
        PathBuf::from("/tmp/tina_ext_codec.sock"),
        b"ping;status;quit;".to_vec(),
        64,
    );

    // No delimiter inside the cap → Full, connection torn down.
    let mut oversize = vec![b'X'; 64];
    oversize.push(b';');
    let big = run_codec_service(PathBuf::from("/tmp/tina_ext_codec_big.sock"), oversize, 8);

    // Embedded NUL inside a complete frame → Malformed.
    let bad = run_codec_service(
        PathBuf::from("/tmp/tina_ext_codec_nul.sock"),
        b"a\0b;".to_vec(),
        64,
    );

    Report {
        frames: happy.server_saw.len() as u64,
        echoed_bytes: happy.client_received.len() as u64,
        oversize_rejected: big.rejected,
        malformed_rejected: bad.rejected,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn custom_codec_drives_a_service_and_bounds_input() {
        let report = run();
        // ping, status, quit → 3 frames seen.
        assert_eq!(report.frames, 3, "server decoded three frames");
        // "ok:ping;ok:status;" echoed before quit closed the stream.
        assert!(report.echoed_bytes >= b"ok:ping;ok:status;".len() as u64);
        assert!(report.oversize_rejected, "oversize frame must surface Full");
        assert!(
            report.malformed_rejected,
            "embedded NUL must surface Malformed"
        );
    }

    #[test]
    fn codec_is_replayable() {
        // Same bytes in, same frames out — independent of any clock. The
        // `quit;` frame closes the connection so the run quiesces.
        let a = run_codec_service(
            PathBuf::from("/tmp/tina_ext_codec_r.sock"),
            b"x;y;quit;".to_vec(),
            64,
        );
        let b = run_codec_service(
            PathBuf::from("/tmp/tina_ext_codec_r.sock"),
            b"x;y;quit;".to_vec(),
            64,
        );
        assert_eq!(a.server_saw, b.server_saw);
        assert_eq!(a.client_received, b.client_received);
    }

    #[test]
    fn delimiter_before_later_overflow_keeps_finished_frame() {
        let mut codec = SemicolonCodec::new(4);
        codec.feed(b"ok;abcdef");
        assert!(matches!(
            codec.next_frame(),
            FrameDecision::Frame(ref f) if f == b"ok"
        ));
        assert!(matches!(codec.next_frame(), FrameDecision::Full));
    }
}
