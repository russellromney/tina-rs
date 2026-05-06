//! Tokio side of the outbound-HTTP comparison.
//!
//! Hosts an `axum` counter server, then drives it via `reqwest` as the
//! outbound HTTP client. Walks the same scripted endpoint sequence the
//! Tina side runs.

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::get;
use axum::{Router, serve};
use tokio::net::TcpListener;
use tokio::runtime::Builder;
use tokio::sync::oneshot;

use super::SideReport;

#[derive(Debug, Default)]
struct CounterState {
    value: AtomicU32,
}

impl CounterState {
    fn read(&self) -> u32 {
        self.value.load(Ordering::Relaxed)
    }
    fn increment(&self) -> u32 {
        self.value.fetch_add(1, Ordering::Relaxed) + 1
    }
}

async fn get_counter(State(state): State<Arc<CounterState>>) -> impl IntoResponse {
    state.read().to_string()
}

async fn post_counter(State(state): State<Arc<CounterState>>) -> impl IntoResponse {
    state.increment().to_string()
}

async fn fallback() -> impl IntoResponse {
    StatusCode::NOT_FOUND
}

pub(crate) fn run() -> SideReport {
    let runtime = Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("tokio runtime");

    let (addr_tx, addr_rx) = std::sync::mpsc::channel::<SocketAddr>();
    let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();
    let (report_tx, report_rx) = std::sync::mpsc::channel::<SideReport>();

    let server_handle = thread::spawn(move || {
        runtime.block_on(async move {
            let state = Arc::new(CounterState::default());
            let app = Router::new()
                .route("/counter", get(get_counter).post(post_counter))
                .fallback(fallback)
                .with_state(state);
            let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
            let local = listener.local_addr().expect("local addr");
            addr_tx.send(local).expect("publish addr");
            // Run client work on the same Tokio runtime the server uses.
            let server_task = tokio::spawn(async move {
                serve(listener, app)
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

    let _server_addr = addr_rx
        .recv_timeout(Duration::from_secs(2))
        .expect("server addr");
    let report = report_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("client report");
    let deadline = Instant::now() + Duration::from_secs(2);
    while !server_handle.is_finished() && Instant::now() < deadline {
        thread::yield_now();
    }
    server_handle.join().expect("server thread joined cleanly");

    report
}

async fn scripted_client(addr: SocketAddr) -> SideReport {
    let client = reqwest::Client::builder()
        .build()
        .expect("reqwest client");
    let base = format!("http://{addr}");

    let mut report = SideReport {
        successful_get: 0,
        successful_post: 0,
        final_counter_value: 0,
        got_404_for_missing: false,
        exit_clean: true,
    };

    let r0 = client
        .get(format!("{base}/counter"))
        .send()
        .await
        .expect("initial GET");
    if r0.status().as_u16() == 200 {
        report.successful_get += 1;
    }
    let body0 = r0.text().await.unwrap_or_default();
    assert_eq!(body0.trim(), "0", "initial GET should report counter=0");

    for _ in 0..3 {
        let r = client
            .post(format!("{base}/counter"))
            .send()
            .await
            .expect("POST");
        if r.status().as_u16() == 200 {
            report.successful_post += 1;
        }
    }

    let r4 = client
        .get(format!("{base}/counter"))
        .send()
        .await
        .expect("final GET");
    if r4.status().as_u16() == 200 {
        report.successful_get += 1;
    }
    let body4 = r4.text().await.unwrap_or_default();
    report.final_counter_value = body4.trim().parse().unwrap_or(0);

    let r5 = client
        .get(format!("{base}/missing"))
        .send()
        .await
        .expect("404 GET");
    if r5.status().as_u16() == 404 {
        report.got_404_for_missing = true;
    }

    report
}
