//! Tokio-vs-Tina mid-flight cancellation.
//!
//! A driver fans out [`FANOUT`] slow worker calls (each takes
//! [`WORK_MS`] ms), then asks for cancellation [`CANCEL_AFTER_MS`] ms
//! later — well before the workers finish. We want to see how each
//! runtime surfaces "the requester gave up" to the workers and to
//! whatever was holding caller state.
//!
//! The point is to expose the gap: Tina has no public *external*
//! cancellation API today. The closest thing is to send a `Stop`
//! message to the requester isolate, which closes its pending
//! IsolateCalls and forces the runtime to mark every worker reply
//! that arrives later as `CallReplyRejected { RequesterClosed }`.
//! The Tokio side uses `JoinSet::abort_all`, which preempts at the
//! await boundary.

pub mod tina_impl;
pub mod tokio_impl;

/// Workers the driver fans out to.
pub const FANOUT: u32 = 6;

/// Per-worker work duration. Long enough that none have replied by
/// the time the cancellation fires.
pub const WORK_MS: u64 = 100;

/// Time after dispatch when the host asks for cancellation.
pub const CANCEL_AFTER_MS: u64 = 30;

/// What each side observed end-to-end.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Report {
    /// Replies the driver had absorbed before cancellation.
    pub replies_before_cancel: u32,
    /// Replies that arrived *after* cancellation. In Tina these surface
    /// as `CallReplyRejected` trace events; the driver no longer
    /// counts them. The Tokio side's aborted tasks never deliver, so
    /// this is also 0 there.
    pub replies_after_cancel: u32,
    /// True when the host actually delivered the cancel signal.
    pub cancel_observed: bool,
    /// True when the side reached the end of `run` cleanly.
    pub exit_clean: bool,
}

/// Asserts the structural invariants both sides should satisfy.
/// Exact `replies_before_cancel` is timing-sensitive (depends on
/// scheduler), so the smoke test only checks shape.
pub fn assert_report_invariants(side: &str, report: &Report) {
    assert!(
        report.replies_before_cancel < FANOUT,
        "{side}: cancel must fire before all workers finish, got {report:?}",
    );
    assert!(
        report.cancel_observed,
        "{side}: cancel signal should reach the requester, got {report:?}",
    );
    assert_eq!(
        report.replies_after_cancel, 0,
        "{side}: cancelled requester should not absorb later replies, got {report:?}",
    );
    assert!(
        report.exit_clean,
        "{side}: expected exit_clean, got {report:?}"
    );
}
