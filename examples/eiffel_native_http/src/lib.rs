//! Tina-vs-Tokio: a native HTTP/1.1 counter server. The shared
//! `scripted_client` (a tiny std::net HTTP/1.1 client) hits both
//! sides with the same script.
//!
//! - Tokio: `axum` Counter on `tokio::net::TcpListener`.
//! - Tina: `tina_http::HttpListener` + a `Counter` isolate.
//!
//! Read [`tokio_impl`] and [`tina_impl`] top-to-bottom; the README
//! compares feel.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::time::Duration;

pub mod tina_impl;
pub mod tokio_impl;

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Report {
    pub successful_get: u32,
    pub successful_post: u32,
    pub final_counter_value: u32,
    pub got_404_for_missing: bool,
    pub exit_clean: bool,
}

/// Drives both sides with the same HTTP/1.1 script:
/// `GET /counter → POST × 3 → GET /counter → GET /missing`.
/// Returns a [`Report`] of what the server did.
///
/// Shared because it's the test *client*, not a server harness —
/// both sides implement HTTP server-side however they like.
pub fn scripted_client(addr: SocketAddr) -> Report {
    let mut report = Report {
        exit_clean: true,
        ..Report::default()
    };

    let r1 = one_request(
        addr,
        b"GET /counter HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n",
    );
    let (s1, b1) = parse_status_and_body(&r1);
    if s1 == 200 {
        report.successful_get += 1;
    }
    assert_eq!(b1.trim(), "0", "first GET should report counter=0");

    for _ in 0..3 {
        let r = one_request(
            addr,
            b"POST /counter HTTP/1.1\r\nHost: x\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
        );
        let (status, _) = parse_status_and_body(&r);
        if status == 200 {
            report.successful_post += 1;
        }
    }

    let r2 = one_request(
        addr,
        b"GET /counter HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n",
    );
    let (s2, b2) = parse_status_and_body(&r2);
    if s2 == 200 {
        report.successful_get += 1;
    }
    report.final_counter_value = b2.trim().parse().expect("counter is u32");

    let r3 = one_request(
        addr,
        b"GET /missing HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n",
    );
    let (s3, _) = parse_status_and_body(&r3);
    if s3 == 404 {
        report.got_404_for_missing = true;
    }

    report
}

fn one_request(addr: SocketAddr, request: &[u8]) -> Vec<u8> {
    let mut stream =
        TcpStream::connect_timeout(&addr, Duration::from_secs(2)).expect("connect to server");
    stream
        .set_read_timeout(Some(Duration::from_secs(2)))
        .expect("read timeout");
    stream.write_all(request).expect("write request");
    stream.flush().expect("flush");
    let mut response = Vec::new();
    stream.read_to_end(&mut response).expect("read response");
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
