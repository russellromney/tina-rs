//! Public runner proof for the local I/O, codec, and IPC specimens.
//!
//! Characterization pins the exact byte/frame arithmetic of each
//! scripted flow on the deterministic simulator, plus the platform's
//! live Unix rail contract. Public smoke exercises the documented `all`
//! binary path — the same eight public flows in the same order.

use specimen_local_io_codec_ipc::{
    SpecimenReport, admin_socket, file_ingest, framed_keyspace, live_unix_smoke,
};

/// The specimen flows use fixed `/tmp/specimen_file_*` paths; the two
/// tests in this target run in parallel threads, so serialize them to
/// keep each run's files away from the other's.
static FLOWS_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// The flows the documented `all` mode runs, in the same order.
fn run_all_flows() -> anyhow::Result<Vec<SpecimenReport>> {
    let _guard = FLOWS_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    Ok(vec![
        file_ingest::smoke()?,
        file_ingest::bad_input_cap_reached()?,
        file_ingest::copy_smoke()?,
        admin_socket::smoke()?,
        admin_socket::bad_input_line_too_long()?,
        framed_keyspace::smoke()?,
        framed_keyspace::bad_input_frame_too_large()?,
        live_unix_smoke::smoke()?,
    ])
}

/// Documented public runner path:
/// `cargo run --manifest-path examples/specimen_local_io_codec_ipc/Cargo.toml -- all`.
#[test]
fn public_smoke() {
    let reports = run_all_flows().expect("all specimen flows ran");
    assert_eq!(reports.len(), 8, "the all mode runs eight flows");
    for report in &reports {
        assert!(report.ok, "specimen flow failed: {report:?}");
    }
}

/// Pins the exact bytes/frames each scripted flow moves.
#[test]
fn public_characterization() {
    let reports = run_all_flows().expect("all specimen flows ran");
    let by_name = |name: &str| -> &SpecimenReport {
        reports
            .iter()
            .find(|report| report.name == name)
            .unwrap_or_else(|| panic!("flow {name} present"))
    };

    // 43-byte payload ("the quick brown fox jumps over the lazy dog"),
    // 8-byte chunks, cap 64: six chunks transferred, loop ends at EOF.
    let ingest = by_name("file_ingest");
    assert!(ingest.ok, "{ingest:?}");
    assert_eq!(ingest.bytes, 43);
    assert_eq!(ingest.frames, 6);

    // 24-byte payload, 4-byte chunks, cap 8: honest CapReached after
    // two chunks, exactly 8 bytes transferred.
    let capped = by_name("file_ingest:cap_reached");
    assert!(capped.ok, "{capped:?}");
    assert_eq!(capped.bytes, 8);
    assert_eq!(capped.frames, 2);

    // 37-byte payload ("copy me through a bounded two-FD pump") through
    // the two-FD pump: one copy, both file closes settled.
    let copy = by_name("file_copy");
    assert!(copy.ok, "{copy:?}");
    assert_eq!(copy.bytes, 37);
    assert_eq!(copy.frames, 1);

    // ping/status/shutdown: the server decodes 3 lines; `ok` pins the
    // exact decoded replies ["ok ping", "ok status"].
    let admin = by_name("admin_socket");
    assert!(admin.ok, "{admin:?}");
    assert_eq!(admin.frames, 3);

    // An over-cap raw line is rejected as malformed-or-full, never
    // decoded as a frame.
    let long_line = by_name("admin_socket:line_too_long");
    assert!(long_line.ok, "{long_line:?}");

    // set:a=1 / set:b=2 / get:a: 3 server frames; `ok` pins the exact
    // ack frames ["ack:set:a=1", "ack:set:b=2", "ack:get:a"].
    let keyspace = by_name("framed_keyspace");
    assert!(keyspace.ok, "{keyspace:?}");
    assert_eq!(keyspace.frames, 3);

    // A raw frame whose U16 prefix exceeds the body cap is refused
    // before allocation.
    let oversized = by_name("framed_keyspace:frame_too_large");
    assert!(oversized.ok, "{oversized:?}");

    // Live rail: bind+close Ok on Unix, typed Unsupported off Unix; the
    // probe moves no bytes or frames either way.
    let live = by_name("live_unix_smoke");
    assert!(live.ok, "{live:?}");
    assert_eq!((live.bytes, live.frames), (0, 0));
}
