//! Tokio-vs-Tina mid-flight cancellation.
//!
//! A driver fans out [`FANOUT`] slow worker calls (each takes
//! [`WORK_MS`] ms), then asks for cancellation [`CANCEL_AFTER_MS`] ms
//! later — well before the workers finish. We want to see how each
//! runtime surfaces "the requester gave up" to the workers and to
//! whatever was holding caller state.
//!
//! Tina ships first-form cancel: `call_with_handle(...).reply(...)`
//! returns a caller-owned `CallHandle`, and `cancel_call(handle)`
//! closes the wait. Workers that already accepted their request still
//! finish; their replies become typed `CallReplyRejected` /
//! `DeferredReplyRejected` trace events.
//!
//! Tokio uses `JoinSet::abort_all`, which preempts at the next await
//! boundary so aborted tasks never deliver.

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
    /// Replies the driver absorbed before cancellation.
    pub replies_before_cancel: u32,
    /// Replies that arrived *after* cancellation, observed as runtime
    /// trace rejections (Tina) or never delivered (Tokio):
    /// - Tina: workers that already accepted their request keep
    ///   running; the runtime rejects each late reply with a typed
    ///   trace event (`CallReplyRejected` / `DeferredReplyRejected`).
    ///   This counter is non-zero whenever some worker finished after
    ///   cancel.
    /// - Tokio: `abort_all` preempts at the next await; aborted
    ///   tasks never run their reply path. Always 0.
    pub replies_after_cancel: u32,
    /// True when the host actually delivered the cancel signal.
    pub cancel_observed: bool,
    /// True when the side reached the end of `run` cleanly.
    pub exit_clean: bool,
}

/// Structural invariants both sides should satisfy. Exact
/// `replies_before_cancel` is timing-sensitive (depends on scheduler),
/// so we only check shape.
pub fn assert_report_invariants(side: &str, report: &Report) {
    assert!(
        report.replies_before_cancel < FANOUT,
        "{side}: cancel must fire before all workers finish, got {report:?}",
    );
    assert!(
        report.cancel_observed,
        "{side}: cancel signal should reach the requester, got {report:?}",
    );
    assert!(
        report.exit_clean,
        "{side}: expected exit_clean, got {report:?}"
    );
    // The driver never absorbs a post-cancel reply: in Tokio the
    // task is aborted; in Tina the runtime rejects late replies as
    // typed trace events instead of delivering them.
    assert!(
        report
            .replies_before_cancel
            .saturating_add(report.replies_after_cancel)
            <= FANOUT,
        "{side}: replies_before + replies_after must not exceed FANOUT, got {report:?}",
    );
}
