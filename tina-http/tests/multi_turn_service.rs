//! Multi-turn service integration: a service that defers its reply by
//! issuing an outbound runtime call inside its handler, then replies
//! through the continuation chain.
//!
//! Demonstrates the shape unlocked by the generalized
//! `HttpListener<S, M>`: the service's message type is an enum with
//! both an `Inbound(HttpRequest)` variant and an internal continuation
//! variant. The connection isolate calls the service via
//! `From<HttpRequest> for ServiceMsg`. The service does its work
//! across multiple handler turns; the runtime preserves the call
//! context, so `Effect::Reply` from inside the continuation handler
//! replies to the original connection-isolate caller.

mod common;

use std::convert::Infallible;
use std::time::Duration;

use tina::prelude::*;
use tina::{CallContext, RequestContext, reply_to};
use tina_http::{
    HttpClient, HttpClientConfig, HttpClientError, HttpClientMsg, HttpLimits, HttpListener,
    HttpListenerMsg, HttpRequest, HttpResponse,
};
use tina_runtime::{
    CallOutcome, DefaultThreadedMailboxFactory, RuntimeCall, ThreadedRuntime,
    ThreadedRuntimeConfig, call,
};

use common::{TestShard, assert_status_starts_with, scripted_request};

/// Service messages: `Inbound` is the user-callable variant the
/// connection isolate constructs from `HttpRequest`; `ClientReturned`
/// is the continuation from the outbound `HttpClient` call.
#[derive(Debug)]
enum ProxyMsg {
    Inbound(HttpRequest),
    ClientReturned(
        RequestContext<HttpResponse>,
        CallOutcome<Result<HttpResponse, HttpClientError>>,
    ),
}

impl From<HttpRequest> for ProxyMsg {
    fn from(req: HttpRequest) -> Self {
        Self::Inbound(req)
    }
}

/// Service that proxies inbound requests to a fixed upstream via the
/// outbound `HttpClient`. The reply lands across two handler turns:
/// `Inbound` issues the outbound call; `ClientReturned` replies to the
/// connection isolate with the wire response.
struct Proxy {
    client: Address<HttpClientMsg, Result<HttpResponse, HttpClientError>>,
    upstream: std::net::SocketAddr,
    client_call_timeout: Duration,
}

impl Isolate for Proxy {
    tina::isolate_types! {
        message: ProxyMsg,
        reply: HttpResponse,
        send: tina::Outbound<Infallible>,
        spawn: Infallible,
        io: RuntimeCall<ProxyMsg>,
        shard: TestShard,
    }

    fn handle(
        &mut self,
        msg: ProxyMsg,
        _ctx: &mut Context<'_, TestShard, Self::Reply>,
    ) -> Effect<Self> {
        match msg {
            ProxyMsg::Inbound(_) => reply(HttpResponse::internal_error()),
            ProxyMsg::ClientReturned(request, outcome) => {
                reply_to(request, Self::response_for_client_outcome(outcome))
            }
        }
    }

    fn handle_call(&mut self, msg: ProxyMsg, call_ctx: CallContext<'_, Self>) -> Effect<Self> {
        match msg {
            ProxyMsg::Inbound(req) => {
                let request = call_ctx.into_request_context();
                self.forward_request(req, request)
            }
            ProxyMsg::ClientReturned(_, _) => {
                call_ctx.reject(tina::CallRejectedReason::UnsupportedMessage)
            }
        }
    }
}

impl Proxy {
    fn forward_request(
        &self,
        req: HttpRequest,
        request: RequestContext<HttpResponse>,
    ) -> Effect<Self> {
        // Forward the inbound path/method/body to upstream. Build a
        // fresh outbound request — the inbound's `Connection` and
        // `Host` headers are stripped by the outbound encoder.
        let body_bytes = req.body.as_buffered().unwrap_or(&[]).to_vec();
        let outbound = HttpRequest::builder(req.method, req.path)
            .header("Host", "upstream")
            .body(body_bytes)
            .build();
        call(
            self.client,
            HttpClientMsg::call(self.upstream, outbound),
            self.client_call_timeout,
        )
        .then_with_request(request, ProxyMsg::ClientReturned)
    }

    fn response_for_client_outcome(
        outcome: CallOutcome<Result<HttpResponse, HttpClientError>>,
    ) -> HttpResponse {
        match outcome {
            CallOutcome::Replied(Ok(r)) => r,
            // Client refused because it was already in flight,
            // or pool admission refused — map to 503.
            CallOutcome::Replied(Err(HttpClientError::Busy))
            | CallOutcome::Replied(Err(HttpClientError::PoolFull)) => {
                HttpResponse::service_unavailable()
            }
            // Client-side errors that aren't admission-flavoured.
            CallOutcome::Replied(Err(_)) => HttpResponse::bad_gateway(),
            CallOutcome::Full => HttpResponse::service_unavailable(),
            CallOutcome::Closed => HttpResponse::internal_error(),
            CallOutcome::Rejected(_) => HttpResponse::internal_error(),
            CallOutcome::Timeout => HttpResponse::gateway_timeout(),
        }
    }
}

#[test]
fn proxy_service_forwards_to_upstream_and_replies() {
    // Spin up an upstream HttpListener with a sync Counter service.
    let upstream_runtime = ThreadedRuntime::with_config(
        TestShard,
        DefaultThreadedMailboxFactory,
        ThreadedRuntimeConfig {
            command_capacity: 64,
            idle_wait: Duration::from_millis(1),
            ..Default::default()
        },
    );
    let upstream_counter = upstream_runtime
        .register_with_capacity::<common::Counter, Infallible>(common::Counter::default(), 16)
        .expect("register upstream counter");
    let upstream_listener_isolate = HttpListener::<TestShard>::new(
        "127.0.0.1:0".parse().expect("loopback"),
        upstream_counter,
        HttpLimits::default(),
        Duration::from_secs(2),
        16,
    );
    let upstream_listener = upstream_runtime
        .register_with_capacity::<HttpListener<TestShard>, _>(upstream_listener_isolate, 8)
        .expect("register upstream listener");
    let upstream_bound = upstream_runtime.observe_next_bound();
    upstream_runtime
        .try_send(upstream_listener, HttpListenerMsg::Start)
        .expect("send Start upstream");
    let upstream_addr = upstream_bound
        .wait(Duration::from_secs(2))
        .expect("upstream bound");

    // Spin up the proxy runtime: client + Proxy service + listener.
    let proxy_runtime = ThreadedRuntime::with_config(
        TestShard,
        DefaultThreadedMailboxFactory,
        ThreadedRuntimeConfig {
            command_capacity: 64,
            idle_wait: Duration::from_millis(1),
            ..Default::default()
        },
    );
    let client = proxy_runtime
        .register_with_capacity::<HttpClient<TestShard>, Infallible>(
            HttpClient::<TestShard>::new(HttpClientConfig::dev()),
            16,
        )
        .expect("register client");
    let proxy_service = proxy_runtime
        .register_with_capacity::<Proxy, Infallible>(
            Proxy {
                client,
                upstream: upstream_addr,
                client_call_timeout: Duration::from_secs(2),
            },
            16,
        )
        .expect("register proxy");
    let proxy_listener_isolate = HttpListener::<TestShard, ProxyMsg>::new(
        "127.0.0.1:0".parse().expect("loopback"),
        proxy_service,
        HttpLimits::default(),
        Duration::from_secs(5),
        16,
    );
    let proxy_listener = proxy_runtime
        .register_with_capacity::<HttpListener<TestShard, ProxyMsg>, _>(proxy_listener_isolate, 8)
        .expect("register proxy listener");
    let proxy_bound = proxy_runtime.observe_next_bound();
    proxy_runtime
        .try_send(proxy_listener, HttpListenerMsg::Start)
        .expect("send Start proxy");
    let proxy_addr = proxy_bound
        .wait(Duration::from_secs(2))
        .expect("proxy bound");

    // GET /counter through the proxy. Upstream counter starts at 0.
    let response = scripted_request(proxy_addr, b"GET /counter HTTP/1.1\r\nHost: x\r\n\r\n");
    assert_status_starts_with(&response, "200");
    let text = std::str::from_utf8(&response).expect("utf8");
    assert!(
        text.ends_with("0"),
        "expected counter=0 in response: {text}"
    );

    // POST /counter through the proxy bumps the upstream counter.
    let response = scripted_request(
        proxy_addr,
        b"POST /counter HTTP/1.1\r\nHost: x\r\nContent-Length: 0\r\n\r\n",
    );
    assert_status_starts_with(&response, "200");
    let text = std::str::from_utf8(&response).expect("utf8");
    assert!(
        text.ends_with("1"),
        "expected counter=1 in response: {text}"
    );

    // Tear down.
    let _ = proxy_runtime.try_send(proxy_listener, HttpListenerMsg::Stop);
    let _ = proxy_runtime.shutdown();
    let _ = upstream_runtime.try_send(upstream_listener, HttpListenerMsg::Stop);
    let _ = upstream_runtime.shutdown();
}

#[test]
fn proxy_service_returns_503_when_outbound_client_is_busy() {
    // Black-hole upstream: accepts but never replies. The proxy's
    // outbound client takes the first call and stays busy. Concurrent
    // inbound calls find the client mailbox full → CallOutcome::Full
    // at the proxy's `call(client, ...)` → proxy maps to 503.
    use std::net::TcpListener as StdTcpListener;
    use std::thread;
    let black_hole = StdTcpListener::bind("127.0.0.1:0").expect("bind black-hole");
    let upstream_addr = black_hole.local_addr().expect("upstream addr");
    let _silent = thread::spawn(move || {
        while let Ok((stream, _)) = black_hole.accept() {
            thread::sleep(Duration::from_secs(5));
            drop(stream);
        }
    });

    let proxy_runtime = ThreadedRuntime::with_config(
        TestShard,
        DefaultThreadedMailboxFactory,
        ThreadedRuntimeConfig {
            command_capacity: 64,
            idle_wait: Duration::from_millis(1),
            ..Default::default()
        },
    );
    // Mailbox capacity 1 on the client: a second concurrent call from
    // the proxy will find the mailbox full and the runtime returns
    // CallOutcome::Full to the proxy.
    let client = proxy_runtime
        .register_with_capacity::<HttpClient<TestShard>, Infallible>(
            HttpClient::<TestShard>::new(HttpClientConfig::dev()),
            1,
        )
        .expect("register client");
    let proxy_service = proxy_runtime
        .register_with_capacity::<Proxy, Infallible>(
            Proxy {
                client,
                upstream: upstream_addr,
                client_call_timeout: Duration::from_secs(2),
            },
            16,
        )
        .expect("register proxy");
    let proxy_listener_isolate = HttpListener::<TestShard, ProxyMsg>::new(
        "127.0.0.1:0".parse().expect("loopback"),
        proxy_service,
        HttpLimits::default(),
        Duration::from_secs(2),
        16,
    );
    let proxy_listener = proxy_runtime
        .register_with_capacity::<HttpListener<TestShard, ProxyMsg>, _>(proxy_listener_isolate, 8)
        .expect("register proxy listener");
    let proxy_bound = proxy_runtime.observe_next_bound();
    proxy_runtime
        .try_send(proxy_listener, HttpListenerMsg::Start)
        .expect("send Start");
    let proxy_addr = proxy_bound
        .wait(Duration::from_secs(2))
        .expect("proxy bound");

    // Fire two concurrent inbound TCPs against the proxy. Each spawns
    // a connection isolate that calls the proxy service. The first
    // proxy.handle(Inbound) issues call(client, ...) and ties up the
    // client. The second proxy.handle(Inbound) issues call(client,
    // ...) which sees the client mailbox full → CallOutcome::Full →
    // 503 to the wire.
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    let count_200 = Arc::new(AtomicUsize::new(0));
    let count_503 = Arc::new(AtomicUsize::new(0));
    let count_other = Arc::new(AtomicUsize::new(0));
    let mut handles = Vec::new();
    for _ in 0..6 {
        let c200 = Arc::clone(&count_200);
        let c503 = Arc::clone(&count_503);
        let cother = Arc::clone(&count_other);
        handles.push(thread::spawn(move || {
            use std::io::{Read, Write};
            use std::net::TcpStream;
            let Ok(mut stream) = TcpStream::connect_timeout(&proxy_addr, Duration::from_secs(2))
            else {
                cother.fetch_add(1, Ordering::Relaxed);
                return;
            };
            let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
            let _ = stream.write_all(b"GET /x HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n");
            let _ = stream.flush();
            let mut buf = Vec::new();
            let _ = stream.read_to_end(&mut buf);
            let text = String::from_utf8_lossy(&buf);
            if text.starts_with("HTTP/1.1 200") {
                c200.fetch_add(1, Ordering::Relaxed);
            } else if text.starts_with("HTTP/1.1 503") {
                c503.fetch_add(1, Ordering::Relaxed);
            } else {
                cother.fetch_add(1, Ordering::Relaxed);
            }
        }));
    }
    for h in handles {
        h.join().expect("client thread");
    }

    let s = count_200.load(Ordering::Relaxed);
    let u = count_503.load(Ordering::Relaxed);
    let o = count_other.load(Ordering::Relaxed);
    // The first concurrent caller occupies the client (state=Some) and
    // sits there until its 2s client_call_timeout fires. The rest call
    // the client, see state=Some, and reply Err(Busy) — proxy maps to
    // 503 on the wire. The first eventually times out: proxy maps to
    // 504 (HTTP/1.1 504, also non-200, non-503). We accept either.
    let count_other_5xx = u + count_other.load(Ordering::Relaxed);
    assert_eq!(s, 0, "no 200s (upstream is a black hole): {s}/{u}/{o}");
    assert!(
        u >= 1,
        "at least one wire-level 503 from Busy mapping: {s}/{u}/{o}"
    );
    let _ = count_other_5xx; // keep the variable expressive even when we only assert on `u`

    let _ = proxy_runtime.try_send(proxy_listener, HttpListenerMsg::Stop);
    let _ = proxy_runtime.shutdown();
}
