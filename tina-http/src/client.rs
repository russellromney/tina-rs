//! Native HTTP/1.1 client connection isolate.
//!
//! [`HttpClient`] is a long-lived service-shaped isolate. Users invoke
//! it the same way as any other Tina service:
//!
//! ```rust,ignore
//! call(http_client, HttpClientMsg::call(target, request), timeout)
//!     .reply(MyMsg::HttpReturned)
//! ```
//!
//! One request at a time. A new `Call` while busy replies
//! [`HttpClientError::Busy`]. For parallelism, register multiple
//! clients or front the client with [`crate::HttpConnectionPool`].

use std::marker::PhantomData;
use std::net::SocketAddr;

use http::header::HOST;
use http::HeaderValue;
use tina::prelude::*;
use tina_runtime::{CallError, StreamId, TlsStreamId, sleep, tcp_connect, tls_connect};

use crate::parse::{HttpResponseHead, ResponseParseProgress, encode_request, parse_response_head};
use crate::target::{HttpHostPolicy, HttpTarget};
use crate::transport::HttpTransport;
use crate::types::{
    HttpClientConfig, HttpClientError, HttpRequest, HttpResponse, HttpTransportPhase,
};

/// Bytes the client asks for per `tcp_read`. Matches the server side.
const READ_CHUNK: usize = 4096;

/// One outbound HTTP/1.1 call: which target to connect to and what
/// request to send.
#[derive(Debug, Clone)]
pub struct OutboundCall {
    /// Where (and how) to send the request. Plain TCP or native HTTPS.
    pub target: HttpTarget,
    /// Request to write on the wire. The encoder fills in
    /// `Content-Length` and `Connection: close` automatically. For
    /// HTTPS, the `Host:` header is auto-populated from the target's
    /// [`HttpHostPolicy`] unless one was supplied explicitly (in
    /// which case [`HttpClientError::DuplicateHostHeader`] surfaces).
    pub request: HttpRequest,
}

/// Messages handled by [`HttpClient`].
///
/// `Call` is the user-callable variant. The rest are internal
/// continuations produced by the client's own runtime calls; user code
/// never constructs them directly.
#[derive(Debug, Clone)]
pub enum HttpClientMsg {
    /// Run this outbound call.
    Call(OutboundCall),
    /// `tcp_connect` reply.
    Connected(Result<(StreamId, SocketAddr, SocketAddr), CallError>),
    /// `tls_connect` reply.
    TlsConnected(Result<TlsStreamId, CallError>),
    /// `tcp_write` / `tls_write` reply.
    Wrote(Result<usize, CallError>),
    /// `tcp_read` / `tls_read` reply.
    Read(Result<Vec<u8>, CallError>),
    /// `tcp_close_stream` / `tls_close` reply.
    Closed(Result<(), CallError>),
    /// Per-call deadline timer fired.
    Deadline(Result<(), CallError>),
}

impl HttpClientMsg {
    /// Builds a `Call` variant from the constituent parts. Accepts
    /// any value that converts into [`HttpTarget`] — a bare
    /// `SocketAddr` is interpreted as plain HTTP for backward
    /// compatibility.
    pub fn call(target: impl Into<HttpTarget>, request: HttpRequest) -> Self {
        Self::Call(OutboundCall {
            target: target.into(),
            request,
        })
    }
}

/// Service-shaped HTTP/1.1 client.
///
/// Generic over `S: Shard`. Reply type is
/// `Result<HttpResponse, HttpClientError>`.
pub struct HttpClient<S: Shard + 'static> {
    config: HttpClientConfig,
    state: Option<ActiveCall>,
    _shard: PhantomData<S>,
}

/// State for one in-flight call. Cleared at terminal.
struct ActiveCall {
    request_bytes: Vec<u8>,
    transport: Option<HttpTransport>,
    pending_write: Vec<u8>,
    read_buf: Vec<u8>,
    parsed_head: Option<HttpResponseHead>,
    head_len: usize,
}

impl<S: Shard + 'static> HttpClient<S> {
    /// Constructs a fresh client with the given config.
    pub fn new(config: HttpClientConfig) -> Self {
        Self {
            config,
            state: None,
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
            HttpClientMsg::Call(call) => self.handle_call(call),

            HttpClientMsg::Connected(Ok((stream, _local, _peer))) => {
                let transport = HttpTransport::Tcp(stream);
                let Some(state) = self.state.as_mut() else {
                    // Stale: previous call already ended. Close the
                    // dangling stream so the kernel does not leak.
                    return transport
                        .close_call(self.config.request_timeout)
                        .reply(HttpClientMsg::Closed);
                };
                state.transport = Some(transport);
                state.pending_write = state.request_bytes.clone();
                self.write_more()
            }
            HttpClientMsg::Connected(Err(_)) => self.fail(HttpClientError::Connect),

            HttpClientMsg::TlsConnected(Ok(stream)) => {
                let transport = HttpTransport::Tls(stream);
                let Some(state) = self.state.as_mut() else {
                    return transport
                        .close_call(self.config.request_timeout)
                        .reply(HttpClientMsg::Closed);
                };
                state.transport = Some(transport);
                state.pending_write = state.request_bytes.clone();
                self.write_more()
            }
            HttpClientMsg::TlsConnected(Err(source)) => self.fail(HttpClientError::Transport {
                phase: HttpTransportPhase::Connect,
                source,
            }),

            HttpClientMsg::Wrote(Ok(count)) => self.handle_wrote(count),
            HttpClientMsg::Wrote(Err(source)) => self.fail(self.transport_or_flat_error(
                HttpTransportPhase::Write,
                source,
                HttpClientError::Write,
            )),

            HttpClientMsg::Read(Ok(bytes)) => self.handle_bytes_read(bytes),
            HttpClientMsg::Read(Err(source)) => self.fail(self.transport_or_flat_error(
                HttpTransportPhase::Read,
                source,
                HttpClientError::Read,
            )),

            HttpClientMsg::Deadline(_) => {
                if self.state.is_some() {
                    self.fail(HttpClientError::Timeout)
                } else {
                    noop()
                }
            }

            HttpClientMsg::Closed(_) => noop(),
        }
    }
}

impl<S: Shard + 'static> HttpClient<S> {
    fn handle_call(&mut self, call: OutboundCall) -> Effect<Self> {
        if self.state.is_some() {
            return reply(Err(HttpClientError::Busy));
        }
        let OutboundCall { target, request } = call;
        // Apply the Host policy *before* encoding so the wire bytes
        // already carry the right `Host:` value.
        let request = match apply_host_policy(request, &target) {
            Ok(request) => request,
            Err(error) => return reply(Err(error)),
        };
        let request_bytes = encode_request(&request);
        self.state = Some(ActiveCall {
            request_bytes,
            transport: None,
            pending_write: Vec::new(),
            read_buf: Vec::new(),
            parsed_head: None,
            head_len: 0,
        });
        let connect_effect: Effect<Self> = match target {
            HttpTarget::Http(addr) => tcp_connect(addr).reply(HttpClientMsg::Connected),
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
            .reply(HttpClientMsg::TlsConnected),
        };
        let deadline_effect: Effect<Self> =
            sleep(self.config.request_timeout).reply(HttpClientMsg::Deadline);
        batch(vec![connect_effect, deadline_effect])
    }

    /// If the active call is on a TLS transport, surface the typed
    /// `Transport` variant carrying the runtime error and phase. If
    /// the active call is on TCP (or there is no active state), keep
    /// the older flat variant for source compat.
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

    fn write_more(&mut self) -> Effect<Self> {
        let state = self.state.as_ref().expect("state present during write");
        let transport = state.transport.expect("transport set before write");
        transport
            .write_call(state.pending_write.clone(), self.config.request_timeout)
            .reply(HttpClientMsg::Wrote)
    }

    fn handle_wrote(&mut self, count: usize) -> Effect<Self> {
        let Some(state) = self.state.as_mut() else {
            return noop();
        };
        if count == 0 {
            return self.fail(HttpClientError::Write);
        }
        if count >= state.pending_write.len() {
            state.pending_write.clear();
            self.read_more()
        } else {
            state.pending_write.drain(..count);
            self.write_more()
        }
    }

    fn read_more(&mut self) -> Effect<Self> {
        let state = self.state.as_ref().expect("state present during read");
        let transport = state.transport.expect("transport set before read");
        transport
            .read_call(READ_CHUNK, self.config.request_timeout)
            .reply(HttpClientMsg::Read)
    }

    fn handle_bytes_read(&mut self, bytes: Vec<u8>) -> Effect<Self> {
        let Some(state) = self.state.as_mut() else {
            return noop();
        };
        if bytes.is_empty() {
            // Peer closed: succeed if body is complete, else fail.
            return if state.parsed_head.is_some() && body_complete(state) {
                self.deliver_success()
            } else {
                self.fail(HttpClientError::Closed)
            };
        }
        state.read_buf.extend_from_slice(&bytes);

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
            self.deliver_success()
        } else {
            self.read_more()
        }
    }

    fn deliver_success(&mut self) -> Effect<Self> {
        let state = self.state.take().expect("state present at delivery");
        let head = state.parsed_head.expect("head parsed before delivery");
        let body_end = state.head_len + head.content_length;
        let body = state.read_buf[state.head_len..body_end].to_vec();
        let response = HttpResponse {
            status: head.status,
            version: head.version,
            headers: head.headers,
            body: crate::HttpResponseBody::Buffered(body),
        };
        self.finish(Ok(response), state.transport)
    }

    fn fail(&mut self, error: HttpClientError) -> Effect<Self> {
        let transport = self.state.take().and_then(|s| s.transport);
        self.finish(Err(error), transport)
    }

    /// Replies the result and closes the underlying transport, if any.
    fn finish(
        &mut self,
        result: Result<HttpResponse, HttpClientError>,
        transport: Option<HttpTransport>,
    ) -> Effect<Self> {
        let reply_effect: Effect<Self> = reply(result);
        if let Some(transport) = transport {
            let close_effect: Effect<Self> = transport
                .close_call(self.config.request_timeout)
                .reply(HttpClientMsg::Closed);
            batch(vec![reply_effect, close_effect])
        } else {
            reply_effect
        }
    }
}

fn body_complete(state: &ActiveCall) -> bool {
    let Some(head) = state.parsed_head.as_ref() else {
        return false;
    };
    let needed = state.head_len + head.content_length;
    state.read_buf.len() >= needed
}

/// Resolves the `Host:` header on an outbound request from the
/// target's [`HttpHostPolicy`]. Plain HTTP keeps whatever the caller
/// supplied (HTTP/1.1 requires the caller to set Host explicitly for
/// TCP targets; the encoder does not synthesize one). HTTPS targets
/// auto-populate Host from the policy unless the caller already set
/// one — in which case [`HttpClientError::DuplicateHostHeader`]
/// surfaces so the conflict is visible.
fn apply_host_policy(
    mut request: HttpRequest,
    target: &HttpTarget,
) -> Result<HttpRequest, HttpClientError> {
    let HttpTarget::Https {
        server_name, host, ..
    } = target
    else {
        return Ok(request);
    };
    let policy_value = match host {
        HttpHostPolicy::UseServerName => server_name.as_str(),
        HttpHostPolicy::Explicit(name) => name.as_str(),
    };
    if request.headers.contains_key(HOST) {
        return Err(HttpClientError::DuplicateHostHeader);
    }
    let value = HeaderValue::from_str(policy_value)
        .map_err(|_| HttpClientError::DuplicateHostHeader)?;
    request.headers.insert(HOST, value);
    Ok(request)
}
