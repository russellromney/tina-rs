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

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use tina::{Address, Effect, Shard, ShardId};
use tina_codec::{DecodeStatus, FrameDecision, SyncCodec, decode_chunk};
use tina_runtime::{
    LoopStep, UnixAcceptReply, UnixBindReply, UnixConnectReply, UnixListenerId, UnixReadReply,
    UnixStreamId, UnixWriteAll, UnixWriteOwnedReply, unix_accept, unix_bind, unix_close_stream,
    unix_connect, unix_read,
};
use tina_sim::{Simulator, SimulatorConfig};

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

    fn feed(&mut self, bytes: &[u8]) -> usize {
        if self.full {
            return 0;
        }
        if self.buf.last() == Some(&b';') {
            return 0;
        }
        let room = self
            .max_frame
            .saturating_add(1)
            .saturating_sub(self.buf.len());
        let through_delimiter = bytes
            .iter()
            .position(|byte| *byte == b';')
            .map_or(bytes.len(), |index| index + 1);
        let consumed = room.min(through_delimiter);
        self.buf.extend_from_slice(&bytes[..consumed]);
        if consumed == room && self.buf.last() != Some(&b';') {
            self.full = true;
        }
        consumed
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
    Wrote(UnixWriteOwnedReply),
    Done,
}

struct CodecServer {
    path: PathBuf,
    listener: Option<UnixListenerId>,
    stream: Option<UnixStreamId>,
    codec: SemicolonCodec,
    write_all: Option<UnixWriteAll>,
    /// After the pending reply flushes, close the connection (a `quit`
    /// frame was seen). Flush first, then close, so the echoes land.
    closing: bool,
    seen: Arc<Mutex<Vec<Vec<u8>>>>,
    rejected: Arc<Mutex<bool>>,
}

#[tina_runtime::isolate(message = ServerMsg, shard = CodecShard)]
impl CodecServer {
    fn handle(
        &mut self,
        msg: ServerMsg,
        _ctx: &mut Context<'_, CodecShard, Self::Reply>,
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
                let mut reply = Vec::new();
                let mut tear_down = false;
                let seen = &self.seen;
                let closing = &mut self.closing;
                let status = decode_chunk(&mut self.codec, &bytes, |frame| {
                    if !*closing {
                        seen.lock().unwrap().push(frame.clone());
                        if frame == b"quit" {
                            // Flush whatever we already framed, then close.
                            *closing = true;
                        } else {
                            reply.extend_from_slice(b"ok:");
                            reply.extend_from_slice(&frame);
                            reply.push(b';');
                        }
                    }
                });
                if !self.closing
                    && matches!(status, DecodeStatus::Malformed(_) | DecodeStatus::Full)
                {
                    // Bad stream: tear down now, discard partial reply.
                    *self.rejected.lock().unwrap() = true;
                    tear_down = true;
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
                    let mut write_all = UnixWriteAll::new(self.stream.expect("stream"), reply);
                    let effect = write_all
                        .next_effect(ServerMsg::Wrote)
                        .expect("reply buffer is non-empty");
                    self.write_all = Some(write_all);
                    effect
                }
            }
            ServerMsg::Read(Err(_)) => Effect::Stop,
            ServerMsg::Wrote(reply) => {
                let write_all = self.write_all.as_mut().expect("write helper armed");
                match write_all.advance::<Self, _, _>(reply, ServerMsg::Wrote) {
                    LoopStep::Pending(effect) => effect,
                    LoopStep::Done(_) => {
                        self.write_all = None;
                        if self.closing {
                            self.close()
                        } else {
                            unix_read(self.stream.expect("stream"), 64).then(ServerMsg::Read)
                        }
                    }
                    LoopStep::Failed(_) => self.close(),
                }
            }
            ServerMsg::Done => Effect::Stop,
        }
    }

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
    Wrote(UnixWriteOwnedReply),
    Read(UnixReadReply),
    Done,
}

struct CodecClient {
    path: PathBuf,
    stream: Option<UnixStreamId>,
    outbound: Vec<u8>,
    write_all: Option<UnixWriteAll>,
    received: Arc<Mutex<Vec<u8>>>,
}

#[tina_runtime::isolate(message = ClientMsg, shard = CodecShard)]
impl CodecClient {
    fn handle(
        &mut self,
        msg: ClientMsg,
        _ctx: &mut Context<'_, CodecShard, Self::Reply>,
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
                        unix_read(self.stream.expect("stream"), 64).then(ClientMsg::Read)
                    }
                    LoopStep::Failed(_) => Effect::Stop,
                }
            }
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
        write_all: None,
        closing: false,
        seen: Arc::clone(&seen),
        rejected: Arc::clone(&rejected),
    };
    let server_addr: Address<ServerMsg, ()> = sim.register(server);

    let client = CodecClient {
        path,
        stream: None,
        outbound: payload,
        write_all: None,
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
    fn empty_payload_does_not_arm_an_empty_write() {
        let result = run_codec_service(
            PathBuf::from("/tmp/tina_ext_codec_empty.sock"),
            Vec::new(),
            8,
        );
        assert!(result.server_saw.is_empty());
        assert!(result.client_received.is_empty());
    }

    #[test]
    fn delimiter_before_later_overflow_keeps_finished_frame() {
        let mut codec = SemicolonCodec::new(4);
        let mut frames = Vec::new();
        let status = decode_chunk(&mut codec, b"ok;abcdef", |frame| frames.push(frame));
        assert_eq!(frames, [b"ok".to_vec()]);
        assert_eq!(status, DecodeStatus::Full);
    }

    #[test]
    fn quit_ignores_an_oversize_suffix_in_the_same_transport_chunk() {
        let result = run_codec_service(
            PathBuf::from("/tmp/tina_ext_codec_quit_suffix.sock"),
            b"quit;abcdef".to_vec(),
            4,
        );
        assert_eq!(result.server_saw, [b"quit".to_vec()]);
        assert!(
            !result.rejected,
            "quit is an intentional close, not bad input"
        );
    }
}
