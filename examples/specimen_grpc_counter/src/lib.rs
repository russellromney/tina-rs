use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use prost::Message;
use tina::prelude::*;
use tina_http::{
    GrpcClientStreamingRequest, GrpcLimits, GrpcRequest, GrpcResponse, GrpcRouter, GrpcRouterMsg,
    GrpcServerStreamingResponse, Http2Listener, Http2ListenerMsg, Http2ServerConfig,
    grpc_unary_call_h2c,
};
use tina_runtime::{DefaultThreadedMailboxFactory, ThreadedRuntime, ThreadedRuntimeConfig};

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

#[derive(Clone, PartialEq, Message)]
pub struct BlobReply {
    #[prost(bytes, tag = "1")]
    pub bytes: Vec<u8>,
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

pub fn start_server() -> Result<SpecimenServer, String> {
    start_server_on("127.0.0.1:0".parse::<SocketAddr>().expect("loopback"))
}

pub fn start_server_on(bind_addr: SocketAddr) -> Result<SpecimenServer, String> {
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
    let watch_responses = Arc::new(Mutex::new(Vec::new()));
    for delta in [40_u64, 5, 40, 5, 40, 5, 40, 5, 40, 5, 40, 5, 40, 5, 40, 5] {
        let watch_response = (
            delta,
            GrpcServerStreamingResponse::from_messages(
            &runtime,
                vec![
                    CounterReply { value: delta + 1 },
                    CounterReply { value: delta + 2 },
                ],
            GrpcLimits {
                max_message_bytes: 100_000,
            },
            16,
        )
            .map_err(|error| format!("register watch response: {error:?}"))?,
        );
        watch_responses
            .lock()
            .expect("watch responses")
            .push(watch_response);
    }
    let watch_responses_for_route = Arc::clone(&watch_responses);
    let router = GrpcRouter::<SpecimenShard>::new(GrpcLimits {
        max_message_bytes: 100_000,
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
        "/specimen.Counter/BigBlob",
        |request: GrpcRequest<CounterRequest>| {
            Ok(GrpcResponse::new(BlobReply {
                bytes: vec![7; request.message.delta as usize],
            }))
        },
    )
    .server_streaming(
        "/specimen.Counter/Watch",
        move |request: GrpcRequest<CounterRequest>| {
            let mut responses = watch_responses_for_route.lock().expect("watch responses");
            let Some(index) = responses
                .iter()
                .position(|(delta, _)| *delta == request.message.delta)
            else {
                return Err(tina_http::GrpcStatus::new(
                    tina_http::GrpcStatusCode::ResourceExhausted,
                ));
            };
            Ok(responses.swap_remove(index).1)
        },
    )
    .client_streaming(
        "/specimen.Counter/Sum",
        |request: GrpcClientStreamingRequest<CounterRequest>| {
            Ok(GrpcResponse::new(CounterReply {
                value: request.messages.iter().map(|message| message.delta).sum(),
            }))
        },
    );

    let service = runtime
        .register_with_capacity::<GrpcRouter<SpecimenShard>, _>(router, 16)
        .map_err(|error| format!("register router: {error:?}"))?;
    let config = Http2ServerConfig::default();
    let listener = runtime
        .register_with_capacity::<Http2Listener<SpecimenShard, GrpcRouterMsg>, _>(
            Http2Listener::<SpecimenShard, GrpcRouterMsg>::new(
                bind_addr,
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
    let reply: CounterReply = grpc_unary_call_h2c(
        server.addr,
        "/specimen.Counter/Increment",
        &CounterRequest { delta: 7 },
        Duration::from_secs(2),
        GrpcLimits {
            max_message_bytes: 100_000,
        },
    )
    .map_err(|error| format!("grpc call: {error:?}"))?;

    server.shutdown()?;
    Ok(reply.value)
}
