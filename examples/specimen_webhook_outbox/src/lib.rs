//! Tina vs. hand-rolled: a webhook outbox that records before it sends,
//! survives a restart, and resumes the work it never finished.
//!
//! Phase A enqueues three webhooks. Two are sent and durably marked sent; the
//! third is sent but the process "crashes" before the mark is durable. Phase B
//! is a fresh process: recover, compact the journal, and resume the one unsent
//! webhook. Because the first form is **at-least-once**, the third webhook is
//! delivered again — that is the honest outcome, not a bug.
//!
//! - [`tina_impl`] composes [`tina_runtime::DurableOutbox`] with the journal
//!   rails: record-before-apply is a type rule, recovery is typed, compaction
//!   and the resume queue are one call each.
//! - [`hand_impl`] writes the same outbox by hand over a flat file, so you can
//!   see everything the durable form has to get right: append-before-send,
//!   dedup of completed work, journal growth, and recovery.
//!
//! Both sides must produce the same [`Report`].

pub mod hand_impl;
pub mod tina_impl;

/// The three webhooks Phase A enqueues.
pub const WEBHOOKS: [&str; 3] = ["order.created", "order.paid", "order.shipped"];

/// What each side observed across the two phases.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Report {
    /// Webhooks delivered (side effect performed) in Phase A.
    pub phase_a_sent: u64,
    /// Webhooks durably marked sent in Phase A (one fewer — the crash).
    pub phase_a_marked: u64,
    /// Pending webhooks recovered at the start of Phase B.
    pub recovered_pending: u64,
    /// Webhooks re-delivered in Phase B (the at-least-once replay).
    pub phase_b_resent: u64,
    /// Webhooks durably marked sent after Phase B.
    pub final_marked: u64,
    /// Journal records on disk before compaction.
    pub journal_records_before_compaction: u64,
    /// Journal records on disk after compaction (only the live backlog).
    pub journal_records_after_compaction: u64,
    /// Recovery reported a clean tail (the commit fence was clear).
    pub exit_clean: bool,
}

/// The Report both sides must produce: three sent in A, two marked (the third
/// crashed), one recovered + resent in B, three marked in total. Compaction
/// drops the two completed records, leaving the one still-pending record.
pub const EXPECTED: Report = Report {
    phase_a_sent: 3,
    phase_a_marked: 2,
    recovered_pending: 1,
    phase_b_resent: 1,
    final_marked: 3,
    journal_records_before_compaction: 5, // 3 enqueue + 2 complete
    journal_records_after_compaction: 1,  // only the one pending enqueue
    exit_clean: true,
};
