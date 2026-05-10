//! Native HTTPS client tests against a thread-spawned rustls server
//! (Tina's TLS lane has one worker — server and client cannot share
//! a runtime).

use std::convert::Infallible;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener as StdTcpListener};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use http::header::HOST;
use http::{HeaderValue, Method, StatusCode};
use tina::prelude::*;
use tina_http::{
    HttpClient, HttpClientConfig, HttpClientError, HttpClientMsg, HttpHostPolicy, HttpRequest,
    HttpRequestBody, HttpResponse, HttpResponseBody, HttpTarget, HttpTransportPhase, TlsTrustRoots,
};
use tina_runtime::{
    CallError, CallOutcome, DefaultThreadedMailboxFactory, ThreadedRuntime, ThreadedRuntimeConfig,
};

#[derive(Debug, Default)]
struct TestShard;

impl Shard for TestShard {
    fn id(&self) -> ShardId {
        ShardId::new(202)
    }
}

struct CertBundle {
    cert_der: Vec<u8>,
    server_config: rustls::ServerConfig,
}

fn build_cert_bundle() -> CertBundle {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let certified =
        rcgen::generate_simple_self_signed(vec!["localhost".to_string()]).expect("rcgen self-sign");
    let cert_der = certified.cert.der().to_vec();
    let key_der = certified.key_pair.serialize_der();
    let server_cert = rustls::pki_types::CertificateDer::from(cert_der.clone());
    let server_key = rustls::pki_types::PrivateKeyDer::Pkcs8(
        rustls::pki_types::PrivatePkcs8KeyDer::from(key_der),
    );
    let server_config = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(vec![server_cert], server_key)
        .expect("server config");
    CertBundle {
        cert_der,
        server_config,
    }
}

/// One-shot rustls HTTPS server: reads one request, replies 200 with
/// the wire `Host:` value as the body.
fn spawn_one_shot_https_server(server_config: rustls::ServerConfig) -> SocketAddr {
    let listener = StdTcpListener::bind("127.0.0.1:0").expect("bind one-shot https");
    let addr = listener.local_addr().expect("local addr");
    thread::spawn(move || {
        let (tcp, _) = match listener.accept() {
            Ok(pair) => pair,
            Err(_) => return,
        };
        tcp.set_read_timeout(Some(Duration::from_secs(5)))
            .expect("read timeout");
        tcp.set_write_timeout(Some(Duration::from_secs(5)))
            .expect("write timeout");
        let connection = match rustls::ServerConnection::new(Arc::new(server_config)) {
            Ok(c) => c,
            Err(_) => return,
        };
        let mut stream = rustls::StreamOwned::new(connection, tcp);
        let mut buf = [0u8; 4096];
        let n = match stream.read(&mut buf) {
            Ok(n) => n,
            Err(_) => return,
        };
        let request = std::str::from_utf8(&buf[..n]).unwrap_or("");
        let host = request
            .lines()
            .find_map(|line| {
                line.strip_prefix("Host: ")
                    .or_else(|| line.strip_prefix("host: "))
            })
            .unwrap_or("")
            .trim_end_matches('\r');
        let body = host.as_bytes();
        let head = format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        );
        let _ = stream.write_all(head.as_bytes());
        let _ = stream.write_all(body);
        let _ = stream.flush();
        stream.conn.send_close_notify();
        let _ = stream.flush();
    });
    addr
}

fn run_one_fetch(
    target: HttpTarget,
    request: HttpRequest,
) -> Result<HttpResponse, HttpClientError> {
    let runtime = ThreadedRuntime::with_config(
        TestShard,
        DefaultThreadedMailboxFactory,
        ThreadedRuntimeConfig {
            command_capacity: 64,
            idle_wait: Duration::from_millis(1),
            ..Default::default()
        },
    );
    let client = runtime
        .register_with_capacity::<HttpClient<TestShard>, Infallible>(
            HttpClient::<TestShard>::new(HttpClientConfig::dev()),
            16,
        )
        .expect("register client");
    let result = match runtime
        .call_blocking(
            client,
            HttpClientMsg::call(target, request),
            Duration::from_secs(5),
        )
        .expect("client call runs")
    {
        CallOutcome::Replied(inner) => inner,
        CallOutcome::Full => Err(HttpClientError::Busy),
        CallOutcome::Closed => Err(HttpClientError::Closed),
        CallOutcome::Timeout => Err(HttpClientError::Timeout),
    };
    let _ = runtime.shutdown();
    result
}

fn body_str(response: &HttpResponse) -> String {
    let bytes = match &response.body {
        HttpResponseBody::Buffered(b) => b.clone(),
        HttpResponseBody::Stream(_) | HttpResponseBody::ChunkedStream(_) => Vec::new(),
    };
    String::from_utf8(bytes).expect("utf8 body")
}

fn empty_request(path: &str) -> HttpRequest {
    HttpRequest {
        method: Method::GET,
        path: path.to_string(),
        version: http::Version::HTTP_11,
        headers: http::HeaderMap::new(),
        body: HttpRequestBody::default(),
    }
}

#[test]
fn https_client_round_trips_against_rustls_server() {
    let bundle = build_cert_bundle();
    let addr = spawn_one_shot_https_server(bundle.server_config);
    let trust = TlsTrustRoots::from_der(vec![bundle.cert_der]);
    let target = HttpTarget::https(addr, "localhost", trust);
    let response = run_one_fetch(target, empty_request("/host")).expect("https fetch ok");
    assert_eq!(response.status, StatusCode::OK);
    assert_eq!(body_str(&response), "localhost");
}

#[test]
fn explicit_host_policy_overrides_default() {
    let bundle = build_cert_bundle();
    let addr = spawn_one_shot_https_server(bundle.server_config);
    let trust = TlsTrustRoots::from_der(vec![bundle.cert_der]);
    let target = HttpTarget::Https {
        addr,
        server_name: "localhost".to_string(),
        trust_roots: trust,
        host: HttpHostPolicy::Explicit("api.example.com".to_string()),
    };
    let response = run_one_fetch(target, empty_request("/host")).expect("https fetch ok");
    assert_eq!(response.status, StatusCode::OK);
    assert_eq!(body_str(&response), "api.example.com");
}

#[test]
fn empty_host_policy_value_is_typed_error() {
    // Empty Host on the wire (`Host: \r\n`) is silently surprising;
    // reject so the user picks `host: None` if that was the intent.
    let bundle = build_cert_bundle();
    let addr = spawn_one_shot_https_server(bundle.server_config);
    let trust = TlsTrustRoots::from_der(vec![bundle.cert_der]);
    let target = HttpTarget::Https {
        addr,
        server_name: "localhost".to_string(),
        trust_roots: trust,
        host: HttpHostPolicy::Explicit(String::new()),
    };
    let result = run_one_fetch(target, empty_request("/host"));
    assert!(
        matches!(result, Err(HttpClientError::InvalidHostHeaderValue)),
        "expected InvalidHostHeaderValue for empty policy, got {result:?}"
    );
}

#[test]
fn empty_http_host_policy_is_typed_error() {
    // Symmetric proof for plain HTTP targets.
    use std::net::TcpListener as StdTcpListener;
    let listener = StdTcpListener::bind("127.0.0.1:0").expect("bind plain");
    let addr = listener.local_addr().expect("addr");
    drop(listener);
    let target = HttpTarget::http_with_host(addr, "");
    let result = run_one_fetch(target, empty_request("/"));
    assert!(
        matches!(result, Err(HttpClientError::InvalidHostHeaderValue)),
        "expected InvalidHostHeaderValue on empty HTTP host, got {result:?}"
    );
}

#[test]
fn invalid_host_policy_value_is_typed_error() {
    // Failure surfaces at encode — no tls_connect runs. `\n` makes
    // `HeaderValue::from_str` reject the policy value.
    let bundle = build_cert_bundle();
    let addr = spawn_one_shot_https_server(bundle.server_config);
    let trust = TlsTrustRoots::from_der(vec![bundle.cert_der]);
    let target = HttpTarget::Https {
        addr,
        server_name: "localhost".to_string(),
        trust_roots: trust,
        host: HttpHostPolicy::Explicit("bad\nhost".to_string()),
    };
    let result = run_one_fetch(target, empty_request("/host"));
    assert!(
        matches!(result, Err(HttpClientError::InvalidHostHeaderValue)),
        "expected InvalidHostHeaderValue for non-ASCII Host policy, got {result:?}"
    );
}

#[test]
fn http_target_host_policy_inserts_host_header() {
    // Verifies the symmetric HTTP-side host policy: HttpTarget::Http
    // with `Some(host)` populates the wire `Host:` header just like
    // HTTPS does.
    use std::io::{Read, Write};
    use std::net::TcpListener as StdTcpListener;
    let listener = StdTcpListener::bind("127.0.0.1:0").expect("bind plain");
    let addr = listener.local_addr().expect("addr");
    let server_thread = thread::spawn(move || {
        let (mut tcp, _) = listener.accept().expect("accept");
        tcp.set_read_timeout(Some(Duration::from_secs(5))).ok();
        let mut buf = [0u8; 4096];
        let mut have = 0;
        loop {
            let n = match tcp.read(&mut buf[have..]) {
                Ok(0) | Err(_) => return Vec::new(),
                Ok(n) => n,
            };
            have += n;
            if buf[..have].windows(4).any(|w| w == b"\r\n\r\n") {
                break;
            }
        }
        let head = std::str::from_utf8(&buf[..have]).unwrap_or("");
        // Encoder emits header names in canonical lowercase.
        let host = head
            .lines()
            .find_map(|line| {
                line.strip_prefix("host: ")
                    .or_else(|| line.strip_prefix("Host: "))
            })
            .unwrap_or("")
            .trim_end_matches('\r')
            .as_bytes()
            .to_vec();
        let head = format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            host.len()
        );
        let _ = tcp.write_all(head.as_bytes());
        let _ = tcp.write_all(&host);
        host
    });

    let target = HttpTarget::http_with_host(addr, "internal.svc");
    let result = run_one_fetch(target, empty_request("/")).expect("plain http fetch ok");
    let observed = server_thread.join().expect("server thread");
    assert_eq!(observed, b"internal.svc");
    assert_eq!(result.status, StatusCode::OK);
}

#[test]
fn duplicate_host_header_on_http_target_is_typed_error() {
    // Symmetric DuplicateHostHeader proof for the HTTP path.
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind plain");
    let addr = listener.local_addr().expect("addr");
    // Server doesn't matter — failure surfaces at encode.
    drop(listener);
    let target = HttpTarget::http_with_host(addr, "internal.svc");
    let mut request = empty_request("/");
    request
        .headers
        .insert(HOST, HeaderValue::from_static("conflict.example.com"));
    let result = run_one_fetch(target, request);
    assert!(
        matches!(result, Err(HttpClientError::DuplicateHostHeader)),
        "expected DuplicateHostHeader on HTTP target, got {result:?}"
    );
}

#[test]
fn duplicate_host_header_is_typed_error() {
    let bundle = build_cert_bundle();
    let addr = spawn_one_shot_https_server(bundle.server_config);
    let trust = TlsTrustRoots::from_der(vec![bundle.cert_der]);
    let target = HttpTarget::https(addr, "localhost", trust);
    let mut request = empty_request("/host");
    request
        .headers
        .insert(HOST, HeaderValue::from_static("conflict.example.com"));
    let result = run_one_fetch(target, request);
    assert!(
        matches!(result, Err(HttpClientError::DuplicateHostHeader)),
        "expected DuplicateHostHeader, got {result:?}"
    );
}

#[test]
fn bad_server_name_yields_typed_tls_name_error() {
    // `ServerName::try_from` fails before any TCP connect.
    let bundle = build_cert_bundle();
    let addr = spawn_one_shot_https_server(bundle.server_config);
    let trust = TlsTrustRoots::from_der(vec![bundle.cert_der]);
    let target = HttpTarget::Https {
        addr,
        server_name: "not a valid dns name!".to_string(),
        trust_roots: trust,
        host: HttpHostPolicy::UseServerName,
    };
    let result = run_one_fetch(target, empty_request("/host"));
    match result {
        Err(HttpClientError::Transport {
            phase: HttpTransportPhase::Connect,
            source: CallError::TlsName,
        }) => {}
        other => panic!("expected TlsName transport error, got {other:?}"),
    }
}

#[test]
fn untrusted_root_yields_typed_transport_error() {
    let bundle = build_cert_bundle();
    let addr = spawn_one_shot_https_server(bundle.server_config);
    // Different cert as the trust root → server leaf isn't anchored.
    let other = rcgen::generate_simple_self_signed(vec!["other.example".to_string()])
        .expect("rcgen self-sign 2");
    let untrusted = TlsTrustRoots::from_der(vec![other.cert.der().to_vec()]);
    let target = HttpTarget::Https {
        addr,
        server_name: "localhost".to_string(),
        trust_roots: untrusted,
        host: HttpHostPolicy::UseServerName,
    };
    let result = run_one_fetch(target, empty_request("/host"));
    match result {
        // TlsHandshake (peer rejected) or Io (peer aborted) — both
        // are typed Transport variants, not the flat TCP `Connect`.
        Err(HttpClientError::Transport {
            phase: HttpTransportPhase::Connect,
            source: CallError::TlsHandshake,
        }) => {}
        Err(HttpClientError::Transport {
            phase: HttpTransportPhase::Connect,
            source: CallError::Io,
        }) => {}
        other => panic!("expected typed Transport(Connect, TlsHandshake|Io), got {other:?}"),
    }
}
