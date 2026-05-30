//! Connect policy: bounded DNS, bounded connect, family ordering, and
//! Happy Eyeballs caps.
//!
//! Every cap here is finite and validated before first use. Each cap can be
//! named as a [`BudgetSurface`] with a stable name, so a manifest row and a
//! live pressure row describe the same bound. There is no hidden retry and
//! no unbounded attempt storage: the helper admits at most
//! `max_total_attempts` connect slots over the life of one connect.

use std::error::Error;
use std::fmt;
use std::net::SocketAddr;
use std::time::Duration;

use tina_runtime::budget::{BudgetCap, BudgetKind, BudgetSurface, BudgetUnit};

/// Which address family to try first when DNS returns a mix.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AddressFamilyPolicy {
    /// Try IPv6 addresses before IPv4 (RFC 8305 default shape).
    Ipv6First,
    /// Try IPv4 addresses before IPv6.
    Ipv4First,
    /// Keep the resolver's order untouched.
    PreserveOrder,
}

/// Happy Eyeballs caps: stagger delay and concurrency.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HappyEyeballsPolicy {
    /// Delay before starting the next staggered attempt. Explicit even
    /// when zero — zero means "start every admitted attempt at once".
    pub delay: Duration,
    /// Maximum concurrent in-flight connect attempts.
    pub max_concurrent_attempts: usize,
}

impl HappyEyeballsPolicy {
    /// A sequential policy: one attempt at a time, `delay` between them.
    pub fn sequential(delay: Duration) -> Self {
        Self {
            delay,
            max_concurrent_attempts: 1,
        }
    }
}

/// Bounded policy over runtime DNS + TCP/TLS connect.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ConnectPolicy {
    /// DNS lookup deadline.
    pub dns_timeout: Duration,
    /// Per-attempt connect deadline.
    pub connect_timeout: Duration,
    /// Maximum resolved addresses to consider after ordering.
    pub max_resolved_addresses: usize,
    /// Order to try address families.
    pub address_family: AddressFamilyPolicy,
    /// Happy Eyeballs caps.
    pub happy_eyeballs: HappyEyeballsPolicy,
    /// Maximum connect attempts admitted over the whole connect.
    pub max_total_attempts: usize,
}

impl ConnectPolicy {
    /// A conservative default: IPv6-first, 3 addresses, 2 concurrent, a
    /// 250ms stagger, 6 total attempts, 5s DNS / 10s connect deadlines.
    pub fn balanced() -> Self {
        Self {
            dns_timeout: Duration::from_secs(5),
            connect_timeout: Duration::from_secs(10),
            max_resolved_addresses: 3,
            address_family: AddressFamilyPolicy::Ipv6First,
            happy_eyeballs: HappyEyeballsPolicy {
                delay: Duration::from_millis(250),
                max_concurrent_attempts: 2,
            },
            max_total_attempts: 6,
        }
    }

    /// Validate every cap before first use.
    ///
    /// Rejects zero attempt caps, zero deadlines, a concurrency cap above
    /// the total cap, and a resolved-address cap above the total attempt
    /// cap (which could never all be tried).
    pub fn validate(&self) -> Result<(), ConnectPolicyError> {
        if self.max_total_attempts == 0 {
            return Err(ConnectPolicyError::ZeroTotalAttempts);
        }
        if self.max_resolved_addresses == 0 {
            return Err(ConnectPolicyError::ZeroResolvedAddresses);
        }
        if self.happy_eyeballs.max_concurrent_attempts == 0 {
            return Err(ConnectPolicyError::ZeroConcurrentAttempts);
        }
        if self.happy_eyeballs.max_concurrent_attempts > self.max_total_attempts {
            return Err(ConnectPolicyError::ConcurrencyAboveTotal {
                concurrent: self.happy_eyeballs.max_concurrent_attempts,
                total: self.max_total_attempts,
            });
        }
        if self.dns_timeout.is_zero() {
            return Err(ConnectPolicyError::ZeroDnsTimeout);
        }
        if self.connect_timeout.is_zero() {
            return Err(ConnectPolicyError::ZeroConnectTimeout);
        }
        Ok(())
    }

    /// Effective number of attempts this policy can run: the smaller of the
    /// resolved-address cap and the total attempt cap.
    pub fn effective_attempt_cap(&self) -> usize {
        self.max_resolved_addresses.min(self.max_total_attempts)
    }

    /// Order resolved addresses by the family policy, then truncate to the
    /// resolved-address cap. Stable within a family (keeps resolver order).
    pub fn order_addresses(&self, addrs: &[SocketAddr]) -> Vec<SocketAddr> {
        let mut ordered: Vec<SocketAddr> = match self.address_family {
            AddressFamilyPolicy::PreserveOrder => addrs.to_vec(),
            AddressFamilyPolicy::Ipv6First => {
                let mut v: Vec<SocketAddr> =
                    addrs.iter().filter(|a| a.is_ipv6()).copied().collect();
                v.extend(addrs.iter().filter(|a| a.is_ipv4()).copied());
                v
            }
            AddressFamilyPolicy::Ipv4First => {
                let mut v: Vec<SocketAddr> =
                    addrs.iter().filter(|a| a.is_ipv4()).copied().collect();
                v.extend(addrs.iter().filter(|a| a.is_ipv6()).copied());
                v
            }
        };
        ordered.truncate(self.max_resolved_addresses);
        ordered
    }

    /// Manifest rows for the caps this policy names.
    ///
    /// Names are stable and match the live pressure rows the helper emits:
    /// `{prefix}.connect.attempts`, `{prefix}.connect.concurrent`, and
    /// `{prefix}.connect.resolved_addresses`.
    pub fn budget_surfaces(&self, prefix: &str) -> Vec<BudgetSurface> {
        vec![
            BudgetSurface::new(
                format!("{prefix}.connect.attempts"),
                BudgetKind::ConnectAttempt,
                BudgetUnit::Attempts,
                BudgetCap::fixed(self.max_total_attempts),
            )
            .owned_by("connect"),
            BudgetSurface::new(
                format!("{prefix}.connect.concurrent"),
                BudgetKind::ConnectAttempt,
                BudgetUnit::Attempts,
                BudgetCap::fixed(self.happy_eyeballs.max_concurrent_attempts),
            )
            .owned_by("connect"),
            BudgetSurface::new(
                format!("{prefix}.connect.resolved_addresses"),
                BudgetKind::ConnectAttempt,
                BudgetUnit::Attempts,
                BudgetCap::fixed(self.max_resolved_addresses),
            )
            .owned_by("connect"),
        ]
    }
}

impl Default for ConnectPolicy {
    fn default() -> Self {
        Self::balanced()
    }
}

/// Why a [`ConnectPolicy`] failed validation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnectPolicyError {
    /// `max_total_attempts` was zero — the helper could never connect.
    ZeroTotalAttempts,
    /// `max_resolved_addresses` was zero.
    ZeroResolvedAddresses,
    /// `max_concurrent_attempts` was zero.
    ZeroConcurrentAttempts,
    /// Concurrency cap exceeds the total attempt cap.
    ConcurrencyAboveTotal {
        /// Configured concurrency cap.
        concurrent: usize,
        /// Configured total attempt cap.
        total: usize,
    },
    /// DNS timeout was zero — every lookup would fail immediately.
    ZeroDnsTimeout,
    /// Connect timeout was zero — every attempt would fail immediately.
    ZeroConnectTimeout,
}

impl fmt::Display for ConnectPolicyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ZeroTotalAttempts => f.write_str("max_total_attempts must be positive"),
            Self::ZeroResolvedAddresses => f.write_str("max_resolved_addresses must be positive"),
            Self::ZeroConcurrentAttempts => f.write_str("max_concurrent_attempts must be positive"),
            Self::ConcurrencyAboveTotal { concurrent, total } => write!(
                f,
                "max_concurrent_attempts {concurrent} exceeds max_total_attempts {total}"
            ),
            Self::ZeroDnsTimeout => f.write_str("dns_timeout must be positive"),
            Self::ZeroConnectTimeout => f.write_str("connect_timeout must be positive"),
        }
    }
}

impl Error for ConnectPolicyError {}

#[cfg(test)]
mod tests {
    use super::*;
    use tina::capacity::CapacityPolicy;
    use tina_runtime::budget::ServiceBudgetManifest;

    fn v4(s: &str) -> SocketAddr {
        format!("{s}:80").parse().unwrap()
    }
    fn v6(s: &str) -> SocketAddr {
        format!("[{s}]:80").parse().unwrap()
    }

    #[test]
    fn balanced_policy_validates() {
        ConnectPolicy::balanced().validate().unwrap();
    }

    #[test]
    fn zero_total_attempts_is_rejected() {
        let mut p = ConnectPolicy::balanced();
        p.max_total_attempts = 0;
        assert_eq!(p.validate(), Err(ConnectPolicyError::ZeroTotalAttempts));
    }

    #[test]
    fn concurrency_above_total_is_rejected() {
        let mut p = ConnectPolicy::balanced();
        p.max_total_attempts = 2;
        p.happy_eyeballs.max_concurrent_attempts = 3;
        assert_eq!(
            p.validate(),
            Err(ConnectPolicyError::ConcurrencyAboveTotal {
                concurrent: 3,
                total: 2
            })
        );
    }

    #[test]
    fn zero_deadlines_are_rejected() {
        let mut p = ConnectPolicy::balanced();
        p.dns_timeout = Duration::ZERO;
        assert_eq!(p.validate(), Err(ConnectPolicyError::ZeroDnsTimeout));
        let mut p = ConnectPolicy::balanced();
        p.connect_timeout = Duration::ZERO;
        assert_eq!(p.validate(), Err(ConnectPolicyError::ZeroConnectTimeout));
    }

    #[test]
    fn ipv6_first_orders_v6_then_v4_and_truncates() {
        let mut p = ConnectPolicy::balanced();
        p.max_resolved_addresses = 3;
        p.address_family = AddressFamilyPolicy::Ipv6First;
        let addrs = vec![v4("127.0.0.1"), v6("::1"), v4("127.0.0.2"), v6("::2")];
        let ordered = p.order_addresses(&addrs);
        assert_eq!(ordered.len(), 3);
        assert!(ordered[0].is_ipv6());
        assert!(ordered[1].is_ipv6());
        assert!(ordered[2].is_ipv4());
    }

    #[test]
    fn ipv4_first_orders_v4_then_v6() {
        let mut p = ConnectPolicy::balanced();
        p.max_resolved_addresses = 4;
        p.address_family = AddressFamilyPolicy::Ipv4First;
        let addrs = vec![v6("::1"), v4("127.0.0.1")];
        let ordered = p.order_addresses(&addrs);
        assert!(ordered[0].is_ipv4());
        assert!(ordered[1].is_ipv6());
    }

    #[test]
    fn preserve_order_keeps_resolver_order() {
        let mut p = ConnectPolicy::balanced();
        p.address_family = AddressFamilyPolicy::PreserveOrder;
        let addrs = vec![v4("127.0.0.1"), v6("::1")];
        assert_eq!(p.order_addresses(&addrs), addrs);
    }

    #[test]
    fn budget_surfaces_validate_and_use_stable_names() {
        let p = ConnectPolicy::balanced();
        let surfaces = p.budget_surfaces("ws");
        let names: Vec<&str> = surfaces.iter().map(|s| s.name.as_str()).collect();
        assert!(names.contains(&"ws.connect.attempts"));
        assert!(names.contains(&"ws.connect.concurrent"));
        assert!(names.contains(&"ws.connect.resolved_addresses"));
        let mut m = ServiceBudgetManifest::new("ws", CapacityPolicy::Production);
        m.extend(surfaces);
        m.validate().unwrap();
    }
}
