//! Tokio side: pure axum + `Arc<Mutex<CounterState>>` shared between
//! handlers. The whole service lives inside one Tokio runtime.

use std::sync::{Arc, Mutex};

use axum::Router;
use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::{get, post};
use tokio::net::TcpListener as TokioTcpListener;

use crate::{Report, scripted_client};

#[derive(Default)]
struct CounterState {
    value: u64,
}

type AppState = Arc<Mutex<CounterState>>;

async fn read_counter(State(state): State<AppState>) -> (StatusCode, String) {
    let value = state.lock().expect("counter state lock").value;
    (StatusCode::OK, value.to_string())
}

async fn increment_counter(State(state): State<AppState>) -> (StatusCode, String) {
    let mut guard = state.lock().expect("counter state lock");
    guard.value += 1;
    (StatusCode::OK, guard.value.to_string())
}

pub fn run() -> Report {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("build tokio runtime");

    runtime.block_on(async move {
        let listener = TokioTcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind tokio listener");
        let addr = listener.local_addr().expect("tokio listener local addr");

        let state: AppState = Arc::new(Mutex::new(CounterState::default()));
        let app = Router::new()
            .route("/counter", get(read_counter))
            .route("/counter/increment", post(increment_counter))
            .with_state(state);

        let server = tokio::spawn(async move { axum::serve(listener, app).await });

        let report = tokio::task::spawn_blocking(move || scripted_client(addr))
            .await
            .expect("client task");

        server.abort();
        match server.await {
            Err(error) if error.is_cancelled() => {}
            Ok(Ok(())) => {}
            Ok(Err(error)) => panic!("axum server failed: {error}"),
            Err(error) => panic!("axum server task failed: {error}"),
        }
        report
    })
}
