//! No-panic fuzz for the RPC frame codec: full-frame decode under default
//! limits plus the body-only decoder that trusts an already-checked length
//! prefix.

#![no_main]

use libfuzzer_sys::fuzz_target;
use tina_rpc::{FrameLimits, decode, decode_body};

fuzz_target!(|data: &[u8]| {
    let limits = FrameLimits::default();
    let _ = decode(data, &limits);
    let _ = decode_body(data);
});
