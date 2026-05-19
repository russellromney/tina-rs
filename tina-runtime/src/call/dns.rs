//! DNS rail constructor.

use std::net::SocketAddr;
use std::time::Duration;

use super::{CallInput, CallOutput, TypedCall};

/// Returns a typed DNS lookup helper.
pub fn dns_lookup(
    host: impl Into<String>,
    port: u16,
    timeout: Duration,
) -> TypedCall<Vec<SocketAddr>> {
    TypedCall::new(
        CallInput::DnsLookup {
            host: host.into(),
            port,
            timeout,
        },
        CallOutput::into_dns_resolved,
    )
}
