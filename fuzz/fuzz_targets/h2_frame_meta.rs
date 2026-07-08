//! No-panic fuzz for HTTP/2 frame-header decoding. Two bytes pick the
//! max-frame-size cap so the too-large rejection path is exercised too.

#![no_main]

use libfuzzer_sys::fuzz_target;
use tina_http::http2::fuzzing::fuzz_decode_frame_meta;

fuzz_target!(|data: &[u8]| {
    if data.len() < 2 {
        return;
    }
    let max_frame_size = usize::from(u16::from_le_bytes([data[0], data[1]]));
    let _ = fuzz_decode_frame_meta(&data[2..], max_frame_size);
});
