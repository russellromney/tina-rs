//! Explicit Full-handling policy state.
//!
//! Several services need the same boring "on Full, shed or retry-with-backoff"
//! shape. [`FullHandling`] records the policy and returns a decision; it does
//! not send messages, schedule sleep, or own the mailbox.
//!
//! Retry delays come from [`tina::time::Backoff`]. Every retry delay is a
//! Tina `sleep` scheduled by the caller. The helper does not resend by
//! itself.

use std::time::{Duration, Instant};

use tina::Deadline;
use tina::time::Backoff;

/// Policy mode for [`FullHandling`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FullPolicyMode {
    /// Always shed on Full. No retry delay is ever returned.
    Shed,
    /// Retry on Full with the supplied backoff schedule until the schedule
    /// exhausts or the optional deadline elapses.
    RetryBackoff,
}

/// Snapshot of one [`FullHandling`]'s counters.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct FullHandlingReport {
    /// Configured policy mode.
    pub mode: FullPolicyMode,
    /// Number of times the helper returned [`FullDecision::Shed`].
    pub shed_count: u64,
    /// Number of times the helper returned a [`FullDecision::RetryAfter`]
    /// (counts every scheduled retry delay).
    pub retry_attempts: u64,
    /// Number of times the helper returned [`FullDecision::Exhausted`].
    pub exhausted_count: u64,
    /// Number of `on_full` observations that were the first attempt (no retry
    /// has yet been recorded for this caller obligation).
    pub first_attempt_full: u64,
    /// Number of `on_full` observations after at least one retry attempt.
    pub retry_full: u64,
    /// Number of [`FullHandling::record_success`] calls that resolved a
    /// caller obligation that had already incurred a retry.
    pub retry_success: u64,
    /// Number of [`FullHandling::record_success`] calls that resolved a
    /// caller obligation on the first attempt.
    pub first_attempt_success: u64,
}

/// Opaque token returned on each [`FullDecision::RetryAfter`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct FullHandlingToken {
    ordinal: u64,
}

impl FullHandlingToken {
    /// Strictly increasing ordinal of this retry-delay decision.
    pub const fn ordinal(self) -> u64 {
        self.ordinal
    }
}

/// Decision returned by [`FullHandling::on_full`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[must_use = "FullDecision must be matched and the caller must take action (reply or sleep)"]
pub enum FullDecision {
    /// Shed: reply with the typed Busy / Full immediately.
    Shed(FullHandlingReport),
    /// Retry: schedule `sleep(delay).then(...)` carrying `token`.
    RetryAfter {
        /// Duration to sleep before the next attempt.
        delay: Duration,
        /// Opaque ordinal that the caller embeds in the continuation message.
        token: FullHandlingToken,
        /// Snapshot of the helper after the decision was recorded.
        report: FullHandlingReport,
    },
    /// Retry exhausted: reply with typed Busy / Failed. No more sleeps will
    /// be scheduled until [`FullHandling::reset`] is called.
    Exhausted {
        /// Reason the helper stopped scheduling retries.
        reason: FullExhaustionReason,
        /// Snapshot of the helper after the decision was recorded.
        report: FullHandlingReport,
    },
}

/// Reason a [`FullDecision::Exhausted`] was returned.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FullExhaustionReason {
    /// The supplied [`Backoff`]'s attempt budget ran out.
    BudgetExhausted,
    /// The supplied [`Deadline`] elapsed before another retry could fit.
    DeadlineElapsed,
}

/// Plain state for the "on Full, shed or retry" decision shape.
///
/// `FullHandling` does not own a mailbox, message, address, or caller. It only
/// records attempts and returns the next decision.
#[derive(Debug, Clone, Copy)]
#[must_use = "FullHandling is state; store it on the isolate and call on_full(now, deadline)"]
pub struct FullHandling {
    mode: FullPolicyMode,
    backoff: Option<Backoff>,
    next_ordinal: u64,
    attempts: u64,
    shed_count: u64,
    retry_attempts: u64,
    exhausted_count: u64,
    first_attempt_full: u64,
    retry_full: u64,
    first_attempt_success: u64,
    retry_success: u64,
}

impl FullHandling {
    /// Always shed on Full. No retry timing is needed.
    pub const fn shed() -> Self {
        Self {
            mode: FullPolicyMode::Shed,
            backoff: None,
            next_ordinal: 0,
            attempts: 0,
            shed_count: 0,
            retry_attempts: 0,
            exhausted_count: 0,
            first_attempt_full: 0,
            retry_full: 0,
            first_attempt_success: 0,
            retry_success: 0,
        }
    }

    /// Retry on Full using `backoff`'s budget. The caller is responsible for
    /// the idempotency of the work being retried.
    pub const fn retry_backoff(backoff: Backoff) -> Self {
        Self {
            mode: FullPolicyMode::RetryBackoff,
            backoff: Some(backoff),
            next_ordinal: 0,
            attempts: 0,
            shed_count: 0,
            retry_attempts: 0,
            exhausted_count: 0,
            first_attempt_full: 0,
            retry_full: 0,
            first_attempt_success: 0,
            retry_success: 0,
        }
    }

    /// Configured policy mode.
    pub const fn mode(&self) -> FullPolicyMode {
        self.mode
    }

    /// Returns a snapshot.
    pub const fn report(&self) -> FullHandlingReport {
        FullHandlingReport {
            mode: self.mode,
            shed_count: self.shed_count,
            retry_attempts: self.retry_attempts,
            exhausted_count: self.exhausted_count,
            first_attempt_full: self.first_attempt_full,
            retry_full: self.retry_full,
            retry_success: self.retry_success,
            first_attempt_success: self.first_attempt_success,
        }
    }

    /// Records a Full observation and returns the next decision.
    pub fn on_full(&mut self, now: Instant, deadline: Option<Deadline>) -> FullDecision {
        let is_first_attempt = self.attempts == 0;
        if is_first_attempt {
            self.first_attempt_full = self.first_attempt_full.saturating_add(1);
        } else {
            self.retry_full = self.retry_full.saturating_add(1);
        }
        self.attempts = self.attempts.saturating_add(1);

        match self.mode {
            FullPolicyMode::Shed => {
                self.shed_count = self.shed_count.saturating_add(1);
                FullDecision::Shed(self.report())
            }
            FullPolicyMode::RetryBackoff => {
                let backoff = self
                    .backoff
                    .as_mut()
                    .expect("retry_backoff must carry a Backoff");
                match backoff.next_delay_until(now, deadline) {
                    tina::time::TimerDecision::Sleep(delay) => {
                        self.retry_attempts = self.retry_attempts.saturating_add(1);
                        self.next_ordinal = self.next_ordinal.saturating_add(1);
                        let token = FullHandlingToken {
                            ordinal: self.next_ordinal,
                        };
                        FullDecision::RetryAfter {
                            delay: delay.delay(),
                            token,
                            report: self.report(),
                        }
                    }
                    tina::time::TimerDecision::Exhausted => {
                        self.exhausted_count = self.exhausted_count.saturating_add(1);
                        FullDecision::Exhausted {
                            reason: FullExhaustionReason::BudgetExhausted,
                            report: self.report(),
                        }
                    }
                    tina::time::TimerDecision::DeadlineElapsed => {
                        self.exhausted_count = self.exhausted_count.saturating_add(1);
                        FullDecision::Exhausted {
                            reason: FullExhaustionReason::DeadlineElapsed,
                            report: self.report(),
                        }
                    }
                }
            }
        }
    }

    /// Records that the caller obligation resolved successfully. Resets the
    /// internal attempt counter so the next obligation begins fresh.
    pub fn record_success(&mut self) -> FullHandlingReport {
        if self.attempts > 0 {
            self.retry_success = self.retry_success.saturating_add(1);
        } else {
            self.first_attempt_success = self.first_attempt_success.saturating_add(1);
        }
        self.reset();
        self.report()
    }

    /// Resets the attempt count and the underlying [`Backoff`] sequence.
    /// Counter totals (shed_count, retry_attempts, exhausted_count,
    /// first_attempt_full, retry_full, retry_success, first_attempt_success)
    /// are preserved.
    pub fn reset(&mut self) {
        self.attempts = 0;
        if let Some(backoff) = self.backoff.as_mut() {
            backoff.reset();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn now_zero() -> Instant {
        Instant::now()
    }

    #[test]
    fn shed_returns_typed_decision_no_sleep() {
        let mut state = FullHandling::shed();
        match state.on_full(now_zero(), None) {
            FullDecision::Shed(report) => {
                assert_eq!(report.shed_count, 1);
                assert_eq!(report.first_attempt_full, 1);
            }
            other => panic!("expected Shed, got {other:?}"),
        }
    }

    #[test]
    fn retry_returns_bounded_sleeps_then_exhausted() {
        let backoff = Backoff::constant(Duration::from_millis(10), 2).unwrap();
        let mut state = FullHandling::retry_backoff(backoff);
        let now = now_zero();
        let first = state.on_full(now, None);
        let second = state.on_full(now, None);
        let third = state.on_full(now, None);

        match first {
            FullDecision::RetryAfter { delay, token, .. } => {
                assert_eq!(delay, Duration::from_millis(10));
                assert_eq!(token.ordinal(), 1);
            }
            other => panic!("expected first retry, got {other:?}"),
        }
        match second {
            FullDecision::RetryAfter { delay, token, .. } => {
                assert_eq!(delay, Duration::from_millis(10));
                assert_eq!(token.ordinal(), 2);
            }
            other => panic!("expected second retry, got {other:?}"),
        }
        match third {
            FullDecision::Exhausted { reason, report } => {
                assert_eq!(reason, FullExhaustionReason::BudgetExhausted);
                assert_eq!(report.retry_attempts, 2);
                assert_eq!(report.exhausted_count, 1);
                assert_eq!(report.first_attempt_full, 1);
                assert_eq!(report.retry_full, 2);
            }
            other => panic!("expected Exhausted, got {other:?}"),
        }
    }

    #[test]
    fn deadline_elapsed_returns_exhausted_without_sleep() {
        let backoff = Backoff::constant(Duration::from_millis(10), 4).unwrap();
        let mut state = FullHandling::retry_backoff(backoff);
        let now = now_zero();
        let deadline = Deadline::from_instant(now, Duration::from_millis(1));
        let past = now + Duration::from_millis(5);
        match state.on_full(past, Some(deadline)) {
            FullDecision::Exhausted { reason, .. } => {
                assert_eq!(reason, FullExhaustionReason::DeadlineElapsed);
            }
            other => panic!("expected DeadlineElapsed, got {other:?}"),
        }
    }

    #[test]
    fn record_success_resets_and_distinguishes_first_attempt() {
        let backoff = Backoff::constant(Duration::from_millis(10), 4).unwrap();
        let mut state = FullHandling::retry_backoff(backoff);
        let now = now_zero();
        let _ = state.on_full(now, None);
        let report = state.record_success();
        assert_eq!(report.retry_success, 1);
        assert_eq!(report.first_attempt_success, 0);

        // After reset, the next on_full counts as a first attempt again.
        match state.on_full(now, None) {
            FullDecision::RetryAfter { token, report, .. } => {
                assert_eq!(token.ordinal(), 2);
                assert_eq!(report.first_attempt_full, 2);
            }
            other => panic!("expected retry, got {other:?}"),
        }
        let report = state.record_success();
        assert_eq!(report.retry_success, 2);
    }

    #[test]
    fn first_attempt_success_is_separate_from_retry_success() {
        let mut state = FullHandling::shed();
        let report = state.record_success();
        assert_eq!(report.first_attempt_success, 1);
        assert_eq!(report.retry_success, 0);
    }

    #[test]
    fn report_separates_first_attempt_full_from_retry_full() {
        let backoff = Backoff::constant(Duration::from_millis(5), 4).unwrap();
        let mut state = FullHandling::retry_backoff(backoff);
        let now = now_zero();
        // Caller A: one first-attempt Full, then a retry Full.
        let _ = state.on_full(now, None);
        let _ = state.on_full(now, None);
        let _ = state.record_success();
        // Caller B: one first-attempt Full only.
        let _ = state.on_full(now, None);
        let report = state.report();
        assert_eq!(report.first_attempt_full, 2);
        assert_eq!(report.retry_full, 1);
    }
}
