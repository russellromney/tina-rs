//! H7: a service trait arg named `encoding` would shadow the builder's own
//! `encoding` local and silently encode the encoder default onto the wire
//! instead of the caller's value. Reject it with a clear, spanned diagnostic.

use tina_rpc::service;

#[service(name = "Bank")]
pub trait Bank {
    fn charge(&mut self, encoding: u64) -> u64;
}

fn main() {}
