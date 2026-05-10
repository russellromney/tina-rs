//! Tina side: an `IterBodySource` wraps a closure-iterator into a
//! chunk source — no custom `Isolate` impl. The service routes
//! `/big` to a known-length stream and `/big-chunked` to an
//! unknown-length stream framed by `Transfer-Encoding: chunked`.
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
use tina_runtime::{DefaultThreadedMailboxFactory, ThreadedRuntime};

use crate::{CHUNK_BYTES, RESPONSE_BODY_BYTES, Report, slow_reader_client};

/// Service: `/big` returns a known-length stream, `/big-chunked`
/// returns a chunked stream. Both pull from chunk sources the
/// caller supplies — same producer, different framing.
struct StreamingService {
    known_source: Address<ResponseChunkMsg, ResponseChunkReply>,
    chunked_source: Address<ResponseChunkMsg, ResponseChunkReply>,
}

impl Isolate for StreamingService {
    tina::isolate_types! {
        message: HttpRequest,
        reply: HttpResponse,
        send: tina::Outbound<Infallible>,
        spawn: Infallible,
        call: Infallible,
        shard: SingleShard,
    }

    fn handle(
        &mut self,
        request: HttpRequest,
        _ctx: &mut Context<'_, SingleShard, Self::Reply>,
    ) -> Effect<Self> {
        let response = match request.path.as_str() {
            "/big" => HttpResponse::stream_known_length(
                StatusCode::OK,
                RESPONSE_BODY_BYTES,
                self.known_source,
            ),
            "/big-chunked" => HttpResponse::stream_chunked(StatusCode::OK, self.chunked_source),
            _ => HttpResponse::not_found(),
        };
        reply(response)
    }
}

/// Iterator that yields exactly enough chunks to cover
/// `RESPONSE_BODY_BYTES`. Each chunk is `CHUNK_BYTES` of the same
/// filler byte, except the last which may be shorter.
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
    let runtime = ThreadedRuntime::new(SingleShard, DefaultThreadedMailboxFactory);
    let metrics = BodyMetrics::new();

    let known_source = runtime
        .register_with_capacity::<IterBodySource<SingleShard>, Infallible>(
            IterBodySource::new(body_chunks()),
            16,
        )
        .map_err(|e| anyhow::anyhow!("register known source: {e:?}"))?;
    let chunked_source = runtime
        .register_with_capacity::<IterBodySource<SingleShard>, Infallible>(
            IterBodySource::new(body_chunks()),
            16,
        )
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

    let (bytes, ok, wall_ms) = slow_reader_client(server_addr);

    runtime
        .try_send(listener, HttpListenerMsg::Stop)
        .map_err(|e| anyhow::anyhow!("send Stop: {e:?}"))?;
    let _ = runtime.shutdown();

    let snap = metrics.snapshot();
    Ok(Report {
        bytes_received: bytes,
        status_ok: ok,
        wall_clock_ms: wall_ms,
        exit_clean: snap.drained(),
        tokio_response_alloc_floor: None,
        tina_response_high_water: Some(snap.response_body_high_water),
    })
}
