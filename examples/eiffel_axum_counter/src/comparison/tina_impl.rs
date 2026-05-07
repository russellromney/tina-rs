use std::convert::Infallible;
use std::time::Duration;

use axum::Router;
use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::{get, post};
use tina::prelude::*;
use tina_runtime::{DefaultThreadedMailboxFactory, ThreadedRuntimeConfig};
use tina_tokio_bridge::{BridgeError, BridgeHost, BridgeRequest};
use tina_tower_bridge::{Service, TinaService, TinaTowerService};
use tokio::net::TcpListener as TokioTcpListener;

use super::{SideReport, scripted_client};

#[derive(Debug, Clone, PartialEq, Eq)]
enum CounterRequest {
    Read,
    Increment,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct CounterReply {
    value: u64,
}

#[derive(Debug)]
struct Counter {
    value: u64,
}

#[tina::isolate(message = BridgeRequest<CounterRequest, CounterReply>)]
impl Counter {
    fn handle(
        &mut self,
        msg: BridgeRequest<CounterRequest, CounterReply>,
        _ctx: &mut Context<'_, SingleShard>,
    ) -> Effect<Self> {
        let (request, responder) = msg.into_parts();
        match request {
            CounterRequest::Read => {}
            CounterRequest::Increment => self.value += 1,
        }
        let _ = responder.respond(CounterReply { value: self.value });
        noop()
    }
}

type CounterService = TinaService<CounterRequest, CounterReply>;

/// Map a typed `BridgeError` to an HTTP status. The mapping is a
/// table, not a string match: Full and Closed are 503 (we are not
/// taking new work), Timeout is 504 (the upstream did not respond
/// within the per-call deadline).
fn map_error(error: BridgeError) -> StatusCode {
    match error {
        BridgeError::Full | BridgeError::Closed => StatusCode::SERVICE_UNAVAILABLE,
        BridgeError::Timeout => StatusCode::GATEWAY_TIMEOUT,
    }
}

async fn read_counter(State(svc): State<CounterService>) -> (StatusCode, String) {
    let mut svc = svc;
    match svc.call(CounterRequest::Read).await {
        Ok(reply) => (StatusCode::OK, reply.value.to_string()),
        Err(error) => (map_error(error), format!("{error}")),
    }
}

async fn increment_counter(State(svc): State<CounterService>) -> (StatusCode, String) {
    let mut svc = svc;
    match svc.call(CounterRequest::Increment).await {
        Ok(reply) => (StatusCode::OK, reply.value.to_string()),
        Err(error) => (map_error(error), format!("{error}")),
    }
}

pub(crate) fn run() -> SideReport {
    let mut host = BridgeHost::new(
        SingleShard,
        DefaultThreadedMailboxFactory,
        ThreadedRuntimeConfig {
            command_capacity: 16,
            idle_wait: Duration::from_millis(1),
            ..Default::default()
        },
    );
    let bridge = host
        .register_bridge::<Counter, CounterRequest, CounterReply, Infallible>(
            Counter { value: 0 },
            16,
            Duration::from_secs(2),
        )
        .expect("register counter bridge");
    let svc: CounterService = TinaTowerService::new(bridge);

    let tokio_runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("build tokio runtime");

    let report = tokio_runtime.block_on(async move {
        let listener = TokioTcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind tokio listener");
        let addr = listener.local_addr().expect("tokio listener local addr");

        let app = Router::new()
            .route("/counter", get(read_counter))
            .route("/counter/increment", post(increment_counter))
            .with_state(svc);

        let server = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        let report = tokio::task::spawn_blocking(move || scripted_client(addr))
            .await
            .expect("client task");

        server.abort();
        let _ = server.await;
        report
    });

    drop(tokio_runtime);

    let _ = host
        .drain_and_shutdown(Duration::from_secs(2))
        .expect("bridge host drains and shuts down cleanly");

    report
}
