//! Unresolved, user-facing endpoint types.
//!
//! An endpoint owns user intent: host, port, authority/`Host:`, SNI/server
//! name, trust roots, path, ALPN. A resolved low-level target
//! ([`HttpTarget`], [`Http2Target`], [`WebSocketTarget`], [`GrpcTarget`])
//! owns one chosen [`SocketAddr`]. The endpoint resolves into a target once
//! the connect helper has picked a winning address — no truth is lost in a
//! string along the way.

use std::net::SocketAddr;

use crate::http2::{AlpnProtocols, Http2Target};
use crate::grpc::GrpcLimits;
use crate::grpc_client::GrpcTarget;
use crate::target::{HttpHostPolicy, HttpTarget, TlsTrustRoots};
use crate::websocket_client::WebSocketTarget;

/// Stable identity for one managed endpoint.
///
/// The manager assigns ids; the helper and reports only carry them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct EndpointId(u64);

impl EndpointId {
    /// Build an id from a raw value.
    pub const fn new(value: u64) -> Self {
        Self(value)
    }

    /// The raw value.
    pub const fn value(self) -> u64 {
        self.0
    }
}

/// Monotonic generation for one endpoint.
///
/// Each resolve/reconnect cycle bumps the generation so a reply that names
/// an old generation can be recognised as stale and ignored — an old
/// session can never replace the current one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct EndpointGeneration(u64);

impl EndpointGeneration {
    /// The first generation.
    pub const fn first() -> Self {
        Self(1)
    }

    /// Build a generation from a raw value.
    pub const fn new(value: u64) -> Self {
        Self(value)
    }

    /// The raw value.
    pub const fn value(self) -> u64 {
        self.0
    }

    /// The next generation. Panics on overflow (a u64 of reconnects is not
    /// reachable in practice; an overflow is a logic bug, not a runtime
    /// condition to paper over).
    #[must_use]
    pub fn next(self) -> Self {
        Self(
            self.0
                .checked_add(1)
                .expect("endpoint generation counter exhausted"),
        )
    }
}

/// The transport security a connect attempt must dial, addr-free.
///
/// This is the exact TLS truth the connector preserves: server name, trust
/// roots, and ALPN. A plain endpoint carries none.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConnectSecurity {
    /// Plain TCP.
    Plain,
    /// TLS with explicit SNI, DER trust roots, and ALPN.
    Tls {
        /// SNI / rustls server name.
        server_name: String,
        /// Explicit DER trust roots (no ambient platform store).
        trust_roots: TlsTrustRoots,
        /// ALPN protocols offered.
        alpn: AlpnProtocols,
    },
}

impl ConnectSecurity {
    /// True when this is a TLS dial.
    pub fn is_tls(&self) -> bool {
        matches!(self, Self::Tls { .. })
    }

    /// The SNI server name, when TLS.
    pub fn server_name(&self) -> Option<&str> {
        match self {
            Self::Plain => None,
            Self::Tls { server_name, .. } => Some(server_name),
        }
    }
}

/// One chosen address plus the transport security to dial it.
///
/// This is what the connector connects. It deliberately carries no
/// protocol concern (path, method, authority): those live on the endpoint
/// and are reattached when the winning address resolves into a target.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedEndpoint {
    /// The chosen socket address.
    pub addr: SocketAddr,
    /// The transport security to dial.
    pub security: ConnectSecurity,
}

impl ResolvedEndpoint {
    /// Build a resolved endpoint.
    pub fn new(addr: SocketAddr, security: ConnectSecurity) -> Self {
        Self { addr, security }
    }
}

// ---------------------------------------------------------------------------
// HTTP/1.1
// ---------------------------------------------------------------------------

/// Unresolved HTTP/1.1 endpoint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HttpEndpoint {
    host: String,
    port: u16,
    security: Http1Security,
    host_header: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Http1Security {
    Plain,
    Tls {
        server_name: String,
        trust_roots: TlsTrustRoots,
    },
}

impl HttpEndpoint {
    /// Plain HTTP endpoint. `Host:` defaults to `host`.
    pub fn http(host: impl Into<String>, port: u16) -> Self {
        Self {
            host: host.into(),
            port,
            security: Http1Security::Plain,
            host_header: None,
        }
    }

    /// HTTPS endpoint. SNI defaults to `host`; `Host:` defaults to the SNI.
    pub fn https(host: impl Into<String>, port: u16, trust_roots: TlsTrustRoots) -> Self {
        let host = host.into();
        Self {
            host: host.clone(),
            port,
            security: Http1Security::Tls {
                server_name: host,
                trust_roots,
            },
            host_header: None,
        }
    }

    /// Override the wire `Host:` header.
    #[must_use]
    pub fn host_header(mut self, host: impl Into<String>) -> Self {
        self.host_header = Some(host.into());
        self
    }

    /// Override the TLS server name (SNI) for an HTTPS endpoint. No-op on a
    /// plain endpoint.
    #[must_use]
    pub fn server_name(mut self, server_name: impl Into<String>) -> Self {
        if let Http1Security::Tls { server_name: sni, .. } = &mut self.security {
            *sni = server_name.into();
        }
        self
    }

    /// The DNS host.
    pub fn host(&self) -> &str {
        &self.host
    }

    /// The DNS port.
    pub fn port(&self) -> u16 {
        self.port
    }

    /// The effective wire authority / `Host:` value.
    pub fn authority(&self) -> String {
        self.host_header.clone().unwrap_or_else(|| self.host.clone())
    }

    /// The transport security to dial. HTTP/1 over TLS offers no ALPN.
    pub fn connect_security(&self) -> ConnectSecurity {
        match &self.security {
            Http1Security::Plain => ConnectSecurity::Plain,
            Http1Security::Tls {
                server_name,
                trust_roots,
            } => ConnectSecurity::Tls {
                server_name: server_name.clone(),
                trust_roots: trust_roots.clone(),
                alpn: AlpnProtocols::none(),
            },
        }
    }

    /// Resolve into a low-level [`HttpTarget`] at `addr`.
    pub fn resolve(&self, addr: SocketAddr) -> HttpTarget {
        match &self.security {
            Http1Security::Plain => match &self.host_header {
                Some(host) => HttpTarget::http_with_host(addr, host.clone()),
                None => HttpTarget::http_with_host(addr, self.host.clone()),
            },
            Http1Security::Tls {
                server_name,
                trust_roots,
            } => {
                let mut target = HttpTarget::https(addr, server_name.clone(), trust_roots.clone());
                if let (HttpTarget::Https { host, .. }, Some(explicit)) =
                    (&mut target, &self.host_header)
                {
                    *host = HttpHostPolicy::Explicit(explicit.clone());
                }
                target
            }
        }
    }
}

// ---------------------------------------------------------------------------
// HTTP/2
// ---------------------------------------------------------------------------

/// Unresolved HTTP/2 endpoint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Http2Endpoint {
    authority: String,
    host: String,
    port: u16,
    security: Http2Security,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Http2Security {
    H2c,
    Tls {
        server_name: String,
        trust_roots: Vec<Vec<u8>>,
    },
}

impl Http2Endpoint {
    /// Prior-knowledge cleartext h2c endpoint.
    pub fn h2c(authority: impl Into<String>, host: impl Into<String>, port: u16) -> Self {
        Self {
            authority: authority.into(),
            host: host.into(),
            port,
            security: Http2Security::H2c,
        }
    }

    /// h2 over TLS endpoint with explicit SNI and DER trust roots.
    pub fn tls(
        authority: impl Into<String>,
        host: impl Into<String>,
        port: u16,
        server_name: impl Into<String>,
        trust_roots: Vec<Vec<u8>>,
    ) -> Self {
        Self {
            authority: authority.into(),
            host: host.into(),
            port,
            security: Http2Security::Tls {
                server_name: server_name.into(),
                trust_roots,
            },
        }
    }

    /// The DNS host.
    pub fn host(&self) -> &str {
        &self.host
    }

    /// The DNS port.
    pub fn port(&self) -> u16 {
        self.port
    }

    /// The wire `:authority`.
    pub fn authority(&self) -> &str {
        &self.authority
    }

    /// The transport security to dial. h2 over TLS offers `h2` ALPN.
    pub fn connect_security(&self) -> ConnectSecurity {
        match &self.security {
            Http2Security::H2c => ConnectSecurity::Plain,
            Http2Security::Tls {
                server_name,
                trust_roots,
            } => ConnectSecurity::Tls {
                server_name: server_name.clone(),
                trust_roots: TlsTrustRoots::from_der(trust_roots.clone()),
                alpn: AlpnProtocols::h2(),
            },
        }
    }

    /// Resolve into a low-level [`Http2Target`] at `addr`.
    pub fn resolve(&self, addr: SocketAddr) -> Http2Target {
        match &self.security {
            Http2Security::H2c => Http2Target::H2c {
                authority: self.authority.clone(),
                addr,
            },
            Http2Security::Tls {
                server_name,
                trust_roots,
            } => Http2Target::Tls {
                authority: self.authority.clone(),
                addr,
                server_name: server_name.clone(),
                trust_roots: trust_roots.clone(),
                alpn: AlpnProtocols::h2(),
            },
        }
    }
}

// ---------------------------------------------------------------------------
// gRPC
// ---------------------------------------------------------------------------

/// Unresolved gRPC endpoint: an [`Http2Endpoint`] plus gRPC limits.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GrpcEndpoint {
    http2: Http2Endpoint,
    limits: GrpcLimits,
}

impl GrpcEndpoint {
    /// h2c gRPC endpoint.
    pub fn h2c(authority: impl Into<String>, host: impl Into<String>, port: u16) -> Self {
        Self {
            http2: Http2Endpoint::h2c(authority, host, port),
            limits: GrpcLimits::default(),
        }
    }

    /// h2-over-TLS gRPC endpoint.
    pub fn tls(
        authority: impl Into<String>,
        host: impl Into<String>,
        port: u16,
        server_name: impl Into<String>,
        trust_roots: Vec<Vec<u8>>,
    ) -> Self {
        Self {
            http2: Http2Endpoint::tls(authority, host, port, server_name, trust_roots),
            limits: GrpcLimits::default(),
        }
    }

    /// Override the gRPC message-size limits.
    #[must_use]
    pub fn with_limits(mut self, limits: GrpcLimits) -> Self {
        self.limits = limits;
        self
    }

    /// The underlying HTTP/2 endpoint.
    pub fn http2(&self) -> &Http2Endpoint {
        &self.http2
    }

    /// The DNS host.
    pub fn host(&self) -> &str {
        self.http2.host()
    }

    /// The DNS port.
    pub fn port(&self) -> u16 {
        self.http2.port()
    }

    /// The wire `:authority`.
    pub fn authority(&self) -> &str {
        self.http2.authority()
    }

    /// The gRPC message-size limits.
    pub fn limits(&self) -> GrpcLimits {
        self.limits
    }

    /// The transport security to dial.
    pub fn connect_security(&self) -> ConnectSecurity {
        self.http2.connect_security()
    }

    /// Resolve into a low-level [`GrpcTarget`] at `addr`.
    pub fn resolve(&self, addr: SocketAddr) -> GrpcTarget {
        GrpcTarget {
            http2: self.http2.resolve(addr),
            limits: self.limits,
        }
    }
}

// ---------------------------------------------------------------------------
// WebSocket
// ---------------------------------------------------------------------------

/// Unresolved WebSocket endpoint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WebSocketEndpoint {
    host: String,
    port: u16,
    path: String,
    security: WsSecurity,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum WsSecurity {
    Ws,
    Wss {
        server_name: String,
        trust_roots: TlsTrustRoots,
    },
}

impl WebSocketEndpoint {
    /// Plain `ws://` endpoint.
    pub fn ws(host: impl Into<String>, port: u16, path: impl Into<String>) -> Self {
        Self {
            host: host.into(),
            port,
            path: path.into(),
            security: WsSecurity::Ws,
        }
    }

    /// Secure `wss://` endpoint with explicit SNI and DER trust roots.
    pub fn wss(
        host: impl Into<String>,
        port: u16,
        path: impl Into<String>,
        server_name: impl Into<String>,
        trust_roots: TlsTrustRoots,
    ) -> Self {
        Self {
            host: host.into(),
            port,
            path: path.into(),
            security: WsSecurity::Wss {
                server_name: server_name.into(),
                trust_roots,
            },
        }
    }

    /// The DNS host.
    pub fn host(&self) -> &str {
        &self.host
    }

    /// The DNS port.
    pub fn port(&self) -> u16 {
        self.port
    }

    /// The request path.
    pub fn path(&self) -> &str {
        &self.path
    }

    /// The wire authority / `Host:` value.
    pub fn authority(&self) -> String {
        self.host.clone()
    }

    /// The transport security to dial. WSS offers no ALPN.
    pub fn connect_security(&self) -> ConnectSecurity {
        match &self.security {
            WsSecurity::Ws => ConnectSecurity::Plain,
            WsSecurity::Wss {
                server_name,
                trust_roots,
            } => ConnectSecurity::Tls {
                server_name: server_name.clone(),
                trust_roots: trust_roots.clone(),
                alpn: AlpnProtocols::none(),
            },
        }
    }

    /// Resolve into a low-level [`WebSocketTarget`] at `addr`.
    pub fn resolve(&self, addr: SocketAddr) -> WebSocketTarget {
        match &self.security {
            WsSecurity::Ws => WebSocketTarget::ws(addr, self.host.clone(), self.path.clone()),
            WsSecurity::Wss {
                server_name,
                trust_roots,
            } => WebSocketTarget::wss(
                addr,
                self.host.clone(),
                self.path.clone(),
                server_name.clone(),
                trust_roots.clone(),
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn addr() -> SocketAddr {
        "127.0.0.1:443".parse().unwrap()
    }

    #[test]
    fn generation_advances_and_reads_back() {
        let g = EndpointGeneration::first();
        assert_eq!(g.value(), 1);
        assert_eq!(g.next().value(), 2);
        assert!(g.next() > g);
    }

    #[test]
    fn http_plain_resolves_to_http_target_with_host() {
        let ep = HttpEndpoint::http("api.local", 80);
        assert_eq!(ep.authority(), "api.local");
        assert!(!ep.connect_security().is_tls());
        match ep.resolve("127.0.0.1:80".parse().unwrap()) {
            HttpTarget::Http { host, .. } => assert_eq!(host.as_deref(), Some("api.local")),
            other => panic!("expected Http, got {other:?}"),
        }
    }

    #[test]
    fn https_preserves_sni_trust_roots_and_host_override() {
        let roots = TlsTrustRoots::from_der(vec![vec![1, 2, 3]]);
        let ep = HttpEndpoint::https("api.local", 443, roots.clone())
            .server_name("sni.local")
            .host_header("host.local");
        match ep.connect_security() {
            ConnectSecurity::Tls {
                server_name,
                trust_roots,
                alpn,
            } => {
                assert_eq!(server_name, "sni.local");
                assert_eq!(trust_roots.root_certificates_der, roots.root_certificates_der);
                assert!(!alpn.is_h2());
            }
            other => panic!("expected Tls, got {other:?}"),
        }
        assert_eq!(ep.authority(), "host.local");
        match ep.resolve(addr()) {
            HttpTarget::Https {
                server_name, host, ..
            } => {
                assert_eq!(server_name, "sni.local");
                assert!(matches!(host, HttpHostPolicy::Explicit(h) if h == "host.local"));
            }
            other => panic!("expected Https, got {other:?}"),
        }
    }

    #[test]
    fn http2_tls_offers_h2_alpn_and_preserves_authority() {
        let ep = Http2Endpoint::tls("api.local", "api.local", 443, "sni.local", vec![vec![9]]);
        assert_eq!(ep.authority(), "api.local");
        assert!(matches!(
            ep.connect_security(),
            ConnectSecurity::Tls { alpn, .. } if alpn.is_h2()
        ));
        match ep.resolve(addr()) {
            Http2Target::Tls {
                authority,
                server_name,
                alpn,
                ..
            } => {
                assert_eq!(authority, "api.local");
                assert_eq!(server_name, "sni.local");
                assert!(alpn.is_h2());
            }
            other => panic!("expected Tls, got {other:?}"),
        }
    }

    #[test]
    fn grpc_endpoint_resolves_to_grpc_target() {
        let ep = GrpcEndpoint::h2c("svc.local", "svc.local", 50051);
        assert_eq!(ep.host(), "svc.local");
        let target = ep.resolve("127.0.0.1:50051".parse().unwrap());
        assert_eq!(target.http2.authority(), "svc.local");
    }

    #[test]
    fn wss_preserves_path_sni_and_trust_roots() {
        let roots = TlsTrustRoots::from_der(vec![vec![7]]);
        let ep = WebSocketEndpoint::wss("rt.local", 443, "/ws", "sni.local", roots.clone());
        assert_eq!(ep.path(), "/ws");
        match ep.resolve(addr()) {
            WebSocketTarget::Wss {
                host,
                path,
                server_name,
                trust_roots,
                ..
            } => {
                assert_eq!(host, "rt.local");
                assert_eq!(path, "/ws");
                assert_eq!(server_name, "sni.local");
                assert_eq!(trust_roots.root_certificates_der, roots.root_certificates_der);
            }
            other => panic!("expected Wss, got {other:?}"),
        }
    }
}
