use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use prost::Message;
use tina::prelude::*;
use tina_http::{
    GrpcLimits, GrpcRequest, GrpcResponse, GrpcRouter, GrpcRouterMsg, Http2Listener, Http2ListenerMsg,
    Http2ServerConfig, grpc_unary_call_h2c,
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

#[derive(Debug, Default)]
pub struct SpecimenShard;

impl Shard for SpecimenShard {
    fn id(&self) -> ShardId {
        ShardId::new(570)
    }
}

pub fn run_smoke() -> Result<u64, String> {
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

    let reply: CounterReply = grpc_unary_call_h2c(
        addr,
        "/specimen.Counter/Increment",
        &CounterRequest { delta: 7 },
        Duration::from_secs(2),
        GrpcLimits {
            max_message_bytes: 1024,
        },
    )
    .map_err(|error| format!("grpc call: {error:?}"))?;

    let _ = runtime.try_send(listener, Http2ListenerMsg::Stop);
    runtime
        .shutdown()
        .map_err(|error| format!("shutdown: {error:?}"))?;
    Ok(reply.value)
}
