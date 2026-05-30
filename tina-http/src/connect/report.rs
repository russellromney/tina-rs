//! Typed connect reports.
//!
//! A connect either returns a resolved low-level target or one
//! [`ConnectReport`]. The report keeps every fact the plan requires:
//! endpoint identity, DNS truth, the ordered attempt list with per-attempt
//! family and terminal reason, the winner, and the cancelled-loser /
//! late-completion (tombstone) counts. Nothing is collapsed into a generic
//! "failed".

use std::net::SocketAddr;

use super::endpoint::{EndpointGeneration, EndpointId};

/// IPv4 vs IPv6 family of one attempted address.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AddressFamily {
    /// IPv4 address.
    V4,
    /// IPv6 address.
    V6,
}

impl AddressFamily {
    /// Classify a socket address.
    pub fn of(addr: &SocketAddr) -> Self {
        match addr {
            SocketAddr::V4(_) => Self::V4,
            SocketAddr::V6(_) => Self::V6,
        }
    }
}

/// What happened to the DNS phase.
///
/// `Full`, `Closed`, and `Timeout` are distinct from any TCP/TLS connect
/// failure: a full DNS lane never started a connect at all.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DnsOutcome {
    /// The endpoint carried a resolved address; DNS was not consulted.
    NotAttempted,
    /// DNS returned this many addresses.
    Resolved {
        /// Count of resolved addresses before the family/cap ordering.
        count: usize,
    },
    /// The bounded DNS lane was full.
    Full,
    /// The DNS lane was closed.
    Closed,
    /// DNS exceeded its deadline.
    Timeout,
    /// DNS failed as a normal I/O error (no such host, network error).
    Failed,
}

impl DnsOutcome {
    /// True when DNS produced at least one address.
    pub fn resolved_any(&self) -> bool {
        matches!(self, Self::Resolved { count } if *count > 0)
    }
}

/// Terminal reason for one connect attempt.
///
/// TCP connection-refused is not separable from other I/O at the runtime
/// call boundary (`CallError::Io` covers both), so a refused peer surfaces
/// as [`ConnectAttemptOutcome::ConnectIo`]. TLS failures keep their distinct
/// handshake / certificate / name / ALPN truth.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConnectAttemptOutcome {
    /// The transport connected. This attempt is the winner.
    Connected,
    /// TCP connect failed as I/O (includes connection-refused).
    ConnectIo,
    /// TCP connect exceeded the connect deadline.
    ConnectTimeout,
    /// TLS handshake failed.
    TlsHandshake,
    /// TLS certificate validation failed.
    TlsCertificate,
    /// TLS server-name validation failed.
    TlsName,
    /// TLS ALPN negotiation did not yield the offered protocol.
    TlsAlpnMismatch,
    /// The bounded TLS lane was full.
    TlsFull,
    /// The TLS lane was closed.
    TlsClosed,
    /// This attempt was a loser and its caller-side wait was cancelled.
    Cancelled,
    /// This attempt completed after the race was already won; its result
    /// is tombstoned and counted, never converted into a user success.
    LateCompletion,
}

impl ConnectAttemptOutcome {
    /// True when this attempt connected.
    pub fn is_connected(&self) -> bool {
        matches!(self, Self::Connected)
    }
}

/// One attempted connect, in dispatch order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConnectAttemptReport {
    /// The address this attempt dialed.
    pub addr: SocketAddr,
    /// Address family of `addr`.
    pub family: AddressFamily,
    /// Terminal outcome of this attempt.
    pub outcome: ConnectAttemptOutcome,
}

impl ConnectAttemptReport {
    /// Build a report row for one attempt.
    pub fn new(addr: SocketAddr, outcome: ConnectAttemptOutcome) -> Self {
        Self {
            family: AddressFamily::of(&addr),
            addr,
            outcome,
        }
    }
}

/// The TLS truth a connect preserved, when the endpoint was secure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConnectTlsTruth {
    /// SNI / rustls server name used for certificate validation.
    pub server_name: String,
    /// Whether `h2` ALPN was offered.
    pub alpn_h2: bool,
}

/// One connect outcome: the full ordered truth of a resolve+race.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConnectReport {
    /// Endpoint identity.
    pub endpoint: EndpointId,
    /// Endpoint generation at the time of this connect.
    pub generation: EndpointGeneration,
    /// Host as named by the endpoint (pre-DNS).
    pub host: String,
    /// Port as named by the endpoint.
    pub port: u16,
    /// Wire authority / `Host:` truth carried by the endpoint.
    pub authority: String,
    /// TLS truth, present only for secure endpoints.
    pub tls: Option<ConnectTlsTruth>,
    /// What DNS did.
    pub dns: DnsOutcome,
    /// Addresses considered after family ordering and the resolved cap.
    pub resolved_addresses: Vec<SocketAddr>,
    /// Attempts in dispatch order, with per-attempt family and reason.
    pub attempted: Vec<ConnectAttemptReport>,
    /// Winning address, if any attempt connected.
    pub winner: Option<SocketAddr>,
    /// Count of losers whose caller-side wait was cancelled.
    pub cancelled_losers: usize,
    /// Count of attempts that completed after the race was won.
    pub late_completions: usize,
}

impl ConnectReport {
    /// True when an attempt connected.
    pub fn succeeded(&self) -> bool {
        self.winner.is_some()
    }

    /// Number of attempts dispatched (live or terminal).
    pub fn attempt_count(&self) -> usize {
        self.attempted.len()
    }

    /// One stable discovery line for logs / golden tests.
    pub fn discovery_line(&self) -> String {
        let winner = match self.winner {
            Some(addr) => addr.to_string(),
            None => "none".to_string(),
        };
        format!(
            "connect endpoint={} gen={} host={} port={} authority={} dns={:?} \
             resolved={} attempts={} winner={} cancelled_losers={} late={}",
            self.endpoint.value(),
            self.generation.value(),
            self.host,
            self.port,
            self.authority,
            self.dns,
            self.resolved_addresses.len(),
            self.attempted.len(),
            winner,
            self.cancelled_losers,
            self.late_completions,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v4(port: u16) -> SocketAddr {
        format!("127.0.0.1:{port}").parse().unwrap()
    }

    fn v6(port: u16) -> SocketAddr {
        format!("[::1]:{port}").parse().unwrap()
    }

    #[test]
    fn family_classification_is_by_socket_kind() {
        assert_eq!(AddressFamily::of(&v4(1)), AddressFamily::V4);
        assert_eq!(AddressFamily::of(&v6(1)), AddressFamily::V6);
    }

    #[test]
    fn attempt_report_carries_family_from_address() {
        let r = ConnectAttemptReport::new(v6(80), ConnectAttemptOutcome::Connected);
        assert_eq!(r.family, AddressFamily::V6);
        assert!(r.outcome.is_connected());
    }

    #[test]
    fn dns_full_is_not_resolved() {
        assert!(!DnsOutcome::Full.resolved_any());
        assert!(!DnsOutcome::Resolved { count: 0 }.resolved_any());
        assert!(DnsOutcome::Resolved { count: 2 }.resolved_any());
    }

    #[test]
    fn report_discovery_line_names_winner_and_tombstones() {
        let report = ConnectReport {
            endpoint: EndpointId::new(7),
            generation: EndpointGeneration::new(3),
            host: "api.local".to_string(),
            port: 443,
            authority: "api.local".to_string(),
            tls: Some(ConnectTlsTruth {
                server_name: "api.local".to_string(),
                alpn_h2: true,
            }),
            dns: DnsOutcome::Resolved { count: 2 },
            resolved_addresses: vec![v6(443), v4(443)],
            attempted: vec![
                ConnectAttemptReport::new(v6(443), ConnectAttemptOutcome::Connected),
                ConnectAttemptReport::new(v4(443), ConnectAttemptOutcome::Cancelled),
            ],
            winner: Some(v6(443)),
            cancelled_losers: 1,
            late_completions: 0,
        };
        assert!(report.succeeded());
        assert_eq!(report.attempt_count(), 2);
        let line = report.discovery_line();
        assert!(line.contains("winner=[::1]:443"));
        assert!(line.contains("cancelled_losers=1"));
    }
}
