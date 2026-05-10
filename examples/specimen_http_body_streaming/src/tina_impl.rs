//! Tina side: `HttpResponse::with_stream(...)` + a `BigBody` chunk
//! source isolate. Each `Next` call from the connection isolate
//! pulls one `CHUNK_BYTES` chunk; that chunk is the only body
//! buffer resident in the connection at the time of writing.
//!
//! The `BodyMetrics` shared with the listener records the
//! response-body high-water across the whole run. Because every
//! pull is one chunk and we wait for the previous chunk to drain
//! before pulling the next, the high-water is `<= CHUNK_BYTES` —
//! never the whole `RESPONSE_BODY_BYTES`.
//!
//! Compare with [`crate::tokio_impl`] where the whole vec is
//! resident before the first byte goes on the wire.

use std::convert::Infallible;
use std::time::Duration;

use http::StatusCode;
use tina::prelude::*;
use tina_http::{
    BodyMetrics, HttpListener, HttpListenerMsg, HttpRequest, HttpResponse, HttpServerConfig,
    ResponseChunkMsg, ResponseChunkReply, ResponseStream,
};
use tina_runtime::{DefaultThreadedMailboxFactory, ThreadedRuntime};

use crate::{CHUNK_BYTES, RESPONSE_BODY_BYTES, Report, slow_reader_client};

/// Yields chunks of `CHUNK_BYTES` until `RESPONSE_BODY_BYTES` is
/// reached, then `Eof`. Filler is `b'a'`.
struct BigBody {
    yielded: usize,
}

impl Isolate for BigBody {
    tina::isolate_types! {
        message: ResponseChunkMsg,
        reply: ResponseChunkReply,
        send: tina::Outbound<Infallible>,
        spawn: Infallible,
        call: Infallible,
        shard: SingleShard,
    }

    fn handle(
        &mut self,
        _msg: ResponseChunkMsg,
        _ctx: &mut Context<'_, SingleShard, Self::Reply>,
    ) -> Effect<Self> {
        if self.yielded >= RESPONSE_BODY_BYTES {
            return reply(ResponseChunkReply::Eof);
        }
        let remaining = RESPONSE_BODY_BYTES - self.yielded;
        let take = CHUNK_BYTES.min(remaining);
        self.yielded += take;
        reply(ResponseChunkReply::Chunk(vec![b'a'; take]))
    }
}

/// Service: `/big` returns a streaming body, anything else 404.
struct StreamingService {
    body_source: Address<ResponseChunkMsg, ResponseChunkReply>,
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
        let response = if request.path == "/big" {
            HttpResponse::with_stream(
                StatusCode::OK,
                ResponseStream {
                    content_length: RESPONSE_BODY_BYTES,
                    source: self.body_source,
                },
            )
        } else {
            HttpResponse::not_found()
        };
        reply(response)
    }
}

pub fn run() -> anyhow::Result<Report> {
    let runtime = ThreadedRuntime::new(SingleShard, DefaultThreadedMailboxFactory);
    let metrics = BodyMetrics::new();

    let body_source = runtime
        .register_with_capacity::<_, Infallible>(BigBody { yielded: 0 }, 16)
        .map_err(|e| anyhow::anyhow!("register body source: {e:?}"))?;
    let service = runtime
        .register_with_capacity::<_, Infallible>(StreamingService { body_source }, 16)
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
