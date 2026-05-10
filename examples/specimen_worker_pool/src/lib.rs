//! Worker pool with deferred replies.
//!
//! Frontend captures one [`tina::DeferredReply`] per client request,
//! dispatches to the next worker round-robin, and replies through
//! the matching slot when the worker finishes. Workers have varied
//! work times so replies arrive out of order.

pub mod tina_impl;
pub mod tokio_impl;

pub const WORKERS: usize = 3;
pub const CLIENTS: usize = 8;
pub const MAX_PENDING: usize = 8;

/// Each request carries a payload; the worker returns
/// `payload + worker_id`. Used by the smoke test to confirm the
/// frontend routed each reply back to the right caller.
pub fn expected_for(payload: u64, worker_id: u64) -> u64 {
    payload.wrapping_add(worker_id)
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Report {
    pub clients: usize,
    pub correct_replies: usize,
    pub wrong_replies: usize,
    pub failed: usize,
    pub exit_clean: bool,
}
