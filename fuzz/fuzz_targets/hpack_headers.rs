//! Drives the REAL production header-decode entry (gate + fast-literal path +
//! `catch_unwind`), not a copy of the gate. Under the fuzzer's panic=abort the
//! process crashes exactly when the soundness gate lets a panic input through
//! (e.g. if the gate is removed), so a clean run is evidence the shipped entry
//! contains every input. Also the only coverage of the fast-literal path that
//! runs first on every inbound header block.

#![no_main]

use libfuzzer_sys::fuzz_target;
use tina_http::http2::fuzzing::fuzz_hpack_guarded_decode;

fuzz_target!(|data: &[u8]| {
    fuzz_hpack_guarded_decode(data);
});
