//! Specimen acceptance tests: each smoke path and each bad-input proof
//! must pass.

use specimen_local_io_codec_ipc::{
    admin_socket, file_ingest, framed_keyspace, live_unix_unsupported_smoke,
};

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

/// Live Unix-domain rails return typed `Unsupported` from the live
/// driver this slice. This is an honest deferral. The smoke drives the
/// real `LocalSystem` runtime and asserts the typed answer, so a
/// regression that silently changes the live deferral fails here.
#[test]
fn live_unix_unsupported_pin() {
    let report = live_unix_unsupported_smoke::smoke();
    assert!(
        report.ok,
        "live unix_bind must report typed Unsupported: {report:?}"
    );
}
