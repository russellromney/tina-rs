//! Tokio reference: hand-rolled snapshot + journal framing on top
//! of `tokio::fs`. Every byte layout is the example's choice; every
//! sync point is up to the example to remember.

use std::path::{Path, PathBuf};

use tempfile::TempDir;
use tokio::fs::{File, OpenOptions};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::runtime::Builder;

use crate::{PHASE_A_INCREMENTS, PHASE_B_INCREMENTS, Report};

const SNAPSHOT_BYTES: usize = 16; // last_journal_index (u64) + value (u64)
const RECORD_BYTES: usize = 16; // record_index (u64) + value (u64)

pub fn run() -> anyhow::Result<Report> {
    let runtime = Builder::new_current_thread().enable_all().build()?;
    let dir = TempDir::new()?;
    runtime.block_on(async_run(dir.path().to_path_buf()))
}

async fn async_run(dir: PathBuf) -> anyhow::Result<Report> {
    let snapshot_path = dir.join("counter.snap");
    let journal_path = dir.join("counter.journal");

    // Phase A: fresh dir, recover (empty), apply increments, snapshot.
    let mut counter = recover(&snapshot_path, &journal_path).await?;
    assert_eq!(counter.value, 0);
    assert_eq!(counter.last_journal_index, 0);
    for _ in 0..PHASE_A_INCREMENTS {
        counter.value += 1;
        counter.last_journal_index += 1;
        append_journal(&journal_path, counter.last_journal_index, counter.value).await?;
    }
    let phase_a_final = counter.value;
    commit_snapshot(&snapshot_path, counter.last_journal_index, counter.value).await?;
    // Simulate process restart: shadow the in-memory counter with a
    // fresh `recover()` against the same files.

    let mut counter = recover(&snapshot_path, &journal_path).await?;
    let phase_b_recovered = counter.value;
    let mut journal_records_phase_b = 0;
    for _ in 0..PHASE_B_INCREMENTS {
        counter.value += 1;
        counter.last_journal_index += 1;
        append_journal(&journal_path, counter.last_journal_index, counter.value).await?;
        journal_records_phase_b += 1;
    }
    let phase_b_final = counter.value;

    Ok(Report {
        phase_a_final,
        phase_b_recovered,
        phase_b_final,
        snapshot_committed: true,
        journal_records_phase_b,
        exit_clean: true,
    })
}

#[derive(Debug)]
struct Counter {
    value: u64,
    last_journal_index: u64,
}

async fn recover(snapshot_path: &Path, journal_path: &Path) -> anyhow::Result<Counter> {
    let (snapshot_value, last_journal_index) = match read_all(snapshot_path).await? {
        Some(bytes) if bytes.len() == SNAPSHOT_BYTES => {
            let last_idx = u64::from_le_bytes(bytes[..8].try_into().unwrap());
            let value = u64::from_le_bytes(bytes[8..16].try_into().unwrap());
            (value, last_idx)
        }
        Some(other) => anyhow::bail!("snapshot file has unexpected length {}", other.len()),
        None => (0, 0),
    };

    let mut value = snapshot_value;
    let mut last_seen_index = last_journal_index;
    if let Some(bytes) = read_all(journal_path).await? {
        if bytes.len() % RECORD_BYTES != 0 {
            anyhow::bail!(
                "journal file has unexpected length {} (not a multiple of {})",
                bytes.len(),
                RECORD_BYTES,
            );
        }
        for chunk in bytes.chunks_exact(RECORD_BYTES) {
            let index = u64::from_le_bytes(chunk[..8].try_into().unwrap());
            let recorded = u64::from_le_bytes(chunk[8..16].try_into().unwrap());
            if index > last_journal_index {
                value = recorded;
                last_seen_index = index;
            }
        }
    }

    Ok(Counter {
        value,
        last_journal_index: last_seen_index,
    })
}

async fn append_journal(path: &Path, index: u64, value: u64) -> anyhow::Result<()> {
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .await?;
    let mut buf = [0u8; RECORD_BYTES];
    buf[..8].copy_from_slice(&index.to_le_bytes());
    buf[8..16].copy_from_slice(&value.to_le_bytes());
    file.write_all(&buf).await?;
    file.flush().await?;
    file.sync_all().await?;
    Ok(())
}

async fn commit_snapshot(path: &Path, last_journal_index: u64, value: u64) -> anyhow::Result<()> {
    let tmp = path.with_extension("snap.tmp");
    let mut file = File::create(&tmp).await?;
    let mut buf = [0u8; SNAPSHOT_BYTES];
    buf[..8].copy_from_slice(&last_journal_index.to_le_bytes());
    buf[8..16].copy_from_slice(&value.to_le_bytes());
    file.write_all(&buf).await?;
    file.flush().await?;
    file.sync_all().await?;
    tokio::fs::rename(&tmp, path).await?;
    // We deliberately do not fsync the parent directory here. A real
    // shop would; whether *this* shop thought to is the property
    // Tina's helper makes uniform.
    Ok(())
}

async fn read_all(path: &Path) -> anyhow::Result<Option<Vec<u8>>> {
    match File::open(path).await {
        Ok(mut file) => {
            let mut buf = Vec::new();
            file.read_to_end(&mut buf).await?;
            Ok(Some(buf))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
    }
}
