//! Shared scaffolding for AWS bridge closers and drain accounting.
//!
//! Every AWS service in this crate uses the same admission close flag,
//! the same metrics-driven in-flight counter, and the same drain loop.
//! Lifting that loop here keeps each service's `close_and_drain`
//! readable and means a fix to the polling cadence or the drain rules
//! lands in one place.
//!
//! The per-service `XxxCloser` types still own their own typed
//! `XxxDrainReport` — service-specific operation kinds stay typed.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

/// Outcome of a drain wait.
pub(crate) struct DrainResult {
    /// True iff in-flight count reached zero before the deadline.
    pub drained: bool,
    /// In-flight count at the moment the report was taken.
    pub in_flight_remaining: u64,
    /// Per-operation kinds in flight when the deadline fired.
    /// Empty when `drained == true`.
    pub in_flight_kinds: Vec<(&'static str, u64)>,
}

/// Wait up to `timeout` for the in-flight count to reach zero. Polls
/// every millisecond. When the deadline fires while there is still
/// in-flight work, the caller-supplied `in_flight_kinds` thunk is
/// invoked exactly once to capture a per-kind snapshot for the drain
/// report.
pub(crate) fn await_drain<K>(
    in_flight: &AtomicU64,
    in_flight_kinds: K,
    timeout: Duration,
) -> DrainResult
where
    K: FnOnce() -> Vec<(&'static str, u64)>,
{
    let deadline = Instant::now() + timeout;
    loop {
        let remaining = in_flight.load(Ordering::Relaxed);
        if remaining == 0 {
            return DrainResult {
                drained: true,
                in_flight_remaining: 0,
                in_flight_kinds: Vec::new(),
            };
        }
        if Instant::now() >= deadline {
            return DrainResult {
                drained: false,
                in_flight_remaining: remaining,
                in_flight_kinds: in_flight_kinds(),
            };
        }
        std::thread::sleep(Duration::from_millis(1));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn await_drain_returns_drained_when_in_flight_is_zero() {
        let in_flight = AtomicU64::new(0);
        let result = await_drain(&in_flight, Vec::new, Duration::from_millis(10));
        assert!(result.drained);
        assert_eq!(result.in_flight_remaining, 0);
        assert!(result.in_flight_kinds.is_empty());
    }

    #[test]
    fn await_drain_times_out_with_remaining_and_kinds() {
        let in_flight = AtomicU64::new(3);
        let result = await_drain(
            &in_flight,
            || vec![("put_object", 2), ("head_object", 1)],
            Duration::from_millis(5),
        );
        assert!(!result.drained);
        assert_eq!(result.in_flight_remaining, 3);
        assert_eq!(result.in_flight_kinds.len(), 2);
    }
}
