//! Tokio side: `axum`'s simplest "big body" shape. Build a
//! `Vec<u8>` of `RESPONSE_BODY_BYTES`, hand it to
//! `axum::body::Body::from(...)`, and let the runtime drain it.
//!
//! There is no per-chunk pull point in this shape. The body is
//! resident in memory — the entire `RESPONSE_BODY_BYTES` is
//! allocated before the response starts. axum/hyper handle the
//! TCP pacing for us against the slow client.

use std::net::SocketAddr;
use std::thread;
use std::time::{Duration, Instant};

use axum::Router;
use axum::body::Body;
use axum::http::StatusCode;
use axum::response::Response;
use axum::routing::get;
use axum::serve;
use tokio::net::TcpListener;
use tokio::runtime::Builder;
use tokio::sync::oneshot;

use crate::{RESPONSE_BODY_BYTES, Report, slow_reader_client};

// On the Tokio side we know the lower bound on body residency: the
// `Vec<u8>` we hand to `Body::from(...)` is RESPONSE_BODY_BYTES
// long and lives until the response finishes streaming. Hyper may
// queue more bytes internally; we don't see those.
const TOKIO_RESPONSE_ALLOC_FLOOR: usize = RESPONSE_BODY_BYTES;

async fn big() -> Response {
    let body: Vec<u8> = vec![b'a'; RESPONSE_BODY_BYTES];
    Response::builder()
        .status(StatusCode::OK)
        .header("content-length", body.len())
        .body(Body::from(body))
        .expect("build response")
}

pub fn run() -> anyhow::Result<Report> {
    let runtime = Builder::new_current_thread().enable_all().build()?;
    let (addr_tx, addr_rx) = std::sync::mpsc::channel::<SocketAddr>();
    let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();

    let server_handle = thread::spawn(move || {
        runtime.block_on(async move {
            let app = Router::new().route("/big", get(big));
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
    let (bytes, ok, wall_ms) = slow_reader_client(server_addr, "/big");
    let shutdown = shutdown_tx
        .send(())
        .map_err(|_| anyhow::anyhow!("HTTP server stop receiver dropped"));

    let deadline = Instant::now() + Duration::from_secs(2);
    while !server_handle.is_finished() && Instant::now() < deadline {
        thread::yield_now();
    }
    let joined = server_handle
        .join()
        .map_err(|_| anyhow::anyhow!("server thread panicked"));
    shutdown?;
    joined?;

    Ok(Report {
        bytes_received: bytes,
        status_ok: ok,
        wall_clock_ms: wall_ms,
        exit_clean: true,
        tokio_response_alloc_floor: Some(TOKIO_RESPONSE_ALLOC_FLOOR),
        tina_response_high_water: None,
        tina_chunked_decoded_bytes: None,
        tina_chunked_wire_bytes: None,
        tina_capacity_discovery_line: None,
    })
}
