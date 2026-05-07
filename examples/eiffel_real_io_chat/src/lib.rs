//! Tokio-vs-Tina: slow-consumer chat fanout over real loopback TCP.
//!
//! Same workload, two implementations. Read [`tokio_impl`] and
//! [`tina_impl`] top-to-bottom; the README compares feel.

pub mod tina_impl;
pub mod tokio_impl;

/// Workload knobs. The server attempts `burst` deliveries into a
/// slow-consumer queue with `slow_consumer_capacity`.
#[derive(Debug, Clone, Copy)]
pub struct RunConfig {
    pub burst: usize,
    pub slow_consumer_capacity: usize,
}

impl Default for RunConfig {
    fn default() -> Self {
        Self {
            burst: 64,
            slow_consumer_capacity: 1,
        }
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
