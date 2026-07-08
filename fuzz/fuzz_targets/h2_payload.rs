//! No-panic fuzz for HTTP/2 DATA/HEADERS payload padding+priority stripping.
//! One leading byte is the frame flags; the rest is the payload.

#![no_main]

use libfuzzer_sys::fuzz_target;
use tina_http::http2::fuzzing::fuzz_payload_views;

fuzz_target!(|data: &[u8]| {
    let (flags, payload) = data.split_first().map_or((0u8, &[][..]), |(f, p)| (*f, p));
    fuzz_payload_views(flags, payload);
});
