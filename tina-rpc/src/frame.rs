//! Wire frame format and codec.
//!
//! All multi-byte integers are big-endian. Wire layout:
//!
//! ```text
//! [u32 length]                         // total bytes in body that follow
//!    ↓ body (length bytes):
//! [u8  version]                        // = FRAME_VERSION_V1 (1)
//! [u8  kind]                           // 0=Request, 1=Reply, 2=Error
//! [u8  error_code]                     // 0=None; 1..=6 only when kind=Error
//! [u64 request_id]                     // 8 bytes
//! [u8  service_len][service_bytes]     // service_len ∈ 0..=255, UTF-8
//! [u8  method_len ][method_bytes]      // method_len  ∈ 0..=255, UTF-8
//! [payload bytes]                      // length - (13 + service_len + method_len)
//! ```
//!
//! # Allocation safety
//!
//! Decode order is fixed: read the four-byte length prefix, validate against
//! `max_frame_size`, *then* allocate the body buffer. Inside the body, every
//! variable-length field carries an explicit length byte that is validated
//! against the remaining body bytes before any string or payload allocation.
//! A hostile peer cannot cause an allocation larger than the body slice it
//! actually delivered.
//!
//! This is the reason this codec is hand-rolled rather than built on a serde
//! format. Generic serde formats (postcard, bincode) read varint or fixed
//! lengths and call `Vec::with_capacity(declared_length)` *before* reading
//! the body bytes, which lets a few bytes on the wire force a multi-gigabyte
//! allocation. The plan's "decode before allocate" rule rules that out.
//!
//! # No public wire compatibility promise
//!
//! First form may change. Do not persist frames or expose them to untrusted
//! peers.

use std::error::Error as StdError;
use std::fmt;

/// Wire format version. First form is `1`.
pub const FRAME_VERSION_V1: u8 = 1;

/// Bytes used by the length prefix at the start of every frame.
pub const LENGTH_PREFIX_SIZE: usize = 4;

/// Minimum body size: a frame with empty service, empty method, and empty
/// payload.
///
/// Layout: 1 (version) + 1 (kind) + 1 (error_code) + 8 (request_id) +
/// 1 (service_len) + 1 (method_len) = 13 bytes.
pub const MIN_BODY_SIZE: usize = 13;

/// Maximum length, in bytes, of a service name.
pub const MAX_SERVICE_LEN: usize = u8::MAX as usize;

/// Maximum length, in bytes, of a method name.
pub const MAX_METHOD_LEN: usize = u8::MAX as usize;

const KIND_REQUEST: u8 = 0;
const KIND_REPLY: u8 = 1;
const KIND_ERROR: u8 = 2;

const ERROR_CODE_NONE: u8 = 0;
const ERROR_CODE_FULL: u8 = 1;
const ERROR_CODE_UNKNOWN_SERVICE: u8 = 2;
const ERROR_CODE_UNKNOWN_METHOD: u8 = 3;
const ERROR_CODE_DECODE: u8 = 4;
const ERROR_CODE_PROTOCOL: u8 = 5;
const ERROR_CODE_INTERNAL: u8 = 6;

/// Frame kind: request, reply, or error.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FrameKind {
    /// Caller asks the server to invoke a service method.
    Request,
    /// Server reports a successful invocation.
    Reply,
    /// Server reports a server-side condition. See [`FrameError`].
    Error,
}

/// Server-reported error code. Sent inside an [`FrameKind::Error`] frame.
///
/// **Wire-error invariant.** These variants are *server-reported only*.
/// They name conditions the **server** observed about a request that the
/// **client** could not have observed from its side: the service name was
/// unknown to the registry, the method was unknown to the service, the
/// payload failed to decode at the service, the service mailbox was at
/// capacity, the frame violated wire format, or an internal failure. The
/// matching client-observable conditions — local-deadline `Timeout`,
/// `ConnectionClosed` — live on [`crate::ClientResult`] and are produced
/// purely by the client stub. The two enums are intentionally disjoint;
/// **do not** add `Timeout` or `ConnectionClosed` variants here. The
/// [`frame_error_variants_are_server_reported`](crate#guard-tests) guard
/// test fails if the variant set drifts.
///
/// Note: a server-side service-call timeout (the registry's `IsolateCall`
/// to a registered service elapses) maps to [`FrameError::Internal`], not
/// to a fictional `Timeout` variant. The client's local deadline is what
/// surfaces as `ClientResult::Timeout`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FrameError {
    /// The target service is at capacity.
    Full,
    /// The frame named a service the registry does not recognize.
    UnknownService,
    /// The frame named a method the service does not recognize.
    UnknownMethod,
    /// The server could not decode the request payload.
    Decode,
    /// The frame violated the wire format (bad version, wrong field, etc.).
    Protocol,
    /// The server hit an internal failure unrelated to the request.
    Internal,
}

/// Parsed frame in memory.
///
/// Public field access is allowed but bypasses the kind/error consistency
/// invariant enforced by [`Frame::request`], [`Frame::reply`], and
/// [`Frame::error`]. Both [`encode`] and [`decode`] re-validate on the way
/// out and on the way in respectively, so an inconsistent in-memory frame
/// cannot leak onto the wire and a peer cannot deliver one to your handler.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Frame {
    /// Wire format version. Always [`FRAME_VERSION_V1`] today.
    pub version: u8,
    /// Frame kind.
    pub kind: FrameKind,
    /// Server-reported error. `Some` only for [`FrameKind::Error`] frames.
    pub error: Option<FrameError>,
    /// Caller-assigned identifier. Server echoes it back on Reply or Error.
    pub request_id: u64,
    /// Service name. UTF-8, up to [`MAX_SERVICE_LEN`] bytes.
    pub service: String,
    /// Method name. UTF-8, up to [`MAX_METHOD_LEN`] bytes.
    pub method: String,
    /// Opaque payload bytes. The codec does not parse this; encoding (JSON,
    /// postcard, …) is the consumer's choice. Error frames may carry a
    /// diagnostic payload but are not required to.
    pub payload: Vec<u8>,
}

impl Frame {
    /// Builds a [`FrameKind::Request`] frame.
    pub fn request(
        request_id: u64,
        service: impl Into<String>,
        method: impl Into<String>,
        payload: Vec<u8>,
    ) -> Self {
        Self {
            version: FRAME_VERSION_V1,
            kind: FrameKind::Request,
            error: None,
            request_id,
            service: service.into(),
            method: method.into(),
            payload,
        }
    }

    /// Builds a [`FrameKind::Reply`] frame.
    pub fn reply(
        request_id: u64,
        service: impl Into<String>,
        method: impl Into<String>,
        payload: Vec<u8>,
    ) -> Self {
        Self {
            version: FRAME_VERSION_V1,
            kind: FrameKind::Reply,
            error: None,
            request_id,
            service: service.into(),
            method: method.into(),
            payload,
        }
    }

    /// Builds a [`FrameKind::Error`] frame.
    ///
    /// Server side only. Client stubs surface `timeout` and
    /// `connection_closed` locally and must not synthesize wire error frames.
    pub fn error(
        request_id: u64,
        service: impl Into<String>,
        method: impl Into<String>,
        code: FrameError,
        payload: Vec<u8>,
    ) -> Self {
        Self {
            version: FRAME_VERSION_V1,
            kind: FrameKind::Error,
            error: Some(code),
            request_id,
            service: service.into(),
            method: method.into(),
            payload,
        }
    }
}

/// Caller-supplied limits enforced during encode/decode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameLimits {
    /// Maximum body size in bytes (everything after the length prefix). Must
    /// be at least [`MIN_BODY_SIZE`] and at most `u32::MAX as usize`.
    pub max_frame_size: usize,
}

impl FrameLimits {
    /// Constructs limits with the given maximum body size.
    pub const fn new(max_frame_size: usize) -> Self {
        Self { max_frame_size }
    }
}

impl Default for FrameLimits {
    /// 1 MiB body cap. Conservative, easy to revisit.
    fn default() -> Self {
        Self::new(1024 * 1024)
    }
}

/// Errors returned when a frame cannot be encoded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EncodeError {
    /// Service name exceeded [`MAX_SERVICE_LEN`].
    ServiceTooLong {
        /// Actual service-name length.
        len: usize,
    },
    /// Method name exceeded [`MAX_METHOD_LEN`].
    MethodTooLong {
        /// Actual method-name length.
        len: usize,
    },
    /// Encoded body would exceed `FrameLimits::max_frame_size`.
    FrameTooLarge {
        /// Encoded body length.
        len: usize,
        /// Configured maximum.
        max: usize,
    },
    /// `FrameLimits::max_frame_size` was below [`MIN_BODY_SIZE`].
    LimitsTooSmall {
        /// Configured maximum.
        max: usize,
        /// Required minimum.
        min: usize,
    },
    /// `FrameLimits::max_frame_size` exceeded `u32::MAX`.
    LimitsTooLarge {
        /// Configured maximum.
        max: usize,
    },
    /// Frame kind/error_code combination was invalid (e.g. Reply with error).
    InconsistentFrame,
}

impl fmt::Display for EncodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ServiceTooLong { len } => {
                write!(
                    f,
                    "service name length {len} exceeds {MAX_SERVICE_LEN} bytes"
                )
            }
            Self::MethodTooLong { len } => {
                write!(f, "method name length {len} exceeds {MAX_METHOD_LEN} bytes")
            }
            Self::FrameTooLarge { len, max } => {
                write!(f, "encoded frame body {len} exceeds max_frame_size {max}")
            }
            Self::LimitsTooSmall { max, min } => {
                write!(f, "max_frame_size {max} is below minimum body size {min}")
            }
            Self::LimitsTooLarge { max } => {
                write!(f, "max_frame_size {max} exceeds u32::MAX")
            }
            Self::InconsistentFrame => {
                write!(f, "frame kind and error code combination is invalid")
            }
        }
    }
}

impl StdError for EncodeError {}

/// Errors returned when a frame cannot be decoded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DecodeError {
    /// Input was shorter than the length prefix.
    LengthPrefixTruncated,
    /// Length prefix declared a body smaller than [`MIN_BODY_SIZE`].
    BodyTooSmall {
        /// Declared body length.
        len: usize,
        /// Required minimum.
        min: usize,
    },
    /// Length prefix declared a body larger than `FrameLimits::max_frame_size`.
    BodyTooLarge {
        /// Declared body length.
        len: usize,
        /// Configured maximum.
        max: usize,
    },
    /// Frame body was shorter than the length prefix declared.
    BodyTruncated {
        /// Body length declared by the prefix.
        expected: usize,
        /// Bytes actually available after the prefix.
        got: usize,
    },
    /// `FrameLimits::max_frame_size` was below [`MIN_BODY_SIZE`].
    LimitsTooSmall {
        /// Configured maximum.
        max: usize,
        /// Required minimum.
        min: usize,
    },
    /// `FrameLimits::max_frame_size` exceeded `u32::MAX`.
    LimitsTooLarge {
        /// Configured maximum.
        max: usize,
    },
    /// Wire version was not [`FRAME_VERSION_V1`].
    UnsupportedVersion(u8),
    /// Kind byte was outside the allowed range.
    UnknownKind(u8),
    /// Error code byte was outside the allowed range.
    UnknownErrorCode(u8),
    /// Service name length exceeded the remaining body bytes.
    ServiceLengthOverrun,
    /// Method name length exceeded the remaining body bytes.
    MethodLengthOverrun,
    /// Service name was not valid UTF-8.
    ServiceNotUtf8,
    /// Method name was not valid UTF-8.
    MethodNotUtf8,
    /// Reply or Request frame had a non-zero error code.
    UnexpectedErrorCode,
    /// Error frame had error code 0.
    MissingErrorCode,
    /// `decode` was given a buffer longer than the declared frame.
    /// Streaming decoders should drive [`parse_length_prefix`] and
    /// [`decode_body`] directly; the convenience [`decode`] entry
    /// point rejects trailing bytes so a concatenated second frame
    /// or accidental garbage cannot be silently dropped.
    TrailingBytes {
        /// Number of unread bytes after the declared frame.
        extra: usize,
    },
}

impl fmt::Display for DecodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::LengthPrefixTruncated => write!(f, "input too short for length prefix"),
            Self::BodyTooSmall { len, min } => {
                write!(f, "frame body length {len} is below minimum {min}")
            }
            Self::BodyTooLarge { len, max } => {
                write!(f, "frame body length {len} exceeds max_frame_size {max}")
            }
            Self::BodyTruncated { expected, got } => {
                write!(
                    f,
                    "frame body truncated: expected {expected} bytes, got {got}"
                )
            }
            Self::LimitsTooSmall { max, min } => {
                write!(f, "max_frame_size {max} is below minimum body size {min}")
            }
            Self::LimitsTooLarge { max } => {
                write!(f, "max_frame_size {max} exceeds u32::MAX")
            }
            Self::UnsupportedVersion(v) => write!(f, "unsupported wire version {v}"),
            Self::UnknownKind(k) => write!(f, "unknown frame kind byte {k}"),
            Self::UnknownErrorCode(c) => write!(f, "unknown error code byte {c}"),
            Self::ServiceLengthOverrun => write!(f, "service name length overruns body"),
            Self::MethodLengthOverrun => write!(f, "method name length overruns body"),
            Self::ServiceNotUtf8 => write!(f, "service name is not valid UTF-8"),
            Self::MethodNotUtf8 => write!(f, "method name is not valid UTF-8"),
            Self::TrailingBytes { extra } => write!(
                f,
                "decode received {extra} bytes after the declared frame; \
                 use parse_length_prefix + decode_body for streaming input"
            ),
            Self::UnexpectedErrorCode => {
                write!(f, "request or reply frame had a non-zero error code")
            }
            Self::MissingErrorCode => write!(f, "error frame had error code 0"),
        }
    }
}

impl StdError for DecodeError {}

fn check_limits_for_decode(limits: &FrameLimits) -> Result<(), DecodeError> {
    if limits.max_frame_size < MIN_BODY_SIZE {
        return Err(DecodeError::LimitsTooSmall {
            max: limits.max_frame_size,
            min: MIN_BODY_SIZE,
        });
    }
    if limits.max_frame_size > u32::MAX as usize {
        return Err(DecodeError::LimitsTooLarge {
            max: limits.max_frame_size,
        });
    }
    Ok(())
}

fn check_limits_for_encode(limits: &FrameLimits) -> Result<(), EncodeError> {
    if limits.max_frame_size < MIN_BODY_SIZE {
        return Err(EncodeError::LimitsTooSmall {
            max: limits.max_frame_size,
            min: MIN_BODY_SIZE,
        });
    }
    if limits.max_frame_size > u32::MAX as usize {
        return Err(EncodeError::LimitsTooLarge {
            max: limits.max_frame_size,
        });
    }
    Ok(())
}

/// Parses a length prefix and validates it against `limits`.
///
/// This is the entry point for streaming decoders: read four bytes from the
/// transport, call this function, then read exactly `body_len` more bytes
/// before calling [`decode_body`]. Allocating only after this check satisfies
/// the "decode before allocate" rule from the phase plan.
pub fn parse_length_prefix(
    prefix: [u8; LENGTH_PREFIX_SIZE],
    limits: &FrameLimits,
) -> Result<usize, DecodeError> {
    check_limits_for_decode(limits)?;

    let body_len = u32::from_be_bytes(prefix) as usize;

    if body_len < MIN_BODY_SIZE {
        return Err(DecodeError::BodyTooSmall {
            len: body_len,
            min: MIN_BODY_SIZE,
        });
    }
    if body_len > limits.max_frame_size {
        return Err(DecodeError::BodyTooLarge {
            len: body_len,
            max: limits.max_frame_size,
        });
    }

    Ok(body_len)
}

/// Decodes a frame body (the bytes after the length prefix).
///
/// The caller is responsible for having read exactly the number of bytes
/// reported by [`parse_length_prefix`]. The body slice is the upper bound on
/// every internal allocation: service, method, and payload are all carved
/// out of it without copying length information from anywhere else.
pub fn decode_body(body: &[u8]) -> Result<Frame, DecodeError> {
    if body.len() < MIN_BODY_SIZE {
        return Err(DecodeError::BodyTooSmall {
            len: body.len(),
            min: MIN_BODY_SIZE,
        });
    }

    // Fixed-size header: version (1) + kind (1) + error_code (1) + request_id (8) = 11.
    let version = body[0];
    if version != FRAME_VERSION_V1 {
        return Err(DecodeError::UnsupportedVersion(version));
    }

    let kind = match body[1] {
        KIND_REQUEST => FrameKind::Request,
        KIND_REPLY => FrameKind::Reply,
        KIND_ERROR => FrameKind::Error,
        other => return Err(DecodeError::UnknownKind(other)),
    };

    let error = decode_error_code(kind, body[2])?;

    let mut request_id_buf = [0u8; 8];
    request_id_buf.copy_from_slice(&body[3..11]);
    let request_id = u64::from_be_bytes(request_id_buf);

    // Variable-length fields: service.
    let service_len = body[11] as usize;
    let service_start = 12;
    let service_end = service_start + service_len;
    // service_end must leave at least one byte for method_len.
    if service_end >= body.len() {
        return Err(DecodeError::ServiceLengthOverrun);
    }
    let service = std::str::from_utf8(&body[service_start..service_end])
        .map_err(|_| DecodeError::ServiceNotUtf8)?
        .to_owned();

    // Variable-length fields: method.
    let method_len_byte_index = service_end;
    let method_len = body[method_len_byte_index] as usize;
    let method_start = method_len_byte_index + 1;
    let method_end = method_start + method_len;
    if method_end > body.len() {
        return Err(DecodeError::MethodLengthOverrun);
    }
    let method = std::str::from_utf8(&body[method_start..method_end])
        .map_err(|_| DecodeError::MethodNotUtf8)?
        .to_owned();

    // Payload is whatever remains.
    let payload = body[method_end..].to_vec();

    Ok(Frame {
        version,
        kind,
        error,
        request_id,
        service,
        method,
        payload,
    })
}

/// Decodes one complete frame (length prefix plus body) from `bytes`.
///
/// Convenience for callers that already hold exactly one frame in memory.
/// The input must contain exactly the declared frame and no trailing
/// bytes; concatenated frames or trailing garbage return
/// [`DecodeError::TrailingBytes`]. Streaming decoders should drive
/// [`parse_length_prefix`] and [`decode_body`] directly.
pub fn decode(bytes: &[u8], limits: &FrameLimits) -> Result<Frame, DecodeError> {
    if bytes.len() < LENGTH_PREFIX_SIZE {
        return Err(DecodeError::LengthPrefixTruncated);
    }

    let mut prefix = [0u8; LENGTH_PREFIX_SIZE];
    prefix.copy_from_slice(&bytes[..LENGTH_PREFIX_SIZE]);
    let body_len = parse_length_prefix(prefix, limits)?;

    // checked_add guards against pathological max_frame_size values; the
    // limits check rules these out, but make the math explicit.
    let body_end = LENGTH_PREFIX_SIZE
        .checked_add(body_len)
        .ok_or(DecodeError::BodyTooLarge {
            len: body_len,
            max: limits.max_frame_size,
        })?;

    if bytes.len() < body_end {
        return Err(DecodeError::BodyTruncated {
            expected: body_len,
            got: bytes.len() - LENGTH_PREFIX_SIZE,
        });
    }

    if bytes.len() > body_end {
        return Err(DecodeError::TrailingBytes {
            extra: bytes.len() - body_end,
        });
    }

    decode_body(&bytes[LENGTH_PREFIX_SIZE..body_end])
}

/// Encodes a frame to a fresh buffer (length prefix plus body).
///
/// Validates `limits` and the frame's internal consistency. The returned
/// buffer is exactly `LENGTH_PREFIX_SIZE + body_len` bytes long.
pub fn encode(frame: &Frame, limits: &FrameLimits) -> Result<Vec<u8>, EncodeError> {
    check_limits_for_encode(limits)?;

    let service_bytes = frame.service.as_bytes();
    let method_bytes = frame.method.as_bytes();

    if service_bytes.len() > MAX_SERVICE_LEN {
        return Err(EncodeError::ServiceTooLong {
            len: service_bytes.len(),
        });
    }
    if method_bytes.len() > MAX_METHOD_LEN {
        return Err(EncodeError::MethodTooLong {
            len: method_bytes.len(),
        });
    }

    let error_byte = encode_error_code(frame).ok_or(EncodeError::InconsistentFrame)?;

    let body_len = MIN_BODY_SIZE + service_bytes.len() + method_bytes.len() + frame.payload.len();
    if body_len > limits.max_frame_size {
        return Err(EncodeError::FrameTooLarge {
            len: body_len,
            max: limits.max_frame_size,
        });
    }

    let mut out = Vec::with_capacity(LENGTH_PREFIX_SIZE + body_len);
    out.extend_from_slice(&(body_len as u32).to_be_bytes());
    out.push(frame.version);
    out.push(match frame.kind {
        FrameKind::Request => KIND_REQUEST,
        FrameKind::Reply => KIND_REPLY,
        FrameKind::Error => KIND_ERROR,
    });
    out.push(error_byte);
    out.extend_from_slice(&frame.request_id.to_be_bytes());
    out.push(service_bytes.len() as u8);
    out.extend_from_slice(service_bytes);
    out.push(method_bytes.len() as u8);
    out.extend_from_slice(method_bytes);
    out.extend_from_slice(&frame.payload);

    debug_assert_eq!(out.len(), LENGTH_PREFIX_SIZE + body_len);

    Ok(out)
}

fn encode_error_code(frame: &Frame) -> Option<u8> {
    match (frame.kind, frame.error) {
        (FrameKind::Request, None) | (FrameKind::Reply, None) => Some(ERROR_CODE_NONE),
        (FrameKind::Request, Some(_)) | (FrameKind::Reply, Some(_)) => None,
        (FrameKind::Error, Some(code)) => Some(error_to_byte(code)),
        (FrameKind::Error, None) => None,
    }
}

fn decode_error_code(kind: FrameKind, byte: u8) -> Result<Option<FrameError>, DecodeError> {
    match kind {
        FrameKind::Request | FrameKind::Reply => {
            if byte == ERROR_CODE_NONE {
                Ok(None)
            } else {
                Err(DecodeError::UnexpectedErrorCode)
            }
        }
        FrameKind::Error => match byte {
            ERROR_CODE_NONE => Err(DecodeError::MissingErrorCode),
            ERROR_CODE_FULL => Ok(Some(FrameError::Full)),
            ERROR_CODE_UNKNOWN_SERVICE => Ok(Some(FrameError::UnknownService)),
            ERROR_CODE_UNKNOWN_METHOD => Ok(Some(FrameError::UnknownMethod)),
            ERROR_CODE_DECODE => Ok(Some(FrameError::Decode)),
            ERROR_CODE_PROTOCOL => Ok(Some(FrameError::Protocol)),
            ERROR_CODE_INTERNAL => Ok(Some(FrameError::Internal)),
            other => Err(DecodeError::UnknownErrorCode(other)),
        },
    }
}

fn error_to_byte(code: FrameError) -> u8 {
    match code {
        FrameError::Full => ERROR_CODE_FULL,
        FrameError::UnknownService => ERROR_CODE_UNKNOWN_SERVICE,
        FrameError::UnknownMethod => ERROR_CODE_UNKNOWN_METHOD,
        FrameError::Decode => ERROR_CODE_DECODE,
        FrameError::Protocol => ERROR_CODE_PROTOCOL,
        FrameError::Internal => ERROR_CODE_INTERNAL,
    }
}
