//! Copyable Tina production-service skeleton.
//!
//! This system intentionally keeps the glue local. It assembles native
//! `tina-http`, a controller isolate, a SQLite bridge pool consumer,
//! an outbound keepalive pool, readiness, capacity reporting, graceful
//! shutdown, and one live-replay fact without becoming a web framework.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::time::Duration;

pub mod tina_impl;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunMode {
    Smoke,
    Pressure,
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct RunReport {
    pub health_ok: bool,
    pub ready_ok: bool,
    pub created_item: bool,
    pub read_item: bool,
    pub notified_item: bool,
    pub missing_404: bool,
    pub method_405: bool,
    pub bad_request_400: bool,
    pub body_cap_413: bool,
    pub db_constraint_409: bool,
    pub outbound_pressure_503: bool,
    pub ready_after_db_close_503: bool,
    pub ready_during_shutdown_503: bool,
    pub ingress_rejects_after_close: bool,
    pub shutdown_clean: bool,
    pub multi_turn_notify: bool,
    pub capacity_line: String,
    pub live_replay_fact: String,
}

impl RunReport {
    pub fn summary_line(&self) -> String {
        format!(
            "system=mini_saas_api health_ok={} ready_ok={} created_item={} read_item={} \
             notified_item={} missing_404={} method_405={} bad_request_400={} body_cap_413={} \
             db_constraint_409={} outbound_pressure_503={} ready_after_db_close_503={} \
             ready_during_shutdown_503={} ingress_rejects_after_close={} shutdown_clean={} \
             multi_turn_notify={}",
            self.health_ok,
            self.ready_ok,
            self.created_item,
            self.read_item,
            self.notified_item,
            self.missing_404,
            self.method_405,
            self.bad_request_400,
            self.body_cap_413,
            self.db_constraint_409,
            self.outbound_pressure_503,
            self.ready_after_db_close_503,
            self.ready_during_shutdown_503,
            self.ingress_rejects_after_close,
            self.shutdown_clean,
            self.multi_turn_notify,
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResponseParts {
    pub status: u16,
    pub body: String,
}

pub fn run(mode: RunMode) -> anyhow::Result<RunReport> {
    tina_impl::run(mode)
}

pub fn one_request(addr: SocketAddr, request: &[u8]) -> anyhow::Result<ResponseParts> {
    let mut stream = TcpStream::connect_timeout(&addr, Duration::from_secs(2))?;
    stream.set_read_timeout(Some(Duration::from_secs(3)))?;
    stream.write_all(request)?;
    stream.flush()?;
    let mut response = Vec::new();
    stream.read_to_end(&mut response)?;
    parse_response(&response)
}

pub fn parse_response(response: &[u8]) -> anyhow::Result<ResponseParts> {
    let separator = b"\r\n\r\n";
    let header_end = response
        .windows(separator.len())
        .position(|w| w == separator)
        .ok_or_else(|| anyhow::anyhow!("response missing CRLFCRLF: {response:?}"))?;
    let head = std::str::from_utf8(&response[..header_end])?;
    let line = head
        .lines()
        .next()
        .ok_or_else(|| anyhow::anyhow!("response missing status line"))?;
    let status = line
        .split_whitespace()
        .nth(1)
        .ok_or_else(|| anyhow::anyhow!("response missing status code: {line:?}"))?
        .parse::<u16>()?;
    let body = std::str::from_utf8(&response[header_end + separator.len()..])
        .unwrap_or("")
        .to_owned();
    Ok(ResponseParts { status, body })
}

pub fn get(addr: SocketAddr, path: &str) -> anyhow::Result<ResponseParts> {
    one_request(
        addr,
        format!("GET {path} HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n").as_bytes(),
    )
}

pub fn post(addr: SocketAddr, path: &str, body: &str) -> anyhow::Result<ResponseParts> {
    one_request(
        addr,
        format!(
            "POST {path} HTTP/1.1\r\nHost: x\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            body.len(),
            body
        )
        .as_bytes(),
    )
}

pub fn put(addr: SocketAddr, path: &str) -> anyhow::Result<ResponseParts> {
    one_request(
        addr,
        format!("PUT {path} HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n").as_bytes(),
    )
}
