//! No-panic fuzz for the gRPC length-prefixed frame reassembler
//! (`next_grpc_frame_boundary` in `grpc.rs`). Exercises only the
//! length-prefix framing state machine — the 5-byte header check, the
//! `max_message_bytes` cap, and the boundary/drain math — never the `prost`
//! decode step, which is fuzzed upstream.

#![no_main]

use libfuzzer_sys::fuzz_target;
use tina_http::grpc::fuzzing::fuzz_grpc_frame_reassembly;

fuzz_target!(|data: &[u8]| {
    fuzz_grpc_frame_reassembly(data);
});
