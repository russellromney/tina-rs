use std::path::{Path, PathBuf};

use tempfile::TempDir;
use tokio::fs::{File, OpenOptions};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::runtime::Builder;

use super::{PHASE_A_INCREMENTS, PHASE_B_INCREMENTS, SideReport};

// A snapshot is two u64s: last_journal_index, then counter value.
const SNAPSHOT_BYTES: usize = 16;
// One journal record is two u64s: record_index, then counter value.
const RECORD_BYTES: usize = 16;

pub(crate) fn run() -> SideReport {
    let runtime = Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("tokio runtime");

    let dir = TempDir::new().expect("tempdir");
    runtime.block_on(async_run(dir.path().to_path_buf()))
}

async fn async_run(dir: PathBuf) -> SideReport {
    let snapshot_path = dir.join("counter.snap");
    let journal_path = dir.join("counter.journal");

    // Phase A: fresh dir, recover (empty), apply increments, snapshot.
    let mut counter = recover(&snapshot_path, &journal_path).await;
    assert_eq!(counter.value, 0);
    assert_eq!(counter.last_journal_index, 0);

    for _ in 0..PHASE_A_INCREMENTS {
        counter.value += 1;
        counter.last_journal_index += 1;
        append_journal(&journal_path, counter.last_journal_index, counter.value).await;
    }
    let phase_a_final = counter.value;

    commit_snapshot(&snapshot_path, counter.last_journal_index, counter.value).await;
    let snapshot_committed = true;

    // Simulate process restart by dropping the in-memory state and reading
    // back from disk. This is the apples-to-apples shape against Tina's
    // separate ThreadedRuntime processes.
    drop(counter);

    let mut counter = recover(&snapshot_path, &journal_path).await;
    let phase_b_recovered = counter.value;

    let mut journal_records_phase_b = 0;
    for _ in 0..PHASE_B_INCREMENTS {
        counter.value += 1;
        counter.last_journal_index += 1;
        append_journal(&journal_path, counter.last_journal_index, counter.value).await;
        journal_records_phase_b += 1;
    }
    let phase_b_final = counter.value;

    SideReport {
        phase_a_final,
        phase_b_recovered,
        phase_b_final,
        snapshot_committed,
        journal_records_phase_b,
        exit_clean: true,
    }
}

#[derive(Debug)]
struct Counter {
    value: u64,
    last_journal_index: u64,
}

async fn recover(snapshot_path: &Path, journal_path: &Path) -> Counter {
    let (snapshot_value, last_journal_index) = match read_all(snapshot_path).await {
        Some(bytes) if bytes.len() == SNAPSHOT_BYTES => {
            let last_idx = u64::from_le_bytes(bytes[..8].try_into().unwrap());
            let value = u64::from_le_bytes(bytes[8..16].try_into().unwrap());
            (value, last_idx)
        }
        Some(other) => {
            // Hand-rolled framing means hand-rolled recovery decisions. Tina's
            // snapshot helper rejects mis-sized payloads centrally; here every
            // app has to decide what "corrupt" means.
            panic!("snapshot file has unexpected length {}", other.len());
        }
        None => (0, 0),
    };

    let mut value = snapshot_value;
    let mut last_seen_index = last_journal_index;
    if let Some(bytes) = read_all(journal_path).await {
        // The journal is a stream of fixed-size records. A real shop would
        // also need to detect torn tails (write crashed mid-record) and
        // decide whether to truncate; we skip that here, but the choice is
        // ours to make every time.
        if bytes.len() % RECORD_BYTES != 0 {
            panic!(
                "journal file has unexpected length {} (not a multiple of {})",
                bytes.len(),
                RECORD_BYTES
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

    Counter {
        value,
        last_journal_index: last_seen_index,
    }
}

async fn append_journal(journal_path: &Path, index: u64, value: u64) {
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(journal_path)
        .await
        .expect("open journal");
    let mut buf = [0u8; RECORD_BYTES];
    buf[..8].copy_from_slice(&index.to_le_bytes());
    buf[8..16].copy_from_slice(&value.to_le_bytes());
    file.write_all(&buf).await.expect("write journal record");
    file.flush().await.expect("flush journal");
    file.sync_all().await.expect("sync journal");
}

async fn commit_snapshot(snapshot_path: &Path, last_journal_index: u64, value: u64) {
    let tmp = snapshot_path.with_extension("snap.tmp");
    let mut file = File::create(&tmp).await.expect("create snapshot tmp");
    let mut buf = [0u8; SNAPSHOT_BYTES];
    buf[..8].copy_from_slice(&last_journal_index.to_le_bytes());
    buf[8..16].copy_from_slice(&value.to_le_bytes());
    file.write_all(&buf).await.expect("write snapshot");
    file.flush().await.expect("flush snapshot");
    file.sync_all().await.expect("sync snapshot");
    tokio::fs::rename(&tmp, snapshot_path)
        .await
        .expect("rename snapshot");
    // We deliberately do not fsync the parent directory here. A real shop
    // would; whether *this* shop thought to is the property Tina's helper
    // makes uniform.
}

async fn read_all(path: &Path) -> Option<Vec<u8>> {
    match File::open(path).await {
        Ok(mut file) => {
            let mut buf = Vec::new();
            file.read_to_end(&mut buf).await.expect("read file");
            Some(buf)
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(other) => panic!("read error on {path:?}: {other:?}"),
    }
}
