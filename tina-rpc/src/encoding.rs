//! Pluggable payload encoding adapter.
//!
//! The frame envelope (see [`crate::frame`]) carries an opaque payload.
//! Application types (request/response structs, primitives, etc.) are
//! converted to and from those bytes by an [`Encoding`] implementation. First
//! form ships [`Json`]; postcard, bincode, and others may follow as separate
//! adapters in later phases.
//!
//! # `max_size` semantics
//!
//! - **Encode**: the produced byte count is capped *during* serialization. As
//!   soon as the cumulative output would exceed `max_size`, the encoder
//!   returns [`EncodingError::PayloadTooLarge`] without continuing. No
//!   allocation larger than `max_size + small_chunk` happens.
//! - **Decode**: the input slice length is checked against `max_size` *before*
//!   the underlying deserializer runs. **However**, deserializing JSON (or
//!   any text/structured format) into typed Rust values can amplify the wire
//!   byte count into heap memory: a 1 KiB JSON input that is `[null, null,
//!   ...]` allocates one `Option<…>` per element, easily ~10× the wire
//!   length. `max_size` therefore bounds the *wire* bytes, not deserialized
//!   memory. Callers should size `max_size` with knowledge of the encoder's
//!   amplification factor (≤ ~10× for [`Json`] in practice).
//!
//! Bounded allocation is preserved either way: an attacker cannot force
//! *unbounded* memory growth — only `~factor × max_size` per request — and
//! Rock 2 will combine this with a bounded in-flight count.
//!
//! # Compile-time encoding selection
//!
//! [`Encoding`] is intentionally not object-safe (generic methods). Rock 4
//! (client stub) parameterizes on `E: Encoding` at the type level rather than
//! holding `Box<dyn Encoding>`. Runtime encoding selection is out of scope
//! for first form. If it becomes necessary later, an enum dispatch wrapper
//! (`enum AnyEncoding { Json, Postcard }` with non-generic methods) is the
//! boring add-on.

use std::error::Error as StdError;
use std::fmt;
use std::io::{self, Write};

use serde::Serialize;
use serde::de::DeserializeOwned;

/// A payload codec.
///
/// `T: DeserializeOwned` is deliberate. Borrowed deserialization
/// (`Deserialize<'de>`) is out of scope for first form: the connection
/// isolate hands payload bytes off as `Vec<u8>` and does not retain a
/// borrowing buffer.
pub trait Encoding {
    /// Encoder identifier for tracing and diagnostics. E.g. `"json"`. Stable
    /// across releases.
    fn name(&self) -> &'static str;

    /// Encodes `value` to bytes, capping the output at `max_size`.
    fn encode<T>(&self, value: &T, max_size: usize) -> Result<Vec<u8>, EncodingError>
    where
        T: Serialize;

    /// Decodes `bytes` to `T`, rejecting if `bytes.len() > max_size` before
    /// the underlying deserializer runs. See module docs for amplification
    /// notes.
    fn decode<T>(&self, bytes: &[u8], max_size: usize) -> Result<T, EncodingError>
    where
        T: DeserializeOwned;
}

/// Stable category for an underlying encoder error.
///
/// Concrete encoders (e.g. [`Json`]) classify their native error type into
/// these categories so callers can match without depending on upstream error
/// text.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum EncodingErrorKind {
    /// Input was not well-formed in the encoder's grammar.
    Syntax,
    /// Input was well-formed but did not match the requested Rust type.
    DataMismatch,
    /// Input ended before a complete value could be parsed.
    UnexpectedEof,
    /// Anything else (custom Serialize/Deserialize errors, I/O failures,
    /// …). Reserve for genuinely unclassifiable upstream errors.
    Other,
}

/// Errors returned by an [`Encoding`] implementation.
///
/// `EncodingError` is `#[non_exhaustive]` because future encoders may surface
/// categories the current set does not cover (e.g. compression failure).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum EncodingError {
    /// The underlying encoder failed to serialize the value.
    Encode {
        /// Encoder name, from [`Encoding::name`].
        encoder: &'static str,
        /// Stable error category.
        kind: EncodingErrorKind,
    },
    /// The underlying encoder failed to deserialize the bytes.
    Decode {
        /// Encoder name.
        encoder: &'static str,
        /// Stable error category.
        kind: EncodingErrorKind,
    },
    /// Payload exceeded the configured maximum size.
    PayloadTooLarge {
        /// Actual payload length in bytes.
        len: usize,
        /// Configured maximum.
        max: usize,
    },
}

impl fmt::Display for EncodingError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Encode { encoder, kind } => write!(f, "{encoder} encode failed: {kind:?}"),
            Self::Decode { encoder, kind } => write!(f, "{encoder} decode failed: {kind:?}"),
            Self::PayloadTooLarge { len, max } => {
                write!(f, "payload {len} bytes exceeds max {max}")
            }
        }
    }
}

impl StdError for EncodingError {}

/// JSON encoding via [`serde_json`].
///
/// Boring and debuggable. The default first-form choice.
#[derive(Debug, Clone, Copy, Default)]
pub struct Json;

impl Encoding for Json {
    fn name(&self) -> &'static str {
        "json"
    }

    fn encode<T>(&self, value: &T, max_size: usize) -> Result<Vec<u8>, EncodingError>
    where
        T: Serialize,
    {
        let mut buf = Vec::new();
        let mut capped = CappedWriter::new(&mut buf, max_size);
        match serde_json::to_writer(&mut capped, value) {
            Ok(()) => Ok(buf),
            Err(err) => {
                if capped.overflowed {
                    Err(EncodingError::PayloadTooLarge {
                        // Cumulative bytes attempted = bytes written + the
                        // chunk that pushed past the cap. We report `max + 1`
                        // as a conservative lower bound; the exact attempted
                        // count is not always knowable through serde_json's
                        // io::Write boundary.
                        len: max_size.saturating_add(1),
                        max: max_size,
                    })
                } else {
                    Err(EncodingError::Encode {
                        encoder: self.name(),
                        kind: classify_serde_json(&err),
                    })
                }
            }
        }
    }

    fn decode<T>(&self, bytes: &[u8], max_size: usize) -> Result<T, EncodingError>
    where
        T: DeserializeOwned,
    {
        if bytes.len() > max_size {
            return Err(EncodingError::PayloadTooLarge {
                len: bytes.len(),
                max: max_size,
            });
        }
        serde_json::from_slice(bytes).map_err(|err| EncodingError::Decode {
            encoder: self.name(),
            kind: classify_serde_json(&err),
        })
    }
}

fn classify_serde_json(err: &serde_json::Error) -> EncodingErrorKind {
    use serde_json::error::Category;
    match err.classify() {
        Category::Eof => EncodingErrorKind::UnexpectedEof,
        Category::Syntax => EncodingErrorKind::Syntax,
        Category::Data => EncodingErrorKind::DataMismatch,
        Category::Io => EncodingErrorKind::Other,
    }
}

/// `io::Write` adapter that errors once cumulative bytes exceed `max`.
///
/// Used by [`Json::encode`] so serialization stops as soon as the output
/// would exceed the cap, rather than producing a giant `Vec` and rejecting
/// it after the fact.
struct CappedWriter<'a> {
    buf: &'a mut Vec<u8>,
    max: usize,
    overflowed: bool,
}

impl<'a> CappedWriter<'a> {
    fn new(buf: &'a mut Vec<u8>, max: usize) -> Self {
        Self {
            buf,
            max,
            overflowed: false,
        }
    }
}

impl Write for CappedWriter<'_> {
    fn write(&mut self, chunk: &[u8]) -> io::Result<usize> {
        let projected = self.buf.len().saturating_add(chunk.len());
        if projected > self.max {
            self.overflowed = true;
            return Err(io::Error::new(
                io::ErrorKind::OutOfMemory,
                "exceeds encoding max_size",
            ));
        }
        self.buf.extend_from_slice(chunk);
        Ok(chunk.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
