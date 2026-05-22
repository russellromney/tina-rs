//! Live h2/TLS proofs for the native HTTP/2 client.
//!
//! Stands up a rustls TLS server that negotiates ALPN and then speaks
//! HTTP/2 over the encrypted stream, and dials it with the native client
//! using an `Http2Target::Tls`. Asserts:
//! - happy path: server advertises `h2`, client offers `h2`, the request
//!   round-trips over TLS and returns `Replied`
//! - ALPN mismatch: server advertises no `h2`, client offers `h2`, the
//!   client surfaces `Http2ClientOutcome::TlsAlpnMismatch` (the rail
//!   fails the connect before any HTTP/2 framing)
//!
//! The server peer is hand-rolled (rustls + raw HTTP/2 framing),
//! independent of the client's code.

mod common;

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener};
use std::sync::Arc;
use std::time::Duration;

use common::TestShard;
use tina::prelude::*;
use tina_http::{
    AlpnProtocols, Http2ClientConnection, Http2ClientLimits, Http2ClientMsg, Http2ClientOutcome,
    Http2ClientReply, Http2ClientRequest, Http2Target,
};
use tina_runtime::{
    CallOutcome, DefaultThreadedMailboxFactory, ThreadedRuntime, ThreadedRuntimeConfig,
};

const CLIENT_PREFACE: &[u8] = b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n";
const FRAME_DATA: u8 = 0x0;
const FRAME_HEADERS: u8 = 0x1;
const FRAME_SETTINGS: u8 = 0x4;
const FLAG_ACK: u8 = 0x1;
const FLAG_END_STREAM: u8 = 0x1;
const FLAG_END_HEADERS: u8 = 0x4;

// ---- HPACK literal-without-indexing encoding (mirror of the client) ----

fn encode_int(mut value: usize, prefix_bits: u8, pattern: u8, out: &mut Vec<u8>) {
    let max = (1_usize << prefix_bits) - 1;
    if value < max {
        out.push(pattern | value as u8);
        return;
    }
    out.push(pattern | max as u8);
    value -= max;
    while value >= 128 {
        out.push((value as u8 & 0x7f) | 0x80);
        value >>= 7;
    }
    out.push(value as u8);
}

fn literal_header(name: &str, value: &str, out: &mut Vec<u8>) {
    out.push(0);
    encode_int(name.len(), 7, 0, out);
    out.extend_from_slice(name.as_bytes());
    encode_int(value.len(), 7, 0, out);
    out.extend_from_slice(value.as_bytes());
}

fn write_frame<S: Write>(stream: &mut S, ty: u8, flags: u8, stream_id: u32, payload: &[u8]) {
    let len = payload.len();
    let mut out = Vec::with_capacity(9 + len);
    out.push(((len >> 16) & 0xff) as u8);
    out.push(((len >> 8) & 0xff) as u8);
    out.push((len & 0xff) as u8);
    out.push(ty);
    out.push(flags);
    out.extend_from_slice(&(stream_id & 0x7fff_ffff).to_be_bytes());
    out.extend_from_slice(payload);
    stream.write_all(&out).expect("write frame");
    stream.flush().expect("flush frame");
}

struct RawFrame {
    ty: u8,
    flags: u8,
    stream_id: u32,
    #[allow(dead_code)]
    payload: Vec<u8>,
}

fn read_frame<S: Read>(stream: &mut S) -> std::io::Result<RawFrame> {
    let mut head = [0_u8; 9];
    stream.read_exact(&mut head)?;
    let len = ((head[0] as usize) << 16) | ((head[1] as usize) << 8) | head[2] as usize;
    let mut payload = vec![0_u8; len];
    stream.read_exact(&mut payload)?;
    let mut sid = [0_u8; 4];
    sid.copy_from_slice(&head[5..9]);
    Ok(RawFrame {
        ty: head[3],
        flags: head[4],
        stream_id: u32::from_be_bytes(sid) & 0x7fff_ffff,
        payload,
    })
}

/// Generate a self-signed cert and a rustls server config advertising
/// `server_alpn`. Returns the config and the cert DER (the client trust
/// root).
fn server_config(server_alpn: Vec<Vec<u8>>) -> (Arc<rustls::ServerConfig>, Vec<u8>) {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let certified =
        rcgen::generate_simple_self_signed(vec!["localhost".to_string()]).expect("self-signed");
    let cert_der = certified.cert.der().to_vec();
    let key_der = certified.key_pair.serialize_der();
    let cert = rustls::pki_types::CertificateDer::from(cert_der.clone());
    let key = rustls::pki_types::PrivateKeyDer::Pkcs8(rustls::pki_types::PrivatePkcs8KeyDer::from(
        key_der,
    ));
    let mut config = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(vec![cert], key)
        .expect("server config");
    config.alpn_protocols = server_alpn;
    (Arc::new(config), cert_der)
}

/// Spawn a one-shot TLS+HTTP/2 server. After the TLS handshake, if
/// `speak_http2` is set it reads the client preface, exchanges SETTINGS,
/// reads one HEADERS, and replies 200 + body. Returns (addr, cert_der,
/// join handle).
fn spawn_tls_h2_server(
    server_alpn: Vec<Vec<u8>>,
    speak_http2: bool,
    body: &'static [u8],
) -> (SocketAddr, Vec<u8>, std::thread::JoinHandle<()>) {
    let (config, cert_der) = server_config(server_alpn);
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind tls listener");
    let addr = listener.local_addr().expect("addr");
    let handle = std::thread::spawn(move || {
        let (mut tcp, _) = listener.accept().expect("accept");
        let mut conn = rustls::ServerConnection::new(config).expect("server conn");
        // Drive the TLS handshake to completion.
        while conn.is_handshaking() {
            if conn.complete_io(&mut tcp).is_err() {
                return;
            }
        }
        if !speak_http2 {
            // ALPN-mismatch case: the client fails at connect; nothing to
            // do but hold briefly so the client observes the closed ALPN.
            std::thread::sleep(Duration::from_millis(50));
            return;
        }
        let mut stream = rustls::Stream::new(&mut conn, &mut tcp);
        // Client preface.
        let mut preface = [0_u8; CLIENT_PREFACE.len()];
        if stream.read_exact(&mut preface).is_err() {
            return;
        }
        assert_eq!(&preface, CLIENT_PREFACE, "client preface over TLS");
        write_frame(&mut stream, FRAME_SETTINGS, 0, 0, &[]);
        // Read frames until the request HEADERS arrives, ACKing settings.
        let request_stream = loop {
            let frame = match read_frame(&mut stream) {
                Ok(frame) => frame,
                Err(_) => return,
            };
            match frame.ty {
                FRAME_SETTINGS if frame.flags & FLAG_ACK == 0 => {
                    write_frame(&mut stream, FRAME_SETTINGS, FLAG_ACK, 0, &[]);
                }
                FRAME_HEADERS => break frame.stream_id,
                _ => {}
            }
        };
        // Reply: 200 + content-length + body, ending the stream.
        let mut block = Vec::new();
        literal_header(":status", "200", &mut block);
        literal_header("content-length", &body.len().to_string(), &mut block);
        write_frame(
            &mut stream,
            FRAME_HEADERS,
            FLAG_END_HEADERS,
            request_stream,
            &block,
        );
        write_frame(
            &mut stream,
            FRAME_DATA,
            FLAG_END_STREAM,
            request_stream,
            body,
        );
        std::thread::sleep(Duration::from_millis(50));
    });
    (addr, cert_der, handle)
}

fn runtime() -> ThreadedRuntime<TestShard, DefaultThreadedMailboxFactory> {
    ThreadedRuntime::with_config(
        TestShard,
        DefaultThreadedMailboxFactory,
        ThreadedRuntimeConfig {
            command_capacity: 64,
            idle_wait: Duration::from_millis(1),
            ..Default::default()
        },
    )
}

fn make_client(
    runtime: &ThreadedRuntime<TestShard, DefaultThreadedMailboxFactory>,
    target: Http2Target,
) -> Address<Http2ClientMsg, Http2ClientReply> {
    let client = make_client_unstarted(runtime, target);
    runtime
        .try_send(client, Http2ClientMsg::Begin)
        .expect("begin");
    client
}

fn make_client_unstarted(
    runtime: &ThreadedRuntime<TestShard, DefaultThreadedMailboxFactory>,
    target: Http2Target,
) -> Address<Http2ClientMsg, Http2ClientReply> {
    runtime
        .register_with_capacity::<Http2ClientConnection<TestShard>, _>(
            Http2ClientConnection::<TestShard>::new(target, Http2ClientLimits::default()),
            32,
        )
        .expect("register client")
}

fn submit_with_request_queued_before_begin(
    runtime: &ThreadedRuntime<TestShard, DefaultThreadedMailboxFactory>,
    client: Address<Http2ClientMsg, Http2ClientReply>,
    request: Http2ClientRequest,
) -> CallOutcome<Http2ClientReply> {
    std::thread::scope(|scope| {
        let waiter = scope.spawn(|| {
            runtime
                .call_blocking(
                    client,
                    Http2ClientMsg::Submit(request),
                    Duration::from_secs(5),
                )
                .expect("call returns")
        });

        // Give the runtime a turn to admit and park the Submit before
        // Begin can fail fast. The thing under test is "queued caller
        // gets typed TLS failure"; without this ordering, a fast connect
        // failure can close the isolate before the call is admitted, and
        // CallOutcome::Closed is also honest.
        std::thread::sleep(Duration::from_millis(50));
        runtime
            .try_send(client, Http2ClientMsg::Begin)
            .expect("begin");
        waiter.join().expect("submit thread")
    })
}

#[test]
fn h2_over_tls_round_trips_with_alpn_h2() {
    let (addr, cert_der, server) = spawn_tls_h2_server(vec![b"h2".to_vec()], true, b"tls-ok");
    let runtime = runtime();
    let target = Http2Target::Tls {
        authority: "localhost".into(),
        addr,
        server_name: "localhost".into(),
        trust_roots: vec![cert_der],
        alpn: AlpnProtocols::h2(),
    };
    let client = make_client(&runtime, target);

    let outcome = runtime
        .call_blocking(
            client,
            Http2ClientMsg::Submit(Http2ClientRequest::get("/secure")),
            Duration::from_secs(5),
        )
        .expect("call returns");
    match outcome {
        CallOutcome::Replied(Http2ClientReply::Outcome {
            outcome: Http2ClientOutcome::Replied(response),
            ..
        }) => {
            assert_eq!(response.status.as_u16(), 200);
            assert_eq!(response.body, b"tls-ok");
        }
        other => panic!("expected Replied over TLS, got {other:?}"),
    }

    let _ = runtime.try_send(client, Http2ClientMsg::Stop);
    let _ = runtime.shutdown();
    server.join().expect("server thread");
}

#[test]
fn h2_over_tls_without_server_h2_alpn_is_typed_mismatch() {
    // Server advertises no ALPN; client offers h2. The TLS rail fails the
    // connect with TlsAlpnMismatch before any HTTP/2 framing, and the
    // client surfaces the typed outcome — not a silent downgrade.
    let (addr, cert_der, server) = spawn_tls_h2_server(Vec::new(), false, b"");
    let runtime = runtime();
    let target = Http2Target::Tls {
        authority: "localhost".into(),
        addr,
        server_name: "localhost".into(),
        trust_roots: vec![cert_der],
        alpn: AlpnProtocols::h2(),
    };
    let client = make_client_unstarted(&runtime, target);

    let outcome = submit_with_request_queued_before_begin(
        &runtime,
        client,
        Http2ClientRequest::get("/secure"),
    );
    match outcome {
        CallOutcome::Replied(Http2ClientReply::Outcome {
            outcome: Http2ClientOutcome::TlsAlpnMismatch,
            ..
        }) => {}
        other => panic!("expected TlsAlpnMismatch, got {other:?}"),
    }

    let _ = runtime.try_send(client, Http2ClientMsg::Stop);
    let _ = runtime.shutdown();
    let _ = server.join();
}

#[test]
fn h2_over_tls_with_untrusted_cert_fails_without_panic() {
    // A server cert the client does not trust must fail the connect
    // (mapped to Closed at the client level; the precise TlsCertificate
    // reason lives on the rail/trace). Never a panic.
    let (addr, _cert_der, server) = spawn_tls_h2_server(vec![b"h2".to_vec()], false, b"");
    let runtime = runtime();
    let target = Http2Target::Tls {
        authority: "localhost".into(),
        addr,
        server_name: "localhost".into(),
        // Wrong trust root: a different self-signed cert.
        trust_roots: vec![server_config(Vec::new()).1],
        alpn: AlpnProtocols::h2(),
    };
    let client = make_client_unstarted(&runtime, target);

    let outcome = submit_with_request_queued_before_begin(
        &runtime,
        client,
        Http2ClientRequest::get("/secure"),
    );
    match outcome {
        CallOutcome::Replied(Http2ClientReply::Outcome { outcome, .. }) => {
            assert!(
                matches!(
                    outcome,
                    Http2ClientOutcome::Closed | Http2ClientOutcome::TlsAlpnMismatch
                ),
                "cert failure should be a typed non-Replied outcome, got {outcome:?}",
            );
        }
        other => panic!("expected Outcome, got {other:?}"),
    }

    let _ = runtime.try_send(client, Http2ClientMsg::Stop);
    let _ = runtime.shutdown();
    let _ = server.join();
}
