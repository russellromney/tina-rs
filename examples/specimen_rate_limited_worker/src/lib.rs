//! Tokio-vs-Tina: a single-consumer rate-limited worker fed by a
//! burst of jobs.
//!
//! Both sides:
//!
//! - bound the worker's queue at [`QUEUE_CAPACITY`];
//! - submit [`BURST_JOBS`] jobs with non-blocking observed sends;
//! - process one job per [`RATE_WINDOW_MS`].
//!
//! What we are looking at: how does overload show up at the
//! producer? In Tokio, a bounded `mpsc` returns
//! `TrySendError::Full`. In Tina, `LocalSystem::try_send_outcome` records
//! `MailboxFull` or `IngressFull` without collapsing terminal outcomes. Both sides
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
//! `tina_runtime::RateLimit<()>` token bucket — `try_admit_at(&(), ctx.now())`
//! returns `Admitted` (process one) or `RateLimited { retry_after }` (sleep
//! that long, then ask again) on the normal path. Its exhaustive decision also
//! preserves `KeyCapacityFull` and `Closed` if the owner configuration changes,
//! so the rate window falls out of the limiter's deterministic `retry_after`
//! instead of a hand-rolled sleep without hiding terminal truth.
//! The overload signal is still the bounded mailbox, mirrored on the Tokio
//! side so the two implementations report structurally identical numbers.
//! For the *reject-at-the-door* use of the same primitive — admit or refuse
//! per tenant — see `examples/systems/system_tenant_rate_limiter`.

use tina_runtime::{CallError, HostBurstSnapshot};

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

const RATE_PER_SEC: u64 = {
    assert!(RATE_WINDOW_MS > 0, "RATE_WINDOW_MS must be non-zero");
    assert!(
        RATE_WINDOW_MS <= 1_000 && 1_000 % RATE_WINDOW_MS == 0,
        "RATE_WINDOW_MS must divide one second exactly",
    );
    1_000 / RATE_WINDOW_MS
};

/// Exact reason the worker stopped before completing the healthy run.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum WorkerTerminal {
    /// The worker completed without a terminal failure.
    #[default]
    None,
    /// The policy had no capacity to track another distinct key.
    RateKeyCapacityFull,
    /// The policy had been explicitly closed.
    RateClosed,
    /// The runtime-owned sleep failed before the worker could resume.
    PacingCallFailed(CallError),
}

/// Outcome of the Tina host's observed end-of-burst control send.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum BurstCloseSettlement {
    /// The implementation does not use a Tina control message.
    #[default]
    NotApplicable,
    /// The worker mailbox accepted `BurstClosed`.
    Delivered,
    /// The worker mailbox was already closed or stale.
    Closed,
    /// The worker thread stopped before the send could be observed.
    WorkerStopped,
}

/// What each side observed end-to-end.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Report {
    /// Submissions the worker accepted (queued).
    pub jobs_admitted: u32,
    /// Submissions rejected at the producer because the worker queue
    /// was full.
    pub jobs_full: u32,
    /// Submissions rejected because the mailbox closed or worker stopped.
    pub jobs_terminal: u32,
    /// Jobs the worker actually processed.
    pub jobs_processed: u32,
    /// Exact Tina host-burst outcomes before the domain-level projection.
    pub tina_burst: Option<HostBurstSnapshot>,
    /// Exact reason the worker stopped early, if any.
    pub worker_terminal: WorkerTerminal,
    /// Exact outcome of the Tina end-of-burst control send.
    pub burst_close_settlement: BurstCloseSettlement,
    /// Whether each side reached the end of `run` cleanly.
    pub exit_clean: bool,
}

/// Asserts the structural invariants both sides should produce. Use
/// from smoke tests so the assertion lives in one place.
pub fn assert_report_invariants(side: &str, report: &Report) {
    assert_eq!(
        report.jobs_admitted + report.jobs_full + report.jobs_terminal,
        BURST_JOBS,
        "{side}: admitted + full + terminal should account for every burst job, got {report:?}",
    );
    assert_eq!(
        report.jobs_terminal, 0,
        "{side}: worker must remain live for the whole burst, got {report:?}",
    );
    assert!(
        report.jobs_full > 0,
        "{side}: expected overload to be visible (jobs_full > 0), got {report:?}",
    );
    assert_eq!(
        report.jobs_processed, report.jobs_admitted,
        "{side}: every admitted job should have been processed, got {report:?}",
    );
    assert_eq!(
        report.worker_terminal,
        WorkerTerminal::None,
        "{side}: no worker terminal expected in the healthy run, got {report:?}",
    );
    if let Some(burst) = report.tina_burst {
        assert_eq!(
            report.burst_close_settlement,
            BurstCloseSettlement::Delivered,
            "{side}: Tina end-of-burst control did not settle, got {report:?}",
        );
        assert_eq!(
            burst.submitted, BURST_JOBS,
            "{side}: Tina burst submission count diverged, got {report:?}",
        );
        assert_eq!(
            burst.observed, burst.submitted,
            "{side}: Tina burst observers did not settle, got {report:?}",
        );
        assert_eq!(burst.admitted, report.jobs_admitted);
        assert_eq!(burst.mailbox_full + burst.ingress_full, report.jobs_full);
        assert_eq!(
            burst.mailbox_closed + burst.worker_stopped,
            report.jobs_terminal,
        );
    } else {
        assert_eq!(
            report.burst_close_settlement,
            BurstCloseSettlement::NotApplicable,
            "{side}: non-Tina report carried Tina control settlement, got {report:?}",
        );
    }
    assert!(
        report.exit_clean,
        "{side}: expected exit_clean, got {report:?}"
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[should_panic(expected = "worker must remain live for the whole burst")]
    fn invariants_reject_terminal_submission() {
        assert_report_invariants(
            "tina",
            &Report {
                jobs_admitted: QUEUE_CAPACITY as u32,
                jobs_full: BURST_JOBS - QUEUE_CAPACITY as u32 - 1,
                jobs_terminal: 1,
                jobs_processed: QUEUE_CAPACITY as u32,
                tina_burst: None,
                worker_terminal: WorkerTerminal::None,
                burst_close_settlement: BurstCloseSettlement::NotApplicable,
                exit_clean: true,
            },
        );
    }

    fn report_with_worker_terminal(worker_terminal: WorkerTerminal) -> Report {
        Report {
            jobs_admitted: QUEUE_CAPACITY as u32,
            jobs_full: BURST_JOBS - QUEUE_CAPACITY as u32,
            jobs_terminal: 0,
            jobs_processed: QUEUE_CAPACITY as u32,
            tina_burst: None,
            worker_terminal,
            burst_close_settlement: BurstCloseSettlement::NotApplicable,
            exit_clean: false,
        }
    }

    #[test]
    #[should_panic(expected = "no worker terminal expected")]
    fn invariants_reject_key_capacity_policy_terminal() {
        assert_report_invariants(
            "tina",
            &report_with_worker_terminal(WorkerTerminal::RateKeyCapacityFull),
        );
    }

    #[test]
    #[should_panic(expected = "no worker terminal expected")]
    fn invariants_reject_closed_policy_terminal() {
        assert_report_invariants(
            "tina",
            &report_with_worker_terminal(WorkerTerminal::RateClosed),
        );
    }
}
