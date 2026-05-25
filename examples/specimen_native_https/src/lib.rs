//! Tina-vs-Tokio HTTPS/1.1 counter specimen. The scripted flow is the same on
//! both sides: `GET /counter → POST × 3 → GET /counter → GET /missing`.
//!
//! - Tokio side (`tokio_impl`): a hand-rolled `tokio + tokio-rustls` server,
//!   hit by the stdlib-rustls [`scripted_client`] below — the interop
//!   counterparty.
//! - Tina side (`tina_impl`): a `tina_http::HttpsListener` server **and** a
//!   `tina_http::HttpClient` HTTPS client in **one runtime, on one shard**.
//!   TLS rides Tina's TCP rail, so client and server share a runtime — the
//!   case the old single-worker TLS lane could not run.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::sync::Arc;
use std::time::Duration;

pub mod tina_impl;
pub mod tls_identity;
pub mod tokio_impl;

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Report {
    pub successful_get: u32,
    pub successful_post: u32,
    pub final_counter_value: u32,
    pub got_404_for_missing: bool,
    pub exit_clean: bool,
}

/// Real rustls client. `cert_der` is the server leaf, also used as
/// the client's trust root.
pub fn scripted_client(addr: SocketAddr, cert_der: Vec<u8>) -> Report {
    let mut report = Report {
        exit_clean: true,
        ..Report::default()
    };

    let r1 = one_request(
        addr,
        cert_der.clone(),
        b"GET /counter HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
    );
    let (s1, b1) = parse_status_and_body(&r1);
    if s1 == 200 {
        report.successful_get += 1;
    }
    assert_eq!(b1.trim(), "0", "first GET should report counter=0");

    for _ in 0..3 {
        let r = one_request(
            addr,
            cert_der.clone(),
            b"POST /counter HTTP/1.1\r\nHost: localhost\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
        );
        let (status, _) = parse_status_and_body(&r);
        if status == 200 {
            report.successful_post += 1;
        }
    }

    let r2 = one_request(
        addr,
        cert_der.clone(),
        b"GET /counter HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
    );
    let (s2, b2) = parse_status_and_body(&r2);
    if s2 == 200 {
        report.successful_get += 1;
    }
    report.final_counter_value = b2.trim().parse().expect("counter is u32");

    let r3 = one_request(
        addr,
        cert_der,
        b"GET /missing HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
    );
    let (s3, _) = parse_status_and_body(&r3);
    if s3 == 404 {
        report.got_404_for_missing = true;
    }

    report
}

fn one_request(addr: SocketAddr, root_cert_der: Vec<u8>, request: &[u8]) -> Vec<u8> {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let mut roots = rustls::RootCertStore::empty();
    roots
        .add(rustls::pki_types::CertificateDer::from(root_cert_der))
        .expect("add self-signed root");
    let config = rustls::ClientConfig::builder()
        .with_root_certificates(roots)
        .with_no_client_auth();
    let server_name =
        rustls::pki_types::ServerName::try_from("localhost").expect("server name");
    let connection =
        rustls::ClientConnection::new(Arc::new(config), server_name).expect("client conn");
    let tcp = TcpStream::connect_timeout(&addr, Duration::from_secs(5)).expect("tcp connect");
    tcp.set_read_timeout(Some(Duration::from_secs(10)))
        .expect("read timeout");
    tcp.set_write_timeout(Some(Duration::from_secs(10)))
        .expect("write timeout");
    let mut stream = rustls::StreamOwned::new(connection, tcp);
    while stream.conn.is_handshaking() {
        stream
            .conn
            .complete_io(&mut stream.sock)
            .expect("client handshake");
    }
    stream.write_all(request).expect("write");
    stream.flush().expect("flush");
    let mut response = Vec::new();
    let _ = stream.read_to_end(&mut response);
    response
}

fn parse_status_and_body(response: &[u8]) -> (u16, String) {
    let separator = b"\r\n\r\n";
    let header_end = response
        .windows(separator.len())
        .position(|w| w == separator)
        .expect("response has CRLFCRLF");
    let head = std::str::from_utf8(&response[..header_end]).expect("ASCII headers");
    let line = head.lines().next().expect("status line");
    let mut parts = line.split_whitespace();
    let _version = parts.next();
    let status: u16 = parts
        .next()
        .expect("status code")
        .parse()
        .expect("status u16");
    let body = std::str::from_utf8(&response[header_end + separator.len()..])
        .unwrap_or("")
        .to_owned();
    (status, body)
}
