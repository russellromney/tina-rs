//! Hand-rolled: the same webhook outbox over a flat append-only log.
//!
//! This is the baseline the durable form replaces. It works for the happy
//! path, but notice everything it has to invent and what it still skips:
//!
//! - append-before-send is a convention here (an `ENQ` line, fsynced, before
//!   the send), not a type rule — nothing stops a future edit from sending
//!   first;
//! - dedup of completed work is a manual `ENQ` minus `DONE` set difference;
//! - there is no checksum, so a torn write is undetectable;
//! - compaction rewrites the log in place and is **not** atomic — a crash
//!   mid-rewrite corrupts it, with no commit fence to flag the uncertainty.
//!
//! The durable form ([`crate::tina_impl`]) gets all of that for free.

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::Path;

use tempfile::TempDir;

use crate::{Report, WEBHOOKS};

pub fn run() -> anyhow::Result<Report> {
    let dir = TempDir::new()?;
    let log = dir.path().join("webhooks.log");
    let mut delivered: Vec<String> = Vec::new();
    let mut report = Report::default();

    // --- Phase A: enqueue three, send + mark two, crash before marking the third.
    for (position, hook) in WEBHOOKS.iter().enumerate() {
        let id = (position as u64) + 1;
        // Append-before-send: record intent and fsync, *then* deliver.
        append_line(&log, &format!("ENQ {id} {hook}"))?;
        delivered.push((*hook).to_owned());
        report.phase_a_sent += 1;
        if position != WEBHOOKS.len() - 1 {
            append_line(&log, &format!("DONE {id}"))?;
            report.phase_a_marked += 1;
        }
    }
    report.journal_records_before_compaction = count_lines(&log)?;

    // --- Phase B: recover by parsing the log into ENQ-minus-DONE.
    let pending = parse_pending(&log)?;
    report.recovered_pending = pending.len() as u64;
    // No commit fence exists, so "clean" is an assumption a real hand-rolled
    // version would have to earn with its own crash-during-commit story.
    report.exit_clean = true;

    // Compact: rewrite the log with only the still-pending ENQ lines. NOT
    // atomic — a crash here loses the log.
    let mut compacted = String::new();
    for (id, payload) in &pending {
        compacted.push_str(&format!("ENQ {id} {payload}\n"));
    }
    fs::write(&log, compacted)?;
    report.journal_records_after_compaction = count_lines(&log)?;

    // Resume the unsent webhooks (at-least-once: the third is delivered again).
    for (id, payload) in pending {
        delivered.push(payload);
        report.phase_b_resent += 1;
        append_line(&log, &format!("DONE {id}"))?;
    }

    report.final_marked = report.phase_a_marked + report.phase_b_resent;
    Ok(report)
}

fn append_line(log: &Path, line: &str) -> anyhow::Result<()> {
    let mut file = OpenOptions::new().create(true).append(true).open(log)?;
    writeln!(file, "{line}")?;
    file.sync_all()?;
    Ok(())
}

fn count_lines(log: &Path) -> anyhow::Result<u64> {
    let text = fs::read_to_string(log).unwrap_or_default();
    Ok(text.lines().filter(|line| !line.is_empty()).count() as u64)
}

/// Parse the log into still-pending `(id, payload)` pairs, in enqueue order.
fn parse_pending(log: &Path) -> anyhow::Result<Vec<(u64, String)>> {
    let text = fs::read_to_string(log).unwrap_or_default();
    let mut order: Vec<u64> = Vec::new();
    let mut payloads: std::collections::HashMap<u64, String> = std::collections::HashMap::new();
    let mut done: std::collections::HashSet<u64> = std::collections::HashSet::new();
    for line in text.lines().filter(|line| !line.is_empty()) {
        let mut parts = line.splitn(3, ' ');
        match (parts.next(), parts.next(), parts.next()) {
            (Some("ENQ"), Some(id), Some(payload)) => {
                let id: u64 = id.parse()?;
                order.push(id);
                payloads.insert(id, payload.to_owned());
            }
            (Some("DONE"), Some(id), None) => {
                done.insert(id.parse()?);
            }
            _ => anyhow::bail!("corrupt log line: {line:?}"),
        }
    }
    Ok(order
        .into_iter()
        .filter(|id| !done.contains(id))
        .filter_map(|id| payloads.get(&id).map(|payload| (id, payload.clone())))
        .collect())
}
