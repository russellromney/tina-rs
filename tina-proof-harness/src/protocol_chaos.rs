//! Typed protocol-chaos report and case vocabulary.
//!
//! Every bad-peer story reduces to one typed [`ProtocolChaosReport`]: a TCP
//! transport twist, a WebSocket frame abuse, an HTTP/2 frame abuse, or a gRPC
//! framing abuse. The report is the proof artifact, not a log line. It carries
//! a stable name, the protocol family, byte tallies, what the peer did, what
//! the terminal side did, how many messages reached app code, any
//! close/reset/status, the typed protocol facts observed, the elapsed budget,
//! and any facts the harness cannot model.
//!
//! Protocol facts are stored as typed [`ProtocolFact`] values and fingerprinted
//! through the runtime's existing stable trace/fact tags — never debug strings.

use std::time::Duration;

use tina::{IsolateId, ShardId};
use tina_runtime::{
    EventId, GrpcStatusCode, Http2ResetReason, ProtocolFact, ProtocolFamily, RuntimeEvent,
    RuntimeEventKind, RuntimeFact, WebSocketCloseReason, stable_trace_hash,
};
use tina_sim::dst::{LiveReplayFact, UnsupportedLiveFact};

use crate::bad_peer::BadPeerOutcome;

/// Protocol family a chaos report belongs to.
///
/// Transport-level scenarios (`Tcp`, `Http1`, `Tls`) have no typed
/// [`ProtocolFact`] family; the protocol families map to the runtime's
/// [`ProtocolFamily`] so fact filtering stays typed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProtocolChaosFamily {
    /// Raw TCP transport (half-close, reset, slowloris, reconnect storm).
    Tcp,
    /// HTTP/1.x request framing over TCP.
    Http1,
    /// WebSocket session framing.
    WebSocket,
    /// HTTP/2 connection/stream framing.
    Http2,
    /// Native gRPC over HTTP/2.
    Grpc,
    /// TLS handshake.
    Tls,
}

impl ProtocolChaosFamily {
    /// Maps to the runtime protocol-fact family, when this family produces
    /// typed protocol facts. Transport families return `None`.
    pub const fn protocol_family(self) -> Option<ProtocolFamily> {
        match self {
            Self::WebSocket => Some(ProtocolFamily::WebSocket),
            Self::Http2 => Some(ProtocolFamily::Http2),
            Self::Grpc => Some(ProtocolFamily::Grpc),
            Self::Tcp | Self::Http1 | Self::Tls => None,
        }
    }

    /// Short stable label for report lines.
    pub const fn label(self) -> &'static str {
        match self {
            Self::Tcp => "tcp",
            Self::Http1 => "http1",
            Self::WebSocket => "websocket",
            Self::Http2 => "http2",
            Self::Grpc => "grpc",
            Self::Tls => "tls",
        }
    }
}

/// What the bad peer did on the wire.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PeerAction {
    /// Sent bytes then waited for a reply.
    SentBytes,
    /// Sent bytes then closed its write half.
    HalfClosed,
    /// Reset the connection abruptly (RST / `SO_LINGER(0)`).
    Reset,
    /// Dripped bytes slowly, one at a time (slowloris).
    Dripped,
    /// Stalled without reading or writing.
    Stalled,
    /// Connected and disconnected repeatedly.
    ReconnectStorm,
    /// Sent a sequence of protocol frames/chunks.
    SentFrames,
    /// Never completed a connection.
    NeverConnected,
}

/// What the receiving (terminal) side did in response.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TerminalAction {
    /// The server closed the connection (graceful EOF).
    ServerClosed,
    /// A reset was observed on the connection.
    Reset,
    /// The peer/connection hit a cap or timed out without a clean close.
    TimedOut,
    /// The protocol layer rejected the input and refused app delivery.
    Rejected,
    /// The protocol layer delivered a valid message to app code, then closed.
    DeliveredAndClosed,
    /// The protocol layer delivered a valid message and kept the session open.
    Delivered,
    /// No terminal action observed within the budget.
    None,
}

/// Typed close/reset/status observed at the end of a chaos run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProtocolCloseStatus {
    /// WebSocket close with an optional close code.
    WebSocketClose {
        /// Close code, when one was sent.
        code: Option<u16>,
        /// Typed close reason.
        reason: WebSocketCloseReason,
    },
    /// HTTP/2 stream reset (RST_STREAM).
    Http2Reset(Http2ResetReason),
    /// HTTP/2 connection-level GOAWAY.
    Http2GoAway,
    /// gRPC final status code.
    GrpcStatus(GrpcStatusCode),
    /// Transport reset (TCP RST / EPIPE / aborted).
    TransportReset,
    /// Transport graceful close (FIN / EOF).
    TransportClosed,
}

/// One typed protocol-chaos report.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProtocolChaosReport {
    /// Stable case name.
    pub name: &'static str,
    /// Protocol family.
    pub family: ProtocolChaosFamily,
    /// Bytes the peer wrote toward the server.
    pub bytes_written: usize,
    /// Bytes the peer read back from the server.
    pub bytes_read: usize,
    /// What the peer did.
    pub peer_action: PeerAction,
    /// What the terminal side did.
    pub terminal_action: TerminalAction,
    /// Number of valid messages delivered to app code.
    pub app_deliveries: usize,
    /// Close/reset/status, when any was observed.
    pub close_status: Option<ProtocolCloseStatus>,
    /// Typed protocol facts observed during the run, in order.
    pub protocol_facts: Vec<ProtocolFact>,
    /// Wall-clock (or simulated) budget consumed.
    pub elapsed: Duration,
    /// Facts the harness saw but cannot model as typed protocol facts.
    pub unsupported_facts: Vec<UnsupportedLiveFact>,
}

impl ProtocolChaosReport {
    /// Builds a report for a typed protocol run (WebSocket/HTTP2/gRPC).
    pub fn new(name: &'static str, family: ProtocolChaosFamily) -> Self {
        Self {
            name,
            family,
            bytes_written: 0,
            bytes_read: 0,
            peer_action: PeerAction::SentFrames,
            terminal_action: TerminalAction::None,
            app_deliveries: 0,
            close_status: None,
            protocol_facts: Vec::new(),
            elapsed: Duration::ZERO,
            unsupported_facts: Vec::new(),
        }
    }

    /// Folds a transport-level [`BadPeerOutcome`] into a typed report so the
    /// existing TCP/HTTP/TLS bad-peer scenarios still emit one report row.
    ///
    /// The caller names the [`PeerAction`] because [`BadPeerOutcome`] does not
    /// carry the scenario shape. Transport runs carry no protocol facts and no
    /// app deliveries; their close status comes from the observed reset/EOF.
    pub fn from_bad_peer(
        name: &'static str,
        family: ProtocolChaosFamily,
        peer_action: PeerAction,
        outcome: &BadPeerOutcome,
    ) -> Self {
        let (terminal_action, close_status) = if outcome.peer_reset {
            (
                TerminalAction::Reset,
                Some(ProtocolCloseStatus::TransportReset),
            )
        } else if outcome.server_closed {
            (
                TerminalAction::ServerClosed,
                Some(ProtocolCloseStatus::TransportClosed),
            )
        } else if !outcome.connected {
            (TerminalAction::None, None)
        } else {
            (TerminalAction::TimedOut, None)
        };
        Self {
            name,
            family,
            bytes_written: outcome.bytes_sent,
            bytes_read: outcome.bytes_read,
            peer_action,
            terminal_action,
            app_deliveries: 0,
            close_status,
            protocol_facts: Vec::new(),
            elapsed: Duration::from_millis(outcome.elapsed_ms),
            unsupported_facts: Vec::new(),
        }
    }

    /// Records one observed protocol fact.
    pub fn push_fact(&mut self, fact: ProtocolFact) {
        self.protocol_facts.push(fact);
    }

    /// Records one unsupported fact (harness fail-closed honesty).
    pub fn push_unsupported(&mut self, what: impl Into<String>, reason: impl Into<String>) {
        self.unsupported_facts
            .push(UnsupportedLiveFact::new(what, reason));
    }

    /// Number of typed protocol facts in this report.
    pub fn fact_count(&self) -> usize {
        self.protocol_facts.len()
    }

    /// Returns the observed protocol facts as [`LiveReplayFact::Protocol`]
    /// entries, ready to attach to a `LiveReplayCapture` so a live chaos run
    /// can be saved beside capacity facts.
    pub fn live_replay_facts(&self) -> Vec<LiveReplayFact> {
        self.protocol_facts
            .iter()
            .copied()
            .map(LiveReplayFact::protocol)
            .collect()
    }

    /// Stable fingerprint of the observed protocol-fact sequence.
    pub fn protocol_fact_hash(&self) -> u64 {
        protocol_fact_sequence_hash(&self.protocol_facts)
    }

    /// One grep-friendly line for `--nocapture` proof output.
    pub fn summary_line(&self) -> String {
        format!(
            "protocol_chaos name={} family={} peer={:?} terminal={:?} bytes_w={} bytes_r={} app_deliveries={} facts={} fact_hash=0x{:016x} close={:?} unsupported={} elapsed_ms={}",
            self.name,
            self.family.label(),
            self.peer_action,
            self.terminal_action,
            self.bytes_written,
            self.bytes_read,
            self.app_deliveries,
            self.fact_count(),
            self.protocol_fact_hash(),
            self.close_status,
            self.unsupported_facts.len(),
            self.elapsed.as_millis(),
        )
    }
}

/// Stable fingerprint of a typed protocol-fact sequence.
///
/// Hashes the typed [`ProtocolFact`] values through the runtime's existing
/// stable trace/fact tags — never debug strings — by wrapping each fact in a
/// synthetic `FactObserved` event with a fixed shard/isolate and a sequential
/// id, then folding with [`stable_trace_hash`]. Two equal fact sequences hash
/// equal; reordering or editing any fact changes the hash.
pub fn protocol_fact_sequence_hash(facts: &[ProtocolFact]) -> u64 {
    let events: Vec<RuntimeEvent> = facts
        .iter()
        .enumerate()
        .map(|(index, fact)| {
            RuntimeEvent::new(
                EventId::new(index as u64),
                None,
                ShardId::new(0),
                IsolateId::new(0),
                RuntimeEventKind::FactObserved {
                    fact: RuntimeFact::Protocol(*fact),
                },
            )
        })
        .collect();
    stable_trace_hash(events.iter())
}

/// What a [`ProtocolChaosReport`] must show for a saved chaos case to pass.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProtocolChaosExpectation {
    /// Expected protocol family.
    pub family: ProtocolChaosFamily,
    /// Expected count of valid messages delivered to app code.
    pub app_deliveries: usize,
    /// Expected terminal action.
    pub terminal_action: TerminalAction,
    /// Expected close/reset/status.
    pub close_status: Option<ProtocolCloseStatus>,
    /// Expected protocol facts, in order.
    pub protocol_facts: Vec<ProtocolFact>,
}

impl ProtocolChaosExpectation {
    /// Checks a report against this expectation, returning every diverged
    /// field. An empty result means the report matched.
    pub fn check(&self, report: &ProtocolChaosReport) -> Result<(), Box<ProtocolChaosMismatch>> {
        let mut diverged = Vec::new();
        if report.family != self.family {
            diverged.push(ChaosField::Family);
        }
        if report.app_deliveries != self.app_deliveries {
            diverged.push(ChaosField::AppDeliveries);
        }
        if report.terminal_action != self.terminal_action {
            diverged.push(ChaosField::TerminalAction);
        }
        if report.close_status != self.close_status {
            diverged.push(ChaosField::CloseStatus);
        }
        // Compare facts as an ordered sequence via the stable typed fingerprint
        // so a debug-string drift never masks a fact change, and vice versa.
        if protocol_fact_sequence_hash(&report.protocol_facts)
            != protocol_fact_sequence_hash(&self.protocol_facts)
        {
            diverged.push(ChaosField::ProtocolFacts);
        }
        if diverged.is_empty() {
            Ok(())
        } else {
            Err(Box::new(ProtocolChaosMismatch {
                name: report.name,
                diverged,
                expected_app_deliveries: self.app_deliveries,
                actual_app_deliveries: report.app_deliveries,
                expected_terminal: self.terminal_action,
                actual_terminal: report.terminal_action,
                expected_close: self.close_status,
                actual_close: report.close_status,
                expected_facts: self.protocol_facts.clone(),
                actual_facts: report.protocol_facts.clone(),
            }))
        }
    }
}

/// A named chaos case: a stable name plus its expectation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProtocolChaosCase {
    /// Stable case name.
    pub name: &'static str,
    /// Expected report shape.
    pub expectation: ProtocolChaosExpectation,
}

impl ProtocolChaosCase {
    /// Builds a chaos case.
    pub fn new(name: &'static str, expectation: ProtocolChaosExpectation) -> Self {
        Self { name, expectation }
    }

    /// Checks a report against this case, also verifying the report name.
    pub fn check(&self, report: &ProtocolChaosReport) -> Result<(), Box<ProtocolChaosMismatch>> {
        if report.name != self.name {
            return Err(Box::new(ProtocolChaosMismatch {
                name: self.name,
                diverged: vec![ChaosField::Name],
                expected_app_deliveries: self.expectation.app_deliveries,
                actual_app_deliveries: report.app_deliveries,
                expected_terminal: self.expectation.terminal_action,
                actual_terminal: report.terminal_action,
                expected_close: self.expectation.close_status,
                actual_close: report.close_status,
                expected_facts: self.expectation.protocol_facts.clone(),
                actual_facts: report.protocol_facts.clone(),
            }));
        }
        self.expectation.check(report)
    }
}

/// Which report field diverged from a chaos expectation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChaosField {
    /// Case name.
    Name,
    /// Protocol family.
    Family,
    /// App delivery count.
    AppDeliveries,
    /// Terminal action.
    TerminalAction,
    /// Close/reset/status.
    CloseStatus,
    /// Protocol facts.
    ProtocolFacts,
}

/// Typed mismatch between a report and an expectation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProtocolChaosMismatch {
    /// Case name.
    pub name: &'static str,
    /// Diverged fields.
    pub diverged: Vec<ChaosField>,
    /// Expected app delivery count.
    pub expected_app_deliveries: usize,
    /// Observed app delivery count.
    pub actual_app_deliveries: usize,
    /// Expected terminal action.
    pub expected_terminal: TerminalAction,
    /// Observed terminal action.
    pub actual_terminal: TerminalAction,
    /// Expected close status.
    pub expected_close: Option<ProtocolCloseStatus>,
    /// Observed close status.
    pub actual_close: Option<ProtocolCloseStatus>,
    /// Expected protocol facts.
    pub expected_facts: Vec<ProtocolFact>,
    /// Observed protocol facts.
    pub actual_facts: Vec<ProtocolFact>,
}

impl ProtocolChaosMismatch {
    /// Returns true when `field` diverged.
    pub fn includes(&self, field: ChaosField) -> bool {
        self.diverged.contains(&field)
    }
}

impl std::fmt::Display for ProtocolChaosMismatch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        writeln!(
            f,
            "protocol chaos case `{}` diverged: {:?}",
            self.name, self.diverged
        )?;
        if self.includes(ChaosField::AppDeliveries) {
            writeln!(
                f,
                "  app_deliveries: expected {}, got {}",
                self.expected_app_deliveries, self.actual_app_deliveries
            )?;
        }
        if self.includes(ChaosField::TerminalAction) {
            writeln!(
                f,
                "  terminal: expected {:?}, got {:?}",
                self.expected_terminal, self.actual_terminal
            )?;
        }
        if self.includes(ChaosField::CloseStatus) {
            writeln!(
                f,
                "  close: expected {:?}, got {:?}",
                self.expected_close, self.actual_close
            )?;
        }
        if self.includes(ChaosField::ProtocolFacts) {
            writeln!(f, "  protocol facts:")?;
            writeln!(f, "    expected:")?;
            for fact in &self.expected_facts {
                writeln!(f, "      - {fact:?}")?;
            }
            writeln!(f, "    actual:")?;
            for fact in &self.actual_facts {
                writeln!(f, "      - {fact:?}")?;
            }
        }
        Ok(())
    }
}

impl std::error::Error for ProtocolChaosMismatch {}

#[cfg(test)]
mod tests {
    use super::*;
    use tina_runtime::{ProtocolConnectionId, WebSocketSessionId};

    fn ws_fact(code: u16) -> ProtocolFact {
        ProtocolFact::WebSocketSessionClosed {
            session: WebSocketSessionId::new(1),
            reason: WebSocketCloseReason::ProtocolError,
            code: Some(code),
        }
    }

    #[test]
    fn fact_hash_is_stable_typed_and_order_sensitive() {
        let a = ProtocolFact::Http2StreamReset {
            connection: ProtocolConnectionId::new(1),
            stream: tina_runtime::Http2StreamId::new(1),
            direction: tina_runtime::ProtocolDirection::Inbound,
            reason: Http2ResetReason::FrameSizeError,
        };
        let b = ws_fact(1002);
        assert_eq!(
            protocol_fact_sequence_hash(&[a, b]),
            protocol_fact_sequence_hash(&[a, b])
        );
        assert_ne!(
            protocol_fact_sequence_hash(&[a, b]),
            protocol_fact_sequence_hash(&[b, a])
        );
        assert_ne!(
            protocol_fact_sequence_hash(&[a]),
            protocol_fact_sequence_hash(&[a, b])
        );
    }

    #[test]
    fn from_bad_peer_maps_reset_and_close() {
        let mut outcome = BadPeerOutcome {
            label: "x",
            connected: true,
            bytes_sent: 10,
            bytes_read: 0,
            reply_prefix: String::new(),
            server_closed: false,
            peer_reset: true,
            elapsed_ms: 5,
            error: None,
            connects_failed: 0,
            connection_errors: Vec::new(),
            connects_ok: 1,
        };
        let report = ProtocolChaosReport::from_bad_peer(
            "reset",
            ProtocolChaosFamily::Tcp,
            PeerAction::Reset,
            &outcome,
        );
        assert_eq!(report.terminal_action, TerminalAction::Reset);
        assert_eq!(
            report.close_status,
            Some(ProtocolCloseStatus::TransportReset)
        );
        assert_eq!(report.bytes_written, 10);
        assert!(report.protocol_facts.is_empty());

        outcome.peer_reset = false;
        outcome.server_closed = true;
        let report = ProtocolChaosReport::from_bad_peer(
            "halfclose",
            ProtocolChaosFamily::Http1,
            PeerAction::HalfClosed,
            &outcome,
        );
        assert_eq!(report.terminal_action, TerminalAction::ServerClosed);
        assert_eq!(
            report.close_status,
            Some(ProtocolCloseStatus::TransportClosed)
        );
    }

    #[test]
    fn expectation_check_reports_each_diverged_field() {
        let report = {
            let mut r = ProtocolChaosReport::new("ws_case", ProtocolChaosFamily::WebSocket);
            r.app_deliveries = 1;
            r.terminal_action = TerminalAction::Delivered;
            r.push_fact(ws_fact(1000));
            r
        };
        let exact = ProtocolChaosExpectation {
            family: ProtocolChaosFamily::WebSocket,
            app_deliveries: 1,
            terminal_action: TerminalAction::Delivered,
            close_status: None,
            protocol_facts: vec![ws_fact(1000)],
        };
        ProtocolChaosCase::new("ws_case", exact)
            .check(&report)
            .expect("matching report");

        let drifted = ProtocolChaosExpectation {
            family: ProtocolChaosFamily::WebSocket,
            app_deliveries: 0,
            terminal_action: TerminalAction::Rejected,
            close_status: Some(ProtocolCloseStatus::WebSocketClose {
                code: Some(1002),
                reason: WebSocketCloseReason::ProtocolError,
            }),
            protocol_facts: vec![ws_fact(1002)],
        };
        let mismatch = drifted.check(&report).expect_err("drift");
        assert!(mismatch.includes(ChaosField::AppDeliveries));
        assert!(mismatch.includes(ChaosField::TerminalAction));
        assert!(mismatch.includes(ChaosField::CloseStatus));
        assert!(mismatch.includes(ChaosField::ProtocolFacts));
    }
}
