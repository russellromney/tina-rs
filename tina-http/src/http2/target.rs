//! Target shapes for the native HTTP/2 client.
//!
//! Targets are typed up-front so that authority, scheme, and TLS truth
//! cannot be lost in a `String`. An `Http2Target::H2c` cannot carry a
//! `server_name` or trust roots; an `Http2Target::Tls` must carry both.
//! `AlpnProtocols` is named, not a raw byte bag — `AlpnProtocols::h2()`
//! advertises a single `h2` protocol; `AlpnProtocols::none()` says the
//! caller wants a non-ALPN TLS connection. The h2c target never touches
//! TLS rails.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::net::SocketAddr;

/// ALPN protocol list offered by a TLS-shaped Tina connect/accept.
///
/// Named, not a `Vec<Vec<u8>>` — this prevents a caller from accidentally
/// shipping non-ASCII bytes through the rustls config and forces the
/// runtime/sim to keep a stable trace tag per known protocol. Today only
/// `h2` is named; `none()` says the caller explicitly does not want ALPN.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AlpnProtocols(AlpnProtocolsRepr);

#[derive(Debug, Clone, PartialEq, Eq)]
enum AlpnProtocolsRepr {
    /// `None` ALPN: TLS handshake proceeds without offering an ALPN list.
    None,
    /// Single-protocol `h2` ALPN.
    H2,
}

impl AlpnProtocols {
    /// Offer no ALPN. The TLS handshake does not include an ALPN extension
    /// and the selected protocol output is `SelectedAlpn::None`.
    pub fn none() -> Self {
        Self(AlpnProtocolsRepr::None)
    }

    /// Offer only `h2`. If the server does not negotiate `h2`, the TLS
    /// rail fails the connect with `tina_runtime::CallError::TlsAlpnMismatch`,
    /// which the native HTTP/2 client surfaces as
    /// `Http2ClientOutcome::TlsAlpnMismatch`.
    pub fn h2() -> Self {
        Self(AlpnProtocolsRepr::H2)
    }

    /// Returns true if the ALPN list is `h2`.
    pub fn is_h2(&self) -> bool {
        matches!(self.0, AlpnProtocolsRepr::H2)
    }

    /// Wire-byte ALPN protocol list for the TLS rail (`tls_connect_alpn`
    /// / `tls_bind_alpn`). Empty means "offer no ALPN".
    pub(crate) fn wire(&self) -> Vec<Vec<u8>> {
        match self.0 {
            AlpnProtocolsRepr::None => Vec::new(),
            AlpnProtocolsRepr::H2 => vec![b"h2".to_vec()],
        }
    }
}

/// One HTTP/2 client connection target.
///
/// The two variants are typed shapes, not a string bag — `H2c` cannot
/// carry SNI or trust roots, and `Tls` must carry both. Per-RFC-7540 §3.1
/// the authority used on the wire (`:authority` and routing) is the
/// `authority` field; `addr` is the resolved socket address used for the
/// raw TCP connect.
///
/// The split is enforced by the type system, not by a runtime check. An
/// `H2c` target cannot name a `server_name` or `trust_roots` field:
///
/// ```compile_fail
/// use tina_http::Http2Target;
/// let _ = Http2Target::H2c {
///     authority: "x".into(),
///     addr: "127.0.0.1:80".parse().unwrap(),
///     server_name: "x".into(), // no such field on H2c — does not compile
/// };
/// ```
///
/// A `Tls` target must carry `server_name`, `trust_roots`, and `alpn`:
///
/// ```compile_fail
/// use tina_http::Http2Target;
/// let _ = Http2Target::Tls {
///     authority: "x".into(),
///     addr: "127.0.0.1:443".parse().unwrap(),
///     // missing server_name / trust_roots / alpn — does not compile
/// };
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Http2Target {
    /// Prior-knowledge cleartext h2c. No TLS rail is consulted.
    H2c {
        /// Wire `:authority` and route key used for connection reuse.
        authority: String,
        /// Resolved socket address used for the TCP connect.
        addr: SocketAddr,
    },
    /// HTTPS/2 over TLS with explicit ALPN, SNI, and trust roots.
    Tls {
        /// Wire `:authority` and route key used for connection reuse.
        authority: String,
        /// Resolved socket address used for the TCP connect.
        addr: SocketAddr,
        /// SNI / rustls server name used for certificate validation.
        server_name: String,
        /// Explicit DER trust roots (no ambient platform store).
        trust_roots: Vec<Vec<u8>>,
        /// ALPN list offered to the server. For h2 over TLS this is
        /// always `AlpnProtocols::h2()`.
        alpn: AlpnProtocols,
    },
}

impl Http2Target {
    /// Returns the wire authority used for `:authority` and route keys.
    pub fn authority(&self) -> &str {
        match self {
            Self::H2c { authority, .. } | Self::Tls { authority, .. } => authority,
        }
    }

    /// Returns the resolved socket address.
    pub fn addr(&self) -> SocketAddr {
        match self {
            Self::H2c { addr, .. } | Self::Tls { addr, .. } => *addr,
        }
    }

    /// Returns `true` when this target asks for TLS.
    pub fn is_tls(&self) -> bool {
        matches!(self, Self::Tls { .. })
    }

    /// Returns the route key used for client-connection reuse.
    ///
    /// Reuse is "one connection isolate carries many admitted streams"
    /// (Phase 116 first form). Idle eviction, max lifetime, and health
    /// policy are Phase 119. The route key folds authority and the
    /// TLS/root/ALPN policy so two configurations that differ in trust
    /// roots do not share a connection.
    ///
    /// `trust_roots` is hashed (DefaultHasher is sufficient for sharding;
    /// the key is never used as a security boundary on its own — the TLS
    /// rail still validates against the actual roots — so collision
    /// resistance only needs to keep two distinct sets in distinct
    /// pool slots).
    pub fn route_key(&self) -> String {
        match self {
            Self::H2c { authority, .. } => format!("h2c::{authority}"),
            Self::Tls {
                authority,
                server_name,
                alpn,
                trust_roots,
                ..
            } => {
                let alpn_tag = if alpn.is_h2() { "h2" } else { "none" };
                let mut hasher = DefaultHasher::new();
                for root in trust_roots {
                    root.hash(&mut hasher);
                }
                let roots_hash = hasher.finish();
                format!("tls::{authority}@{server_name}:{alpn_tag}:{roots_hash:016x}")
            }
        }
    }
}
