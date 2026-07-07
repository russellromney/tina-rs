//! Live interop proofs for the native HTTP/2 client against the
//! in-tree Tina HTTP/2 server (Counter service). This file covers the
//! happy paths a well-behaved server exercises:
//! - h2c GET round-trip → typed `Replied` with status + body
//! - h2c POST round-trip → body echoed byte-for-byte via `/echo`
//! - multiple sequential streams share one connection isolate (report
//!   confirms `opened == closed`, zero protocol errors)
//! - response body over the client cap → typed `BodyTooLarge`
//! - in-window POST through the outbound flow-control pacer
//! - streaming request body: multi-chunk source round-trips to `/echo`;
//!   empty source still closes with an empty DATA(END_STREAM); the
//!   source is `Cancel`led (released, not orphaned) on completion
//! - route-key shape distinguishes a TLS target from an h2c one
//!
//! Real h2/TLS (round-trip with `h2` selected, ALPN mismatch, untrusted
//! cert) lives in `http2_client_tls_live.rs`. Adversarial / concurrency
//! / flow-control-under-window coverage —
//! server RST_STREAM, GOAWAY, malformed frames, *concurrent* streams
//! not crossing replies, `Full` admission under a peer concurrency
//! cap, caller cancel, and a 128 KB upload paced through real
//! WINDOW_UPDATE round trips — lives in `http2_client_adversarial.rs`,
//! which dials a hand-rolled misbehaving/foreign HTTP/2 peer.
//!
//! DST replay coverage and the native gRPC client are separate slices.

mod common;

use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use common::{Counter, TestShard};
use http::{HeaderMap, Method};
use tina::CallContext;
use tina::prelude::*;
use tina_http::{
    AlpnProtocols, Http2ClientConnection, Http2ClientLimits, Http2ClientMsg, Http2ClientOutcome,
    Http2ClientReply, Http2ClientRequest, Http2ClientRequestBody, Http2ClientStreamCall,
    Http2ClientStreamingRequest, Http2Limits, Http2Listener, Http2ListenerMsg, Http2ResponseChunk,
    Http2ServerConfig, Http2Target, IterBodySource, ResponseChunkMsg, ResponseChunkReply,
};
use tina_runtime::{
    CallOutcome, DefaultThreadedMailboxFactory, ThreadedRuntime, ThreadedRuntimeConfig,
};

fn start_server() -> (
    ThreadedRuntime<TestShard, DefaultThreadedMailboxFactory>,
    Address<Http2ListenerMsg>,
    SocketAddr,
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
    let counter = runtime
        .register_with_capacity::<Counter, Infallible>(Counter::default(), 16)
        .expect("register counter");
    let config = Http2ServerConfig {
        limits: Http2Limits::default(),
        service_call_timeout: Duration::from_secs(5),
        connection_mailbox_capacity: 16,
        listener_mailbox_capacity: 8,
    };
    let listener = runtime
        .register_with_capacity::<Http2Listener<TestShard>, _>(
            Http2Listener::<TestShard>::new("127.0.0.1:0".parse().unwrap(), counter, config),
            config.listener_mailbox_capacity,
        )
        .expect("register http2 listener");
    let bound = runtime.observe_next_bound();
    runtime
        .try_send(listener, Http2ListenerMsg::Start)
        .expect("start listener");
    let addr = bound
        .wait(Duration::from_secs(2))
        .expect("listener publishes bound address");
    (runtime, listener, addr)
}

fn make_client(
    runtime: &ThreadedRuntime<TestShard, DefaultThreadedMailboxFactory>,
    target: Http2Target,
    limits: Http2ClientLimits,
) -> Address<Http2ClientMsg, Http2ClientReply> {
    let client = runtime
        .register_with_capacity::<Http2ClientConnection<TestShard>, _>(
            Http2ClientConnection::<TestShard>::new(target, limits),
            32,
        )
        .expect("register http2 client");
    runtime
        .try_send(client, Http2ClientMsg::Begin)
        .expect("begin client");
    client
}

#[test]
fn h2c_get_round_trip_returns_typed_replied_outcome() {
    let (runtime, listener, addr) = start_server();
    let target = Http2Target::H2c {
        authority: "test".into(),
        addr,
    };
    let client = make_client(&runtime, target, Http2ClientLimits::default());

    let outcome = runtime
        .call_blocking(
            client,
            Http2ClientMsg::Submit(Http2ClientRequest::get("/counter")),
            Duration::from_secs(5),
        )
        .expect("call returns outcome");
    match outcome {
        CallOutcome::Replied(Http2ClientReply::Outcome {
            outcome: Http2ClientOutcome::Replied(response),
            ..
        }) => {
            assert_eq!(response.status.as_u16(), 200, "status from response");
            assert!(!response.body.is_empty(), "response body must be non-empty");
        }
        other => panic!("expected Replied, got {other:?}"),
    }

    let _ = runtime.try_send(listener, Http2ListenerMsg::Stop);
    let _ = runtime.try_send(client, Http2ClientMsg::Stop);
    let _ = runtime.shutdown();
}

#[test]
fn h2c_post_body_is_echoed_back_byte_for_byte() {
    // POSTs the body to the test Counter's `/echo` endpoint, which
    // returns the buffered request body unchanged. Proves the DATA
    // frame round-tripped end-to-end (HEADERS + DATA + END_STREAM on
    // request; HEADERS + DATA + END_STREAM on response), not just that
    // the server returned a 200.
    let (runtime, listener, addr) = start_server();
    let target = Http2Target::H2c {
        authority: "test".into(),
        addr,
    };
    let client = make_client(&runtime, target, Http2ClientLimits::default());

    let body = b"hello-from-tina-client".to_vec();
    let mut req = Http2ClientRequest::post("/echo", body.clone());
    req.headers.insert(
        "content-length",
        http::HeaderValue::from_str(&body.len().to_string()).unwrap(),
    );
    let outcome = runtime
        .call_blocking(client, Http2ClientMsg::Submit(req), Duration::from_secs(5))
        .expect("call returns outcome");
    match outcome {
        CallOutcome::Replied(Http2ClientReply::Outcome {
            outcome: Http2ClientOutcome::Replied(response),
            ..
        }) => {
            assert_eq!(response.status.as_u16(), 200);
            assert_eq!(
                response.body, body,
                "/echo must echo the exact bytes the client sent"
            );
        }
        other => panic!("expected Replied, got {other:?}"),
    }

    let _ = runtime.try_send(listener, Http2ListenerMsg::Stop);
    let _ = runtime.try_send(client, Http2ClientMsg::Stop);
    let _ = runtime.shutdown();
}

#[test]
fn h2c_multiple_streams_share_one_client_connection() {
    // First-form reuse: "one connection isolate carries many admitted
    // streams." Three sequential GETs over the same client isolate
    // should all succeed and the per-connection report should show
    // opened_streams == 3 and closed_streams == 3.
    let (runtime, listener, addr) = start_server();
    let target = Http2Target::H2c {
        authority: "test".into(),
        addr,
    };
    let client = make_client(&runtime, target, Http2ClientLimits::default());

    for _ in 0..3 {
        let outcome = runtime
            .call_blocking(
                client,
                Http2ClientMsg::Submit(Http2ClientRequest::get("/counter")),
                Duration::from_secs(5),
            )
            .expect("call returns outcome");
        match outcome {
            CallOutcome::Replied(Http2ClientReply::Outcome {
                outcome: Http2ClientOutcome::Replied(response),
                ..
            }) => {
                assert_eq!(response.status.as_u16(), 200);
            }
            other => panic!("expected Replied, got {other:?}"),
        }
    }

    let report = runtime
        .call_blocking(client, Http2ClientMsg::Report, Duration::from_secs(2))
        .expect("report returns");
    match report {
        CallOutcome::Replied(Http2ClientReply::Report(report)) => {
            assert_eq!(report.opened_streams, 3, "three streams admitted");
            assert_eq!(report.closed_streams, 3, "three streams closed cleanly");
            assert_eq!(report.protocol_errors, 0, "no protocol errors");
            assert_eq!(report.reset_streams, 0, "no inbound resets");
        }
        other => panic!("expected Report, got {other:?}"),
    }

    let _ = runtime.try_send(listener, Http2ListenerMsg::Stop);
    let _ = runtime.try_send(client, Http2ClientMsg::Stop);
    let _ = runtime.shutdown();
}

#[test]
fn response_body_above_cap_returns_typed_body_too_large() {
    // The plan: "oversized received message is `ResourceExhausted` /
    // typed cap failure before unbounded allocation." For raw HTTP/2
    // we map that to `Http2ProtocolError::BodyTooLarge { cap_bytes }`.
    // POST 4 KB to /echo with a 1 KB response cap; client RSTs and
    // returns the typed error, NOT `HeadersTooLarge`.
    let (runtime, listener, addr) = start_server();
    let target = Http2Target::H2c {
        authority: "test".into(),
        addr,
    };
    let limits = Http2ClientLimits {
        max_response_body_bytes: 1024,
        ..Http2ClientLimits::default()
    };
    let client = make_client(&runtime, target, limits);

    let body = vec![b'x'; 4096];
    let mut req = Http2ClientRequest::post("/echo", body.clone());
    req.headers.insert(
        "content-length",
        http::HeaderValue::from_str(&body.len().to_string()).unwrap(),
    );
    let outcome = runtime
        .call_blocking(client, Http2ClientMsg::Submit(req), Duration::from_secs(5))
        .expect("call returns outcome");
    match outcome {
        CallOutcome::Replied(Http2ClientReply::Outcome {
            outcome:
                Http2ClientOutcome::ProtocolError(tina_http::Http2ProtocolError::BodyTooLarge {
                    cap_bytes,
                }),
            ..
        }) => {
            assert_eq!(cap_bytes, 1024);
        }
        other => panic!("expected ProtocolError(BodyTooLarge), got {other:?}"),
    }

    let _ = runtime.try_send(listener, Http2ListenerMsg::Stop);
    let _ = runtime.try_send(client, Http2ClientMsg::Stop);
    let _ = runtime.shutdown();
}

#[test]
fn h2c_post_in_window_body_round_trips_against_real_server() {
    // In-window outbound proof against the real Tina server: a 32 KB
    // POST fits inside the 65535-byte default window, so it round-trips
    // without needing WINDOW_UPDATE. This guards against the
    // outbound-flow-control pacer regressing the common in-window case.
    //
    // The "actually park and resume on WINDOW_UPDATE" proof (a 128 KB
    // upload through real window-update round trips) lives in
    // `http2_client_adversarial.rs::large_upload_paces_through_real_window_updates`,
    // which uses a hand-rolled peer that drains and credits
    // incrementally — the in-tree server cannot host that proof because
    // its *response* path parks the whole body until it fits the window
    // (see the KNOWN LIMITATION in `http2/server.rs`).
    let runtime = ThreadedRuntime::with_config(
        TestShard,
        DefaultThreadedMailboxFactory,
        ThreadedRuntimeConfig {
            command_capacity: 64,
            idle_wait: Duration::from_millis(1),
            ..Default::default()
        },
    );
    let counter = runtime
        .register_with_capacity::<Counter, Infallible>(Counter::default(), 16)
        .expect("register counter");
    let limits = Http2Limits {
        max_body_bytes: 256 * 1024,
        max_response_body_bytes: 256 * 1024,
        ..Http2Limits::default()
    };
    let config = Http2ServerConfig {
        limits,
        service_call_timeout: Duration::from_secs(5),
        connection_mailbox_capacity: 16,
        listener_mailbox_capacity: 8,
    };
    let listener = runtime
        .register_with_capacity::<Http2Listener<TestShard>, _>(
            Http2Listener::<TestShard>::new("127.0.0.1:0".parse().unwrap(), counter, config),
            config.listener_mailbox_capacity,
        )
        .expect("register http2 listener");
    let bound = runtime.observe_next_bound();
    runtime
        .try_send(listener, Http2ListenerMsg::Start)
        .expect("start listener");
    let addr = bound
        .wait(Duration::from_secs(2))
        .expect("listener publishes bound address");

    let target = Http2Target::H2c {
        authority: "test".into(),
        addr,
    };
    let client_limits = Http2ClientLimits {
        max_response_body_bytes: 256 * 1024,
        ..Http2ClientLimits::default()
    };
    let client = make_client(&runtime, target, client_limits);

    // 32 KB body — half the 65535-byte default window, so the upload
    // fits in one window without needing WINDOW_UPDATE.
    let body = vec![b'a'; 32 * 1024];
    let mut req = Http2ClientRequest::post("/counter", body.clone());
    req.headers.insert(
        "content-length",
        http::HeaderValue::from_str(&body.len().to_string()).unwrap(),
    );
    let outcome = runtime
        .call_blocking(client, Http2ClientMsg::Submit(req), Duration::from_secs(15))
        .expect("call returns outcome");
    match outcome {
        CallOutcome::Replied(Http2ClientReply::Outcome {
            outcome: Http2ClientOutcome::Replied(response),
            ..
        }) => {
            assert_eq!(response.status.as_u16(), 200);
        }
        other => panic!("expected Replied, got {other:?}"),
    }

    let _ = runtime.try_send(listener, Http2ListenerMsg::Stop);
    let _ = runtime.try_send(client, Http2ClientMsg::Stop);
    let _ = runtime.shutdown();
}

#[test]
fn h2c_streaming_request_body_round_trips_chunks_to_echo() {
    // Streaming request bodies: the client sends HEADERS *without*
    // END_STREAM, then pulls body chunks from a source isolate and ships
    // them as DATA frames, ending the request with END_STREAM once the
    // source replies `Eof`. The in-tree server reassembles the DATA frames
    // into a buffered body before dispatching to `/echo`, so a correct
    // round-trip proves every chunk arrived in order and the final
    // END_STREAM closed the request half.
    let (runtime, listener, addr) = start_server();
    let target = Http2Target::H2c {
        authority: "test".into(),
        addr,
    };
    let client = make_client(&runtime, target, Http2ClientLimits::default());

    let chunks = vec![
        b"chunk-one-".to_vec(),
        b"chunk-two-".to_vec(),
        b"chunk-three".to_vec(),
    ];
    let expected: Vec<u8> = chunks.iter().flatten().copied().collect();
    let source = IterBodySource::<TestShard>::register(&runtime, chunks.into_iter(), 16)
        .expect("register body source");

    let req = Http2ClientStreamingRequest {
        method: Method::POST,
        path: "/echo".into(),
        headers: http::HeaderMap::new(),
        source,
    };
    let outcome = runtime
        .call_blocking(
            client,
            Http2ClientMsg::SubmitStreaming(req),
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
                response.body, expected,
                "/echo must echo the concatenated streamed chunks byte-for-byte"
            );
        }
        other => panic!("expected Replied, got {other:?}"),
    }

    let report = runtime
        .call_blocking(client, Http2ClientMsg::Report, Duration::from_secs(2))
        .expect("report returns");
    match report {
        CallOutcome::Replied(Http2ClientReply::Report(report)) => {
            assert_eq!(report.opened_streams, 1, "one streaming stream admitted");
            assert_eq!(report.closed_streams, 1, "stream closed cleanly");
            assert_eq!(report.protocol_errors, 0, "no protocol errors");
        }
        other => panic!("expected Report, got {other:?}"),
    }

    let _ = runtime.try_send(listener, Http2ListenerMsg::Stop);
    let _ = runtime.try_send(client, Http2ClientMsg::Stop);
    let _ = runtime.shutdown();
}

#[test]
fn h2c_streaming_request_with_empty_source_sends_empty_body() {
    // A streaming source that yields no chunks (immediate `Eof`) must
    // still close the request half: HEADERS without END_STREAM, then a
    // lone empty DATA(END_STREAM). `/echo` sees a buffered empty body and
    // returns 200 with an empty body — proving the empty-body END_STREAM
    // path fires rather than hanging the stream open.
    let (runtime, listener, addr) = start_server();
    let target = Http2Target::H2c {
        authority: "test".into(),
        addr,
    };
    let client = make_client(&runtime, target, Http2ClientLimits::default());

    let source = IterBodySource::<TestShard>::register(&runtime, std::iter::empty(), 16)
        .expect("register empty body source");
    let req = Http2ClientStreamingRequest {
        method: Method::POST,
        path: "/echo".into(),
        headers: http::HeaderMap::new(),
        source,
    };
    let outcome = runtime
        .call_blocking(
            client,
            Http2ClientMsg::SubmitStreaming(req),
            Duration::from_secs(5),
        )
        .expect("call returns outcome");
    match outcome {
        CallOutcome::Replied(Http2ClientReply::Outcome {
            outcome: Http2ClientOutcome::Replied(response),
            ..
        }) => {
            assert_eq!(response.status.as_u16(), 200);
            assert!(response.body.is_empty(), "empty source yields empty body");
        }
        other => panic!("expected Replied, got {other:?}"),
    }

    let _ = runtime.try_send(listener, Http2ListenerMsg::Stop);
    let _ = runtime.try_send(client, Http2ClientMsg::Stop);
    let _ = runtime.shutdown();
}

/// Drive `ResponseNext` pulls to completion, returning the reassembled
/// body and the trailing HEADERS block. Panics on any non-`Data`/`End`
/// chunk so a `Reset`/`Closed`/`ProtocolError` fails the test loudly.
fn collect_streamed_response(
    runtime: &ThreadedRuntime<TestShard, DefaultThreadedMailboxFactory>,
    client: Address<Http2ClientMsg, Http2ClientReply>,
    stream_id: u32,
) -> (Vec<u8>, HeaderMap) {
    let mut body = Vec::new();
    loop {
        let reply = runtime
            .call_blocking(
                client,
                Http2ClientMsg::ResponseNext { stream_id },
                Duration::from_secs(5),
            )
            .expect("response pull returns");
        match reply {
            CallOutcome::Replied(Http2ClientReply::ResponseChunk { chunk, .. }) => match chunk {
                Http2ResponseChunk::Data(bytes) => body.extend_from_slice(&bytes),
                Http2ResponseChunk::End { trailers } => return (body, trailers),
                other => panic!("unexpected terminal chunk: {other:?}"),
            },
            other => panic!("expected ResponseChunk, got {other:?}"),
        }
    }
}

#[test]
fn h2c_open_stream_delivers_head_then_pulled_body() {
    // Streaming response: OpenStream a GET, get the response head as
    // `ResponseStreaming`, then pull the body chunk-by-chunk to `End`.
    // Proves the head/pull protocol end-to-end against the real server.
    let (runtime, listener, addr) = start_server();
    let target = Http2Target::H2c {
        authority: "test".into(),
        addr,
    };
    let client = make_client(&runtime, target, Http2ClientLimits::default());

    let call = Http2ClientStreamCall {
        method: Method::GET,
        path: "/counter".into(),
        headers: HeaderMap::new(),
        body: Http2ClientRequestBody::Buffered(Vec::new()),
    };
    let outcome = runtime
        .call_blocking(
            client,
            Http2ClientMsg::OpenStream(call),
            Duration::from_secs(5),
        )
        .expect("open stream returns");
    let stream_id = match outcome {
        CallOutcome::Replied(Http2ClientReply::Outcome {
            stream_id,
            outcome: Http2ClientOutcome::ResponseStreaming { status, .. },
        }) => {
            assert_eq!(status.as_u16(), 200, "streamed response head status");
            stream_id
        }
        other => panic!("expected ResponseStreaming head, got {other:?}"),
    };

    let (body, _trailers) = collect_streamed_response(&runtime, client, stream_id);
    assert!(!body.is_empty(), "pulled body must be non-empty");

    let _ = runtime.try_send(listener, Http2ListenerMsg::Stop);
    let _ = runtime.try_send(client, Http2ClientMsg::Stop);
    let _ = runtime.shutdown();
}

#[test]
fn h2c_open_stream_reassembles_multi_frame_response_body() {
    // A 32 KB echo response arrives as several DATA frames; the client
    // hands them out one pull at a time and the caller reassembles the
    // exact bytes. Proves multi-chunk streamed delivery + the
    // credit-on-consume window accounting does not corrupt the body.
    let runtime = ThreadedRuntime::with_config(
        TestShard,
        DefaultThreadedMailboxFactory,
        ThreadedRuntimeConfig {
            command_capacity: 64,
            idle_wait: Duration::from_millis(1),
            ..Default::default()
        },
    );
    let counter = runtime
        .register_with_capacity::<Counter, Infallible>(Counter::default(), 16)
        .expect("register counter");
    let limits = Http2Limits {
        max_body_bytes: 256 * 1024,
        max_response_body_bytes: 256 * 1024,
        ..Http2Limits::default()
    };
    let config = Http2ServerConfig {
        limits,
        service_call_timeout: Duration::from_secs(5),
        connection_mailbox_capacity: 16,
        listener_mailbox_capacity: 8,
    };
    let listener = runtime
        .register_with_capacity::<Http2Listener<TestShard>, _>(
            Http2Listener::<TestShard>::new("127.0.0.1:0".parse().unwrap(), counter, config),
            config.listener_mailbox_capacity,
        )
        .expect("register http2 listener");
    let bound = runtime.observe_next_bound();
    runtime
        .try_send(listener, Http2ListenerMsg::Start)
        .expect("start listener");
    let addr = bound
        .wait(Duration::from_secs(2))
        .expect("listener publishes bound address");

    let target = Http2Target::H2c {
        authority: "test".into(),
        addr,
    };
    let client_limits = Http2ClientLimits {
        max_response_body_bytes: 256 * 1024,
        ..Http2ClientLimits::default()
    };
    let client = make_client(&runtime, target, client_limits);

    let payload = vec![b'z'; 32 * 1024];
    let mut headers = HeaderMap::new();
    headers.insert(
        "content-length",
        http::HeaderValue::from_str(&payload.len().to_string()).unwrap(),
    );
    let call = Http2ClientStreamCall {
        method: Method::POST,
        path: "/echo".into(),
        headers,
        body: Http2ClientRequestBody::Buffered(payload.clone()),
    };
    let outcome = runtime
        .call_blocking(
            client,
            Http2ClientMsg::OpenStream(call),
            Duration::from_secs(15),
        )
        .expect("open stream returns");
    let stream_id = match outcome {
        CallOutcome::Replied(Http2ClientReply::Outcome {
            stream_id,
            outcome: Http2ClientOutcome::ResponseStreaming { status, .. },
        }) => {
            assert_eq!(status.as_u16(), 200);
            stream_id
        }
        other => panic!("expected ResponseStreaming head, got {other:?}"),
    };

    let (body, _trailers) = collect_streamed_response(&runtime, client, stream_id);
    assert_eq!(body, payload, "reassembled streamed body must match echo");

    let _ = runtime.try_send(listener, Http2ListenerMsg::Stop);
    let _ = runtime.try_send(client, Http2ClientMsg::Stop);
    let _ = runtime.shutdown();
}

/// A streaming request body source that records how many
/// `ResponseChunkMsg::Cancel`s it received before stopping. Proves the
/// connection releases the source instead of orphaning it.
struct RecordingSource {
    chunks: std::vec::IntoIter<Vec<u8>>,
    cancels: Arc<AtomicUsize>,
}

impl Isolate for RecordingSource {
    tina::isolate_types! {
        message: ResponseChunkMsg,
        reply: ResponseChunkReply,
        send: tina::Outbound<Infallible>,
        spawn: Infallible,
        io: tina_runtime::RuntimeCall<ResponseChunkMsg>,
        shard: TestShard,
    }

    fn handle(
        &mut self,
        msg: ResponseChunkMsg,
        _ctx: &mut Context<'_, TestShard, Self::Reply>,
    ) -> Effect<Self> {
        match msg {
            ResponseChunkMsg::Cancel => {
                self.cancels.fetch_add(1, Ordering::SeqCst);
                stop()
            }
            _ => reply(self.next_chunk()),
        }
    }

    fn handle_call(&mut self, msg: ResponseChunkMsg, call: CallContext<'_, Self>) -> Effect<Self> {
        match msg {
            ResponseChunkMsg::Cancel => {
                self.cancels.fetch_add(1, Ordering::SeqCst);
                stop()
            }
            _ => call.reply(self.next_chunk()),
        }
    }
}

impl RecordingSource {
    fn next_chunk(&mut self) -> ResponseChunkReply {
        match self.chunks.next() {
            Some(bytes) if !bytes.is_empty() => ResponseChunkReply::Chunk(bytes),
            _ => ResponseChunkReply::Eof,
        }
    }
}

#[test]
fn h2c_streaming_request_source_is_cancelled_on_completion() {
    // No-leak proof: a chunk source only releases its resources on
    // `ResponseChunkMsg::Cancel` (a clean `Eof` reply leaves it alive),
    // so the connection must `Cancel` the source when it is done pulling.
    // After a complete streamed round-trip the source records exactly one
    // Cancel — not zero (orphaned) and not many (cancel storm).
    let (runtime, listener, addr) = start_server();
    let target = Http2Target::H2c {
        authority: "test".into(),
        addr,
    };
    let client = make_client(&runtime, target, Http2ClientLimits::default());

    let cancels = Arc::new(AtomicUsize::new(0));
    let source = runtime
        .register_with_capacity::<RecordingSource, Infallible>(
            RecordingSource {
                chunks: vec![b"alpha".to_vec(), b"beta".to_vec()].into_iter(),
                cancels: cancels.clone(),
            },
            16,
        )
        .expect("register recording source");

    let req = Http2ClientStreamingRequest {
        method: Method::POST,
        path: "/echo".into(),
        headers: HeaderMap::new(),
        source,
    };
    let outcome = runtime
        .call_blocking(
            client,
            Http2ClientMsg::SubmitStreaming(req),
            Duration::from_secs(5),
        )
        .expect("call returns outcome");
    match outcome {
        CallOutcome::Replied(Http2ClientReply::Outcome {
            outcome: Http2ClientOutcome::Replied(response),
            ..
        }) => {
            assert_eq!(response.status.as_u16(), 200);
            assert_eq!(response.body, b"alphabeta");
        }
        other => panic!("expected Replied, got {other:?}"),
    }

    // The Cancel rides out asynchronously after the response completes;
    // poll briefly for it rather than racing.
    let deadline = Instant::now() + Duration::from_secs(2);
    while cancels.load(Ordering::SeqCst) == 0 && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(
        cancels.load(Ordering::SeqCst),
        1,
        "source must receive exactly one Cancel when the stream completes"
    );

    let _ = runtime.try_send(listener, Http2ListenerMsg::Stop);
    let _ = runtime.try_send(client, Http2ClientMsg::Stop);
    let _ = runtime.shutdown();
}

// Real h2/TLS coverage (happy path with `h2` selected, plus ALPN
// mismatch) lives in `http2_client_tls_live.rs`, which stands up a
// rustls + HTTP/2 server peer. This file is the h2c suite.

#[test]
fn tls_target_route_key_distinguishes_from_h2c_route_key() {
    // Reuse keying: TLS and h2c with the same authority must not share a
    // pool entry. This is a unit-shape proof, but it pins the route key
    // shape that pool work in Phase 119 will read.
    let h2c = Http2Target::H2c {
        authority: "x".into(),
        addr: "127.0.0.1:8080".parse().unwrap(),
    };
    let tls = Http2Target::Tls {
        authority: "x".into(),
        addr: "127.0.0.1:8443".parse().unwrap(),
        server_name: "x".into(),
        trust_roots: vec![vec![0_u8; 4]],
        alpn: AlpnProtocols::h2(),
    };
    assert_ne!(h2c.route_key(), tls.route_key());
}

#[test]
fn request_method_and_path_round_trip_through_targets() {
    // Compile/shape proof: Http2ClientRequest helpers produce GET/POST
    // requests with the right method, and Http2Target accessors return
    // the wire authority unchanged.
    let req = Http2ClientRequest::get("/health");
    assert_eq!(req.method, Method::GET);
    assert_eq!(req.path, "/health");
    let target = Http2Target::H2c {
        authority: "service.local".into(),
        addr: "127.0.0.1:9090".parse().unwrap(),
    };
    assert_eq!(target.authority(), "service.local");
    assert!(!target.is_tls());
}
