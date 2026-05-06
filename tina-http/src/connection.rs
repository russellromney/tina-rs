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

    // Outbound write state. `pending_response` is the bytes still to
    // write; `tcp_write` may accept fewer than we send, in which case
    // `handle_wrote` drains the accepted prefix and we re-issue the
    // remainder.
    pending_response: Vec<u8>,

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
            will_close: false,
            head_deadline_armed: true,
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

    fn handle(&mut self, msg: HttpConnectionMsg, _ctx: &mut Context<'_, S>) -> Effect<Self> {
        match msg {
            HttpConnectionMsg::Begin => self.start(),

            HttpConnectionMsg::Read(Ok(bytes)) => self.handle_bytes_read(bytes),
            HttpConnectionMsg::Read(Err(_)) => self.begin_close(),

            HttpConnectionMsg::HeaderDeadline(_) => self.handle_header_deadline(),

            HttpConnectionMsg::ServiceReturned(outcome) => self.handle_service_outcome(outcome),

            HttpConnectionMsg::Wrote(Ok(count)) => self.handle_wrote(count),
            HttpConnectionMsg::Wrote(Err(_)) => self.begin_close(),

            HttpConnectionMsg::Closed(_) => stop(),
        }
    }
}

impl<S: Shard + 'static> HttpConnection<S> {
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
        let body_end = self.head_len + head.content_length;
        let body = self.read_buf[self.head_len..body_end].to_vec();
        // First form: drop any bytes after content_length. We do not
        // support pipelining, and we close this connection after the
        // response anyway.
        // First form is one request per connection, so every response is
        // terminal for the socket. Force close so HTTP/1.1 clients see an
        // honest `Connection: close` header before the runtime closes the
        // stream.
        self.will_close = true;

        let request = head.into_request(body);
        call(self.service, request, self.service_call_timeout)
            .reply(HttpConnectionMsg::ServiceReturned)
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
        let bytes = encode_response(&response, self.will_close);
        self.pending_response = bytes;
        self.write_pending()
    }

    /// Issues a `tcp_write` for whatever still remains in
    /// `self.pending_response`. The drain happens in `handle_wrote` once
    /// we know how many bytes the runtime accepted; we do not pre-copy
    /// the buffer with an offset, mirroring the partial-write pattern in
    /// `tina-runtime/examples/tcp_echo.rs`.
    fn write_pending(&mut self) -> Effect<Self> {
        // Cloning here is unavoidable in first form: `tcp_write` takes
        // `Vec<u8>` by value because the runtime owns the bytes during
        // the call. The clone is bounded to remaining-bytes via the
        // drain in `handle_wrote`, so the worst case is O(N) total
        // copies for an N-byte response, not O(N^2).
        tcp_write(self.stream, self.pending_response.clone()).reply(HttpConnectionMsg::Wrote)
    }

    fn handle_wrote(&mut self, count: usize) -> Effect<Self> {
        if count == 0 {
            return self.begin_close();
        }
        if count >= self.pending_response.len() {
            self.pending_response.clear();
            self.begin_close()
        } else {
            self.pending_response.drain(..count);
            self.write_pending()
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
