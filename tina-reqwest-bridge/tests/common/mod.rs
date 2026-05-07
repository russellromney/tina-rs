//! Shared fixtures for reqwest bridge tests.

use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
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

/// Boxed future returned by every fake-server handler in this fixture.
pub type HandlerFuture =
    std::pin::Pin<Box<dyn std::future::Future<Output = HyperResponse<Full<Bytes>>> + Send>>;

/// One small hyper HTTP/1.1 server that returns whatever the handler
/// closure decides per request.
pub struct FakeServer {
    pub addr: SocketAddr,
    shutdown: Option<oneshot::Sender<()>>,
    runtime: Arc<Runtime>,
    join: Option<tokio::task::JoinHandle<()>>,
}

impl FakeServer {
    /// Spawn one fake HTTP server. `handler` runs on every request.
    pub fn spawn<H, F>(handler: H) -> Self
    where
        H: Fn(HyperRequest<Incoming>) -> F + Send + Sync + 'static,
        F: std::future::Future<Output = HyperResponse<Full<Bytes>>> + Send + 'static,
    {
        let runtime = Arc::new(
            tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .worker_threads(1)
                .build()
                .expect("test tokio runtime"),
        );
        let (addr_tx, addr_rx) = oneshot::channel::<SocketAddr>();
        let (shutdown_tx, mut shutdown_rx) = oneshot::channel::<()>();
        let handler = Arc::new(handler);
        let runtime_for_spawn = Arc::clone(&runtime);
        let join = runtime.spawn(async move {
            let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
            let addr = listener.local_addr().expect("local addr");
            addr_tx.send(addr).expect("send addr");
            loop {
                tokio::select! {
                    _ = &mut shutdown_rx => break,
                    accept = listener.accept() => {
                        let (stream, _peer) = match accept {
                            Ok(p) => p,
                            Err(_) => continue,
                        };
                        let handler = Arc::clone(&handler);
                        runtime_for_spawn.spawn(async move {
                            let io = TokioIo::new(stream);
                            let svc = service_fn(move |req| {
                                let handler = Arc::clone(&handler);
                                async move { Ok::<_, Infallible>(handler(req).await) }
                            });
                            let _ = http1::Builder::new().serve_connection(io, svc).await;
                        });
                    }
                }
            }
        });
        let addr = addr_rx.blocking_recv().expect("fake server bound address");
        Self {
            addr,
            shutdown: Some(shutdown_tx),
            runtime,
            join: Some(join),
        }
    }

    /// `http://127.0.0.1:port` URL helper.
    pub fn url(&self, path: &str) -> String {
        format!("http://{}{}", self.addr, path)
    }

    /// Stop the server.
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

impl Drop for FakeServer {
    fn drop(&mut self) {
        if let Some(tx) = self.shutdown.take() {
            let _ = tx.send(());
        }
    }
}

/// Reply 200 OK with `body` after `delay`.
pub fn delayed_ok(
    body: &'static [u8],
    delay: Duration,
) -> impl Fn(HyperRequest<Incoming>) -> HandlerFuture + Send + Sync + 'static {
    move |_req: HyperRequest<Incoming>| {
        let body = body;
        Box::pin(async move {
            tokio::time::sleep(delay).await;
            HyperResponse::builder()
                .status(StatusCode::OK)
                .body(Full::new(Bytes::from_static(body)))
                .expect("build response")
        })
    }
}

/// Reply 200 OK that drains the request body and echoes its length.
pub fn echo_body_len() -> impl Fn(HyperRequest<Incoming>) -> HandlerFuture + Send + Sync + 'static {
    move |req: HyperRequest<Incoming>| {
        Box::pin(async move {
            let body = req.into_body().collect().await.expect("collect body");
            let len = body.to_bytes().len();
            HyperResponse::builder()
                .status(StatusCode::OK)
                .body(Full::new(Bytes::from(len.to_string())))
                .expect("build response")
        })
    }
}

/// Sentinel that survives test-fn boundaries; lets a closure flag a
/// fake-server condition without sharing borrowed state.
#[derive(Debug, Default, Clone)]
pub struct Beacon(pub Arc<AtomicBool>);

impl Beacon {
    pub fn fired(&self) -> bool {
        self.0.load(Ordering::Acquire)
    }

    pub fn fire(&self) {
        self.0.store(true, Ordering::Release);
    }
}

/// Counter shared between test code and a fake server. Each request
/// reads the current count, decides what to do, then increments.
#[derive(Debug, Default, Clone)]
pub struct Counter(pub Arc<AtomicUsize>);

impl Counter {
    pub fn read(&self) -> usize {
        self.0.load(Ordering::Acquire)
    }

    pub fn bump(&self) -> usize {
        self.0.fetch_add(1, Ordering::AcqRel)
    }
}

/// TCP listener that drops the first `drop_first` connections (closes
/// the socket without reading or writing) and then accepts subsequent
/// ones with a handler.
///
/// Used to force reqwest into transport-level errors so retry-on-IO
/// can be tested deterministically.
pub struct FlakyServer {
    pub addr: SocketAddr,
    pub accepted: Counter,
    shutdown: Option<oneshot::Sender<()>>,
    runtime: Arc<Runtime>,
    join: Option<tokio::task::JoinHandle<()>>,
}

impl FlakyServer {
    /// `drop_first` connections close immediately; subsequent ones get
    /// handled by `handler`.
    pub fn spawn<H, F>(drop_first: usize, handler: H) -> Self
    where
        H: Fn(HyperRequest<Incoming>) -> F + Send + Sync + 'static,
        F: std::future::Future<Output = HyperResponse<Full<Bytes>>> + Send + 'static,
    {
        let runtime = Arc::new(
            tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .worker_threads(1)
                .build()
                .expect("flaky tokio runtime"),
        );
        let (addr_tx, addr_rx) = oneshot::channel::<SocketAddr>();
        let (shutdown_tx, mut shutdown_rx) = oneshot::channel::<()>();
        let handler = Arc::new(handler);
        let runtime_for_spawn = Arc::clone(&runtime);
        let counter = Counter::default();
        let counter_for_task = counter.clone();
        let join = runtime.spawn(async move {
            let listener = TcpListener::bind("127.0.0.1:0").await.expect("flaky bind");
            let addr = listener.local_addr().expect("flaky local addr");
            addr_tx.send(addr).expect("flaky send addr");
            loop {
                tokio::select! {
                    _ = &mut shutdown_rx => break,
                    accept = listener.accept() => {
                        let (stream, _peer) = match accept {
                            Ok(p) => p,
                            Err(_) => continue,
                        };
                        let attempt = counter_for_task.bump();
                        if attempt < drop_first {
                            // Close the connection without speaking
                            // any HTTP. Reqwest sees a transport-level
                            // failure (EOF before headers).
                            drop(stream);
                            continue;
                        }
                        let handler = Arc::clone(&handler);
                        runtime_for_spawn.spawn(async move {
                            let io = TokioIo::new(stream);
                            let svc = service_fn(move |req| {
                                let handler = Arc::clone(&handler);
                                async move { Ok::<_, Infallible>(handler(req).await) }
                            });
                            let _ = http1::Builder::new().serve_connection(io, svc).await;
                        });
                    }
                }
            }
        });
        let addr = addr_rx.blocking_recv().expect("flaky bound address");
        Self {
            addr,
            accepted: counter,
            shutdown: Some(shutdown_tx),
            runtime,
            join: Some(join),
        }
    }

    pub fn url(&self, path: &str) -> String {
        format!("http://{}{}", self.addr, path)
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

impl Drop for FlakyServer {
    fn drop(&mut self) {
        if let Some(tx) = self.shutdown.take() {
            let _ = tx.send(());
        }
    }
}

/// Reply 200 OK and echo a single header value back as the body. The
/// header name is matched case-insensitively. If the header is absent
/// the body is empty.
pub fn echo_header(
    name: &'static str,
) -> impl Fn(HyperRequest<Incoming>) -> HandlerFuture + Send + Sync + 'static {
    move |req: HyperRequest<Incoming>| {
        let value = req
            .headers()
            .get(name)
            .map(|v| v.as_bytes().to_vec())
            .unwrap_or_default();
        Box::pin(async move {
            HyperResponse::builder()
                .status(StatusCode::OK)
                .header("x-echoed-from", name)
                .body(Full::new(Bytes::from(value)))
                .expect("build response")
        })
    }
}

/// Reply with a fixed status and body.
pub fn fixed_status(
    status: StatusCode,
    body: &'static [u8],
) -> impl Fn(HyperRequest<Incoming>) -> HandlerFuture + Send + Sync + 'static {
    move |_req: HyperRequest<Incoming>| {
        Box::pin(async move {
            HyperResponse::builder()
                .status(status)
                .body(Full::new(Bytes::from_static(body)))
                .expect("build response")
        })
    }
}

/// Echo full request body back as the response body. Useful for POST
/// roundtrip tests.
pub fn echo_body() -> impl Fn(HyperRequest<Incoming>) -> HandlerFuture + Send + Sync + 'static {
    move |req: HyperRequest<Incoming>| {
        Box::pin(async move {
            let bytes = req
                .into_body()
                .collect()
                .await
                .expect("collect body")
                .to_bytes();
            HyperResponse::builder()
                .status(StatusCode::OK)
                .body(Full::new(bytes))
                .expect("build response")
        })
    }
}
