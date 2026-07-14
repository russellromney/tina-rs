//! Length-prefixed mini-keyspace protocol over simulator Unix-domain sockets.

use std::path::PathBuf;
use std::time::Duration;

use tina::{Effect, Shard, ShardId, stop_with};
use tina_codec::{DecodeStatus, FrameDecision, LengthDelimitedFramer, LengthPrefix, decode_chunk};
use tina_runtime::{
    CallError, FramedWriteError, LoopStep, UnixAcceptReply, UnixBindReply, UnixConnectReply,
    UnixFramedWriter, UnixListenerId, UnixReadReply, UnixStreamId, UnixWriteAll,
    UnixWriteOwnedReply, unix_accept, unix_bind, unix_close_listener, unix_close_stream,
    unix_connect, unix_read,
};
use tina_sim::{Simulator, SimulatorConfig};

use crate::{RunError, SpecimenReport, map_start, wait_actor};

#[derive(Debug, Default)]
pub struct KeyspaceShard;

impl Shard for KeyspaceShard {
    fn id(&self) -> ShardId {
        ShardId::new(103)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyspaceEndpoint {
    Server,
    Client,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyspaceStage {
    Bind,
    Accept,
    Connect,
    Read,
    Encode,
    Write,
    CloseStream,
    CloseListener,
    Protocol,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KeyspaceIssueKind {
    Call(CallError),
    Frame(FramedWriteError),
    EofBeforeResponses { expected: usize, received: usize },
    MalformedResponse,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyspaceIssue {
    pub endpoint: KeyspaceEndpoint,
    pub stage: KeyspaceStage,
    pub error: KeyspaceIssueKind,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyspaceFailure {
    pub issues: Vec<KeyspaceIssue>,
}

impl std::fmt::Display for KeyspaceFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{:?}", self.issues)
    }
}

impl std::error::Error for KeyspaceFailure {}

pub type KeyspaceRunError = RunError<KeyspaceFailure>;

#[derive(Debug)]
enum ServerMsg {
    Start,
    Bound(UnixBindReply),
    Accepted(UnixAcceptReply),
    ListenerClosed(Result<(), CallError>),
    Read(UnixReadReply),
    Wrote(UnixWriteOwnedReply),
    StreamClosed(Result<(), CallError>),
}

#[derive(Debug)]
struct ServerReport {
    frames: Vec<Vec<u8>>,
    saw_full_or_malformed: bool,
}

struct KeyspaceServer {
    path: PathBuf,
    listener: Option<UnixListenerId>,
    stream: Option<UnixStreamId>,
    framer: LengthDelimitedFramer,
    writer: Option<UnixFramedWriter>,
    max_response_body_len: usize,
    max_encoded_len: usize,
    frames: Vec<Vec<u8>>,
    saw_full_or_malformed: bool,
    listener_closing_before_read: bool,
    issues: Vec<KeyspaceIssue>,
}

impl KeyspaceServer {
    fn issue(&mut self, stage: KeyspaceStage, error: KeyspaceIssueKind) {
        self.issues.push(KeyspaceIssue {
            endpoint: KeyspaceEndpoint::Server,
            stage,
            error,
        });
    }

    fn begin_finish(&mut self) -> Effect<Self> {
        self.writer = None;
        self.cleanup()
    }

    fn cleanup(&mut self) -> Effect<Self> {
        if let Some(stream) = self.stream.take() {
            return unix_close_stream(stream).then(ServerMsg::StreamClosed);
        }
        if let Some(listener) = self.listener.take() {
            self.listener_closing_before_read = false;
            return unix_close_listener(listener).then(ServerMsg::ListenerClosed);
        }
        let report = ServerReport {
            frames: std::mem::take(&mut self.frames),
            saw_full_or_malformed: self.saw_full_or_malformed,
        };
        if self.issues.is_empty() {
            stop_with(Ok::<_, KeyspaceFailure>(report))
        } else {
            stop_with(Err::<ServerReport, _>(KeyspaceFailure {
                issues: std::mem::take(&mut self.issues),
            }))
        }
    }

    fn next_read(&self) -> Effect<Self> {
        unix_read(self.stream.expect("stream open while reading"), 64).then(ServerMsg::Read)
    }
}

#[tina_runtime::isolate(message = ServerMsg, shard = KeyspaceShard)]
impl KeyspaceServer {
    fn handle(
        &mut self,
        msg: ServerMsg,
        _ctx: &mut Context<'_, KeyspaceShard, Self::Reply>,
    ) -> Effect<Self> {
        match msg {
            ServerMsg::Start => unix_bind(self.path.clone()).then(ServerMsg::Bound),
            ServerMsg::Bound(Ok((listener, _))) => {
                self.listener = Some(listener);
                unix_accept(listener).then(ServerMsg::Accepted)
            }
            ServerMsg::Bound(Err(error)) => {
                self.issue(KeyspaceStage::Bind, KeyspaceIssueKind::Call(error));
                self.begin_finish()
            }
            ServerMsg::Accepted(Ok(stream)) => {
                self.stream = Some(stream);
                self.listener_closing_before_read = true;
                let listener = self.listener.take().expect("listener owned after accept");
                unix_close_listener(listener).then(ServerMsg::ListenerClosed)
            }
            ServerMsg::Accepted(Err(error)) => {
                self.issue(KeyspaceStage::Accept, KeyspaceIssueKind::Call(error));
                self.begin_finish()
            }
            ServerMsg::ListenerClosed(result) if self.listener_closing_before_read => {
                self.listener_closing_before_read = false;
                match result {
                    Ok(()) => self.next_read(),
                    Err(error) => {
                        self.issue(KeyspaceStage::CloseListener, KeyspaceIssueKind::Call(error));
                        self.begin_finish()
                    }
                }
            }
            ServerMsg::ListenerClosed(result) => {
                if let Err(error) = result {
                    self.issue(KeyspaceStage::CloseListener, KeyspaceIssueKind::Call(error));
                }
                self.cleanup()
            }
            ServerMsg::Read(Ok(bytes)) if bytes.is_empty() => {
                if !matches!(self.framer.finish(), FrameDecision::NeedMore) {
                    self.saw_full_or_malformed = true;
                }
                self.begin_finish()
            }
            ServerMsg::Read(Ok(bytes)) => {
                let stream = self.stream.expect("stream open while decoding");
                let mut writer = UnixFramedWriter::length_delimited(
                    stream,
                    LengthPrefix::U16,
                    self.max_response_body_len,
                    self.max_encoded_len,
                );
                let mut frame_error = None;
                let status = decode_chunk(&mut self.framer, &bytes, |frame| {
                    if frame_error.is_some() {
                        return;
                    }
                    let mut response = b"ack:".to_vec();
                    response.extend_from_slice(&frame);
                    if let Err(error) = writer.push_frame(response) {
                        frame_error = Some(error);
                        return;
                    }
                    self.frames.push(frame);
                });
                if let Some(error) = frame_error {
                    self.issue(KeyspaceStage::Encode, KeyspaceIssueKind::Frame(error));
                    return self.begin_finish();
                }
                if matches!(status, DecodeStatus::Malformed(_) | DecodeStatus::Full) {
                    self.saw_full_or_malformed = true;
                    return self.begin_finish();
                }
                if let Some(effect) = writer.next_effect(ServerMsg::Wrote) {
                    self.writer = Some(writer);
                    effect
                } else {
                    self.next_read()
                }
            }
            ServerMsg::Read(Err(error)) => {
                self.issue(KeyspaceStage::Read, KeyspaceIssueKind::Call(error));
                self.begin_finish()
            }
            ServerMsg::Wrote(reply) => {
                let writer = self.writer.as_mut().expect("framed writer armed");
                match writer.advance::<Self, _, _>(reply, ServerMsg::Wrote) {
                    LoopStep::Pending(effect) => effect,
                    LoopStep::Done(_) => {
                        self.writer = None;
                        self.next_read()
                    }
                    LoopStep::Failed(error) => {
                        self.issue(KeyspaceStage::Write, KeyspaceIssueKind::Call(error));
                        self.begin_finish()
                    }
                }
            }
            ServerMsg::StreamClosed(result) => {
                if let Err(error) = result {
                    self.issue(KeyspaceStage::CloseStream, KeyspaceIssueKind::Call(error));
                }
                self.cleanup()
            }
        }
    }
}

#[derive(Debug)]
enum KeyspacePayload {
    Frames(Vec<Vec<u8>>),
    RawMalformed(Vec<u8>),
}

#[derive(Debug)]
enum ClientMsg {
    Start,
    Connected(UnixConnectReply),
    Wrote(UnixWriteOwnedReply),
    Read(UnixReadReply),
    StreamClosed(Result<(), CallError>),
}

#[derive(Debug)]
struct ClientReport {
    bytes: usize,
    frames: Vec<Vec<u8>>,
    raw_write_error: Option<CallError>,
}

struct KeyspaceClient {
    path: PathBuf,
    stream: Option<UnixStreamId>,
    payload: Option<KeyspacePayload>,
    framed_writer: Option<UnixFramedWriter>,
    raw_writer: Option<UnixWriteAll>,
    response_framer: LengthDelimitedFramer,
    max_body_len: usize,
    max_encoded_len: usize,
    expected_responses: usize,
    wait_for_eof: bool,
    received_bytes: usize,
    response_frames: Vec<Vec<u8>>,
    raw_write_error: Option<CallError>,
    issues: Vec<KeyspaceIssue>,
}

impl KeyspaceClient {
    fn issue(&mut self, stage: KeyspaceStage, error: KeyspaceIssueKind) {
        self.issues.push(KeyspaceIssue {
            endpoint: KeyspaceEndpoint::Client,
            stage,
            error,
        });
    }

    fn begin_finish(&mut self) -> Effect<Self> {
        self.framed_writer = None;
        self.raw_writer = None;
        if let Some(stream) = self.stream.take() {
            unix_close_stream(stream).then(ClientMsg::StreamClosed)
        } else {
            self.publish()
        }
    }

    fn publish(&mut self) -> Effect<Self> {
        let report = ClientReport {
            bytes: self.received_bytes,
            frames: std::mem::take(&mut self.response_frames),
            raw_write_error: self.raw_write_error,
        };
        if self.issues.is_empty() {
            stop_with(Ok::<_, KeyspaceFailure>(report))
        } else {
            stop_with(Err::<ClientReport, _>(KeyspaceFailure {
                issues: std::mem::take(&mut self.issues),
            }))
        }
    }

    fn next_read(&self) -> Effect<Self> {
        unix_read(self.stream.expect("stream open while reading"), 64).then(ClientMsg::Read)
    }
}

#[tina_runtime::isolate(message = ClientMsg, shard = KeyspaceShard)]
impl KeyspaceClient {
    fn handle(
        &mut self,
        msg: ClientMsg,
        _ctx: &mut Context<'_, KeyspaceShard, Self::Reply>,
    ) -> Effect<Self> {
        match msg {
            ClientMsg::Start => unix_connect(self.path.clone()).then(ClientMsg::Connected),
            ClientMsg::Connected(Ok(stream)) => {
                self.stream = Some(stream);
                match self.payload.take().expect("client payload available") {
                    KeyspacePayload::Frames(frames) => {
                        if frames.is_empty() {
                            return self.begin_finish();
                        }
                        let mut writer = UnixFramedWriter::length_delimited(
                            stream,
                            LengthPrefix::U16,
                            self.max_body_len,
                            self.max_encoded_len,
                        );
                        for frame in frames {
                            if let Err(error) = writer.push_frame(frame) {
                                self.issue(KeyspaceStage::Encode, KeyspaceIssueKind::Frame(error));
                                return self.begin_finish();
                            }
                        }
                        let effect = writer
                            .next_effect(ClientMsg::Wrote)
                            .expect("non-empty frame batch has a write effect");
                        self.framed_writer = Some(writer);
                        effect
                    }
                    KeyspacePayload::RawMalformed(bytes) => {
                        self.wait_for_eof = true;
                        let mut writer = UnixWriteAll::new(stream, bytes);
                        let effect = writer
                            .next_effect(ClientMsg::Wrote)
                            .expect("malformed payload is non-empty");
                        self.raw_writer = Some(writer);
                        effect
                    }
                }
            }
            ClientMsg::Connected(Err(error)) => {
                self.issue(KeyspaceStage::Connect, KeyspaceIssueKind::Call(error));
                self.begin_finish()
            }
            ClientMsg::Wrote(reply) => {
                let step = if let Some(writer) = self.framed_writer.as_mut() {
                    writer.advance::<Self, _, _>(reply, ClientMsg::Wrote)
                } else {
                    self.raw_writer
                        .as_mut()
                        .expect("raw writer armed")
                        .advance::<Self, _, _>(reply, ClientMsg::Wrote)
                };
                match step {
                    LoopStep::Pending(effect) => effect,
                    LoopStep::Done(_) => {
                        self.framed_writer = None;
                        self.raw_writer = None;
                        self.next_read()
                    }
                    LoopStep::Failed(error) => {
                        if self.wait_for_eof {
                            self.raw_write_error = Some(error);
                        } else {
                            self.issue(KeyspaceStage::Write, KeyspaceIssueKind::Call(error));
                        }
                        self.begin_finish()
                    }
                }
            }
            ClientMsg::Read(Ok(bytes)) if bytes.is_empty() => {
                match self.response_framer.finish() {
                    FrameDecision::Frame(frame) => self.response_frames.push(frame),
                    FrameDecision::Malformed(_) | FrameDecision::Full => self.issue(
                        KeyspaceStage::Protocol,
                        KeyspaceIssueKind::MalformedResponse,
                    ),
                    FrameDecision::NeedMore => {}
                }
                if !self.wait_for_eof && self.response_frames.len() < self.expected_responses {
                    self.issue(
                        KeyspaceStage::Protocol,
                        KeyspaceIssueKind::EofBeforeResponses {
                            expected: self.expected_responses,
                            received: self.response_frames.len(),
                        },
                    );
                }
                self.begin_finish()
            }
            ClientMsg::Read(Ok(bytes)) => {
                self.received_bytes += bytes.len();
                let status = decode_chunk(&mut self.response_framer, &bytes, |frame| {
                    self.response_frames.push(frame)
                });
                if matches!(status, DecodeStatus::Malformed(_) | DecodeStatus::Full) {
                    self.issue(
                        KeyspaceStage::Protocol,
                        KeyspaceIssueKind::MalformedResponse,
                    );
                    return self.begin_finish();
                }
                if !self.wait_for_eof && self.response_frames.len() >= self.expected_responses {
                    self.begin_finish()
                } else {
                    self.next_read()
                }
            }
            ClientMsg::Read(Err(error)) => {
                self.issue(KeyspaceStage::Read, KeyspaceIssueKind::Call(error));
                self.begin_finish()
            }
            ClientMsg::StreamClosed(result) => {
                if let Err(error) = result {
                    self.issue(KeyspaceStage::CloseStream, KeyspaceIssueKind::Call(error));
                }
                self.publish()
            }
        }
    }
}

/// Typed result from a complete keyspace exchange.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyspaceRun {
    pub server_frames: Vec<Vec<u8>>,
    pub client_frames: Vec<Vec<u8>>,
    pub client_received_bytes: usize,
    pub server_saw_full_or_malformed: bool,
    /// Transport termination seen by the deliberate raw injector, if any.
    pub raw_write_error: Option<CallError>,
}

fn run_exchange(
    path: PathBuf,
    payload: KeyspacePayload,
    max_body_len: usize,
    max_encoded_len: usize,
) -> Result<KeyspaceRun, KeyspaceRunError> {
    if max_body_len == 0 {
        return Err(RunError::InvalidConfig(
            "max_body_len must be greater than zero",
        ));
    }
    if max_body_len > u16::MAX as usize - 4 {
        return Err(RunError::InvalidConfig(
            "max_body_len must leave room for the ack: response prefix",
        ));
    }
    let Some(max_response_body_len) = max_body_len.checked_add(4) else {
        return Err(RunError::InvalidConfig(
            "max_body_len is too large for response framing",
        ));
    };
    let expected_responses = match &payload {
        KeyspacePayload::Frames(frames) => frames.len(),
        KeyspacePayload::RawMalformed(_) => 0,
    };
    let mut config = SimulatorConfig::default();
    config.unix.default_write_cap = 2;
    let mut sim = Simulator::new(KeyspaceShard, config);

    let server = sim.register(KeyspaceServer {
        path: path.clone(),
        listener: None,
        stream: None,
        framer: LengthDelimitedFramer::new(LengthPrefix::U16, max_body_len),
        writer: None,
        max_response_body_len,
        max_encoded_len,
        frames: Vec::new(),
        saw_full_or_malformed: false,
        listener_closing_before_read: false,
        issues: Vec::new(),
    });
    let server_waiter = sim
        .observe_result::<Result<ServerReport, KeyspaceFailure>, _, _>(server)
        .map_err(|error| RunError::Observe {
            actor: "keyspace server",
            error,
        })?;

    let client = sim.register(KeyspaceClient {
        path,
        stream: None,
        payload: Some(payload),
        framed_writer: None,
        raw_writer: None,
        response_framer: LengthDelimitedFramer::new(LengthPrefix::U16, max_response_body_len),
        max_body_len,
        max_encoded_len,
        expected_responses,
        wait_for_eof: false,
        received_bytes: 0,
        response_frames: Vec::new(),
        raw_write_error: None,
        issues: Vec::new(),
    });
    let client_waiter = sim
        .observe_result::<Result<ClientReport, KeyspaceFailure>, _, _>(client)
        .map_err(|error| RunError::Observe {
            actor: "keyspace client",
            error,
        })?;

    map_start::<_, KeyspaceFailure>("keyspace server", sim.try_send(server, ServerMsg::Start))?;
    map_start::<_, KeyspaceFailure>("keyspace client", sim.try_send(client, ClientMsg::Start))?;
    sim.run_until_quiescent();
    if sim.has_in_flight_calls() {
        return Err(RunError::InFlightCalls);
    }
    let server = wait_actor("keyspace server", server_waiter, Duration::ZERO)?;
    let client = wait_actor("keyspace client", client_waiter, Duration::ZERO)?;

    Ok(KeyspaceRun {
        server_frames: server.frames,
        client_frames: client.frames,
        client_received_bytes: client.bytes,
        server_saw_full_or_malformed: server.saw_full_or_malformed,
        raw_write_error: client.raw_write_error,
    })
}

/// Run one bounded length-delimited exchange.
pub fn run_framed_keyspace(
    path: PathBuf,
    frames: Vec<Vec<u8>>,
    max_body_len: usize,
    max_encoded_len: usize,
) -> Result<KeyspaceRun, KeyspaceRunError> {
    run_exchange(
        path,
        KeyspacePayload::Frames(frames),
        max_body_len,
        max_encoded_len,
    )
}

pub fn smoke() -> Result<SpecimenReport, KeyspaceRunError> {
    let result = run_framed_keyspace(
        PathBuf::from("/tmp/specimen_framed_keyspace.sock"),
        vec![b"set:a=1".to_vec(), b"set:b=2".to_vec(), b"get:a".to_vec()],
        128,
        512,
    )?;
    Ok(SpecimenReport {
        name: "framed_keyspace",
        bytes: result.client_received_bytes as u64,
        frames: result.server_frames.len() as u64,
        ok: !result.server_saw_full_or_malformed
            && result.client_frames
                == [
                    b"ack:set:a=1".to_vec(),
                    b"ack:set:b=2".to_vec(),
                    b"ack:get:a".to_vec(),
                ],
        note: format!(
            "server_frames={} response_frames={} client_bytes={}",
            result.server_frames.len(),
            result.client_frames.len(),
            result.client_received_bytes
        ),
    })
}

/// Inject an intentionally invalid raw frame whose prefix exceeds the body cap.
pub fn bad_input_frame_too_large() -> Result<SpecimenReport, KeyspaceRunError> {
    let mut oversized = Vec::with_capacity(202);
    oversized.extend_from_slice(&(200u16).to_be_bytes());
    oversized.extend_from_slice(&[b'A'; 200]);
    let result = run_exchange(
        PathBuf::from("/tmp/specimen_framed_keyspace_bad.sock"),
        KeyspacePayload::RawMalformed(oversized),
        16,
        64,
    )?;
    Ok(SpecimenReport {
        name: "framed_keyspace:frame_too_large",
        bytes: result.client_received_bytes as u64,
        frames: result.server_frames.len() as u64,
        ok: result.server_saw_full_or_malformed
            && result
                .raw_write_error
                .is_none_or(|error| error == CallError::Io),
        note: format!(
            "framer_rejected_oversize_frame={}",
            result.server_saw_full_or_malformed
        ),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn client_decodes_every_response_across_partial_writes() {
        let result = run_framed_keyspace(
            PathBuf::from("/tmp/specimen_keyspace_coalesced.sock"),
            vec![b"abcd".to_vec(), b"x".to_vec()],
            4,
            32,
        )
        .expect("bounded keyspace exchange");
        assert_eq!(result.server_frames, [b"abcd".to_vec(), b"x".to_vec()]);
        assert_eq!(
            result.client_frames,
            [b"ack:abcd".to_vec(), b"ack:x".to_vec()]
        );
        assert!(!result.server_saw_full_or_malformed);
    }

    #[test]
    fn empty_frame_batch_closes_both_actors_without_a_write() {
        let result = run_framed_keyspace(
            PathBuf::from("/tmp/specimen_keyspace_empty.sock"),
            Vec::new(),
            4,
            16,
        )
        .expect("empty exchange");
        assert!(result.server_frames.is_empty());
        assert!(result.client_frames.is_empty());
        assert_eq!(result.client_received_bytes, 0);
    }

    #[test]
    fn bounded_body_refusal_is_typed() {
        let error = run_framed_keyspace(
            PathBuf::from("/tmp/specimen_keyspace_body_full.sock"),
            vec![b"abcde".to_vec()],
            4,
            32,
        )
        .unwrap_err();
        assert!(matches!(
            error,
            RunError::Actor {
                actor: "keyspace client",
                error: KeyspaceFailure { ref issues },
            } if issues == &[KeyspaceIssue {
                endpoint: KeyspaceEndpoint::Client,
                stage: KeyspaceStage::Encode,
                error: KeyspaceIssueKind::Frame(FramedWriteError::BodyFull {
                    body_len: 5,
                    max_body_len: 4,
                }),
            }]
        ));
    }

    #[test]
    fn zero_body_cap_is_a_fallible_config_error() {
        assert_eq!(
            run_framed_keyspace(PathBuf::from("unused"), Vec::new(), 0, 16).unwrap_err(),
            RunError::InvalidConfig("max_body_len must be greater than zero")
        );
    }

    #[test]
    fn body_cap_must_leave_u16_wire_room_for_ack_prefix() {
        assert_eq!(
            run_framed_keyspace(
                PathBuf::from("unused"),
                Vec::new(),
                u16::MAX as usize - 3,
                usize::MAX,
            )
            .unwrap_err(),
            RunError::InvalidConfig("max_body_len must leave room for the ack: response prefix")
        );
    }

    #[test]
    fn malformed_raw_injector_preserves_early_peer_close() {
        let mut payload = Vec::with_capacity(18);
        payload.extend_from_slice(&(16u16).to_be_bytes());
        payload.extend_from_slice(&[b'x'; 16]);
        let result = run_exchange(
            PathBuf::from("/tmp/specimen_keyspace_raw_close.sock"),
            KeyspacePayload::RawMalformed(payload),
            4,
            16,
        )
        .expect("server rejection is the expected protocol outcome");
        assert!(result.server_saw_full_or_malformed);
        assert_eq!(result.raw_write_error, Some(CallError::Io));
    }

    fn client_eof_result(
        response_framer: LengthDelimitedFramer,
    ) -> Result<ClientReport, KeyspaceFailure> {
        let mut sim = Simulator::new(KeyspaceShard, SimulatorConfig::default());
        let client = sim.register(KeyspaceClient {
            path: PathBuf::from("unused"),
            stream: None,
            payload: Some(KeyspacePayload::Frames(Vec::new())),
            framed_writer: None,
            raw_writer: None,
            response_framer,
            max_body_len: 8,
            max_encoded_len: 32,
            expected_responses: 2,
            wait_for_eof: false,
            received_bytes: 0,
            response_frames: vec![b"ack:first".to_vec()],
            raw_write_error: None,
            issues: Vec::new(),
        });
        let waiter = sim
            .observe_result::<Result<ClientReport, KeyspaceFailure>, _, _>(client)
            .expect("claim client result");
        sim.try_send(client, ClientMsg::Read(Ok(Vec::new())))
            .expect("deliver peer EOF");
        sim.run_until_quiescent();
        assert!(!sim.has_in_flight_calls());
        waiter
            .wait(Duration::ZERO)
            .expect("client stopped with result")
    }

    #[test]
    fn clean_peer_eof_before_expected_response_count_is_typed() {
        let failure = client_eof_result(LengthDelimitedFramer::new(LengthPrefix::U16, 12))
            .expect_err("one of two responses is premature EOF");
        assert_eq!(
            failure.issues,
            [KeyspaceIssue {
                endpoint: KeyspaceEndpoint::Client,
                stage: KeyspaceStage::Protocol,
                error: KeyspaceIssueKind::EofBeforeResponses {
                    expected: 2,
                    received: 1,
                },
            }]
        );
    }

    #[test]
    fn partial_response_at_peer_eof_is_malformed_and_incomplete() {
        let mut framer = LengthDelimitedFramer::new(LengthPrefix::U16, 12);
        assert_eq!(framer.feed([0]), 1);
        let failure = client_eof_result(framer).expect_err("partial prefix must fail closed");
        assert_eq!(
            failure.issues,
            [
                KeyspaceIssue {
                    endpoint: KeyspaceEndpoint::Client,
                    stage: KeyspaceStage::Protocol,
                    error: KeyspaceIssueKind::MalformedResponse,
                },
                KeyspaceIssue {
                    endpoint: KeyspaceEndpoint::Client,
                    stage: KeyspaceStage::Protocol,
                    error: KeyspaceIssueKind::EofBeforeResponses {
                        expected: 2,
                        received: 1,
                    },
                },
            ]
        );
    }

    #[test]
    fn truncated_request_at_peer_eof_is_not_reported_as_clean() {
        let mut framer = LengthDelimitedFramer::new(LengthPrefix::U16, 8);
        assert_eq!(framer.feed([0]), 1);
        let mut sim = Simulator::new(KeyspaceShard, SimulatorConfig::default());
        let server = sim.register(KeyspaceServer {
            path: PathBuf::from("unused"),
            listener: None,
            stream: None,
            framer,
            writer: None,
            max_response_body_len: 12,
            max_encoded_len: 32,
            frames: Vec::new(),
            saw_full_or_malformed: false,
            listener_closing_before_read: false,
            issues: Vec::new(),
        });
        let waiter = sim
            .observe_result::<Result<ServerReport, KeyspaceFailure>, _, _>(server)
            .expect("claim server result");
        sim.try_send(server, ServerMsg::Read(Ok(Vec::new())))
            .expect("deliver peer EOF");
        sim.run_until_quiescent();

        let report = waiter
            .wait(Duration::ZERO)
            .expect("server stopped with result")
            .expect("truncated input is a protocol report, not a rail failure");
        assert!(report.saw_full_or_malformed);
        assert!(!sim.has_in_flight_calls());
    }
}
