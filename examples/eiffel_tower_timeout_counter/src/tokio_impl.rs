//! Tokio reference. The service is `tower::service_fn` over an
//! `Arc<Mutex<Counter>>`; the slow work is `tokio::sleep`.
//!
//! Wrapped in `ServiceBuilder::new().timeout(...).concurrency_limit(...)`
//! so the same Tower stack runs against both the Tokio and Tina
//! services.

use std::convert::Infallible;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::runtime::Builder;
use tokio::task::JoinSet;
use tower::limit::ConcurrencyLimitLayer;
use tower::timeout::TimeoutLayer;
use tower::timeout::error::Elapsed;
use tower::{BoxError, Service, ServiceBuilder, ServiceExt};

use crate::{BURST, CONCURRENCY, Report, SLOW_HANDLER_MS, TIMEOUT_MS};

#[derive(Debug, Default)]
struct Counter {
    value: u64,
}

#[derive(Debug, Clone, Copy)]
struct BrushRequest;

#[derive(Debug, Clone, Copy)]
struct BrushReply {
    #[allow(dead_code)]
    value: u64,
}

pub fn run() -> anyhow::Result<Report> {
    let rt = Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()?;
    rt.block_on(async {
        let state = Arc::new(Mutex::new(Counter::default()));
        let inner = tower::service_fn(move |_req: BrushRequest| {
            let state = Arc::clone(&state);
            async move {
                tokio::time::sleep(Duration::from_millis(SLOW_HANDLER_MS)).await;
                let mut counter = state.lock().expect("counter mutex");
                counter.value += 1;
                Ok::<_, Infallible>(BrushReply {
                    value: counter.value,
                })
            }
        });
        let svc = ServiceBuilder::new()
            .layer(TimeoutLayer::new(Duration::from_millis(TIMEOUT_MS)))
            .layer(ConcurrencyLimitLayer::new(CONCURRENCY))
            .service(inner);

        let mut report = Report::default();
        let mut joins: JoinSet<Outcome> = JoinSet::new();
        for _ in 0..BURST {
            let svc = svc.clone();
            joins.spawn(async move { call_one(svc).await });
        }
        while let Some(joined) = joins.join_next().await {
            match joined.expect("join") {
                Outcome::Ok => report.successful_count += 1,
                Outcome::ServiceUnavailable => report.service_unavailable_count += 1,
                Outcome::GatewayTimeout => report.gateway_timeout_count += 1,
            }
        }
        report.exit_clean = true;
        Ok(report)
    })
}

enum Outcome {
    Ok,
    ServiceUnavailable,
    GatewayTimeout,
}

async fn call_one<S>(svc: S) -> Outcome
where
    S: Service<BrushRequest, Response = BrushReply, Error = BoxError>,
{
    match svc.oneshot(BrushRequest).await {
        Ok(_) => Outcome::Ok,
        Err(e) if e.is::<Elapsed>() => Outcome::GatewayTimeout,
        Err(_) => Outcome::ServiceUnavailable,
    }
}
