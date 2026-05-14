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
//!   For the common iterator-style source, [`IterBodySource`] turns
//!   any `Iterator<Item = Vec<u8>>` into a chunk source without a
//!   custom `Isolate` impl.
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
//! Two response framings are supported:
//!
//! - [`ResponseStream`] — declared `Content-Length`. The connection
//!   emits the length in the head and writes raw chunk bytes.
//! - [`ChunkedResponseStream`] — `Transfer-Encoding: chunked`. The
//!   connection emits the chunked header and frames each `Chunk`
//!   reply as `size CRLF data CRLF`, with a `0 CRLF CRLF` terminator
//!   when the source replies `Eof`.
//!
//! Use [`crate::HttpResponse::stream_known_length`] when the total is
//! known up front. Use [`crate::HttpResponse::stream_chunked`] when
//! it is not. There is no "guess a length" path.
//!
//! Request bodies may be `Content-Length` or chunked. Chunked request
//! bodies require [`crate::HttpLimits::inbound_stream_chunk_size`] so
//! the service pulls decoded chunks through the same
//! [`RequestChunkReply`] path. Chunk trailers are rejected.
//!
//! # Backpressure
//!
//! Pull-based: the consumer issues one `Next` at a time, only after
//! the previous chunk has been fully written (response side) or fully
//! processed (request side). The chunk source can take any amount of
//! time to produce the next chunk; the consumer naturally waits.

use std::convert::Infallible;
use std::marker::PhantomData;

use tina::prelude::*;
use tina::{Address, CallContext};

/// Pulled by the consumer from a chunk source.
#[derive(Debug, Clone)]
pub enum ResponseChunkMsg {
    /// Request the next chunk of response body bytes.
    Next,
    /// The connection is abandoning the wire. The source should stop
    /// producing and release any owned resources — files, downstream
    /// calls, pending slots. Duplicate cancels are harmless.
    Cancel,
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

/// A streaming response body framed by `Content-Length`. The source
/// is expected to deliver exactly `content_length` bytes total
/// across `Chunk` replies before `Eof`. Under-produce surfaces as
/// `body_io_error_count`; over-produce is truncated to the declared
/// length.
#[derive(Debug, Clone)]
pub struct ResponseStream {
    /// Total bytes the source promises to deliver across all `Chunk`
    /// replies before `Eof`. Emitted as `Content-Length` on the wire.
    pub content_length: usize,
    /// Chunk source. The connection isolate pulls from this address.
    pub source: Address<ResponseChunkMsg, ResponseChunkReply>,
}

/// A streaming response body framed by `Transfer-Encoding: chunked`.
/// No length is declared up front; the source produces chunks until
/// it replies `Eof`, and the connection writes the `0\r\n\r\n`
/// terminator. The chunk source contract is identical to
/// [`ResponseStream`] — same `Next` / `Chunk` / `Eof` exchange — only
/// the wire framing differs.
///
/// Use this when the body length is not known up front. If you do
/// know the length, prefer [`ResponseStream`] so the client can size
/// its read buffer.
#[derive(Debug, Clone)]
pub struct ChunkedResponseStream {
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
    /// Declared `Content-Length` from the wire. Meaningless when
    /// `chunked` is `true`.
    pub content_length: usize,
    /// `true` when the request used `Transfer-Encoding: chunked`.
    /// There is no declared length; the service pulls chunks until
    /// `Eof`.
    pub chunked: bool,
    /// Chunk source — the connection isolate.
    pub source: Address<crate::HttpConnectionMsg, RequestChunkReply>,
}

/// HTTP/2 streaming request body source.
///
/// The HTTP/2 connection isolate owns the socket and flow-control windows.
/// Services pull chunks through this handle; the connection returns stream and
/// connection window credit only after a chunk is delivered or discarded.
#[derive(Debug, Clone)]
pub struct Http2RequestStream {
    /// HTTP/2 stream id.
    pub stream_id: u32,
    /// Declared `Content-Length` if one arrived. HTTP/2 does not require it.
    pub content_length: Option<usize>,
    /// Chunk source — the HTTP/2 connection isolate.
    pub source: Address<crate::Http2ConnectionMsg, crate::Http2ConnectionReply>,
}

/// Iterator-backed chunk source for the response side. Wraps any
/// `Iterator<Item = Vec<u8>> + Send + 'static` into an [`Isolate`]
/// that answers [`ResponseChunkMsg::Next`] by yielding the next
/// item, or [`ResponseChunkReply::Eof`] when the iterator drains.
///
/// # Contract
///
/// - `Some(non_empty_bytes)` → wire chunk of those bytes.
/// - `None` → end of stream (`Eof`).
/// - `Some(empty_vec)` → also end of stream. The chunked-encoding
///   wire format reserves `0\r\n\r\n` as the terminator, so an
///   empty mid-stream chunk has no honest representation; we treat
///   it as `Eof` rather than emit anything ambiguous.
///
/// # Single-use
///
/// The iterator drains once and is gone. After it returns `None`,
/// every subsequent `Next` call replies `Eof`. To serve the same
/// content again, register a fresh [`IterBodySource`] for the
/// next request — the source isolate is per-stream, not per-route.
///
/// # Boxing
///
/// The iterator is boxed so the resulting type is
/// `IterBodySource<S>` — one concrete type per shard — which keeps
/// `register_with_capacity` tractable. The iterator runs once per
/// `Next` reply, so each chunk is bounded by however much the
/// iterator produces in one step.
///
/// # Example
///
/// ```rust,ignore
/// use tina_http::{HttpResponse, IterBodySource};
///
/// let chunk_size = 4 * 1024;
/// let total_chunks = 64;
/// let chunks = (0..total_chunks).map(move |i| vec![(i % 251) as u8; chunk_size]);
/// let source = runtime.register_with_capacity::<IterBodySource<MyShard>, _>(
///     IterBodySource::new(chunks),
///     16,
/// )?;
/// let response = HttpResponse::stream_known_length(
///     StatusCode::OK,
///     chunk_size * total_chunks,
///     source,
/// );
/// ```
pub struct IterBodySource<S: Shard + 'static> {
    iter: Box<dyn Iterator<Item = Vec<u8>> + Send + 'static>,
    _shard: PhantomData<S>,
}

impl<S: Shard + 'static> IterBodySource<S> {
    /// Wraps an iterator into a chunk source. The iterator is boxed
    /// so callers can pass any closure-based iterator without naming
    /// the iterator type.
    pub fn new<I>(iter: I) -> Self
    where
        I: Iterator<Item = Vec<u8>> + Send + 'static,
    {
        Self {
            iter: Box::new(iter),
            _shard: PhantomData,
        }
    }

    /// Registers an iterator-backed chunk source on `runtime` and
    /// returns its address. Equivalent to:
    ///
    /// ```rust,ignore
    /// runtime.register_with_capacity::<IterBodySource<S>, Infallible>(
    ///     IterBodySource::new(iter),
    ///     mailbox_capacity,
    /// )
    /// ```
    ///
    /// but without the turbofish or the `Infallible` placeholder.
    /// Use this when you don't already have a custom registration
    /// closure.
    pub fn register<F, I>(
        runtime: &tina_runtime::ThreadedRuntime<S, F>,
        iter: I,
        mailbox_capacity: usize,
    ) -> Result<Address<ResponseChunkMsg, ResponseChunkReply>, tina_runtime::ThreadedRuntimeError>
    where
        S: Send + Sync,
        F: tina_runtime::MailboxFactory + Send + 'static,
        I: Iterator<Item = Vec<u8>> + Send + 'static,
    {
        runtime.register_with_capacity::<IterBodySource<S>, Infallible>(
            IterBodySource::new(iter),
            mailbox_capacity,
        )
    }
}

impl<S: Shard + 'static> Isolate for IterBodySource<S> {
    tina::isolate_types! {
        message: ResponseChunkMsg,
        reply: ResponseChunkReply,
        send: tina::Outbound<Infallible>,
        spawn: Infallible,
        // RuntimeCall (not Infallible) so the isolate can register
        // on both the threaded runtime and the deterministic
        // simulator. We never make outbound calls; the channel is
        // there only to satisfy the simulator's bound.
        call: tina_runtime::RuntimeCall<ResponseChunkMsg>,
        shard: S,
    }

    fn handle(
        &mut self,
        msg: ResponseChunkMsg,
        _ctx: &mut Context<'_, S, Self::Reply>,
    ) -> Effect<Self> {
        match msg {
            ResponseChunkMsg::Cancel => stop(),
            _ => reply(self.next_reply(msg)),
        }
    }

    fn handle_call(&mut self, msg: ResponseChunkMsg, call: CallContext<'_, Self>) -> Effect<Self> {
        match msg {
            ResponseChunkMsg::Cancel => stop(),
            _ => call.reply(self.next_reply(msg)),
        }
    }
}

impl<S: Shard + 'static> IterBodySource<S> {
    fn next_reply(&mut self, msg: ResponseChunkMsg) -> ResponseChunkReply {
        match msg {
            ResponseChunkMsg::Next => match self.iter.next() {
                Some(bytes) if !bytes.is_empty() => ResponseChunkReply::Chunk(bytes),
                // Empty `Vec<u8>` is treated as `Eof` so the iterator
                // can signal end-of-stream without an explicit option.
                _ => ResponseChunkReply::Eof,
            },
            ResponseChunkMsg::Cancel => ResponseChunkReply::Eof,
        }
    }
}
