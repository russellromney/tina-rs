//! Tiny Axum app where a handler calls into a Tina service through
//! [`TinaTowerService`].
//!
//! Run with:
//!
//! ```text
//! cargo run --example axum_counter -p tina-tower-bridge
//! curl -X POST http://127.0.0.1:8080/brush
//! ```

use std::convert::Infallible;
use std::time::Duration;

use axum::Router;
use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::post;
use tina::prelude::*;
use tina_runtime::{DefaultThreadedMailboxFactory, ThreadedRuntimeConfig};
use tina_tokio_bridge::{BridgeError, BridgeHandle, BridgeHost, BridgeMessage, BridgeRequest};
use tina_tower_bridge::TinaTowerService;
use tower_service::Service;

#[derive(Debug, Default)]
struct Counter {
    brushes: u64,
}

#[derive(Debug, Clone)]
struct BrushRequest;

#[derive(Debug, Clone)]
struct BrushReply {
    brushes: u64,
}

#[derive(Debug)]
enum CounterMsg {
    Request(BridgeRequest<BrushRequest, BrushReply>),
}

impl From<BridgeRequest<BrushRequest, BrushReply>> for CounterMsg {
    fn from(value: BridgeRequest<BrushRequest, BrushReply>) -> Self {
        Self::Request(value)
    }
}

impl BridgeMessage for CounterMsg {
    fn bridge_cancelled(&self) -> bool {
        match self {
            Self::Request(req) => req.bridge_cancelled(),
        }
    }
}

#[tina_runtime::isolate(message = CounterMsg)]
impl Counter {
    fn handle(&mut self, msg: CounterMsg, _ctx: &mut Context<'_, SingleShard>) -> Effect<Self> {
        match msg {
            CounterMsg::Request(req) => {
                let (_, responder) = req.into_parts();
                self.brushes += 1;
                let _ = responder.respond(BrushReply {
                    brushes: self.brushes,
                });
                noop()
            }
        }
    }
}

type CounterBridge = BridgeHandle<
    BrushRequest,
    BrushReply,
    SingleShard,
    DefaultThreadedMailboxFactory,
    (),
    CounterMsg,
>;
type CounterService = TinaTowerService<
    BrushRequest,
    BrushReply,
    SingleShard,
    DefaultThreadedMailboxFactory,
    (),
    CounterMsg,
>;

async fn brush(State(svc): State<CounterService>) -> Result<String, StatusCode> {
    let mut svc = svc;
    match svc.call(BrushRequest).await {
        Ok(reply) => Ok(format!("brushed {}\n", reply.brushes)),
        Err(BridgeError::Full) => Err(StatusCode::SERVICE_UNAVAILABLE),
        Err(BridgeError::Closed) => Err(StatusCode::SERVICE_UNAVAILABLE),
        Err(BridgeError::Timeout) => Err(StatusCode::GATEWAY_TIMEOUT),
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let host = BridgeHost::new(
        SingleShard,
        DefaultThreadedMailboxFactory,
        ThreadedRuntimeConfig {
            command_capacity: 16,
            idle_wait: Duration::from_millis(1),
            ..Default::default()
        },
    );
    let bridge: CounterBridge = host
        .register_bridge::<Counter, BrushRequest, BrushReply, Infallible>(
            Counter::default(),
            32,
            Duration::from_secs(2),
        )
        .map_err(|e| format!("register: {e:?}"))?;
    let svc: CounterService = TinaTowerService::new(bridge);

    let app = Router::new().route("/brush", post(brush)).with_state(svc);

    let listener = tokio::net::TcpListener::bind("127.0.0.1:8080").await?;
    println!("listening on http://{}", listener.local_addr()?);
    axum::serve(listener, app).await?;
    Ok(())
}
