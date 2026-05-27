//! Pure HTTP/2 bad-peer probes.
//!
//! A small frame-and-stream state machine that turns malformed or hostile
//! HTTP/2 behaviour into typed [`ProtocolFact`] values and a typed outcome —
//! never "connection closed". It models frame size limits, pseudo-header
//! validation, stream lifecycle, RST_STREAM, GOAWAY, and flow-control window
//! exhaustion.
//!
//! It is not a full HTTP/2 stack and decodes no HPACK: HEADERS frames carry
//! already-decoded pseudo-headers so the probe can express duplicate-header
//! abuse without a compression dependency. The point is the typed fact
//! mapping, not byte-perfect parity with `tina-http`.

use tina_runtime::{
    Http2CloseReason, Http2FlowControlSide, Http2ResetReason, Http2StreamId, ProtocolConnectionId,
    ProtocolDirection, ProtocolFact,
};

use crate::protocol_chaos::{
    PeerAction, ProtocolChaosExpectation, ProtocolChaosFamily, ProtocolChaosReport,
    ProtocolCloseStatus, TerminalAction,
};

/// HTTP/2 connection limits the engine enforces.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Http2Limits {
    /// SETTINGS_MAX_FRAME_SIZE: a frame larger than this is a frame-size error.
    pub max_frame_size: usize,
    /// Initial per-stream send window.
    pub initial_stream_window: i64,
    /// Initial connection-level send window.
    pub initial_connection_window: i64,
}

impl Default for Http2Limits {
    fn default() -> Self {
        Self {
            max_frame_size: 16_384,
            initial_stream_window: 65_535,
            initial_connection_window: 65_535,
        }
    }
}

/// One HTTP/2 frame the probe drives, with already-decoded headers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Http2Frame {
    /// HEADERS frame opening (or continuing) a stream.
    Headers {
        /// Stream id.
        stream: u32,
        /// Already-decoded pseudo-headers `(name, value)`, e.g. `(":method", "GET")`.
        pseudo_headers: Vec<(&'static str, &'static str)>,
        /// Declared frame length on the wire (used for the frame-size check).
        declared_len: usize,
        /// Whether END_STREAM was set.
        end_stream: bool,
    },
    /// DATA frame carrying body bytes.
    Data {
        /// Stream id.
        stream: u32,
        /// Declared frame length on the wire.
        declared_len: usize,
        /// Whether END_STREAM was set.
        end_stream: bool,
    },
    /// RST_STREAM frame with a wire error code.
    RstStream {
        /// Stream id.
        stream: u32,
        /// Wire error code.
        code: u32,
    },
    /// GOAWAY frame closing the connection.
    GoAway {
        /// Last processed stream id.
        last_stream: u32,
        /// Wire error code.
        code: u32,
    },
    /// Reserve `bytes` of the stream's send window (drives window exhaustion).
    WindowReserve {
        /// Stream id.
        stream: u32,
        /// Bytes to reserve.
        bytes: u32,
    },
}

/// Typed terminal outcome of an HTTP/2 probe.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Http2Outcome {
    /// No hostile event fired; streams opened/closed cleanly.
    Clean,
    /// A stream was reset (RST_STREAM, framing, or protocol error).
    StreamReset(Http2ResetReason),
    /// A stream closed via connection GOAWAY.
    GoAway,
    /// A flow-control window reached zero.
    FlowControlExhausted(Http2FlowControlSide),
}

#[derive(Debug, Clone, Copy)]
struct StreamState {
    open: bool,
    remote_end_stream: bool,
    send_window: i64,
}

/// Pure HTTP/2 connection state machine for bad-peer probes.
pub struct Http2Connection {
    limits: Http2Limits,
    connection: ProtocolConnectionId,
    streams: Vec<(u32, StreamState)>,
    connection_window: i64,
    facts: Vec<ProtocolFact>,
    outcome: Http2Outcome,
    closed: bool,
}

impl Http2Connection {
    /// Builds a connection state machine.
    pub fn new(limits: Http2Limits) -> Self {
        Self {
            limits,
            connection: ProtocolConnectionId::new(1),
            streams: Vec::new(),
            connection_window: limits.initial_connection_window,
            facts: Vec::new(),
            outcome: Http2Outcome::Clean,
            closed: false,
        }
    }

    fn stream_mut(&mut self, stream: u32) -> Option<&mut StreamState> {
        self.streams
            .iter_mut()
            .find(|(id, _)| *id == stream)
            .map(|(_, state)| state)
    }

    fn stream(&self, stream: u32) -> Option<StreamState> {
        self.streams
            .iter()
            .find(|(id, _)| *id == stream)
            .map(|(_, state)| *state)
    }

    /// Applies one frame.
    pub fn apply(&mut self, frame: &Http2Frame) {
        if self.closed {
            return;
        }
        match frame {
            Http2Frame::Headers {
                stream,
                pseudo_headers,
                declared_len,
                end_stream,
            } => self.apply_headers(*stream, pseudo_headers, *declared_len, *end_stream),
            Http2Frame::Data {
                stream,
                declared_len,
                end_stream,
            } => self.apply_data(*stream, *declared_len, *end_stream),
            Http2Frame::RstStream { stream, code } => self.apply_rst(*stream, *code),
            Http2Frame::GoAway { last_stream, code } => self.apply_goaway(*last_stream, *code),
            Http2Frame::WindowReserve { stream, bytes } => self.apply_window(*stream, *bytes),
        }
    }

    fn apply_headers(
        &mut self,
        stream: u32,
        pseudo_headers: &[(&'static str, &'static str)],
        declared_len: usize,
        end_stream: bool,
    ) {
        if declared_len > self.limits.max_frame_size {
            self.reset_stream(
                stream,
                ProtocolDirection::Inbound,
                Http2ResetReason::FrameSizeError,
            );
            return;
        }
        if has_duplicate_pseudo_header(pseudo_headers) {
            self.reset_stream(
                stream,
                ProtocolDirection::Inbound,
                Http2ResetReason::ProtocolError,
            );
            return;
        }
        if self.stream(stream).is_none() {
            let window = self.limits.initial_stream_window;
            self.streams.push((
                stream,
                StreamState {
                    open: true,
                    remote_end_stream: end_stream,
                    send_window: window,
                },
            ));
            self.facts.push(ProtocolFact::Http2StreamOpened {
                connection: self.connection,
                stream: Http2StreamId::new(stream),
                direction: ProtocolDirection::Inbound,
            });
        } else if let Some(state) = self.stream_mut(stream) {
            state.remote_end_stream = end_stream;
        }
    }

    fn apply_data(&mut self, stream: u32, declared_len: usize, end_stream: bool) {
        if declared_len > self.limits.max_frame_size {
            self.reset_stream(
                stream,
                ProtocolDirection::Inbound,
                Http2ResetReason::FrameSizeError,
            );
            return;
        }
        match self.stream(stream) {
            // DATA on a stream that is closed or already saw END_STREAM is a
            // stream-closed error, not silent acceptance.
            None => self.reset_stream(
                stream,
                ProtocolDirection::Inbound,
                Http2ResetReason::StreamClosed,
            ),
            Some(state) if !state.open || state.remote_end_stream => self.reset_stream(
                stream,
                ProtocolDirection::Inbound,
                Http2ResetReason::StreamClosed,
            ),
            Some(_) => {
                if end_stream {
                    if let Some(state) = self.stream_mut(stream) {
                        state.remote_end_stream = true;
                    }
                }
            }
        }
    }

    fn apply_rst(&mut self, stream: u32, code: u32) {
        if self.stream(stream).is_some() {
            let reason = classify_reset(code);
            self.reset_stream(stream, ProtocolDirection::Inbound, reason);
        } else {
            // RST for an unknown stream: a stream-closed protocol error.
            self.reset_stream(
                stream,
                ProtocolDirection::Inbound,
                Http2ResetReason::StreamClosed,
            );
        }
    }

    fn apply_goaway(&mut self, last_stream: u32, _code: u32) {
        self.closed = true;
        let active: Vec<u32> = self
            .streams
            .iter()
            .filter(|(id, state)| state.open && *id > last_stream)
            .map(|(id, _)| *id)
            .collect();
        for id in active {
            if let Some(state) = self.stream_mut(id) {
                state.open = false;
            }
            self.facts.push(ProtocolFact::Http2StreamClosed {
                connection: self.connection,
                stream: Http2StreamId::new(id),
                reason: Http2CloseReason::GoAway,
            });
        }
        self.outcome = Http2Outcome::GoAway;
    }

    fn apply_window(&mut self, stream: u32, bytes: u32) {
        let bytes = i64::from(bytes);
        self.connection_window -= bytes;
        let mut stream_side_full = false;
        if let Some(state) = self.stream_mut(stream) {
            state.send_window -= bytes;
            if state.send_window <= 0 {
                stream_side_full = true;
            }
        }
        if stream_side_full {
            self.facts.push(ProtocolFact::Http2FlowControlFull {
                connection: self.connection,
                stream: Http2StreamId::new(stream),
                side: Http2FlowControlSide::StreamSend,
            });
            self.outcome = Http2Outcome::FlowControlExhausted(Http2FlowControlSide::StreamSend);
        } else if self.connection_window <= 0 {
            self.facts.push(ProtocolFact::Http2FlowControlFull {
                connection: self.connection,
                stream: Http2StreamId::new(0),
                side: Http2FlowControlSide::ConnectionSend,
            });
            self.outcome = Http2Outcome::FlowControlExhausted(Http2FlowControlSide::ConnectionSend);
        }
    }

    fn reset_stream(
        &mut self,
        stream: u32,
        direction: ProtocolDirection,
        reason: Http2ResetReason,
    ) {
        if let Some(state) = self.stream_mut(stream) {
            state.open = false;
        }
        self.facts.push(ProtocolFact::Http2StreamReset {
            connection: self.connection,
            stream: Http2StreamId::new(stream),
            direction,
            reason,
        });
        self.outcome = Http2Outcome::StreamReset(reason);
    }

    /// Returns the typed facts observed so far.
    pub fn facts(&self) -> &[ProtocolFact] {
        &self.facts
    }

    /// Returns the terminal outcome.
    pub fn outcome(&self) -> &Http2Outcome {
        &self.outcome
    }

    /// Consumes the connection into facts + outcome.
    pub fn finish(self) -> (Vec<ProtocolFact>, Http2Outcome) {
        (self.facts, self.outcome)
    }
}

/// Returns true when a pseudo-header name appears more than once.
fn has_duplicate_pseudo_header(headers: &[(&'static str, &'static str)]) -> bool {
    let mut seen: Vec<&str> = Vec::new();
    for (name, _) in headers {
        if name.starts_with(':') {
            if seen.contains(name) {
                return true;
            }
            seen.push(name);
        }
    }
    false
}

/// Maps a wire RST_STREAM error code to a typed reason.
fn classify_reset(code: u32) -> Http2ResetReason {
    match code {
        0x0 => Http2ResetReason::NoError,
        0x1 => Http2ResetReason::ProtocolError,
        0x2 => Http2ResetReason::InternalError,
        0x3 => Http2ResetReason::FlowControlError,
        0x4 => Http2ResetReason::SettingsTimeout,
        0x5 => Http2ResetReason::StreamClosed,
        0x6 => Http2ResetReason::FrameSizeError,
        0x7 => Http2ResetReason::RefusedStream,
        0x8 => Http2ResetReason::Cancel,
        0x9 => Http2ResetReason::CompressionError,
        0xa => Http2ResetReason::ConnectError,
        0xb => Http2ResetReason::EnhanceYourCalm,
        0xc => Http2ResetReason::InadequateSecurity,
        0xd => Http2ResetReason::Http11Required,
        other => Http2ResetReason::OtherCode(other),
    }
}

/// One HTTP/2 bad-peer probe: a frame sequence plus its typed expectation.
#[derive(Debug, Clone)]
pub struct Http2Probe {
    /// Stable probe name.
    pub name: &'static str,
    /// Connection limits.
    pub limits: Http2Limits,
    /// Frames the hostile peer sends.
    pub frames: Vec<Http2Frame>,
    /// Expected typed facts, in order.
    pub expected_facts: Vec<ProtocolFact>,
    /// Expected terminal outcome.
    pub expected_outcome: Http2Outcome,
}

impl Http2Probe {
    /// Runs the probe frames through a fresh connection.
    pub fn run(&self) -> (Vec<ProtocolFact>, Http2Outcome) {
        let mut conn = Http2Connection::new(self.limits);
        for frame in &self.frames {
            conn.apply(frame);
        }
        conn.finish()
    }

    /// Runs the probe and asserts its facts and outcome match.
    pub fn check(&self) -> Result<ProtocolChaosReport, Http2ProbeMismatch> {
        let (facts, outcome) = self.run();
        let mut diverged = Vec::new();
        if facts != self.expected_facts {
            diverged.push("facts");
        }
        if outcome != self.expected_outcome {
            diverged.push("outcome");
        }
        if diverged.is_empty() {
            Ok(self.report(&facts, &outcome))
        } else {
            Err(Http2ProbeMismatch {
                name: self.name,
                diverged,
                expected_facts: self.expected_facts.clone(),
                actual_facts: facts,
                expected_outcome: self.expected_outcome.clone(),
                actual_outcome: outcome,
            })
        }
    }

    fn report(&self, facts: &[ProtocolFact], outcome: &Http2Outcome) -> ProtocolChaosReport {
        let mut report = ProtocolChaosReport::new(self.name, ProtocolChaosFamily::Http2);
        report.peer_action = PeerAction::SentFrames;
        report.protocol_facts = facts.to_vec();
        match outcome {
            Http2Outcome::StreamReset(reason) => {
                report.terminal_action = TerminalAction::Rejected;
                report.close_status = Some(ProtocolCloseStatus::Http2Reset(*reason));
            }
            Http2Outcome::GoAway => {
                report.terminal_action = TerminalAction::ServerClosed;
                report.close_status = Some(ProtocolCloseStatus::Http2GoAway);
            }
            Http2Outcome::FlowControlExhausted(_) => {
                report.terminal_action = TerminalAction::TimedOut;
            }
            Http2Outcome::Clean => {}
        }
        report
    }

    /// Builds the derived chaos expectation for this probe.
    pub fn expectation(&self) -> ProtocolChaosExpectation {
        let report = self.report(&self.expected_facts, &self.expected_outcome);
        ProtocolChaosExpectation {
            family: ProtocolChaosFamily::Http2,
            app_deliveries: 0,
            terminal_action: report.terminal_action,
            close_status: report.close_status,
            protocol_facts: self.expected_facts.clone(),
        }
    }
}

/// Typed mismatch from an HTTP/2 probe.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Http2ProbeMismatch {
    /// Probe name.
    pub name: &'static str,
    /// Which parts diverged.
    pub diverged: Vec<&'static str>,
    /// Expected facts.
    pub expected_facts: Vec<ProtocolFact>,
    /// Observed facts.
    pub actual_facts: Vec<ProtocolFact>,
    /// Expected outcome.
    pub expected_outcome: Http2Outcome,
    /// Observed outcome.
    pub actual_outcome: Http2Outcome,
}

impl std::fmt::Display for Http2ProbeMismatch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        writeln!(
            f,
            "http2 probe `{}` diverged: {:?}",
            self.name, self.diverged
        )?;
        writeln!(
            f,
            "  outcome: expected {:?}, got {:?}",
            self.expected_outcome, self.actual_outcome
        )?;
        writeln!(f, "  facts expected: {:?}", self.expected_facts)?;
        writeln!(f, "  facts actual:   {:?}", self.actual_facts)
    }
}

impl std::error::Error for Http2ProbeMismatch {}

fn opened(stream: u32) -> ProtocolFact {
    ProtocolFact::Http2StreamOpened {
        connection: ProtocolConnectionId::new(1),
        stream: Http2StreamId::new(stream),
        direction: ProtocolDirection::Inbound,
    }
}

fn reset(stream: u32, reason: Http2ResetReason) -> ProtocolFact {
    ProtocolFact::Http2StreamReset {
        connection: ProtocolConnectionId::new(1),
        stream: Http2StreamId::new(stream),
        direction: ProtocolDirection::Inbound,
        reason,
    }
}

/// The hermetic HTTP/2 bad-peer probe suite.
pub fn http2_probe_suite() -> Vec<Http2Probe> {
    let limits = Http2Limits::default();
    vec![
        // 1. Invalid HTTP/2 frame size on a HEADERS frame.
        Http2Probe {
            name: "h2_invalid_frame_size",
            limits,
            frames: vec![Http2Frame::Headers {
                stream: 1,
                pseudo_headers: vec![(":method", "GET"), (":path", "/")],
                declared_len: limits.max_frame_size + 1,
                end_stream: true,
            }],
            expected_facts: vec![reset(1, Http2ResetReason::FrameSizeError)],
            expected_outcome: Http2Outcome::StreamReset(Http2ResetReason::FrameSizeError),
        },
        // 2. Duplicate pseudo-header.
        Http2Probe {
            name: "h2_duplicate_pseudo_header",
            limits,
            frames: vec![Http2Frame::Headers {
                stream: 1,
                pseudo_headers: vec![(":method", "GET"), (":method", "POST"), (":path", "/")],
                declared_len: 32,
                end_stream: true,
            }],
            expected_facts: vec![reset(1, Http2ResetReason::ProtocolError)],
            expected_outcome: Http2Outcome::StreamReset(Http2ResetReason::ProtocolError),
        },
        // 3. DATA after the stream was closed by END_STREAM.
        Http2Probe {
            name: "h2_data_after_stream_close",
            limits,
            frames: vec![
                Http2Frame::Headers {
                    stream: 1,
                    pseudo_headers: vec![(":method", "POST"), (":path", "/")],
                    declared_len: 32,
                    end_stream: true,
                },
                Http2Frame::Data {
                    stream: 1,
                    declared_len: 8,
                    end_stream: false,
                },
            ],
            expected_facts: vec![opened(1), reset(1, Http2ResetReason::StreamClosed)],
            expected_outcome: Http2Outcome::StreamReset(Http2ResetReason::StreamClosed),
        },
        // 4. RST_STREAM while a response body stream is active.
        Http2Probe {
            name: "h2_rst_stream_during_body",
            limits,
            frames: vec![
                Http2Frame::Headers {
                    stream: 3,
                    pseudo_headers: vec![(":method", "GET"), (":path", "/stream")],
                    declared_len: 32,
                    end_stream: false,
                },
                Http2Frame::RstStream {
                    stream: 3,
                    code: 0x8,
                },
            ],
            expected_facts: vec![opened(3), reset(3, Http2ResetReason::Cancel)],
            expected_outcome: Http2Outcome::StreamReset(Http2ResetReason::Cancel),
        },
        // 5. GOAWAY while streams are active.
        Http2Probe {
            name: "h2_goaway_with_active_streams",
            limits,
            frames: vec![
                Http2Frame::Headers {
                    stream: 1,
                    pseudo_headers: vec![(":method", "GET"), (":path", "/a")],
                    declared_len: 32,
                    end_stream: false,
                },
                Http2Frame::Headers {
                    stream: 3,
                    pseudo_headers: vec![(":method", "GET"), (":path", "/b")],
                    declared_len: 32,
                    end_stream: false,
                },
                Http2Frame::GoAway {
                    last_stream: 1,
                    code: 0x0,
                },
            ],
            expected_facts: vec![
                opened(1),
                opened(3),
                ProtocolFact::Http2StreamClosed {
                    connection: ProtocolConnectionId::new(1),
                    stream: Http2StreamId::new(3),
                    reason: Http2CloseReason::GoAway,
                },
            ],
            expected_outcome: Http2Outcome::GoAway,
        },
        // 6. Flow-control window exhaustion on a stream.
        Http2Probe {
            name: "h2_flow_control_exhaustion",
            limits: Http2Limits {
                max_frame_size: 16_384,
                initial_stream_window: 64,
                initial_connection_window: 65_535,
            },
            frames: vec![
                Http2Frame::Headers {
                    stream: 1,
                    pseudo_headers: vec![(":method", "POST"), (":path", "/upload")],
                    declared_len: 32,
                    end_stream: false,
                },
                Http2Frame::WindowReserve {
                    stream: 1,
                    bytes: 64,
                },
            ],
            expected_facts: vec![
                opened(1),
                ProtocolFact::Http2FlowControlFull {
                    connection: ProtocolConnectionId::new(1),
                    stream: Http2StreamId::new(1),
                    side: Http2FlowControlSide::StreamSend,
                },
            ],
            expected_outcome: Http2Outcome::FlowControlExhausted(Http2FlowControlSide::StreamSend),
        },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_probe_maps_to_typed_facts_not_just_closed() {
        for probe in http2_probe_suite() {
            let report = probe
                .check()
                .unwrap_or_else(|mismatch| panic!("{mismatch}"));
            // Every probe produces at least one typed protocol fact.
            assert!(
                !report.protocol_facts.is_empty(),
                "{}: must emit a typed fact, not just close",
                probe.name
            );
            probe
                .expectation()
                .check(&report)
                .unwrap_or_else(|mismatch| {
                    panic!("chaos expectation for {}: {mismatch}", probe.name)
                });
        }
    }

    #[test]
    fn frame_size_error_is_typed_frame_size_reason() {
        let probe = http2_probe_suite()
            .into_iter()
            .find(|p| p.name == "h2_invalid_frame_size")
            .expect("probe present");
        let (facts, outcome) = probe.run();
        assert_eq!(facts, vec![reset(1, Http2ResetReason::FrameSizeError)]);
        assert_eq!(
            outcome,
            Http2Outcome::StreamReset(Http2ResetReason::FrameSizeError)
        );
    }

    #[test]
    fn goaway_closes_only_streams_above_last_processed() {
        let probe = http2_probe_suite()
            .into_iter()
            .find(|p| p.name == "h2_goaway_with_active_streams")
            .expect("probe present");
        let (facts, _) = probe.run();
        // Stream 1 (<= last_stream) is not force-closed; stream 3 is.
        let closed_streams: Vec<u32> = facts
            .iter()
            .filter_map(|fact| match fact {
                ProtocolFact::Http2StreamClosed { stream, .. } => Some(stream.get()),
                _ => None,
            })
            .collect();
        assert_eq!(closed_streams, vec![3]);
    }

    #[test]
    fn drifted_probe_expectation_fails_closed() {
        let mut probe = http2_probe_suite()
            .into_iter()
            .find(|p| p.name == "h2_duplicate_pseudo_header")
            .expect("probe present");
        probe.expected_outcome = Http2Outcome::Clean;
        let mismatch = probe.check().expect_err("drift detected");
        assert!(mismatch.diverged.contains(&"outcome"));
    }

    #[test]
    fn connection_level_flow_control_exhaustion_is_typed() {
        // A small connection window with a roomy stream window: the reserve
        // empties the connection side, not the stream side.
        let limits = Http2Limits {
            max_frame_size: 16_384,
            initial_stream_window: 1_000,
            initial_connection_window: 64,
        };
        let mut conn = Http2Connection::new(limits);
        conn.apply(&Http2Frame::Headers {
            stream: 1,
            pseudo_headers: vec![(":method", "POST"), (":path", "/upload")],
            declared_len: 32,
            end_stream: false,
        });
        conn.apply(&Http2Frame::WindowReserve {
            stream: 1,
            bytes: 64,
        });
        // Inspect via the borrowing accessors before consuming the connection.
        assert_eq!(
            conn.outcome(),
            &Http2Outcome::FlowControlExhausted(Http2FlowControlSide::ConnectionSend)
        );
        assert!(conn.facts().iter().any(|fact| matches!(
            fact,
            ProtocolFact::Http2FlowControlFull {
                side: Http2FlowControlSide::ConnectionSend,
                ..
            }
        )));
        let (facts, outcome) = conn.finish();
        assert_eq!(
            outcome,
            Http2Outcome::FlowControlExhausted(Http2FlowControlSide::ConnectionSend)
        );
        assert_eq!(facts.len(), 2, "stream-opened + flow-control-full");
    }
}
