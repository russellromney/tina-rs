//! Public runner proof for the HTTP body-streaming specimen.
//!
//! Characterization pins the streamed byte counts and the one-chunk
//! in-flight high water. Public smoke exercises the documented Tina
//! path, including the chunked round-trip.

use specimen_http_body_streaming::{CHUNK_BYTES, RESPONSE_BODY_BYTES, Report, tina_impl};

fn assert_streamed(report: Report) {
    // /big — known length, slow reader.
    assert_eq!(report.bytes_received, RESPONSE_BODY_BYTES);
    assert!(report.status_ok);
    assert!(report.exit_clean, "metrics must drain on shutdown");

    // Pressure: peak body resident is exactly one chunk.
    let hw = report
        .tina_response_high_water
        .expect("tina side reports response high water");
    assert_eq!(hw, CHUNK_BYTES);
    assert!(hw < RESPONSE_BODY_BYTES);

    // /big-chunked — round-trip through the chunked decoder.
    let decoded = report
        .tina_chunked_decoded_bytes
        .expect("tina side reports chunked decoded length");
    assert_eq!(decoded, RESPONSE_BODY_BYTES);
    let wire = report
        .tina_chunked_wire_bytes
        .expect("tina side reports chunked wire length");
    assert!(wire > decoded, "framing overhead must be visible");

    let line = report
        .tina_capacity_discovery_line
        .expect("tina side reports capacity discovery line");
    assert!(
        line.contains("surface=specimen_http_body_streaming.response_body"),
        "{line}"
    );
    assert!(line.contains("weight_unit=bytes"), "{line}");
    assert!(line.contains("shared_scope=http.bodies"), "{line}");
    assert!(
        line.contains(&format!("high_weight={CHUNK_BYTES}")),
        "{line}"
    );
}

/// Pins streaming byte counts before/after host-result migration.
#[test]
fn public_characterization() {
    assert_streamed(tina_impl::run().expect("tina side ran"));
}

/// Documented public runner path: `tina_impl::run()`.
#[test]
fn public_smoke() {
    assert_streamed(tina_impl::run().expect("tina side ran"));
}
