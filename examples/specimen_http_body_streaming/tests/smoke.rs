//! Smoke: each side delivers the full body to the slow reader and
//! exits cleanly. Tina additionally caps in-flight body bytes near
//! one chunk's worth.

use specimen_http_body_streaming::{CHUNK_BYTES, RESPONSE_BODY_BYTES, tina_impl, tokio_impl};

#[test]
fn tokio_smoke() {
    let r = tokio_impl::run().expect("tokio side ran");
    assert_eq!(r.bytes_received, RESPONSE_BODY_BYTES);
    assert!(r.status_ok);
    assert!(r.exit_clean);
    assert!(r.tina_response_high_water.is_none(), "no metrics on tokio");
    assert_eq!(
        r.tokio_response_alloc_floor,
        Some(RESPONSE_BODY_BYTES),
        "tokio side allocates the whole body Vec up front"
    );
}

#[test]
fn tina_smoke_caps_in_flight_body_near_one_chunk() {
    let r = tina_impl::run().expect("tina side ran");
    assert_eq!(r.bytes_received, RESPONSE_BODY_BYTES);
    assert!(r.status_ok);
    assert!(r.exit_clean, "metrics must drain on shutdown");
    let hw = r
        .tina_response_high_water
        .expect("tina side reports response high water");
    // The connection pulls the next chunk only after the previous
    // one fully drains, so peak charge equals exactly one chunk.
    // A regression that buffered whole bodies would push this up.
    assert_eq!(
        hw, CHUNK_BYTES,
        "tina response_body_high_water = {hw}, expected exactly {CHUNK_BYTES} (one chunk). \
         A regression here means the connection started buffering whole bodies again."
    );
    assert!(
        hw < RESPONSE_BODY_BYTES,
        "high water must be strictly less than total body"
    );
}
