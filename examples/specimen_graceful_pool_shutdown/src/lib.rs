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

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Report {
    pub callers: usize,
    pub completed: usize,
    pub closed: usize,
    pub failed: usize,
    pub exit_clean: bool,
}

pub fn assert_report_invariants(side: &str, r: &Report) {
    let total = r.completed + r.closed + r.failed;
    assert_eq!(total, CALLERS, "{side}: {r:?}");
    assert!(
        r.closed > 0,
        "{side}: shutdown should close some, got {r:?}"
    );
    assert!(r.exit_clean, "{side}: {r:?}");
}
