//! H3: a service trait method whose argument shares a name reserved by the
//! generated `<method>_request` constructor (`deadline`, `correlator`,
//! `reply_to`, `max_payload`) must be rejected with a clear, spanned
//! diagnostic — not an opaque E0415 deep inside generated code.

use tina_rpc::service;

#[service(name = "Billing")]
pub trait Billing {
    fn charge(&mut self, deadline: u64) -> u64;
}

fn main() {}
