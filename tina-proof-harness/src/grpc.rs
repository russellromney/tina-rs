//! Pure gRPC bad-peer probes.
//!
//! A small length-prefixed message decoder plus trailer-status handling that
//! turns malformed gRPC responses into typed [`ProtocolFact`] values and a
//! typed [`GrpcOutcome`] — never a bare "connection closed". It models the
//! 5-byte gRPC frame prefix, the message size cap, the compression flag, and
//! the `grpc-status` trailer.
//!
//! It is not a full gRPC stack: it decodes framing and status, not protobuf
//! bodies. The point is the typed outcome and fact mapping.

use tina_runtime::{GrpcStatusCode, GrpcStreamId, ProtocolConnectionId, ProtocolFact};

use crate::protocol_chaos::{
    PeerAction, ProtocolChaosExpectation, ProtocolChaosFamily, ProtocolChaosReport,
    ProtocolCloseStatus, TerminalAction,
};

/// gRPC decode limits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GrpcLimits {
    /// Maximum decoded message length in one frame.
    pub max_message_bytes: usize,
}

impl Default for GrpcLimits {
    fn default() -> Self {
        Self {
            max_message_bytes: 4 * 1024 * 1024,
        }
    }
}

/// Length of the gRPC frame prefix: 1 compression flag + 4 length bytes.
const GRPC_PREFIX_LEN: usize = 5;

/// Typed terminal outcome of a gRPC probe.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GrpcOutcome {
    /// A well-formed final status arrived.
    Status(GrpcStatusCode),
    /// No `grpc-status` trailer was present; the client defaults to UNKNOWN.
    MissingStatus,
    /// A frame declared a message larger than the cap.
    MessageTooLarge,
    /// A frame was truncated or malformed.
    MalformedFrame,
    /// A frame set the compression flag with no negotiated decompressor.
    CompressedUnsupported,
}

/// One observed gRPC probe run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GrpcRun {
    /// Number of well-formed messages decoded before any error.
    pub messages: usize,
    /// Typed protocol facts emitted.
    pub facts: Vec<ProtocolFact>,
    /// Terminal outcome.
    pub outcome: GrpcOutcome,
}

/// Decodes a gRPC response body + trailers into a typed run.
///
/// `trailers` are the response trailers as `(name, value)` pairs; the decoder
/// reads `grpc-status` from them. A missing status defaults to UNKNOWN per the
/// gRPC spec — a typed outcome, not a panic.
pub fn decode_grpc_response(body: &[u8], trailers: &[(&str, &str)], limits: GrpcLimits) -> GrpcRun {
    let connection = ProtocolConnectionId::new(1);
    let stream = GrpcStreamId::new(1);
    let mut facts = Vec::new();
    let mut messages = 0usize;
    let mut cursor = 0usize;

    while cursor < body.len() {
        if body.len() - cursor < GRPC_PREFIX_LEN {
            facts.push(ProtocolFact::GrpcFinalStatusReceived {
                connection,
                stream,
                status: GrpcStatusCode::Internal,
            });
            return GrpcRun {
                messages,
                facts,
                outcome: GrpcOutcome::MalformedFrame,
            };
        }
        let compressed = body[cursor];
        let declared_len = u32::from_be_bytes([
            body[cursor + 1],
            body[cursor + 2],
            body[cursor + 3],
            body[cursor + 4],
        ]) as usize;
        if compressed != 0 {
            facts.push(ProtocolFact::GrpcFinalStatusReceived {
                connection,
                stream,
                status: GrpcStatusCode::Unimplemented,
            });
            return GrpcRun {
                messages,
                facts,
                outcome: GrpcOutcome::CompressedUnsupported,
            };
        }
        // Reject oversized frames by the declared length before requiring the
        // bytes, so a hostile peer cannot make the decoder buffer forever.
        if declared_len > limits.max_message_bytes {
            facts.push(ProtocolFact::GrpcFinalStatusReceived {
                connection,
                stream,
                status: GrpcStatusCode::ResourceExhausted,
            });
            return GrpcRun {
                messages,
                facts,
                outcome: GrpcOutcome::MessageTooLarge,
            };
        }
        let frame_end = cursor + GRPC_PREFIX_LEN + declared_len;
        if frame_end > body.len() {
            facts.push(ProtocolFact::GrpcFinalStatusReceived {
                connection,
                stream,
                status: GrpcStatusCode::Internal,
            });
            return GrpcRun {
                messages,
                facts,
                outcome: GrpcOutcome::MalformedFrame,
            };
        }
        messages += 1;
        cursor = frame_end;
    }

    match grpc_status_from_trailers(trailers) {
        None => {
            facts.push(ProtocolFact::GrpcFinalStatusReceived {
                connection,
                stream,
                status: GrpcStatusCode::Unknown,
            });
            GrpcRun {
                messages,
                facts,
                outcome: GrpcOutcome::MissingStatus,
            }
        }
        Some(status) => {
            facts.push(ProtocolFact::GrpcFinalStatusReceived {
                connection,
                stream,
                status,
            });
            GrpcRun {
                messages,
                facts,
                outcome: GrpcOutcome::Status(status),
            }
        }
    }
}

/// Reads and maps the `grpc-status` trailer, when present and well-formed.
fn grpc_status_from_trailers(trailers: &[(&str, &str)]) -> Option<GrpcStatusCode> {
    let raw = trailers
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case("grpc-status"))
        .map(|(_, value)| *value)?;
    let code = raw.trim().parse::<u32>().ok()?;
    status_from_code(code)
}

/// Maps a canonical gRPC status integer to a typed code.
fn status_from_code(code: u32) -> Option<GrpcStatusCode> {
    Some(match code {
        0 => GrpcStatusCode::Ok,
        1 => GrpcStatusCode::Cancelled,
        2 => GrpcStatusCode::Unknown,
        3 => GrpcStatusCode::InvalidArgument,
        4 => GrpcStatusCode::DeadlineExceeded,
        5 => GrpcStatusCode::NotFound,
        6 => GrpcStatusCode::AlreadyExists,
        7 => GrpcStatusCode::PermissionDenied,
        8 => GrpcStatusCode::ResourceExhausted,
        9 => GrpcStatusCode::FailedPrecondition,
        10 => GrpcStatusCode::Aborted,
        11 => GrpcStatusCode::OutOfRange,
        12 => GrpcStatusCode::Unimplemented,
        13 => GrpcStatusCode::Internal,
        14 => GrpcStatusCode::Unavailable,
        15 => GrpcStatusCode::DataLoss,
        16 => GrpcStatusCode::Unauthenticated,
        _ => return None,
    })
}

/// Encodes one gRPC frame: compression flag 0 + 4-byte length + payload.
pub fn encode_grpc_frame(payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(GRPC_PREFIX_LEN + payload.len());
    out.push(0);
    out.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    out.extend_from_slice(payload);
    out
}

/// Encodes a gRPC frame prefix that declares `declared_len` bytes but carries
/// none — used to drive the oversized-message check without allocating.
pub fn encode_grpc_oversized_prefix(declared_len: u32) -> Vec<u8> {
    let mut out = Vec::with_capacity(GRPC_PREFIX_LEN);
    out.push(0);
    out.extend_from_slice(&declared_len.to_be_bytes());
    out
}

/// One hermetic gRPC bad-peer probe.
#[derive(Debug, Clone)]
pub struct GrpcProbe {
    /// Stable probe name.
    pub name: &'static str,
    /// Decode limits.
    pub limits: GrpcLimits,
    /// Response body bytes.
    pub body: Vec<u8>,
    /// Response trailers.
    pub trailers: Vec<(&'static str, &'static str)>,
    /// Expected count of well-formed messages decoded before any error.
    pub expected_messages: usize,
    /// Expected typed facts, in order.
    pub expected_facts: Vec<ProtocolFact>,
    /// Expected terminal outcome.
    pub expected_outcome: GrpcOutcome,
}

impl GrpcProbe {
    /// Runs the probe.
    pub fn run(&self) -> GrpcRun {
        decode_grpc_response(&self.body, &self.trailers, self.limits)
    }

    /// Runs the probe and asserts its facts and outcome match.
    pub fn check(&self) -> Result<ProtocolChaosReport, GrpcProbeMismatch> {
        let run = self.run();
        let mut diverged = Vec::new();
        if run.facts != self.expected_facts {
            diverged.push("facts");
        }
        if run.outcome != self.expected_outcome {
            diverged.push("outcome");
        }
        if diverged.is_empty() {
            Ok(self.report(&run))
        } else {
            Err(GrpcProbeMismatch {
                name: self.name,
                diverged,
                expected_facts: self.expected_facts.clone(),
                actual_facts: run.facts,
                expected_outcome: self.expected_outcome.clone(),
                actual_outcome: run.outcome,
            })
        }
    }

    fn report(&self, run: &GrpcRun) -> ProtocolChaosReport {
        let mut report = ProtocolChaosReport::new(self.name, ProtocolChaosFamily::Grpc);
        report.peer_action = PeerAction::SentFrames;
        report.bytes_written = self.body.len();
        report.app_deliveries = run.messages;
        report.protocol_facts = run.facts.clone();
        match &run.outcome {
            GrpcOutcome::Status(status) => {
                report.close_status = Some(ProtocolCloseStatus::GrpcStatus(*status));
                report.terminal_action = if matches!(status, GrpcStatusCode::Ok) {
                    TerminalAction::DeliveredAndClosed
                } else {
                    TerminalAction::Rejected
                };
            }
            GrpcOutcome::MissingStatus => {
                report.close_status =
                    Some(ProtocolCloseStatus::GrpcStatus(GrpcStatusCode::Unknown));
                report.terminal_action = TerminalAction::Rejected;
            }
            GrpcOutcome::MessageTooLarge => {
                report.close_status = Some(ProtocolCloseStatus::GrpcStatus(
                    GrpcStatusCode::ResourceExhausted,
                ));
                report.terminal_action = TerminalAction::Rejected;
            }
            GrpcOutcome::MalformedFrame | GrpcOutcome::CompressedUnsupported => {
                report.terminal_action = TerminalAction::Rejected;
            }
        }
        report
    }

    /// Builds the derived chaos expectation.
    pub fn expectation(&self) -> ProtocolChaosExpectation {
        // Derive counters from the expected run shape.
        let run = GrpcRun {
            messages: self.expected_messages,
            facts: self.expected_facts.clone(),
            outcome: self.expected_outcome.clone(),
        };
        let report = self.report(&run);
        ProtocolChaosExpectation {
            family: ProtocolChaosFamily::Grpc,
            app_deliveries: report.app_deliveries,
            terminal_action: report.terminal_action,
            close_status: report.close_status,
            protocol_facts: self.expected_facts.clone(),
        }
    }
}

/// Typed mismatch from a gRPC probe.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GrpcProbeMismatch {
    /// Probe name.
    pub name: &'static str,
    /// Which parts diverged.
    pub diverged: Vec<&'static str>,
    /// Expected facts.
    pub expected_facts: Vec<ProtocolFact>,
    /// Observed facts.
    pub actual_facts: Vec<ProtocolFact>,
    /// Expected outcome.
    pub expected_outcome: GrpcOutcome,
    /// Observed outcome.
    pub actual_outcome: GrpcOutcome,
}

impl std::fmt::Display for GrpcProbeMismatch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        writeln!(
            f,
            "grpc probe `{}` diverged: {:?}",
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

impl std::error::Error for GrpcProbeMismatch {}

fn status_fact(status: GrpcStatusCode) -> ProtocolFact {
    ProtocolFact::GrpcFinalStatusReceived {
        connection: ProtocolConnectionId::new(1),
        stream: GrpcStreamId::new(1),
        status,
    }
}

/// The hermetic gRPC bad-peer probe suite.
pub fn grpc_probe_suite() -> Vec<GrpcProbe> {
    vec![
        // 1. Trailers missing grpc-status: client defaults to UNKNOWN.
        GrpcProbe {
            name: "grpc_missing_status",
            limits: GrpcLimits::default(),
            body: encode_grpc_frame(b"a tiny valid message"),
            trailers: vec![("content-type", "application/grpc")],
            expected_messages: 1,
            expected_facts: vec![status_fact(GrpcStatusCode::Unknown)],
            expected_outcome: GrpcOutcome::MissingStatus,
        },
        // 2. Oversized message frame: ResourceExhausted.
        GrpcProbe {
            name: "grpc_oversized_message",
            limits: GrpcLimits {
                max_message_bytes: 16,
            },
            body: encode_grpc_oversized_prefix(1_000_000),
            trailers: vec![("grpc-status", "0")],
            expected_messages: 0,
            expected_facts: vec![status_fact(GrpcStatusCode::ResourceExhausted)],
            expected_outcome: GrpcOutcome::MessageTooLarge,
        },
        // 3. Well-formed OK response (control case).
        GrpcProbe {
            name: "grpc_valid_ok",
            limits: GrpcLimits::default(),
            body: encode_grpc_frame(b"ok body"),
            trailers: vec![("grpc-status", "0"), ("grpc-message", "")],
            expected_messages: 1,
            expected_facts: vec![status_fact(GrpcStatusCode::Ok)],
            expected_outcome: GrpcOutcome::Status(GrpcStatusCode::Ok),
        },
        // 4. Truncated frame (declared length exceeds the body remaining).
        GrpcProbe {
            name: "grpc_truncated_frame",
            limits: GrpcLimits::default(),
            body: {
                let mut bytes = encode_grpc_oversized_prefix(32);
                bytes.extend_from_slice(b"short");
                bytes
            },
            trailers: vec![("grpc-status", "0")],
            expected_messages: 0,
            expected_facts: vec![status_fact(GrpcStatusCode::Internal)],
            expected_outcome: GrpcOutcome::MalformedFrame,
        },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_probe_returns_typed_outcome_and_fact() {
        for probe in grpc_probe_suite() {
            let report = probe
                .check()
                .unwrap_or_else(|mismatch| panic!("{mismatch}"));
            assert!(
                !report.protocol_facts.is_empty(),
                "{}: must emit a typed gRPC fact",
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
    fn missing_status_defaults_to_unknown() {
        let probe = grpc_probe_suite()
            .into_iter()
            .find(|p| p.name == "grpc_missing_status")
            .expect("probe present");
        let run = probe.run();
        assert_eq!(run.outcome, GrpcOutcome::MissingStatus);
        assert_eq!(run.facts, vec![status_fact(GrpcStatusCode::Unknown)]);
        // The valid message was still counted, but the status is UNKNOWN.
        assert_eq!(run.messages, 1);
    }

    #[test]
    fn oversized_message_is_resource_exhausted_not_buffered() {
        let probe = grpc_probe_suite()
            .into_iter()
            .find(|p| p.name == "grpc_oversized_message")
            .expect("probe present");
        let run = probe.run();
        assert_eq!(run.outcome, GrpcOutcome::MessageTooLarge);
        assert_eq!(
            run.facts,
            vec![status_fact(GrpcStatusCode::ResourceExhausted)]
        );
        // The decoder rejected the frame by its declared length, not by
        // allocating a million bytes.
        assert_eq!(probe.body.len(), GRPC_PREFIX_LEN);
    }

    #[test]
    fn drifted_probe_expectation_fails_closed() {
        let mut probe = grpc_probe_suite()
            .into_iter()
            .find(|p| p.name == "grpc_valid_ok")
            .expect("probe present");
        probe.expected_outcome = GrpcOutcome::MissingStatus;
        let mismatch = probe.check().expect_err("drift detected");
        assert!(mismatch.diverged.contains(&"outcome"));
    }
}
