//! Outbound call target: plain HTTP or native HTTPS.
//!
//! `HttpTarget` is the explicit "where and how" of an outbound HTTP/1.1
//! call. The plain TCP variant carries only the socket address. The
//! TLS variant carries the address, the server name validated by
//! rustls during handshake, an explicit set of DER trust roots (no
//! system roots), and a [`HttpHostPolicy`] that decides which name
//! gets sent in the request `Host:` header.
//!
//! No defaults are hidden: a TLS target can only be built by spelling
//! out its server name and roots.

use std::net::SocketAddr;

/// Where to send an outbound HTTP/1.1 call.
#[derive(Debug, Clone)]
pub enum HttpTarget {
    /// Plain TCP target.
    Http(SocketAddr),
    /// Native HTTPS target. `server_name` is what rustls verifies
    /// during the TLS handshake; `trust_roots` is the explicit set
    /// of DER root certificates the client trusts (no system roots);
    /// `host` decides which name lands in the request `Host:` header.
    Https {
        addr: SocketAddr,
        server_name: String,
        trust_roots: TlsTrustRoots,
        host: HttpHostPolicy,
    },
}

impl HttpTarget {
    /// Plain TCP target.
    pub fn http(addr: SocketAddr) -> Self {
        Self::Http(addr)
    }

    /// HTTPS target with `Host:` defaulted to `server_name` and
    /// explicit DER trust roots.
    pub fn https(addr: SocketAddr, server_name: impl Into<String>, trust_roots: TlsTrustRoots) -> Self {
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

/// Explicit DER root certificates for a TLS client. There is no
/// system-roots default. Construct via [`TlsTrustRoots::from_der`].
#[derive(Debug, Clone, Default)]
pub struct TlsTrustRoots {
    /// DER-encoded trust root certificates (typically self-signed
    /// for tests, or a single private CA in production).
    pub root_certificates_der: Vec<Vec<u8>>,
}

impl TlsTrustRoots {
    /// Builds a `TlsTrustRoots` from DER bytes.
    pub fn from_der(roots: Vec<Vec<u8>>) -> Self {
        Self {
            root_certificates_der: roots,
        }
    }
}

/// Where the request `Host:` header value comes from on an HTTPS
/// call. `UseServerName` mirrors the SNI name (and certificate name)
/// so all three agree by default. `Explicit(name)` allows a deliberate
/// override; the runtime keeps the SNI name intact so cert validation
/// is unaffected.
#[derive(Debug, Clone)]
pub enum HttpHostPolicy {
    /// `Host:` value is `server_name`.
    UseServerName,
    /// `Host:` value is the supplied string. SNI / cert name remain
    /// `server_name`.
    Explicit(String),
}
