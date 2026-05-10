//! Tiny in-process upstream servers. Each side spawns its own pool
//! so the runs do not collide.

use std::net::SocketAddr;
use std::sync::Arc;
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

#[derive(Clone, Copy, Debug)]
pub enum Behavior {
    /// Return `200`.
    Ok,
    /// Return `503`.
    Unavailable,
    /// Sleep 500 ms before responding.
    Slow,
}

pub struct Upstream {
    pub addrs: Vec<SocketAddr>,
    shutdown: Vec<oneshot::Sender<()>>,
    threads: Vec<thread::JoinHandle<()>>,
}

impl Upstream {
    pub fn stop(mut self) {
        for tx in self.shutdown.drain(..) {
            let _ = tx.send(());
        }
        for t in self.threads.drain(..) {
            let _ = t.join();
        }
    }
}

pub fn spawn(behaviors: &[Behavior]) -> anyhow::Result<Upstream> {
    let mut addrs = Vec::with_capacity(behaviors.len());
    let mut shutdowns = Vec::with_capacity(behaviors.len());
    let mut threads = Vec::with_capacity(behaviors.len());

    for behavior in behaviors.iter().copied() {
        let runtime = Builder::new_current_thread().enable_all().build()?;
        let (addr_tx, addr_rx) = std::sync::mpsc::channel::<SocketAddr>();
        let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();

        let handle = thread::spawn(move || {
            runtime.block_on(async move {
                let state = Arc::new(behavior);
                let app = Router::new().route("/hook", get(handler)).with_state(state);
                let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
                let local = listener.local_addr().expect("local addr");
                addr_tx.send(local).expect("publish addr");
                axum::serve(listener, app)
                    .with_graceful_shutdown(async move {
                        let _ = shutdown_rx.await;
                    })
                    .await
                    .expect("serve");
            });
        });

        let addr = addr_rx.recv_timeout(Duration::from_secs(2))?;
        addrs.push(addr);
        shutdowns.push(shutdown_tx);
        threads.push(handle);
    }

    Ok(Upstream {
        addrs,
        shutdown: shutdowns,
        threads,
    })
}

async fn handler(State(state): State<Arc<Behavior>>) -> impl IntoResponse {
    match *state {
        Behavior::Ok => StatusCode::OK,
        Behavior::Unavailable => StatusCode::SERVICE_UNAVAILABLE,
        Behavior::Slow => {
            tokio::time::sleep(Duration::from_millis(500)).await;
            StatusCode::OK
        }
    }
}

pub fn workload() -> Vec<Behavior> {
    vec![
        Behavior::Ok,
        Behavior::Ok,
        Behavior::Unavailable,
        Behavior::Slow,
    ]
}
