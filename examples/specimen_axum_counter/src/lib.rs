//! Tina-vs-Tokio: a small counter service exposed over Axum, where
//! the Tina side keeps the counter in a `Counter` isolate and bridges
//! requests across the two-runtime boundary via `tina_tokio_bridge`.
//!
//! Read [`tokio_impl`] and [`tina_impl`] top-to-bottom; the README
//! compares feel and discusses the two-runtime cost.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::time::Duration;

pub mod tina_impl;
pub mod tokio_impl;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Report {
    pub statuses: Vec<u16>,
    pub bodies: Vec<String>,
}

impl Report {
    pub fn assert_expected(&self) {
        assert_eq!(
            self.statuses,
            vec![200, 200, 200],
            "all three counter requests should return 200"
        );
        assert_eq!(
            self.bodies,
            vec!["1".to_string(), "2".to_string(), "2".to_string()],
            "increment, increment, get should produce 1, 2, 2"
        );
    }
}

/// Three-step scripted HTTP client used by both sides:
/// `POST /counter/increment` → `POST /counter/increment` → `GET /counter`.
pub fn scripted_client(addr: SocketAddr) -> Report {
    let mut report = Report {
        statuses: Vec::new(),
        bodies: Vec::new(),
    };
    let scripted = [
        ("POST", "/counter/increment"),
        ("POST", "/counter/increment"),
        ("GET", "/counter"),
    ];
    for (method, path) in scripted {
        let (status, body) = http_request(addr, method, path);
        report.statuses.push(status);
        report.bodies.push(body);
    }
    report
}

fn http_request(addr: SocketAddr, method: &str, path: &str) -> (u16, String) {
    let mut stream = TcpStream::connect(addr).expect("client connects");
    stream
        .set_read_timeout(Some(Duration::from_secs(3)))
        .expect("set read timeout");
    let request = format!(
        "{method} {path} HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\nContent-Length: 0\r\n\r\n"
    );
    stream
        .write_all(request.as_bytes())
        .expect("write http request");
    let mut response = Vec::new();
    stream.read_to_end(&mut response).expect("read response");
    parse_http_response(&response)
}

fn parse_http_response(bytes: &[u8]) -> (u16, String) {
    let text = std::str::from_utf8(bytes).expect("response is utf8");
    let mut header_end = 0;
    for (idx, window) in text.as_bytes().windows(4).enumerate() {
        if window == b"\r\n\r\n" {
            header_end = idx + 4;
            break;
        }
    }
    assert!(header_end > 0, "missing header/body delimiter");
    let status_line = text.lines().next().expect("status line");
    let status = status_line
        .split_whitespace()
        .nth(1)
        .expect("status code")
        .parse::<u16>()
        .expect("status is u16");
    let body = &text[header_end..];
    (status, body.to_string())
}
