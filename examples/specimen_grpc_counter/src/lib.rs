use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use prost::Message;
use tina::prelude::*;
use tina_http::{
    GrpcClient, GrpcClientStreamingRequest, GrpcLimits, GrpcRequest, GrpcRequestStream,
    GrpcResponse, GrpcRouter, GrpcRouterMsg, GrpcServerStreamingResponse, GrpcStatus,
    GrpcStatusCode, GrpcStreamReply, GrpcStreamingCall, GrpcStreamingResponse, GrpcUnaryOutcome,
    Http2ClientConnection, Http2ClientLimits, Http2ClientMsg, Http2Listener, Http2ListenerMsg,
    Http2ServerConfig, Http2Target, ResponseChunkMsg, ResponseChunkReply, grpc_stream_finish,
    grpc_stream_message,
};
use tina_runtime::{
    CallOutcome, DefaultThreadedMailboxFactory, ThreadedRuntime, ThreadedRuntimeConfig,
};

const STREAMING_SOURCE_CAPACITY: usize = 16;

#[derive(Clone, PartialEq, Message)]
pub struct CounterRequest {
    #[prost(uint64, tag = "1")]
    pub delta: u64,
}

#[derive(Clone, PartialEq, Message)]
pub struct CounterReply {
    #[prost(uint64, tag = "1")]
    pub value: u64,
}

#[derive(Debug, Default)]
pub struct SpecimenShard;

impl Shard for SpecimenShard {
    fn id(&self) -> ShardId {
        ShardId::new(570)
    }
}

pub struct SpecimenServer {
    pub addr: SocketAddr,
    runtime: Option<ThreadedRuntime<SpecimenShard, DefaultThreadedMailboxFactory>>,
    listener: Address<Http2ListenerMsg>,
}

impl SpecimenServer {
    pub fn shutdown(mut self) -> Result<(), String> {
        self.shutdown_inner()
    }

    fn shutdown_inner(&mut self) -> Result<(), String> {
        let Some(runtime) = self.runtime.take() else {
            return Ok(());
        };
        let listener_stop = runtime
            .try_send(self.listener, Http2ListenerMsg::Stop)
            .map_err(|error| format!("stop listener: {error:?}"));
        let runtime_shutdown = runtime
            .shutdown_report()
            .ensure_clean()
            .map_err(|error| format!("shutdown: {error}"));
        listener_stop?;
        runtime_shutdown
    }

    /// Exercise the native gRPC client (no Tokio, no blocking helper):
    /// one unary OK call, one non-OK status call, and one client
    /// cancellation, all over a single `Http2ClientConnection` isolate.
    /// This is the copied path users should follow.
    pub fn native_grpc_smoke(&self) -> Result<NativeGrpcSmoke, String> {
        let runtime = self
            .runtime
            .as_ref()
            .ok_or_else(|| "server already shut down".to_owned())?;

        // One connection isolate carries every call below.
        let target = Http2Target::H2c {
            authority: "specimen".into(),
            addr: self.addr,
        };
        let conn = runtime
            .register_with_capacity::<Http2ClientConnection<SpecimenShard>, _>(
                Http2ClientConnection::<SpecimenShard>::new(target, Http2ClientLimits::default())
                    .map_err(|error| format!("HTTP/2 client config: {error}"))?,
                32,
            )
            .map_err(|error| format!("register connection: {error:?}"))?;
        runtime
            .try_send(conn, Http2ClientMsg::Begin)
            .map_err(|error| format!("begin connection: {error:?}"))?;
        let client = GrpcClient::new(
            conn,
            GrpcLimits {
                max_message_bytes: 1024,
                ..Default::default()
            },
        );

        // 1. Unary OK — the response message is decoded only because the
        //    status was OK.
        let increment_value = match unary_call::<CounterReply>(
            runtime,
            &client,
            "/specimen.Counter/Increment",
            &CounterRequest { delta: 7 },
        )? {
            GrpcUnaryOutcome::Ok(reply) => reply.value,
            other => return Err(format!("Increment: expected Ok, got {other:?}")),
        };

        // 2. Non-OK gRPC status is the caller outcome, not a success.
        let forbidden_status = match unary_call::<CounterReply>(
            runtime,
            &client,
            "/specimen.Counter/Forbidden",
            &CounterRequest { delta: 0 },
        )? {
            GrpcUnaryOutcome::Status(status) => status.code,
            other => return Err(format!("Forbidden: expected Status, got {other:?}")),
        };

        // 3. Client cancellation: a second thread cancels the in-flight
        //    stream. The server here is fast, so the call may complete
        //    before the cancel lands — we tolerate either outcome and
        //    only require that the connection survives (proven by the
        //    follow-up call below).
        let cancel_outcome = std::thread::scope(|scope| {
            let canceller = scope.spawn(move || {
                std::thread::sleep(Duration::from_millis(5));
                // Streams 1 and 3 were used above; this call is stream 5.
                let _ = runtime.try_send(conn, Http2ClientMsg::Cancel { stream_id: 5 });
            });
            let submit = client
                .unary_request("/specimen.Counter/Forbidden", &CounterRequest { delta: 0 })
                .map_err(|error| format!("encode cancel request: {error:?}"))?;
            let reply = runtime
                .call_blocking(client.connection(), submit, Duration::from_secs(2))
                .map_err(|error| format!("cancel call: {error:?}"))?;
            canceller
                .join()
                .map_err(|_| "cancellation thread panicked".to_owned())?;
            Ok::<String, String>(format!("{reply:?}"))
        })?;

        // Connection survives cancellation: a final call still completes.
        match unary_call::<CounterReply>(
            runtime,
            &client,
            "/specimen.Counter/Forbidden",
            &CounterRequest { delta: 0 },
        )? {
            GrpcUnaryOutcome::Status(_) => {}
            other => return Err(format!("post-cancel call: expected Status, got {other:?}")),
        }

        runtime
            .try_send(conn, Http2ClientMsg::Stop)
            .map_err(|error| format!("stop native gRPC connection: {error:?}"))?;
        Ok(NativeGrpcSmoke {
            increment_value,
            forbidden_status,
            cancel_outcome,
        })
    }
}

impl Drop for SpecimenServer {
    fn drop(&mut self) {
        let _ = self.shutdown_inner();
    }
}

struct StreamingEchoSource {
    stream_slot: Arc<Mutex<Option<GrpcRequestStream<CounterRequest>>>>,
    pending: Option<tina::RequestContext<ResponseChunkReply>>,
    eof: bool,
    limits: GrpcLimits,
}

impl StreamingEchoSource {
    fn finish_with_status(&mut self, status: GrpcStatus) -> ResponseChunkReply {
        self.eof = true;
        grpc_stream_finish(status)
    }

    fn reply_for_message(&mut self, request: CounterRequest) -> ResponseChunkReply {
        grpc_stream_message(
            &CounterReply {
                value: request.delta,
            },
            self.limits,
        )
        .unwrap_or_else(|_| {
            self.finish_with_status(GrpcStatus::new(GrpcStatusCode::ResourceExhausted))
        })
    }

    fn pull_request(&self) -> Effect<Self> {
        self.stream_slot
            .lock()
            .expect("streaming stream slot")
            .as_ref()
            .expect("streaming request stream installed")
            .pull_next_effect(Duration::from_secs(2))
    }

    fn handle_request_chunk_outcome(
        &mut self,
        outcome: tina_runtime::CallOutcome<tina_http::Http2ConnectionReply>,
    ) -> Effect<Self> {
        let Some(pending) = self.pending.take() else {
            return noop();
        };
        let reply = {
            let mut guard = self.stream_slot.lock().expect("streaming stream slot");
            let requests = guard.as_mut().expect("streaming request stream installed");
            requests.accept_http2_outcome(outcome)
        };
        self.reply_to_stream_result(pending, reply)
    }

    fn reply_to_stream_result(
        &mut self,
        pending: tina::RequestContext<ResponseChunkReply>,
        reply: GrpcStreamReply<CounterRequest>,
    ) -> Effect<Self> {
        match reply {
            GrpcStreamReply::Message(request) => reply_to(pending, self.reply_for_message(request)),
            GrpcStreamReply::NeedMore => {
                self.pending = Some(pending);
                self.pull_request()
            }
            GrpcStreamReply::Eof => {
                self.eof = true;
                reply_to(pending, ResponseChunkReply::Eof)
            }
            GrpcStreamReply::Status(status) => reply_to(pending, self.finish_with_status(status)),
            GrpcStreamReply::Cancelled => reply_to(
                pending,
                self.finish_with_status(GrpcStatus::new(GrpcStatusCode::Cancelled)),
            ),
            GrpcStreamReply::DeadlineExceeded => reply_to(
                pending,
                self.finish_with_status(GrpcStatus::new(GrpcStatusCode::DeadlineExceeded)),
            ),
        }
    }
}

#[tina_runtime::isolate(
    message = ResponseChunkMsg,
    reply = ResponseChunkReply,
    shard = SpecimenShard
)]
impl StreamingEchoSource {
    fn handle(
        &mut self,
        msg: ResponseChunkMsg,
        _ctx: &mut Context<'_, SpecimenShard, Self::Reply>,
    ) -> Effect<Self> {
        match msg {
            ResponseChunkMsg::Cancel => stop(),
            ResponseChunkMsg::Next => reply(ResponseChunkReply::Eof),
            ResponseChunkMsg::Http2RequestChunk(outcome) => {
                self.handle_request_chunk_outcome(outcome)
            }
        }
    }

    fn handle_call(
        &mut self,
        msg: ResponseChunkMsg,
        call: tina::CallContext<'_, Self>,
    ) -> Effect<Self> {
        match msg {
            ResponseChunkMsg::Cancel => stop(),
            ResponseChunkMsg::Next => {
                if self.eof {
                    return call.reply(ResponseChunkReply::Eof);
                }
                let reply = {
                    let mut guard = self.stream_slot.lock().expect("streaming stream slot");
                    guard
                        .as_mut()
                        .expect("streaming request stream installed")
                        .next_buffered()
                };
                if !matches!(reply, GrpcStreamReply::NeedMore) {
                    return self.reply_to_stream_result(call.into_request_context(), reply);
                }
                self.pending = Some(call.into_request_context());
                self.pull_request()
            }
            ResponseChunkMsg::Http2RequestChunk(outcome) => {
                self.handle_request_chunk_outcome(outcome)
            }
        }
    }
}

type StreamingEchoSlot = Arc<Mutex<Option<GrpcRequestStream<CounterRequest>>>>;
type StreamingEchoAddress = tina::Address<ResponseChunkMsg, ResponseChunkReply>;

struct StreamingEchoSourcePool {
    available: Mutex<Vec<(StreamingEchoSlot, StreamingEchoAddress)>>,
}

impl StreamingEchoSourcePool {
    fn register(
        runtime: &ThreadedRuntime<SpecimenShard, DefaultThreadedMailboxFactory>,
        limits: GrpcLimits,
    ) -> Result<Arc<Self>, String> {
        let pool = Arc::new(Self {
            available: Mutex::new(Vec::with_capacity(STREAMING_SOURCE_CAPACITY)),
        });
        for _ in 0..STREAMING_SOURCE_CAPACITY {
            let stream_slot = Arc::new(Mutex::new(None));
            let source = runtime
                .register_with_capacity::<StreamingEchoSource, Infallible>(
                    StreamingEchoSource {
                        stream_slot: Arc::clone(&stream_slot),
                        pending: None,
                        eof: false,
                        limits,
                    },
                    16,
                )
                .map_err(|error| format!("register streaming source: {error:?}"))?;
            pool.available
                .lock()
                .expect("streaming source pool")
                .push((stream_slot, source));
        }
        Ok(pool)
    }

    fn claim(
        &self,
        request_stream: GrpcRequestStream<CounterRequest>,
    ) -> Result<GrpcStreamingResponse<CounterReply>, GrpcStatus> {
        let (stream_slot, source) = self
            .available
            .lock()
            .expect("streaming source pool")
            .pop()
            .ok_or_else(|| {
                GrpcStatus::with_message(
                    GrpcStatusCode::ResourceExhausted,
                    format!("streaming source pool capacity {STREAMING_SOURCE_CAPACITY} exhausted"),
                )
            })?;
        *stream_slot.lock().expect("streaming stream slot") = Some(request_stream);
        Ok(GrpcStreamingResponse::new(source))
    }
}

pub fn start_server() -> anyhow::Result<SpecimenServer> {
    let runtime = ThreadedRuntime::try_with_config(
        SpecimenShard,
        DefaultThreadedMailboxFactory,
        ThreadedRuntimeConfig {
            command_capacity: 64,
            idle_wait: Duration::from_millis(1),
            ..Default::default()
        },
    )?;

    let state = Arc::new(Mutex::new(0_u64));
    let router_state = Arc::clone(&state);
    let streaming_sources = StreamingEchoSourcePool::register(
        &runtime,
        GrpcLimits {
            max_message_bytes: 1024,
            ..Default::default()
        },
    )
    .map_err(anyhow::Error::msg)?;
    let streaming_sources_for_route = Arc::clone(&streaming_sources);
    let watch_responses = Arc::new(Mutex::new(Vec::new()));
    for _ in 0..16 {
        let watch_response = GrpcServerStreamingResponse::from_messages(
            &runtime,
            vec![CounterReply { value: 41 }, CounterReply { value: 42 }],
            GrpcLimits {
                max_message_bytes: 1024,
                ..Default::default()
            },
            16,
        )
        .map_err(|error| anyhow::anyhow!("register watch response: {error:?}"))?;
        watch_responses
            .lock()
            .expect("watch responses")
            .push(watch_response);
    }
    let watch_responses_for_route = Arc::clone(&watch_responses);
    let router = GrpcRouter::<SpecimenShard>::new(GrpcLimits {
        max_message_bytes: 1024,
        ..Default::default()
    })
    .unary(
        "/specimen.Counter/Increment",
        move |request: GrpcRequest<CounterRequest>| {
            let mut value = router_state.lock().expect("counter lock");
            *value += request.message.delta;
            Ok(GrpcResponse::new(CounterReply { value: *value }))
        },
    )
    .unary(
        "/specimen.Counter/Forbidden",
        |_request: GrpcRequest<CounterRequest>| {
            // Always a non-OK gRPC status, so the native client demo can
            // show that a status is the caller outcome, not a success.
            Err::<GrpcResponse<CounterReply>, _>(GrpcStatus::with_message(
                GrpcStatusCode::PermissionDenied,
                "counter is read-only",
            ))
        },
    )
    .server_streaming(
        "/specimen.Counter/Watch",
        move |_request: GrpcRequest<CounterRequest>| {
            watch_responses_for_route
                .lock()
                .expect("watch responses")
                .pop()
                .ok_or_else(|| {
                    tina_http::GrpcStatus::new(tina_http::GrpcStatusCode::ResourceExhausted)
                })
        },
    )
    .client_streaming(
        "/specimen.Counter/Sum",
        |request: GrpcClientStreamingRequest<CounterRequest>| {
            Ok(GrpcResponse::new(CounterReply {
                value: request.messages.iter().map(|message| message.delta).sum(),
            }))
        },
    )
    .streaming(
        "/specimen.Counter/Chat",
        move |request: GrpcStreamingCall<CounterRequest, CounterReply>| {
            streaming_sources_for_route.claim(request.requests)
        },
    );

    let service = runtime
        .register_with_capacity::<GrpcRouter<SpecimenShard>, _>(router, 16)
        .map_err(|error| anyhow::anyhow!("register router: {error:?}"))?;
    let config = Http2ServerConfig::default();
    let listener = runtime
        .register_with_capacity::<Http2Listener<SpecimenShard, GrpcRouterMsg>, _>(
            Http2Listener::<SpecimenShard, GrpcRouterMsg>::new(
                "127.0.0.1:0".parse::<SocketAddr>().expect("loopback"),
                service,
                config,
            )
            .map_err(|error| anyhow::anyhow!("HTTP/2 server config: {error}"))?,
            config.listener_mailbox_capacity,
        )
        .map_err(|error| anyhow::anyhow!("register listener: {error:?}"))?;

    let bound = runtime.observe_next_bound()?;
    runtime
        .try_send(listener, Http2ListenerMsg::Start)
        .map_err(|error| anyhow::anyhow!("start listener: {error:?}"))?;
    let addr = bound
        .wait(Duration::from_secs(2))
        .map_err(|error| anyhow::anyhow!("listener did not publish bound address: {error:?}"))?;

    Ok(SpecimenServer {
        addr,
        runtime: Some(runtime),
        listener,
    })
}

/// Summary of one native gRPC client run.
#[derive(Debug, Clone)]
pub struct NativeGrpcSmoke {
    /// Value returned by the OK unary `Increment` call.
    pub increment_value: u64,
    /// Status code returned by the non-OK `Forbidden` call.
    pub forbidden_status: GrpcStatusCode,
    /// Debug rendering of the cancelled call's outcome (Replied or
    /// LocalCancel, depending on the race).
    pub cancel_outcome: String,
}

/// Issue one unary gRPC call through the native client and decode the
/// typed outcome. The copied path: build the submit, call the
/// connection, fold the reply.
fn unary_call<Resp: prost::Message + Default>(
    runtime: &ThreadedRuntime<SpecimenShard, DefaultThreadedMailboxFactory>,
    client: &GrpcClient,
    path: &str,
    request: &CounterRequest,
) -> Result<GrpcUnaryOutcome<Resp>, String> {
    let submit = client
        .unary_request(path, request)
        .map_err(|error| format!("encode {path}: {error:?}"))?;
    let CallOutcome::Replied(reply) = runtime
        .call_blocking(client.connection(), submit, Duration::from_secs(2))
        .map_err(|error| format!("call {path}: {error:?}"))?
    else {
        return Err(format!("call {path}: host timed out or target gone"));
    };
    Ok(client.unary_outcome_from_reply::<Resp>(reply))
}

/// Smoke entry point. Uses the **native gRPC client** (the copied path),
/// not `grpc_unary_call_h2c_blocking`. Returns the OK increment value so
/// the smoke test can pin it; the non-OK status and client cancellation
/// are exercised inside [`SpecimenServer::native_grpc_smoke`].
pub fn run_smoke() -> anyhow::Result<u64> {
    let server = start_server()?;
    let smoke = server.native_grpc_smoke().map_err(anyhow::Error::msg)?;
    if smoke.forbidden_status != GrpcStatusCode::PermissionDenied {
        anyhow::bail!(
            "expected PermissionDenied from Forbidden, got {:?}",
            smoke.forbidden_status
        );
    }
    server.shutdown().map_err(anyhow::Error::msg)?;
    Ok(smoke.increment_value)
}
