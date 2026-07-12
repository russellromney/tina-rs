//! Tokio-vs-Tina: a single-consumer rate-limited worker fed by a
//! burst of jobs.
//!
//! Both sides:
//!
//! - bound the worker's queue at [`QUEUE_CAPACITY`];
//! - submit [`BURST_JOBS`] jobs as fast as the host can `try_send`;
//! - process one job per [`RATE_WINDOW_MS`].
//!
//! What we are looking at: how does overload show up at the
//! producer? In Tokio, a bounded `mpsc` returns
//! `TrySendError::Full`. In Tina, `LocalSystem::try_send` returns
//! `IngressFull` once the worker mailbox is at capacity. Both sides
//! count admitted vs full and report structurally identical numbers.
//!
//! The exact split between `admitted` and `full` is timing-sensitive
//! (the consumer may have drained one slot by the time the producer
//! pushes the Nth job). The smoke tests assert structural properties
//! rather than exact counts: every burst job is accounted for,
//! overload was visible, and every admitted job was processed.
//!
//! Note on naming: the "rate" here is *throughput pacing*, not *admission
//! rate limiting* (accept/reject at the door). The Tina side paces with a
//! `tina_runtime::RateLimit<()>` token bucket — `try_admit((), ctx.now())`
//! returns `Admitted` (process one) or `RateLimited { retry_after }` (sleep
//! that long, then ask again) — so the rate window falls out of the
//! limiter's deterministic `retry_after` instead of a hand-rolled sleep.
//! The overload signal is still the bounded mailbox, mirrored on the Tokio
//! side so the two implementations report structurally identical numbers.
//! For the *reject-at-the-door* use of the same primitive — admit or refuse
//! per tenant — see `examples/systems/system_tenant_rate_limiter`.

pub mod tina_impl;
pub mod tokio_impl;

/// Total jobs the host tries to push at the worker as fast as it can.
pub const BURST_JOBS: u32 = 32;

/// Worker queue / mailbox capacity. Jobs past this see backpressure
/// (`TrySendError::Full` on Tokio, `IngressFull` on Tina) at the
/// producer.
pub const QUEUE_CAPACITY: usize = 4;

/// Time the worker takes to "process" one job. Drives the rate limit.
pub const RATE_WINDOW_MS: u64 = 5;

/// What each side observed end-to-end.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Report {
    /// Submissions the worker accepted (queued).
    pub jobs_admitted: u32,
    /// Submissions rejected at the producer because the worker queue
    /// was full.
    pub jobs_full: u32,
    /// Jobs the worker actually processed.
    pub jobs_processed: u32,
    /// Whether each side reached the end of `run` cleanly.
    pub exit_clean: bool,
}

/// Asserts the structural invariants both sides should produce. Use
/// from smoke tests so the assertion lives in one place.
pub fn assert_report_invariants(side: &str, report: &Report) {
    assert_eq!(
        report.jobs_admitted + report.jobs_full,
        BURST_JOBS,
        "{side}: admitted + full should account for every burst job, got {report:?}",
    );
    assert!(
        report.jobs_full > 0,
        "{side}: expected overload to be visible (jobs_full > 0), got {report:?}",
    );
    assert_eq!(
        report.jobs_processed, report.jobs_admitted,
        "{side}: every admitted job should have been processed, got {report:?}",
    );
    assert!(
        report.exit_clean,
        "{side}: expected exit_clean, got {report:?}"
    );
}
