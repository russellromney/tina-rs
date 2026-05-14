//! Cancel a wave of in-flight pool acquires and prove the pool admits
//! new acquires immediately afterward.
//!
//! Capacity 1, max_waiters 4. The driver:
//!
//! 1. Acquires once (gets the resource).
//! 2. Fires `WAITERS` more acquires with `call_cancelable` — they
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

#[derive(Debug, Default, Clone, PartialEq, Eq)]
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
    /// Highest live waiter count since pool construction. Tina
    /// only — tokio's `Semaphore` does not expose this.
    pub waiters_high_water: usize,
    /// Configured waiter cap. Pair with `waiters_high_water` for
    /// `unknown -> measured -> fixed`.
    pub waiters_max: usize,
    /// One discovery line for `pool.demo.waiters`. Tina only.
    /// Empty on the tokio side.
    pub discovery_line: String,
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

/// Tina-only capacity invariants. The park wave drives high water
/// to the configured cap; the discovery line carries cap, high
/// water, and a next-action hint.
pub fn assert_tina_capacity_invariants(r: &Report) {
    assert_eq!(
        r.waiters_max, WAITERS,
        "tina: report should reflect configured cap; {r:?}"
    );
    assert!(
        r.waiters_high_water >= WAITERS,
        "tina: park wave should drive high water to WAITERS={WAITERS}; {r:?}"
    );
    assert!(
        !r.discovery_line.is_empty(),
        "tina: discovery line should be set; {r:?}"
    );
    assert!(
        r.discovery_line.contains("surface=pool.demo.waiters"),
        "tina: discovery line should name the surface; got {:?}",
        r.discovery_line
    );
    assert!(
        r.discovery_line.contains(&format!("max={WAITERS}")),
        "tina: discovery line should carry the cap; got {:?}",
        r.discovery_line
    );
    assert!(
        r.discovery_line
            .contains(&format!("high={}", r.waiters_high_water)),
        "tina: discovery line should carry observed high water; got {:?}",
        r.discovery_line
    );
}
