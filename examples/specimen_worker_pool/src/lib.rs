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

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Report {
    pub clients: usize,
    pub correct_replies: usize,
    pub wrong_replies: usize,
    pub failed: usize,
    pub exit_clean: bool,
}
