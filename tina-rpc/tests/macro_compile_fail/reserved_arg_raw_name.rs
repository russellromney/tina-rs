//! Raw-ident spelling of a reserved generated request parameter must be
//! rejected directly by the macro, not by opaque duplicate-binding errors.

use tina_rpc::service;

#[service(name = "Billing")]
pub trait Billing {
    fn charge(&mut self, r#deadline: u64) -> u64;
}

fn main() {}
