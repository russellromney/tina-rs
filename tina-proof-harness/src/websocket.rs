//! Pure WebSocket session engine and a hermetic compliance corpus.
//!
//! The engine takes raw bytes — never a real socket — and runs them through
//! RFC 6455 frame parsing and session state: fragmentation reassembly, UTF-8
//! validation of text, control-frame rules, masking direction, and the close
//! handshake. It emits typed [`ProtocolFact`] values and app deliveries, so a
//! [`crate::ProtocolChaosReport`] can prove that valid data reaches app code
//! exactly once and that malformed bytes never do.
//!
//! This is a self-contained protocol model, not a tap into `tina-http`. It is
//! the substrate both the compliance corpus (here) and the byte-replay
//! workflow ([`crate::byte_replay`]) run on. It makes no byte-perfect claim
//! about any particular server; it encodes the protocol rules directly.

use tina_runtime::{ProtocolFact, WebSocketCloseReason, WebSocketSessionId};

use crate::protocol_chaos::{
    PeerAction, ProtocolChaosExpectation, ProtocolChaosFamily, ProtocolChaosReport,
    ProtocolCloseStatus, TerminalAction,
};

/// Which side of the session the engine is decoding frames from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WebSocketRole {
    /// Decoding client→server frames. They must be masked.
    Server,
    /// Decoding server→client frames. They must be unmasked.
    Client,
}

/// Bounds the session enforces while decoding.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WebSocketLimits {
    /// Maximum bytes in one reassembled message.
    pub max_message_bytes: usize,
    /// Maximum payload bytes in one non-control frame.
    pub max_frame_payload: usize,
    /// Maximum frames decoded before the session bails out.
    pub max_frames: usize,
}

impl Default for WebSocketLimits {
    fn default() -> Self {
        Self {
            max_message_bytes: 1 << 16,
            max_frame_payload: 1 << 16,
            max_frames: 256,
        }
    }
}

/// A message delivered to app code after reassembly + validation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AppMessage {
    /// A complete, valid UTF-8 text message.
    Text(String),
    /// A complete binary message.
    Binary(Vec<u8>),
}

/// WebSocket opcodes the engine recognises.
const OPCODE_CONTINUATION: u8 = 0x0;
const OPCODE_TEXT: u8 = 0x1;
const OPCODE_BINARY: u8 = 0x2;
const OPCODE_CLOSE: u8 = 0x8;
const OPCODE_PING: u8 = 0x9;
const OPCODE_PONG: u8 = 0xA;

/// Why the engine closed the session on its own (protective close).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WsViolation {
    /// Generic protocol error → close 1002.
    Protocol,
    /// Text payload was not valid UTF-8 → close 1007.
    InvalidUtf8,
    /// Reassembled message or frame exceeded a size cap → close 1009.
    TooBig,
}

impl WsViolation {
    const fn close_code(self) -> u16 {
        match self {
            Self::Protocol => 1002,
            Self::InvalidUtf8 => 1007,
            Self::TooBig => 1009,
        }
    }

    const fn close_reason(self) -> WebSocketCloseReason {
        match self {
            Self::Protocol | Self::InvalidUtf8 => WebSocketCloseReason::ProtocolError,
            // No fact reason exactly names "message too big"; an abnormal close
            // carrying code 1009 is the honest typed shape.
            Self::TooBig => WebSocketCloseReason::Abnormal,
        }
    }
}

/// One parsed frame header + payload (already unmasked).
struct DecodedFrame {
    fin: bool,
    opcode: u8,
    payload: Vec<u8>,
    total_len: usize,
}

/// Result of one parse attempt against the buffer head.
enum FrameStep {
    /// Not enough bytes yet.
    NeedMore,
    /// A complete frame.
    Frame(DecodedFrame),
    /// A protocol violation.
    Violation(WsViolation),
}

/// Pure WebSocket session decoder.
pub struct WebSocketSession {
    role: WebSocketRole,
    limits: WebSocketLimits,
    session: WebSocketSessionId,
    buffer: Vec<u8>,
    frag_opcode: Option<u8>,
    frag_payload: Vec<u8>,
    closed: bool,
    local_error_close: bool,
    app_messages: Vec<AppMessage>,
    facts: Vec<ProtocolFact>,
    close: Option<(Option<u16>, WebSocketCloseReason)>,
    pings: usize,
    pongs: usize,
    frames: usize,
    bytes_fed: usize,
}

impl WebSocketSession {
    /// Builds a session for one logical WebSocket connection.
    pub fn new(role: WebSocketRole, session: WebSocketSessionId, limits: WebSocketLimits) -> Self {
        Self {
            role,
            limits,
            session,
            buffer: Vec::new(),
            frag_opcode: None,
            frag_payload: Vec::new(),
            closed: false,
            local_error_close: false,
            app_messages: Vec::new(),
            facts: Vec::new(),
            close: None,
            pings: 0,
            pongs: 0,
            frames: 0,
            bytes_fed: 0,
        }
    }

    /// Feeds one ordered chunk of wire bytes and decodes what is now complete.
    pub fn feed(&mut self, chunk: &[u8]) {
        self.bytes_fed += chunk.len();
        if self.closed {
            // After a close, bytes on the wire are not delivered as app data.
            return;
        }
        self.buffer.extend_from_slice(chunk);
        self.drain();
    }

    fn drain(&mut self) {
        while !self.closed {
            if self.frames >= self.limits.max_frames {
                self.close_with(WsViolation::Protocol);
                return;
            }
            match parse_frame(&self.buffer, self.role, self.limits) {
                FrameStep::NeedMore => return,
                FrameStep::Violation(v) => {
                    self.close_with(v);
                    return;
                }
                FrameStep::Frame(frame) => {
                    self.buffer.drain(..frame.total_len);
                    self.frames += 1;
                    self.handle_frame(frame);
                }
            }
        }
    }

    fn handle_frame(&mut self, frame: DecodedFrame) {
        match frame.opcode {
            OPCODE_PING => self.pings += 1,
            OPCODE_PONG => self.pongs += 1,
            OPCODE_CLOSE => self.handle_close(frame.payload),
            OPCODE_TEXT | OPCODE_BINARY => self.handle_data_start(frame),
            OPCODE_CONTINUATION => self.handle_continuation(frame),
            // Unknown opcodes are rejected before reaching here.
            _ => self.close_with(WsViolation::Protocol),
        }
    }

    fn handle_data_start(&mut self, frame: DecodedFrame) {
        if self.frag_opcode.is_some() {
            // New data frame while a fragmented message is open.
            self.close_with(WsViolation::Protocol);
            return;
        }
        if frame.fin {
            self.complete_message(frame.opcode, frame.payload);
        } else {
            if frame.payload.len() > self.limits.max_message_bytes {
                self.close_with(WsViolation::TooBig);
                return;
            }
            self.frag_opcode = Some(frame.opcode);
            self.frag_payload = frame.payload;
        }
    }

    fn handle_continuation(&mut self, frame: DecodedFrame) {
        let Some(opcode) = self.frag_opcode else {
            // Continuation with nothing to continue.
            self.close_with(WsViolation::Protocol);
            return;
        };
        if self.frag_payload.len() + frame.payload.len() > self.limits.max_message_bytes {
            self.close_with(WsViolation::TooBig);
            return;
        }
        self.frag_payload.extend_from_slice(&frame.payload);
        if frame.fin {
            let payload = std::mem::take(&mut self.frag_payload);
            self.frag_opcode = None;
            self.complete_message(opcode, payload);
        }
    }

    fn complete_message(&mut self, opcode: u8, payload: Vec<u8>) {
        if payload.len() > self.limits.max_message_bytes {
            self.close_with(WsViolation::TooBig);
            return;
        }
        if opcode == OPCODE_TEXT {
            match String::from_utf8(payload) {
                Ok(text) => self.app_messages.push(AppMessage::Text(text)),
                // Invalid UTF-8 must never reach app code as a valid message.
                Err(_) => self.close_with(WsViolation::InvalidUtf8),
            }
        } else {
            self.app_messages.push(AppMessage::Binary(payload));
        }
    }

    fn handle_close(&mut self, payload: Vec<u8>) {
        // Close payload is 0 bytes, or a 2-byte code plus optional UTF-8 reason.
        if payload.len() == 1 {
            self.close_with(WsViolation::Protocol);
            return;
        }
        let code = if payload.len() >= 2 {
            Some(u16::from_be_bytes([payload[0], payload[1]]))
        } else {
            None
        };
        if let Some(code) = code {
            if !valid_close_code(code) {
                self.close_with(WsViolation::Protocol);
                return;
            }
            if payload.len() > 2 && std::str::from_utf8(&payload[2..]).is_err() {
                self.close_with(WsViolation::InvalidUtf8);
                return;
            }
        }
        let reason = close_reason_from_code(code);
        self.record_close(code, reason, false);
    }

    fn close_with(&mut self, violation: WsViolation) {
        let code = violation.close_code();
        let reason = violation.close_reason();
        self.record_close(Some(code), reason, true);
    }

    fn record_close(&mut self, code: Option<u16>, reason: WebSocketCloseReason, local_error: bool) {
        if self.closed {
            return;
        }
        self.closed = true;
        self.local_error_close = local_error;
        self.close = Some((code, reason));
        self.facts.push(ProtocolFact::WebSocketSessionClosed {
            session: self.session,
            reason,
            code,
        });
    }

    /// Finishes decoding and returns the typed run.
    pub fn finish(self) -> WebSocketRun {
        WebSocketRun {
            app_messages: self.app_messages,
            facts: self.facts,
            close: self.close,
            local_error_close: self.local_error_close,
            pings: self.pings,
            pongs: self.pongs,
            frames: self.frames,
            bytes_fed: self.bytes_fed,
            bytes_remaining: self.buffer.len(),
        }
    }
}

/// Typed outcome of running bytes through a [`WebSocketSession`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WebSocketRun {
    /// Valid messages delivered to app code, in order.
    pub app_messages: Vec<AppMessage>,
    /// Typed protocol facts emitted, in order.
    pub facts: Vec<ProtocolFact>,
    /// Final close, when the session closed: `(code, reason)`.
    pub close: Option<(Option<u16>, WebSocketCloseReason)>,
    /// Whether the close was a local protective close (vs a peer close).
    pub local_error_close: bool,
    /// Valid ping frames observed (not app deliveries).
    pub pings: usize,
    /// Valid pong frames observed (not app deliveries).
    pub pongs: usize,
    /// Frames decoded.
    pub frames: usize,
    /// Total wire bytes fed.
    pub bytes_fed: usize,
    /// Wire bytes left unparsed (a partial trailing frame).
    pub bytes_remaining: usize,
}

impl WebSocketRun {
    /// Builds a [`ProtocolChaosReport`] from this run.
    pub fn to_report(&self, name: &'static str) -> ProtocolChaosReport {
        let close_status = self
            .close
            .map(|(code, reason)| ProtocolCloseStatus::WebSocketClose { code, reason });
        // Terminal action is derived from the close reason, the same way
        // `WebSocketComplianceCase::expectation` derives it, so a report and
        // its expectation never disagree on a protocol-error close.
        let terminal_action = terminal_for(self.close, self.app_messages.is_empty());
        let mut report = ProtocolChaosReport::new(name, ProtocolChaosFamily::WebSocket);
        report.bytes_written = self.bytes_fed;
        report.peer_action = PeerAction::SentFrames;
        report.terminal_action = terminal_action;
        report.app_deliveries = self.app_messages.len();
        report.close_status = close_status;
        report.protocol_facts = self.facts.clone();
        report
    }
}

/// Parses the frame at the head of `buf`. Does not mutate `buf`; the caller
/// drains `total_len` bytes once a frame is returned.
fn parse_frame(buf: &[u8], role: WebSocketRole, limits: WebSocketLimits) -> FrameStep {
    if buf.len() < 2 {
        return FrameStep::NeedMore;
    }
    let b0 = buf[0];
    let b1 = buf[1];
    let fin = b0 & 0x80 != 0;
    let rsv = b0 & 0x70;
    let opcode = b0 & 0x0f;
    let masked = b1 & 0x80 != 0;
    let len_code = (b1 & 0x7f) as usize;

    // Reserved bits require a negotiated extension, which the engine never has.
    if rsv != 0 {
        return FrameStep::Violation(WsViolation::Protocol);
    }
    // Opcode must be a known data or control opcode.
    if !matches!(
        opcode,
        OPCODE_CONTINUATION
            | OPCODE_TEXT
            | OPCODE_BINARY
            | OPCODE_CLOSE
            | OPCODE_PING
            | OPCODE_PONG
    ) {
        return FrameStep::Violation(WsViolation::Protocol);
    }
    let is_control = opcode & 0x08 != 0;

    // Masking direction: client→server must be masked; server→client must not.
    let expect_masked = matches!(role, WebSocketRole::Server);
    if masked != expect_masked {
        return FrameStep::Violation(WsViolation::Protocol);
    }

    let mut cursor = 2;
    let payload_len = match len_code {
        126 => {
            if buf.len() < cursor + 2 {
                return FrameStep::NeedMore;
            }
            let len = u16::from_be_bytes([buf[cursor], buf[cursor + 1]]) as usize;
            cursor += 2;
            len
        }
        127 => {
            if buf.len() < cursor + 8 {
                return FrameStep::NeedMore;
            }
            let mut raw = [0u8; 8];
            raw.copy_from_slice(&buf[cursor..cursor + 8]);
            let len = u64::from_be_bytes(raw);
            // Most-significant bit must be 0 per RFC 6455.
            if len & 0x8000_0000_0000_0000 != 0 {
                return FrameStep::Violation(WsViolation::Protocol);
            }
            cursor += 8;
            // Clamp to usize range via the size cap so huge declared lengths do
            // not buffer forever.
            if len > limits.max_frame_payload as u64 {
                return FrameStep::Violation(WsViolation::TooBig);
            }
            len as usize
        }
        other => other,
    };

    // Control frames must be <= 125 bytes and not fragmented.
    if is_control {
        if payload_len > 125 {
            return FrameStep::Violation(WsViolation::Protocol);
        }
        if !fin {
            return FrameStep::Violation(WsViolation::Protocol);
        }
    } else if payload_len > limits.max_frame_payload {
        return FrameStep::Violation(WsViolation::TooBig);
    }

    if masked && buf.len() < cursor + 4 {
        return FrameStep::NeedMore;
    }
    let mask_key = if masked {
        let key = [
            buf[cursor],
            buf[cursor + 1],
            buf[cursor + 2],
            buf[cursor + 3],
        ];
        cursor += 4;
        Some(key)
    } else {
        None
    };

    let total_len = cursor + payload_len;
    if buf.len() < total_len {
        return FrameStep::NeedMore;
    }
    let mut payload = buf[cursor..total_len].to_vec();
    if let Some(key) = mask_key {
        for (index, byte) in payload.iter_mut().enumerate() {
            *byte ^= key[index % 4];
        }
    }
    FrameStep::Frame(DecodedFrame {
        fin,
        opcode,
        payload,
        total_len,
    })
}

/// Returns true when `code` is a close code a peer may send on the wire.
fn valid_close_code(code: u16) -> bool {
    matches!(code, 1000..=1003 | 1007..=1011 | 3000..=4999)
}

fn close_reason_from_code(code: Option<u16>) -> WebSocketCloseReason {
    match code {
        None => WebSocketCloseReason::Normal,
        Some(1000) => WebSocketCloseReason::Normal,
        Some(1001) => WebSocketCloseReason::GoingAway,
        Some(1002) => WebSocketCloseReason::ProtocolError,
        Some(_) => WebSocketCloseReason::Abnormal,
    }
}

/// Encodes one WebSocket frame for corpus/test authoring.
///
/// When `mask` is `Some`, the payload is masked with the given key (a client
/// frame). When `None`, the frame is unmasked (a server frame).
pub fn encode_frame(fin: bool, opcode: u8, payload: &[u8], mask: Option<[u8; 4]>) -> Vec<u8> {
    let mut out = Vec::with_capacity(payload.len() + 14);
    let b0 = if fin { 0x80 } else { 0x00 } | (opcode & 0x0f);
    out.push(b0);
    let mask_bit = if mask.is_some() { 0x80 } else { 0x00 };
    let len = payload.len();
    if len < 126 {
        out.push(mask_bit | len as u8);
    } else if len <= u16::MAX as usize {
        out.push(mask_bit | 126);
        out.extend_from_slice(&(len as u16).to_be_bytes());
    } else {
        out.push(mask_bit | 127);
        out.extend_from_slice(&(len as u64).to_be_bytes());
    }
    if let Some(key) = mask {
        out.extend_from_slice(&key);
        for (index, byte) in payload.iter().enumerate() {
            out.push(byte ^ key[index % 4]);
        }
    } else {
        out.extend_from_slice(payload);
    }
    out
}

/// A masked client frame.
pub fn client_frame(fin: bool, opcode: u8, payload: &[u8]) -> Vec<u8> {
    encode_frame(fin, opcode, payload, Some([0x37, 0xfa, 0x21, 0x3d]))
}

/// An unmasked server frame.
pub fn server_frame(fin: bool, opcode: u8, payload: &[u8]) -> Vec<u8> {
    encode_frame(fin, opcode, payload, None)
}

/// Encodes a Close frame body: 2-byte code plus optional UTF-8 reason.
pub fn close_payload(code: u16, reason: &str) -> Vec<u8> {
    let mut out = code.to_be_bytes().to_vec();
    out.extend_from_slice(reason.as_bytes());
    out
}

/// One hermetic WebSocket compliance case.
///
/// Each case names its input bytes, the exact app messages that must reach app
/// code, the expected close (code + typed reason), and the expected protocol
/// facts. Running the case produces a [`ProtocolChaosReport`] whose counters
/// are derived from those expectations.
#[derive(Debug, Clone)]
pub struct WebSocketComplianceCase {
    /// Stable case name.
    pub name: &'static str,
    /// Which side's frames are being decoded.
    pub role: WebSocketRole,
    /// Decode limits.
    pub limits: WebSocketLimits,
    /// Raw input bytes.
    pub input: Vec<u8>,
    /// Exact app messages expected to be delivered.
    pub expected_app_messages: Vec<AppMessage>,
    /// Expected close `(code, reason)`, when the session should close.
    pub expected_close: Option<(Option<u16>, WebSocketCloseReason)>,
    /// Expected typed protocol facts, in order.
    pub expected_facts: Vec<ProtocolFact>,
}

impl WebSocketComplianceCase {
    /// Runs the case input through a fresh session.
    pub fn run(&self) -> WebSocketRun {
        let mut session = WebSocketSession::new(self.role, WebSocketSessionId::new(1), self.limits);
        session.feed(&self.input);
        session.finish()
    }

    /// Derives the chaos expectation from the named app/close/fact data.
    pub fn expectation(&self) -> ProtocolChaosExpectation {
        let close_status = self
            .expected_close
            .map(|(code, reason)| ProtocolCloseStatus::WebSocketClose { code, reason });
        let terminal_action =
            terminal_for(self.expected_close, self.expected_app_messages.is_empty());
        ProtocolChaosExpectation {
            family: ProtocolChaosFamily::WebSocket,
            app_deliveries: self.expected_app_messages.len(),
            terminal_action,
            close_status,
            protocol_facts: self.expected_facts.clone(),
        }
    }

    /// Runs the case and asserts the run matches every expectation.
    ///
    /// Checks the exact app message content (so "valid data reaches app once"
    /// and "invalid bytes never reach app" are both provable), the close, the
    /// typed facts, and the derived chaos report counters.
    pub fn check(&self) -> Result<ProtocolChaosReport, Box<WebSocketComplianceMismatch>> {
        let run = self.run();
        let mut diverged = Vec::new();
        if run.app_messages != self.expected_app_messages {
            diverged.push("app_messages");
        }
        // Normalise close to `(code, reason)` for comparison.
        if run.close != self.expected_close {
            diverged.push("close");
        }
        if run.facts != self.expected_facts {
            diverged.push("facts");
        }
        if diverged.is_empty() {
            Ok(run.to_report(self.name))
        } else {
            Err(Box::new(WebSocketComplianceMismatch {
                name: self.name,
                diverged,
                expected_app_messages: self.expected_app_messages.clone(),
                actual_app_messages: run.app_messages,
                expected_close: self.expected_close,
                actual_close: run.close,
                expected_facts: self.expected_facts.clone(),
                actual_facts: run.facts,
            }))
        }
    }
}

fn is_error_reason(reason: WebSocketCloseReason) -> bool {
    matches!(
        reason,
        WebSocketCloseReason::ProtocolError
            | WebSocketCloseReason::Abnormal
            | WebSocketCloseReason::SlowPeer
    )
}

/// Maps a close + whether app messages were delivered to a terminal action.
///
/// Shared by [`WebSocketRun::to_report`] and
/// [`WebSocketComplianceCase::expectation`] so a report and its expectation
/// always agree, including on a protocol-error close.
fn terminal_for(
    close: Option<(Option<u16>, WebSocketCloseReason)>,
    app_empty: bool,
) -> TerminalAction {
    match (close, app_empty) {
        (Some((_, reason)), _) if is_error_reason(reason) => TerminalAction::Rejected,
        (Some(_), true) => TerminalAction::ServerClosed,
        (Some(_), false) => TerminalAction::DeliveredAndClosed,
        (None, true) => TerminalAction::None,
        (None, false) => TerminalAction::Delivered,
    }
}

/// Why a [`WebSocketComplianceCase`] diverged from its expectation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WebSocketComplianceMismatch {
    /// Case name.
    pub name: &'static str,
    /// Which parts diverged.
    pub diverged: Vec<&'static str>,
    /// Expected app messages.
    pub expected_app_messages: Vec<AppMessage>,
    /// Observed app messages.
    pub actual_app_messages: Vec<AppMessage>,
    /// Expected close.
    pub expected_close: Option<(Option<u16>, WebSocketCloseReason)>,
    /// Observed close.
    pub actual_close: Option<(Option<u16>, WebSocketCloseReason)>,
    /// Expected facts.
    pub expected_facts: Vec<ProtocolFact>,
    /// Observed facts.
    pub actual_facts: Vec<ProtocolFact>,
}

impl std::fmt::Display for WebSocketComplianceMismatch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        writeln!(
            f,
            "websocket compliance case `{}` diverged: {:?}",
            self.name, self.diverged
        )?;
        writeln!(
            f,
            "  app messages: expected {:?}, got {:?}",
            self.expected_app_messages, self.actual_app_messages
        )?;
        writeln!(
            f,
            "  close: expected {:?}, got {:?}",
            self.expected_close, self.actual_close
        )?;
        writeln!(
            f,
            "  facts: expected {:?}, got {:?}",
            self.expected_facts, self.actual_facts
        )
    }
}

impl std::error::Error for WebSocketComplianceMismatch {}

fn ws_closed(code: u16, reason: WebSocketCloseReason) -> ProtocolFact {
    ProtocolFact::WebSocketSessionClosed {
        session: WebSocketSessionId::new(1),
        reason,
        code: Some(code),
    }
}

/// The hermetic WebSocket compliance corpus.
///
/// Small, deterministic, and bounded — it is the CI-sized slice the proof
/// targets run. Every case names what reaches app code and what the session
/// does instead when the bytes are malformed.
pub fn compliance_corpus() -> Vec<WebSocketComplianceCase> {
    let limits = WebSocketLimits::default();
    vec![
        // 1. Valid single text frame, delivered once.
        WebSocketComplianceCase {
            name: "ws_valid_text",
            role: WebSocketRole::Server,
            limits,
            input: client_frame(true, OPCODE_TEXT, b"hello"),
            expected_app_messages: vec![AppMessage::Text("hello".to_owned())],
            expected_close: None,
            expected_facts: vec![],
        },
        // 2. Valid single binary frame.
        WebSocketComplianceCase {
            name: "ws_valid_binary",
            role: WebSocketRole::Server,
            limits,
            input: client_frame(true, OPCODE_BINARY, &[0x01, 0x02, 0x03]),
            expected_app_messages: vec![AppMessage::Binary(vec![0x01, 0x02, 0x03])],
            expected_close: None,
            expected_facts: vec![],
        },
        // 3. Valid fragmented text: "Hel" + "lo" reaches app once after reassembly.
        WebSocketComplianceCase {
            name: "ws_valid_fragmented_text",
            role: WebSocketRole::Server,
            limits,
            input: {
                let mut bytes = client_frame(false, OPCODE_TEXT, b"Hel");
                bytes.extend(client_frame(true, OPCODE_CONTINUATION, b"lo"));
                bytes
            },
            expected_app_messages: vec![AppMessage::Text("Hello".to_owned())],
            expected_close: None,
            expected_facts: vec![],
        },
        // 4. Valid fragmented text splitting a multibyte codepoint across frames.
        WebSocketComplianceCase {
            name: "ws_valid_fragmented_split_codepoint",
            role: WebSocketRole::Server,
            limits,
            input: {
                // 'Ã' (U+00C3) encodes as 0xC3 0x83.
                let mut bytes = client_frame(false, OPCODE_TEXT, &[0xC3]);
                bytes.extend(client_frame(true, OPCODE_CONTINUATION, &[0x83]));
                bytes
            },
            expected_app_messages: vec![AppMessage::Text("\u{00C3}".to_owned())],
            expected_close: None,
            expected_facts: vec![],
        },
        // 5. Invalid UTF-8 across fragments: never delivered, close 1007.
        WebSocketComplianceCase {
            name: "ws_invalid_utf8_across_fragments",
            role: WebSocketRole::Server,
            limits,
            input: {
                let mut bytes = client_frame(false, OPCODE_TEXT, &[0xC3]);
                bytes.extend(client_frame(true, OPCODE_CONTINUATION, &[0x28]));
                bytes
            },
            expected_app_messages: vec![],
            expected_close: Some((Some(1007), WebSocketCloseReason::ProtocolError)),
            expected_facts: vec![ws_closed(1007, WebSocketCloseReason::ProtocolError)],
        },
        // 6. Reserved bits set without an extension: protocol error 1002.
        WebSocketComplianceCase {
            name: "ws_reserved_bits_without_extension",
            role: WebSocketRole::Server,
            limits,
            input: {
                let mut bytes = client_frame(true, OPCODE_TEXT, b"x");
                bytes[0] |= 0x40; // set RSV1
                bytes
            },
            expected_app_messages: vec![],
            expected_close: Some((Some(1002), WebSocketCloseReason::ProtocolError)),
            expected_facts: vec![ws_closed(1002, WebSocketCloseReason::ProtocolError)],
        },
        // 7. Oversized control frame (ping > 125 bytes): protocol error 1002.
        WebSocketComplianceCase {
            name: "ws_oversized_control_frame",
            role: WebSocketRole::Server,
            limits,
            input: client_frame(true, OPCODE_PING, &[0x61; 200]),
            expected_app_messages: vec![],
            expected_close: Some((Some(1002), WebSocketCloseReason::ProtocolError)),
            expected_facts: vec![ws_closed(1002, WebSocketCloseReason::ProtocolError)],
        },
        // 8. Oversized message: a single frame over the message cap, close 1009.
        WebSocketComplianceCase {
            name: "ws_oversized_message",
            role: WebSocketRole::Server,
            limits: WebSocketLimits {
                max_message_bytes: 8,
                max_frame_payload: 8,
                max_frames: 64,
            },
            input: client_frame(true, OPCODE_TEXT, b"way too long for the cap"),
            expected_app_messages: vec![],
            expected_close: Some((Some(1009), WebSocketCloseReason::Abnormal)),
            expected_facts: vec![ws_closed(1009, WebSocketCloseReason::Abnormal)],
        },
        // 9. Masked server frame: decoding server→client, a masked frame is illegal.
        WebSocketComplianceCase {
            name: "ws_masked_server_frame",
            role: WebSocketRole::Client,
            limits,
            input: client_frame(true, OPCODE_TEXT, b"masked"),
            expected_app_messages: vec![],
            expected_close: Some((Some(1002), WebSocketCloseReason::ProtocolError)),
            expected_facts: vec![ws_closed(1002, WebSocketCloseReason::ProtocolError)],
        },
        // 10. Unmasked client frame: decoding client→server, an unmasked frame is illegal.
        WebSocketComplianceCase {
            name: "ws_unmasked_client_frame",
            role: WebSocketRole::Server,
            limits,
            input: server_frame(true, OPCODE_TEXT, b"unmasked"),
            expected_app_messages: vec![],
            expected_close: Some((Some(1002), WebSocketCloseReason::ProtocolError)),
            expected_facts: vec![ws_closed(1002, WebSocketCloseReason::ProtocolError)],
        },
        // 11. Ping/pong edge: a valid ping is not an app delivery and does not close.
        WebSocketComplianceCase {
            name: "ws_ping_pong_edge",
            role: WebSocketRole::Server,
            limits,
            input: {
                let mut bytes = client_frame(true, OPCODE_PING, b"hb");
                bytes.extend(client_frame(true, OPCODE_TEXT, b"after-ping"));
                bytes
            },
            expected_app_messages: vec![AppMessage::Text("after-ping".to_owned())],
            expected_close: None,
            expected_facts: vec![],
        },
        // 12. Close handshake edge: a valid peer close 1000 ends the session cleanly.
        WebSocketComplianceCase {
            name: "ws_close_handshake_edge",
            role: WebSocketRole::Server,
            limits,
            input: client_frame(true, OPCODE_CLOSE, &close_payload(1000, "bye")),
            expected_app_messages: vec![],
            expected_close: Some((Some(1000), WebSocketCloseReason::Normal)),
            expected_facts: vec![ws_closed(1000, WebSocketCloseReason::Normal)],
        },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_compliance_case_matches_its_expectation() {
        for case in compliance_corpus() {
            let report = case.check().unwrap_or_else(|mismatch| panic!("{mismatch}"));
            // The derived chaos expectation also matches the produced report.
            case.expectation()
                .check(&report)
                .unwrap_or_else(|mismatch| {
                    panic!("chaos expectation for {}: {mismatch}", case.name)
                });
        }
    }

    #[test]
    fn valid_fragmented_text_reaches_app_exactly_once() {
        let case = compliance_corpus()
            .into_iter()
            .find(|c| c.name == "ws_valid_fragmented_text")
            .expect("case present");
        let run = case.run();
        assert_eq!(run.app_messages.len(), 1);
        assert_eq!(run.app_messages[0], AppMessage::Text("Hello".to_owned()));
        assert!(run.close.is_none());
    }

    #[test]
    fn invalid_bytes_never_reach_app_as_valid_data() {
        for name in [
            "ws_invalid_utf8_across_fragments",
            "ws_reserved_bits_without_extension",
            "ws_oversized_control_frame",
            "ws_oversized_message",
            "ws_masked_server_frame",
            "ws_unmasked_client_frame",
        ] {
            let case = compliance_corpus()
                .into_iter()
                .find(|c| c.name == name)
                .expect("case present");
            let run = case.run();
            assert!(
                run.app_messages.is_empty(),
                "{name}: malformed bytes must not deliver an app message, got {:?}",
                run.app_messages
            );
            assert!(run.close.is_some(), "{name}: must close");
            assert!(run.local_error_close, "{name}: must be a protective close");
        }
    }

    #[test]
    fn close_code_fact_and_report_counters_match() {
        let case = compliance_corpus()
            .into_iter()
            .find(|c| c.name == "ws_invalid_utf8_across_fragments")
            .expect("case present");
        let report = case.check().expect("matches");
        assert_eq!(report.app_deliveries, 0);
        assert_eq!(report.fact_count(), 1);
        assert_eq!(
            report.close_status,
            Some(ProtocolCloseStatus::WebSocketClose {
                code: Some(1007),
                reason: WebSocketCloseReason::ProtocolError,
            })
        );
        assert_eq!(report.terminal_action, TerminalAction::Rejected);
    }

    #[test]
    fn partial_frame_is_buffered_not_misparsed() {
        // Feed a text frame one byte at a time; it must deliver once, intact.
        let frame = client_frame(true, OPCODE_TEXT, b"chunked");
        let mut session = WebSocketSession::new(
            WebSocketRole::Server,
            WebSocketSessionId::new(1),
            WebSocketLimits::default(),
        );
        for byte in &frame {
            session.feed(&[*byte]);
        }
        let run = session.finish();
        assert_eq!(
            run.app_messages,
            vec![AppMessage::Text("chunked".to_owned())]
        );
        assert_eq!(run.bytes_remaining, 0);
        assert!(run.close.is_none());
    }

    #[test]
    fn continuation_without_start_is_rejected() {
        let case = WebSocketComplianceCase {
            name: "ws_orphan_continuation",
            role: WebSocketRole::Server,
            limits: WebSocketLimits::default(),
            input: client_frame(true, OPCODE_CONTINUATION, b"orphan"),
            expected_app_messages: vec![],
            expected_close: Some((Some(1002), WebSocketCloseReason::ProtocolError)),
            expected_facts: vec![ws_closed(1002, WebSocketCloseReason::ProtocolError)],
        };
        case.check().expect("orphan continuation closes 1002");
    }

    #[test]
    fn new_data_frame_during_fragmentation_is_rejected() {
        let mut input = client_frame(false, OPCODE_TEXT, b"start");
        input.extend(client_frame(true, OPCODE_TEXT, b"interrupt"));
        let case = WebSocketComplianceCase {
            name: "ws_interleaved_data",
            role: WebSocketRole::Server,
            limits: WebSocketLimits::default(),
            input,
            expected_app_messages: vec![],
            expected_close: Some((Some(1002), WebSocketCloseReason::ProtocolError)),
            expected_facts: vec![ws_closed(1002, WebSocketCloseReason::ProtocolError)],
        };
        case.check().expect("interleaved data frame closes 1002");
    }

    #[test]
    fn one_byte_close_payload_is_protocol_error() {
        let case = WebSocketComplianceCase {
            name: "ws_short_close",
            role: WebSocketRole::Server,
            limits: WebSocketLimits::default(),
            input: client_frame(true, OPCODE_CLOSE, &[0x03]),
            expected_app_messages: vec![],
            expected_close: Some((Some(1002), WebSocketCloseReason::ProtocolError)),
            expected_facts: vec![ws_closed(1002, WebSocketCloseReason::ProtocolError)],
        };
        case.check().expect("1-byte close payload closes 1002");
    }
}
