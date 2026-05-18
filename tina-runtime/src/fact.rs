//! Replayable protocol/runtime facts.
//!
//! A *fact* is one named, typed observation a Tina isolate can emit alongside
//! ordinary effects. Facts ride through [`tina::Effect::Fact`] and surface in
//! the trace as [`crate::RuntimeEventKind::FactObserved`] events. Unlike a
//! report counter, a fact is part of replay truth: a deterministic substrate
//! sees the same fact in the same order, and a replay projection that drops or
//! adds a fact fails closed.
//!
//! The closed [`RuntimeFact`] enum is small on purpose. New fact families are
//! added as new top-level variants with a fresh stable tag; existing tags must
//! not be renumbered.
//!
//! Ordinary isolates declare `type Fact = std::convert::Infallible` and never
//! emit a fact. The `IntoRuntimeFact` bound on the runtime registration path
//! enforces that any fact type an isolate actually uses must convert into
//! [`RuntimeFact`] losslessly. The `Infallible` impl is unreachable, so
//! ordinary code pays zero runtime cost.

use std::convert::Infallible;

/// Closed set of replay-visible facts a Tina isolate can emit.
///
/// New top-level fact families append a new variant with a fresh stable tag.
/// Adding a variant must not move existing tags; the stable-tag test in the
/// runtime trace module proves this.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum RuntimeFact {
    /// A protocol-layer fact (HTTP/2, HTTP body, WebSocket, gRPC).
    Protocol(ProtocolFact),
}

/// Lossless conversion from an isolate's `Fact` type into [`RuntimeFact`].
///
/// The runtime/sim registration path requires this bound on `I::Fact` so any
/// fact an isolate actually emits is guaranteed to land in the replay trace
/// in one canonical shape. Ordinary isolates use `Fact = Infallible`; the
/// bundled impl below is unreachable.
pub trait IntoRuntimeFact {
    /// Converts `self` into the trace-visible [`RuntimeFact`].
    fn into_runtime_fact(self) -> RuntimeFact;
}

impl IntoRuntimeFact for Infallible {
    fn into_runtime_fact(self) -> RuntimeFact {
        match self {}
    }
}

impl IntoRuntimeFact for RuntimeFact {
    fn into_runtime_fact(self) -> RuntimeFact {
        self
    }
}

impl IntoRuntimeFact for ProtocolFact {
    fn into_runtime_fact(self) -> RuntimeFact {
        RuntimeFact::Protocol(self)
    }
}

/// Direction of a protocol message relative to the local peer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ProtocolDirection {
    /// The local peer sent the message.
    Outbound,
    /// The local peer received the message.
    Inbound,
}

/// Local-only protocol connection id. The runtime/sim does not own real
/// sockets here; this id only correlates fact events that share a connection
/// inside one trace.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ProtocolConnectionId(u64);

impl ProtocolConnectionId {
    /// Creates a connection id from a raw integer.
    pub const fn new(raw: u64) -> Self {
        Self(raw)
    }

    /// Returns the raw connection id.
    pub const fn get(self) -> u64 {
        self.0
    }
}

/// HTTP/2 stream id as carried on the wire.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Http2StreamId(u32);

impl Http2StreamId {
    /// Creates an HTTP/2 stream id from a raw integer.
    pub const fn new(raw: u32) -> Self {
        Self(raw)
    }

    /// Returns the raw stream id.
    pub const fn get(self) -> u32 {
        self.0
    }
}

/// HTTP/2 RST_STREAM error code that closed a stream.
///
/// Closed variant set covering the codes the rest of `tina-http` emits today.
/// Future codes append at the end and pick up a fresh stable tag.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Http2ResetReason {
    /// Peer or local stack reset the stream cleanly with `NO_ERROR`.
    NoError,
    /// Stream had a protocol error.
    ProtocolError,
    /// Stream was refused.
    RefusedStream,
    /// Stream was cancelled.
    Cancel,
    /// Internal error.
    InternalError,
    /// Flow-control violation.
    FlowControlError,
    /// Settings timeout.
    SettingsTimeout,
    /// Stream not yet ready for the frame received.
    StreamClosed,
    /// Frame size error.
    FrameSizeError,
    /// HTTP/1.1 required.
    Http11Required,
    /// Compression error.
    CompressionError,
    /// Connect error during CONNECT tunnel.
    ConnectError,
    /// Enhance your calm.
    EnhanceYourCalm,
    /// Inadequate security.
    InadequateSecurity,
    /// Reset reason captured by the local stack that does not match any of the
    /// named codes above.
    OtherCode(u32),
}

/// HTTP/2 stream close reason (clean lifecycle close, not RST_STREAM).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Http2CloseReason {
    /// Both halves saw END_STREAM cleanly.
    EndStream,
    /// Local side closed; remote END_STREAM not yet observed.
    LocalCloseOnly,
    /// Remote side closed; local side already half-closed.
    RemoteCloseOnly,
    /// Connection went away (GOAWAY) while the stream was still open.
    GoAway,
}

/// Side of HTTP/2 flow-control window exhaustion observed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Http2FlowControlSide {
    /// The connection-level send window is at zero.
    ConnectionSend,
    /// The connection-level receive window is at zero.
    ConnectionReceive,
    /// The per-stream send window is at zero.
    StreamSend,
    /// The per-stream receive window is at zero.
    StreamReceive,
}

/// WebSocket session id used to correlate fact events for one logical session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct WebSocketSessionId(u64);

impl WebSocketSessionId {
    /// Creates a session id from a raw integer.
    pub const fn new(raw: u64) -> Self {
        Self(raw)
    }

    /// Returns the raw session id.
    pub const fn get(self) -> u64 {
        self.0
    }
}

/// Reason a WebSocket session closed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum WebSocketCloseReason {
    /// Peer initiated a normal close.
    Normal,
    /// Peer went away.
    GoingAway,
    /// Protocol error.
    ProtocolError,
    /// Local side initiated close.
    LocalInitiated,
    /// Outbound queue would have exceeded its budget; the slow-peer guard
    /// closed the session.
    SlowPeer,
    /// Session closed because of an idle timeout.
    IdleTimeout,
    /// Generic abnormal close that did not match the named cases.
    Abnormal,
}

/// gRPC stream id, used to correlate gRPC fact events with their HTTP/2 stream
/// when the carrier is HTTP/2.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct GrpcStreamId(u64);

impl GrpcStreamId {
    /// Creates a gRPC stream id from a raw integer.
    pub const fn new(raw: u64) -> Self {
        Self(raw)
    }

    /// Returns the raw stream id.
    pub const fn get(self) -> u64 {
        self.0
    }
}

/// gRPC final status code.
///
/// Mirrors the well-known canonical gRPC status code set. New codes append at
/// the end and pick up a fresh stable tag.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum GrpcStatusCode {
    /// `OK` (0).
    Ok,
    /// `CANCELLED` (1).
    Cancelled,
    /// `UNKNOWN` (2).
    Unknown,
    /// `INVALID_ARGUMENT` (3).
    InvalidArgument,
    /// `DEADLINE_EXCEEDED` (4).
    DeadlineExceeded,
    /// `NOT_FOUND` (5).
    NotFound,
    /// `ALREADY_EXISTS` (6).
    AlreadyExists,
    /// `PERMISSION_DENIED` (7).
    PermissionDenied,
    /// `RESOURCE_EXHAUSTED` (8).
    ResourceExhausted,
    /// `FAILED_PRECONDITION` (9).
    FailedPrecondition,
    /// `ABORTED` (10).
    Aborted,
    /// `OUT_OF_RANGE` (11).
    OutOfRange,
    /// `UNIMPLEMENTED` (12).
    Unimplemented,
    /// `INTERNAL` (13).
    Internal,
    /// `UNAVAILABLE` (14).
    Unavailable,
    /// `DATA_LOSS` (15).
    DataLoss,
    /// `UNAUTHENTICATED` (16).
    Unauthenticated,
}

/// Closed set of protocol-layer facts emitted by Tina protocol isolates.
///
/// Variants are stable: a new fact must append to the end and pick up a fresh
/// tag in the runtime trace module's `protocol_fact_tag` helper. Reordering
/// variants would break replay hashes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum ProtocolFact {
    /// One HTTP/2 stream opened on a connection.
    Http2StreamOpened {
        /// Local connection id.
        connection: ProtocolConnectionId,
        /// HTTP/2 stream id.
        stream: Http2StreamId,
        /// Which peer opened the stream relative to the local side.
        direction: ProtocolDirection,
    },

    /// One HTTP/2 stream closed cleanly.
    Http2StreamClosed {
        /// Local connection id.
        connection: ProtocolConnectionId,
        /// HTTP/2 stream id.
        stream: Http2StreamId,
        /// Why the stream closed.
        reason: Http2CloseReason,
    },

    /// One HTTP/2 stream was reset (RST_STREAM).
    Http2StreamReset {
        /// Local connection id.
        connection: ProtocolConnectionId,
        /// HTTP/2 stream id.
        stream: Http2StreamId,
        /// Which side sent the reset.
        direction: ProtocolDirection,
        /// Error code carried by the reset.
        reason: Http2ResetReason,
    },

    /// HTTP/2 flow-control window reached zero on one side.
    Http2FlowControlFull {
        /// Local connection id.
        connection: ProtocolConnectionId,
        /// HTTP/2 stream id, or 0 for connection-level.
        stream: Http2StreamId,
        /// Which window emptied.
        side: Http2FlowControlSide,
    },

    /// HTTP body stream observed a high-water mark on its in-memory buffer.
    HttpBodyHighWater {
        /// Local connection id.
        connection: ProtocolConnectionId,
        /// Per-connection body id (separate from HTTP/2 stream id so HTTP/1
        /// bodies can also be named).
        body_id: u64,
        /// Direction of the body relative to the local peer.
        direction: ProtocolDirection,
        /// Number of buffered bytes at the moment the high-water guard fired.
        buffered_bytes: u64,
        /// Configured high-water threshold for this body.
        threshold_bytes: u64,
    },

    /// WebSocket session was closed because the peer fell behind its outbound
    /// budget.
    WebSocketSlowPeerClosed {
        /// WebSocket session id.
        session: WebSocketSessionId,
        /// Number of frames queued for the slow peer when the guard fired.
        queued_frames: u32,
        /// Number of queued bytes at that moment.
        queued_bytes: u64,
    },

    /// WebSocket session closed for any reason.
    WebSocketSessionClosed {
        /// WebSocket session id.
        session: WebSocketSessionId,
        /// Reason the session closed.
        reason: WebSocketCloseReason,
        /// Local-initiated close code, if any.
        code: Option<u16>,
    },

    /// Native gRPC server sent a final status frame to the client.
    GrpcFinalStatusSent {
        /// Local HTTP/2 connection id carrying this gRPC stream.
        connection: ProtocolConnectionId,
        /// gRPC stream id.
        stream: GrpcStreamId,
        /// Final status code.
        status: GrpcStatusCode,
    },
}

impl ProtocolFact {
    /// Returns a short, stable debug/render token for the fact family.
    pub const fn family(self) -> ProtocolFamily {
        match self {
            Self::Http2StreamOpened { .. }
            | Self::Http2StreamClosed { .. }
            | Self::Http2StreamReset { .. }
            | Self::Http2FlowControlFull { .. } => ProtocolFamily::Http2,
            Self::HttpBodyHighWater { .. } => ProtocolFamily::HttpBody,
            Self::WebSocketSlowPeerClosed { .. } | Self::WebSocketSessionClosed { .. } => {
                ProtocolFamily::WebSocket
            }
            Self::GrpcFinalStatusSent { .. } => ProtocolFamily::Grpc,
        }
    }
}

/// Stable family token used by trace-aware tooling and projection presets.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum ProtocolFamily {
    /// HTTP/2 connection or stream level.
    Http2,
    /// HTTP request or response body, transport-independent.
    HttpBody,
    /// WebSocket session level.
    WebSocket,
    /// Native gRPC request/response.
    Grpc,
}
