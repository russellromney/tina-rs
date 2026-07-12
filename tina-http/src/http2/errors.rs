//! HTTP/2 protocol errors and wire-error-code constants.
//!
//! These are shared between the server isolate and the native client
//! isolate. The module is internal; the public surface re-exports
//! [`Http2ProtocolError`] through `http2/mod.rs`.

use tina_runtime::Http2ResetReason;

/// Why caller-supplied HTTP/2 limits cannot construct a connection.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Http2ConfigError {
    /// `max_frame_size` is outside the RFC 9113 SETTINGS range.
    FrameSizeOutOfRange { value: usize },
    /// `max_concurrent_streams` does not fit its 32-bit SETTINGS value.
    ConcurrentStreamsOutOfRange { value: usize },
    /// The initial connection receive window is below HTTP/2's fixed default.
    InitialConnectionWindowTooSmall { value: i32 },
    /// The initial stream receive window is negative.
    InitialStreamWindowNegative { value: i32 },
    /// The connection's outbound frame queue has zero capacity.
    ZeroOutboundQueueCapacity,
    /// The client's pre-connect submit queue has zero capacity.
    ZeroPreConnectSubmitCapacity,
    /// The server's request-stream chunk size is zero.
    ZeroRequestStreamChunkSize,
    /// A connection isolate mailbox has zero capacity.
    ZeroConnectionMailboxCapacity,
    /// The server listener mailbox has zero capacity.
    ZeroListenerMailboxCapacity,
    /// The TLS I/O deadline is zero.
    ZeroTlsIoTimeout,
}

impl std::fmt::Display for Http2ConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::FrameSizeOutOfRange { value } => write!(
                f,
                "max_frame_size must be in 16384..=16777215 (got {value})"
            ),
            Self::ConcurrentStreamsOutOfRange { value } => {
                write!(f, "max_concurrent_streams must fit u32 (got {value})")
            }
            Self::InitialConnectionWindowTooSmall { value } => write!(
                f,
                "initial_connection_window must be at least 65535 (got {value})"
            ),
            Self::InitialStreamWindowNegative { value } => {
                write!(
                    f,
                    "initial_stream_window must be non-negative (got {value})"
                )
            }
            Self::ZeroOutboundQueueCapacity => {
                f.write_str("connection_outbound_queue_capacity must be positive")
            }
            Self::ZeroPreConnectSubmitCapacity => {
                f.write_str("pre_connect_submit_capacity must be positive")
            }
            Self::ZeroRequestStreamChunkSize => {
                f.write_str("request_stream_chunk_size must be positive")
            }
            Self::ZeroConnectionMailboxCapacity => {
                f.write_str("connection_mailbox_capacity must be positive")
            }
            Self::ZeroListenerMailboxCapacity => {
                f.write_str("listener_mailbox_capacity must be positive")
            }
            Self::ZeroTlsIoTimeout => f.write_str("tls_io_timeout must be non-zero"),
        }
    }
}

impl std::error::Error for Http2ConfigError {}

/// HTTP/2 wire error codes (RFC 9113 §7).
pub(super) const ERR_NO_ERROR: u32 = 0x0;
pub(super) const ERR_PROTOCOL_ERROR: u32 = 0x1;
pub(super) const ERR_FLOW_CONTROL_ERROR: u32 = 0x3;
pub(super) const ERR_SETTINGS_ERROR: u32 = 0x4;
pub(super) const ERR_STREAM_CLOSED: u32 = 0x5;
pub(super) const ERR_FRAME_SIZE_ERROR: u32 = 0x6;
pub(super) const ERR_REFUSED_STREAM: u32 = 0x7;
#[allow(dead_code)]
pub(super) const ERR_CANCEL: u32 = 0x8;
pub(super) const ERR_ENHANCE_YOUR_CALM: u32 = 0xb;

/// Protocol/lifecycle errors surfaced by the frame and connection layers.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Http2ProtocolError {
    BadPreface,
    FrameTooLarge {
        len: usize,
        max: usize,
    },
    TruncatedFrame,
    BadFrameLength,
    BadStreamId,
    HeadersTooLarge,
    HpackUnsupported,
    InvalidPseudoHeaders,
    /// HEADERS / trailers carried a pseudo-header forbidden in that
    /// position (e.g., `:status` in trailers).
    InvalidTrailerPseudoHeader,
    /// DATA arrived before any HEADERS were received on the stream.
    /// Connection-level protocol error per RFC 9113 §8.1.
    DataBeforeHeaders,
    StreamClosed,
    StreamLimitFull,
    WindowOverflow,
    FlowControl,
    RequestTrailersUnsupported,
    SettingsUnsupported,
    InvalidSettingsValue,
    UnsupportedFrame(u8),
    /// Standalone or otherwise-unexpected `CONTINUATION` frame; full
    /// continuation support is not implemented, so any such frame is a
    /// connection-level protocol error.
    UnexpectedContinuation,
    /// HTTP/2 `content-length` header was malformed, conflicted across
    /// duplicates, or DATA bytes did not match the declared length.
    ContentLengthMismatch,
    /// Response body bytes exceeded the client cap. Distinct from
    /// `HeadersTooLarge` so callers can tell apart a too-large head from
    /// a too-large body.
    BodyTooLarge {
        cap_bytes: usize,
    },
    /// Outbound HEADERS block does not fit in one frame and CONTINUATION
    /// is not supported by this first form. Stream id was not consumed.
    OutboundHeadersTooLarge,
    /// Stream id space exhausted (2^31 client streams). The connection
    /// must be retired; a new one needs to be opened for further work.
    StreamIdExhausted,
}

/// Maps an HTTP/2 wire error code to the typed [`Http2ResetReason`].
pub(super) fn classify_h2_reset(code: u32) -> Http2ResetReason {
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
