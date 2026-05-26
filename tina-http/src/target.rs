//! Outbound HTTP/1.1 target: plain TCP or native TLS. TLS variants
//! carry explicit DER trust roots — no system roots, no defaults.

use std::net::SocketAddr;

/// Where to send an outbound HTTP/1.1 call.
///
/// Both variants accept a `host` value that decides the wire `Host:`
/// header. On HTTP it is `Option<String>` — `None` means the caller
/// is responsible for setting `Host:` themselves (HTTP/1.0 style).
/// On HTTPS it is required as an [`HttpHostPolicy`] because SNI and
/// `Host:` need to agree by default.
#[derive(Debug, Clone)]
pub enum HttpTarget {
    Http {
        addr: SocketAddr,
        /// Wire `Host:` header. `None` leaves the caller's request
        /// untouched; `Some(value)` is inserted (and conflicts with
        /// a caller-supplied Host).
        host: Option<String>,
    },
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
    /// Plain HTTP target, caller-managed `Host:`.
    pub fn http(addr: SocketAddr) -> Self {
        Self::Http { addr, host: None }
    }

    /// Plain HTTP target with the wire `Host:` value supplied here.
    /// A caller-set `Host:` header on the request will conflict.
    pub fn http_with_host(addr: SocketAddr, host: impl Into<String>) -> Self {
        Self::Http {
            addr,
            host: Some(host.into()),
        }
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
    /// Backward-compatible: bare `SocketAddr` → plain HTTP, caller-managed Host.
    fn from(addr: SocketAddr) -> Self {
        Self::Http { addr, host: None }
    }
}

/// DER trust roots for a TLS client. Construct via
/// [`TlsTrustRoots::from_der`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
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
