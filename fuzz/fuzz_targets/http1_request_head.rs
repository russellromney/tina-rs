//! No-panic fuzz for HTTP/1.1 request-head parsing under default limits.

#![no_main]

use libfuzzer_sys::fuzz_target;
use tina_http::parse::parse_request_head;
use tina_http::types::HttpLimits;

fuzz_target!(|data: &[u8]| {
    let limits = HttpLimits::default();
    let _ = parse_request_head(data, &limits);
});
