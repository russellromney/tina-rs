//! Explicit drain state for service shutdown (Phase 101 Rock 4).
//!
//! `DrainState` is small state plus a small report. It does not close any
//! resource, send any message, or pick the ordering. The service still:
//!
//! 1. calls [`DrainState::begin`] to switch to "no new admission";
//! 2. settles every parked caller with the service's chosen reply;
//! 3. waits for or retires outstanding in-flight work;
//! 4. emits a final report and the runtime `stop_with` effect.
//!
//! The helper records counters and exposes [`DrainState::can_stop`] so the
//! "ready to stop" check is one line. Late completion after drain stays
//! visible: the helper rejects re-opening admission and counts the late event.

/// Stage of a service drain.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DrainStage {
    /// Service still admits new work.
    Open,
    /// Service is draining: no new admission, settling parked + in-flight.
    Draining,
    /// Service has emitted its final report. Late completion is recorded but
    /// state cannot transition back.
    Stopped,
}

/// Snapshot of one drain.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct DrainReport {
    /// Current stage of the drain.
    pub stage: DrainStage,
    /// Number of admit attempts the service accepted before drain began.
    pub admitted: u64,
    /// Number of admitted work items that completed cleanly.
    pub completed: u64,
    /// Number of admitted work items that were explicitly cancelled or
    /// retired during drain.
    pub cancelled_or_retired: u64,
    /// Number of admitted work items that were dropped during drain (no
    /// reply, no retire — only counted, never silently recovered).
    pub dropped: u64,
    /// Number of admit attempts that the service refused because the
    /// resource was at capacity.
    pub full: u64,
    /// Number of new admit attempts that arrived after [`begin`](DrainState::begin).
    /// These must be rejected by the service with a typed "Stopping" reply.
    pub admits_after_drain: u64,
    /// Number of completion events that landed after [`finish`](DrainState::finish).
    /// They are visible and never reopen admission.
    pub late_completions: u64,
}

impl DrainReport {
    /// `true` if the drain has begun and no work is outstanding.
    pub const fn ready_to_stop(self, outstanding: u64) -> bool {
        matches!(self.stage, DrainStage::Draining) && outstanding == 0
    }
}

/// State for one shutdown drain.
///
/// `DrainState` is plain data. The service drives transitions explicitly.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[must_use = "DrainState is state; transition with begin/finish and record() admit/complete events"]
pub struct DrainState {
    stage: DrainStage,
    admitted: u64,
    completed: u64,
    cancelled_or_retired: u64,
    dropped: u64,
    full: u64,
    admits_after_drain: u64,
    late_completions: u64,
}

impl Default for DrainState {
    fn default() -> Self {
        Self::new()
    }
}

impl DrainState {
    /// Creates a fresh drain state in [`DrainStage::Open`].
    pub const fn new() -> Self {
        Self {
            stage: DrainStage::Open,
            admitted: 0,
            completed: 0,
            cancelled_or_retired: 0,
            dropped: 0,
            full: 0,
            admits_after_drain: 0,
            late_completions: 0,
        }
    }

    /// Current stage.
    pub const fn stage(&self) -> DrainStage {
        self.stage
    }

    /// `true` if the service is still open for new admission.
    pub const fn is_open(&self) -> bool {
        matches!(self.stage, DrainStage::Open)
    }

    /// `true` if the service has begun draining and has not yet stopped.
    pub const fn is_draining(&self) -> bool {
        matches!(self.stage, DrainStage::Draining)
    }

    /// `true` if the service has emitted the final report.
    pub const fn is_stopped(&self) -> bool {
        matches!(self.stage, DrainStage::Stopped)
    }

    /// Returns a snapshot.
    pub const fn report(&self) -> DrainReport {
        DrainReport {
            stage: self.stage,
            admitted: self.admitted,
            completed: self.completed,
            cancelled_or_retired: self.cancelled_or_retired,
            dropped: self.dropped,
            full: self.full,
            admits_after_drain: self.admits_after_drain,
            late_completions: self.late_completions,
        }
    }

    /// Records one admission. Returns `true` if the service should accept it,
    /// `false` if the service should reject with a typed "Stopping" / "Closed"
    /// reply. Capacity decisions remain with the caller; this only refuses
    /// when [`begin`](Self::begin) has been called.
    pub fn admit(&mut self) -> AdmitDecision {
        if self.is_open() {
            self.admitted = self.admitted.saturating_add(1);
            AdmitDecision::Accept
        } else {
            self.admits_after_drain = self.admits_after_drain.saturating_add(1);
            AdmitDecision::Stopping
        }
    }

    /// Records that an admitted unit completed cleanly. Safe to call after
    /// [`finish`](Self::finish): the late completion is counted but does not
    /// move the stage back.
    pub fn record_complete(&mut self) {
        if self.is_stopped() {
            self.late_completions = self.late_completions.saturating_add(1);
            return;
        }
        self.completed = self.completed.saturating_add(1);
    }

    /// Records that an admitted unit was explicitly cancelled or retired
    /// during drain.
    pub fn record_cancelled_or_retired(&mut self) {
        if self.is_stopped() {
            self.late_completions = self.late_completions.saturating_add(1);
            return;
        }
        self.cancelled_or_retired = self.cancelled_or_retired.saturating_add(1);
    }

    /// Records that an admitted unit was dropped without a reply.
    pub fn record_dropped(&mut self) {
        if self.is_stopped() {
            self.late_completions = self.late_completions.saturating_add(1);
            return;
        }
        self.dropped = self.dropped.saturating_add(1);
    }

    /// Records that an admit attempt was refused by the service because the
    /// resource was at capacity.
    pub fn record_full(&mut self) {
        self.full = self.full.saturating_add(1);
    }

    /// Begins drain: refuses further admission.
    ///
    /// Safe to call more than once; subsequent calls are no-ops.
    pub fn begin(&mut self) {
        if matches!(self.stage, DrainStage::Open) {
            self.stage = DrainStage::Draining;
        }
    }

    /// `true` if the service can stop now: drain has begun and `outstanding`
    /// in-flight work is zero. The service supplies its own outstanding count
    /// because that lives elsewhere ([`crate::LocalPermitGate`], a pending set,
    /// etc.).
    pub const fn can_stop(&self, outstanding: u64) -> bool {
        matches!(self.stage, DrainStage::Draining) && outstanding == 0
    }

    /// Marks the drain as stopped. Late completion events recorded after this
    /// point are routed to `late_completions` and do not move the stage.
    pub fn finish(&mut self) -> DrainReport {
        if !self.is_stopped() {
            self.stage = DrainStage::Stopped;
        }
        self.report()
    }
}

/// Decision returned by [`DrainState::admit`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[must_use = "service must reply with the matching typed admission outcome"]
pub enum AdmitDecision {
    /// Admission accepted; the service should reserve a slot.
    Accept,
    /// Admission refused because drain has begun; the service must reply
    /// with its typed "Stopping" / "Closed" outcome.
    Stopping,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stop_while_idle() {
        let mut drain = DrainState::new();
        drain.begin();
        assert!(drain.can_stop(0));
        let report = drain.finish();
        assert_eq!(report.stage, DrainStage::Stopped);
        assert_eq!(report.admitted, 0);
        assert_eq!(report.completed, 0);
    }

    #[test]
    fn admit_after_begin_returns_stopping() {
        let mut drain = DrainState::new();
        assert_eq!(drain.admit(), AdmitDecision::Accept);
        drain.begin();
        assert_eq!(drain.admit(), AdmitDecision::Stopping);
        assert_eq!(drain.admit(), AdmitDecision::Stopping);
        let report = drain.report();
        assert_eq!(report.admitted, 1);
        assert_eq!(report.admits_after_drain, 2);
    }

    #[test]
    fn stop_blocked_until_outstanding_drains() {
        let mut drain = DrainState::new();
        let _ = drain.admit();
        let _ = drain.admit();
        drain.begin();
        assert!(!drain.can_stop(2), "still 2 outstanding");
        drain.record_complete();
        assert!(!drain.can_stop(1), "still 1 outstanding");
        drain.record_complete();
        assert!(drain.can_stop(0));
    }

    #[test]
    fn late_completion_counts_without_reopening_admission() {
        let mut drain = DrainState::new();
        let _ = drain.admit();
        drain.begin();
        drain.finish();
        assert_eq!(drain.admit(), AdmitDecision::Stopping);
        drain.record_complete();
        drain.record_complete();
        let report = drain.report();
        assert_eq!(report.stage, DrainStage::Stopped);
        assert_eq!(report.late_completions, 2);
        assert_eq!(report.completed, 0, "completions after finish are late");
    }

    #[test]
    fn cancelled_and_dropped_record_separately() {
        let mut drain = DrainState::new();
        let _ = drain.admit();
        let _ = drain.admit();
        let _ = drain.admit();
        drain.begin();
        drain.record_complete();
        drain.record_cancelled_or_retired();
        drain.record_dropped();
        let report = drain.report();
        assert_eq!(report.completed, 1);
        assert_eq!(report.cancelled_or_retired, 1);
        assert_eq!(report.dropped, 1);
    }

    #[test]
    fn full_counter_is_independent_of_drain_stage() {
        let mut drain = DrainState::new();
        drain.record_full();
        drain.record_full();
        drain.begin();
        drain.record_full();
        assert_eq!(drain.report().full, 3);
    }
}
