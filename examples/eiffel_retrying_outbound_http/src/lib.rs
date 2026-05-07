//! Tina-vs-Tokio: outbound HTTP with caller-owned retry against a
//! flaky upstream.
//!
//! Both sides hit a small upstream that returns `503` for the first
//! [`FLAKY_503_RUNS`] requests and `200 OK` after that. The point is
//! how retry shows up in user code:
//!
//! - Tokio: hand-rolled retry loop around `reqwest::Client::send`.
//!   Per-attempt timeout via `tokio::time::timeout`, sleep between
//!   attempts via `tokio::time::sleep`.
//! - Tina: [`tina_reqwest_bridge::ReqwestWorker`] with
//!   [`tina_reqwest_bridge::RetryPolicy::None`] — no hidden retry — and
//!   a small `Caller` isolate that drives retry with `sleep(...).reply(...)`
//!   and `call(...).reply(...)`. Every attempt is one trace event.
//!
//! Both sides produce the same [`Report`].
//!
//! See `tests/smoke.rs` for the equivalence proof.

pub mod tina_impl;
pub mod tokio_impl;

/// Number of requests that should fail with `503` before the server
/// flips to `200 OK`. Both sides share this so the retry budget below
/// is exactly enough.
pub const FLAKY_503_RUNS: u32 = 2;

/// Maximum total attempts (initial + retries). With `FLAKY_503_RUNS = 2`
/// and `MAX_ATTEMPTS = 3`, attempts 1 and 2 see `503`, attempt 3 sees
/// `200`.
pub const MAX_ATTEMPTS: u32 = 3;

/// What each side observed end-to-end.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Report {
    /// Total attempts made. Both sides retry on `503`, so this should
    /// be `FLAKY_503_RUNS + 1` when retry succeeds and `MAX_ATTEMPTS`
    /// when it gives up at the budget.
    pub attempts_made: u32,
    /// Number of `503` responses seen during the retry loop.
    pub transient_503_count: u32,
    /// Whether the final attempt produced `200 OK`.
    pub final_ok: bool,
    /// Whether each side reached the end of `run` without bailing out.
    pub exit_clean: bool,
}

/// Expected counts under [`FLAKY_503_RUNS`] and [`MAX_ATTEMPTS`]: two
/// transient `503`s, then `200` on the third attempt.
pub fn expected_report() -> Report {
    Report {
        attempts_made: FLAKY_503_RUNS + 1,
        transient_503_count: FLAKY_503_RUNS,
        final_ok: true,
        exit_clean: true,
    }
}
