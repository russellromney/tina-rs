//! Tokio-vs-Tina periodic batcher.
//!
//! Producer pushes [`TOTAL_ITEMS`] items as fast as the host can
//! `try_send`. The batcher accumulates them and flushes a batch when
//! either:
//!
//! - the batch hits [`BATCH_SIZE`] items, or
//! - the [`BATCH_INTERVAL_MS`] timer fires with at least one item
//!   pending.
//!
//! Both sides walk the same script and produce the same total count.
//! What we are looking at:
//!
//! - **Tokio**: `tokio::select!` between `mpsc::recv()` and
//!   `tokio::time::sleep_until(...)`. The two-arm select is the
//!   canonical "wake on either" shape; rearming the timer after each
//!   flush is one extra line.
//! - **Tina**: a single isolate. Items arrive as `Submit(item)`
//!   messages; the timer is a `sleep(...).reply(Tick)` continuation.
//!   Cancellation, rearming, and "is a timer in flight?" are all
//!   handler-local state — no `select!`, no future to drop.
//!
//! [`Report`] equality across sides is the proof that the timer +
//! bounded buffer + flush composition is honest in both runtimes.

pub mod tina_impl;
pub mod tokio_impl;

/// Items the host pushes through the batcher.
pub const TOTAL_ITEMS: u32 = 12;

/// Maximum batch size. Hitting this triggers an immediate flush.
pub const BATCH_SIZE: usize = 5;

/// How long the batcher waits after the first item before flushing
/// (if `BATCH_SIZE` is not reached first).
pub const BATCH_INTERVAL_MS: u64 = 30;

/// Inter-item gap on the producer side. Set so that:
/// - the first 5 items arrive faster than `BATCH_INTERVAL` (size flush),
/// - the next 5 arrive faster than `BATCH_INTERVAL` (size flush),
/// - the last 2 sit alone for the full `BATCH_INTERVAL` (timer flush).
pub const PRODUCER_GAP_MS: u64 = 1;

/// Pause the producer takes between bursts so the timer flush has a
/// chance to fire on the trailing batch.
pub const TRAILING_PAUSE_MS: u64 = 80;

/// What each side observed end-to-end.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Report {
    /// Total items the batcher saw.
    pub items_seen: u32,
    /// Number of size-triggered flushes (batch reached [`BATCH_SIZE`]).
    pub size_flushes: u32,
    /// Number of timer-triggered flushes (interval fired with a
    /// non-empty buffer).
    pub timer_flushes: u32,
    /// Whether each side reached the end of `run` cleanly.
    pub exit_clean: bool,
}

/// Expected counts under the constants above. Both sides should
/// produce two size flushes (items 1–5, items 6–10) and one timer
/// flush (items 11–12, after the trailing pause).
pub fn expected_report() -> Report {
    Report {
        items_seen: TOTAL_ITEMS,
        size_flushes: 2,
        timer_flushes: 1,
        exit_clean: true,
    }
}
