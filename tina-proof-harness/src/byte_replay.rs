//! WebSocket protocol byte replay: save a bad-frame case, reproduce it, and
//! shrink it.
//!
//! A [`ProtocolByteReplayCase`] is the materialized form of a protocol bug:
//! the ordered byte chunks that triggered it, the direction they flowed, the
//! decode limits, and the typed expectations (app deliveries, close, and a
//! stable fingerprint over the observed [`ProtocolFact`] sequence). Replay
//! feeds the chunks back through the pure [`crate::websocket`] session and
//! compares the typed outcome.
//!
//! Two honesty rules:
//!
//! - The expected facts are pinned as a count plus a stable hash over the
//!   typed [`ProtocolFact`] values — never debug text.
//! - A case that carries unsupported facts, or that exceeds its byte/chunk
//!   budget, is replay-blocked: it never passes as an exact replay.

use std::path::Path;

use tina_runtime::{ProtocolFact, WebSocketCloseReason};
use tina_sim::dst::UnsupportedLiveFact;

use crate::protocol_chaos::{
    ProtocolChaosFamily, ProtocolCloseStatus, protocol_fact_sequence_hash,
};
use crate::websocket::{WebSocketLimits, WebSocketRole, WebSocketSession};

/// Which direction the captured bytes flowed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ByteReplayDirection {
    /// Client→server bytes; decoded with [`WebSocketRole::Server`].
    ClientToServer,
    /// Server→client bytes; decoded with [`WebSocketRole::Client`].
    ServerToClient,
}

impl ByteReplayDirection {
    const fn role(self) -> WebSocketRole {
        match self {
            Self::ClientToServer => WebSocketRole::Server,
            Self::ServerToClient => WebSocketRole::Client,
        }
    }

    const fn label(self) -> &'static str {
        match self {
            Self::ClientToServer => "client-to-server",
            Self::ServerToClient => "server-to-client",
        }
    }

    fn from_label(label: &str) -> Option<Self> {
        match label {
            "client-to-server" => Some(Self::ClientToServer),
            "server-to-client" => Some(Self::ServerToClient),
            _ => None,
        }
    }
}

/// A saved, replayable WebSocket byte case.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProtocolByteReplayCase {
    /// Stable case name.
    pub name: &'static str,
    /// Direction the bytes flowed.
    pub direction: ByteReplayDirection,
    /// Decode limits the session enforces.
    pub limits: WebSocketLimits,
    /// Maximum total bytes the case may carry.
    pub max_bytes: usize,
    /// Maximum number of chunks the case may carry.
    pub max_chunks: usize,
    /// Ordered byte chunks, replayed in sequence.
    pub chunks: Vec<Vec<u8>>,
    /// Expected count of valid app deliveries.
    pub expected_app_deliveries: usize,
    /// Expected close `(code, reason)`.
    pub expected_close: Option<(Option<u16>, WebSocketCloseReason)>,
    /// Expected count of typed protocol facts.
    pub expected_fact_count: usize,
    /// Stable fingerprint over the expected typed protocol-fact sequence.
    pub expected_fact_hash: u64,
    /// Facts the live capture could not model; their presence blocks replay.
    pub unsupported_facts: Vec<UnsupportedLiveFact>,
}

impl ProtocolByteReplayCase {
    /// Builds a case from chunks and pins the expected outcome by observing one
    /// run. Use this when first capturing a bad-frame case.
    pub fn capture(
        name: &'static str,
        direction: ByteReplayDirection,
        limits: WebSocketLimits,
        chunks: Vec<Vec<u8>>,
    ) -> Self {
        let total_bytes: usize = chunks.iter().map(Vec::len).sum();
        let mut case = Self {
            name,
            direction,
            limits,
            max_bytes: total_bytes.max(1),
            max_chunks: chunks.len().max(1),
            chunks,
            expected_app_deliveries: 0,
            expected_close: None,
            expected_fact_count: 0,
            expected_fact_hash: protocol_fact_sequence_hash(&[]),
            unsupported_facts: Vec::new(),
        };
        case.pin_from_observation();
        case
    }

    /// Sets the byte/chunk budget and returns the case.
    pub const fn with_budget(mut self, max_bytes: usize, max_chunks: usize) -> Self {
        self.max_bytes = max_bytes;
        self.max_chunks = max_chunks;
        self
    }

    /// Records one unsupported fact and returns the case. A case with
    /// unsupported facts is replay-blocked.
    pub fn with_unsupported_fact(
        mut self,
        fact: impl Into<String>,
        reason: impl Into<String>,
    ) -> Self {
        self.unsupported_facts
            .push(UnsupportedLiveFact::new(fact, reason));
        self
    }

    /// Re-observes one run and pins the expected app deliveries, close, fact
    /// count, and fact hash from it.
    pub fn pin_from_observation(&mut self) {
        let report = self.observe();
        self.expected_app_deliveries = report.app_deliveries;
        self.expected_close = report.close;
        self.expected_fact_count = report.fact_count;
        self.expected_fact_hash = report.fact_hash;
    }

    /// Total bytes carried by the case.
    pub fn total_bytes(&self) -> usize {
        self.chunks.iter().map(Vec::len).sum()
    }

    /// True when the case is over budget or carries unsupported facts.
    pub fn replay_blocked(&self) -> bool {
        !self.unsupported_facts.is_empty()
            || self.total_bytes() > self.max_bytes
            || self.chunks.len() > self.max_chunks
    }

    /// Runs the chunks through a fresh session and returns the observed report.
    pub fn observe(&self) -> ProtocolByteReplayReport {
        let mut session = WebSocketSession::new(
            self.direction.role(),
            tina_runtime::WebSocketSessionId::new(1),
            self.limits,
        );
        for chunk in &self.chunks {
            session.feed(chunk);
        }
        let run = session.finish();
        ProtocolByteReplayReport {
            name: self.name,
            family: ProtocolChaosFamily::WebSocket,
            app_deliveries: run.app_messages.len(),
            close: run.close,
            close_status: run
                .close
                .map(|(code, reason)| ProtocolCloseStatus::WebSocketClose { code, reason }),
            fact_count: run.facts.len(),
            fact_hash: protocol_fact_sequence_hash(&run.facts),
            facts: run.facts,
            bytes_replayed: run.bytes_fed,
            chunks_replayed: self.chunks.len(),
            replay_blocked: self.replay_blocked(),
        }
    }

    /// Replays the case and compares the observed outcome to the pinned
    /// expectation. Fails closed if the case is replay-blocked.
    pub fn replay(&self) -> Result<ProtocolByteReplayReport, Box<ProtocolByteReplayMismatch>> {
        let report = self.observe();
        let mut diverged = Vec::new();
        if report.replay_blocked {
            diverged.push(ByteReplayField::Blocked);
        }
        if report.app_deliveries != self.expected_app_deliveries {
            diverged.push(ByteReplayField::AppDeliveries);
        }
        if report.close != self.expected_close {
            diverged.push(ByteReplayField::Close);
        }
        if report.fact_count != self.expected_fact_count {
            diverged.push(ByteReplayField::FactCount);
        }
        if report.fact_hash != self.expected_fact_hash {
            diverged.push(ByteReplayField::FactHash);
        }
        if diverged.is_empty() {
            Ok(report)
        } else {
            Err(Box::new(ProtocolByteReplayMismatch {
                name: self.name,
                diverged,
                expected_app_deliveries: self.expected_app_deliveries,
                actual_app_deliveries: report.app_deliveries,
                expected_close: self.expected_close,
                actual_close: report.close,
                expected_fact_count: self.expected_fact_count,
                actual_fact_count: report.fact_count,
                expected_fact_hash: self.expected_fact_hash,
                actual_fact_hash: report.fact_hash,
                unsupported_facts: self.unsupported_facts.clone(),
            }))
        }
    }

    /// Greedily removes chunks while `still_fails` holds, then refreshes the
    /// expected facts for the smaller case.
    ///
    /// `still_fails(&report)` decides whether a shrunk run still reproduces the
    /// bug the case is about (e.g. "still closes with a protocol-error fact").
    /// The returned case is itself replayable: its expectations are refreshed
    /// from its final, smaller run, so a stale fact never lingers.
    pub fn shrink<F>(&self, mut still_fails: F) -> ProtocolByteReplayShrink
    where
        F: FnMut(&ProtocolByteReplayReport) -> bool,
    {
        let original_len = self.chunks.len();
        let mut chunks = self.chunks.clone();
        let mut changed = true;
        while changed && chunks.len() > 1 {
            changed = false;
            for index in 0..chunks.len() {
                if chunks.len() <= 1 {
                    break;
                }
                let mut candidate = chunks.clone();
                candidate.remove(index);
                let trial = self.with_chunks(candidate.clone());
                if still_fails(&trial.observe()) {
                    chunks = candidate;
                    changed = true;
                    break;
                }
            }
        }
        let mut shrunk = self.with_chunks(chunks);
        // Refresh budget to the shrunk size and re-pin facts so the shrunk
        // case is self-consistent.
        shrunk.max_bytes = shrunk.total_bytes().max(1);
        shrunk.max_chunks = shrunk.chunks.len().max(1);
        shrunk.pin_from_observation();
        ProtocolByteReplayShrink {
            original_len,
            shrunk_len: shrunk.chunks.len(),
            shrunk_report: shrunk.observe(),
            shrunk_case: shrunk,
        }
    }

    fn with_chunks(&self, chunks: Vec<Vec<u8>>) -> Self {
        Self {
            name: self.name,
            direction: self.direction,
            limits: self.limits,
            max_bytes: self.max_bytes,
            max_chunks: self.max_chunks,
            chunks,
            expected_app_deliveries: self.expected_app_deliveries,
            expected_close: self.expected_close,
            expected_fact_count: self.expected_fact_count,
            expected_fact_hash: self.expected_fact_hash,
            unsupported_facts: self.unsupported_facts.clone(),
        }
    }

    /// Writes the case to a small line-oriented file.
    pub fn save(&self, path: impl AsRef<Path>) -> Result<(), ProtocolByteReplayIoError> {
        if self.name.contains('\n') {
            return Err(ProtocolByteReplayIoError::Decode {
                line: 0,
                reason: "name may not contain a newline".to_owned(),
            });
        }
        let mut body = String::new();
        body.push_str("tina-protocol-byte-replay-v1\n");
        body.push_str(&format!("name={}\n", self.name));
        body.push_str("family=websocket\n");
        body.push_str(&format!("direction={}\n", self.direction.label()));
        body.push_str(&format!(
            "max_message_bytes={}\n",
            self.limits.max_message_bytes
        ));
        body.push_str(&format!(
            "max_frame_payload={}\n",
            self.limits.max_frame_payload
        ));
        body.push_str(&format!("max_frames={}\n", self.limits.max_frames));
        body.push_str(&format!("max_bytes={}\n", self.max_bytes));
        body.push_str(&format!("max_chunks={}\n", self.max_chunks));
        body.push_str(&format!(
            "expected_app_deliveries={}\n",
            self.expected_app_deliveries
        ));
        body.push_str(&format!(
            "expected_close={}\n",
            encode_close(self.expected_close)
        ));
        body.push_str(&format!(
            "expected_fact_count={}\n",
            self.expected_fact_count
        ));
        body.push_str(&format!(
            "expected_fact_hash=0x{:016x}\n",
            self.expected_fact_hash
        ));
        for fact in &self.unsupported_facts {
            if fact.fact.contains('\n')
                || fact.fact.contains(" | ")
                || fact.reason.contains('\n')
                || fact.reason.contains(" | ")
            {
                return Err(ProtocolByteReplayIoError::Decode {
                    line: 0,
                    reason: "unsupported fact contains a reserved delimiter".to_owned(),
                });
            }
            body.push_str(&format!(
                "unsupported_fact={} | {}\n",
                fact.fact, fact.reason
            ));
        }
        for chunk in &self.chunks {
            body.push_str(&format!("chunk={}\n", encode_hex(chunk)));
        }
        std::fs::write(path, body)?;
        Ok(())
    }

    /// Reads a case written by [`Self::save`]. The caller supplies the static
    /// `name` so the in-memory case keeps `&'static str` identity; the saved
    /// name must match.
    pub fn load(
        path: impl AsRef<Path>,
        name: &'static str,
    ) -> Result<Self, ProtocolByteReplayIoError> {
        let text = std::fs::read_to_string(path)?;
        let mut lines = text.lines();
        match lines.next() {
            Some("tina-protocol-byte-replay-v1") => {}
            Some(other) => {
                return Err(ProtocolByteReplayIoError::Decode {
                    line: 1,
                    reason: format!("unknown header {other:?}"),
                });
            }
            None => return Err(ProtocolByteReplayIoError::MissingField("header")),
        }

        let mut saved_name = None;
        let mut family_seen = false;
        let mut direction = None;
        let mut max_message_bytes = None;
        let mut max_frame_payload = None;
        let mut max_frames = None;
        let mut max_bytes = None;
        let mut max_chunks = None;
        let mut expected_app_deliveries = None;
        let mut expected_close = None;
        let mut expected_fact_count = None;
        let mut expected_fact_hash = None;
        let mut unsupported_facts = Vec::new();
        let mut chunks = Vec::new();

        for (index, line) in lines.enumerate() {
            let line_no = index + 2;
            let Some((key, value)) = line.split_once('=') else {
                return Err(ProtocolByteReplayIoError::Decode {
                    line: line_no,
                    reason: "expected key=value".to_owned(),
                });
            };
            match key {
                "name" => saved_name = Some(value.to_owned()),
                "family" => {
                    if value != "websocket" {
                        return Err(ProtocolByteReplayIoError::Decode {
                            line: line_no,
                            reason: format!("unsupported family {value:?}"),
                        });
                    }
                    family_seen = true;
                }
                "direction" => {
                    direction = Some(ByteReplayDirection::from_label(value).ok_or_else(|| {
                        ProtocolByteReplayIoError::Decode {
                            line: line_no,
                            reason: format!("unknown direction {value:?}"),
                        }
                    })?)
                }
                "max_message_bytes" => max_message_bytes = Some(parse_usize(value, line_no)?),
                "max_frame_payload" => max_frame_payload = Some(parse_usize(value, line_no)?),
                "max_frames" => max_frames = Some(parse_usize(value, line_no)?),
                "max_bytes" => max_bytes = Some(parse_usize(value, line_no)?),
                "max_chunks" => max_chunks = Some(parse_usize(value, line_no)?),
                "expected_app_deliveries" => {
                    expected_app_deliveries = Some(parse_usize(value, line_no)?)
                }
                "expected_close" => expected_close = Some(decode_close(value, line_no)?),
                "expected_fact_count" => expected_fact_count = Some(parse_usize(value, line_no)?),
                "expected_fact_hash" => expected_fact_hash = Some(parse_hex_u64(value, line_no)?),
                "unsupported_fact" => {
                    let Some((fact, reason)) = value.split_once(" | ") else {
                        return Err(ProtocolByteReplayIoError::Decode {
                            line: line_no,
                            reason: "unsupported_fact must be `fact | reason`".to_owned(),
                        });
                    };
                    unsupported_facts.push(UnsupportedLiveFact::new(fact, reason));
                }
                "chunk" => chunks.push(decode_hex(value, line_no)?),
                other => {
                    return Err(ProtocolByteReplayIoError::Decode {
                        line: line_no,
                        reason: format!("unknown key {other:?}"),
                    });
                }
            }
        }

        let saved_name = saved_name.ok_or(ProtocolByteReplayIoError::MissingField("name"))?;
        if saved_name != name {
            return Err(ProtocolByteReplayIoError::NameMismatch {
                expected: name,
                actual: saved_name,
            });
        }
        if !family_seen {
            return Err(ProtocolByteReplayIoError::MissingField("family"));
        }
        Ok(Self {
            name,
            direction: direction.ok_or(ProtocolByteReplayIoError::MissingField("direction"))?,
            limits: WebSocketLimits {
                max_message_bytes: max_message_bytes
                    .ok_or(ProtocolByteReplayIoError::MissingField("max_message_bytes"))?,
                max_frame_payload: max_frame_payload
                    .ok_or(ProtocolByteReplayIoError::MissingField("max_frame_payload"))?,
                max_frames: max_frames
                    .ok_or(ProtocolByteReplayIoError::MissingField("max_frames"))?,
            },
            max_bytes: max_bytes.ok_or(ProtocolByteReplayIoError::MissingField("max_bytes"))?,
            max_chunks: max_chunks.ok_or(ProtocolByteReplayIoError::MissingField("max_chunks"))?,
            chunks,
            expected_app_deliveries: expected_app_deliveries.ok_or(
                ProtocolByteReplayIoError::MissingField("expected_app_deliveries"),
            )?,
            expected_close: expected_close
                .ok_or(ProtocolByteReplayIoError::MissingField("expected_close"))?,
            expected_fact_count: expected_fact_count.ok_or(
                ProtocolByteReplayIoError::MissingField("expected_fact_count"),
            )?,
            expected_fact_hash: expected_fact_hash.ok_or(
                ProtocolByteReplayIoError::MissingField("expected_fact_hash"),
            )?,
            unsupported_facts,
        })
    }
}

/// One observed byte-replay run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProtocolByteReplayReport {
    /// Case name.
    pub name: &'static str,
    /// Protocol family.
    pub family: ProtocolChaosFamily,
    /// Valid app deliveries.
    pub app_deliveries: usize,
    /// Final close `(code, reason)`.
    pub close: Option<(Option<u16>, WebSocketCloseReason)>,
    /// Typed close status mirror.
    pub close_status: Option<ProtocolCloseStatus>,
    /// Observed typed protocol facts.
    pub facts: Vec<ProtocolFact>,
    /// Count of observed facts.
    pub fact_count: usize,
    /// Stable fingerprint over the observed fact sequence.
    pub fact_hash: u64,
    /// Bytes replayed.
    pub bytes_replayed: usize,
    /// Chunks replayed.
    pub chunks_replayed: usize,
    /// True when the case is replay-blocked (over budget or unsupported facts).
    pub replay_blocked: bool,
}

/// Result of shrinking a byte-replay case.
#[derive(Debug, Clone)]
pub struct ProtocolByteReplayShrink {
    /// Chunk count before shrinking.
    pub original_len: usize,
    /// Chunk count after shrinking.
    pub shrunk_len: usize,
    /// The smaller case, with refreshed expectations.
    pub shrunk_case: ProtocolByteReplayCase,
    /// The shrunk case's observed run.
    pub shrunk_report: ProtocolByteReplayReport,
}

/// Which expectation field diverged on replay.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ByteReplayField {
    /// The case was replay-blocked (unsupported facts or over budget).
    Blocked,
    /// App delivery count.
    AppDeliveries,
    /// Close.
    Close,
    /// Fact count.
    FactCount,
    /// Fact hash.
    FactHash,
}

/// Typed replay mismatch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProtocolByteReplayMismatch {
    /// Case name.
    pub name: &'static str,
    /// Diverged fields.
    pub diverged: Vec<ByteReplayField>,
    /// Expected app deliveries.
    pub expected_app_deliveries: usize,
    /// Observed app deliveries.
    pub actual_app_deliveries: usize,
    /// Expected close.
    pub expected_close: Option<(Option<u16>, WebSocketCloseReason)>,
    /// Observed close.
    pub actual_close: Option<(Option<u16>, WebSocketCloseReason)>,
    /// Expected fact count.
    pub expected_fact_count: usize,
    /// Observed fact count.
    pub actual_fact_count: usize,
    /// Expected fact hash.
    pub expected_fact_hash: u64,
    /// Observed fact hash.
    pub actual_fact_hash: u64,
    /// Unsupported facts that blocked the replay, if any.
    pub unsupported_facts: Vec<UnsupportedLiveFact>,
}

impl ProtocolByteReplayMismatch {
    /// Returns true when `field` diverged.
    pub fn includes(&self, field: ByteReplayField) -> bool {
        self.diverged.contains(&field)
    }
}

impl std::fmt::Display for ProtocolByteReplayMismatch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        writeln!(
            f,
            "protocol byte replay `{}` diverged: {:?}",
            self.name, self.diverged
        )?;
        if self.includes(ByteReplayField::Blocked) {
            writeln!(
                f,
                "  replay blocked by unsupported facts or byte/chunk budget:"
            )?;
            for fact in &self.unsupported_facts {
                writeln!(f, "      - {fact}")?;
            }
        }
        writeln!(
            f,
            "  app_deliveries: expected {}, got {}",
            self.expected_app_deliveries, self.actual_app_deliveries
        )?;
        writeln!(
            f,
            "  close: expected {:?}, got {:?}",
            self.expected_close, self.actual_close
        )?;
        writeln!(
            f,
            "  facts: expected {} (hash 0x{:016x}), got {} (hash 0x{:016x})",
            self.expected_fact_count,
            self.expected_fact_hash,
            self.actual_fact_count,
            self.actual_fact_hash
        )
    }
}

impl std::error::Error for ProtocolByteReplayMismatch {}

/// I/O or decode error from the saved-case file helper.
#[derive(Debug)]
pub enum ProtocolByteReplayIoError {
    /// Filesystem I/O failed.
    Io(std::io::Error),
    /// A line could not be decoded.
    Decode {
        /// One-based line number.
        line: usize,
        /// Human-readable reason.
        reason: String,
    },
    /// A required field was absent.
    MissingField(&'static str),
    /// The saved name does not match the caller-supplied static name.
    NameMismatch {
        /// Caller-supplied name.
        expected: &'static str,
        /// Saved name.
        actual: String,
    },
}

impl std::fmt::Display for ProtocolByteReplayIoError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(error) => write!(f, "protocol byte replay I/O failed: {error}"),
            Self::Decode { line, reason } => {
                write!(
                    f,
                    "protocol byte replay decode failed on line {line}: {reason}"
                )
            }
            Self::MissingField(field) => {
                write!(f, "protocol byte replay missing field `{field}`")
            }
            Self::NameMismatch { expected, actual } => write!(
                f,
                "protocol byte replay name mismatch: expected {expected:?}, saved {actual:?}"
            ),
        }
    }
}

impl std::error::Error for ProtocolByteReplayIoError {}

impl From<std::io::Error> for ProtocolByteReplayIoError {
    fn from(value: std::io::Error) -> Self {
        Self::Io(value)
    }
}

fn encode_hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

fn decode_hex(value: &str, line: usize) -> Result<Vec<u8>, ProtocolByteReplayIoError> {
    if value.len() % 2 != 0 {
        return Err(ProtocolByteReplayIoError::Decode {
            line,
            reason: "hex chunk must have an even length".to_owned(),
        });
    }
    let mut out = Vec::with_capacity(value.len() / 2);
    let bytes = value.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        let pair = &value[index..index + 2];
        let byte =
            u8::from_str_radix(pair, 16).map_err(|error| ProtocolByteReplayIoError::Decode {
                line,
                reason: format!("invalid hex {pair:?}: {error}"),
            })?;
        out.push(byte);
        index += 2;
    }
    Ok(out)
}

fn parse_usize(value: &str, line: usize) -> Result<usize, ProtocolByteReplayIoError> {
    value
        .parse::<usize>()
        .map_err(|error| ProtocolByteReplayIoError::Decode {
            line,
            reason: format!("invalid integer {value:?}: {error}"),
        })
}

fn parse_hex_u64(value: &str, line: usize) -> Result<u64, ProtocolByteReplayIoError> {
    let digits = value.strip_prefix("0x").unwrap_or(value);
    u64::from_str_radix(digits, 16).map_err(|error| ProtocolByteReplayIoError::Decode {
        line,
        reason: format!("invalid hex u64 {value:?}: {error}"),
    })
}

fn encode_close(close: Option<(Option<u16>, WebSocketCloseReason)>) -> String {
    match close {
        None => "none".to_owned(),
        Some((Some(code), reason)) => format!("code:{code}:{}", encode_reason(reason)),
        Some((None, reason)) => format!("nocode:{}", encode_reason(reason)),
    }
}

fn decode_close(
    value: &str,
    line: usize,
) -> Result<Option<(Option<u16>, WebSocketCloseReason)>, ProtocolByteReplayIoError> {
    if value == "none" {
        return Ok(None);
    }
    let parts: Vec<&str> = value.split(':').collect();
    match parts.as_slice() {
        ["code", code, reason] => {
            let code = code
                .parse::<u16>()
                .map_err(|error| ProtocolByteReplayIoError::Decode {
                    line,
                    reason: format!("invalid close code {code:?}: {error}"),
                })?;
            Ok(Some((Some(code), decode_reason(reason, line)?)))
        }
        ["nocode", reason] => Ok(Some((None, decode_reason(reason, line)?))),
        _ => Err(ProtocolByteReplayIoError::Decode {
            line,
            reason: format!("invalid expected_close {value:?}"),
        }),
    }
}

fn encode_reason(reason: WebSocketCloseReason) -> &'static str {
    match reason {
        WebSocketCloseReason::Normal => "Normal",
        WebSocketCloseReason::GoingAway => "GoingAway",
        WebSocketCloseReason::ProtocolError => "ProtocolError",
        WebSocketCloseReason::LocalInitiated => "LocalInitiated",
        WebSocketCloseReason::SlowPeer => "SlowPeer",
        WebSocketCloseReason::IdleTimeout => "IdleTimeout",
        WebSocketCloseReason::Abnormal => "Abnormal",
        // `WebSocketCloseReason` is non-exhaustive. A future variant has no
        // saved-format token, so it serialises to a sentinel `decode_reason`
        // rejects: the saved case fails closed instead of round-tripping a
        // reason the format does not understand.
        _ => "Other",
    }
}

fn decode_reason(
    value: &str,
    line: usize,
) -> Result<WebSocketCloseReason, ProtocolByteReplayIoError> {
    Ok(match value {
        "Normal" => WebSocketCloseReason::Normal,
        "GoingAway" => WebSocketCloseReason::GoingAway,
        "ProtocolError" => WebSocketCloseReason::ProtocolError,
        "LocalInitiated" => WebSocketCloseReason::LocalInitiated,
        "SlowPeer" => WebSocketCloseReason::SlowPeer,
        "IdleTimeout" => WebSocketCloseReason::IdleTimeout,
        "Abnormal" => WebSocketCloseReason::Abnormal,
        other => {
            return Err(ProtocolByteReplayIoError::Decode {
                line,
                reason: format!("unknown close reason {other:?}"),
            });
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::websocket::{WebSocketLimits, client_frame, server_frame};

    const OPCODE_TEXT: u8 = 0x1;
    const OPCODE_PING: u8 = 0x9;

    /// A saved bad-frame case: a valid ping, a valid text frame, then a frame
    /// with reserved bits set (the bug). Wrapped in extra chunks so shrink has
    /// something to remove.
    fn bad_frame_case() -> ProtocolByteReplayCase {
        let mut bad = client_frame(true, OPCODE_TEXT, b"x");
        bad[0] |= 0x40; // RSV1 set: protocol error.
        let chunks = vec![
            client_frame(true, OPCODE_PING, b"hb"),
            client_frame(true, OPCODE_TEXT, b"ok"),
            bad,
            b"trailing-junk-ignored-after-close".to_vec(),
        ];
        ProtocolByteReplayCase::capture(
            "ws_bad_reserved_bits",
            ByteReplayDirection::ClientToServer,
            WebSocketLimits::default(),
            chunks,
        )
    }

    #[test]
    fn byte_replay_reproduces_saved_bad_frame_case() {
        let case = bad_frame_case();
        // The case closes with a protocol error and delivers exactly the one
        // valid text message before the bad frame.
        let report = case.replay().expect("case reproduces its pinned outcome");
        assert_eq!(report.app_deliveries, 1);
        assert_eq!(
            report.close,
            Some((Some(1002), WebSocketCloseReason::ProtocolError))
        );
        assert_eq!(report.fact_count, 1);
        assert!(!report.replay_blocked);
    }

    #[test]
    fn shrink_reduces_chunks_and_refreshes_expected_facts() {
        let case = bad_frame_case();
        let original_close = case.expected_close;
        // The original delivers the valid "ok" text before the bad frame.
        assert_eq!(case.expected_app_deliveries, 1);
        let shrink = case.shrink(|report| {
            // The bug is "session closes with a protocol-error fact".
            report.close == Some((Some(1002), WebSocketCloseReason::ProtocolError))
        });
        assert!(shrink.shrunk_len < shrink.original_len, "{shrink:?}");
        // The shrunk case is self-consistent and still reproduces.
        let report = shrink
            .shrunk_case
            .replay()
            .expect("shrunk case replays against refreshed expectations");
        assert_eq!(
            report.close,
            Some((Some(1002), WebSocketCloseReason::ProtocolError))
        );
        // Close preserved; the bug-defining close is unchanged by shrinking.
        assert_eq!(shrink.shrunk_case.expected_close, original_close);
        // Expectations were freshly observed, not inherited stale: dropping the
        // leading valid text chunk drops the app delivery from 1 to 0, and the
        // shrunk case pins that new value.
        assert_eq!(shrink.shrunk_case.expected_app_deliveries, 0);
        assert_eq!(
            shrink.shrunk_case.expected_app_deliveries,
            report.app_deliveries
        );
    }

    #[test]
    fn saved_case_round_trips() {
        let case = bad_frame_case();
        let path = std::env::temp_dir().join(format!(
            "tina-byte-replay-{}-{}.case",
            std::process::id(),
            case.expected_fact_hash
        ));
        case.save(&path).expect("save");
        let loaded = ProtocolByteReplayCase::load(&path, "ws_bad_reserved_bits").expect("load");
        let _ = std::fs::remove_file(&path);
        assert_eq!(loaded, case);
        loaded.replay().expect("loaded case still reproduces");
    }

    #[test]
    fn unsupported_facts_fail_closed() {
        let case = bad_frame_case().with_unsupported_fact(
            "kernel recv-buffer race",
            "OS socket timing is not modeled by the pure session",
        );
        assert!(case.replay_blocked());
        let mismatch = case
            .replay()
            .expect_err("unsupported live facts must fail closed");
        assert!(mismatch.includes(ByteReplayField::Blocked));
        assert!(mismatch.to_string().contains("kernel recv-buffer race"));
    }

    #[test]
    fn save_rejects_unsupported_fact_reason_delimiters() {
        let case = bad_frame_case().with_unsupported_fact("future-fact", "bad | delimiter");
        let path = std::env::temp_dir().join(format!(
            "tina-byte-bad-reason-{}-{}.case",
            std::process::id(),
            case.expected_fact_hash
        ));
        let err = case
            .save(&path)
            .expect_err("reserved delimiter in reason must fail closed");
        assert!(matches!(err, ProtocolByteReplayIoError::Decode { .. }));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn over_budget_case_is_replay_blocked() {
        let case = bad_frame_case().with_budget(2, 1);
        assert!(case.replay_blocked());
        let mismatch = case.replay().expect_err("over-budget case fails closed");
        assert!(mismatch.includes(ByteReplayField::Blocked));
    }

    #[test]
    fn drifted_expected_fact_hash_is_detected() {
        let mut case = bad_frame_case();
        case.expected_fact_hash ^= 0xdead;
        let mismatch = case.replay().expect_err("fact hash drift");
        assert!(mismatch.includes(ByteReplayField::FactHash));
    }

    #[test]
    fn server_to_client_direction_reproduces_and_round_trips() {
        // Server→client frames must be unmasked. A valid server text frame
        // delivers; a masked frame (illegal from a server) is a protocol error.
        let chunks = vec![
            server_frame(true, OPCODE_TEXT, b"hi"),
            client_frame(true, OPCODE_TEXT, b"illegal-mask"),
        ];
        let case = ProtocolByteReplayCase::capture(
            "ws_s2c",
            ByteReplayDirection::ServerToClient,
            WebSocketLimits::default(),
            chunks,
        );
        let report = case.replay().expect("reproduces");
        assert_eq!(report.app_deliveries, 1);
        assert_eq!(
            report.close,
            Some((Some(1002), WebSocketCloseReason::ProtocolError))
        );

        let path = std::env::temp_dir().join(format!(
            "tina-byte-s2c-{}-{}.case",
            std::process::id(),
            case.expected_fact_hash
        ));
        case.save(&path).expect("save");
        let loaded = ProtocolByteReplayCase::load(&path, "ws_s2c").expect("load");
        let _ = std::fs::remove_file(&path);
        assert_eq!(loaded.direction, ByteReplayDirection::ServerToClient);
        assert_eq!(loaded, case);
        loaded
            .replay()
            .expect("loaded server-to-client case reproduces");
    }

    #[test]
    fn load_rejects_name_mismatch_corrupt_header_and_missing_field() {
        let case = bad_frame_case();
        let path = std::env::temp_dir().join(format!(
            "tina-byte-err-{}-{}.case",
            std::process::id(),
            case.expected_fact_hash
        ));
        case.save(&path).expect("save");
        // A saved name that does not match the caller's static name fails closed.
        let err = ProtocolByteReplayCase::load(&path, "different_name")
            .expect_err("name mismatch must fail closed");
        assert!(matches!(
            err,
            ProtocolByteReplayIoError::NameMismatch { .. }
        ));
        let _ = std::fs::remove_file(&path);

        // A file with an unknown header fails to decode.
        let corrupt =
            std::env::temp_dir().join(format!("tina-byte-corrupt-{}.case", std::process::id()));
        std::fs::write(&corrupt, "not-a-valid-header\nname=x\n").expect("write");
        let err = ProtocolByteReplayCase::load(&corrupt, "x")
            .expect_err("corrupt header must fail to decode");
        assert!(matches!(err, ProtocolByteReplayIoError::Decode { .. }));
        let _ = std::fs::remove_file(&corrupt);

        // A well-formed header missing required fields reports the field.
        let partial =
            std::env::temp_dir().join(format!("tina-byte-partial-{}.case", std::process::id()));
        std::fs::write(&partial, "tina-protocol-byte-replay-v1\nname=x\n").expect("write");
        let err = ProtocolByteReplayCase::load(&partial, "x")
            .expect_err("missing fields must fail closed");
        assert!(matches!(err, ProtocolByteReplayIoError::MissingField(_)));
        let _ = std::fs::remove_file(&partial);
    }

    #[test]
    fn load_rejects_bad_hex_chunk() {
        let corrupt =
            std::env::temp_dir().join(format!("tina-byte-badhex-{}.case", std::process::id()));
        // An odd-length hex chunk cannot decode to bytes.
        let body = "tina-protocol-byte-replay-v1\n\
            name=x\nfamily=websocket\ndirection=client-to-server\n\
            max_message_bytes=16\nmax_frame_payload=16\nmax_frames=16\n\
            max_bytes=16\nmax_chunks=4\nexpected_app_deliveries=0\n\
            expected_close=none\nexpected_fact_count=0\n\
            expected_fact_hash=0x0\nchunk=abc\n";
        std::fs::write(&corrupt, body).expect("write");
        let err = ProtocolByteReplayCase::load(&corrupt, "x")
            .expect_err("odd-length hex chunk must fail to decode");
        assert!(matches!(err, ProtocolByteReplayIoError::Decode { .. }));
        let _ = std::fs::remove_file(&corrupt);
    }

    #[test]
    fn load_requires_family_field() {
        let missing =
            std::env::temp_dir().join(format!("tina-byte-no-family-{}.case", std::process::id()));
        let body = "tina-protocol-byte-replay-v1\n\
            name=x\ndirection=client-to-server\n\
            max_message_bytes=16\nmax_frame_payload=16\nmax_frames=16\n\
            max_bytes=16\nmax_chunks=4\nexpected_app_deliveries=0\n\
            expected_close=none\nexpected_fact_count=0\n\
            expected_fact_hash=0x0\nchunk=abcd\n";
        std::fs::write(&missing, body).expect("write");
        let err =
            ProtocolByteReplayCase::load(&missing, "x").expect_err("missing family must fail");
        assert!(matches!(
            err,
            ProtocolByteReplayIoError::MissingField("family")
        ));
        let _ = std::fs::remove_file(&missing);
    }
}
