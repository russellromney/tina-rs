//! Public runner proof for the custom-codec extension.
//!
//! Public smoke drives the documented `run()` over the simulator's
//! deterministic Unix rails. Characterization pins the exact frame and
//! echo transcript, the codec's typed Full/Malformed rejections as
//! distinct from transport failure, and byte-identical replay.

use std::path::PathBuf;

use tina_extension_custom_codec::{
    CodecRejection, Report, SemicolonMalformed, run, run_codec_service,
};

/// Bytes the client must collect on the documented happy path:
/// "ok:ping;ok:status;" echoed before the `quit` frame closes the stream.
const EXPECTED_ECHO: &[u8] = b"ok:ping;ok:status;";

fn assert_report(report: &Report) {
    assert_eq!(report.frames, 3, "ping, status, quit");
    assert_eq!(
        report.echoed_bytes,
        EXPECTED_ECHO.len() as u64,
        "exact echo transcript"
    );
    assert!(report.oversize_rejected, "oversize frame must surface Full");
    assert!(
        report.malformed_rejected,
        "embedded NUL must surface Malformed"
    );
    assert!(
        report.io_failures.is_empty(),
        "no Unix rail failures: {:?}",
        report.io_failures
    );
}

/// Documented public runner path: `run()`.
#[test]
fn public_smoke() {
    assert_report(&run());
}

/// Pins the exact deterministic exchange, the typed rejections, and the
/// replay fact.
#[test]
fn public_characterization() {
    assert_report(&run());

    // Per-process socket paths: `cargo test --all-targets` may run this
    // target alongside the crate's in-lib tests that use their own fixed
    // paths, and stale sockets from an earlier run must never confuse the
    // codec rails.
    let sock = |name: &str| {
        PathBuf::from(format!(
            "/tmp/tina_ext_codec_public_{name}_{}.sock",
            std::process::id()
        ))
    };

    // Codec-policy rejections stay typed and distinct from rail failure.
    let full = run_codec_service(sock("full"), b"abcdef;".to_vec(), 2);
    assert_eq!(full.rejection, Some(CodecRejection::Full));
    assert!(full.io_failures.is_empty());
    let malformed = run_codec_service(sock("malformed"), b"a\0b;".to_vec(), 8);
    assert_eq!(
        malformed.rejection,
        Some(CodecRejection::Malformed(SemicolonMalformed::EmbeddedNul))
    );
    assert!(malformed.io_failures.is_empty());

    // Replay: same bytes in, byte-identical frames and echoes out.
    let probe = |suffix: &str| {
        run_codec_service(sock(&format!("replay_{suffix}")), b"x;y;quit;".to_vec(), 64)
    };
    let first = probe("a");
    let second = probe("b");
    assert_eq!(
        first.server_saw,
        [b"x".to_vec(), b"y".to_vec(), b"quit".to_vec()]
    );
    assert_eq!(first.client_received, b"ok:x;ok:y;");
    assert_eq!(first.rejection, None);
    assert_eq!(first.server_saw, second.server_saw);
    assert_eq!(first.client_received, second.client_received);
    assert_eq!(first.rejection, second.rejection);
    assert_eq!(first.io_failures, second.io_failures);
}
