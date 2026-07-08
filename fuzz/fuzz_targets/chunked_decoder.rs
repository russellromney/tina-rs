//! No-panic + cap-invariant fuzz for the incremental chunked decoder.
//!
//! Input layout: two bytes pick the decoded-body cap, one byte picks a
//! split point, the rest is the wire input fed in two pieces so every
//! CRLF/size-line seam can land on a feed boundary.

#![no_main]

use libfuzzer_sys::fuzz_target;
use tina_http::chunked_decoder::ChunkedDecoder;

fuzz_target!(|data: &[u8]| {
    if data.len() < 3 {
        return;
    }
    let cap = usize::from(u16::from_le_bytes([data[0], data[1]]));
    let wire = &data[3..];
    let split = usize::from(data[2]).min(wire.len());

    let mut decoder = ChunkedDecoder::new(cap);
    let mut decoded = Vec::new();
    let (first, _) = decoder.feed_all(&wire[..split], &mut decoded);
    assert!(
        decoded.len() <= cap,
        "decoded {} exceeded cap {cap} after first feed ({first:?})",
        decoded.len()
    );
    let (second, _) = decoder.feed_all(&wire[split..], &mut decoded);
    assert!(
        decoded.len() <= cap,
        "decoded {} exceeded cap {cap} after second feed ({second:?})",
        decoded.len()
    );
});
