//! Tiny Axum app where a handler calls into a Tina service through
//! [`TinaService`].
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
use tina_runtime::{DefaultThreadedMailboxFactory, LocalSystem};
use tina_tokio_bridge::{BridgeError, BridgeHost, BridgeRequest};
use tina_tower_bridge::{Service, TinaService, TinaTowerService};

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

#[tina::isolate(message = BridgeRequest<BrushRequest, BrushReply>)]
impl Counter {
    fn handle(
        &mut self,
        msg: BridgeRequest<BrushRequest, BrushReply>,
        _ctx: &mut Context<'_, SingleShard, Self::Reply>,
    ) -> Effect<Self> {
        let (_, responder) = msg.into_parts();
        self.brushes += 1;
        let _ = responder.respond(BrushReply {
            brushes: self.brushes,
        });
        noop()
    }
}

type CounterService = TinaService<BrushRequest, BrushReply>;

async fn brush(State(svc): State<CounterService>) -> Result<String, StatusCode> {
    let mut svc = svc;
    match svc.call(BrushRequest).await {
        Ok(reply) => Ok(format!("brushed {}\n", reply.brushes)),
        Err(BridgeError::Full) | Err(BridgeError::Closed) | Err(BridgeError::UnknownShard(_)) => {
            Err(StatusCode::SERVICE_UNAVAILABLE)
        }
        Err(BridgeError::Timeout) => Err(StatusCode::GATEWAY_TIMEOUT),
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let tina_app = LocalSystem::single_shard(SingleShard, DefaultThreadedMailboxFactory)
        .ingress_capacity(16)
        .idle_wait(Duration::from_millis(1))
        .try_build()?;
    let mut host = BridgeHost::from_app(tina_app);
    let bridge = host
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
    let serve_result = axum::serve(listener, app).await;
    let shutdown_result = host.drain_and_shutdown(Duration::from_secs(2));
    serve_result?;
    let shutdown = shutdown_result.map_err(|error| format!("shutdown bridge host: {error:?}"))?;
    if !shutdown.drained_within_timeout {
        return Err(format!(
            "bridge host still had {} handles after drain timeout",
            shutdown.outstanding_handles_at_shutdown
        )
        .into());
    }
    Ok(())
}
