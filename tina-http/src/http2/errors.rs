//! HTTP/2 protocol errors and wire-error-code constants.
//!
//! These are shared between the server isolate and the native client
//! isolate. The module is internal; the public surface re-exports
//! [`Http2ProtocolError`] through `http2/mod.rs`.

use tina_runtime::Http2ResetReason;

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
