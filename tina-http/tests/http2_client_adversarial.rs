//! Adversarial / concurrency / interop live proofs for the native
//! HTTP/2 client. These tests stand up a *hand-rolled* HTTP/2 server
//! peer on a raw `TcpStream` (independent of the client's framing code)
//! and dial it from the real `Http2ClientConnection`. They pin the
//! paths a well-behaved in-tree Tina server never exercises:
//!
//! - server `RST_STREAM` mid-stream → client `Reset(reason)` + an
//!   inbound `Http2StreamReset` protocol fact
//! - server `RST_STREAM` on stream 0 → connection-level protocol error
//! - `GOAWAY(last_stream_id = 0)` → refuse the unprocessed stream
//!   (`Closed`, retryable); `GOAWAY(last_stream_id >= in-flight)` →
//!   let the admitted stream settle but block new admission
//! - malformed inbound frame → typed error, never a panic
//! - foreign-server happy path → `Replied` (interop, independent framing)
//! - concurrent streams do not cross replies
//! - peer `MAX_CONCURRENT_STREAMS` cap → excess submit is `Full`
//! - caller `Cancel` → `LocalCancel`, connection survives
//! - 128 KB upload paces through real `WINDOW_UPDATE` round trips
//! - outbound open/close lifecycle protocol facts are emitted
//!
//! The peer runs on its own thread. The client runs in a Tina runtime.

mod common;

use std::convert::Infallible;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::mpsc;
use std::time::Duration;

use common::TestShard;
use http::{HeaderMap, Method};
use tina::prelude::*;
use tina_http::{
    GrpcClient, GrpcLimits, Http2ClientConnection, Http2ClientLimits, Http2ClientMsg,
    Http2ClientOutcome, Http2ClientReply, Http2ClientRequest, Http2ClientRequestBody,
    Http2ClientStreamCall, Http2ProtocolError, Http2ResponseChunk, Http2Target,
};
use tina_runtime::{
    CallOutcome, DefaultThreadedMailboxFactory, Http2ResetReason, ProtocolDirection, ProtocolFact,
    RuntimeEventKind, RuntimeFact, ThreadedRuntime, ThreadedRuntimeConfig,
};

/// Drain the runtime trace and return the protocol facts the client
/// emitted. Mirrors the websocket live-test helper.
fn protocol_facts(
    runtime: &ThreadedRuntime<TestShard, DefaultThreadedMailboxFactory>,
) -> Vec<ProtocolFact> {
    runtime
        .complete_trace()
        .expect("complete trace")
        .into_iter()
        .filter_map(|event| match event.kind() {
            RuntimeEventKind::FactObserved {
                fact: RuntimeFact::Protocol(protocol),
            } => Some(protocol),
            _ => None,
        })
        .collect()
}

const CLIENT_PREFACE: &[u8] = b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n";
const FRAME_HEADERS: u8 = 0x1;
const FRAME_RST_STREAM: u8 = 0x3;
const FRAME_SETTINGS: u8 = 0x4;
const FRAME_GOAWAY: u8 = 0x7;
const FLAG_ACK: u8 = 0x1;
const ERR_REFUSED_STREAM: u32 = 0x7;
const ERR_NO_ERROR: u32 = 0x0;

#[derive(Clone, PartialEq, prost::Message)]
struct QueuedGrpcRequest {}

#[derive(Debug)]
struct RawFrame {
    ty: u8,
    flags: u8,
    stream_id: u32,
    #[allow(dead_code)]
    payload: Vec<u8>,
}

fn write_frame(stream: &mut TcpStream, ty: u8, flags: u8, stream_id: u32, payload: &[u8]) {
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

fn read_frame(stream: &mut TcpStream) -> std::io::Result<RawFrame> {
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

fn encode_str(s: &str, out: &mut Vec<u8>) {
    encode_int(s.len(), 7, 0, out);
    out.extend_from_slice(s.as_bytes());
}

fn literal_header(name: &str, value: &str, out: &mut Vec<u8>) {
    out.push(0); // literal header, never indexed, new name
    encode_str(name, out);
    encode_str(value, out);
}

const FRAME_DATA: u8 = 0x0;
const FRAME_WINDOW_UPDATE: u8 = 0x8;
const FLAG_END_STREAM: u8 = 0x1;
const FLAG_END_HEADERS: u8 = 0x4;
const SETTINGS_MAX_CONCURRENT_STREAMS: u16 = 0x3;

fn write_settings(stream: &mut TcpStream, settings: &[(u16, u32)]) {
    let mut payload = Vec::with_capacity(settings.len() * 6);
    for (id, value) in settings {
        payload.extend_from_slice(&id.to_be_bytes());
        payload.extend_from_slice(&value.to_be_bytes());
    }
    write_frame(stream, FRAME_SETTINGS, 0, 0, &payload);
}

fn write_window_update(stream: &mut TcpStream, stream_id: u32, increment: u32) {
    write_frame(
        stream,
        FRAME_WINDOW_UPDATE,
        0,
        stream_id,
        &(increment & 0x7fff_ffff).to_be_bytes(),
    );
}

/// Send a complete `:status` response: HEADERS (+ content-length) and,
/// if `body` is non-empty, one DATA frame, both ending the stream.
fn send_response(stream: &mut TcpStream, stream_id: u32, status: &str, body: &[u8]) {
    let mut block = Vec::new();
    literal_header(":status", status, &mut block);
    literal_header("content-length", &body.len().to_string(), &mut block);
    let header_flags = FLAG_END_HEADERS | if body.is_empty() { FLAG_END_STREAM } else { 0 };
    write_frame(stream, FRAME_HEADERS, header_flags, stream_id, &block);
    if !body.is_empty() {
        write_frame(stream, FRAME_DATA, FLAG_END_STREAM, stream_id, body);
    }
}

/// Read preface and exchange SETTINGS, advertising `settings` to the
/// client. Does not consume the client's HEADERS — the caller reads
/// frames afterward.
fn complete_handshake_with(stream: &mut TcpStream, settings: &[(u16, u32)]) {
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .expect("read timeout");
    let mut preface = [0_u8; CLIENT_PREFACE.len()];
    stream.read_exact(&mut preface).expect("read preface");
    assert_eq!(&preface, CLIENT_PREFACE, "client preface");
    write_settings(stream, settings);
    loop {
        let frame = read_frame(stream).expect("frame during handshake");
        if frame.ty == FRAME_SETTINGS && frame.flags & FLAG_ACK == 0 {
            write_frame(stream, FRAME_SETTINGS, FLAG_ACK, 0, &[]);
            return;
        }
    }
}

/// Read the next HEADERS frame, skipping SETTINGS (incl. our-settings
/// ACK), PING, and WINDOW_UPDATE frames. Returns the stream id.
fn next_headers(stream: &mut TcpStream) -> u32 {
    loop {
        let frame = read_frame(stream).expect("frame before headers");
        if frame.ty == FRAME_HEADERS {
            return frame.stream_id;
        }
    }
}

/// Read the client preface, exchange SETTINGS, and read frames until the
/// client's first HEADERS arrives. Returns the opened stream id.
fn accept_until_headers(stream: &mut TcpStream) -> u32 {
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .expect("read timeout");
    let mut preface = [0_u8; CLIENT_PREFACE.len()];
    stream.read_exact(&mut preface).expect("read preface");
    assert_eq!(&preface, CLIENT_PREFACE, "client preface");
    // Our SETTINGS (empty) + ACK of the client's SETTINGS once we see it.
    write_frame(stream, FRAME_SETTINGS, 0, 0, &[]);
    loop {
        let frame = read_frame(stream).expect("frame before headers");
        match frame.ty {
            FRAME_SETTINGS if frame.flags & FLAG_ACK == 0 => {
                write_frame(stream, FRAME_SETTINGS, FLAG_ACK, 0, &[]);
            }
            FRAME_SETTINGS => {}
            FRAME_HEADERS => return frame.stream_id,
            _ => {}
        }
    }
}

/// Spawn a one-shot raw HTTP/2 peer. The closure runs after the first
/// client HEADERS has been read, receiving the live socket and the
/// opened stream id, so it can script misbehavior. Returns the bound
/// address and a join handle.
fn spawn_peer<F>(behavior: F) -> (std::net::SocketAddr, std::thread::JoinHandle<()>)
where
    F: FnOnce(&mut TcpStream, u32) + Send + 'static,
{
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind peer");
    let addr = listener.local_addr().expect("peer addr");
    let handle = std::thread::spawn(move || {
        let (mut sock, _) = listener.accept().expect("accept client");
        let stream_id = accept_until_headers(&mut sock);
        behavior(&mut sock, stream_id);
        // Keep the socket alive briefly so the client can read our last
        // frame before the OS drops it.
        std::thread::sleep(Duration::from_millis(50));
    });
    (addr, handle)
}

fn run_client(
    addr: std::net::SocketAddr,
) -> (
    ThreadedRuntime<TestShard, DefaultThreadedMailboxFactory>,
    Address<Http2ClientMsg, Http2ClientReply>,
) {
    let runtime = ThreadedRuntime::with_config(
        TestShard,
        DefaultThreadedMailboxFactory,
        ThreadedRuntimeConfig {
            command_capacity: 64,
            idle_wait: Duration::from_millis(1),
            ..Default::default()
        },
    );
    let target = Http2Target::H2c {
        authority: "peer".into(),
        addr,
    };
    let client = runtime
        .register_with_capacity::<Http2ClientConnection<TestShard>, _>(
            Http2ClientConnection::<TestShard>::new(target, Http2ClientLimits::default())
                .expect("default HTTP/2 client limits are valid"),
            32,
        )
        .expect("register client");
    let _: Result<(), Infallible> = Ok(());
    runtime
        .try_send(client, Http2ClientMsg::Begin)
        .expect("begin");
    (runtime, client)
}

#[test]
fn client_uses_default_peer_frame_cap_before_server_settings() {
    const BODY_LEN: usize = 40_000;
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind raw peer");
    let addr = listener.local_addr().expect("peer addr");
    let peer = std::thread::spawn(move || {
        let (mut sock, _) = listener.accept().expect("accept client");
        sock.set_read_timeout(Some(Duration::from_secs(5)))
            .expect("read timeout");
        let mut preface = [0_u8; CLIENT_PREFACE.len()];
        sock.read_exact(&mut preface).expect("read client preface");
        assert_eq!(&preface, CLIENT_PREFACE);

        let mut stream_id = None;
        let mut body_bytes = 0;
        loop {
            let frame = read_frame(&mut sock).expect("request frame before server SETTINGS");
            match frame.ty {
                FRAME_SETTINGS => {}
                FRAME_HEADERS => stream_id = Some(frame.stream_id),
                FRAME_DATA => {
                    assert!(
                        frame.payload.len() <= 16 * 1024,
                        "client used its local 64 KiB receive cap as the peer's outbound cap"
                    );
                    body_bytes += frame.payload.len();
                    if frame.flags & FLAG_END_STREAM != 0 {
                        break;
                    }
                }
                other => panic!("unexpected pre-SETTINGS client frame {other}: {frame:?}"),
            }
        }
        assert_eq!(body_bytes, BODY_LEN);
        let stream_id = stream_id.expect("request HEADERS observed");

        write_frame(&mut sock, FRAME_SETTINGS, 0, 0, &[]);
        write_frame(&mut sock, FRAME_SETTINGS, FLAG_ACK, 0, &[]);
        send_response(&mut sock, stream_id, "200", b"ok");
    });

    let runtime = ThreadedRuntime::with_config(
        TestShard,
        DefaultThreadedMailboxFactory,
        ThreadedRuntimeConfig {
            command_capacity: 64,
            idle_wait: Duration::from_millis(1),
            ..Default::default()
        },
    );
    let target = Http2Target::H2c {
        authority: "peer".into(),
        addr,
    };
    let client = runtime
        .register_with_capacity::<Http2ClientConnection<TestShard>, _>(
            Http2ClientConnection::<TestShard>::new(
                target,
                Http2ClientLimits {
                    max_frame_size: 64 * 1024,
                    ..Http2ClientLimits::default()
                },
            )
            .expect("valid HTTP/2 client limits"),
            32,
        )
        .expect("register client");
    runtime.try_send(client, Http2ClientMsg::Begin).unwrap();
    let outcome = runtime
        .call_blocking(
            client,
            Http2ClientMsg::Submit(Http2ClientRequest::post("/upload", vec![b'x'; BODY_LEN])),
            Duration::from_secs(5),
        )
        .expect("client request returns");
    assert!(matches!(
        outcome,
        CallOutcome::Replied(Http2ClientReply::Outcome {
            outcome: Http2ClientOutcome::Replied(_),
            ..
        })
    ));
    let _ = runtime.try_send(client, Http2ClientMsg::Stop);
    let _ = runtime.shutdown();
    peer.join().expect("raw peer joins");
}

#[test]
fn server_rst_stream_maps_to_typed_reset() {
    // Peer accepts the request, then sends RST_STREAM(REFUSED_STREAM)
    // on the client's stream. The client must surface
    // `Http2ClientOutcome::Reset(RefusedStream)`.
    let (addr, peer) = spawn_peer(|sock, stream_id| {
        write_frame(
            sock,
            FRAME_RST_STREAM,
            0,
            stream_id,
            &ERR_REFUSED_STREAM.to_be_bytes(),
        );
    });
    let (runtime, client) = run_client(addr);

    let outcome = runtime
        .call_blocking(
            client,
            Http2ClientMsg::Submit(Http2ClientRequest::get("/x")),
            Duration::from_secs(5),
        )
        .expect("call returns outcome");
    match outcome {
        CallOutcome::Replied(Http2ClientReply::Outcome {
            outcome: Http2ClientOutcome::Reset(Http2ResetReason::RefusedStream),
            ..
        }) => {}
        other => panic!("expected Reset(RefusedStream), got {other:?}"),
    }

    let _ = runtime.try_send(client, Http2ClientMsg::Stop);
    let _ = runtime.shutdown();
    peer.join().expect("peer thread joins");
}

#[test]
fn streamed_response_peer_rst_delivers_reset_to_parked_pull() {
    // OpenStream → response head (no END_STREAM) → the caller pulls and
    // parks (no body buffered yet). The peer then RST_STREAMs. The
    // terminal `Reset` must reach the *parked pull* as a
    // `ResponseChunk::Reset`, not just the (already-consumed) head waiter.
    let (addr, peer) = spawn_peer(|sock, stream_id| {
        // Response head only — the body is "streaming".
        let mut block = Vec::new();
        literal_header(":status", "200", &mut block);
        write_frame(sock, FRAME_HEADERS, FLAG_END_HEADERS, stream_id, &block);
        // Give the client time to deliver the head and park its first
        // pull before we reset, so the reset lands on the parked pull.
        std::thread::sleep(Duration::from_millis(150));
        write_frame(
            sock,
            FRAME_RST_STREAM,
            0,
            stream_id,
            &ERR_REFUSED_STREAM.to_be_bytes(),
        );
    });
    let (runtime, client) = run_client(addr);

    let head = runtime
        .call_blocking(
            client,
            Http2ClientMsg::OpenStream(Http2ClientStreamCall {
                method: Method::GET,
                path: "/x".into(),
                headers: HeaderMap::new(),
                body: Http2ClientRequestBody::Buffered(Vec::new()),
            }),
            Duration::from_secs(5),
        )
        .expect("open returns");
    let stream_id = match head {
        CallOutcome::Replied(Http2ClientReply::Outcome {
            stream_id,
            outcome: Http2ClientOutcome::ResponseStreaming { status, .. },
        }) => {
            assert_eq!(status.as_u16(), 200);
            stream_id
        }
        other => panic!("expected ResponseStreaming head, got {other:?}"),
    };

    let chunk = runtime
        .call_blocking(
            client,
            Http2ClientMsg::ResponseNext { stream_id },
            Duration::from_secs(5),
        )
        .expect("pull returns");
    match chunk {
        CallOutcome::Replied(Http2ClientReply::ResponseChunk {
            chunk: Http2ResponseChunk::Reset(Http2ResetReason::RefusedStream),
            ..
        }) => {}
        other => panic!("expected ResponseChunk::Reset(RefusedStream), got {other:?}"),
    }

    let _ = runtime.try_send(client, Http2ClientMsg::Stop);
    let _ = runtime.shutdown();
    peer.join().expect("peer thread joins");
}

/// Drive a streamed-response stream whose head declares `content-length:
/// declared` but whose body, ending with END_STREAM, is `body`. Returns the
/// terminal chunk the parked pull receives after draining any DATA.
fn streamed_content_length_terminal(declared: usize, body: Vec<u8>) -> Http2ResponseChunk {
    let (addr, peer) = spawn_peer(move |sock, stream_id| {
        // Streamed head: status + a content-length the body will violate.
        let mut block = Vec::new();
        literal_header(":status", "200", &mut block);
        literal_header("content-length", &declared.to_string(), &mut block);
        write_frame(sock, FRAME_HEADERS, FLAG_END_HEADERS, stream_id, &block);
        // The whole (mis-sized) body in one DATA frame ending the stream.
        write_frame(sock, FRAME_DATA, FLAG_END_STREAM, stream_id, &body);
    });
    let (runtime, client) = run_client(addr);

    let head = runtime
        .call_blocking(
            client,
            Http2ClientMsg::OpenStream(Http2ClientStreamCall {
                method: Method::GET,
                path: "/x".into(),
                headers: HeaderMap::new(),
                body: Http2ClientRequestBody::Buffered(Vec::new()),
            }),
            Duration::from_secs(5),
        )
        .expect("open returns");
    let stream_id = match head {
        CallOutcome::Replied(Http2ClientReply::Outcome {
            stream_id,
            outcome: Http2ClientOutcome::ResponseStreaming { .. },
        }) => stream_id,
        other => panic!("expected ResponseStreaming head, got {other:?}"),
    };

    // Pull until a terminal chunk (End / ProtocolError / Reset / Closed);
    // skip any Data chunks the short/over body delivered first.
    let terminal = loop {
        let chunk = runtime
            .call_blocking(
                client,
                Http2ClientMsg::ResponseNext { stream_id },
                Duration::from_secs(5),
            )
            .expect("pull returns");
        match chunk {
            CallOutcome::Replied(Http2ClientReply::ResponseChunk {
                chunk: Http2ResponseChunk::Data(_),
                ..
            }) => continue,
            CallOutcome::Replied(Http2ClientReply::ResponseChunk { chunk, .. }) => break chunk,
            other => panic!("expected ResponseChunk, got {other:?}"),
        }
    };

    let _ = runtime.try_send(client, Http2ClientMsg::Stop);
    let _ = runtime.shutdown();
    peer.join().expect("peer thread joins");
    terminal
}

#[test]
fn streamed_response_short_body_vs_content_length_is_protocol_error() {
    // Head declares content-length: 10, body sends 4 bytes + END_STREAM.
    // The streamed path must NOT hand the caller a clean `End`; a declared
    // length that the body violates is a malformed response (RFC 9113
    // §8.1.1), exactly as the buffered path already enforces.
    let terminal = streamed_content_length_terminal(10, b"abcd".to_vec());
    match terminal {
        Http2ResponseChunk::ProtocolError(Http2ProtocolError::ContentLengthMismatch)
        | Http2ResponseChunk::Reset(_) => {}
        other => panic!("expected ContentLengthMismatch / Reset, got {other:?}"),
    }
}

#[test]
fn streamed_response_over_body_vs_content_length_is_protocol_error() {
    // Head declares content-length: 4, body sends 6 bytes + END_STREAM.
    let terminal = streamed_content_length_terminal(4, b"abcdef".to_vec());
    match terminal {
        Http2ResponseChunk::ProtocolError(Http2ProtocolError::ContentLengthMismatch)
        | Http2ResponseChunk::Reset(_) => {}
        other => panic!("expected ContentLengthMismatch / Reset, got {other:?}"),
    }
}

#[test]
fn server_rst_stream_on_stream_zero_is_connection_protocol_error() {
    // RFC 9113 §6.4: RST_STREAM on stream 0x0 is a connection-level
    // PROTOCOL_ERROR. The client must NOT silently ignore it — it
    // should fail the in-flight stream with a typed protocol error and
    // tear the connection down. (Before the fix, this frame was a
    // silent no-op.)
    let (addr, peer) = spawn_peer(|sock, _stream_id| {
        write_frame(sock, FRAME_RST_STREAM, 0, 0, &ERR_NO_ERROR.to_be_bytes());
    });
    let (runtime, client) = run_client(addr);

    let outcome = runtime
        .call_blocking(
            client,
            Http2ClientMsg::Submit(Http2ClientRequest::get("/x")),
            Duration::from_secs(5),
        )
        .expect("call returns outcome");
    match outcome {
        CallOutcome::Replied(Http2ClientReply::Outcome {
            outcome: Http2ClientOutcome::ProtocolError(Http2ProtocolError::BadStreamId),
            ..
        }) => {}
        other => panic!("expected ProtocolError(BadStreamId), got {other:?}"),
    }

    let _ = runtime.try_send(client, Http2ClientMsg::Stop);
    let _ = runtime.shutdown();
    peer.join().expect("peer thread joins");
}

#[test]
fn server_goaway_below_stream_id_refuses_unprocessed_stream() {
    // Peer sends GOAWAY(last_stream_id = 0, NO_ERROR) immediately after
    // the client's HEADERS — i.e., "I processed nothing." The client's
    // stream id is 1 > 0, so it was NOT processed and must be refused
    // with `Closed` so the caller knows it can safely retry. Before the
    // fix, the GOAWAY payload was ignored and the stream hung until the
    // socket dropped.
    let (addr, peer) = spawn_peer(|sock, _stream_id| {
        let mut payload = Vec::with_capacity(8);
        payload.extend_from_slice(&0_u32.to_be_bytes()); // last_stream_id = 0
        payload.extend_from_slice(&ERR_NO_ERROR.to_be_bytes());
        write_frame(sock, FRAME_GOAWAY, 0, 0, &payload);
    });
    let (runtime, client) = run_client(addr);

    let outcome = runtime
        .call_blocking(
            client,
            Http2ClientMsg::Submit(Http2ClientRequest::get("/x")),
            Duration::from_secs(5),
        )
        .expect("call returns outcome");
    match outcome {
        CallOutcome::Replied(Http2ClientReply::Outcome {
            outcome: Http2ClientOutcome::Closed,
            ..
        }) => {}
        other => panic!("expected Closed (refused, retryable), got {other:?}"),
    }

    let _ = runtime.try_send(client, Http2ClientMsg::Stop);
    let _ = runtime.shutdown();
    peer.join().expect("peer thread joins");
}

#[test]
fn malformed_inbound_frame_does_not_panic_and_fails_stream_typed() {
    // Peer sends a RST_STREAM with a 3-byte payload (must be 4). The
    // client maps this to a typed `BadFrameLength` connection error and
    // fails the in-flight stream — never a panic. We confirm via a
    // channel that the test thread observed a typed outcome at all.
    let (tx, rx) = mpsc::channel();
    let (addr, peer) = spawn_peer(move |sock, stream_id| {
        // 3-byte RST_STREAM payload — illegal length.
        write_frame(sock, FRAME_RST_STREAM, 0, stream_id, &[0, 0, 0]);
        let _ = tx.send(());
    });
    let (runtime, client) = run_client(addr);

    let outcome = runtime
        .call_blocking(
            client,
            Http2ClientMsg::Submit(Http2ClientRequest::get("/x")),
            Duration::from_secs(5),
        )
        .expect("call returns outcome");
    match outcome {
        CallOutcome::Replied(Http2ClientReply::Outcome {
            outcome: Http2ClientOutcome::ProtocolError(Http2ProtocolError::BadFrameLength),
            ..
        }) => {}
        other => panic!("expected ProtocolError(BadFrameLength), got {other:?}"),
    }
    rx.recv_timeout(Duration::from_secs(2))
        .expect("peer sent the malformed frame");

    let _ = runtime.try_send(client, Http2ClientMsg::Stop);
    let _ = runtime.shutdown();
    peer.join().expect("peer thread joins");
}

#[test]
fn foreign_server_happy_path_get_returns_replied() {
    // Interop proof against a non-Tina HTTP/2 server: the hand-rolled
    // peer (independent of the client's framing code) sends a proper
    // 200 + body. Proves the client does not depend on Tina-server
    // framing quirks.
    let (addr, peer) = spawn_peer(|sock, stream_id| {
        send_response(sock, stream_id, "200", b"foreign-ok");
    });
    let (runtime, client) = run_client(addr);

    let outcome = runtime
        .call_blocking(
            client,
            Http2ClientMsg::Submit(Http2ClientRequest::get("/x")),
            Duration::from_secs(5),
        )
        .expect("call returns outcome");
    match outcome {
        CallOutcome::Replied(Http2ClientReply::Outcome {
            outcome: Http2ClientOutcome::Replied(response),
            ..
        }) => {
            assert_eq!(response.status.as_u16(), 200);
            assert_eq!(response.body, b"foreign-ok");
        }
        other => panic!("expected Replied, got {other:?}"),
    }

    let _ = runtime.try_send(client, Http2ClientMsg::Stop);
    let _ = runtime.shutdown();
    peer.join().expect("peer thread joins");
}

#[test]
fn concurrent_streams_do_not_cross_replies() {
    // Two requests are submitted concurrently (separate host threads).
    // The peer reads both HEADERS, then replies in REVERSE order, with
    // each response body tagged by the stream id it answers
    // (`stream-<id>`). Each caller must receive the body that matches
    // the stream id in its own outcome — proof that concurrent streams
    // on one connection do not cross replies.
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind peer");
    let addr = listener.local_addr().expect("peer addr");
    let peer = std::thread::spawn(move || {
        let (mut sock, _) = listener.accept().expect("accept");
        complete_handshake_with(&mut sock, &[]);
        let first = next_headers(&mut sock);
        let second = next_headers(&mut sock);
        assert_ne!(first, second, "two distinct client stream ids");
        // Respond out of admission order to stress reply routing.
        send_response(
            &mut sock,
            second,
            "200",
            format!("stream-{second}").as_bytes(),
        );
        send_response(
            &mut sock,
            first,
            "200",
            format!("stream-{first}").as_bytes(),
        );
        std::thread::sleep(Duration::from_millis(50));
    });
    let (runtime, client) = run_client(addr);

    std::thread::scope(|scope| {
        let runtime = &runtime;
        let a = scope.spawn(move || {
            runtime
                .call_blocking(
                    client,
                    Http2ClientMsg::Submit(Http2ClientRequest::get("/a")),
                    Duration::from_secs(5),
                )
                .expect("call a")
        });
        let b = scope.spawn(move || {
            runtime
                .call_blocking(
                    client,
                    Http2ClientMsg::Submit(Http2ClientRequest::get("/b")),
                    Duration::from_secs(5),
                )
                .expect("call b")
        });
        for outcome in [a.join().unwrap(), b.join().unwrap()] {
            match outcome {
                CallOutcome::Replied(Http2ClientReply::Outcome {
                    stream_id,
                    outcome: Http2ClientOutcome::Replied(response),
                }) => {
                    assert_eq!(
                        response.body,
                        format!("stream-{stream_id}").into_bytes(),
                        "reply for stream {stream_id} carried another stream's body",
                    );
                }
                other => panic!("expected Replied, got {other:?}"),
            }
        }
    });

    let _ = runtime.try_send(client, Http2ClientMsg::Stop);
    let _ = runtime.shutdown();
    peer.join().expect("peer thread joins");
}

#[test]
fn peer_max_concurrent_streams_one_yields_full_for_the_excess_submit() {
    // The peer advertises SETTINGS_MAX_CONCURRENT_STREAMS = 1 and holds
    // the first stream open briefly before answering it. Two concurrent
    // submits therefore cannot both be admitted: exactly one is admitted
    // and later `Replied`, the other is rejected with
    // `Http2ClientOutcome::Full`. Proves both the `Full` admission
    // outcome and that the client honors the peer's concurrency cap.
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind peer");
    let addr = listener.local_addr().expect("peer addr");
    let (first_seen_tx, first_seen_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let peer = std::thread::spawn(move || {
        let (mut sock, _) = listener.accept().expect("accept");
        complete_handshake_with(&mut sock, &[(SETTINGS_MAX_CONCURRENT_STREAMS, 1)]);
        // Only one stream should ever reach the wire (the other is
        // refused client-side as Full). Read it, then hold it open until
        // the test has driven the second submit. This is deliberately
        // channel-driven instead of sleep-driven: CI runners vary enough
        // that a fixed sleep can turn this proof into "peer closed first".
        let only = next_headers(&mut sock);
        first_seen_tx.send(()).expect("signal first stream seen");
        release_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("test releases first stream");
        send_response(&mut sock, only, "200", b"ok");
        std::thread::sleep(Duration::from_millis(50));
    });
    let (runtime, client) = run_client(addr);

    let outcomes = std::thread::scope(|scope| {
        let runtime = &runtime;
        let a = scope.spawn(move || {
            runtime
                .call_blocking(
                    client,
                    Http2ClientMsg::Submit(Http2ClientRequest::get("/a")),
                    Duration::from_secs(5),
                )
                .expect("call a")
        });
        first_seen_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("first stream reaches peer and occupies the cap");
        let b = scope.spawn(move || {
            runtime
                .call_blocking(
                    client,
                    Http2ClientMsg::Submit(Http2ClientRequest::get("/b")),
                    Duration::from_secs(5),
                )
                .expect("call b")
        });
        let second = b.join().unwrap();
        release_tx.send(()).expect("release first stream");
        (a.join().unwrap(), second)
    });

    let mut saw_full = 0;
    let mut saw_replied = 0;
    for outcome in [outcomes.0, outcomes.1] {
        match outcome {
            CallOutcome::Replied(Http2ClientReply::Outcome { outcome, .. }) => match outcome {
                Http2ClientOutcome::Full => saw_full += 1,
                Http2ClientOutcome::Replied(_) => saw_replied += 1,
                other => panic!("unexpected outcome {other:?}"),
            },
            other => panic!("expected Outcome, got {other:?}"),
        }
    }
    assert_eq!(saw_full, 1, "exactly one submit must be rejected Full");
    assert_eq!(
        saw_replied, 1,
        "exactly one submit must be admitted+replied"
    );

    let _ = runtime.try_send(client, Http2ClientMsg::Stop);
    let _ = runtime.shutdown();
    peer.join().expect("peer thread joins");
}

#[test]
fn abandoned_streamed_response_is_cancelled_and_slot_reused() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind peer");
    let addr = listener.local_addr().expect("peer addr");
    let peer = std::thread::spawn(move || {
        let (mut sock, _) = listener.accept().expect("accept");
        complete_handshake_with(&mut sock, &[(SETTINGS_MAX_CONCURRENT_STREAMS, 1)]);

        let first = next_headers(&mut sock);
        let mut block = Vec::new();
        literal_header(":status", "200", &mut block);
        write_frame(&mut sock, FRAME_HEADERS, FLAG_END_HEADERS, first, &block);

        loop {
            let frame = read_frame(&mut sock).expect("client cancels abandoned stream");
            if frame.ty == FRAME_RST_STREAM {
                assert_eq!(frame.stream_id, first);
                break;
            }
        }

        let second = next_headers(&mut sock);
        send_response(&mut sock, second, "200", b"ok");
        std::thread::sleep(Duration::from_millis(50));
    });

    let runtime = ThreadedRuntime::with_config(
        TestShard,
        DefaultThreadedMailboxFactory,
        ThreadedRuntimeConfig {
            command_capacity: 64,
            idle_wait: Duration::from_millis(1),
            ..Default::default()
        },
    );
    let target = Http2Target::H2c {
        authority: "peer".into(),
        addr,
    };
    let limits = Http2ClientLimits {
        max_concurrent_streams: 1,
        response_stream_idle_timeout: Duration::from_millis(25),
        ..Http2ClientLimits::default()
    };
    let client = runtime
        .register_with_capacity::<Http2ClientConnection<TestShard>, _>(
            Http2ClientConnection::<TestShard>::new(target, limits)
                .expect("valid HTTP/2 client limits"),
            32,
        )
        .expect("register client");
    runtime
        .try_send(client, Http2ClientMsg::Begin)
        .expect("begin");

    let head = runtime
        .call_blocking(
            client,
            Http2ClientMsg::OpenStream(Http2ClientStreamCall {
                method: Method::GET,
                path: "/stream".into(),
                headers: HeaderMap::new(),
                body: Http2ClientRequestBody::Buffered(Vec::new()),
            }),
            Duration::from_secs(5),
        )
        .expect("open stream returns head");
    assert!(matches!(
        head,
        CallOutcome::Replied(Http2ClientReply::Outcome {
            outcome: Http2ClientOutcome::ResponseStreaming { .. },
            ..
        })
    ));

    std::thread::sleep(Duration::from_millis(100));

    let outcome = runtime
        .call_blocking(
            client,
            Http2ClientMsg::Submit(Http2ClientRequest::get("/after-abandon")),
            Duration::from_secs(5),
        )
        .expect("second request admitted");
    match outcome {
        CallOutcome::Replied(Http2ClientReply::Outcome {
            outcome: Http2ClientOutcome::Replied(response),
            ..
        }) => {
            assert_eq!(response.status, http::StatusCode::OK);
            assert_eq!(response.body, b"ok");
        }
        other => panic!("expected second request to reuse freed slot, got {other:?}"),
    }

    let _ = runtime.try_send(client, Http2ClientMsg::Stop);
    let _ = runtime.shutdown();
    peer.join().expect("peer thread joins");
}

#[test]
fn abandoned_streamed_response_after_end_stream_is_reaped_without_reset_and_slot_reused() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind peer");
    let addr = listener.local_addr().expect("peer addr");
    let peer = std::thread::spawn(move || {
        let (mut sock, _) = listener.accept().expect("accept");
        sock.set_read_timeout(Some(Duration::from_secs(5)))
            .expect("set timeout");
        complete_handshake_with(&mut sock, &[(SETTINGS_MAX_CONCURRENT_STREAMS, 1)]);

        let first = next_headers(&mut sock);
        let mut block = Vec::new();
        literal_header(":status", "200", &mut block);
        literal_header("content-length", "4", &mut block);
        write_frame(&mut sock, FRAME_HEADERS, FLAG_END_HEADERS, first, &block);
        write_frame(&mut sock, FRAME_DATA, FLAG_END_STREAM, first, b"done");

        let second = loop {
            let frame = read_frame(&mut sock).expect("second request or control frame");
            match frame.ty {
                FRAME_RST_STREAM if frame.stream_id == first => {
                    panic!("client must not reset an already-ended abandoned stream")
                }
                FRAME_HEADERS => break frame.stream_id,
                _ => continue,
            }
        };
        send_response(&mut sock, second, "200", b"ok");
        std::thread::sleep(Duration::from_millis(50));
    });

    let runtime = ThreadedRuntime::with_config(
        TestShard,
        DefaultThreadedMailboxFactory,
        ThreadedRuntimeConfig {
            command_capacity: 64,
            idle_wait: Duration::from_millis(1),
            ..Default::default()
        },
    );
    let target = Http2Target::H2c {
        authority: "peer".into(),
        addr,
    };
    let limits = Http2ClientLimits {
        max_concurrent_streams: 1,
        // The peer's END_STREAM arrives right behind the streaming head,
        // but the idle timer arms at head delivery. The timeout must
        // dwarf frame-processing jitter on a loaded runner, or the timer
        // fires before the client has read the END_STREAM and sends the
        // RST this test asserts never happens.
        response_stream_idle_timeout: Duration::from_millis(300),
        ..Http2ClientLimits::default()
    };
    let client = runtime
        .register_with_capacity::<Http2ClientConnection<TestShard>, _>(
            Http2ClientConnection::<TestShard>::new(target, limits)
                .expect("valid HTTP/2 client limits"),
            32,
        )
        .expect("register client");
    runtime
        .try_send(client, Http2ClientMsg::Begin)
        .expect("begin");

    let head = runtime
        .call_blocking(
            client,
            Http2ClientMsg::OpenStream(Http2ClientStreamCall {
                method: Method::GET,
                path: "/stream".into(),
                headers: HeaderMap::new(),
                body: Http2ClientRequestBody::Buffered(Vec::new()),
            }),
            Duration::from_secs(5),
        )
        .expect("open stream returns head");
    assert!(matches!(
        head,
        CallOutcome::Replied(Http2ClientReply::Outcome {
            outcome: Http2ClientOutcome::ResponseStreaming { .. },
            ..
        })
    ));

    std::thread::sleep(Duration::from_millis(900));

    let outcome = runtime
        .call_blocking(
            client,
            Http2ClientMsg::Submit(Http2ClientRequest::get("/after-eof-abandon")),
            Duration::from_secs(5),
        )
        .expect("second request admitted");
    match outcome {
        CallOutcome::Replied(Http2ClientReply::Outcome {
            outcome: Http2ClientOutcome::Replied(response),
            ..
        }) => {
            assert_eq!(response.status, http::StatusCode::OK);
            assert_eq!(response.body, b"ok");
        }
        other => panic!("expected second request to reuse freed slot, got {other:?}"),
    }

    let _ = runtime.try_send(client, Http2ClientMsg::Stop);
    let _ = runtime.shutdown();
    peer.join().expect("peer thread joins");
}

#[test]
fn pre_connect_queue_capacity_is_shared_across_request_shapes() {
    // Before the TCP connect completes, Submit, SubmitGrpcUnary, and
    // OpenStream all wait in the same user-visible
    // "pre-connect submit queue" budget. The cap is total, not one cap
    // per request shape; otherwise a service configured for two parked
    // requests could silently park extra work.
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind peer");
    let addr = listener.local_addr().expect("peer addr");
    let peer = std::thread::spawn(move || {
        let (mut sock, _) = listener.accept().expect("accept");
        complete_handshake_with(&mut sock, &[]);
        for _ in 0..2 {
            let stream_id = next_headers(&mut sock);
            send_response(&mut sock, stream_id, "200", b"ok");
        }
        std::thread::sleep(Duration::from_millis(50));
    });

    let runtime = ThreadedRuntime::with_config(
        TestShard,
        DefaultThreadedMailboxFactory,
        ThreadedRuntimeConfig {
            command_capacity: 64,
            idle_wait: Duration::from_millis(1),
            ..Default::default()
        },
    );
    let target = Http2Target::H2c {
        authority: "peer".into(),
        addr,
    };
    let limits = Http2ClientLimits {
        pre_connect_submit_capacity: 2,
        ..Http2ClientLimits::default()
    };
    let client = runtime
        .register_with_capacity::<Http2ClientConnection<TestShard>, _>(
            Http2ClientConnection::<TestShard>::new(target, limits)
                .expect("valid HTTP/2 client limits"),
            32,
        )
        .expect("register client");

    let outcomes = std::thread::scope(|scope| {
        let runtime_ref = &runtime;
        let submit = scope.spawn(move || {
            let grpc_client = GrpcClient::new(client, GrpcLimits::default());
            let msg = grpc_client
                .unary_request("/queued.Service/Call", &QueuedGrpcRequest {})
                .expect("compact gRPC user-facing request");
            runtime_ref
                .call_blocking(client, msg, Duration::from_secs(5))
                .expect("queued compact gRPC submit returns")
        });
        let runtime_ref = &runtime;
        let open = scope.spawn(move || {
            runtime_ref
                .call_blocking(
                    client,
                    Http2ClientMsg::OpenStream(Http2ClientStreamCall {
                        method: Method::GET,
                        path: "/queued-open".into(),
                        headers: HeaderMap::new(),
                        body: Http2ClientRequestBody::Buffered(Vec::new()),
                    }),
                    Duration::from_secs(5),
                )
                .expect("queued open stream returns")
        });

        // Give both parked calls a turn through the runtime before the
        // third request probes the shared pre-connect budget.
        std::thread::sleep(Duration::from_millis(100));
        let third = runtime
            .call_blocking(
                client,
                Http2ClientMsg::Submit(Http2ClientRequest::get("/over-cap")),
                Duration::from_secs(2),
            )
            .expect("over-cap submit returns promptly");
        match third {
            CallOutcome::Replied(Http2ClientReply::Outcome {
                stream_id: 0,
                outcome: Http2ClientOutcome::Full,
            }) => {}
            other => panic!("expected third pre-connect request to be Full, got {other:?}"),
        }

        runtime
            .try_send(client, Http2ClientMsg::Begin)
            .expect("begin client after queue fills");
        (submit.join().unwrap(), open.join().unwrap())
    });

    match outcomes.0 {
        // A `SubmitGrpcUnary` completes through the compact gRPC receive path.
        CallOutcome::Replied(Http2ClientReply::Outcome {
            outcome: Http2ClientOutcome::GrpcUnaryReplied { status, .. },
            ..
        }) => assert_eq!(status.as_u16(), 200),
        other => panic!("expected queued SubmitGrpcUnary to be GrpcUnaryReplied, got {other:?}"),
    }
    match outcomes.1 {
        CallOutcome::Replied(Http2ClientReply::Outcome {
            outcome: Http2ClientOutcome::ResponseStreaming { status, .. },
            ..
        }) => assert_eq!(status.as_u16(), 200),
        other => panic!("expected queued OpenStream head, got {other:?}"),
    }

    let _ = runtime.try_send(client, Http2ClientMsg::Stop);
    let _ = runtime.shutdown();
    peer.join().expect("peer thread joins");
}

#[test]
fn large_upload_paces_through_real_window_updates() {
    // 128 KB POST against a peer that drains DATA and feeds WINDOW_UPDATE
    // incrementally, then answers small. This forces the client's
    // outbound pacer to park on the 65535-byte default window and resume
    // on credit — a real end-to-end flow-control round trip, without the
    // server-side response deadlock that limits the in-tree server test.
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind peer");
    let addr = listener.local_addr().expect("peer addr");
    let total = 128 * 1024;
    let peer = std::thread::spawn(move || {
        let (mut sock, _) = listener.accept().expect("accept");
        complete_handshake_with(&mut sock, &[]);
        let stream_id = next_headers(&mut sock);
        // Drain DATA, crediting back each chunk so the client can keep
        // sending, until END_STREAM.
        let mut received = 0usize;
        loop {
            let frame = read_frame(&mut sock).expect("data frame");
            if frame.ty == FRAME_DATA {
                let n = frame.payload.len() as u32;
                received += frame.payload.len();
                if n > 0 {
                    write_window_update(&mut sock, 0, n);
                    write_window_update(&mut sock, frame.stream_id, n);
                }
                if frame.flags & FLAG_END_STREAM != 0 {
                    break;
                }
            }
        }
        assert_eq!(received, total, "peer received the whole body");
        send_response(&mut sock, stream_id, "200", b"ok");
        std::thread::sleep(Duration::from_millis(50));
    });
    let (runtime, client) = run_client(addr);

    let body = vec![b'z'; total];
    let mut req = Http2ClientRequest::post("/upload", body);
    req.headers.insert(
        "content-length",
        http::HeaderValue::from_str(&total.to_string()).unwrap(),
    );
    let outcome = runtime
        .call_blocking(client, Http2ClientMsg::Submit(req), Duration::from_secs(15))
        .expect("call returns outcome");
    match outcome {
        CallOutcome::Replied(Http2ClientReply::Outcome {
            outcome: Http2ClientOutcome::Replied(response),
            ..
        }) => assert_eq!(response.status.as_u16(), 200),
        other => panic!("expected Replied, got {other:?}"),
    }

    // The report must show the upload parked on flow control at least
    // once (128 KB > the 65535-byte default window).
    let report = runtime
        .call_blocking(client, Http2ClientMsg::Report, Duration::from_secs(2))
        .expect("report");
    match report {
        CallOutcome::Replied(Http2ClientReply::Report(report)) => {
            assert!(
                report.flow_control_parks > 0,
                "expected at least one flow-control park, got {report:?}"
            );
        }
        other => panic!("expected Report, got {other:?}"),
    }

    let _ = runtime.try_send(client, Http2ClientMsg::Stop);
    let _ = runtime.shutdown();
    peer.join().expect("peer thread joins");
}

#[test]
fn padded_response_data_delivers_unpadded_body_only() {
    // The peer answers with a PADDED DATA frame (pad-length byte + body +
    // padding). The client must deliver ONLY the unpadded body, and count the
    // full on-wire payload (padding included) against its flow-control windows
    // per RFC 9113 §6.9.1 — so a padding-using peer is not starved. A
    // flow-control miscount would surface here as a non-`Replied` outcome.
    const FLAG_PADDED: u8 = 0x8;
    let (addr, peer) = spawn_peer(|sock, stream_id| {
        let mut block = Vec::new();
        literal_header(":status", "200", &mut block);
        write_frame(sock, FRAME_HEADERS, FLAG_END_HEADERS, stream_id, &block);
        // Padded DATA: [pad_len=4]["hello"][4 pad bytes], END_STREAM.
        let mut data = Vec::new();
        data.push(4u8);
        data.extend_from_slice(b"hello");
        data.extend_from_slice(&[0u8; 4]);
        write_frame(
            sock,
            FRAME_DATA,
            FLAG_END_STREAM | FLAG_PADDED,
            stream_id,
            &data,
        );
    });
    let (runtime, client) = run_client(addr);

    let outcome = runtime
        .call_blocking(
            client,
            Http2ClientMsg::Submit(Http2ClientRequest::get("/x")),
            Duration::from_secs(5),
        )
        .expect("call returns outcome");
    match outcome {
        CallOutcome::Replied(Http2ClientReply::Outcome {
            outcome: Http2ClientOutcome::Replied(response),
            ..
        }) => {
            assert_eq!(response.status.as_u16(), 200);
            assert_eq!(
                response.body,
                b"hello".to_vec(),
                "only the unpadded body is delivered to the caller"
            );
        }
        other => panic!("expected Replied with unpadded body, got {other:?}"),
    }

    let _ = runtime.try_send(client, Http2ClientMsg::Stop);
    let _ = runtime.shutdown();
    peer.join().expect("peer thread joins");
}

#[test]
fn caller_cancel_returns_local_cancel_and_keeps_connection_alive() {
    // The peer accepts the request and holds it (never answers). The
    // caller's submit is admitted (stream 1); a separate thread sends
    // `Cancel { stream_id: 1 }`. The submit must come back
    // `LocalCancel`, and a follow-up GET on the same connection must
    // still succeed — cancellation does not poison the connection.
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind peer");
    let addr = listener.local_addr().expect("peer addr");
    let peer = std::thread::spawn(move || {
        let (mut sock, _) = listener.accept().expect("accept");
        complete_handshake_with(&mut sock, &[]);
        let _held = next_headers(&mut sock); // stream 1: never answered
        // Answer the *second* request (stream 3) so the follow-up GET
        // completes after the cancel.
        let second = next_headers(&mut sock);
        send_response(&mut sock, second, "200", b"after-cancel");
        std::thread::sleep(Duration::from_millis(50));
    });
    let (runtime, client) = run_client(addr);

    // Cancel stream 1 shortly after submitting it.
    std::thread::scope(|scope| {
        let runtime = &runtime;
        let cancel = scope.spawn(move || {
            std::thread::sleep(Duration::from_millis(60));
            let _ = runtime.try_send(client, Http2ClientMsg::Cancel { stream_id: 1 });
        });
        let outcome = runtime
            .call_blocking(
                client,
                Http2ClientMsg::Submit(Http2ClientRequest::get("/held")),
                Duration::from_secs(5),
            )
            .expect("held call returns");
        cancel.join().unwrap();
        match outcome {
            CallOutcome::Replied(Http2ClientReply::Outcome {
                stream_id: 1,
                outcome: Http2ClientOutcome::LocalCancel,
            }) => {}
            other => panic!("expected LocalCancel on stream 1, got {other:?}"),
        }
    });

    // Connection survives: a fresh GET completes.
    let follow_up = runtime
        .call_blocking(
            client,
            Http2ClientMsg::Submit(Http2ClientRequest::get("/again")),
            Duration::from_secs(5),
        )
        .expect("follow-up call returns");
    match follow_up {
        CallOutcome::Replied(Http2ClientReply::Outcome {
            outcome: Http2ClientOutcome::Replied(response),
            ..
        }) => assert_eq!(response.body, b"after-cancel"),
        other => panic!("expected Replied after cancel, got {other:?}"),
    }

    let _ = runtime.try_send(client, Http2ClientMsg::Stop);
    let _ = runtime.shutdown();
    peer.join().expect("peer thread joins");
}

#[test]
fn client_emits_outbound_open_and_close_lifecycle_facts() {
    // The plan requires the client to emit stream-lifecycle protocol
    // facts (not just private counters). A happy-path GET against the
    // foreign peer must produce an `Http2StreamOpened { direction:
    // Outbound }` and a matching `Http2StreamClosed` fact, captured from
    // the runtime trace.
    let (addr, peer) = spawn_peer(|sock, stream_id| {
        send_response(sock, stream_id, "200", b"ok");
    });
    let (runtime, client) = run_client(addr);

    let outcome = runtime
        .call_blocking(
            client,
            Http2ClientMsg::Submit(Http2ClientRequest::get("/x")),
            Duration::from_secs(5),
        )
        .expect("call returns outcome");
    assert!(matches!(
        outcome,
        CallOutcome::Replied(Http2ClientReply::Outcome {
            outcome: Http2ClientOutcome::Replied(_),
            ..
        })
    ));

    let facts = protocol_facts(&runtime);
    assert!(
        facts.iter().any(|f| matches!(
            f,
            ProtocolFact::Http2StreamOpened {
                direction: ProtocolDirection::Outbound,
                ..
            }
        )),
        "expected an outbound Http2StreamOpened fact, got {facts:?}",
    );
    assert!(
        facts
            .iter()
            .any(|f| matches!(f, ProtocolFact::Http2StreamClosed { .. })),
        "expected an Http2StreamClosed fact, got {facts:?}",
    );

    let _ = runtime.try_send(client, Http2ClientMsg::Stop);
    let _ = runtime.shutdown();
    peer.join().expect("peer thread joins");
}

#[test]
fn client_emits_inbound_reset_fact_on_peer_rst() {
    // A peer RST_STREAM must surface as an inbound-direction
    // `Http2StreamReset` protocol fact (in addition to the typed
    // caller outcome), so replay/observability sees the reset cause.
    let (addr, peer) = spawn_peer(|sock, stream_id| {
        write_frame(
            sock,
            FRAME_RST_STREAM,
            0,
            stream_id,
            &ERR_REFUSED_STREAM.to_be_bytes(),
        );
    });
    let (runtime, client) = run_client(addr);

    let _ = runtime
        .call_blocking(
            client,
            Http2ClientMsg::Submit(Http2ClientRequest::get("/x")),
            Duration::from_secs(5),
        )
        .expect("call returns outcome");

    let facts = protocol_facts(&runtime);
    assert!(
        facts.iter().any(|f| matches!(
            f,
            ProtocolFact::Http2StreamReset {
                direction: ProtocolDirection::Inbound,
                reason: Http2ResetReason::RefusedStream,
                ..
            }
        )),
        "expected an inbound Http2StreamReset(RefusedStream) fact, got {facts:?}",
    );

    let _ = runtime.try_send(client, Http2ClientMsg::Stop);
    let _ = runtime.shutdown();
    peer.join().expect("peer thread joins");
}

#[test]
fn goaway_above_stream_id_lets_admitted_stream_settle_then_blocks_new_admission() {
    // The other half of the GOAWAY contract: a GOAWAY whose
    // `last_stream_id` covers the in-flight stream (>= its id) must let
    // that stream settle normally, while a *subsequent* submit is
    // refused (`Closed`) because admission is closed after GOAWAY.
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind peer");
    let addr = listener.local_addr().expect("peer addr");
    let peer = std::thread::spawn(move || {
        let (mut sock, _) = listener.accept().expect("accept");
        complete_handshake_with(&mut sock, &[]);
        let first = next_headers(&mut sock);
        // GOAWAY(last_stream_id = first, NO_ERROR): "I am going away, but
        // I did process stream `first`." Then actually answer it.
        let mut payload = Vec::with_capacity(8);
        payload.extend_from_slice(&(first & 0x7fff_ffff).to_be_bytes());
        payload.extend_from_slice(&ERR_NO_ERROR.to_be_bytes());
        write_frame(&mut sock, FRAME_GOAWAY, 0, 0, &payload);
        send_response(&mut sock, first, "200", b"settled");
        std::thread::sleep(Duration::from_millis(80));
    });
    let (runtime, client) = run_client(addr);

    // First request: admitted before/at GOAWAY's last_stream_id, settles.
    let first = runtime
        .call_blocking(
            client,
            Http2ClientMsg::Submit(Http2ClientRequest::get("/first")),
            Duration::from_secs(5),
        )
        .expect("first call returns");
    match first {
        CallOutcome::Replied(Http2ClientReply::Outcome {
            outcome: Http2ClientOutcome::Replied(response),
            ..
        }) => assert_eq!(response.body, b"settled"),
        other => panic!("expected admitted stream to settle Replied, got {other:?}"),
    }

    // Second request after GOAWAY: admission is closed.
    let second = runtime
        .call_blocking(
            client,
            Http2ClientMsg::Submit(Http2ClientRequest::get("/second")),
            Duration::from_secs(5),
        )
        .expect("second call returns");
    match second {
        CallOutcome::Replied(Http2ClientReply::Outcome {
            outcome: Http2ClientOutcome::Closed,
            ..
        }) => {}
        other => panic!("expected new admission after GOAWAY to be Closed, got {other:?}"),
    }

    let _ = runtime.try_send(client, Http2ClientMsg::Stop);
    let _ = runtime.shutdown();
    peer.join().expect("peer thread joins");
}
