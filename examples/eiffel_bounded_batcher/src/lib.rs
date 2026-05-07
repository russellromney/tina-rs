//! Bounded batcher with deferred replies.
//!
//! Many callers each `call(batcher, Submit(item))`. The batcher
//! holds every caller's reply slot until either:
//!
//! - the batch size hits [`BATCH_SIZE`], or
//! - [`BATCH_TIMEOUT_MS`] elapses since the first item.
//!
//! Then it computes the batch result (sum of items) and replies to
//! every caller with the same total. Callers see the batch reply
//! through their original `call(...).reply(...)` continuation.

pub mod tina_impl;
pub mod tokio_impl;

pub const CALLERS: usize = 12;
pub const BATCH_SIZE: usize = 4;
pub const BATCH_TIMEOUT_MS: u64 = 30;
pub const MAX_PENDING: usize = 16;

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Report {
    pub callers: usize,
    pub successes: usize,
    pub full_rejects: usize,
    pub failed: usize,
    pub batches_size_flushed: usize,
    pub batches_timer_flushed: usize,
    pub exit_clean: bool,
}
