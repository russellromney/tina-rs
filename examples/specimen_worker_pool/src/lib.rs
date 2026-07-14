//! Worker pool with typed deferred calls.
//!
//! The frontend moves each request's [`tina::RequestContext`] directly into
//! the matching worker-call continuation. Workers have varied work times, so
//! replies arrive out of order without any application-level request IDs or
//! correlation table.

pub mod tina_impl;
pub mod tokio_impl;

pub const WORKERS: usize = 3;
pub const CLIENTS: usize = 8;
pub const DRIVER_BURST_CAP: usize = 8;

/// Each request carries a payload; the worker returns
/// `payload + worker_id`. Used by the smoke test to confirm the
/// frontend routed each reply back to the right caller.
pub fn expected_for(payload: u64, worker_id: u64) -> u64 {
    payload.wrapping_add(worker_id)
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct TerminalReport {
    pub worker_timer_failed: Vec<tina_runtime::CallError>,
    pub worker_full: usize,
    pub worker_closed: usize,
    pub worker_timeout: usize,
    pub worker_rejected: Vec<tina::CallRejectedReason>,
    pub frontend_full: usize,
    pub frontend_closed: usize,
    pub frontend_timeout: usize,
    pub frontend_rejected: Vec<tina::CallRejectedReason>,
    pub tokio_worker_channel_closed: usize,
    pub tokio_reply_channel_closed: usize,
}

impl TerminalReport {
    pub fn total(&self) -> usize {
        self.worker_timer_failed.len()
            + self.worker_full
            + self.worker_closed
            + self.worker_timeout
            + self.worker_rejected.len()
            + self.frontend_full
            + self.frontend_closed
            + self.frontend_timeout
            + self.frontend_rejected.len()
            + self.tokio_worker_channel_closed
            + self.tokio_reply_channel_closed
    }

    pub fn is_empty(&self) -> bool {
        self.total() == 0
    }
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Report {
    pub clients: usize,
    pub correct_replies: usize,
    pub wrong_replies: usize,
    pub terminals: TerminalReport,
    pub exit_clean: bool,
}
