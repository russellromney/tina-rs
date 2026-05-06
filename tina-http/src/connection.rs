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
    CallError, CallOutcome, StreamId, call, tcp_close_stream, tcp_read, tcp_write,
};

use crate::parse::{HttpRequestHead, ParseProgress, encode_response, parse_request_head};
use crate::types::{HttpLimits, HttpRequest, HttpResponse, RequestParseError};

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
    /// Service `call` reply.
    ServiceReturned(CallOutcome<HttpResponse>),
    /// `tcp_write` reply.
    Wrote(Result<usize, CallError>),
    /// `tcp_close_stream` reply.
    Closed(Result<(), CallError>),
}

/// Per-connection isolate.
///
/// Generic over the user's `Shard` type so a single `HttpConnection`
/// implementation works for any shard placement chosen by the
/// surrounding service.
pub struct HttpConnection<S: Shard> {
    stream: StreamId,
    service: Address<HttpRequest, HttpResponse>,
    limits: HttpLimits,
    service_call_timeout: Duration,

    // Accumulating wire state.
    read_buf: Vec<u8>,
    parsed_head: Option<HttpRequestHead>,
    head_len: usize,

    // Outbound write state.
    pending_response: Vec<u8>,
    response_offset: usize,

    // Whether the connection should close after the current response. Set
    // to true on parse error, on service overload that triggers a 503
    // close-policy, on peer-side EOF, on response complete (first form is
    // one request per connection), and on shutdown.
    will_close: bool,

    _shard: std::marker::PhantomData<S>,
}

impl<S: Shard> HttpConnection<S> {
    /// Builds a new connection isolate state for one accepted TCP stream.
    pub fn new(
        stream: StreamId,
        service: Address<HttpRequest, HttpResponse>,
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
            response_offset: 0,
            will_close: false,
            _shard: std::marker::PhantomData,
        }
    }
}

// The `#[tina_runtime::isolate]` macro requires a concrete shard type; we
// write the `Isolate` impl by hand so a single `HttpConnection`
// implementation works for any user-chosen shard.
impl<S: Shard + 'static> Isolate for HttpConnection<S> {
    tina::isolate_types! {
        message: HttpConnectionMsg,
        reply: (),
        send: tina::Outbound<std::convert::Infallible>,
        spawn: std::convert::Infallible,
        call: tina_runtime::RuntimeCall<HttpConnectionMsg>,
        shard: S,
    }

    fn handle(
        &mut self,
        msg: HttpConnectionMsg,
        _ctx: &mut Context<'_, S>,
    ) -> Effect<Self> {
        match msg {
            HttpConnectionMsg::Begin => self.read_more(),

            HttpConnectionMsg::Read(Ok(bytes)) => self.handle_bytes_read(bytes),
            HttpConnectionMsg::Read(Err(_)) => self.begin_close(),

            HttpConnectionMsg::ServiceReturned(outcome) => self.handle_service_outcome(outcome),

            HttpConnectionMsg::Wrote(Ok(count)) => self.handle_wrote(count),
            HttpConnectionMsg::Wrote(Err(_)) => self.begin_close(),

            HttpConnectionMsg::Closed(_) => stop(),
        }
    }
}

impl<S: Shard + 'static> HttpConnection<S> {
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
                }
                ParseProgress::Failed(error) => {
                    return self.send_parse_error(error);
                }
            }
        }

        self.maybe_dispatch_or_read_more()
    }

    fn maybe_dispatch_or_read_more(&mut self) -> Effect<Self> {
        let head = self.parsed_head.as_ref().expect("head parsed before dispatch");
        let needed = self.head_len + head.content_length;
        if self.read_buf.len() < needed {
            self.read_more()
        } else {
            self.dispatch_to_service()
        }
    }

    fn dispatch_to_service(&mut self) -> Effect<Self> {
        let head = self.parsed_head.take().expect("head parsed before dispatch");
        let body_end = self.head_len + head.content_length;
        let body = self.read_buf[self.head_len..body_end].to_vec();
        // First form: drop any bytes after content_length. We do not
        // support pipelining, and we close this connection after the
        // response anyway.
        self.will_close = self.will_close || head.connection_close;

        let request = head.into_request(body);
        call(self.service, request, self.service_call_timeout)
            .reply(HttpConnectionMsg::ServiceReturned)
    }

    fn handle_service_outcome(
        &mut self,
        outcome: CallOutcome<HttpResponse>,
    ) -> Effect<Self> {
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
        let bytes = encode_response(&response, self.will_close);
        self.pending_response = bytes;
        self.response_offset = 0;
        self.write_next_chunk()
    }

    fn write_next_chunk(&mut self) -> Effect<Self> {
        let chunk = self.pending_response[self.response_offset..].to_vec();
        tcp_write(self.stream, chunk).reply(HttpConnectionMsg::Wrote)
    }

    fn handle_wrote(&mut self, count: usize) -> Effect<Self> {
        self.response_offset = self.response_offset.saturating_add(count);
        if self.response_offset < self.pending_response.len() {
            self.write_next_chunk()
        } else {
            self.begin_close()
        }
    }

    fn begin_close(&mut self) -> Effect<Self> {
        tcp_close_stream(self.stream).reply(HttpConnectionMsg::Closed)
    }
}

/// Maps a runtime `CallError` from the service call into a synthetic HTTP
/// response. Used when the service mailbox was full, the address was
/// closed, or the call timed out before the service replied.
fn response_for_call_error(error: &CallError) -> HttpResponse {
    match error {
        CallError::Timeout => HttpResponse::with_status(StatusCode::GATEWAY_TIMEOUT),
        // Closed/Cancelled/Aborted addresses are an internal error from
        // the client's perspective: the service is supposed to be there
        // and isn't.
        _ => HttpResponse::with_status(StatusCode::INTERNAL_SERVER_ERROR),
    }
}

/// Service-overload handler used by callers that want to project a
/// non-`Replied` outcome into a typed HTTP response. The connection
/// isolate uses this internally; it is exposed here so service-side code
/// can build the same mapping (e.g. when wrapping a downstream call).
pub fn response_for_call_outcome(outcome: &CallOutcome<HttpResponse>) -> Option<HttpResponse> {
    match outcome {
        CallOutcome::Replied(_) => None,
        CallOutcome::Full => Some(HttpResponse::with_status(StatusCode::SERVICE_UNAVAILABLE)),
        CallOutcome::Closed => {
            Some(HttpResponse::with_status(StatusCode::INTERNAL_SERVER_ERROR))
        }
        CallOutcome::Timeout => Some(HttpResponse::with_status(StatusCode::GATEWAY_TIMEOUT)),
    }
}
