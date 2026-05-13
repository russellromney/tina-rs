//! Smoke: each side serves the slow reader, exits cleanly, and
//! Tina caps in-flight body at one chunk. The Tina side also
//! exercises the chunked route and asserts the decoded length
//! matches `RESPONSE_BODY_BYTES`.

use specimen_http_body_streaming::{CHUNK_BYTES, RESPONSE_BODY_BYTES, tina_impl, tokio_impl};

#[test]
fn tokio_smoke() {
    let r = tokio_impl::run().expect("tokio side ran");
    assert_eq!(r.bytes_received, RESPONSE_BODY_BYTES);
    assert!(r.status_ok);
    assert!(r.exit_clean);
    assert!(r.tina_response_high_water.is_none(), "no metrics on tokio");
    assert!(
        r.tina_chunked_wire_bytes.is_none(),
        "tokio side has no chunked route"
    );
    assert_eq!(
        r.tokio_response_alloc_floor,
        Some(RESPONSE_BODY_BYTES),
        "tokio side allocates the whole body Vec up front"
    );
}

#[test]
fn tina_smoke_caps_in_flight_body_and_round_trips_chunked() {
    let r = tina_impl::run().expect("tina side ran");

    // /big — known length, slow reader.
    assert_eq!(r.bytes_received, RESPONSE_BODY_BYTES);
    assert!(r.status_ok);
    assert!(r.exit_clean, "metrics must drain on shutdown");

    // Pressure: peak body resident is exactly one chunk. A
    // regression that buffered the whole body would push this up.
    let hw = r
        .tina_response_high_water
        .expect("tina side reports response high water");
    assert_eq!(
        hw, CHUNK_BYTES,
        "tina response_body_high_water = {hw}, expected exactly {CHUNK_BYTES} (one chunk). \
         A regression here means the connection started buffering whole bodies again."
    );
    assert!(
        hw < RESPONSE_BODY_BYTES,
        "high water must be strictly less than total body"
    );

    // /big-chunked — round-trip through the chunked decoder.
    let decoded = r
        .tina_chunked_decoded_bytes
        .expect("tina side reports chunked decoded length");
    assert_eq!(
        decoded, RESPONSE_BODY_BYTES,
        "chunked round-trip must recover all body bytes; got {decoded}"
    );
    let wire = r
        .tina_chunked_wire_bytes
        .expect("tina side reports chunked wire length");
    assert!(
        wire > decoded,
        "chunked wire bytes must exceed decoded bytes (framing overhead); wire={wire} decoded={decoded}"
    );

    let line = r
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
