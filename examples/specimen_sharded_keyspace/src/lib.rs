//! Tina-vs-Tokio: tiny sharded keyspace.
//!
//! Same scripted workload, two implementations. The `tokio` side is
//! plain `Vec<Arc<Mutex<HashMap>>>` with a hand-rolled FNV-1a placement
//! function — what most "sharded" Tokio code actually looks like. The
//! `tina` side uses [`tina_runtime::sharded::ShardPlacement`] and
//! [`tina_runtime::sharded::ShardServiceTable`] to route keyed requests
//! to per-shard `Store` isolates that re-check placement before
//! mutating their own `BTreeMap`. The script also issues a `SUM`
//! command, which the Tina side fans out to every shard sequentially
//! (one `call(...)` per shard) and accumulates into one total.
//!
//! Both sides parse the same script and produce the same [`Report`].
//! See `tests/smoke.rs` for the equivalence proof.

use anyhow::Context;

pub mod tina_impl;
pub mod tokio_impl;

/// Fixed scripted workload. The point of the comparison is implementation
/// feel, so the workload is a single constant rather than a knob.
///
/// `expected_report()` enumerates the expected counts for this script.
pub const SCRIPT: &str = "\
SET alpha 1
SET beta 2
SET gamma 3
SET delta 4
SET epsilon 5
GET alpha
GET missing
DEL beta
SUM
GET beta
QUIT
";

/// Shard ids both implementations operate over. Same list on both sides
/// so the FNV-1a placement maps the same keys to the same shards.
pub const SHARD_RAW_IDS: [u32; 3] = [11, 22, 33];

/// What each side observed end-to-end.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Report {
    /// `SET k v` accepted (the request landed on the right shard and
    /// the value was inserted).
    pub ok: usize,
    /// `GET k` returned a value (key was present on the owner shard).
    pub values: usize,
    /// `GET k` or `DEL k` saw the key was absent.
    pub misses: usize,
    /// `DEL k` removed an existing entry.
    pub deleted: usize,
    /// `SUM` total: number of entries across all shards at the moment of
    /// the SUM command.
    pub sum: u64,
}

/// The expected counts for [`SCRIPT`]:
///
/// - 5 sets (alpha..epsilon)
/// - 1 GET hit (alpha)
/// - 2 misses (`GET missing` then `GET beta` after delete)
/// - 1 deleted (beta)
/// - sum=4 entries after deleting beta (alpha, gamma, delta, epsilon)
pub fn expected_report() -> Report {
    Report {
        ok: 5,
        values: 1,
        misses: 2,
        deleted: 1,
        sum: 4,
    }
}

/// Wire-protocol command. Both sides parse the script into this shape
/// and translate each command into their own internal representation.
#[derive(Debug, Clone)]
pub enum Command {
    /// `SET k v` — write `v` for `k`.
    Set { key: String, value: String },
    /// `GET k` — read `k`.
    Get { key: String },
    /// `DEL k` — remove `k`.
    Del { key: String },
    /// `SUM` — total entry count across all shards.
    Sum,
    /// `QUIT` — stop driving the script.
    Quit,
}

/// Parses a newline-delimited script.
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
        Some("SUM") => Ok(Command::Sum),
        Some("QUIT") => Ok(Command::Quit),
        other => anyhow::bail!("unknown command {other:?} in {line:?}"),
    }
}

/// FNV-1a 64 — same byte-level hash that
/// `tina_runtime::sharded::ShardPlacement` uses internally. The Tokio
/// side calls this directly so both implementations land each key on
/// the same shard.
pub fn fnv1a64(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in bytes {
        h ^= u64::from(*b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}
