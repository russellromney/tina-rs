//! Per-connection isolate for the native HTTP/1.1 server.
//!
//! One [`HttpConnection`] isolate owns one TCP stream. It reads bytes,
//! parses one request head, accumulates the body up to `Content-Length`,
//! calls the service isolate via `tina_runtime::call`, serialises the
//! response, writes it, and closes the stream.
//!
//! First form is one request per connection: there is no keep-alive
//! reuse loop. Pipelined extra bytes after the body are dropped on close.
//! Keep-alive lands in a follow-up slice if user pressure justifies it.
//!
//! Backpressure mapping at the service boundary:
//!
//! | Service `CallOutcome`            | Wire response                |
//! |----------------------------------|------------------------------|
//! | `Replied(HttpResponse)`          | The response itself          |
//! | `Full`                           | `503 Service Unavailable`    |
//! | `Closed`                         | `500 Internal Server Error`  |
//! | `Timeout`                        | `504 Gateway Timeout`        |
//!
//! Parser failures map per [`crate::types::RequestParseError::status`].

use std::time::Duration;

use http::StatusCode;
use tina::prelude::*;
use tina_runtime::{
    CallError, CallOutcome, StreamId, call, sleep, tcp_close_stream, tcp_read, tcp_write,
};

use crate::parse::{HttpRequestHead, ParseProgress, encode_response_head, parse_request_head};
use crate::streaming::{
    RequestChunkReply, RequestStream, ResponseChunkMsg, ResponseChunkReply, ResponseStream,
};
use crate::types::{HttpLimits, HttpRequest, HttpResponse, HttpResponseBody, RequestParseError};

/// Bytes the connection isolate asks for per `tcp_read`. Bounded so a
/// single read does not pull more than this into the runtime, regardless
/// of what the kernel has buffered.
const READ_CHUNK: usize = 4096;

/// Inbound message variants for [`HttpConnection`].
///
/// External code typically only sends [`HttpConnectionMsg::Begin`] once;
/// every other variant is a runtime-call continuation produced by the
/// connection itself.
#[derive(Debug, Clone)]
pub enum HttpConnectionMsg {
    /// Kick off the read loop. Sent once by the listener after spawn.
    Begin,
    /// `tcp_read` reply.
    Read(Result<Vec<u8>, CallError>),
    /// Slow-loris guard: fires once after
    /// [`HttpLimits::header_read_timeout`] from connection start. If
    /// the connection still has not finished reading the request head,
    /// the connection stops and lets runtime cleanup close the stream.
    HeaderDeadline(Result<(), CallError>),
    /// Service `call` reply.
    ServiceReturned(CallOutcome<HttpResponse>),
    /// `tcp_write` reply.
    Wrote(Result<usize, CallError>),
    /// `tcp_close_stream` reply.
    Closed(Result<(), CallError>),
    /// Streaming response: chunk source's reply to a pulled `Next`.
    StreamChunk(CallOutcome<ResponseChunkReply>),
    /// Streaming request: service asks the connection for the next
    /// chunk of the inbound body. Replies with [`RequestChunkReply`].
    RequestBodyNext,
    /// Streaming request: continuation from a `tcp_read` issued while
    /// serving a `RequestBodyNext` call whose buffer was empty. The
    /// outer call context (the service's `RequestBodyNext` call)
    /// propagates through this continuation, so `Effect::Reply` here
    /// answers the service.
    BodyChunkRead(Result<Vec<u8>, CallError>),
}

impl HttpConnectionMsg {
    /// Convenience: build a `RequestBodyNext` for use at a service
    /// call site without spelling out the variant.
    pub fn body_next() -> Self {
        Self::RequestBodyNext
    }
}

/// Per-connection isolate.
///
/// Generic over the user's `Shard` type and the service's message
/// type `M`. `M` defaults to `HttpRequest` for sync-reply services;
/// multi-turn services declare an enum that wraps `HttpRequest` and
/// supply `From<HttpRequest>`.
pub struct HttpConnection<S: Shard, M: From<HttpRequest> + Send + 'static = HttpRequest> {
    stream: StreamId,
    service: Address<M, HttpResponse>,
    limits: HttpLimits,
    service_call_timeout: Duration,

    // Accumulating wire state.
    read_buf: Vec<u8>,
    parsed_head: Option<HttpRequestHead>,
    head_len: usize,

    // Outbound write state. `pending_response` is the bytes still to
    // write; `tcp_write` may accept fewer than we send, in which case
    // `handle_wrote` drains the accepted prefix and we re-issue the
    // remainder.
    pending_response: Vec<u8>,

    // Streaming-response state. `Some` once we have written the head of
    // a streamed response and need to keep pulling chunks until `Eof`.
    // `bytes_remaining` is decremented as chunks are written; when it
    // hits zero (or we receive `Eof`), we close.
    stream_source: Option<Address<ResponseChunkMsg, ResponseChunkReply>>,
    stream_bytes_remaining: usize,
    stream_call_timeout: Duration,

    // Inbound streaming state.
    //
    // When the dispatch path chose the streaming variant, the
    // connection lazily pulls body bytes from the socket as the service
    // calls `RequestBodyNext`. Naming convention:
    //
    // - `inbound_total`: declared `Content-Length`.
    // - `inbound_received`: bytes read from the socket so far in the
    //   body region.
    // - `inbound_delivered`: bytes already replied to the service.
    // - `inbound_buffer`: bytes received from the socket but not yet
    //   delivered to the service (received - delivered).
    // - `inbound_chunk_size`: cap on a single chunk reply.
    //
    // Invariant: `inbound_received >= inbound_delivered` and
    // `inbound_received - inbound_delivered == inbound_buffer.len()`.
    inbound_total: usize,
    inbound_received: usize,
    inbound_delivered: usize,
    inbound_buffer: Vec<u8>,
    inbound_chunk_size: usize,

    // Captured at the first handler turn (`start()`), used to construct
    // the typed self-address for streaming-body dispatch.
    self_shard_id: Option<tina::ShardId>,
    self_isolate_id: Option<tina::IsolateId>,

    // Whether the connection should close after the current response. Set
    // to true on parse error, on service overload that triggers a 503
    // close-policy, on peer-side EOF, on response complete (first form is
    // one request per connection), and on shutdown.
    will_close: bool,

    // Slow-loris guard. While `head_deadline_armed` is true, an
    // outstanding `sleep` runtime call is racing the head-read; if it
    // fires before parsing completes, the connection stops and lets runtime
    // cleanup close the stream. After parsing completes the flag flips,
    // and the deadline message becomes a no-op when it arrives.
    head_deadline_armed: bool,

    _shard: std::marker::PhantomData<S>,
}

impl<S: Shard, M: From<HttpRequest> + Send + 'static> HttpConnection<S, M> {
    /// Builds a new connection isolate state for one accepted TCP stream.
    pub fn new(
        stream: StreamId,
        service: Address<M, HttpResponse>,
        limits: HttpLimits,
        service_call_timeout: Duration,
    ) -> Self {
        Self {
            stream,
            service,
            limits,
            service_call_timeout,
            read_buf: Vec::new(),
            parsed_head: None,
            head_len: 0,
            pending_response: Vec::new(),
            stream_source: None,
            stream_bytes_remaining: 0,
            stream_call_timeout: service_call_timeout,
            inbound_total: 0,
            inbound_received: 0,
            inbound_delivered: 0,
            inbound_buffer: Vec::new(),
            inbound_chunk_size: 0,
            self_shard_id: None,
            self_isolate_id: None,
            will_close: false,
            head_deadline_armed: true,
            _shard: std::marker::PhantomData,
        }
    }
}

// The `#[tina_runtime::isolate]` macro requires a concrete shard type; we
// write the `Isolate` impl by hand so a single `HttpConnection`
// implementation works for any user-chosen shard.
impl<S: Shard + 'static, M: From<HttpRequest> + Send + 'static> Isolate for HttpConnection<S, M> {
    tina::isolate_types! {
        message: HttpConnectionMsg,
        reply: RequestChunkReply,
        send: tina::Outbound<std::convert::Infallible>,
        spawn: std::convert::Infallible,
        call: tina_runtime::RuntimeCall<HttpConnectionMsg>,
        shard: S,
    }

    fn handle(
        &mut self,
        msg: HttpConnectionMsg,
        ctx: &mut Context<'_, S, Self::Reply>,
    ) -> Effect<Self> {
        // Capture self-identity once. Used by the streaming-request
        // dispatch path to hand the service a typed self-address.
        if self.self_isolate_id.is_none() {
            self.self_shard_id = Some(ctx.shard_id());
            self.self_isolate_id = Some(ctx.isolate_id());
        }
        match msg {
            HttpConnectionMsg::Begin => self.start(),

            HttpConnectionMsg::Read(Ok(bytes)) => self.handle_bytes_read(bytes),
            HttpConnectionMsg::Read(Err(_)) => self.begin_close(),

            HttpConnectionMsg::HeaderDeadline(_) => self.handle_header_deadline(),

            HttpConnectionMsg::ServiceReturned(outcome) => self.handle_service_outcome(outcome),

            HttpConnectionMsg::Wrote(Ok(count)) => self.handle_wrote(count),
            HttpConnectionMsg::Wrote(Err(_)) => self.begin_close(),

            HttpConnectionMsg::StreamChunk(outcome) => self.handle_stream_chunk(outcome),

            HttpConnectionMsg::RequestBodyNext => self.handle_request_body_next(),

            HttpConnectionMsg::BodyChunkRead(result) => self.handle_body_chunk_read(result),

            HttpConnectionMsg::Closed(_) => stop(),
        }
    }
}

impl<S: Shard + 'static, M: From<HttpRequest> + Send + 'static> HttpConnection<S, M> {
    /// First-effect hook. Issues both the initial `tcp_read` and the
    /// slow-loris deadline `sleep` in one batch so they race the
    /// client's bytes against the configured timeout.
    fn start(&mut self) -> Effect<Self> {
        let deadline_effect: Effect<Self> =
            sleep(self.limits.header_read_timeout).reply(HttpConnectionMsg::HeaderDeadline);
        let read_effect: Effect<Self> = self.read_more();
        batch(vec![read_effect, deadline_effect])
    }

    fn read_more(&mut self) -> Effect<Self> {
        tcp_read(self.stream, READ_CHUNK).reply(HttpConnectionMsg::Read)
    }

    fn handle_bytes_read(&mut self, bytes: Vec<u8>) -> Effect<Self> {
        if bytes.is_empty() {
            // Peer closed cleanly. If we already parsed a head and have
            // a partial body, this is a truncated request — close
            // without dispatching. If we haven't parsed yet, also close.
            return self.begin_close();
        }

        self.read_buf.extend_from_slice(&bytes);

        if self.parsed_head.is_none() {
            match parse_request_head(&self.read_buf, &self.limits) {
                ParseProgress::NeedMore => return self.read_more(),
                ParseProgress::Complete { head, head_len } => {
                    self.parsed_head = Some(head);
                    self.head_len = head_len;
                    // Disarm the slow-loris guard. The deadline message
                    // may still arrive but `handle_header_deadline`
                    // checks this flag and no-ops.
                    self.head_deadline_armed = false;
                }
                ParseProgress::Failed(error) => {
                    self.head_deadline_armed = false;
                    return self.send_parse_error(error);
                }
            }
        }

        self.maybe_dispatch_or_read_more()
    }

    /// Slow-loris guard: this fires after
    /// [`HttpLimits::header_read_timeout`]. If the connection has not
    /// yet parsed a request head, it stops the isolate and lets runtime
    /// cleanup close the stream. If parsing already completed, the deadline
    /// fires harmlessly.
    fn handle_header_deadline(&mut self) -> Effect<Self> {
        if !self.head_deadline_armed {
            return noop();
        }
        self.head_deadline_armed = false;
        // Slow-loris guard: the read lane has an outstanding
        // `tcp_read`, so issuing `tcp_close_stream` here would fail
        // with `CallError::ResourceBusy` (read and write lanes can run
        // concurrently, but explicit close cannot run while a lane is
        // pending). We could still try to write a 408 — the write
        // lane is free — but the close-after-write would then also
        // fail and the client would see no FIN. The cleanest path on
        // the current runtime is to stop the isolate; the runtime
        // cancels pending calls and drops the stream, which the
        // kernel observes as a clean connection close.
        //
        // Trade-off: no 408 reaches the slow client. RFC 7235 §5.5
        // recommends 408 but does not require it; a clean close is an
        // acceptable response to a slow-loris client. A future runtime
        // affordance — `tcp_cancel_read` or "close cancels pending
        // lanes" — would let us write the 408 first; tracked as a
        // 047/runtime ergonomics note.
        stop()
    }

    fn maybe_dispatch_or_read_more(&mut self) -> Effect<Self> {
        let head = self
            .parsed_head
            .as_ref()
            .expect("head parsed before dispatch");
        let streaming = self.limits.inbound_stream_chunk_size.is_some() && head.content_length > 0;
        if streaming {
            // Streaming: dispatch as soon as the head is parsed. Body
            // bytes are pulled lazily from the socket on demand via
            // `RequestBodyNext` calls from the service.
            return self.dispatch_to_service();
        }
        let needed = self.head_len + head.content_length;
        if self.read_buf.len() < needed {
            self.read_more()
        } else {
            self.dispatch_to_service()
        }
    }

    fn dispatch_to_service(&mut self) -> Effect<Self> {
        let head = self
            .parsed_head
            .take()
            .expect("head parsed before dispatch");
        self.will_close = true;

        // Decide buffered vs streaming dispatch based on the limits.
        let request = match self.limits.inbound_stream_chunk_size {
            Some(chunk_size) if head.content_length > 0 => {
                // Streaming: take whatever body bytes already arrived
                // (the read-ahead from the head-parse rounds), park
                // them in `inbound_buffer`, and hand the service a
                // typed self-address. Subsequent body bytes are
                // pulled from the socket lazily as the service calls
                // `RequestBodyNext`.
                // Reuse `read_buf`'s allocation as the inbound buffer:
                // truncate to body_end, drain off the head, and keep
                // the remaining body bytes. `read_buf` is replaced
                // with an empty Vec so the per-connection memory
                // story is just `inbound_buffer`.
                let body_end = self.head_len + head.content_length;
                let mut buf = std::mem::take(&mut self.read_buf);
                let prebuf_end = buf.len().min(body_end);
                buf.truncate(prebuf_end);
                buf.drain(..self.head_len.min(buf.len()));
                self.inbound_total = head.content_length;
                self.inbound_received = buf.len();
                self.inbound_delivered = 0;
                self.inbound_buffer = buf;
                self.inbound_chunk_size = chunk_size.max(1);
                let me_chunk: Address<HttpConnectionMsg, RequestChunkReply> =
                    tina::Address::new_with_generation(
                        self.shard_id_for_self(),
                        self.isolate_id_for_self(),
                        tina::AddressGeneration::new(0),
                    );
                let stream = RequestStream {
                    content_length: self.inbound_total,
                    source: me_chunk,
                };
                head.into_streaming_request(stream)
            }
            _ => {
                // Buffered: by the time we get here `read_buf` already
                // holds the full body — `maybe_dispatch_or_read_more`
                // returns to `read_more` until the buffer is full.
                //
                // Reuse `read_buf`'s allocation as the body and drop
                // anything else, so the per-connection memory budget
                // is just the body we hand to the service, not
                // `read_buf + body`.
                let body_end = self.head_len + head.content_length;
                let mut buf = std::mem::take(&mut self.read_buf);
                buf.truncate(body_end);
                buf.drain(..self.head_len);
                head.into_request(buf)
            }
        };
        call(self.service, M::from(request), self.service_call_timeout)
            .reply(HttpConnectionMsg::ServiceReturned)
    }

    /// Returns the shard id for self. The dispatch path needs this to
    /// build a typed self-address; we cannot use `ctx.me()` here
    /// because handler entrypoints take `&mut self` and `ctx` is at a
    /// higher scope. The values are recorded by the runtime when the
    /// isolate is registered and unchanging across handler turns —
    /// stash them in `start()` instead.
    fn shard_id_for_self(&self) -> tina::ShardId {
        self.self_shard_id.expect("shard id captured at start()")
    }

    fn isolate_id_for_self(&self) -> tina::IsolateId {
        self.self_isolate_id
            .expect("isolate id captured at start()")
    }

    /// Serves the next inbound body chunk to the calling service.
    ///
    /// - If we already have buffered bytes, drain a chunk from
    ///   `inbound_buffer`, advance `inbound_delivered`, reply.
    /// - If the buffer is empty but we have not received the full body
    ///   from the socket, issue a `tcp_read` and let the
    ///   `BodyChunkRead` continuation answer the service. The outer
    ///   call context (this `RequestBodyNext` call) propagates through
    ///   the `.reply(...)` chain, so a later `Effect::Reply` reaches
    ///   this caller.
    /// - If the buffer is empty and the full body has been delivered,
    ///   reply `Eof`.
    fn handle_request_body_next(&mut self) -> Effect<Self> {
        if self.inbound_delivered >= self.inbound_total {
            return reply(RequestChunkReply::Eof);
        }
        if !self.inbound_buffer.is_empty() {
            return self.serve_chunk_from_buffer();
        }
        // Need to pull more bytes from the socket. Cap the request at
        // what we still need so the kernel does not over-read.
        let want = self
            .inbound_total
            .saturating_sub(self.inbound_received)
            .min(READ_CHUNK);
        if want == 0 {
            // Defensive: nothing left to read but delivered != total.
            // Treat as truncation — reply Eof so the service notices
            // via received bytes < expected.
            return reply(RequestChunkReply::Eof);
        }
        tcp_read(self.stream, want).reply(HttpConnectionMsg::BodyChunkRead)
    }

    fn handle_body_chunk_read(&mut self, result: Result<Vec<u8>, CallError>) -> Effect<Self> {
        let bytes = match result {
            Ok(bytes) => bytes,
            // tcp_read failure mid-body: end the stream. Service sees
            // delivered bytes < expected and can decide what to do.
            Err(_) => return reply(RequestChunkReply::Eof),
        };
        if bytes.is_empty() {
            // Peer closed mid-body. Honest move: terminate the stream
            // with what we have; the service will notice the short
            // delivery via `delivered < expected`.
            return reply(RequestChunkReply::Eof);
        }
        self.inbound_received += bytes.len();
        self.inbound_buffer.extend_from_slice(&bytes);
        self.serve_chunk_from_buffer()
    }

    fn serve_chunk_from_buffer(&mut self) -> Effect<Self> {
        let remaining_total = self.inbound_total - self.inbound_delivered;
        let take = self
            .inbound_chunk_size
            .min(self.inbound_buffer.len())
            .min(remaining_total);
        let chunk: Vec<u8> = self.inbound_buffer.drain(..take).collect();
        self.inbound_delivered += take;
        reply(RequestChunkReply::Chunk(chunk))
    }

    fn handle_service_outcome(&mut self, outcome: CallOutcome<HttpResponse>) -> Effect<Self> {
        let response = match outcome.into_result() {
            Ok(response) => response,
            Err(call_error) => {
                self.will_close = true;
                response_for_call_error(&call_error)
            }
        };
        self.start_writing(response)
    }

    fn send_parse_error(&mut self, error: RequestParseError) -> Effect<Self> {
        self.will_close = true;
        let response = HttpResponse::with_status(error.status());
        self.start_writing(response)
    }

    fn start_writing(&mut self, response: HttpResponse) -> Effect<Self> {
        // Encode the head with the declared body length. The body bytes
        // (or chunks for a streamed body) are written separately so
        // streaming can pace per-chunk.
        let head_bytes = encode_response_head(&response, self.will_close);
        match response.body {
            HttpResponseBody::Buffered(body_bytes) => {
                // Append the buffered body to the head and write it all
                // through the existing partial-write loop.
                let mut bytes = head_bytes;
                bytes.extend_from_slice(&body_bytes);
                self.pending_response = bytes;
                self.write_pending()
            }
            HttpResponseBody::Stream(ResponseStream {
                content_length,
                source,
            }) => {
                // Write the head; once the head has fully drained,
                // `handle_wrote` will pull the first chunk from the
                // source and write it.
                self.pending_response = head_bytes;
                self.stream_source = Some(source);
                self.stream_bytes_remaining = content_length;
                self.write_pending()
            }
        }
    }

    /// Issues a `tcp_write` for whatever still remains in
    /// `self.pending_response`. The drain happens in `handle_wrote` once
    /// we know how many bytes the runtime accepted; we do not pre-copy
    /// the buffer with an offset.
    fn write_pending(&mut self) -> Effect<Self> {
        tcp_write(self.stream, self.pending_response.clone()).reply(HttpConnectionMsg::Wrote)
    }

    fn handle_wrote(&mut self, count: usize) -> Effect<Self> {
        if count == 0 {
            return self.begin_close();
        }
        if count >= self.pending_response.len() {
            self.pending_response.clear();
            // Buffer drained. If we are streaming, pull the next chunk;
            // otherwise close.
            if self.stream_source.is_some() && self.stream_bytes_remaining > 0 {
                self.pull_next_chunk()
            } else {
                self.stream_source = None;
                self.begin_close()
            }
        } else {
            self.pending_response.drain(..count);
            self.write_pending()
        }
    }

    /// Issues a `call(source, Next, t).reply(StreamChunk)` to pull the
    /// next chunk of a streamed response.
    fn pull_next_chunk(&mut self) -> Effect<Self> {
        let source = self.stream_source.expect("stream source set");
        call(source, ResponseChunkMsg::Next, self.stream_call_timeout)
            .reply(HttpConnectionMsg::StreamChunk)
    }

    fn handle_stream_chunk(&mut self, outcome: CallOutcome<ResponseChunkReply>) -> Effect<Self> {
        match outcome {
            CallOutcome::Replied(ResponseChunkReply::Chunk(bytes)) => {
                if bytes.is_empty() {
                    // Treat empty chunk like Eof — defensive against
                    // sources that signal end-of-stream this way.
                    self.stream_source = None;
                    return self.begin_close();
                }
                if bytes.len() > self.stream_bytes_remaining {
                    // Source over-produced relative to declared length.
                    // Truncate to keep the wire framing honest.
                    let mut truncated = bytes;
                    truncated.truncate(self.stream_bytes_remaining);
                    self.stream_bytes_remaining = 0;
                    self.pending_response = truncated;
                } else {
                    self.stream_bytes_remaining -= bytes.len();
                    self.pending_response = bytes;
                }
                self.write_pending()
            }
            CallOutcome::Replied(ResponseChunkReply::Eof) => {
                // Source finished. Close — note the wire `Content-Length`
                // we already emitted is canonical; the source under-
                // producing is a contract violation but we close cleanly.
                self.stream_source = None;
                self.begin_close()
            }
            CallOutcome::Full | CallOutcome::Closed | CallOutcome::Timeout => {
                // Source died mid-stream. Close the wire — the client
                // sees a truncated body relative to `Content-Length`.
                // First-form policy: close, do not try to inject an
                // error response on top of an already-emitted head.
                self.stream_source = None;
                self.begin_close()
            }
        }
    }

    fn begin_close(&mut self) -> Effect<Self> {
        tcp_close_stream(self.stream).reply(HttpConnectionMsg::Closed)
    }
}

/// Maps a runtime `CallError` from the service call into a synthetic HTTP
/// response.
///
/// Every variant of [`CallError`] is matched explicitly: adding a new
/// variant in `tina-runtime` causes a compile error here, forcing an
/// intentional decision rather than a silent default to `500`.
///
/// | `CallError`           | Status                       |
/// |-----------------------|------------------------------|
/// | `TargetFull`          | `503 Service Unavailable`    |
/// | `Timeout`             | `504 Gateway Timeout`        |
/// | `TargetClosed`        | `500 Internal Server Error`  |
/// | `InvalidResource`     | `500 Internal Server Error`  |
/// | `Io`                  | `500 Internal Server Error`  |
/// | `Unsupported`         | `500 Internal Server Error`  |
/// | `ResourceBusy`        | `500 Internal Server Error`  |
/// | `NotFound`            | `500 Internal Server Error`  |
/// | persistence variants  | `500 Internal Server Error`  |
/// | DNS/TLS/process/signal variants | `500 Internal Server Error` |
fn response_for_call_error(error: &CallError) -> HttpResponse {
    let status = match error {
        // Backpressure: service mailbox was full. Standard HTTP shape
        // for "try again later" is 503.
        CallError::TargetFull => StatusCode::SERVICE_UNAVAILABLE,
        // The service did not reply before our call timeout elapsed.
        CallError::Timeout => StatusCode::GATEWAY_TIMEOUT,
        // Service address became unavailable (panicked, stopped,
        // stale). From the client's perspective this is a server-side
        // fault.
        CallError::TargetClosed => StatusCode::INTERNAL_SERVER_ERROR,
        // The remaining variants describe runtime-level faults that do
        // not have a clean HTTP-shaped equivalent. We collapse them all
        // to 500 so the wire response is still well-formed; the trace
        // carries the precise reason. Listed exhaustively so a future
        // CallError variant in tina-runtime forces a compile error here
        // rather than silently routing through a default.
        CallError::InvalidResource
        | CallError::NotFound
        | CallError::Io
        | CallError::Unsupported
        | CallError::ResourceBusy
        | CallError::CorruptRecord
        | CallError::CommitUncertain
        | CallError::StorageFull
        | CallError::StorageClosed
        | CallError::DnsFull
        | CallError::DnsClosed
        | CallError::TlsFull
        | CallError::TlsClosed
        | CallError::TlsCertificate
        | CallError::TlsName
        | CallError::TlsHandshake
        | CallError::SignalFull
        | CallError::SignalClosed
        | CallError::ProcessFull
        | CallError::ProcessClosed
        | CallError::KillUncertain => StatusCode::INTERNAL_SERVER_ERROR,
    };
    HttpResponse::with_status(status)
}

/// Projects a [`CallOutcome`] into an HTTP response when it is *not* a
/// successful reply.
///
/// Returns `None` when the outcome carries a real reply; the caller is
/// expected to use that reply directly. Returns `Some(response)` for
/// `Full`, `Closed`, and `Timeout`, with the same status mapping used by
/// the connection isolate's runtime-call error path.
///
/// Exposed publicly so service-side code can build the same mapping
/// when wrapping a downstream call into its own response shape.
pub fn response_for_call_outcome(outcome: &CallOutcome<HttpResponse>) -> Option<HttpResponse> {
    match outcome {
        CallOutcome::Replied(_) => None,
        CallOutcome::Full => Some(HttpResponse::with_status(StatusCode::SERVICE_UNAVAILABLE)),
        CallOutcome::Closed => Some(HttpResponse::with_status(StatusCode::INTERNAL_SERVER_ERROR)),
        CallOutcome::Timeout => Some(HttpResponse::with_status(StatusCode::GATEWAY_TIMEOUT)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn full_call_error_maps_to_503() {
        assert_eq!(
            response_for_call_error(&CallError::TargetFull).status,
            StatusCode::SERVICE_UNAVAILABLE,
        );
    }

    #[test]
    fn closed_call_error_maps_to_500() {
        assert_eq!(
            response_for_call_error(&CallError::TargetClosed).status,
            StatusCode::INTERNAL_SERVER_ERROR,
        );
    }

    #[test]
    fn timeout_call_error_maps_to_504() {
        assert_eq!(
            response_for_call_error(&CallError::Timeout).status,
            StatusCode::GATEWAY_TIMEOUT,
        );
    }

    #[test]
    fn full_outcome_projects_to_503() {
        let response = response_for_call_outcome(&CallOutcome::<HttpResponse>::Full)
            .expect("Full projects to a response");
        assert_eq!(response.status, StatusCode::SERVICE_UNAVAILABLE);
    }

    #[test]
    fn closed_outcome_projects_to_500() {
        let response = response_for_call_outcome(&CallOutcome::<HttpResponse>::Closed)
            .expect("Closed projects to a response");
        assert_eq!(response.status, StatusCode::INTERNAL_SERVER_ERROR);
    }

    #[test]
    fn timeout_outcome_projects_to_504() {
        let response = response_for_call_outcome(&CallOutcome::<HttpResponse>::Timeout)
            .expect("Timeout projects to a response");
        assert_eq!(response.status, StatusCode::GATEWAY_TIMEOUT);
    }

    #[test]
    fn replied_outcome_projects_to_none() {
        let response = response_for_call_outcome(&CallOutcome::Replied(HttpResponse::ok()));
        assert!(
            response.is_none(),
            "successful replies do not project to a synthetic response"
        );
    }
}
