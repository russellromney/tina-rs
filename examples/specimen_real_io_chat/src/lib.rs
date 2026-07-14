//! Tokio-vs-Tina: slow-consumer chat fanout over real loopback TCP.
//!
//! Same workload, two implementations. Read [`tokio_impl`] and
//! [`tina_impl`] top-to-bottom; the README compares feel.

pub mod tina_impl;
pub mod tokio_impl;

pub const MAX_BURST: usize = 1_000_000;
pub const MAX_BROADCAST_TARGETS: usize = 65_536;
pub const MAX_SLOW_CONSUMER_CAPACITY: usize = 65_536;

/// Workload knobs.
///
/// The client asks for `burst` deliveries. The Tina server only turns
/// `max_broadcast_targets` of them into runtime effects; the rest are counted
/// as visible `Full` pressure before they can become hidden work.
#[derive(Debug, Clone, Copy)]
pub struct RunConfig {
    pub burst: usize,
    pub max_broadcast_targets: usize,
    pub slow_consumer_capacity: usize,
}

impl Default for RunConfig {
    fn default() -> Self {
        Self {
            burst: 64,
            max_broadcast_targets: 64,
            slow_consumer_capacity: 1,
        }
    }
}

impl RunConfig {
    pub fn validate(self) -> anyhow::Result<Self> {
        anyhow::ensure!(self.burst > 0, "burst must be greater than zero");
        anyhow::ensure!(self.burst <= MAX_BURST, "burst exceeds MAX_BURST");
        anyhow::ensure!(
            self.max_broadcast_targets > 0,
            "max_broadcast_targets must be greater than zero"
        );
        anyhow::ensure!(
            self.max_broadcast_targets <= MAX_BROADCAST_TARGETS,
            "max_broadcast_targets exceeds MAX_BROADCAST_TARGETS"
        );
        anyhow::ensure!(
            self.slow_consumer_capacity > 0,
            "slow_consumer_capacity must be greater than zero"
        );
        anyhow::ensure!(
            self.slow_consumer_capacity <= MAX_SLOW_CONSUMER_CAPACITY,
            "slow_consumer_capacity exceeds MAX_SLOW_CONSUMER_CAPACITY"
        );
        self.max_broadcast_targets
            .checked_add(16)
            .ok_or_else(|| anyhow::anyhow!("connection mailbox capacity overflow"))?;
        Ok(self)
    }
}

/// What each side observed. `accepted + full + closed == burst`
/// always (every fanout attempt is accounted for).
///
/// `delivered` and `buffered` are the Tokio-vs-Tina contrast:
/// Tokio's unbounded queue accepts everything but only `delivered`
/// reaches the consumer (`buffered` sits in memory). Tina's bounded
/// admission means `delivered == accepted` and `buffered == 0`.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Report {
    pub accepted: usize,
    pub full: usize,
    pub closed: usize,
    pub delivered: usize,
    pub buffered: usize,
}

impl Report {
    pub fn total(&self) -> usize {
        self.accepted + self.full + self.closed
    }
}
