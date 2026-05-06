use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::time::Duration;

mod tina_impl;
mod tokio_impl;

const SCRIPT: &str = "\
SET llama hay
GET llama
GET missing
DEL llama
GET llama
QUIT
";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SideReport {
    pub response: Vec<u8>,
    pub ok: usize,
    pub values: usize,
    pub misses: usize,
    pub deleted: usize,
}

impl SideReport {
    pub fn assert_expected(&self) {
        assert_eq!(
            String::from_utf8_lossy(&self.response),
            "OK\nVALUE hay\nMISS\nDELETED\nMISS\nBYE\n"
        );
        assert_eq!(self.ok, 1);
        assert_eq!(self.values, 1);
        assert_eq!(self.misses, 2);
        assert_eq!(self.deleted, 1);
    }
}

pub fn run_tokio_side() -> SideReport {
    tokio_impl::run()
}

pub fn run_tina_side() -> SideReport {
    tina_impl::run()
}

pub(crate) fn scripted_client(addr: SocketAddr) -> Vec<u8> {
    let mut stream = TcpStream::connect(addr).expect("client connects");
    stream
        .set_read_timeout(Some(Duration::from_secs(3)))
        .expect("set read timeout");
    stream.write_all(SCRIPT.as_bytes()).expect("write script");
    stream
        .shutdown(std::net::Shutdown::Write)
        .expect("shutdown request write side");

    let mut response = Vec::new();
    stream.read_to_end(&mut response).expect("read response");
    response
}

pub(crate) fn report_from_response(response: Vec<u8>) -> SideReport {
    let text = String::from_utf8(response.clone()).expect("response is utf8");
    SideReport {
        response,
        ok: text.lines().filter(|line| *line == "OK").count(),
        values: text
            .lines()
            .filter(|line| line.starts_with("VALUE "))
            .count(),
        misses: text.lines().filter(|line| *line == "MISS").count(),
        deleted: text.lines().filter(|line| *line == "DELETED").count(),
    }
}

#[derive(Debug)]
pub(crate) enum Command {
    Set { key: String, value: String },
    Get { key: String },
    Del { key: String },
    Quit,
}

pub(crate) fn parse_commands(bytes: &[u8]) -> Vec<Command> {
    let text = std::str::from_utf8(bytes).expect("script is utf8");
    text.lines()
        .filter(|line| !line.trim().is_empty())
        .map(parse_command)
        .collect()
}

fn parse_command(line: &str) -> Command {
    let mut parts = line.split_whitespace();
    match parts.next() {
        Some("SET") => Command::Set {
            key: parts.next().expect("SET key").to_string(),
            value: parts.next().expect("SET value").to_string(),
        },
        Some("GET") => Command::Get {
            key: parts.next().expect("GET key").to_string(),
        },
        Some("DEL") => Command::Del {
            key: parts.next().expect("DEL key").to_string(),
        },
        Some("QUIT") => Command::Quit,
        other => panic!("unknown mini-keyspace command {other:?} in {line:?}"),
    }
}
