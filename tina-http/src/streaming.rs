//! Streaming body types for HTTP requests and responses.
//!
//! A body is either fully buffered (`Vec<u8>`) or streamed through a
//! chunk-source isolate. The source is a service-shaped isolate the
//! consumer pulls from with `call(source, ChunkMsg::Next, t).reply(...)`
//! until `Eof`.
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
//! Pull-based: the consumer issues one `Next` at a time, only after the
//! previous chunk has been fully written (response side) or fully
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

/// Pulled by a service from an inbound body chunk source.
#[derive(Debug, Clone)]
pub enum RequestChunkMsg {
    /// Request the next chunk of request body bytes.
    Next,
}

/// Reply to [`RequestChunkMsg::Next`].
#[derive(Debug, Clone)]
pub enum RequestChunkReply {
    /// One chunk of body bytes.
    Chunk(Vec<u8>),
    /// End of stream.
    Eof,
}

/// A streaming request body: declared length plus a source isolate.
///
/// The source is the connection isolate itself — its address typed with
/// `HttpConnectionMsg` as the message type and `RequestChunkReply` as
/// the reply. The service pulls chunks via:
///
/// ```rust,ignore
/// call(stream.source, HttpConnectionMsg::RequestBodyNext, timeout)
///     .reply(MyMsg::ChunkArrived)
/// ```
///
/// Wrapping the chunk request in `HttpConnectionMsg` lets the
/// connection isolate serve both its TCP continuations and the
/// service's chunk pulls from a single mailbox. A purpose-built chunk
/// source isolate would be cleaner but requires a runtime affordance
/// to publish its address back to the connection at spawn time.
#[derive(Debug, Clone)]
pub struct RequestStream {
    /// Declared `Content-Length` from the wire.
    pub content_length: usize,
    /// Chunk source — the connection isolate.
    pub source: Address<crate::HttpConnectionMsg, RequestChunkReply>,
}
