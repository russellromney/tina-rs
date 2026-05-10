//! Tokio reference: `tokio + tokio-rustls` HTTPS server, hand-rolled
//! HTTP/1.1 to match the Tina side line-for-line.
//!
//! One `tokio::net::TcpListener` accepts plaintext TCP, hands each
//! connection to `tokio_rustls::TlsAcceptor`, then a per-task loop
//! reads exactly one HTTP/1.1 request, mutates a shared counter, and
//! writes a response. State is `Arc<AtomicU32>`.

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::runtime::Builder;
use tokio::sync::oneshot;
use tokio_rustls::TlsAcceptor;

use crate::{Report, scripted_client, tls_identity};

pub fn run() -> anyhow::Result<Report> {
    let identity_bundle = tls_identity::generate();
    let cert_der = identity_bundle.cert_der.clone();

    let server_cert = rustls::pki_types::CertificateDer::from(identity_bundle.cert_chain_der[0].clone());
    let server_key = rustls::pki_types::PrivateKeyDer::Pkcs8(
        rustls::pki_types::PrivatePkcs8KeyDer::from(identity_bundle.private_key_der.clone()),
    );
    let server_config = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(vec![server_cert], server_key)?;
    let acceptor = TlsAcceptor::from(Arc::new(server_config));

    let runtime = Builder::new_current_thread().enable_all().build()?;
    let (addr_tx, addr_rx) = std::sync::mpsc::channel::<SocketAddr>();
    let (shutdown_tx, mut shutdown_rx) = oneshot::channel::<()>();

    let counter = Arc::new(AtomicU32::new(0));

    let server_handle = thread::spawn(move || {
        runtime.block_on(async move {
            let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
            let local = listener.local_addr().expect("local addr");
            addr_tx.send(local).expect("publish addr");
            loop {
                tokio::select! {
                    _ = &mut shutdown_rx => break,
                    accept = listener.accept() => {
                        let (tcp, _) = match accept {
                            Ok(pair) => pair,
                            Err(_) => continue,
                        };
                        let acceptor = acceptor.clone();
                        let counter = Arc::clone(&counter);
                        tokio::spawn(async move {
                            let mut stream = match acceptor.accept(tcp).await {
                                Ok(s) => s,
                                Err(_) => return,
                            };
                            let mut buf = vec![0u8; 4096];
                            let mut have = 0usize;
                            loop {
                                let n = match stream.read(&mut buf[have..]).await {
                                    Ok(0) | Err(_) => return,
                                    Ok(n) => n,
                                };
                                have += n;
                                if buf[..have].windows(4).any(|w| w == b"\r\n\r\n") {
                                    break;
                                }
                            }
                            let head = std::str::from_utf8(&buf[..have]).unwrap_or("");
                            let request_line = head.lines().next().unwrap_or("");
                            let mut parts = request_line.split_whitespace();
                            let method = parts.next().unwrap_or("");
                            let path = parts.next().unwrap_or("");
                            let response = match (method, path) {
                                ("GET", "/counter") => http_text_ok(&counter.load(Ordering::Relaxed).to_string()),
                                ("POST", "/counter") => {
                                    let value = counter.fetch_add(1, Ordering::Relaxed) + 1;
                                    http_text_ok(&value.to_string())
                                }
                                _ => http_not_found(),
                            };
                            let _ = stream.write_all(&response).await;
                            let _ = stream.flush().await;
                            let _ = stream.shutdown().await;
                        });
                    }
                }
            }
        });
    });

    let server_addr = addr_rx.recv_timeout(Duration::from_secs(5))?;
    let report = scripted_client(server_addr, cert_der);
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

fn http_text_ok(body: &str) -> Vec<u8> {
    let mut response = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    )
    .into_bytes();
    response.extend_from_slice(body.as_bytes());
    response
}

fn http_not_found() -> Vec<u8> {
    b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_vec()
}
