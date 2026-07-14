//! Cancel a wave of in-flight pool acquires and prove the pool admits
//! new acquires immediately afterward.
//!
//! Capacity 1, max_waiters 4. The driver:
//!
//! 1. Acquires once (gets the resource).
//! 2. Fires `WAITERS` more acquires with `call_cancelable` — they park.
//! 3. Uses an actor-owned timer, then fires `cancel_call(handle)` for
//!    every parked waiter.
//! 4. Waits for every cancel acknowledgement and the pool pressure
//!    snapshot, then fires `WAITERS` more acquires.
//! 5. Uses a second actor-owned timer to release the held lease so the
//!    retries can drain.
//!
//! Contract: every cancelled wait is reclaimed (`cancel_count >=
//! WAITERS`); none of the retried acquires hits `Full`; one of them
//! receives the resource on release; every retry and release settles
//! before the report is produced.

pub mod tina_impl;
pub mod tokio_impl;

pub const WAITERS: usize = 4;
pub const RETRY_BUDGET_MS: u64 = 250;

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Report {
    /// Cancelled waits, observed by counter on the pool side
    /// (tina) or by the cancel/abort signal count (tokio).
    pub cancelled: usize,
    /// Retried acquire calls dispatched after cancellation reclaimed the
    /// original waiter wave.
    pub retried_dispatched: usize,
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
    /// Exact Tina acquire failures, partitioned by wave.
    pub prime_failures: Vec<tina::pool::AcquireFailure>,
    pub park_failures: Vec<tina::pool::AcquireFailure>,
    pub retry_failures: Vec<tina::pool::AcquireFailure>,
    /// One typed acknowledgement for every explicit cancellation.
    pub cancel_outcomes: Vec<tina::CancelOutcome>,
    /// Exact release failures; an empty list proves every admitted lease settled.
    pub release_failures: Vec<tina::pool::ReleaseFailure>,
    /// Non-pressure terminal returned by the pressure snapshot call.
    pub pressure_terminal: Option<PressureTerminal>,
    /// The pressure call returned, whether with a report or an exact terminal.
    pub pressure_settled: bool,
    /// Exact failures from the driver-owned sequencing timers.
    pub control_timer_failures: Vec<tina_runtime::CallError>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PressureTerminal {
    Full,
    Closed,
    Timeout,
    Rejected(tina::CallRejectedReason),
    WrongReply,
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
        r.retried_dispatched == WAITERS,
        "{side}: every bounded retry should be dispatched; {r:?}"
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
    assert_eq!(r.cancelled, WAITERS, "tina: {r:?}");
    assert_eq!(r.retried_resourced, WAITERS, "tina: {r:?}");
    assert_eq!(r.cancel_outcomes.len(), WAITERS, "tina: {r:?}");
    assert!(
        r.cancel_outcomes
            .iter()
            .all(|outcome| matches!(outcome, tina::CancelOutcome::Cancelled)),
        "tina: every parked wait should cancel exactly once; {r:?}"
    );
    assert!(r.release_failures.is_empty(), "tina: {r:?}");
    assert!(r.pressure_terminal.is_none(), "tina: {r:?}");
    assert!(r.pressure_settled, "tina: pressure must settle; {r:?}");
    assert!(r.control_timer_failures.is_empty(), "tina: {r:?}");
    assert!(r.prime_failures.is_empty(), "tina: {r:?}");
    assert!(r.park_failures.is_empty(), "tina: {r:?}");
    assert!(r.retry_failures.is_empty(), "tina: {r:?}");
    assert_eq!(
        r.waiters_max, WAITERS,
        "tina: report should reflect configured cap; {r:?}"
    );
    assert_eq!(
        r.waiters_high_water, WAITERS,
        "tina: park wave should drive high water exactly to its cap; {r:?}"
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
