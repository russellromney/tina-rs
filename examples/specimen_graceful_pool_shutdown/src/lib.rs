//! Stop a pool while callers are pending.
//!
//! Frontend captures one `DeferredReply` per inbound caller,
//! dispatches to slow workers, and then receives a `Shutdown`
//! message before any worker finishes. The contract:
//!
//! - the frontend stops cleanly,
//! - every still-pending caller sees a typed `Closed` reply (no
//!   silent drop),
//! - the host does not hang.

pub mod tina_impl;
pub mod tokio_impl;

pub const CALLERS: usize = 6;
pub const WORKERS: usize = 2;
pub const MAX_PENDING: usize = 8;
pub const WORK_MS: u64 = 200;
pub const SHUTDOWN_AFTER_MS: u64 = 30;

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Report {
    pub callers: usize,
    pub completed: usize,
    pub closed: usize,
    pub failed: usize,
    pub shutdown_close_observed: bool,
    pub exit_clean: bool,
    /// Tina-only layered terminal accounting. The Tokio comparison leaves
    /// it empty because it does not have Tina's pool/call lanes.
    pub tina_terminals: TinaTerminalCounts,
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct TinaTerminalCounts {
    pub acquire_full: usize,
    pub acquire_closed: usize,
    pub acquire_wrong_shard: usize,
    pub acquire_call_timeout: usize,
    pub acquire_call_full: usize,
    pub acquire_call_closed: usize,
    pub acquire_call_rejections: Vec<tina::CallRejectedReason>,
    pub acquire_wrong_reply: usize,
    pub worker_full: usize,
    pub worker_closed: usize,
    pub worker_timeout: usize,
    pub worker_rejections: Vec<tina::CallRejectedReason>,
    pub worker_timer_failures: Vec<tina_runtime::CallError>,
    pub release_retired: usize,
    pub release_stale_lease: usize,
    pub release_double_release: usize,
    pub release_pool_closed: usize,
    pub release_call_timeout: usize,
    pub release_call_full: usize,
    pub release_call_closed: usize,
    pub release_call_rejections: Vec<tina::CallRejectedReason>,
    pub release_wrong_reply: usize,
    pub close_full: usize,
    pub close_closed: usize,
    pub close_timeout: usize,
    pub close_rejections: Vec<tina::CallRejectedReason>,
    pub close_wrong_reply: usize,
    pub shutdown_timer_failures: Vec<tina_runtime::CallError>,
}

pub fn assert_report_invariants(side: &str, r: &Report) {
    let total = r.completed + r.closed + r.failed;
    assert_eq!(total, CALLERS, "{side}: {r:?}");
    assert!(
        r.closed > 0,
        "{side}: shutdown should close some, got {r:?}"
    );
    assert!(
        r.shutdown_close_observed,
        "{side}: terminal report should prove shutdown close was observed, got {r:?}"
    );
    assert!(r.exit_clean, "{side}: {r:?}");
}

pub fn assert_tina_terminal_invariants(r: &Report) {
    assert_eq!(r.completed, WORKERS, "tina: {r:?}");
    assert_eq!(r.closed, CALLERS - WORKERS, "tina: {r:?}");
    assert_eq!(r.failed, 0, "tina: {r:?}");
    assert_eq!(
        r.tina_terminals.acquire_closed,
        CALLERS - WORKERS,
        "tina: every parked acquire must report Closed: {r:?}"
    );
    assert!(
        r.tina_terminals.acquire_call_rejections.is_empty(),
        "tina: acquire rejection reasons must remain observable: {r:?}"
    );
    assert!(
        r.tina_terminals.worker_rejections.is_empty(),
        "tina: worker rejection reasons must remain observable: {r:?}"
    );
    assert!(
        r.tina_terminals.worker_timer_failures.is_empty(),
        "tina: worker timer failures must remain observable: {r:?}"
    );
    assert!(
        r.tina_terminals.release_call_rejections.is_empty(),
        "tina: release rejection reasons must remain observable: {r:?}"
    );
    assert!(
        r.tina_terminals.close_rejections.is_empty(),
        "tina: close rejection reasons must remain observable: {r:?}"
    );
    assert!(
        r.tina_terminals.shutdown_timer_failures.is_empty(),
        "tina: shutdown timer failures must remain observable: {r:?}"
    );
    let unexpected_scalar_terminals = r.tina_terminals.acquire_full
        + r.tina_terminals.acquire_wrong_shard
        + r.tina_terminals.acquire_call_timeout
        + r.tina_terminals.acquire_call_full
        + r.tina_terminals.acquire_call_closed
        + r.tina_terminals.acquire_wrong_reply
        + r.tina_terminals.worker_full
        + r.tina_terminals.worker_closed
        + r.tina_terminals.worker_timeout
        + r.tina_terminals.release_retired
        + r.tina_terminals.release_stale_lease
        + r.tina_terminals.release_double_release
        + r.tina_terminals.release_pool_closed
        + r.tina_terminals.release_call_timeout
        + r.tina_terminals.release_call_full
        + r.tina_terminals.release_call_closed
        + r.tina_terminals.release_wrong_reply
        + r.tina_terminals.close_full
        + r.tina_terminals.close_closed
        + r.tina_terminals.close_timeout
        + r.tina_terminals.close_wrong_reply;
    assert_eq!(unexpected_scalar_terminals, 0, "tina: {r:?}");
}
