//! Local admin sidecar over simulator Unix-domain sockets with bounded
//! line-delimited input and output.

use std::path::PathBuf;
use std::time::Duration;

use tina::{Effect, Shard, ShardId, stop_with};
use tina_codec::{DecodeStatus, LineFramer, decode_chunk};
use tina_runtime::{
    CallError, FramedWriteError, LoopStep, UnixAcceptReply, UnixBindReply, UnixConnectReply,
    UnixFramedWriter, UnixListenerId, UnixReadReply, UnixStreamId, UnixWriteAll,
    UnixWriteOwnedReply, unix_accept, unix_bind, unix_close_listener, unix_close_stream,
    unix_connect, unix_read,
};
use tina_sim::{Simulator, SimulatorConfig};

use crate::{RunError, SpecimenReport, map_start, wait_actor};

#[derive(Debug, Default)]
pub struct AdminShard;

impl Shard for AdminShard {
    fn id(&self) -> ShardId {
        ShardId::new(102)
    }
}

/// Side of the admin exchange that encountered a failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AdminEndpoint {
    /// Listening/serving side.
    Server,
    /// Connecting side.
    Client,
}

/// Exact operation that failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AdminStage {
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

/// One exact failure observed while completing an admin actor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdminIssue {
    pub endpoint: AdminEndpoint,
    pub stage: AdminStage,
    pub error: AdminIssueKind,
}

/// Typed failure detail for an admin operation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AdminIssueKind {
    Call(CallError),
    Frame(FramedWriteError),
    EofBeforeResponses { expected: usize, received: usize },
    MalformedResponse,
}

/// All primary and cleanup failures from one actor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdminFailure {
    pub issues: Vec<AdminIssue>,
}

impl std::fmt::Display for AdminFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{:?}", self.issues)
    }
}

impl std::error::Error for AdminFailure {}

/// Host-side failure from an admin exchange.
pub type AdminRunError = RunError<AdminFailure>;

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
    seen: Vec<Vec<u8>>,
    malformed_or_full: bool,
}

struct AdminServer {
    path: PathBuf,
    listener: Option<UnixListenerId>,
    stream: Option<UnixStreamId>,
    framer: LineFramer,
    writer: Option<UnixFramedWriter>,
    max_response_line_len: usize,
    max_encoded_len: usize,
    seen: Vec<Vec<u8>>,
    malformed_or_full: bool,
    close_after_write: bool,
    listener_closing_before_read: bool,
    issues: Vec<AdminIssue>,
}

impl AdminServer {
    fn issue(&mut self, stage: AdminStage, error: AdminIssueKind) {
        self.issues.push(AdminIssue {
            endpoint: AdminEndpoint::Server,
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
            seen: std::mem::take(&mut self.seen),
            malformed_or_full: self.malformed_or_full,
        };
        if self.issues.is_empty() {
            stop_with(Ok::<_, AdminFailure>(report))
        } else {
            stop_with(Err::<ServerReport, _>(AdminFailure {
                issues: std::mem::take(&mut self.issues),
            }))
        }
    }

    fn next_read(&self) -> Effect<Self> {
        unix_read(self.stream.expect("stream open while reading"), 64).then(ServerMsg::Read)
    }
}

#[tina_runtime::isolate(message = ServerMsg, shard = AdminShard)]
impl AdminServer {
    fn handle(
        &mut self,
        msg: ServerMsg,
        _ctx: &mut Context<'_, AdminShard, Self::Reply>,
    ) -> Effect<Self> {
        match msg {
            ServerMsg::Start => unix_bind(self.path.clone()).then(ServerMsg::Bound),
            ServerMsg::Bound(Ok((listener, _))) => {
                self.listener = Some(listener);
                unix_accept(listener).then(ServerMsg::Accepted)
            }
            ServerMsg::Bound(Err(error)) => {
                self.issue(AdminStage::Bind, AdminIssueKind::Call(error));
                self.begin_finish()
            }
            ServerMsg::Accepted(Ok(stream)) => {
                self.stream = Some(stream);
                self.listener_closing_before_read = true;
                let listener = self.listener.take().expect("listener owned after accept");
                unix_close_listener(listener).then(ServerMsg::ListenerClosed)
            }
            ServerMsg::Accepted(Err(error)) => {
                self.issue(AdminStage::Accept, AdminIssueKind::Call(error));
                self.begin_finish()
            }
            ServerMsg::ListenerClosed(result) if self.listener_closing_before_read => {
                self.listener_closing_before_read = false;
                match result {
                    Ok(()) => self.next_read(),
                    Err(error) => {
                        self.issue(AdminStage::CloseListener, AdminIssueKind::Call(error));
                        self.begin_finish()
                    }
                }
            }
            ServerMsg::ListenerClosed(result) => {
                if let Err(error) = result {
                    self.issue(AdminStage::CloseListener, AdminIssueKind::Call(error));
                }
                self.cleanup()
            }
            ServerMsg::Read(Ok(bytes)) if bytes.is_empty() => self.begin_finish(),
            ServerMsg::Read(Ok(bytes)) => {
                let stream = self.stream.expect("stream open while decoding");
                let mut writer = UnixFramedWriter::lines(
                    stream,
                    self.max_response_line_len,
                    self.max_encoded_len,
                );
                let mut should_close = false;
                let mut frame_error = None;
                let status = decode_chunk(&mut self.framer, &bytes, |line| {
                    if should_close || frame_error.is_some() {
                        return;
                    }
                    self.seen.push(line.clone());
                    if line == b"shutdown" {
                        should_close = true;
                        return;
                    }
                    let mut response = b"ok ".to_vec();
                    response.extend_from_slice(&line);
                    if let Err(error) = writer.push_frame(response) {
                        frame_error = Some(error);
                    }
                });
                if let Some(error) = frame_error {
                    self.issue(AdminStage::Encode, AdminIssueKind::Frame(error));
                    return self.begin_finish();
                }
                if !should_close
                    && matches!(status, DecodeStatus::Malformed(_) | DecodeStatus::Full)
                {
                    self.malformed_or_full = true;
                    should_close = true;
                }
                if let Some(effect) = writer.next_effect(ServerMsg::Wrote) {
                    self.writer = Some(writer);
                    self.close_after_write = should_close;
                    effect
                } else if should_close {
                    self.begin_finish()
                } else {
                    self.next_read()
                }
            }
            ServerMsg::Read(Err(error)) => {
                self.issue(AdminStage::Read, AdminIssueKind::Call(error));
                self.begin_finish()
            }
            ServerMsg::Wrote(reply) => {
                let writer = self.writer.as_mut().expect("framed writer armed");
                match writer.advance::<Self, _, _>(reply, ServerMsg::Wrote) {
                    LoopStep::Pending(effect) => effect,
                    LoopStep::Done(_) => {
                        self.writer = None;
                        if self.close_after_write {
                            self.begin_finish()
                        } else {
                            self.next_read()
                        }
                    }
                    LoopStep::Failed(error) => {
                        self.issue(AdminStage::Write, AdminIssueKind::Call(error));
                        self.begin_finish()
                    }
                }
            }
            ServerMsg::StreamClosed(result) => {
                if let Err(error) = result {
                    self.issue(AdminStage::CloseStream, AdminIssueKind::Call(error));
                }
                self.cleanup()
            }
        }
    }
}

#[derive(Debug)]
enum AdminPayload {
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
    received_bytes: usize,
    responses: Vec<Vec<u8>>,
    raw_write_error: Option<CallError>,
}

struct AdminClient {
    path: PathBuf,
    stream: Option<UnixStreamId>,
    payload: Option<AdminPayload>,
    framed_writer: Option<UnixFramedWriter>,
    raw_writer: Option<UnixWriteAll>,
    response_framer: LineFramer,
    max_line_len: usize,
    max_encoded_len: usize,
    expected_responses: usize,
    wait_for_eof: bool,
    received_bytes: usize,
    responses: Vec<Vec<u8>>,
    raw_write_error: Option<CallError>,
    issues: Vec<AdminIssue>,
}

impl AdminClient {
    fn issue(&mut self, stage: AdminStage, error: AdminIssueKind) {
        self.issues.push(AdminIssue {
            endpoint: AdminEndpoint::Client,
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
            received_bytes: self.received_bytes,
            responses: std::mem::take(&mut self.responses),
            raw_write_error: self.raw_write_error,
        };
        if self.issues.is_empty() {
            stop_with(Ok::<_, AdminFailure>(report))
        } else {
            stop_with(Err::<ClientReport, _>(AdminFailure {
                issues: std::mem::take(&mut self.issues),
            }))
        }
    }

    fn next_read(&self) -> Effect<Self> {
        unix_read(self.stream.expect("stream open while reading"), 64).then(ClientMsg::Read)
    }
}

#[tina_runtime::isolate(message = ClientMsg, shard = AdminShard)]
impl AdminClient {
    fn handle(
        &mut self,
        msg: ClientMsg,
        _ctx: &mut Context<'_, AdminShard, Self::Reply>,
    ) -> Effect<Self> {
        match msg {
            ClientMsg::Start => unix_connect(self.path.clone()).then(ClientMsg::Connected),
            ClientMsg::Connected(Ok(stream)) => {
                self.stream = Some(stream);
                match self.payload.take().expect("client payload available") {
                    AdminPayload::Frames(frames) => {
                        if frames.is_empty() {
                            return self.begin_finish();
                        }
                        let mut writer = UnixFramedWriter::lines(
                            stream,
                            self.max_line_len,
                            self.max_encoded_len,
                        );
                        for frame in frames {
                            if let Err(error) = writer.push_frame(frame) {
                                self.issue(AdminStage::Encode, AdminIssueKind::Frame(error));
                                return self.begin_finish();
                            }
                        }
                        let effect = writer
                            .next_effect(ClientMsg::Wrote)
                            .expect("non-empty frame batch has a write effect");
                        self.framed_writer = Some(writer);
                        effect
                    }
                    AdminPayload::RawMalformed(bytes) => {
                        self.wait_for_eof = true;
                        if bytes.is_empty() {
                            return self.begin_finish();
                        }
                        let mut writer = UnixWriteAll::new(stream, bytes);
                        let effect = writer
                            .next_effect(ClientMsg::Wrote)
                            .expect("non-empty raw payload has a write effect");
                        self.raw_writer = Some(writer);
                        effect
                    }
                }
            }
            ClientMsg::Connected(Err(error)) => {
                self.issue(AdminStage::Connect, AdminIssueKind::Call(error));
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
                        if self.expected_responses == 0 && !self.wait_for_eof {
                            self.begin_finish()
                        } else {
                            self.next_read()
                        }
                    }
                    LoopStep::Failed(error) => {
                        if self.wait_for_eof {
                            self.raw_write_error = Some(error);
                        } else {
                            self.issue(AdminStage::Write, AdminIssueKind::Call(error));
                        }
                        self.begin_finish()
                    }
                }
            }
            ClientMsg::Read(Ok(bytes)) if bytes.is_empty() => {
                if !self.wait_for_eof && self.responses.len() < self.expected_responses {
                    self.issue(
                        AdminStage::Protocol,
                        AdminIssueKind::EofBeforeResponses {
                            expected: self.expected_responses,
                            received: self.responses.len(),
                        },
                    );
                }
                self.begin_finish()
            }
            ClientMsg::Read(Ok(bytes)) => {
                self.received_bytes += bytes.len();
                let status = decode_chunk(&mut self.response_framer, &bytes, |line| {
                    self.responses.push(line)
                });
                if matches!(status, DecodeStatus::Malformed(_) | DecodeStatus::Full) {
                    self.issue(AdminStage::Protocol, AdminIssueKind::MalformedResponse);
                    return self.begin_finish();
                }
                if !self.wait_for_eof && self.responses.len() >= self.expected_responses {
                    self.begin_finish()
                } else {
                    self.next_read()
                }
            }
            ClientMsg::Read(Err(error)) => {
                self.issue(AdminStage::Read, AdminIssueKind::Call(error));
                self.begin_finish()
            }
            ClientMsg::StreamClosed(result) => {
                if let Err(error) = result {
                    self.issue(AdminStage::CloseStream, AdminIssueKind::Call(error));
                }
                self.publish()
            }
        }
    }
}

/// Result of one admin-socket exchange.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdminRun {
    /// Lines the server saw, in order.
    pub server_saw: Vec<Vec<u8>>,
    /// Complete response frames decoded by the client.
    pub client_responses: Vec<Vec<u8>>,
    /// Encoded response bytes delivered to the client.
    pub client_received_bytes: usize,
    /// Whether the server rejected input as full or malformed.
    pub server_saw_malformed_or_full: bool,
    /// Transport termination seen by the deliberate raw injector, if any.
    pub raw_write_error: Option<CallError>,
}

fn expected_responses(commands: &[Vec<u8>]) -> usize {
    commands
        .iter()
        .take_while(|command| command.as_slice() != b"shutdown")
        .count()
}

fn run_exchange(
    path: PathBuf,
    payload: AdminPayload,
    max_line_len: usize,
    max_encoded_len: usize,
) -> Result<AdminRun, AdminRunError> {
    if max_line_len == 0 {
        return Err(RunError::InvalidConfig(
            "max_line_len must be greater than zero",
        ));
    }
    let Some(max_response_line_len) = max_line_len.checked_add(3) else {
        return Err(RunError::InvalidConfig(
            "max_line_len is too large for response framing",
        ));
    };
    let expected_responses = match &payload {
        AdminPayload::Frames(commands) => expected_responses(commands),
        AdminPayload::RawMalformed(_) => 0,
    };
    let mut config = SimulatorConfig::default();
    config.unix.default_write_cap = 2;
    let mut sim = Simulator::new(AdminShard, config);

    let server = sim.register(AdminServer {
        path: path.clone(),
        listener: None,
        stream: None,
        framer: LineFramer::new(max_line_len),
        writer: None,
        max_response_line_len,
        max_encoded_len,
        seen: Vec::new(),
        malformed_or_full: false,
        close_after_write: false,
        listener_closing_before_read: false,
        issues: Vec::new(),
    });
    let server_waiter = sim
        .observe_result::<Result<ServerReport, AdminFailure>, _, _>(server)
        .map_err(|error| RunError::Observe {
            actor: "admin server",
            error,
        })?;

    let client = sim.register(AdminClient {
        path,
        stream: None,
        payload: Some(payload),
        framed_writer: None,
        raw_writer: None,
        response_framer: LineFramer::new(max_response_line_len),
        max_line_len,
        max_encoded_len,
        expected_responses,
        wait_for_eof: false,
        received_bytes: 0,
        responses: Vec::new(),
        raw_write_error: None,
        issues: Vec::new(),
    });
    let client_waiter = sim
        .observe_result::<Result<ClientReport, AdminFailure>, _, _>(client)
        .map_err(|error| RunError::Observe {
            actor: "admin client",
            error,
        })?;

    map_start::<_, AdminFailure>("admin server", sim.try_send(server, ServerMsg::Start))?;
    map_start::<_, AdminFailure>("admin client", sim.try_send(client, ClientMsg::Start))?;
    sim.run_until_quiescent();
    if sim.has_in_flight_calls() {
        return Err(RunError::InFlightCalls);
    }
    let server = wait_actor("admin server", server_waiter, Duration::ZERO)?;
    let client = wait_actor("admin client", client_waiter, Duration::ZERO)?;

    Ok(AdminRun {
        server_saw: server.seen,
        client_responses: client.responses,
        client_received_bytes: client.received_bytes,
        server_saw_malformed_or_full: server.malformed_or_full,
        raw_write_error: client.raw_write_error,
    })
}

/// Run one bounded client/server line-framed exchange.
pub fn run_admin_socket(
    path: PathBuf,
    mut commands: Vec<Vec<u8>>,
    max_line_len: usize,
    max_encoded_len: usize,
) -> Result<AdminRun, AdminRunError> {
    if let Some(shutdown) = commands
        .iter()
        .position(|command| command.as_slice() == b"shutdown")
    {
        commands.truncate(shutdown + 1);
    }
    run_exchange(
        path,
        AdminPayload::Frames(commands),
        max_line_len,
        max_encoded_len,
    )
}

/// Smoke command: drives commands and a graceful shutdown.
pub fn smoke() -> Result<SpecimenReport, AdminRunError> {
    let result = run_admin_socket(
        PathBuf::from("/tmp/specimen_admin.sock"),
        vec![b"ping".to_vec(), b"status".to_vec(), b"shutdown".to_vec()],
        64,
        256,
    )?;
    let frames = result.server_saw.len() as u64;
    let bytes = result.client_received_bytes as u64;
    Ok(SpecimenReport {
        name: "admin_socket",
        bytes,
        frames,
        ok: !result.server_saw_malformed_or_full
            && result.client_responses == [b"ok ping".to_vec(), b"ok status".to_vec()],
        note: format!(
            "server_saw_lines={} response_frames={} bytes_received_by_client={}",
            frames,
            result.client_responses.len(),
            bytes,
        ),
    })
}

/// Bad-input proof: an over-cap raw line is rejected without using the
/// canonical framed producer that would refuse to encode it.
pub fn bad_input_line_too_long() -> Result<SpecimenReport, AdminRunError> {
    let mut huge = vec![b'X'; 64];
    huge.push(b'\n');
    let result = run_exchange(
        PathBuf::from("/tmp/specimen_admin_bad.sock"),
        AdminPayload::RawMalformed(huge),
        8,
        64,
    )?;
    Ok(SpecimenReport {
        name: "admin_socket:line_too_long",
        bytes: result.client_received_bytes as u64,
        frames: result.server_saw.len() as u64,
        ok: result.server_saw_malformed_or_full
            && result
                .raw_write_error
                .is_none_or(|error| error == CallError::Io),
        note: format!(
            "rejected_oversize_line={}",
            result.server_saw_malformed_or_full
        ),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn coalesced_shutdown_flushes_every_prior_response() {
        let result = run_admin_socket(
            PathBuf::from("/tmp/specimen_admin_coalesced.sock"),
            vec![
                b"abcdefgh".to_vec(),
                b"x".to_vec(),
                b"shutdown".to_vec(),
                b"ignored".to_vec(),
            ],
            8,
            64,
        )
        .expect("bounded exchange");
        assert_eq!(
            result.server_saw,
            [b"abcdefgh".to_vec(), b"x".to_vec(), b"shutdown".to_vec()]
        );
        assert_eq!(
            result.client_responses,
            [b"ok abcdefgh".to_vec(), b"ok x".to_vec()]
        );
        assert!(!result.server_saw_malformed_or_full);
    }

    #[test]
    fn empty_command_batch_closes_both_actors_without_a_write() {
        let result = run_admin_socket(
            PathBuf::from("/tmp/specimen_admin_empty.sock"),
            Vec::new(),
            8,
            16,
        )
        .expect("empty exchange");
        assert!(result.server_saw.is_empty());
        assert!(result.client_responses.is_empty());
        assert_eq!(result.client_received_bytes, 0);
    }

    #[test]
    fn bounded_frame_refusal_is_typed() {
        let error = run_admin_socket(
            PathBuf::from("/tmp/specimen_admin_batch_full.sock"),
            vec![b"ping".to_vec()],
            8,
            4,
        )
        .unwrap_err();
        assert!(matches!(
            error,
            RunError::Actor {
                actor: "admin client",
                error: AdminFailure { ref issues },
            } if issues == &[AdminIssue {
                endpoint: AdminEndpoint::Client,
                stage: AdminStage::Encode,
                error: AdminIssueKind::Frame(FramedWriteError::BatchFull {
                    encoded_len: 0,
                    frame_len: 5,
                    max_encoded_len: 4,
                }),
            }]
        ));
    }

    #[test]
    fn zero_line_cap_is_a_fallible_config_error() {
        assert_eq!(
            run_admin_socket(PathBuf::from("unused"), Vec::new(), 0, 16).unwrap_err(),
            RunError::InvalidConfig("max_line_len must be greater than zero")
        );
    }

    #[test]
    fn malformed_raw_injector_preserves_early_peer_close() {
        let mut payload = vec![b'x'; 16];
        payload.push(b'\n');
        let result = run_exchange(
            PathBuf::from("/tmp/specimen_admin_raw_close.sock"),
            AdminPayload::RawMalformed(payload),
            4,
            16,
        )
        .expect("server rejection is the expected protocol outcome");
        assert!(result.server_saw_malformed_or_full);
        assert_eq!(result.raw_write_error, Some(CallError::Io));
    }
}
