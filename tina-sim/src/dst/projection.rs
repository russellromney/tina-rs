//! Trace projection, replay-config hashing, and unsupported-fact
//! vocabulary for DST.
//!
//! Owns [`TraceShape`], the typed `TraceProjection` projector with the
//! fail-closed [`RuntimeEventKindName`] alphabet, projection error
//! types, the stable `replay_config_hash`/`encode_*` helpers, and the
//! `ProtocolReplayMismatch` vocabulary.

use std::fmt::Write;

use tina_runtime::{
    EventId, ProtocolFamily, RuntimeEvent, RuntimeEventKind, RuntimeFact, stable_trace_hash,
};

use super::{ReplayConfig, ReplayReport};
use crate::{FaultConfig, SimulatorConfig};

/// Stable, copyable shape of a trace run.
///
/// A `TraceShape` is deliberately smaller than a trace: it records the
/// number of typed events and the canonical [`stable_trace_hash`]. That is
/// enough to tell whether a simulator replay is still the same story while
/// keeping live captures and bug reports compact.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TraceShape {
    /// Observed event count.
    pub event_count: usize,
    /// Observed `stable_trace_hash`.
    pub trace_hash: u64,
}

impl TraceShape {
    /// Builds a trace shape from runtime events.
    pub fn from_events(events: &[RuntimeEvent]) -> Self {
        Self {
            event_count: events.len(),
            trace_hash: stable_trace_hash(events.iter()),
        }
    }

    /// Builds a trace shape from a replay report.
    pub const fn from_report<Output>(report: &ReplayReport<Output>) -> Self {
        Self {
            event_count: report.event_count,
            trace_hash: report.trace_hash,
        }
    }
}

/// Runtime event kind names used by live-replay projection specs.
///
/// A projection must name every event kind it keeps or ignores. That makes
/// live-vs-sim comparison fail closed when a trace contains an event kind the
/// projection author did not account for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum RuntimeEventKindName {
    /// [`RuntimeEventKind::MailboxAccepted`].
    MailboxAccepted,
    /// [`RuntimeEventKind::HandlerStarted`].
    HandlerStarted,
    /// [`RuntimeEventKind::HandlerPanicked`].
    HandlerPanicked,
    /// [`RuntimeEventKind::HandlerReportedFailure`].
    HandlerReportedFailure,
    /// [`RuntimeEventKind::HandlerFinished`].
    HandlerFinished,
    /// [`RuntimeEventKind::EffectObserved`].
    EffectObserved,
    /// [`RuntimeEventKind::SendDispatchAttempted`].
    SendDispatchAttempted,
    /// [`RuntimeEventKind::SendAccepted`].
    SendAccepted,
    /// [`RuntimeEventKind::SendRejected`].
    SendRejected,
    /// [`RuntimeEventKind::Spawned`].
    Spawned,
    /// [`RuntimeEventKind::SupervisorRestartTriggered`].
    SupervisorRestartTriggered,
    /// [`RuntimeEventKind::SupervisorRestartRejected`].
    SupervisorRestartRejected,
    /// [`RuntimeEventKind::RestartChildAttempted`].
    RestartChildAttempted,
    /// [`RuntimeEventKind::RestartChildSkipped`].
    RestartChildSkipped,
    /// [`RuntimeEventKind::RestartChildCompleted`].
    RestartChildCompleted,
    /// [`RuntimeEventKind::ChildStopped`].
    ChildStopped,
    /// [`RuntimeEventKind::IsolateStopped`].
    IsolateStopped,
    /// [`RuntimeEventKind::MessageAbandoned`].
    MessageAbandoned,
    /// [`RuntimeEventKind::CallDispatchAttempted`].
    CallDispatchAttempted,
    /// [`RuntimeEventKind::CallRejected`].
    CallRejected,
    /// [`RuntimeEventKind::CallCompleted`].
    CallCompleted,
    /// [`RuntimeEventKind::CallFailed`].
    CallFailed,
    /// [`RuntimeEventKind::CallCompletionRejected`].
    CallCompletionRejected,
    /// [`RuntimeEventKind::CallReplyRejected`].
    CallReplyRejected,
    /// [`RuntimeEventKind::CallReplyAbandoned`].
    CallReplyAbandoned,
    /// [`RuntimeEventKind::CallCancelled`].
    CallCancelled,
    /// [`RuntimeEventKind::SnapshotCommitted`].
    SnapshotCommitted,
    /// [`RuntimeEventKind::SnapshotCommitFailed`].
    SnapshotCommitFailed,
    /// [`RuntimeEventKind::JournalAppended`].
    JournalAppended,
    /// [`RuntimeEventKind::JournalAppendFailed`].
    JournalAppendFailed,
    /// [`RuntimeEventKind::RecoveryStarted`].
    RecoveryStarted,
    /// [`RuntimeEventKind::RecoveryFinished`].
    RecoveryFinished,
    /// [`RuntimeEventKind::RecoveryFailed`].
    RecoveryFailed,
    /// [`RuntimeEventKind::DeferredReplyCaptured`].
    DeferredReplyCaptured,
    /// [`RuntimeEventKind::DeferredReplySent`].
    DeferredReplySent,
    /// [`RuntimeEventKind::DeferredReplyRejected`].
    DeferredReplyRejected,
    /// [`RuntimeEventKind::DeferredReplyDropped`].
    DeferredReplyDropped,
    /// [`RuntimeEventKind::FactObserved`].
    FactObserved,
}

fn runtime_event_kind_name(kind: RuntimeEventKind) -> Option<RuntimeEventKindName> {
    Some(match kind {
        RuntimeEventKind::MailboxAccepted => RuntimeEventKindName::MailboxAccepted,
        RuntimeEventKind::HandlerStarted => RuntimeEventKindName::HandlerStarted,
        RuntimeEventKind::HandlerPanicked => RuntimeEventKindName::HandlerPanicked,
        RuntimeEventKind::HandlerReportedFailure => RuntimeEventKindName::HandlerReportedFailure,
        RuntimeEventKind::HandlerFinished { .. } => RuntimeEventKindName::HandlerFinished,
        RuntimeEventKind::EffectObserved { .. } => RuntimeEventKindName::EffectObserved,
        RuntimeEventKind::SendDispatchAttempted { .. } => {
            RuntimeEventKindName::SendDispatchAttempted
        }
        RuntimeEventKind::SendAccepted { .. } => RuntimeEventKindName::SendAccepted,
        RuntimeEventKind::SendRejected { .. } => RuntimeEventKindName::SendRejected,
        RuntimeEventKind::Spawned { .. } => RuntimeEventKindName::Spawned,
        RuntimeEventKind::SupervisorRestartTriggered { .. } => {
            RuntimeEventKindName::SupervisorRestartTriggered
        }
        RuntimeEventKind::SupervisorRestartRejected { .. } => {
            RuntimeEventKindName::SupervisorRestartRejected
        }
        RuntimeEventKind::RestartChildAttempted { .. } => {
            RuntimeEventKindName::RestartChildAttempted
        }
        RuntimeEventKind::RestartChildSkipped { .. } => RuntimeEventKindName::RestartChildSkipped,
        RuntimeEventKind::RestartChildCompleted { .. } => {
            RuntimeEventKindName::RestartChildCompleted
        }
        RuntimeEventKind::ChildStopped { .. } => RuntimeEventKindName::ChildStopped,
        RuntimeEventKind::IsolateStopped => RuntimeEventKindName::IsolateStopped,
        RuntimeEventKind::MessageAbandoned => RuntimeEventKindName::MessageAbandoned,
        RuntimeEventKind::CallDispatchAttempted { .. } => {
            RuntimeEventKindName::CallDispatchAttempted
        }
        RuntimeEventKind::CallRejected { .. } => RuntimeEventKindName::CallRejected,
        RuntimeEventKind::CallCompleted { .. } => RuntimeEventKindName::CallCompleted,
        RuntimeEventKind::CallFailed { .. } => RuntimeEventKindName::CallFailed,
        RuntimeEventKind::CallCompletionRejected { .. } => {
            RuntimeEventKindName::CallCompletionRejected
        }
        RuntimeEventKind::CallReplyRejected { .. } => RuntimeEventKindName::CallReplyRejected,
        RuntimeEventKind::CallReplyAbandoned { .. } => RuntimeEventKindName::CallReplyAbandoned,
        RuntimeEventKind::CallCancelled { .. } => RuntimeEventKindName::CallCancelled,
        RuntimeEventKind::SnapshotCommitted => RuntimeEventKindName::SnapshotCommitted,
        RuntimeEventKind::SnapshotCommitFailed { .. } => RuntimeEventKindName::SnapshotCommitFailed,
        RuntimeEventKind::JournalAppended { .. } => RuntimeEventKindName::JournalAppended,
        RuntimeEventKind::JournalAppendFailed { .. } => RuntimeEventKindName::JournalAppendFailed,
        RuntimeEventKind::RecoveryStarted => RuntimeEventKindName::RecoveryStarted,
        RuntimeEventKind::RecoveryFinished => RuntimeEventKindName::RecoveryFinished,
        RuntimeEventKind::RecoveryFailed { .. } => RuntimeEventKindName::RecoveryFailed,
        RuntimeEventKind::DeferredReplyCaptured { .. } => {
            RuntimeEventKindName::DeferredReplyCaptured
        }
        RuntimeEventKind::DeferredReplySent { .. } => RuntimeEventKindName::DeferredReplySent,
        RuntimeEventKind::DeferredReplyRejected { .. } => {
            RuntimeEventKindName::DeferredReplyRejected
        }
        RuntimeEventKind::DeferredReplyDropped { .. } => RuntimeEventKindName::DeferredReplyDropped,
        RuntimeEventKind::FactObserved { .. } => RuntimeEventKindName::FactObserved,
    })
}

/// Visible live-replay projection contract.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum TraceProjection {
    /// Compare the exact trace shape.
    #[default]
    Exact,
    /// Compare only named included kinds; named ignored kinds are stripped.
    ///
    /// Every event kind in the trace must appear in `included` or `ignored`.
    /// Anything unnamed fails closed as [`TraceProjectionError`].
    ///
    /// `family_filter` narrows `FactObserved` events further:
    ///
    /// - `None` keeps every `FactObserved` event (current behaviour);
    /// - `Some(family)` keeps only `FactObserved` events whose
    ///   `RuntimeFact::Protocol(fact).family()` matches; non-matching
    ///   facts are dropped silently, the way `ignored` kinds are. Unknown
    ///   runtime event kinds still fail closed.
    Projected {
        /// Event kinds that remain in the projected trace.
        included: Vec<RuntimeEventKindName>,
        /// Event kinds intentionally ignored by this projection.
        ignored: Vec<RuntimeEventKindName>,
        /// Optional protocol-family narrowing for `FactObserved` events.
        family_filter: Option<ProtocolFamily>,
    },
}

impl TraceProjection {
    /// Returns the event kinds this projection intentionally ignores.
    pub fn ignored(&self) -> &[RuntimeEventKindName] {
        match self {
            Self::Exact => &[],
            Self::Projected { ignored, .. } => ignored,
        }
    }

    /// Returns the protocol-family narrowing applied to `FactObserved`
    /// events, if any.
    pub fn family_filter(&self) -> Option<ProtocolFamily> {
        match self {
            Self::Exact => None,
            Self::Projected { family_filter, .. } => *family_filter,
        }
    }

    /// Returns a projection that keeps every
    /// [`RuntimeEventKindName::FactObserved`] event, regardless of
    /// protocol family.
    ///
    /// All other event kinds are explicitly listed in `ignored` so unknown
    /// event kinds still fail closed.
    pub fn protocol_facts() -> Self {
        Self::Projected {
            included: vec![RuntimeEventKindName::FactObserved],
            ignored: every_kind_except(&[RuntimeEventKindName::FactObserved]),
            family_filter: None,
        }
    }

    /// Returns a projection that keeps only `FactObserved` events whose
    /// fact belongs to the given protocol family.
    ///
    /// Non-matching `FactObserved` events are dropped silently the way
    /// `ignored` event kinds are. Unknown runtime event kinds still fail
    /// closed.
    ///
    /// The family is read from
    /// `RuntimeFact::Protocol(fact).family()`; debug-string parsing is
    /// not used.
    pub fn protocol_family(family: ProtocolFamily) -> Self {
        Self::Projected {
            included: vec![RuntimeEventKindName::FactObserved],
            ignored: every_kind_except(&[RuntimeEventKindName::FactObserved]),
            family_filter: Some(family),
        }
    }

    /// Returns a projection that keeps only HTTP/2 protocol facts.
    ///
    /// Equivalent to [`Self::protocol_family`] with
    /// [`ProtocolFamily::Http2`]. Use this at call sites that want to
    /// express HTTP/2-specific intent.
    pub fn http2_streams() -> Self {
        Self::protocol_family(ProtocolFamily::Http2)
    }

    /// Returns a projection that keeps only WebSocket protocol facts.
    ///
    /// Equivalent to [`Self::protocol_family`] with
    /// [`ProtocolFamily::WebSocket`].
    pub fn websocket_sessions() -> Self {
        Self::protocol_family(ProtocolFamily::WebSocket)
    }

    /// Returns a projection that keeps only gRPC protocol facts.
    ///
    /// Equivalent to [`Self::protocol_family`] with
    /// [`ProtocolFamily::Grpc`].
    pub fn grpc_status() -> Self {
        Self::protocol_family(ProtocolFamily::Grpc)
    }
}

/// Returns every [`RuntimeEventKindName`] value other than the ones in
/// `keep`.
fn every_kind_except(keep: &[RuntimeEventKindName]) -> Vec<RuntimeEventKindName> {
    use RuntimeEventKindName as N;
    let all = [
        N::MailboxAccepted,
        N::HandlerStarted,
        N::HandlerPanicked,
        N::HandlerReportedFailure,
        N::HandlerFinished,
        N::EffectObserved,
        N::SendDispatchAttempted,
        N::SendAccepted,
        N::SendRejected,
        N::Spawned,
        N::SupervisorRestartTriggered,
        N::SupervisorRestartRejected,
        N::RestartChildAttempted,
        N::RestartChildSkipped,
        N::RestartChildCompleted,
        N::ChildStopped,
        N::IsolateStopped,
        N::MessageAbandoned,
        N::CallDispatchAttempted,
        N::CallRejected,
        N::CallCompleted,
        N::CallFailed,
        N::CallCompletionRejected,
        N::CallReplyRejected,
        N::CallReplyAbandoned,
        N::CallCancelled,
        N::SnapshotCommitted,
        N::SnapshotCommitFailed,
        N::JournalAppended,
        N::JournalAppendFailed,
        N::RecoveryStarted,
        N::RecoveryFinished,
        N::RecoveryFailed,
        N::DeferredReplyCaptured,
        N::DeferredReplySent,
        N::DeferredReplyRejected,
        N::DeferredReplyDropped,
        N::FactObserved,
    ];
    all.into_iter()
        .filter(|kind| !keep.contains(kind))
        .collect()
}

/// Why a trace could not be projected.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TraceProjectionError {
    /// Event id closest to the unsupported fact.
    pub event_id: EventId,
    /// The event kind when Tina knows how to name it.
    pub kind: Option<RuntimeEventKindName>,
    /// Human-readable reason.
    pub reason: String,
}

impl std::fmt::Display for TraceProjectionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "trace projection unsupported at event {:?}: {}",
            self.event_id, self.reason
        )
    }
}

impl std::error::Error for TraceProjectionError {}

/// Typed outcome of a protocol-fact replay check against a sim trace.
///
/// Used by saved-case verification to be honest about which protocol facts
/// the simulator can execute and which it can only observe in live traces.
/// A live-only fact does not fail replay; it just changes the outcome to
/// [`Self::UnsupportedProtocolFact`] so the caller can record the gap.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProtocolReplayMismatch {
    /// The live trace contains a protocol fact the simulator does not produce
    /// because the underlying live-only physics (real socket, kernel timing)
    /// is not modeled in this simulator.
    UnsupportedProtocolFact {
        /// The live fact that has no simulator counterpart.
        fact: tina_runtime::ProtocolFact,
        /// Human-readable reason for the gap.
        reason: String,
    },
    /// A protocol fact was present in the live trace and missing from the sim
    /// trace (or vice versa) but is otherwise replayable. Typed separately
    /// from [`Self::UnsupportedProtocolFact`] so callers can tell a missing
    /// fact (real bug) from a gap in simulator coverage.
    Diverged {
        /// The protocol fact whose live/sim presence disagreed.
        fact: tina_runtime::ProtocolFact,
        /// Whether the fact was present in the live trace but absent in sim.
        live_only: bool,
    },
}

impl std::fmt::Display for ProtocolReplayMismatch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnsupportedProtocolFact { fact, reason } => write!(
                f,
                "unsupported protocol fact in sim replay: {fact:?}: {reason}"
            ),
            Self::Diverged { fact, live_only } => write!(
                f,
                "protocol fact diverged between live and sim ({}): {fact:?}",
                if *live_only { "live-only" } else { "sim-only" }
            ),
        }
    }
}

impl std::error::Error for ProtocolReplayMismatch {}

/// Applies a visible projection and returns the resulting trace shape.
pub fn project_trace_shape(
    events: &[RuntimeEvent],
    projection: &TraceProjection,
) -> Result<TraceShape, TraceProjectionError> {
    match projection {
        TraceProjection::Exact => Ok(TraceShape::from_events(events)),
        TraceProjection::Projected {
            included,
            ignored,
            family_filter,
        } => {
            let mut projected = Vec::new();
            for event in events {
                let Some(kind) = runtime_event_kind_name(event.kind()) else {
                    return Err(TraceProjectionError {
                        event_id: event.id(),
                        kind: None,
                        reason: "runtime event kind is unknown to this projection".into(),
                    });
                };
                if ignored.contains(&kind) {
                    continue;
                }
                if included.contains(&kind) {
                    // Family narrowing: if the projection asked for one
                    // protocol family, drop `FactObserved` events whose
                    // fact does not match. Non-matching facts behave the
                    // same as `ignored` kinds (silently skipped, no
                    // fail-closed). Unknown event kinds still fail closed
                    // through the check above.
                    if kind == RuntimeEventKindName::FactObserved {
                        if let Some(family) = family_filter {
                            if let RuntimeEventKind::FactObserved { fact } = event.kind() {
                                match fact {
                                    RuntimeFact::Protocol(protocol_fact) => {
                                        if protocol_fact.family() != *family {
                                            continue;
                                        }
                                    }
                                    // Future top-level RuntimeFact families
                                    // are silently skipped by family filters;
                                    // unknown event kinds still fail closed
                                    // through the check above.
                                    _ => continue,
                                }
                            }
                        }
                    }
                    projected.push(event);
                    continue;
                }
                return Err(TraceProjectionError {
                    event_id: event.id(),
                    kind: Some(kind),
                    reason: format!("event kind {kind:?} was not named as included or ignored"),
                });
            }
            Ok(TraceShape {
                event_count: projected.len(),
                trace_hash: stable_trace_hash(projected),
            })
        }
    }
}

fn stable_text_hash(text: &str) -> u64 {
    // FNV-1a over UTF-8 bytes. This hash is for diagnostics and saved-case
    // drift checks, not for trace identity; traces still use stable_trace_hash.
    let mut hash = 0xcbf2_9ce4_8422_2325_u64;
    for byte in text.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

/// Diagnostic fingerprint of visible replay config.
///
/// This is intentionally separate from [`stable_trace_hash`]. The trace hash
/// is the replay identity. The config hash is a small bug-report aid so a
/// changed mailbox cap or fault knob is called out before anyone stares at
/// event hashes.
pub fn replay_config_hash(config: &ReplayConfig) -> u64 {
    let mut encoded = String::new();
    encode_replay_config(&mut encoded, config);
    stable_text_hash(&encoded)
}

fn encode_string(out: &mut String, value: &str) {
    let _ = write!(out, "{}:", value.len());
    out.push_str(value);
}

fn encode_bytes(out: &mut String, bytes: &[u8]) {
    let _ = write!(out, "{}:", bytes.len());
    for byte in bytes {
        let _ = write!(out, "{byte:02x}");
    }
}

fn encode_option_usize(out: &mut String, value: Option<usize>) {
    match value {
        Some(value) => {
            let _ = write!(out, "some({value})");
        }
        None => out.push_str("none"),
    }
}

fn encode_option_u64(out: &mut String, value: Option<u64>) {
    match value {
        Some(value) => {
            let _ = write!(out, "some({value})");
        }
        None => out.push_str("none"),
    }
}

fn encode_option_i32(out: &mut String, value: Option<i32>) {
    match value {
        Some(value) => {
            let _ = write!(out, "some({value})");
        }
        None => out.push_str("none"),
    }
}

fn encode_socket_addr(out: &mut String, addr: std::net::SocketAddr) {
    match addr {
        std::net::SocketAddr::V4(addr) => {
            let _ = write!(out, "v4({}:{})", addr.ip(), addr.port());
        }
        std::net::SocketAddr::V6(addr) => {
            let _ = write!(
                out,
                "v6({}:{}:{}:{})",
                addr.ip(),
                addr.port(),
                addr.flowinfo(),
                addr.scope_id()
            );
        }
    }
}

fn encode_replay_config(out: &mut String, config: &ReplayConfig) {
    out.push_str("ReplayConfig/v1{sim=");
    encode_simulator_config(out, &config.simulator);
    out.push_str(";mailboxes=[");
    for (role, capacity) in &config.mailboxes {
        encode_string(out, role);
        let _ = write!(out, "={capacity};");
    }
    out.push_str("]}");
}

fn encode_simulator_config(out: &mut String, config: &SimulatorConfig) {
    let _ = write!(out, "{{seed={};faults=", config.seed);
    encode_fault_config(out, config.faults);
    out.push_str(";tcp=");
    encode_tcp_config(out, &config.tcp);
    out.push_str(";udp=");
    encode_udp_config(out, &config.udp);
    out.push_str(";dns=");
    encode_dns_config(out, &config.dns);
    out.push_str(";tls=");
    encode_tls_config(out, &config.tls);
    out.push_str(";signal=");
    encode_signal_config(out, &config.signal);
    out.push_str(";process=");
    encode_process_config(out, &config.process);
    out.push_str(";storage=");
    encode_storage_config(out, config.storage);
    out.push('}');
}

fn encode_fault_config(out: &mut String, faults: FaultConfig) {
    out.push_str("{local=");
    match faults.local_send {
        crate::LocalSendFaultMode::None => out.push_str("none"),
        crate::LocalSendFaultMode::DelayByRounds { one_in, rounds } => {
            let _ = write!(out, "delay-rounds({one_in},{rounds})");
        }
    }
    out.push_str(";timer=");
    match faults.timer_wake {
        crate::FaultMode::None => out.push_str("none"),
        crate::FaultMode::DelayBy { one_in, by } => {
            let _ = write!(out, "delay-by({one_in},{})", by.as_nanos());
        }
    }
    out.push_str(";tcp=");
    match faults.tcp_completion {
        crate::TcpCompletionFaultMode::None => out.push_str("none"),
        crate::TcpCompletionFaultMode::DelayBySteps { one_in, steps } => {
            let _ = write!(out, "delay-steps({one_in},{steps})");
        }
        crate::TcpCompletionFaultMode::ReorderReady { one_in } => {
            let _ = write!(out, "reorder-ready({one_in})");
        }
    }
    out.push('}');
}

fn encode_tcp_config(out: &mut String, config: &crate::ScriptedTcpConfig) {
    let _ = write!(
        out,
        "{{pending={};listeners=[",
        config.pending_completion_capacity
    );
    for listener in &config.listeners {
        out.push_str("{bind=");
        encode_socket_addr(out, listener.bind_addr);
        out.push_str(";local=");
        encode_socket_addr(out, listener.local_addr);
        let _ = write!(out, ";backlog={};peers=[", listener.backlog_capacity);
        for peer in &listener.peers {
            let _ = write!(out, "{{after={};peer=", peer.accept_after_step);
            encode_socket_addr(out, peer.peer_addr);
            out.push_str(";in=[");
            for chunk in &peer.inbound_chunks {
                encode_bytes(out, chunk);
                out.push(';');
            }
            let _ = write!(out, "];in_cap={};read_cap=", peer.inbound_capacity);
            encode_option_usize(out, peer.read_chunk_cap);
            let _ = write!(
                out,
                ";write_cap={};out_cap={};}}",
                peer.write_cap, peer.output_capacity
            );
        }
        out.push_str("]}");
    }
    out.push_str("]}");
}

fn encode_udp_config(out: &mut String, config: &crate::ScriptedUdpConfig) {
    let _ = write!(
        out,
        "{{pending={};sockets=[",
        config.pending_completion_capacity
    );
    for socket in &config.sockets {
        out.push_str("{bind=");
        encode_socket_addr(out, socket.bind_addr);
        out.push_str(";local=");
        encode_socket_addr(out, socket.local_addr);
        let _ = write!(out, ";recv_cap={};datagrams=[", socket.recv_capacity);
        for datagram in &socket.inbound_datagrams {
            let _ = write!(out, "{{after={};peer=", datagram.deliver_after_step);
            encode_socket_addr(out, datagram.peer_addr);
            out.push_str(";bytes=");
            encode_bytes(out, &datagram.bytes);
            out.push('}');
        }
        out.push_str("]}");
    }
    out.push_str("]}");
}

fn encode_dns_config(out: &mut String, config: &crate::ScriptedDnsConfig) {
    let _ = write!(
        out,
        "{{pending={};lookups=[",
        config.pending_completion_capacity
    );
    for lookup in &config.lookups {
        out.push_str("{host=");
        encode_string(out, &lookup.host);
        let _ = write!(
            out,
            ";port={};after={};result=",
            lookup.port, lookup.complete_after_step
        );
        match &lookup.result {
            crate::ScriptedDnsResult::Resolved(addrs) => {
                out.push_str("resolved[");
                for addr in addrs {
                    encode_socket_addr(out, *addr);
                    out.push(';');
                }
                out.push(']');
            }
            crate::ScriptedDnsResult::Failed => out.push_str("failed"),
            crate::ScriptedDnsResult::Timeout => out.push_str("timeout"),
        }
        out.push('}');
    }
    out.push_str("]}");
}

fn encode_tls_config(out: &mut String, config: &crate::ScriptedTlsConfig) {
    let _ = write!(
        out,
        "{{pending={};connects=[",
        config.pending_completion_capacity
    );
    for connect in &config.connects {
        out.push_str("{addr=");
        encode_socket_addr(out, connect.addr);
        out.push_str(";server=");
        encode_string(out, &connect.server_name);
        let _ = write!(out, ";after={};result=", connect.complete_after_step);
        match &connect.result {
            crate::ScriptedTlsConnectResult::Connected { reads, writes } => {
                out.push_str("connected{reads=[");
                for read in reads {
                    match read {
                        crate::ScriptedTlsReadResult::Bytes(bytes) => {
                            out.push_str("bytes(");
                            encode_bytes(out, bytes);
                            out.push(')');
                        }
                        crate::ScriptedTlsReadResult::Eof => out.push_str("eof"),
                        crate::ScriptedTlsReadResult::Failed => out.push_str("failed"),
                        crate::ScriptedTlsReadResult::Timeout => out.push_str("timeout"),
                    }
                    out.push(';');
                }
                out.push_str("];writes=[");
                for write in writes {
                    match write {
                        crate::ScriptedTlsWriteResult::Wrote(bytes) => {
                            let _ = write!(out, "wrote({bytes})");
                        }
                        crate::ScriptedTlsWriteResult::Failed => out.push_str("failed"),
                        crate::ScriptedTlsWriteResult::Timeout => out.push_str("timeout"),
                    }
                    out.push(';');
                }
                out.push_str("]}");
            }
            crate::ScriptedTlsConnectResult::Failed => out.push_str("failed"),
            crate::ScriptedTlsConnectResult::Certificate => out.push_str("certificate"),
            crate::ScriptedTlsConnectResult::Name => out.push_str("name"),
            crate::ScriptedTlsConnectResult::Timeout => out.push_str("timeout"),
        }
        out.push('}');
    }
    out.push_str("]}");
}

fn encode_signal_config(out: &mut String, config: &crate::ScriptedSignalConfig) {
    let _ = write!(
        out,
        "{{pending={};events=[",
        config.pending_completion_capacity
    );
    for event in &config.events {
        out.push_str("{name=");
        encode_string(out, &event.name);
        let _ = write!(out, ";after={};result=", event.deliver_after_step);
        match event.result {
            crate::ScriptedSignalResult::Received => out.push_str("received"),
            crate::ScriptedSignalResult::Failed => out.push_str("failed"),
            crate::ScriptedSignalResult::Timeout => out.push_str("timeout"),
        }
        out.push('}');
    }
    out.push_str("]}");
}

fn encode_process_config(out: &mut String, config: &crate::ScriptedProcessConfig) {
    let _ = write!(
        out,
        "{{pending={};runs=[",
        config.pending_completion_capacity
    );
    for run in &config.runs {
        out.push_str("{command=");
        encode_string(out, &run.command);
        out.push_str(";args=[");
        for arg in &run.args {
            encode_string(out, arg);
            out.push(';');
        }
        let _ = write!(out, "];after={};result=", run.complete_after_step);
        match &run.result {
            crate::ScriptedProcessResult::Exited {
                code,
                stdout,
                stderr,
            } => {
                out.push_str("exited(");
                encode_option_i32(out, *code);
                out.push(',');
                encode_bytes(out, stdout);
                out.push(',');
                encode_bytes(out, stderr);
                out.push(')');
            }
            crate::ScriptedProcessResult::Failed => out.push_str("failed"),
            crate::ScriptedProcessResult::Timeout => out.push_str("timeout"),
            crate::ScriptedProcessResult::KillUncertain => out.push_str("kill-uncertain"),
        }
        out.push('}');
    }
    out.push_str("]}");
}

fn encode_storage_config(out: &mut String, config: crate::ScriptedStorageFaultConfig) {
    out.push_str("{fail_journal=");
    encode_option_u64(out, config.fail_journal_append_at);
    out.push_str(";fail_snapshot=");
    encode_option_u64(out, config.fail_snapshot_commit_at);
    out.push_str(";truncate_journal=");
    encode_option_u64(out, config.truncate_journal_tail_at);
    out.push_str(";corrupt_journal=");
    encode_option_u64(out, config.corrupt_journal_record_at);
    out.push_str(";uncertain_snapshot=");
    encode_option_u64(out, config.commit_uncertain_snapshot_at);
    out.push('}');
}
