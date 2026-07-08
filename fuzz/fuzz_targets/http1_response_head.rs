//! No-panic fuzz for HTTP/1.1 response-head parsing under default limits.

#![no_main]

use libfuzzer_sys::fuzz_target;
use tina_http::parse::parse_response_head;
use tina_http::types::HttpLimits;

fuzz_target!(|data: &[u8]| {
    let limits = HttpLimits::default();
    let _ = parse_response_head(data, &limits);
});
