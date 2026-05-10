//! Tokio reference. Hand-rolled retry loop over `reqwest::Client::send`.
//!
//! - axum hosts the flaky upstream on a `current_thread` Tokio runtime
//!   in a side thread.
//! - The client is one `reqwest::Client` shared across attempts.
//! - Retry is the visible part: a `for attempt in 1..=MAX_ATTEMPTS`
//!   loop wraps each call in `tokio::time::timeout(...)`, classifies
//!   the response as success / transient (`5xx`) / fatal, sleeps a
//!   small backoff between attempts, and stops on the first 2xx.

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::thread;
use std::time::Duration;

use axum::Router;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::get;
use tokio::net::TcpListener;
use tokio::runtime::Builder;
use tokio::sync::oneshot;

use crate::{FLAKY_503_RUNS, MAX_ATTEMPTS, Report};

const PER_ATTEMPT_TIMEOUT: Duration = Duration::from_secs(2);
const RETRY_BACKOFF: Duration = Duration::from_millis(20);

#[derive(Debug, Default)]
struct FlakyState {
    seen: AtomicU32,
}

async fn flaky_handler(State(state): State<Arc<FlakyState>>) -> impl IntoResponse {
    let n = state.seen.fetch_add(1, Ordering::Relaxed) + 1;
    if n <= FLAKY_503_RUNS {
        StatusCode::SERVICE_UNAVAILABLE
    } else {
        StatusCode::OK
    }
}

pub fn run() -> anyhow::Result<Report> {
    let runtime = Builder::new_current_thread().enable_all().build()?;
    let (addr_tx, addr_rx) = std::sync::mpsc::channel::<SocketAddr>();
    let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();
    let (report_tx, report_rx) = std::sync::mpsc::channel::<Report>();

    let server_handle = thread::spawn(move || {
        runtime.block_on(async move {
            let state = Arc::new(FlakyState::default());
            let app = Router::new()
                .route("/flaky", get(flaky_handler))
                .with_state(state);
            let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
            let local = listener.local_addr().expect("local addr");
            addr_tx.send(local).expect("publish addr");
            let server_task = tokio::spawn(async move {
                axum::serve(listener, app)
                    .with_graceful_shutdown(async move {
                        let _ = shutdown_rx.await;
                    })
                    .await
                    .expect("serve");
            });
            let report = scripted_client(local).await;
            let _ = report_tx.send(report);
            let _ = shutdown_tx.send(());
            let _ = server_task.await;
        });
    });

    let _ = addr_rx.recv_timeout(Duration::from_secs(2))?;
    let report = report_rx.recv_timeout(Duration::from_secs(10))?;
    server_handle
        .join()
        .map_err(|_| anyhow::anyhow!("server thread panicked"))?;
    Ok(report)
}

async fn scripted_client(addr: SocketAddr) -> Report {
    let client = reqwest::Client::builder().build().expect("reqwest client");
    let url = format!("http://{addr}/flaky");

    let mut report = Report::default();

    for attempt in 1..=MAX_ATTEMPTS {
        report.attempts_made = attempt;

        let send = client.get(&url).send();
        let outcome = match tokio::time::timeout(PER_ATTEMPT_TIMEOUT, send).await {
            Ok(Ok(resp)) => Ok(resp.status()),
            Ok(Err(e)) => Err(format!("transport: {e}")),
            Err(_) => Err("timeout".to_string()),
        };

        match outcome {
            Ok(status) if status.is_success() => {
                report.final_ok = true;
                report.exit_clean = true;
                return report;
            }
            Ok(status) if status.is_server_error() => {
                report.transient_failures += 1;
            }
            Ok(_) | Err(_) => {
                // Fatal: don't retry. Loop exits with final_ok = false.
                report.exit_clean = true;
                return report;
            }
        }

        if attempt < MAX_ATTEMPTS {
            tokio::time::sleep(RETRY_BACKOFF).await;
        }
    }

    report.exit_clean = true;
    report
}
