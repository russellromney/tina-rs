//! The HPACK soundness walker must never admit a block that panics
//! `hpack::Decoder::decode`. This mirrors the production guard (decode only
//! when sound); under the fuzzer's panic=abort it crashes exactly when the
//! walker is unsound, so a clean run is evidence the walker is complete.

#![no_main]

use libfuzzer_sys::fuzz_target;
use tina_http::http2::fuzzing::fuzz_hpack_guarded_decode;

fuzz_target!(|data: &[u8]| {
    fuzz_hpack_guarded_decode(data);
});
