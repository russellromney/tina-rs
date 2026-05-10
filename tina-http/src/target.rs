//! Outbound HTTP/1.1 target: plain TCP or native TLS. TLS variants
//! carry explicit DER trust roots — no system roots, no defaults.

use std::net::SocketAddr;

/// Where to send an outbound HTTP/1.1 call.
#[derive(Debug, Clone)]
pub enum HttpTarget {
    Http(SocketAddr),
    Https {
        addr: SocketAddr,
        /// Validated by rustls during the TLS handshake.
        server_name: String,
        /// Explicit DER roots; no system roots.
        trust_roots: TlsTrustRoots,
        /// Source of the wire `Host:` header.
        host: HttpHostPolicy,
    },
}

impl HttpTarget {
    pub fn http(addr: SocketAddr) -> Self {
        Self::Http(addr)
    }

    /// HTTPS target with `Host:` defaulted to `server_name`.
    pub fn https(
        addr: SocketAddr,
        server_name: impl Into<String>,
        trust_roots: TlsTrustRoots,
    ) -> Self {
        Self::Https {
            addr,
            server_name: server_name.into(),
            trust_roots,
            host: HttpHostPolicy::UseServerName,
        }
    }
}

impl From<SocketAddr> for HttpTarget {
    fn from(addr: SocketAddr) -> Self {
        Self::Http(addr)
    }
}

/// DER trust roots for a TLS client. Construct via
/// [`TlsTrustRoots::from_der`].
#[derive(Debug, Clone, Default)]
pub struct TlsTrustRoots {
    pub root_certificates_der: Vec<Vec<u8>>,
}

impl TlsTrustRoots {
    pub fn from_der(roots: Vec<Vec<u8>>) -> Self {
        Self {
            root_certificates_der: roots,
        }
    }
}

/// Source of the request `Host:` header on an HTTPS call.
/// `Explicit(name)` overrides Host without changing SNI / cert
/// verification.
#[derive(Debug, Clone)]
pub enum HttpHostPolicy {
    UseServerName,
    Explicit(String),
}
