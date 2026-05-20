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
