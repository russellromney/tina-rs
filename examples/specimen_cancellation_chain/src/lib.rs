//! Tokio-vs-Tina mid-flight cancellation.
//!
//! A driver fans out [`FANOUT`] slow worker calls (each takes
//! [`WORK_MS`] ms), then asks for cancellation [`CANCEL_AFTER_MS`] ms
//! later — well before the workers finish. We want to see how each
//! runtime surfaces "the requester gave up" to the workers and to
//! whatever was holding caller state.
//!
//! Tina ships first-form typed-service cancel:
//! `call_cancelable_request(...).then(...)`
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
    /// Calls rejected because the destination mailbox was full.
    pub call_full: u32,
    /// Calls rejected because the destination was closed.
    pub call_closed: u32,
    /// Calls whose mandatory wait timed out before cancellation.
    pub call_timeout: u32,
    /// Calls rejected for another typed runtime reason.
    pub call_rejected: u32,
    /// Waits successfully reclaimed by cancellation.
    pub cancel_cancelled: u32,
    /// Cancellation raced before runtime admission.
    pub cancel_not_admitted: u32,
    /// Cancellation arrived after the call had settled.
    pub cancel_already_completed: u32,
    /// Duplicate cancellation attempts.
    pub cancel_already_cancelled: u32,
    /// Cancellation attempted from the wrong shard.
    pub cancel_wrong_shard: u32,
    /// Entries still unsettled when the implementation join was consumed.
    pub pending: u32,
    /// Whether the implementation observed every required settlement fact.
    pub settlement_complete: bool,
    /// Stale, duplicate, or otherwise invalid settlement continuations.
    pub settlement_protocol_errors: u32,
    /// True when the host actually delivered the cancel signal.
    pub cancel_observed: bool,
    /// True when the side reached the end of `run` cleanly.
    pub exit_clean: bool,
}

/// Shape both sides share: cancel fired, exit was clean, and
/// `replies_before_cancel < FANOUT` (cancellation actually preempted
/// some work).
fn assert_shared_invariants(side: &str, report: &Report) {
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
    assert_eq!(report.pending, 0, "{side}: unsettled calls: {report:?}");
    assert!(
        report.settlement_complete,
        "{side}: incomplete settlement: {report:?}"
    );
    assert_eq!(
        report.settlement_protocol_errors, 0,
        "{side}: settlement protocol errors: {report:?}"
    );
}

/// Tina-side invariants. The runtime does not preempt the workers'
/// sleeps, so every worker eventually finishes; post-cancel replies
/// bounce as typed `CallReplyRejected { CallerCancelled }` events and
/// the host counts them in `replies_after_cancel`. The total must
/// equal `FANOUT`.
pub fn assert_tina_report_invariants(report: &Report) {
    assert_shared_invariants("tina", report);
    let total = report
        .replies_before_cancel
        .checked_add(report.replies_after_cancel)
        .expect("u32 overflow");
    assert_eq!(
        total, FANOUT,
        "tina: every worker should finish (delivered or rejected), got {report:?}",
    );
    assert_eq!(
        report.cancel_cancelled,
        FANOUT - report.replies_before_cancel,
        "tina: each pending call should be cancelled exactly once, got {report:?}",
    );
    assert_eq!(report.call_full, 0, "unexpected full call: {report:?}");
    assert_eq!(report.call_closed, 0, "unexpected closed call: {report:?}");
    assert_eq!(
        report.call_timeout, 0,
        "unexpected timed-out call: {report:?}"
    );
    assert_eq!(
        report.call_rejected, 0,
        "unexpected rejected call: {report:?}"
    );
    assert_eq!(
        report.cancel_not_admitted, 0,
        "unexpected cancel race: {report:?}"
    );
    assert_eq!(
        report.cancel_already_completed, 0,
        "unexpected late cancel: {report:?}"
    );
    assert_eq!(
        report.cancel_already_cancelled, 0,
        "unexpected duplicate cancel: {report:?}"
    );
    assert_eq!(
        report.cancel_wrong_shard, 0,
        "unexpected wrong-shard cancel: {report:?}"
    );
}

/// Tokio-side invariants. `JoinSet::abort_all` preempts at the next
/// await boundary; for the current worker shape (`sleep().await; i`)
/// aborts during sleep skip the reply, so `replies_after_cancel` is
/// typically zero. We assert a loose `replies_before + after <= FANOUT`
/// so a refactor that adds an await between sleep and the reply path
/// (e.g. an `mpsc::Sender::send().await`) does not silently flake the
/// test on a slow CI runner.
pub fn assert_tokio_report_invariants(report: &Report) {
    assert_shared_invariants("tokio", report);
    let total = report
        .replies_before_cancel
        .saturating_add(report.replies_after_cancel);
    assert!(
        total <= FANOUT,
        "tokio: replies_before + replies_after must not exceed FANOUT, got {report:?}",
    );
}
