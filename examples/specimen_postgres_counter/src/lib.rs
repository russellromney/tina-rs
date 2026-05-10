//! Tokio-vs-Tina Postgres counter.
//!
//! Both sides create a private counter table, increment a single row
//! [`INCREMENTS`] times, read the final value back, and drop their
//! table on exit. The point is to compare how each runtime composes
//! with `sqlx::PgPool` against its visible-pressure contract.
//!
//! - **Tokio**: holds an `sqlx::PgPool`, executes statements via
//!   `pool.execute(...)` / `pool.fetch_one(...)`. Any pressure the
//!   pool experiences (acquire timeouts, query failures) bubbles up
//!   as `sqlx::Error` and the surrounding code decides what to do.
//! - **Tina**: drive `tina-sqlx-bridge`. The bridge owns the
//!   `PgPool`. The host script calls in with
//!   `ThreadedRuntime::call_blocking`; admission, in-flight, and
//!   per-attempt timeout are named caps with typed failure modes.
//!   Shard thread does no SQLx work.
//!
//! This specimen does **not** prove DB readiness. It surfaces the
//! shape of the gap between Tina's bounded-runtime story and a
//! production Postgres client.

pub mod tina_impl;
pub mod tokio_impl;

/// Number of increment operations the script issues.
pub const INCREMENTS: u32 = 50;

/// What each side observed end-to-end.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Report {
    /// Final counter value read from the database.
    pub final_value: u64,
    /// Whether each side reached the end of `run` cleanly.
    pub exit_clean: bool,
}

/// Reads `DATABASE_URL` from the environment. Returns `None` when
/// unset, which is how the specimen "skip on missing pg" path is
/// signalled.
pub fn database_url() -> Option<String> {
    std::env::var("DATABASE_URL").ok()
}

/// Per-process unique table name so concurrent runs (or other
/// consumers of the same database) do not collide.
pub fn unique_table(prefix: &str) -> String {
    let pid = std::process::id();
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("{prefix}_{pid}_{nanos}")
}
