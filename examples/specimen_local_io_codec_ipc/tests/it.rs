//! Specimen acceptance tests: each smoke path and each bad-input proof
//! must pass.

use specimen_local_io_codec_ipc::{admin_socket, file_ingest, framed_keyspace, live_unix_smoke};

#[test]
fn file_ingest_smoke() {
    let report = file_ingest::smoke();
    assert!(report.ok, "file_ingest smoke failed: {report:?}");
    assert!(report.bytes > 0);
}

#[test]
fn file_ingest_cap_reached_is_reported() {
    let report = file_ingest::bad_input_cap_reached();
    assert!(
        report.ok,
        "file_ingest must surface CapReached honestly, not silently succeed: {report:?}"
    );
}

#[test]
fn file_copy_round_trips_through_bounded_pump() {
    let report = file_ingest::copy_smoke();
    assert!(report.ok, "bounded file copy failed: {report:?}");
    assert!(report.bytes > 0);
}

#[test]
fn admin_socket_smoke() {
    let report = admin_socket::smoke();
    assert!(report.ok, "admin_socket smoke failed: {report:?}");
    assert!(report.frames >= 2);
}

#[test]
fn admin_socket_rejects_oversize_line() {
    let report = admin_socket::bad_input_line_too_long();
    assert!(
        report.ok,
        "admin_socket must reject lines longer than the framer cap: {report:?}"
    );
}

#[test]
fn framed_keyspace_smoke() {
    let report = framed_keyspace::smoke();
    assert!(report.ok, "framed_keyspace smoke failed: {report:?}");
    assert_eq!(report.frames, 3);
}

#[test]
fn framed_keyspace_rejects_oversize_frame() {
    let report = framed_keyspace::bad_input_frame_too_large();
    assert!(
        report.ok,
        "length-delimited framer must reject body lengths over cap before allocation: {report:?}"
    );
}

/// Drives the live runtime through a `unix_bind` / `unix_close_listener`
/// cycle. On Unix the OS-backed lane must bind+close cleanly; off Unix
/// the contract is typed `Unsupported`. Either is a pass for its
/// platform, so this fails closed if the live rail regresses.
#[test]
fn live_unix_smoke_runs_on_this_platform() {
    let report = live_unix_smoke::smoke().expect("run live Unix smoke");
    assert!(report.ok, "live unix smoke failed: {report:?}");
}
