//! Streaming body types for HTTP requests and responses.
//!
//! A body is either fully buffered (`Vec<u8>`) or streamed through a
//! chunk-source isolate.
//!
//! # Source-call shape
//!
//! - **Response streaming** (service produces). The service registers
//!   a chunk-source isolate whose `Message = ResponseChunkMsg` and
//!   `Reply = ResponseChunkReply`. The connection pulls with
//!   `call(stream.source, ResponseChunkMsg::Next, t).reply(...)`.
//!
//! - **Request streaming** (service consumes). The connection isolate
//!   itself is the chunk source; its `Message = HttpConnectionMsg`,
//!   `Reply = RequestChunkReply`. The service pulls with
//!   `call(stream.source, HttpConnectionMsg::body_next(), t).reply(...)`.
//!   This asymmetry exists because the connection has to fold the
//!   chunk-request variant into its existing message type to keep the
//!   socket and chunk pulls on a single mailbox.
//!
//! # Wire framing
//!
//! Streaming uses `Content-Length` framing — `content_length` must be
//! known up front. The connection isolate emits the declared length in
//! the head and writes chunks as they arrive. Unknown-length streaming
//! would need chunked transfer encoding, which is an explicit non-goal
//! at this layer.
//!
//! # Backpressure
//!
//! Pull-based: the consumer issues one `Next` at a time, only after
//! the previous chunk has been fully written (response side) or fully
//! processed (request side). The chunk source can take any amount of
//! time to produce the next chunk; the consumer naturally waits.

use tina::Address;

/// Pulled by the consumer from a chunk source. Single-variant enum is
/// future-proof for sugar like `NextWithHint(usize)` later.
#[derive(Debug, Clone)]
pub enum ResponseChunkMsg {
    /// Request the next chunk of response body bytes.
    Next,
}

/// Reply to [`ResponseChunkMsg::Next`].
#[derive(Debug, Clone)]
pub enum ResponseChunkReply {
    /// One chunk of body bytes. The consumer expects the source to
    /// have produced at most `content_length` bytes total across all
    /// `Chunk` replies before returning `Eof`.
    Chunk(Vec<u8>),
    /// End of stream. The connection isolate stops pulling and closes
    /// the response.
    Eof,
}

/// A streaming response body: declared length plus a source isolate.
#[derive(Debug, Clone)]
pub struct ResponseStream {
    /// Total bytes the source promises to deliver across all `Chunk`
    /// replies before `Eof`. Emitted as `Content-Length` on the wire.
    pub content_length: usize,
    /// Chunk source. The connection isolate pulls from this address.
    pub source: Address<ResponseChunkMsg, ResponseChunkReply>,
}

/// Reply produced by an inbound chunk source. The service receives
/// this on the call chain rooted at
/// `crate::HttpConnectionMsg::RequestBodyNext` (or its `body_next()`
/// constructor).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RequestChunkReply {
    /// One chunk of body bytes.
    Chunk(Vec<u8>),
    /// End of stream — the declared `Content-Length` was reached.
    Eof,
    /// Read failed mid-body. Distinct from `Eof` so the service
    /// can tell clean short delivery from truncation.
    Error(tina_runtime::CallError),
}

/// A streaming request body: declared length plus a source isolate.
///
/// The source is the connection isolate itself — its address typed
/// with `crate::HttpConnectionMsg` as the message type and
/// [`RequestChunkReply`] as the reply. The service pulls chunks via:
///
/// ```rust,ignore
/// use tina_http::HttpConnectionMsg;
/// call(stream.source, HttpConnectionMsg::body_next(), timeout)
///     .reply(MyMsg::ChunkArrived)
/// ```
///
/// `HttpConnectionMsg::body_next()` is a convenience constructor for
/// the `HttpConnectionMsg::RequestBodyNext` variant. Wrapping the
/// chunk request inside the connection's own message type lets the
/// connection serve both its TCP continuations and the service's
/// chunk pulls from a single mailbox.
#[derive(Debug, Clone)]
pub struct RequestStream {
    /// Declared `Content-Length` from the wire.
    pub content_length: usize,
    /// Chunk source — the connection isolate.
    pub source: Address<crate::HttpConnectionMsg, RequestChunkReply>,
}
