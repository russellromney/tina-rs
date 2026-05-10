//! Tina-vs-Tokio: a counter that survives a simulated process
//! restart via snapshot + journal.
//!
//! Phase A starts with a fresh dir, applies `PHASE_A_INCREMENTS`
//! increments, takes a snapshot. Phase B simulates a restart: drop
//! all in-memory state, recover from disk, apply
//! `PHASE_B_INCREMENTS` more increments without a snapshot. The
//! final value should equal the sum.
//!
//! Read [`tokio_impl`] and [`tina_impl`] top-to-bottom; the README
//! compares feel.

pub mod tina_impl;
pub mod tokio_impl;

pub const PHASE_A_INCREMENTS: u64 = 5;
pub const PHASE_B_INCREMENTS: u64 = 3;
pub const EXPECTED_FINAL_VALUE: u64 = PHASE_A_INCREMENTS + PHASE_B_INCREMENTS;

/// What each side observed across the two phases.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Report {
    pub phase_a_final: u64,
    pub phase_b_recovered: u64,
    pub phase_b_final: u64,
    pub snapshot_committed: bool,
    pub journal_records_phase_b: u64,
    pub exit_clean: bool,
}
