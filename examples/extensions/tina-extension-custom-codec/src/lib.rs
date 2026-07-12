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

use tina::{Effect, Shard, ShardId};
use tina_codec::{DecodeStatus, FrameDecision, SyncCodec, decode_chunk};
use tina_runtime::{
    CallError, LoopStep, UnixAcceptReply, UnixBindReply, UnixConnectReply, UnixListenerCloseReply,
    UnixListenerId, UnixReadReply, UnixStreamCloseReply, UnixStreamId, UnixWriteAll,
    UnixWriteOwnedReply, unix_accept, unix_bind, unix_close_listener, unix_close_stream,
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
    StreamClosed(UnixStreamCloseReply),
    ListenerClosed(UnixListenerCloseReply),
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
    rejection: Arc<Mutex<Option<CodecRejection>>>,
    failures: Arc<Mutex<Vec<CodecIoFailure>>>,
}

#[tina_runtime::isolate(event = ServerMsg, shard = CodecShard)]
impl CodecServer {
    fn handle_event(
        &mut self,
        msg: ServerMsg,
        _ctx: &mut Context<'_, CodecShard, Self::Reply>,
    ) -> Effect<Self> {
        match msg {
            ServerMsg::Start => unix_bind(self.path.clone()).then_service_event(ServerMsg::Bound),
            ServerMsg::Bound(Ok((listener, _))) => {
                self.listener = Some(listener);
                unix_accept(listener).then_service_event(ServerMsg::Accepted)
            }
            ServerMsg::Bound(Err(error)) => self.fail(CodecIoStage::Bind, error),
            ServerMsg::Accepted(Ok(stream)) => {
                self.stream = Some(stream);
                unix_read(stream, 64).then_service_event(ServerMsg::Read)
            }
            ServerMsg::Accepted(Err(error)) => self.fail(CodecIoStage::Accept, error),
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
                if !self.closing {
                    let rejection = match status {
                        DecodeStatus::Malformed(reason) => Some(CodecRejection::Malformed(reason)),
                        DecodeStatus::Full => Some(CodecRejection::Full),
                        DecodeStatus::NeedMore => None,
                    };
                    if let Some(rejection) = rejection {
                        // Bad stream: tear down now, discard partial reply.
                        *self.rejection.lock().unwrap() = Some(rejection);
                        tear_down = true;
                    }
                }
                if tear_down {
                    return self.close();
                }
                if reply.is_empty() {
                    if self.closing {
                        return self.close();
                    }
                    unix_read(self.stream.expect("stream"), 64).then_service_event(ServerMsg::Read)
                } else {
                    let mut write_all = UnixWriteAll::new(self.stream.expect("stream"), reply);
                    let effect = write_all
                        .next_service_event(ServerMsg::Wrote)
                        .expect("reply buffer is non-empty");
                    self.write_all = Some(write_all);
                    effect
                }
            }
            ServerMsg::Read(Err(error)) => self.fail(CodecIoStage::Read, error),
            ServerMsg::Wrote(reply) => {
                let write_all = self.write_all.as_mut().expect("write helper armed");
                match write_all.advance_service_event(reply, ServerMsg::Wrote) {
                    LoopStep::Pending(effect) => effect,
                    LoopStep::Done(_) => {
                        self.write_all = None;
                        if self.closing {
                            self.close()
                        } else {
                            unix_read(self.stream.expect("stream"), 64)
                                .then_service_event(ServerMsg::Read)
                        }
                    }
                    LoopStep::Failed(error) => self.fail(CodecIoStage::Write, error),
                }
            }
            ServerMsg::StreamClosed(result) => {
                if let Err(error) = result {
                    self.record_failure(CodecIoStage::Close, error);
                }
                self.close_listener()
            }
            ServerMsg::ListenerClosed(Ok(())) => Effect::Stop,
            ServerMsg::ListenerClosed(Err(error)) => self.fail(CodecIoStage::Close, error),
        }
    }

    fn close(&mut self) -> Effect<Self> {
        if let Some(stream) = self.stream.take() {
            unix_close_stream(stream).then_service_event(ServerMsg::StreamClosed)
        } else {
            self.close_listener()
        }
    }

    fn close_listener(&mut self) -> Effect<Self> {
        if let Some(listener) = self.listener.take() {
            unix_close_listener(listener).then_service_event(ServerMsg::ListenerClosed)
        } else {
            Effect::Stop
        }
    }

    fn fail(&mut self, stage: CodecIoStage, error: CallError) -> Effect<Self> {
        self.record_failure(stage, error);
        match stage {
            CodecIoStage::Accept => self.close_listener(),
            CodecIoStage::Read | CodecIoStage::Write => self.close(),
            CodecIoStage::Bind | CodecIoStage::Connect | CodecIoStage::Close => Effect::Stop,
        }
    }

    fn record_failure(&mut self, stage: CodecIoStage, error: CallError) {
        self.failures.lock().unwrap().push(CodecIoFailure {
            endpoint: CodecEndpoint::Server,
            stage,
            error,
        });
    }
}

#[derive(Debug)]
enum ClientMsg {
    Start,
    Connected(UnixConnectReply),
    Wrote(UnixWriteOwnedReply),
    Read(UnixReadReply),
    Closed(UnixStreamCloseReply),
}

struct CodecClient {
    path: PathBuf,
    stream: Option<UnixStreamId>,
    outbound: Vec<u8>,
    write_all: Option<UnixWriteAll>,
    received: Arc<Mutex<Vec<u8>>>,
    failures: Arc<Mutex<Vec<CodecIoFailure>>>,
}

#[tina_runtime::isolate(event = ClientMsg, shard = CodecShard)]
impl CodecClient {
    fn handle_event(
        &mut self,
        msg: ClientMsg,
        _ctx: &mut Context<'_, CodecShard, Self::Reply>,
    ) -> Effect<Self> {
        match msg {
            ClientMsg::Start => {
                unix_connect(self.path.clone()).then_service_event(ClientMsg::Connected)
            }
            ClientMsg::Connected(Ok(stream)) => {
                self.stream = Some(stream);
                let bytes = std::mem::take(&mut self.outbound);
                if bytes.is_empty() {
                    self.stream = None;
                    return unix_close_stream(stream).then_service_event(ClientMsg::Closed);
                }
                let mut write_all = UnixWriteAll::new(stream, bytes);
                let effect = write_all
                    .next_service_event(ClientMsg::Wrote)
                    .expect("non-empty client payload has a write step");
                self.write_all = Some(write_all);
                effect
            }
            ClientMsg::Connected(Err(error)) => self.fail(CodecIoStage::Connect, error),
            ClientMsg::Wrote(reply) => {
                let write_all = self.write_all.as_mut().expect("write helper armed");
                match write_all.advance_service_event(reply, ClientMsg::Wrote) {
                    LoopStep::Pending(effect) => effect,
                    LoopStep::Done(_) => {
                        self.write_all = None;
                        unix_read(self.stream.expect("stream"), 64)
                            .then_service_event(ClientMsg::Read)
                    }
                    LoopStep::Failed(error) => self.fail(CodecIoStage::Write, error),
                }
            }
            ClientMsg::Read(Ok(bytes)) => {
                if bytes.is_empty() {
                    if let Some(stream) = self.stream.take() {
                        return unix_close_stream(stream).then_service_event(ClientMsg::Closed);
                    }
                    return Effect::Stop;
                }
                self.received.lock().unwrap().extend_from_slice(&bytes);
                unix_read(self.stream.expect("stream"), 64).then_service_event(ClientMsg::Read)
            }
            ClientMsg::Read(Err(error)) => self.fail(CodecIoStage::Read, error),
            ClientMsg::Closed(Ok(())) => Effect::Stop,
            ClientMsg::Closed(Err(error)) => self.fail(CodecIoStage::Close, error),
        }
    }

    fn fail(&mut self, stage: CodecIoStage, error: CallError) -> Effect<Self> {
        self.failures.lock().unwrap().push(CodecIoFailure {
            endpoint: CodecEndpoint::Client,
            stage,
            error,
        });
        if matches!(stage, CodecIoStage::Read | CodecIoStage::Write) {
            if let Some(stream) = self.stream.take() {
                return unix_close_stream(stream).then_service_event(ClientMsg::Closed);
            }
        }
        Effect::Stop
    }
}

/// Endpoint that observed a terminal Unix rail failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CodecEndpoint {
    Server,
    Client,
}

/// Exact stage that produced a terminal Unix rail failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CodecIoStage {
    Bind,
    Accept,
    Connect,
    Read,
    Write,
    Close,
}

/// Typed terminal Unix failure retained by the example.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CodecIoFailure {
    pub endpoint: CodecEndpoint,
    pub stage: CodecIoStage,
    pub error: CallError,
}

/// Typed codec-policy rejection, distinct from Unix transport failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CodecRejection {
    /// The current frame exceeded the configured bound.
    Full,
    /// The codec rejected a complete frame with its typed reason.
    Malformed(SemicolonMalformed),
}

/// One exchange's observations.
#[derive(Debug, Clone)]
pub struct CodecRun {
    /// Frames the server decoded, in order.
    pub server_saw: Vec<Vec<u8>>,
    /// Bytes the client received.
    pub client_received: Vec<u8>,
    /// Typed codec-policy rejection, if any.
    pub rejection: Option<CodecRejection>,
    /// Exhaustive terminal Unix rail failures, if any.
    pub io_failures: Vec<CodecIoFailure>,
}

/// Run one client/server exchange over the simulator Unix rails.
pub fn run_codec_service(path: PathBuf, payload: Vec<u8>, max_frame: usize) -> CodecRun {
    let mut sim = Simulator::new(CodecShard, SimulatorConfig::default());
    let seen = Arc::new(Mutex::new(Vec::new()));
    let rejection = Arc::new(Mutex::new(None));
    let received = Arc::new(Mutex::new(Vec::new()));
    let failures = Arc::new(Mutex::new(Vec::new()));

    let server = CodecServer {
        path: path.clone(),
        listener: None,
        stream: None,
        codec: SemicolonCodec::new(max_frame),
        write_all: None,
        closing: false,
        seen: Arc::clone(&seen),
        rejection: Arc::clone(&rejection),
        failures: Arc::clone(&failures),
    };
    let server_addr = sim.register_event_service(server, 8);

    let client = CodecClient {
        path,
        stream: None,
        outbound: payload,
        write_all: None,
        received: Arc::clone(&received),
        failures: Arc::clone(&failures),
    };
    let client_addr = sim.register_event_service(client, 8);

    sim.try_send_event(server_addr, ServerMsg::Start).unwrap();
    sim.try_send_event(client_addr, ClientMsg::Start).unwrap();
    sim.run_until_quiescent();

    CodecRun {
        server_saw: seen.lock().unwrap().clone(),
        client_received: received.lock().unwrap().clone(),
        rejection: *rejection.lock().unwrap(),
        io_failures: failures.lock().unwrap().clone(),
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
    /// Typed Unix rail failures across the three exchanges.
    pub io_failures: Vec<CodecIoFailure>,
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
        oversize_rejected: big.rejection == Some(CodecRejection::Full),
        malformed_rejected: bad.rejection
            == Some(CodecRejection::Malformed(SemicolonMalformed::EmbeddedNul)),
        io_failures: happy
            .io_failures
            .into_iter()
            .chain(big.io_failures)
            .chain(bad.io_failures)
            .collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn server(path: PathBuf, failures: Arc<Mutex<Vec<CodecIoFailure>>>) -> CodecServer {
        CodecServer {
            path,
            listener: None,
            stream: None,
            codec: SemicolonCodec::new(64),
            write_all: None,
            closing: false,
            seen: Arc::new(Mutex::new(Vec::new())),
            rejection: Arc::new(Mutex::new(None)),
            failures,
        }
    }

    fn client(path: PathBuf, failures: Arc<Mutex<Vec<CodecIoFailure>>>) -> CodecClient {
        CodecClient {
            path,
            stream: None,
            outbound: b"quit;".to_vec(),
            write_all: None,
            received: Arc::new(Mutex::new(Vec::new())),
            failures,
        }
    }

    fn run_server_event(event: ServerMsg) -> Vec<CodecIoFailure> {
        let failures = Arc::new(Mutex::new(Vec::new()));
        let mut sim = Simulator::new(CodecShard, SimulatorConfig::default());
        let actor = sim.register_event_service(
            server(
                PathBuf::from("/tmp/tina_ext_codec_probe.sock"),
                Arc::clone(&failures),
            ),
            8,
        );
        sim.try_send_event(actor, event).unwrap();
        sim.run_until_quiescent();
        failures.lock().unwrap().clone()
    }

    fn run_client_event(event: ClientMsg) -> Vec<CodecIoFailure> {
        let failures = Arc::new(Mutex::new(Vec::new()));
        let mut sim = Simulator::new(CodecShard, SimulatorConfig::default());
        let actor = sim.register_event_service(
            client(
                PathBuf::from("/tmp/tina_ext_codec_probe.sock"),
                Arc::clone(&failures),
            ),
            8,
        );
        sim.try_send_event(actor, event).unwrap();
        sim.run_until_quiescent();
        failures.lock().unwrap().clone()
    }

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
        assert!(
            report.io_failures.is_empty(),
            "normal and policy-close paths have no Unix rail failures: {:?}",
            report.io_failures
        );
    }

    #[test]
    fn codec_policy_full_and_malformed_remain_distinct() {
        let full = run_codec_service(
            PathBuf::from("/tmp/tina_ext_codec_typed_full.sock"),
            b"abcdef;".to_vec(),
            2,
        );
        assert_eq!(full.rejection, Some(CodecRejection::Full));
        assert!(full.io_failures.is_empty());

        let malformed = run_codec_service(
            PathBuf::from("/tmp/tina_ext_codec_typed_malformed.sock"),
            b"a\0b;".to_vec(),
            8,
        );
        assert_eq!(
            malformed.rejection,
            Some(CodecRejection::Malformed(SemicolonMalformed::EmbeddedNul))
        );
        assert!(malformed.io_failures.is_empty());
    }

    #[test]
    fn server_failure_ledger_preserves_bind_accept_read_write_and_close() {
        let path = PathBuf::from("/tmp/tina_ext_codec_duplicate_bind.sock");
        let failures = Arc::new(Mutex::new(Vec::new()));
        let mut sim = Simulator::new(CodecShard, SimulatorConfig::default());
        let first = sim.register_event_service(server(path.clone(), Arc::clone(&failures)), 8);
        let second = sim.register_event_service(server(path.clone(), Arc::clone(&failures)), 8);
        let peer = sim.register_event_service(client(path, Arc::clone(&failures)), 8);
        sim.try_send_event(first, ServerMsg::Start).unwrap();
        sim.try_send_event(second, ServerMsg::Start).unwrap();
        sim.try_send_event(peer, ClientMsg::Start).unwrap();
        sim.run_until_quiescent();
        assert_eq!(
            *failures.lock().unwrap(),
            [CodecIoFailure {
                endpoint: CodecEndpoint::Server,
                stage: CodecIoStage::Bind,
                error: CallError::Io,
            }]
        );

        assert_eq!(
            run_server_event(ServerMsg::Bound(Ok((
                UnixListenerId::new(9999),
                PathBuf::from("/tmp/tina_ext_codec_invalid_listener.sock"),
            )))),
            [
                CodecIoFailure {
                    endpoint: CodecEndpoint::Server,
                    stage: CodecIoStage::Accept,
                    error: CallError::InvalidResource,
                },
                CodecIoFailure {
                    endpoint: CodecEndpoint::Server,
                    stage: CodecIoStage::Close,
                    error: CallError::InvalidResource,
                },
            ]
        );

        assert_eq!(
            run_server_event(ServerMsg::Accepted(Ok(UnixStreamId::new(9999)))),
            [
                CodecIoFailure {
                    endpoint: CodecEndpoint::Server,
                    stage: CodecIoStage::Read,
                    error: CallError::InvalidResource,
                },
                CodecIoFailure {
                    endpoint: CodecEndpoint::Server,
                    stage: CodecIoStage::Close,
                    error: CallError::InvalidResource,
                },
            ]
        );

        let failures = Arc::new(Mutex::new(Vec::new()));
        let mut actor_state = server(
            PathBuf::from("/tmp/tina_ext_codec_invalid_write.sock"),
            Arc::clone(&failures),
        );
        actor_state.stream = Some(UnixStreamId::new(9999));
        let mut sim = Simulator::new(CodecShard, SimulatorConfig::default());
        let actor = sim.register_event_service(actor_state, 8);
        sim.try_send_event(actor, ServerMsg::Read(Ok(b"x;".to_vec())))
            .unwrap();
        sim.run_until_quiescent();
        assert_eq!(
            *failures.lock().unwrap(),
            [
                CodecIoFailure {
                    endpoint: CodecEndpoint::Server,
                    stage: CodecIoStage::Write,
                    error: CallError::InvalidResource,
                },
                CodecIoFailure {
                    endpoint: CodecEndpoint::Server,
                    stage: CodecIoStage::Close,
                    error: CallError::InvalidResource,
                },
            ]
        );
    }

    #[test]
    fn client_failure_ledger_preserves_connect_read_write_and_close() {
        let failures = Arc::new(Mutex::new(Vec::new()));
        let mut sim = Simulator::new(CodecShard, SimulatorConfig::default());
        let actor = sim.register_event_service(
            client(
                PathBuf::from("/tmp/tina_ext_codec_missing_listener.sock"),
                Arc::clone(&failures),
            ),
            8,
        );
        sim.try_send_event(actor, ClientMsg::Start).unwrap();
        sim.run_until_quiescent();
        assert_eq!(
            *failures.lock().unwrap(),
            [CodecIoFailure {
                endpoint: CodecEndpoint::Client,
                stage: CodecIoStage::Connect,
                error: CallError::NotFound,
            }]
        );

        let failures = Arc::new(Mutex::new(Vec::new()));
        let mut actor_state = client(
            PathBuf::from("/tmp/tina_ext_codec_invalid_client_read.sock"),
            Arc::clone(&failures),
        );
        actor_state.stream = Some(UnixStreamId::new(9999));
        let mut sim = Simulator::new(CodecShard, SimulatorConfig::default());
        let actor = sim.register_event_service(actor_state, 8);
        sim.try_send_event(actor, ClientMsg::Read(Err(CallError::Io)))
            .unwrap();
        sim.run_until_quiescent();
        assert_eq!(
            *failures.lock().unwrap(),
            [
                CodecIoFailure {
                    endpoint: CodecEndpoint::Client,
                    stage: CodecIoStage::Read,
                    error: CallError::Io,
                },
                CodecIoFailure {
                    endpoint: CodecEndpoint::Client,
                    stage: CodecIoStage::Close,
                    error: CallError::InvalidResource,
                },
            ]
        );

        assert_eq!(
            run_client_event(ClientMsg::Connected(Ok(UnixStreamId::new(9999)))),
            [
                CodecIoFailure {
                    endpoint: CodecEndpoint::Client,
                    stage: CodecIoStage::Write,
                    error: CallError::InvalidResource,
                },
                CodecIoFailure {
                    endpoint: CodecEndpoint::Client,
                    stage: CodecIoStage::Close,
                    error: CallError::InvalidResource,
                },
            ]
        );

        assert_eq!(
            run_client_event(ClientMsg::Closed(Err(CallError::Io))),
            [CodecIoFailure {
                endpoint: CodecEndpoint::Client,
                stage: CodecIoStage::Close,
                error: CallError::Io,
            }]
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
        assert_eq!(a.rejection, b.rejection);
        assert_eq!(a.io_failures, b.io_failures);
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
        assert!(result.io_failures.is_empty());
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
            result.rejection.is_none(),
            "quit is an intentional close, not bad input"
        );
    }
}
