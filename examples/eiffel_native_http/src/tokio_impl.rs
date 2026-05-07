//! Tokio reference: `axum::Router` Counter on
//! `tokio::net::TcpListener`, hit by the shared scripted client over
//! a real socket.

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

use crate::{Report, scripted_client};

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

pub fn run() -> anyhow::Result<Report> {
    let runtime = Builder::new_current_thread().enable_all().build()?;
    let (addr_tx, addr_rx) = std::sync::mpsc::channel::<SocketAddr>();
    let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();

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
            serve(listener, app)
                .with_graceful_shutdown(async move {
                    let _ = shutdown_rx.await;
                })
                .await
                .expect("serve");
        });
    });

    let server_addr = addr_rx.recv_timeout(Duration::from_secs(2))?;
    let report = scripted_client(server_addr);
    let _ = shutdown_tx.send(());

    let deadline = Instant::now() + Duration::from_secs(2);
    while !server_handle.is_finished() && Instant::now() < deadline {
        thread::yield_now();
    }
    server_handle
        .join()
        .map_err(|_| anyhow::anyhow!("server thread panicked"))?;

    Ok(report)
}
