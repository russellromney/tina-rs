//! Native HTTP/1.1 client connection isolate.
//!
//! [`HttpClient`] is a long-lived service-shaped isolate. Users invoke
//! it the same way as any other Tina service:
//!
//! ```rust,ignore
//! call(http_client, HttpClientMsg::call(target, request), timeout)
//!     .then(MyMsg::HttpReturned)
//! ```
//!
//! One request at a time. A new `Call` while busy replies
//! [`HttpClientError::Busy`]. For parallelism, register multiple
//! clients or front the client with [`crate::HttpConnectionPool`].

use std::marker::PhantomData;
use std::net::SocketAddr;

use http::HeaderValue;
use http::header::HOST;
use tina::prelude::*;
use tina::{CallContext, RequestContext, reply_to_request};
use tina_runtime::{
    CallError, StreamId, TcpReadBufReply, TcpWriteOwnedReply, TlsReadBufReply, TlsStreamId,
    TlsWriteOwnedReply, sleep, tcp_close_stream, tcp_connect, tcp_read_buf, tcp_write_owned,
    tls_close, tls_connect, tls_read_buf, tls_write_owned,
};

use crate::parse::{
    HttpResponseHead, ResponseParseProgress, encode_request, is_valid_origin_form_request_target,
    parse_response_head,
};
use crate::target::{HttpHostPolicy, HttpTarget};
use crate::transport::HttpTransport;
use crate::types::{
    HttpClientConfig, HttpClientError, HttpRequest, HttpResponse, HttpTransportPhase,
    ResponseParseError,
};

/// Bytes the client asks for per `tcp_read`. Matches the server side.
const READ_CHUNK: usize = 4096;

/// One outbound HTTP/1.1 call. The encoder adds `Content-Length` and
/// `Connection: close`. For HTTPS, the `Host:` header comes from the
/// target's [`HttpHostPolicy`]; supplying one yourself returns
/// [`HttpClientError::DuplicateHostHeader`].
#[derive(Debug, Clone)]
pub struct OutboundCall {
    pub target: HttpTarget,
    pub request: HttpRequest,
}

/// `Call` is user-callable; the rest are continuations from the
/// client's own runtime calls. `Call` is boxed so the enum stays
/// small in the mailbox — `OutboundCall` carries an `HttpRequest`
/// with a `HeaderMap` and body, which dwarfs every other variant.
#[derive(Debug, Clone)]
pub enum HttpClientMsg {
    Call(Box<OutboundCall>),
    Connected {
        generation: u64,
        result: Result<(StreamId, SocketAddr, SocketAddr), CallError>,
    },
    TlsConnected {
        generation: u64,
        result: Result<TlsStreamId, CallError>,
    },
    Wrote {
        generation: u64,
        result: Result<TcpWriteOwnedReply, CallError>,
    },
    Read {
        generation: u64,
        result: Result<TcpReadBufReply, CallError>,
    },
    Closed(Result<(), CallError>),
    Deadline {
        generation: u64,
        result: Result<(), CallError>,
    },
}

impl HttpClientMsg {
    /// A bare `SocketAddr` is interpreted as plain HTTP.
    pub fn call(target: impl Into<HttpTarget>, request: HttpRequest) -> Self {
        Self::Call(Box::new(OutboundCall {
            target: target.into(),
            request,
        }))
    }
}

/// Service-shaped HTTP/1.1 client.
///
/// Generic over `S: Shard`. Reply type is
/// `Result<HttpResponse, HttpClientError>`.
pub struct HttpClient<S: Shard + 'static> {
    config: HttpClientConfig,
    state: Option<ActiveCall>,
    request_generation: u64,
    _shard: PhantomData<S>,
}

/// State for one in-flight call. Cleared at terminal.
struct ActiveCall {
    generation: u64,
    request_bytes: Vec<u8>,
    transport: Option<HttpTransport>,
    pending_write: Vec<u8>,
    read_scratch: Vec<u8>,
    read_buf: Vec<u8>,
    parsed_head: Option<HttpResponseHead>,
    head_len: usize,
    chunked_decoder: Option<crate::chunked_decoder::ChunkedDecoder>,
    body_buf: Vec<u8>,
    reply_to: Option<RequestContext<Result<HttpResponse, HttpClientError>>>,
}

enum Decision {
    Deliver,
    Fail,
    NeedMore(usize),
}

impl<S: Shard + 'static> HttpClient<S> {
    /// Constructs a fresh client with the given config.
    pub fn new(config: HttpClientConfig) -> Self {
        Self {
            config,
            state: None,
            request_generation: 0,
            _shard: PhantomData,
        }
    }

    /// Constructs a client with the development preset.
    pub fn dev() -> Self {
        Self::new(HttpClientConfig::dev())
    }
}

// Hand-rolled `Isolate` impl: macro requires a concrete shard; we want
// the implementation generic so any user shard placement works.
impl<S: Shard + 'static> Isolate for HttpClient<S> {
    tina::isolate_types! {
        message: HttpClientMsg,
        reply: Result<HttpResponse, HttpClientError>,
        send: tina::Outbound<std::convert::Infallible>,
        spawn: std::convert::Infallible,
        call: tina_runtime::RuntimeCall<HttpClientMsg>,
        shard: S,
    }

    fn handle(
        &mut self,
        msg: HttpClientMsg,
        _ctx: &mut Context<'_, S, Self::Reply>,
    ) -> Effect<Self> {
        match msg {
            HttpClientMsg::Call(call) => self.handle_outbound_call(*call, None),

            HttpClientMsg::Connected {
                generation,
                result: Ok((stream, _local, _peer)),
            } => {
                if !self.is_current_generation(generation) {
                    return tcp_close_stream(stream).then(HttpClientMsg::Closed);
                }
                let Some(state) = self.state.as_mut() else {
                    // Previous call already ended; close the dangling stream.
                    return tcp_close_stream(stream).then(HttpClientMsg::Closed);
                };
                state.transport = Some(HttpTransport::Tcp(stream));
                state.pending_write = std::mem::take(&mut state.request_bytes);
                self.write_more()
            }
            HttpClientMsg::Connected {
                generation,
                result: Err(_),
            } => {
                if self.is_current_generation(generation) {
                    self.fail(HttpClientError::Connect)
                } else {
                    noop()
                }
            }

            HttpClientMsg::TlsConnected {
                generation,
                result: Ok(stream),
            } => {
                if !self.is_current_generation(generation) {
                    return tls_close(stream, self.config.request_timeout)
                        .then(HttpClientMsg::Closed);
                }
                let Some(state) = self.state.as_mut() else {
                    return tls_close(stream, self.config.request_timeout)
                        .then(HttpClientMsg::Closed);
                };
                state.transport = Some(HttpTransport::Tls(stream));
                state.pending_write = std::mem::take(&mut state.request_bytes);
                self.write_more()
            }
            HttpClientMsg::TlsConnected {
                generation,
                result: Err(source),
            } => {
                if self.is_current_generation(generation) {
                    self.fail(HttpClientError::Transport {
                        phase: HttpTransportPhase::Connect,
                        source,
                    })
                } else {
                    noop()
                }
            }

            HttpClientMsg::Wrote {
                generation,
                result: Ok(reply),
            } => {
                if self.is_current_generation(generation) {
                    self.handle_wrote(reply)
                } else {
                    noop()
                }
            }
            HttpClientMsg::Wrote {
                generation,
                result: Err(source),
            } => {
                if self.is_current_generation(generation) {
                    self.fail(self.transport_or_flat_error(
                        HttpTransportPhase::Write,
                        source,
                        HttpClientError::Write,
                    ))
                } else {
                    noop()
                }
            }

            HttpClientMsg::Read {
                generation,
                result: Ok(reply),
            } => {
                if self.is_current_generation(generation) {
                    self.handle_read_reply(reply)
                } else {
                    noop()
                }
            }
            HttpClientMsg::Read {
                generation,
                result: Err(source),
            } => {
                if self.is_current_generation(generation) {
                    self.fail(self.transport_or_flat_error(
                        HttpTransportPhase::Read,
                        source,
                        HttpClientError::Read,
                    ))
                } else {
                    noop()
                }
            }

            HttpClientMsg::Deadline { generation, .. } => {
                if self.is_current_generation(generation) {
                    self.fail(HttpClientError::Timeout)
                } else {
                    noop()
                }
            }

            // Both runtime lanes free the stream resource at
            // submit-close time, so a failed close doesn't leak.
            // Outcome is recorded in the trace.
            HttpClientMsg::Closed(_) => noop(),
        }
    }

    fn handle_call(&mut self, msg: HttpClientMsg, call: CallContext<'_, Self>) -> Effect<Self> {
        match msg {
            HttpClientMsg::Call(outbound) => {
                let reply_to = call.into_request_context();
                self.handle_outbound_call(*outbound, Some(reply_to))
            }
            _ => call.reject(tina::CallRejectedReason::UnsupportedMessage),
        }
    }
}

impl<S: Shard + 'static> HttpClient<S> {
    fn handle_outbound_call(
        &mut self,
        call: OutboundCall,
        reply_to: Option<RequestContext<Result<HttpResponse, HttpClientError>>>,
    ) -> Effect<Self> {
        if self.state.is_some() {
            return match reply_to {
                Some(request) => reply_to_request(request, Err(HttpClientError::Busy)),
                None => reply(Err(HttpClientError::Busy)),
            };
        }
        let OutboundCall { target, request } = call;
        let request = match apply_host_policy(request, &target) {
            Ok(request) => request,
            Err(error) => {
                return match reply_to {
                    Some(request) => reply_to_request(request, Err(error)),
                    None => reply(Err(error)),
                };
            }
        };
        let request_bytes = encode_request(&request);
        self.request_generation = self
            .request_generation
            .checked_add(1)
            .expect("HttpClient request_generation overflowed u64");
        let generation = self.request_generation;
        self.state = Some(ActiveCall {
            generation,
            request_bytes,
            transport: None,
            pending_write: Vec::new(),
            read_scratch: Vec::with_capacity(READ_CHUNK),
            read_buf: Vec::new(),
            parsed_head: None,
            head_len: 0,
            chunked_decoder: None,
            body_buf: Vec::new(),
            reply_to,
        });
        let connect_effect: Effect<Self> = match target {
            HttpTarget::Http { addr, .. } => tcp_connect(addr)
                .then(move |result| HttpClientMsg::Connected { generation, result }),
            HttpTarget::Https {
                addr,
                server_name,
                trust_roots,
                host: _,
            } => tls_connect(
                addr,
                server_name,
                trust_roots.root_certificates_der,
                self.config.request_timeout,
            )
            .then(move |result| HttpClientMsg::TlsConnected { generation, result }),
        };
        let deadline_effect: Effect<Self> = sleep(self.config.request_timeout)
            .then(move |result| HttpClientMsg::Deadline { generation, result });
        batch(vec![connect_effect, deadline_effect])
    }

    /// TLS transport produces a typed `Transport` error; TCP keeps
    /// the flat variant for source compat.
    fn transport_or_flat_error(
        &self,
        phase: HttpTransportPhase,
        source: CallError,
        flat: HttpClientError,
    ) -> HttpClientError {
        match self.state.as_ref().and_then(|state| state.transport) {
            Some(HttpTransport::Tls(_)) => HttpClientError::Transport { phase, source },
            _ => flat,
        }
    }

    fn is_current_generation(&self, generation: u64) -> bool {
        self.state
            .as_ref()
            .is_some_and(|state| state.generation == generation)
    }

    fn write_more(&mut self) -> Effect<Self> {
        let state = self.state.as_mut().expect("state present during write");
        let transport = state.transport.expect("transport set before write");
        let bytes = std::mem::take(&mut state.pending_write);
        match transport {
            HttpTransport::Tcp(stream) => {
                let generation = state.generation;
                tcp_write_owned(stream, bytes).then(move |result| HttpClientMsg::Wrote {
                    generation,
                    result: result.map_err(|error| error.error),
                })
            }
            HttpTransport::Tls(stream) => {
                let generation = state.generation;
                tls_write_owned(stream, bytes, self.config.request_timeout).then(move |result| {
                    HttpClientMsg::Wrote {
                        generation,
                        result: result
                            .map(tls_write_reply_to_tcp)
                            .map_err(|error| error.error),
                    }
                })
            }
        }
    }

    fn handle_wrote(&mut self, reply: TcpWriteOwnedReply) -> Effect<Self> {
        let TcpWriteOwnedReply {
            mut bytes,
            written: count,
        } = reply;
        let Some(state) = self.state.as_mut() else {
            return noop();
        };
        if count == 0 {
            state.pending_write = bytes;
            return self.fail(HttpClientError::Write);
        }
        if count >= bytes.len() {
            self.read_more()
        } else {
            bytes.drain(..count);
            state.pending_write = bytes;
            self.write_more()
        }
    }

    fn read_more(&mut self) -> Effect<Self> {
        let state = self.state.as_mut().expect("state present during read");
        let transport = state.transport.expect("transport set before read");
        let buffer = std::mem::take(&mut state.read_scratch);
        match transport {
            HttpTransport::Tcp(stream) => {
                let generation = state.generation;
                tcp_read_buf(stream, buffer, READ_CHUNK).then(move |result| HttpClientMsg::Read {
                    generation,
                    result: result.map_err(|error| error.error),
                })
            }
            HttpTransport::Tls(stream) => {
                let generation = state.generation;
                tls_read_buf(stream, buffer, READ_CHUNK, self.config.request_timeout).then(
                    move |result| HttpClientMsg::Read {
                        generation,
                        result: result
                            .map(tls_read_reply_to_tcp)
                            .map_err(|error| error.error),
                    },
                )
            }
        }
    }

    fn handle_read_reply(&mut self, reply: TcpReadBufReply) -> Effect<Self> {
        let TcpReadBufReply { buffer, len } = reply;
        self.handle_bytes_read(buffer, len)
    }

    fn handle_bytes_read(&mut self, mut buffer: Vec<u8>, len: usize) -> Effect<Self> {
        let Some(state) = self.state.as_mut() else {
            return noop();
        };
        if len == 0 {
            buffer.clear();
            state.read_scratch = buffer;
            // Peer closed: succeed if body is complete, else fail.
            return if state.parsed_head.is_some() && body_complete(state) {
                self.deliver_success()
            } else {
                self.fail(HttpClientError::Closed)
            };
        }
        state.read_buf.extend_from_slice(&buffer[..len]);
        buffer.clear();
        state.read_scratch = buffer;

        if state.parsed_head.is_none() {
            match parse_response_head(&state.read_buf, &self.config.limits) {
                ResponseParseProgress::NeedMore => return self.read_more(),
                ResponseParseProgress::Complete { head, head_len } => {
                    state.parsed_head = Some(head);
                    state.head_len = head_len;
                }
                ResponseParseProgress::Failed(error) => {
                    return self.fail(HttpClientError::Parse(error));
                }
            }
        }

        if body_complete(state) {
            return self.deliver_success();
        }

        let head = state.parsed_head.as_ref().expect("head parsed");
        if head.chunked {
            let decoder = state.chunked_decoder.get_or_insert_with(|| {
                crate::chunked_decoder::ChunkedDecoder::new(self.config.limits.max_body_bytes)
            });
            let body_start = state.head_len;
            let decision = {
                let raw = &state.read_buf[body_start..];
                let (result, consumed) = decoder.feed_all(raw, &mut state.body_buf);
                match result {
                    crate::chunked_decoder::FeedAllResult::Complete => Decision::Deliver,
                    crate::chunked_decoder::FeedAllResult::Failed(_) => Decision::Fail,
                    crate::chunked_decoder::FeedAllResult::NeedMore => Decision::NeedMore(consumed),
                }
            };
            match decision {
                Decision::Deliver => self.deliver_success(),
                Decision::Fail => self.fail(HttpClientError::Parse(
                    ResponseParseError::MalformedChunkedBody,
                )),
                Decision::NeedMore(consumed) => {
                    state.read_buf.drain(body_start..body_start + consumed);
                    self.read_more()
                }
            }
        } else {
            self.read_more()
        }
    }

    fn deliver_success(&mut self) -> Effect<Self> {
        let state = self.state.take().expect("state present at delivery");
        let head = state.parsed_head.expect("head parsed before delivery");
        let body = if head.chunked {
            state.body_buf
        } else {
            let body_end = state.head_len + head.content_length;
            state.read_buf[state.head_len..body_end].to_vec()
        };
        let response = HttpResponse {
            status: head.status,
            version: head.version,
            headers: head.headers,
            body: crate::HttpResponseBody::Buffered(body),
        };
        self.finish(Ok(response), state.transport, state.reply_to)
    }

    fn fail(&mut self, error: HttpClientError) -> Effect<Self> {
        let (transport, reply_to) = self
            .state
            .take()
            .map(|s| (s.transport, s.reply_to))
            .unwrap_or((None, None));
        self.finish(Err(error), transport, reply_to)
    }

    /// Replies the result and closes the underlying transport, if any.
    fn finish(
        &mut self,
        result: Result<HttpResponse, HttpClientError>,
        transport: Option<HttpTransport>,
        reply_to: Option<RequestContext<Result<HttpResponse, HttpClientError>>>,
    ) -> Effect<Self> {
        let reply_effect: Effect<Self> = match reply_to {
            Some(request) => reply_to_request(request, result),
            None => reply(result),
        };
        let Some(transport) = transport else {
            return reply_effect;
        };
        let close_effect: Effect<Self> = match transport {
            HttpTransport::Tcp(stream) => tcp_close_stream(stream).then(HttpClientMsg::Closed),
            HttpTransport::Tls(stream) => {
                tls_close(stream, self.config.request_timeout).then(HttpClientMsg::Closed)
            }
        };
        batch(vec![reply_effect, close_effect])
    }
}

fn body_complete(state: &ActiveCall) -> bool {
    let Some(head) = state.parsed_head.as_ref() else {
        return false;
    };
    if head.chunked {
        false
    } else {
        let needed = state.head_len + head.content_length;
        state.read_buf.len() >= needed
    }
}

fn tls_read_reply_to_tcp(reply: TlsReadBufReply) -> TcpReadBufReply {
    TcpReadBufReply {
        buffer: reply.buffer,
        len: reply.len,
    }
}

fn tls_write_reply_to_tcp(reply: TlsWriteOwnedReply) -> TcpWriteOwnedReply {
    TcpWriteOwnedReply {
        bytes: reply.bytes,
        written: reply.written,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tina::Isolate;
    use tina::{IsolateId, ShardId};
    use tina_runtime::StreamId;

    #[derive(Debug, Default)]
    struct TestShard;

    impl Shard for TestShard {
        fn id(&self) -> ShardId {
            ShardId::new(77)
        }
    }

    fn client() -> HttpClient<TestShard> {
        HttpClient::new(HttpClientConfig::dev())
    }

    fn active_call(generation: u64) -> ActiveCall {
        ActiveCall {
            generation,
            request_bytes: b"GET / HTTP/1.1\r\nHost: x\r\n\r\n".to_vec(),
            transport: None,
            pending_write: Vec::new(),
            read_scratch: Vec::new(),
            read_buf: Vec::new(),
            parsed_head: None,
            head_len: 0,
            chunked_decoder: None,
            body_buf: Vec::new(),
            reply_to: None,
        }
    }

    fn context<'a>(
        shard: &'a mut TestShard,
    ) -> Context<'a, TestShard, Result<HttpResponse, HttpClientError>> {
        Context::new_typed(shard, IsolateId::new(1))
    }

    #[test]
    fn stale_deadline_does_not_timeout_next_request() {
        let mut client = client();
        client.state = Some(active_call(2));
        let mut shard = TestShard;
        let mut ctx = context(&mut shard);

        let _ = client.handle(
            HttpClientMsg::Deadline {
                generation: 1,
                result: Ok(()),
            },
            &mut ctx,
        );

        assert!(client.state.is_some(), "current request must stay active");
    }

    #[test]
    fn stale_read_does_not_reach_next_request_buffer() {
        let mut client = client();
        client.state = Some(active_call(2));
        let mut shard = TestShard;
        let mut ctx = context(&mut shard);
        let bytes = b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\nstale".to_vec();

        let _ = client.handle(
            HttpClientMsg::Read {
                generation: 1,
                result: Ok(TcpReadBufReply {
                    len: bytes.len(),
                    buffer: bytes,
                }),
            },
            &mut ctx,
        );

        let active = client.state.as_ref().expect("current request remains");
        assert!(
            active.read_buf.is_empty(),
            "stale bytes must not cross requests"
        );
    }

    #[test]
    fn stale_connected_does_not_install_transport() {
        let mut client = client();
        client.state = Some(active_call(2));
        let mut shard = TestShard;
        let mut ctx = context(&mut shard);

        let _ = client.handle(
            HttpClientMsg::Connected {
                generation: 1,
                result: Ok((
                    StreamId::new(44),
                    "127.0.0.1:12345".parse().unwrap(),
                    "127.0.0.1:80".parse().unwrap(),
                )),
            },
            &mut ctx,
        );

        let active = client.state.as_ref().expect("current request remains");
        assert!(
            active.transport.is_none(),
            "stale stream must not become current"
        );
        assert!(
            !active.request_bytes.is_empty(),
            "current request bytes must remain pending for its own connect"
        );
    }
}

/// Resolves the wire `Host:` from the target's host policy.
///
/// - `Http { host: None }` leaves the request untouched.
/// - `Http { host: Some(v) }` inserts `v` (or errors on duplicate).
/// - `Https { host: UseServerName }` inserts `server_name`.
/// - `Https { host: Explicit(v) }` inserts `v`.
fn apply_host_policy(
    mut request: HttpRequest,
    target: &HttpTarget,
) -> Result<HttpRequest, HttpClientError> {
    if !is_valid_origin_form_request_target(&request.path) {
        return Err(HttpClientError::InvalidRequestTarget);
    }
    let policy_value: Option<&str> = match target {
        HttpTarget::Http { host: None, .. } => None,
        HttpTarget::Http { host: Some(v), .. } => Some(v.as_str()),
        HttpTarget::Https {
            server_name,
            host: HttpHostPolicy::UseServerName,
            ..
        } => Some(server_name.as_str()),
        HttpTarget::Https {
            host: HttpHostPolicy::Explicit(v),
            ..
        } => Some(v.as_str()),
    };
    let Some(policy_value) = policy_value else {
        return Ok(request);
    };
    // Empty host is a valid `HeaderValue` byte string but produces
    // `Host: \r\n` on the wire — silently surprising. Reject so the
    // user picks `host: None` if that was the intent.
    if policy_value.is_empty() {
        return Err(HttpClientError::InvalidHostHeaderValue);
    }
    if request.headers.contains_key(HOST) {
        return Err(HttpClientError::DuplicateHostHeader);
    }
    let value =
        HeaderValue::from_str(policy_value).map_err(|_| HttpClientError::InvalidHostHeaderValue)?;
    request.headers.insert(HOST, value);
    Ok(request)
}
