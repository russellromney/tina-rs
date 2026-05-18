use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use prost::Message;
use tina::prelude::*;
use tina_http::{
    GrpcClientStreamingRequest, GrpcLimits, GrpcRequest, GrpcRequestStream, GrpcResponse,
    GrpcRouter, GrpcRouterMsg, GrpcServerStreamingResponse, GrpcStatus, GrpcStatusCode,
    GrpcStreamReply, GrpcStreamingCall, GrpcStreamingResponse, Http2Listener, Http2ListenerMsg,
    Http2ServerConfig, ResponseChunkMsg, ResponseChunkReply, grpc_stream_finish,
    grpc_stream_message, grpc_unary_call_h2c_blocking,
};
use tina_runtime::{DefaultThreadedMailboxFactory, ThreadedRuntime, ThreadedRuntimeConfig};

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
        let _ = runtime.try_send(self.listener, Http2ListenerMsg::Stop);
        runtime
            .shutdown()
            .map(|_| ())
            .map_err(|error| format!("shutdown: {error:?}"))
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
        .unwrap_or_else(|_| self.finish_with_status(GrpcStatus::new(GrpcStatusCode::ResourceExhausted)))
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
            GrpcStreamReply::Message(request) => reply_to_request(pending, self.reply_for_message(request)),
            GrpcStreamReply::NeedMore => {
                self.pending = Some(pending);
                self.pull_request()
            }
            GrpcStreamReply::Eof => {
                self.eof = true;
                reply_to_request(pending, ResponseChunkReply::Eof)
            }
            GrpcStreamReply::Status(status) => reply_to_request(pending, self.finish_with_status(status)),
            GrpcStreamReply::Cancelled => reply_to_request(pending, self.finish_with_status(GrpcStatus::new(GrpcStatusCode::Cancelled))),
            GrpcStreamReply::DeadlineExceeded => reply_to_request(pending, self.finish_with_status(GrpcStatus::new(GrpcStatusCode::DeadlineExceeded))),
        }
    }
}

impl Isolate for StreamingEchoSource {
    tina::isolate_types! {
        message: ResponseChunkMsg,
        reply: ResponseChunkReply,
        send: tina::Outbound<Infallible>,
        spawn: Infallible,
        call: tina_runtime::RuntimeCall<ResponseChunkMsg>,
        shard: SpecimenShard,
    }

    fn handle(
        &mut self,
        msg: ResponseChunkMsg,
        _ctx: &mut Context<'_, SpecimenShard, Self::Reply>,
    ) -> Effect<Self> {
        match msg {
            ResponseChunkMsg::Cancel => stop(),
            ResponseChunkMsg::Next => reply(ResponseChunkReply::Eof),
            ResponseChunkMsg::Http2RequestChunk(outcome) => self.handle_request_chunk_outcome(outcome),
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
            ResponseChunkMsg::Http2RequestChunk(outcome) => self.handle_request_chunk_outcome(outcome),
        }
    }
}

struct StreamingEchoSourcePool {
    available: Mutex<
        Vec<(
            Arc<Mutex<Option<GrpcRequestStream<CounterRequest>>>>,
            tina::Address<ResponseChunkMsg, ResponseChunkReply>,
        )>,
    >,
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

pub fn start_server() -> Result<SpecimenServer, String> {
    let runtime = ThreadedRuntime::with_config(
        SpecimenShard,
        DefaultThreadedMailboxFactory,
        ThreadedRuntimeConfig {
            command_capacity: 64,
            idle_wait: Duration::from_millis(1),
            ..Default::default()
        },
    );

    let state = Arc::new(Mutex::new(0_u64));
    let router_state = Arc::clone(&state);
    let streaming_sources = StreamingEchoSourcePool::register(
        &runtime,
        GrpcLimits {
            max_message_bytes: 1024,
        },
    )?;
    let streaming_sources_for_route = Arc::clone(&streaming_sources);
    let watch_responses = Arc::new(Mutex::new(Vec::new()));
    for _ in 0..16 {
        let watch_response = GrpcServerStreamingResponse::from_messages(
            &runtime,
            vec![CounterReply { value: 41 }, CounterReply { value: 42 }],
            GrpcLimits {
                max_message_bytes: 1024,
            },
            16,
        )
        .map_err(|error| format!("register watch response: {error:?}"))?;
        watch_responses
            .lock()
            .expect("watch responses")
            .push(watch_response);
    }
    let watch_responses_for_route = Arc::clone(&watch_responses);
    let router = GrpcRouter::<SpecimenShard>::new(GrpcLimits {
        max_message_bytes: 1024,
    })
    .unary(
        "/specimen.Counter/Increment",
        move |request: GrpcRequest<CounterRequest>| {
            let mut value = router_state.lock().expect("counter lock");
            *value += request.message.delta;
            Ok(GrpcResponse::new(CounterReply { value: *value }))
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
        .map_err(|error| format!("register router: {error:?}"))?;
    let config = Http2ServerConfig::default();
    let listener = runtime
        .register_with_capacity::<Http2Listener<SpecimenShard, GrpcRouterMsg>, _>(
            Http2Listener::<SpecimenShard, GrpcRouterMsg>::new(
                "127.0.0.1:0".parse::<SocketAddr>().expect("loopback"),
                service,
                config,
            ),
            config.listener_mailbox_capacity,
        )
        .map_err(|error| format!("register listener: {error:?}"))?;

    let bound = runtime.observe_next_bound();
    runtime
        .try_send(listener, Http2ListenerMsg::Start)
        .map_err(|error| format!("start listener: {error:?}"))?;
    let addr = bound
        .wait(Duration::from_secs(2))
        .map_err(|error| format!("listener did not publish bound address: {error:?}"))?;

    Ok(SpecimenServer {
        addr,
        runtime: Some(runtime),
        listener,
    })
}

pub fn run_smoke() -> Result<u64, String> {
    let server = start_server()?;
    let reply: CounterReply = grpc_unary_call_h2c_blocking(
        server.addr,
        "/specimen.Counter/Increment",
        &CounterRequest { delta: 7 },
        Duration::from_secs(2),
        GrpcLimits {
            max_message_bytes: 1024,
        },
    )
    .map_err(|error| format!("grpc call: {error:?}"))?;

    server.shutdown()?;
    Ok(reply.value)
}
