use std::convert::Infallible;
use std::net::SocketAddr;
use std::time::Duration;

use prost::Message;
use tina::prelude::*;
use tina_http::{
    GrpcBufferedServerStreamingResponse, GrpcBufferedStreamLimits, GrpcClient,
    GrpcClientStreamingRequest, GrpcLimits, GrpcRequest, GrpcRequestStream, GrpcResponse,
    GrpcRouter, GrpcRouterMsg, GrpcStatus, GrpcStatusCode, GrpcStreamReply, GrpcStreamingCall,
    GrpcStreamingResponse, GrpcUnaryOutcome, Http2ClientConnection, Http2ClientLimits,
    Http2ClientMsg, Http2Listener, Http2ListenerMsg, Http2ServerConfig, Http2Target,
    ResponseChunkMsg, ResponseChunkReply, grpc_stream_finish, grpc_stream_message,
};
use tina_runtime::{CallOutcome, DefaultThreadedMailboxFactory, LocalSystem};

const ACTOR_ROUTE_CAPACITY: usize = 16;

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
    app: Option<LocalSystem<SpecimenShard, DefaultThreadedMailboxFactory>>,
    listener: Address<Http2ListenerMsg>,
}

impl SpecimenServer {
    pub fn shutdown(mut self) -> Result<(), String> {
        self.shutdown_inner()
    }

    fn shutdown_inner(&mut self) -> Result<(), String> {
        let Some(app) = self.app.take() else {
            return Ok(());
        };
        let listener_stop = app
            .try_send(self.listener, Http2ListenerMsg::Stop)
            .map_err(|error| format!("stop listener: {error:?}"));
        let runtime_shutdown = app
            .shutdown()
            .join()
            .map_err(|error| format!("shutdown: {error}"))
            .and_then(|report| {
                report
                    .ensure_clean()
                    .map_err(|error| format!("shutdown: {error}"))
            });
        listener_stop?;
        runtime_shutdown
    }

    /// Exercise the native gRPC client (no Tokio, no blocking helper):
    /// one unary OK call, one non-OK status call, and one client
    /// cancellation, all over a single `Http2ClientConnection` isolate.
    /// This is the copied path users should follow.
    pub fn native_grpc_smoke(&self) -> Result<NativeGrpcSmoke, String> {
        let app = self
            .app
            .as_ref()
            .ok_or_else(|| "server already shut down".to_owned())?;

        // One connection isolate carries every call below.
        let target = Http2Target::H2c {
            authority: "specimen".into(),
            addr: self.addr,
        };
        let conn = app
            .register_root::<Http2ClientConnection<SpecimenShard>, _>(
                Http2ClientConnection::<SpecimenShard>::new(target, Http2ClientLimits::default())
                    .map_err(|error| format!("HTTP/2 client config: {error}"))?,
                32,
            )
            .map_err(|error| format!("register connection: {error:?}"))?;
        app.try_send(conn, Http2ClientMsg::Begin)
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
            app,
            &client,
            "/specimen.Counter/Increment",
            &CounterRequest { delta: 7 },
        )? {
            GrpcUnaryOutcome::Ok(reply) => reply.value,
            other => return Err(format!("Increment: expected Ok, got {other:?}")),
        };

        // 2. Non-OK gRPC status is the caller outcome, not a success.
        let forbidden_status = match unary_call::<CounterReply>(
            app,
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
                let _ = app.try_send(conn, Http2ClientMsg::Cancel { stream_id: 5 });
            });
            let submit = client
                .unary_request("/specimen.Counter/Forbidden", &CounterRequest { delta: 0 })
                .map_err(|error| format!("encode cancel request: {error:?}"))?;
            let reply = app
                .call_blocking(client.connection(), submit, Duration::from_secs(2))
                .map_err(|error| format!("cancel call: {error:?}"))?;
            canceller
                .join()
                .map_err(|_| "cancellation thread panicked".to_owned())?;
            Ok::<String, String>(format!("{reply:?}"))
        })?;

        // Connection survives cancellation: a final call still completes.
        match unary_call::<CounterReply>(
            app,
            &client,
            "/specimen.Counter/Forbidden",
            &CounterRequest { delta: 0 },
        )? {
            GrpcUnaryOutcome::Status(_) => {}
            other => return Err(format!("post-cancel call: expected Status, got {other:?}")),
        }

        app.try_send(conn, Http2ClientMsg::Stop)
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
    requests: GrpcRequestStream<CounterRequest>,
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
        self.requests.pull_next_effect(Duration::from_secs(2))
    }

    fn handle_request_chunk_outcome(
        &mut self,
        outcome: tina_runtime::CallOutcome<tina_http::Http2ConnectionReply>,
    ) -> Effect<Self> {
        let Some(pending) = self.pending.take() else {
            return noop();
        };
        let reply = self.requests.accept_http2_outcome(outcome);
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
                let reply = self.requests.next_buffered();
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

#[derive(Default)]
struct CounterService {
    value: u64,
}

#[tina_runtime::isolate(
    request = GrpcRequest<CounterRequest>,
    reply = Result<GrpcResponse<CounterReply>, GrpcStatus>,
    shard = SpecimenShard
)]
impl CounterService {
    fn handle_request(
        &mut self,
        request: GrpcRequest<CounterRequest>,
        call: RequestCall<'_, Self>,
    ) -> RequestEffect<Self> {
        match request.path() {
            "/specimen.Counter/Increment" => {
                self.value += request.message.delta;
                call.reply(Ok(GrpcResponse::new(CounterReply { value: self.value })))
            }
            "/specimen.Counter/Forbidden" => call.reply(Err(GrpcStatus::with_message(
                GrpcStatusCode::PermissionDenied,
                "counter is read-only",
            ))),
            _ => call.reply(Err(GrpcStatus::new(GrpcStatusCode::Unimplemented))),
        }
    }
}

type StreamingRouteReply = Result<GrpcStreamingResponse<CounterReply>, GrpcStatus>;

#[derive(Debug)]
enum StreamingFactoryEvent {
    Spawned {
        request: tina::RequestContext<StreamingRouteReply>,
        result: tina::SpawnObservedResult<ResponseChunkMsg, ResponseChunkReply>,
    },
}

struct StreamingFactory {
    limits: GrpcLimits,
}

#[tina_runtime::isolate(
    event = StreamingFactoryEvent,
    request = GrpcStreamingCall<CounterRequest, CounterReply>,
    reply = StreamingRouteReply,
    shard = SpecimenShard,
    send = Outbound<ResponseChunkMsg>,
    spawn_observed = tina::SpawnObserved<
        ChildDefinition<StreamingEchoSource>,
        tina::ServiceMessage<
            StreamingFactoryEvent,
            GrpcStreamingCall<CounterRequest, CounterReply>
        >,
        ResponseChunkMsg,
        ResponseChunkReply
    >,
)]
impl StreamingFactory {
    fn handle_event(
        &mut self,
        event: StreamingFactoryEvent,
        _ctx: &mut Context<'_, SpecimenShard, Self::Reply>,
    ) -> Effect<Self> {
        match event {
            StreamingFactoryEvent::Spawned { request, result } => match result {
                Ok(child) if request.is_open() => {
                    reply_to(request, Ok(GrpcStreamingResponse::new(child.address)))
                }
                Ok(child) => send(child.address, ResponseChunkMsg::Cancel),
                Err(error) => reply_to(
                    request,
                    Err(GrpcStatus::with_message(
                        GrpcStatusCode::ResourceExhausted,
                        format!("stream source spawn failed: {error:?}"),
                    )),
                ),
            },
        }
    }

    fn handle_request(
        &mut self,
        request: GrpcStreamingCall<CounterRequest, CounterReply>,
        call: RequestCall<'_, Self>,
    ) -> RequestEffect<Self> {
        let source = StreamingEchoSource {
            requests: request.requests,
            pending: None,
            eof: false,
            limits: self.limits,
        };
        call.capture(|request| {
            spawn_observed(ChildDefinition::new(source, 16)).then_service_event(move |result| {
                StreamingFactoryEvent::Spawned { request, result }
            })
        })
    }
}

pub fn start_server() -> anyhow::Result<SpecimenServer> {
    let app = LocalSystem::single_shard(SpecimenShard, DefaultThreadedMailboxFactory)
        .ingress_capacity(64)
        .idle_wait(Duration::from_millis(1))
        .try_build()?;

    let limits = GrpcLimits {
        max_message_bytes: 1024,
        ..Default::default()
    };
    let counter = app
        .register_request_service::<CounterService, GrpcRequest<CounterRequest>, Infallible>(
            CounterService::default(),
            16,
        )
        .map_err(|error| anyhow::anyhow!("register counter service: {error:?}"))?;
    let streaming = app
        .register_split_service::<
            StreamingFactory,
            StreamingFactoryEvent,
            GrpcStreamingCall<CounterRequest, CounterReply>,
            ResponseChunkMsg,
        >(StreamingFactory { limits }, 16)
        .map_err(|error| anyhow::anyhow!("register streaming factory: {error:?}"))?
        .requests;

    let router = GrpcRouter::<SpecimenShard>::new(limits)
        .with_actor_route_capacity(ACTOR_ROUTE_CAPACITY)?
        .try_unary_actor(
            "/specimen.Counter/Increment",
            counter,
            Duration::from_secs(2),
        )?
        .try_unary_actor(
            "/specimen.Counter/Forbidden",
            counter,
            Duration::from_secs(2),
        )?
        .server_streaming_buffered(
            "/specimen.Counter/Watch",
            move |_request: GrpcRequest<CounterRequest>| {
                GrpcBufferedServerStreamingResponse::from_messages(
                    [CounterReply { value: 41 }, CounterReply { value: 42 }],
                    GrpcBufferedStreamLimits::new(limits, 2, 1024),
                )
                .map_err(|error| {
                    GrpcStatus::with_message(
                        GrpcStatusCode::ResourceExhausted,
                        format!("watch response exceeds bounds: {error:?}"),
                    )
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
        .try_streaming_actor("/specimen.Counter/Chat", streaming, Duration::from_secs(2))?;

    let service = app
        .register_root::<GrpcRouter<SpecimenShard>, _>(router, 16)
        .map_err(|error| anyhow::anyhow!("register router: {error:?}"))?;
    let config = Http2ServerConfig::default();
    let listener = app
        .register_root::<Http2Listener<SpecimenShard, GrpcRouterMsg>, _>(
            Http2Listener::<SpecimenShard, GrpcRouterMsg>::new(
                "127.0.0.1:0".parse::<SocketAddr>().expect("loopback"),
                service,
                config,
            )
            .map_err(|error| anyhow::anyhow!("HTTP/2 server config: {error}"))?,
            config.listener_mailbox_capacity,
        )
        .map_err(|error| anyhow::anyhow!("register listener: {error:?}"))?;

    let bound = app.observe_next_bound()?;
    app.try_send(listener, Http2ListenerMsg::Start)
        .map_err(|error| anyhow::anyhow!("start listener: {error:?}"))?;
    let addr = bound
        .wait(Duration::from_secs(2))
        .map_err(|error| anyhow::anyhow!("listener did not publish bound address: {error:?}"))?;

    Ok(SpecimenServer {
        addr,
        app: Some(app),
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
    app: &LocalSystem<SpecimenShard, DefaultThreadedMailboxFactory>,
    client: &GrpcClient,
    path: &str,
    request: &CounterRequest,
) -> Result<GrpcUnaryOutcome<Resp>, String> {
    let submit = client
        .unary_request(path, request)
        .map_err(|error| format!("encode {path}: {error:?}"))?;
    let CallOutcome::Replied(reply) = app
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
