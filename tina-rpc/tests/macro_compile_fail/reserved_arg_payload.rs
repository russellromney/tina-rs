//! H7: a service trait arg named `payload` collides with the builder's own
//! `payload` local. Reject it with a clear, spanned diagnostic.

use tina_rpc::service;

#[service(name = "Bank")]
pub trait Bank {
    fn charge(&mut self, payload: u64) -> u64;
}

fn main() {}
