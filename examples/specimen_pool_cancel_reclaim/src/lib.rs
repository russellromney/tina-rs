//! Cancel a wave of in-flight pool acquires and prove the pool admits
//! new acquires immediately afterward.
//!
//! Capacity 1, max_waiters 4. The driver:
//!
//! 1. Acquires once (gets the resource).
//! 2. Fires `WAITERS` more acquires with `call_with_handle` — they
//!    park.
//! 3. Sends `CancelAll` — fires `cancel_call(handle)` for every
//!    parked waiter.
//! 4. Sends `RetryAll` — fires `WAITERS` more acquires.
//! 5. Releases the held lease so they can drain.
//!
//! Contract: every cancelled wait is reclaimed (`cancel_count >=
//! WAITERS`); none of the retried acquires hits `Full`; one of them
//! receives the resource on release; the rest are still parked when
//! the report is read.

pub mod tina_impl;
pub mod tokio_impl;

pub const WAITERS: usize = 4;
pub const RETRY_BUDGET_MS: u64 = 250;

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Report {
    /// Cancelled waits, observed by counter on the pool side
    /// (tina) or by the cancel/abort signal count (tokio).
    pub cancelled: usize,
    /// Retried acquires that were admitted (parked or got the
    /// resource).
    pub retried_admitted: usize,
    /// Retried acquires that hit Full (must be 0).
    pub retried_full: usize,
    /// One retry should win the resource after the original lease
    /// releases.
    pub retried_resourced: usize,
    pub exit_clean: bool,
}

pub fn assert_report_invariants(side: &str, r: &Report) {
    assert!(
        r.cancelled >= WAITERS,
        "{side}: pool must reclaim every cancelled waiter; {r:?}"
    );
    assert_eq!(
        r.retried_full, 0,
        "{side}: retries after cancel must not see Full; {r:?}"
    );
    assert!(
        r.retried_admitted >= WAITERS,
        "{side}: retries should all be admitted; {r:?}"
    );
    assert!(
        r.retried_resourced >= 1,
        "{side}: at least one retry should win the resource on release; {r:?}"
    );
    assert!(r.exit_clean, "{side}: {r:?}");
}
