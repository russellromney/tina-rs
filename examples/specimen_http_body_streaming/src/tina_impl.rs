//! Tina side: an `IterBodySource` wraps a closure-iterator into a
//! chunk source — no custom `Isolate` impl. The service routes
//! `/big` to a known-length stream and `/big-chunked` to an
//! unknown-length stream framed by `Transfer-Encoding: chunked`.
//! The smoke run exercises both routes.
//!
//! The `BodyMetrics` shared with the listener records the
//! response-body high-water across the whole run. Each chunk is
//! pulled only after the previous chunk has fully drained, so the
//! peak charge is exactly `CHUNK_BYTES` — never the full
//! `RESPONSE_BODY_BYTES`.
//!
//! Compare with [`crate::tokio_impl`] where the whole vec is
//! resident before the first byte goes on the wire.

use std::convert::Infallible;
use std::time::Duration;

use http::StatusCode;
use tina::prelude::*;
use tina_http::{
    BodyMetrics, HttpListener, HttpListenerMsg, HttpRequest, HttpResponse, HttpServerConfig,
    IterBodySource, ResponseChunkMsg, ResponseChunkReply,
};
use tina_runtime::{DefaultThreadedMailboxFactory, ThreadedRuntime, format_discovery_line};

use crate::{
    CHUNK_BYTES, RESPONSE_BODY_BYTES, Report, decode_chunked, slow_reader_client,
};

/// Service: `/big` returns a known-length stream, `/big-chunked`
/// returns a chunked stream. Both pull from chunk sources the
/// caller supplies — same producer shape, different framing.
struct StreamingService {
    known_source: Address<ResponseChunkMsg, ResponseChunkReply>,
    chunked_source: Address<ResponseChunkMsg, ResponseChunkReply>,
}

#[tina::isolate(message = HttpRequest, reply = HttpResponse)]
impl StreamingService {
    fn handle(
        &mut self,
        request: HttpRequest,
        _ctx: &mut Context<'_, SingleShard, Self::Reply>,
    ) -> Effect<Self> {
        reply(self.response_for(request))
    }

    fn handle_call(
        &mut self,
        request: HttpRequest,
        call: tina::CallContext<'_, Self>,
    ) -> Effect<Self> {
        call.reply(self.response_for(request))
    }

    fn response_for(&self, request: HttpRequest) -> HttpResponse {
        match request.path.as_str() {
            "/big" => HttpResponse::stream_known_length(
                StatusCode::OK,
                RESPONSE_BODY_BYTES,
                self.known_source,
            ),
            "/big-chunked" => HttpResponse::stream_chunked(StatusCode::OK, self.chunked_source),
            _ => HttpResponse::not_found(),
        }
    }
}

/// Closure-iterator that yields enough chunks to cover
/// `RESPONSE_BODY_BYTES`. Each chunk is `CHUNK_BYTES` of `b'a'`,
/// except the last which may be shorter.
fn body_chunks() -> impl Iterator<Item = Vec<u8>> + Send + 'static {
    let mut sent = 0usize;
    std::iter::from_fn(move || {
        if sent >= RESPONSE_BODY_BYTES {
            return None;
        }
        let take = CHUNK_BYTES.min(RESPONSE_BODY_BYTES - sent);
        sent += take;
        Some(vec![b'a'; take])
    })
}

pub fn run() -> anyhow::Result<Report> {
    let runtime: ThreadedRuntime<SingleShard, DefaultThreadedMailboxFactory> =
        ThreadedRuntime::try_new(SingleShard, DefaultThreadedMailboxFactory)?;
    let metrics =
        BodyMetrics::with_body_capacity("http.bodies", CHUNK_BYTES, RESPONSE_BODY_BYTES);

    // `IterBodySource::register` wraps the iterator and registers
    // the source isolate in one step — no turbofish, no
    // `Infallible` placeholder.
    let known_source = IterBodySource::<SingleShard>::register(&runtime, body_chunks(), 16)
        .map_err(|e| anyhow::anyhow!("register known source: {e:?}"))?;
    let chunked_source = IterBodySource::<SingleShard>::register(&runtime, body_chunks(), 16)
        .map_err(|e| anyhow::anyhow!("register chunked source: {e:?}"))?;
    let service = runtime
        .register_with_capacity::<_, Infallible>(
            StreamingService {
                known_source,
                chunked_source,
            },
            16,
        )
        .map_err(|e| anyhow::anyhow!("register service: {e:?}"))?;

    let server_config = HttpServerConfig::dev();
    let listener_isolate = HttpListener::<SingleShard>::with_config(
        "127.0.0.1:0".parse()?,
        service,
        server_config,
    )
    .with_metrics(metrics.clone());
    let listener = runtime
        .register_with_capacity::<_, _>(listener_isolate, server_config.listener_mailbox_capacity)
        .map_err(|e| anyhow::anyhow!("register listener: {e:?}"))?;

    let bound = runtime.observe_next_bound();
    runtime
        .try_send(listener, HttpListenerMsg::Start)
        .map_err(|e| anyhow::anyhow!("send Start: {e:?}"))?;
    let server_addr = bound
        .wait(Duration::from_secs(2))
        .map_err(|e| anyhow::anyhow!("listener bind: {e:?}"))?;

    // `/big` exercises slow-reader pressure on the known-length
    // path; `/big-chunked` exercises round-trip + decode on the
    // chunked path. Each chunk source is single-use so we have
    // exactly one request per source — no need to slow-read the
    // chunked path; the round-trip proves wire shape and the
    // metrics already prove pressure boundedness from `/big`.
    let (known_bytes, known_ok, wall_ms_known) = slow_reader_client(server_addr, "/big");
    let (chunked_wire_bytes, chunked_decoded_len, chunked_ok) =
        chunked_request_decoded(server_addr)?;

    runtime
        .try_send(listener, HttpListenerMsg::Stop)
        .map_err(|e| anyhow::anyhow!("send Stop: {e:?}"))?;
    let _ = runtime.shutdown();

    let snap = metrics.snapshot();
    let capacity_discovery_line = snap
        .response_capacity_report(
            "specimen_http_body_streaming.response_body",
            tina::capacity::CapacityMode::Fixed,
        )
        .map(|report| format_discovery_line(&report));
    Ok(Report {
        bytes_received: known_bytes,
        status_ok: known_ok && chunked_ok,
        wall_clock_ms: wall_ms_known,
        exit_clean: snap.drained(),
        tokio_response_alloc_floor: None,
        tina_response_high_water: Some(snap.response_body_high_water),
        tina_chunked_wire_bytes: Some(chunked_wire_bytes),
        tina_chunked_decoded_bytes: Some(chunked_decoded_len),
        tina_capacity_discovery_line: capacity_discovery_line,
    })
}

/// Fetches `/big-chunked`, captures the whole response, decodes
/// the chunked body, and verifies the head advertises chunked
/// framing. Returns `(wire_body_bytes, decoded_bytes, status_ok)`.
fn chunked_request_decoded(
    addr: std::net::SocketAddr,
) -> anyhow::Result<(usize, usize, bool)> {
    use std::io::{Read, Write};
    use std::net::TcpStream;

    let mut stream = TcpStream::connect_timeout(&addr, Duration::from_secs(5))?;
    stream.set_read_timeout(Some(Duration::from_secs(30)))?;
    stream.write_all(b"GET /big-chunked HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n")?;
    stream.flush()?;
    let mut response = Vec::new();
    let _ = stream.read_to_end(&mut response);
    let separator = b"\r\n\r\n";
    let head_end = response
        .windows(separator.len())
        .position(|w| w == separator)
        .ok_or_else(|| anyhow::anyhow!("chunked response missing CRLFCRLF"))?;
    let head_text = std::str::from_utf8(&response[..head_end])
        .map_err(|_| anyhow::anyhow!("chunked head not ASCII"))?;
    let status_ok = head_text.starts_with("HTTP/1.1 200")
        && head_text.contains("Transfer-Encoding: chunked\r\n");
    let body_wire = &response[head_end + separator.len()..];
    let decoded =
        decode_chunked(body_wire).map_err(|e| anyhow::anyhow!("chunked decode failed: {e}"))?;
    Ok((body_wire.len(), decoded.len(), status_ok))
}
