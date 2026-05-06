use std::sync::{Arc, Mutex};

use axum::Router;
use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::{get, post};
use tokio::net::TcpListener as TokioTcpListener;

use super::{SideReport, scripted_client};

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

pub(crate) fn run() -> SideReport {
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

        let server = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        let report = tokio::task::spawn_blocking(move || scripted_client(addr))
            .await
            .expect("client task");

        server.abort();
        let _ = server.await;
        report
    })
}
