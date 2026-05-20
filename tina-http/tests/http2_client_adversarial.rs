//! Adversarial live proofs for the native HTTP/2 client.
//!
//! These tests stand up a *hand-rolled, deliberately misbehaving*
//! HTTP/2 server peer on a raw `TcpStream` and dial it from the real
//! `Http2ClientConnection`. They pin the spec-compliance paths a
//! well-behaved Tina server never exercises:
//!
//! - server `RST_STREAM` mid-stream → client `Reset(reason)`
//! - server `RST_STREAM` on stream 0 → connection-level protocol error
//!   (client GOAWAYs and fails the in-flight stream, connection dies
//!   cleanly rather than silently ignoring the illegal frame)
//! - server `GOAWAY(last_stream_id = 0)` → client refuses the
//!   unprocessed stream with `Closed` and the caller can retry
//!
//! The peer runs on its own thread. The client runs in a Tina runtime.

mod common;

use std::convert::Infallible;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::mpsc;
use std::time::Duration;

use common::TestShard;
use tina::prelude::*;
use tina_http::{
    Http2ClientConnection, Http2ClientLimits, Http2ClientMsg, Http2ClientOutcome, Http2ClientReply,
    Http2ClientRequest, Http2ProtocolError, Http2Target,
};
use tina_runtime::{
    CallOutcome, DefaultThreadedMailboxFactory, Http2ResetReason, ThreadedRuntime,
    ThreadedRuntimeConfig,
};

const CLIENT_PREFACE: &[u8] = b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n";
const FRAME_HEADERS: u8 = 0x1;
const FRAME_RST_STREAM: u8 = 0x3;
const FRAME_SETTINGS: u8 = 0x4;
const FRAME_GOAWAY: u8 = 0x7;
const FLAG_ACK: u8 = 0x1;
const ERR_REFUSED_STREAM: u32 = 0x7;
const ERR_NO_ERROR: u32 = 0x0;

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
            Http2ClientConnection::<TestShard>::new(target, Http2ClientLimits::default()),
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
    let peer = std::thread::spawn(move || {
        let (mut sock, _) = listener.accept().expect("accept");
        complete_handshake_with(&mut sock, &[(SETTINGS_MAX_CONCURRENT_STREAMS, 1)]);
        // Only one stream will ever reach the wire (the other is refused
        // client-side as Full). Read it, hold briefly, then answer.
        let only = next_headers(&mut sock);
        std::thread::sleep(Duration::from_millis(150));
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
        // Stagger the second submit slightly so the first is admitted
        // and occupies the single slot before the second arrives.
        std::thread::sleep(Duration::from_millis(40));
        let b = scope.spawn(move || {
            runtime
                .call_blocking(
                    client,
                    Http2ClientMsg::Submit(Http2ClientRequest::get("/b")),
                    Duration::from_secs(5),
                )
                .expect("call b")
        });
        (a.join().unwrap(), b.join().unwrap())
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
