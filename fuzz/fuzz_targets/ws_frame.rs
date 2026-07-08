//! No-panic fuzz for the hand-rolled WebSocket frame parser (client and
//! server framing: 7/16/64-bit lengths, mask XOR, buffer draining).

#![no_main]

use libfuzzer_sys::fuzz_target;
use tina_http::websocket::fuzzing::fuzz_parse_frames;

fuzz_target!(|data: &[u8]| {
    fuzz_parse_frames(data);
});
