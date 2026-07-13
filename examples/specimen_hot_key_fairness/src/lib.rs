//! Sharded keyspace under skewed traffic. 90% of writes target a
//! single hot key; the rest spread across cold keys. Each shard has
//! a bounded queue and a fixed processing rate, so the hot shard
//! must reject some writes while the cold shards stay responsive.
//!
//! Both sides report admits and full-rejects per shard. The point
//! is that overload on the hot shard is *visible*, not hidden in a
//! growing buffer.

pub mod tina_impl;
pub mod tokio_impl;

pub const SHARDS: u32 = 3;
pub const HOT_WRITES: u32 = 30;
/// Cold writes per shard. Kept at or below `SHARD_MAILBOX` so a
/// well-behaved cold shard absorbs the whole burst with no
/// rejections — the contrast with the hot shard's overflow is
/// what the smoke test asserts.
pub const COLD_WRITES_PER_SHARD: u32 = 4;
pub const SHARD_MAILBOX: usize = 4;
pub const PER_WRITE_MS: u64 = 5;

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Report {
    pub hot_admitted: u32,
    pub hot_rejected: u32,
    /// Hot submissions rejected because the mailbox closed or the worker stopped.
    pub hot_terminal: u32,
    pub cold_admitted: u32,
    pub cold_rejected: u32,
    /// Cold submissions rejected because the mailbox closed or the worker stopped.
    pub cold_terminal: u32,
    pub hot_turns: u64,
    pub cold_min_turns: u64,
    pub cold_min_expected_turns: u64,
    pub max_cold_progress_deficit_turns: u64,
    pub max_progress_gap_turns: u64,
    pub trace_hash: u64,
    pub fairness_line: String,
    pub exit_clean: bool,
}

pub fn assert_report_invariants(side: &str, r: &Report) {
    let hot_total = r.hot_admitted + r.hot_rejected + r.hot_terminal;
    let cold_total = r.cold_admitted + r.cold_rejected + r.cold_terminal;
    assert_eq!(hot_total, HOT_WRITES, "{side}: {r:?}");
    let cold_expected = (SHARDS - 1) * COLD_WRITES_PER_SHARD;
    assert_eq!(cold_total, cold_expected, "{side}: {r:?}");
    assert_eq!(r.hot_terminal, 0, "{side}: hot worker stopped during burst: {r:?}");
    assert_eq!(r.cold_terminal, 0, "{side}: cold worker stopped during burst: {r:?}");
    assert!(
        r.hot_rejected > 0,
        "{side}: hot shard should overflow under skew, got {r:?}",
    );
    assert_eq!(
        r.cold_rejected, 0,
        "{side}: cold shards should keep up at the configured rate, got {r:?}",
    );
    if side == "tina" {
        assert!(
            r.cold_min_turns > 0,
            "{side}: cold shards must make observable progress, got {r:?}",
        );
        assert!(
            r.cold_min_turns >= r.cold_min_expected_turns,
            "{side}: cold shards must process every admitted cold write plus drain, got {r:?}",
        );
        assert!(
            r.max_cold_progress_deficit_turns == 0,
            "{side}: cold shard progress deficit must be reported and zero on the smoke profile, got {r:?}",
        );
        assert!(
            r.fairness_line.contains("progress_gap_turns"),
            "{side}: fairness report must name its observable lag unit, got {r:?}",
        );
    }
    assert!(r.exit_clean, "{side}: {r:?}");
}

#[cfg(test)]
mod tests {
    use super::*;

    fn good_tina_report() -> Report {
        Report {
            hot_admitted: SHARD_MAILBOX as u32,
            hot_rejected: HOT_WRITES - SHARD_MAILBOX as u32,
            hot_terminal: 0,
            cold_admitted: (SHARDS - 1) * COLD_WRITES_PER_SHARD,
            cold_rejected: 0,
            cold_terminal: 0,
            hot_turns: 9,
            cold_min_turns: 9,
            cold_min_expected_turns: 9,
            max_cold_progress_deficit_turns: 0,
            max_progress_gap_turns: 0,
            trace_hash: 42,
            fairness_line: "lag kind=progress_gap_turns subject=2 reference=1 observed=0 bound=none exceeded=false".to_string(),
            exit_clean: true,
        }
    }

    #[test]
    fn tina_invariants_accept_visible_fairness_report() {
        assert_report_invariants("tina", &good_tina_report());
    }

    #[test]
    #[should_panic(expected = "cold shards must make observable progress")]
    fn tina_invariants_reject_no_cold_progress() {
        let mut report = good_tina_report();
        report.cold_min_turns = 0;
        assert_report_invariants("tina", &report);
    }

    #[test]
    #[should_panic(expected = "cold shards must process every admitted cold write plus drain")]
    fn tina_invariants_reject_incomplete_cold_work() {
        let mut report = good_tina_report();
        report.cold_min_turns = 8;
        assert_report_invariants("tina", &report);
    }

    #[test]
    #[should_panic(expected = "cold shard progress deficit must be reported and zero")]
    fn tina_invariants_reject_reported_progress_deficit() {
        let mut report = good_tina_report();
        report.max_cold_progress_deficit_turns = 1;
        assert_report_invariants("tina", &report);
    }

    #[test]
    #[should_panic(expected = "must name its observable lag unit")]
    fn tina_invariants_reject_unnamed_lag_unit() {
        let mut report = good_tina_report();
        report.fairness_line = "latency_ms=0".to_string();
        assert_report_invariants("tina", &report);
    }

    #[test]
    #[should_panic(expected = "hot worker stopped during burst")]
    fn tina_invariants_reject_terminal_hot_submission() {
        let mut report = good_tina_report();
        report.hot_rejected -= 1;
        report.hot_terminal = 1;
        assert_report_invariants("tina", &report);
    }

    #[test]
    #[should_panic(expected = "cold worker stopped during burst")]
    fn tina_invariants_reject_terminal_cold_submission() {
        let mut report = good_tina_report();
        report.cold_admitted -= 1;
        report.cold_terminal = 1;
        assert_report_invariants("tina", &report);
    }
}
