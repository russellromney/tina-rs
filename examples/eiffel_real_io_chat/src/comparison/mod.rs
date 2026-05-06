use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::time::Duration;

mod tina_impl;
mod tokio_impl;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ComparisonConfig {
    pub burst: usize,
    pub slow_client_capacity: usize,
}

impl Default for ComparisonConfig {
    fn default() -> Self {
        Self {
            burst: 64,
            slow_client_capacity: 1,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SideReport {
    pub response: Vec<u8>,
    pub accepted: usize,
    pub full: usize,
    pub closed: usize,
    pub delivered: usize,
    pub buffered: usize,
    pub saw_real_tcp_calls: bool,
    pub saw_visible_full: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ComparisonReport {
    pub config: ComparisonConfig,
    pub tokio: SideReport,
    pub tina: SideReport,
}

impl ComparisonReport {
    #[allow(dead_code)]
    pub fn assert_expected(&self) {
        assert!(self.config.burst > self.config.slow_client_capacity);
        assert_eq!(self.tokio.accepted, self.config.burst);
        assert_eq!(self.tokio.full, 0);
        assert_eq!(self.tokio.delivered, 1);
        assert_eq!(self.tokio.buffered, self.config.burst - 1);
        assert!(self.tokio.saw_real_tcp_calls);

        assert_eq!(self.tina.accepted, self.config.slow_client_capacity);
        assert_eq!(
            self.tina.full,
            self.config.burst - self.config.slow_client_capacity
        );
        assert_eq!(self.tina.delivered, self.config.slow_client_capacity);
        assert_eq!(self.tina.buffered, 0);
        assert!(self.tina.saw_real_tcp_calls);
        assert!(self.tina.saw_visible_full);
    }
}

#[allow(dead_code)]
pub fn run() -> ComparisonReport {
    run_with_config(ComparisonConfig::default())
}

pub fn run_with_config(config: ComparisonConfig) -> ComparisonReport {
    assert!(config.burst > 0, "burst must be greater than zero");
    assert!(
        config.slow_client_capacity > 0,
        "slow client capacity must be greater than zero"
    );

    ComparisonReport {
        config,
        tokio: run_tokio_side(config.burst),
        tina: run_tina_side(config),
    }
}

pub fn run_tokio_side(burst: usize) -> SideReport {
    tokio_impl::run(burst)
}

pub fn run_tina_side(config: ComparisonConfig) -> SideReport {
    tina_impl::run(config)
}

pub(crate) fn format_response(
    accepted: usize,
    full: usize,
    closed: usize,
    delivered: usize,
    buffered: usize,
) -> Vec<u8> {
    format!(
        "accepted={accepted} full={full} closed={closed} delivered={delivered} buffered={buffered}\n"
    )
    .into_bytes()
}

pub(crate) fn parse_response(
    bytes: Vec<u8>,
    saw_real_tcp_calls: bool,
    saw_visible_full: bool,
) -> SideReport {
    let text = String::from_utf8(bytes.clone()).expect("comparison response is utf8");
    let mut accepted = None;
    let mut full = None;
    let mut closed = None;
    let mut delivered = None;
    let mut buffered = None;

    for field in text.split_whitespace() {
        let Some((key, value)) = field.split_once('=') else {
            continue;
        };
        let value = value.parse::<usize>().expect("numeric response field");
        match key {
            "accepted" => accepted = Some(value),
            "full" => full = Some(value),
            "closed" => closed = Some(value),
            "delivered" => delivered = Some(value),
            "buffered" => buffered = Some(value),
            _ => {}
        }
    }

    SideReport {
        response: bytes,
        accepted: accepted.expect("accepted field"),
        full: full.expect("full field"),
        closed: closed.expect("closed field"),
        delivered: delivered.expect("delivered field"),
        buffered: buffered.expect("buffered field"),
        saw_real_tcp_calls,
        saw_visible_full,
    }
}

pub(crate) fn parse_burst(bytes: &[u8]) -> usize {
    let text = std::str::from_utf8(bytes).expect("burst request is utf8");
    text.trim()
        .parse::<usize>()
        .expect("burst request is usize")
}

pub(crate) fn real_tcp_client(addr: SocketAddr, burst: usize) -> Vec<u8> {
    let mut stream = TcpStream::connect(addr).expect("real tcp client connects");
    stream
        .set_read_timeout(Some(Duration::from_secs(3)))
        .expect("set read timeout");
    stream
        .write_all(burst.to_string().as_bytes())
        .expect("write burst request");
    stream
        .shutdown(std::net::Shutdown::Write)
        .expect("shutdown request write side");

    let mut response = Vec::new();
    let mut buf = [0u8; 128];
    loop {
        let count = stream.read(&mut buf).expect("read comparison response");
        if count == 0 {
            break;
        }
        response.extend_from_slice(&buf[..count]);
        if response.ends_with(b"\n") {
            break;
        }
    }
    response
}
