//! Tina-vs-Tokio: a small counter service that POSTs each increment
//! to an external webhook. The Tina side runs through
//! `tina-reqwest-bridge` and demonstrates three call-site shapes
//! side-by-side:
//!
//! 1. `send_request(...).then(...)` — the polished helper.
//! 2. literal `call(addr, ReqwestMsg::Send(...), ...)` — the raw
//!    layered form, retained so the underlying contract is exercised.
//! 3. `flatten_outcome(...)` inside the reply translator — the
//!    opt-in flat error edge.
//!
//! Both sides target the same fake webhook server. The driver runs
//! 3 increments; the webhook should record bodies `["1", "2", "3"]`
//! in order.
//!
//! Read [`tokio_impl`] and [`tina_impl`] top-to-bottom; the README
//! discusses where the helpers feel useful and where they don't.

use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::body::Incoming;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Request as HyperRequest, Response as HyperResponse, StatusCode};
use hyper_util::rt::TokioIo;
use tokio::net::TcpListener;
use tokio::runtime::Runtime;
use tokio::sync::oneshot;

pub mod tina_impl;
pub mod tokio_impl;

/// Captured webhook view: the POST bodies we received in arrival
/// order. Both sides should produce `["1", "2", "3"]`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Report {
    pub bodies: Vec<String>,
}

impl Report {
    pub fn assert_expected(&self) {
        assert_eq!(
            self.bodies,
            vec!["1".to_string(), "2".to_string(), "3".to_string()],
            "webhook should have received three increments in order"
        );
    }
}

/// Fake webhook server. Listens on an ephemeral port; each POST body
/// is recorded into a shared vector.
pub struct WebhookServer {
    pub addr: SocketAddr,
    pub bodies: Arc<Mutex<Vec<String>>>,
    shutdown: Option<oneshot::Sender<()>>,
    runtime: Arc<Runtime>,
    join: Option<tokio::task::JoinHandle<()>>,
}

impl WebhookServer {
    pub fn spawn() -> Self {
        let runtime = Arc::new(
            tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .worker_threads(1)
                .build()
                .expect("webhook tokio runtime"),
        );
        let bodies: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let bodies_for_task = Arc::clone(&bodies);
        let (addr_tx, addr_rx) = oneshot::channel::<SocketAddr>();
        let (shutdown_tx, mut shutdown_rx) = oneshot::channel::<()>();
        let runtime_for_spawn = Arc::clone(&runtime);
        let join = runtime.spawn(async move {
            let listener = TcpListener::bind("127.0.0.1:0")
                .await
                .expect("webhook listener bind");
            let addr = listener.local_addr().expect("webhook local addr");
            addr_tx.send(addr).expect("webhook send addr");
            loop {
                tokio::select! {
                    _ = &mut shutdown_rx => break,
                    accept = listener.accept() => {
                        let (stream, _peer) = match accept {
                            Ok(p) => p,
                            Err(_) => continue,
                        };
                        let bodies = Arc::clone(&bodies_for_task);
                        runtime_for_spawn.spawn(async move {
                            let io = TokioIo::new(stream);
                            let svc = service_fn(move |req: HyperRequest<Incoming>| {
                                let bodies = Arc::clone(&bodies);
                                async move {
                                    // The specimen documents that both
                                    // sides POST /webhook. A regression
                                    // to GET, PUT, or the wrong path
                                    // is an honest test failure — fail
                                    // here with a 405/404 instead of
                                    // recording the body and letting
                                    // the assertion blame the wrong
                                    // thing.
                                    if req.method() != hyper::Method::POST {
                                        return Ok::<_, Infallible>(
                                            HyperResponse::builder()
                                                .status(StatusCode::METHOD_NOT_ALLOWED)
                                                .body(Full::new(Bytes::from_static(
                                                    b"webhook expects POST",
                                                )))
                                                .expect("webhook 405 response"),
                                        );
                                    }
                                    if req.uri().path() != "/webhook" {
                                        return Ok::<_, Infallible>(
                                            HyperResponse::builder()
                                                .status(StatusCode::NOT_FOUND)
                                                .body(Full::new(Bytes::from_static(
                                                    b"webhook path not found",
                                                )))
                                                .expect("webhook 404 response"),
                                        );
                                    }
                                    let body = req
                                        .into_body()
                                        .collect()
                                        .await
                                        .expect("webhook collect body")
                                        .to_bytes();
                                    let text = String::from_utf8_lossy(&body).into_owned();
                                    bodies.lock().expect("webhook bodies lock").push(text);
                                    Ok::<_, Infallible>(
                                        HyperResponse::builder()
                                            .status(StatusCode::OK)
                                            .body(Full::new(Bytes::from_static(b"ok")))
                                            .expect("webhook response"),
                                    )
                                }
                            });
                            let _ = http1::Builder::new().serve_connection(io, svc).await;
                        });
                    }
                }
            }
        });
        let addr = addr_rx.blocking_recv().expect("webhook bound address");
        Self {
            addr,
            bodies,
            shutdown: Some(shutdown_tx),
            runtime,
            join: Some(join),
        }
    }

    pub fn url(&self) -> String {
        format!("http://{}/webhook", self.addr)
    }

    pub fn snapshot(&self) -> Vec<String> {
        self.bodies.lock().expect("webhook bodies lock").clone()
    }

    pub fn stop(mut self) {
        if let Some(tx) = self.shutdown.take() {
            let _ = tx.send(());
        }
        if let Some(join) = self.join.take() {
            self.runtime.block_on(async move {
                let _ = tokio::time::timeout(Duration::from_secs(2), join).await;
            });
        }
    }
}

impl Drop for WebhookServer {
    fn drop(&mut self) {
        if let Some(tx) = self.shutdown.take() {
            let _ = tx.send(());
        }
    }
}
