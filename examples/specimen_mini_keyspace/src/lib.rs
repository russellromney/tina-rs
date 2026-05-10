//! Tina-vs-Tokio: tiny Redis-shaped key/value service over real
//! loopback TCP.
//!
//! Same scripted workload, two implementations. Read [`tokio_impl`]
//! and [`tina_impl`] top-to-bottom; the README compares feel.

use anyhow::Context;

pub mod tina_impl;
pub mod tokio_impl;

/// The fixed scripted workload both sides run. The point of the
/// comparison is implementation feel, so the workload is a single
/// constant rather than a knob.
pub const SCRIPT: &str = "\
SET llama hay
GET llama
GET missing
DEL llama
GET llama
QUIT
";

/// What each side observed. `ok + values + misses + deleted == 4`
/// after the script (the `QUIT → BYE` line is not counted).
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Report {
    pub ok: usize,
    pub values: usize,
    pub misses: usize,
    pub deleted: usize,
}

impl Report {
    pub fn total(&self) -> usize {
        self.ok + self.values + self.misses + self.deleted
    }
}

/// Wire-protocol command. Both sides parse the script into this
/// shape and translate each command into their own internal
/// representation (Tokio: a `match` on `BTreeMap`; Tina: a `call(...)`
/// to the `Store` isolate).
#[derive(Debug, Clone)]
pub enum Command {
    Set { key: String, value: String },
    Get { key: String },
    Del { key: String },
    Quit,
}

pub fn parse_commands(bytes: &[u8]) -> anyhow::Result<Vec<Command>> {
    std::str::from_utf8(bytes)?
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(parse_command)
        .collect()
}

fn parse_command(line: &str) -> anyhow::Result<Command> {
    let mut parts = line.split_whitespace();
    match parts.next() {
        Some("SET") => Ok(Command::Set {
            key: parts.next().context("SET key")?.to_string(),
            value: parts.next().context("SET value")?.to_string(),
        }),
        Some("GET") => Ok(Command::Get {
            key: parts.next().context("GET key")?.to_string(),
        }),
        Some("DEL") => Ok(Command::Del {
            key: parts.next().context("DEL key")?.to_string(),
        }),
        Some("QUIT") => Ok(Command::Quit),
        other => anyhow::bail!("unknown command {other:?} in {line:?}"),
    }
}
