//! Single-in-flight gate for timer-driven workers (Phase 062 Rock 5).
//!
//! The pattern in `specimen_rate_limited_worker` and any
//! `sleep(window).reply(Tick)` rate-limited isolate is:
//!
//! - on submit: bump a `pending` counter; if it *was* zero, schedule
//!   `sleep(window).reply(Tick)`; else no-op.
//! - on tick: decrement `pending`; if more work remains, schedule
//!   another sleep; else no-op.
//!
//! `SingleCallGate` names that pattern so the invariant "at most one
//! timer/call in flight" is structural rather than five lines repeated
//! at each isolate. The gate is plain data — it does not own the
//! timer, the trace, or the message; the caller still writes
//! `sleep(...).reply(...)` itself. The runtime continues to record one
//! `Sleep` event per scheduled timer.
//!
//! No hidden timer, no hidden queue. Just a counter with a name.

/// Tracks how many pieces of work are pending behind a single-in-flight
/// timer or call. Construct one as a field on the isolate.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SingleCallGate {
    pending: u32,
}

impl SingleCallGate {
    /// Fresh gate: nothing pending, nothing in flight.
    pub const fn new() -> Self {
        Self { pending: 0 }
    }

    /// Number of submits not yet matched by a [`Self::complete`].
    /// Includes the in-flight one if any.
    pub fn pending(&self) -> u32 {
        self.pending
    }

    /// `true` when no submit is currently in flight or queued.
    pub fn is_idle(&self) -> bool {
        self.pending == 0
    }

    /// Records a new submit. Returns `true` if the caller should
    /// schedule the timer/call now (the gate was idle); `false` if a
    /// previous one is still in flight and will pick this up via
    /// [`Self::complete`].
    pub fn submit(&mut self) -> bool {
        let was_idle = self.pending == 0;
        self.pending += 1;
        was_idle
    }

    /// Records that the in-flight timer/call resolved. Returns `true`
    /// if more work remains and the caller should schedule another
    /// timer/call; `false` if the gate is now idle.
    ///
    /// # Panics
    ///
    /// Panics if called with no pending work. The caller must call
    /// this exactly once per resolved timer; a double-complete is a
    /// state-machine bug we want loud, not silent. If the caller
    /// genuinely needs to drop an in-flight tracked entry without
    /// scheduling another (e.g. an error bail), use
    /// [`Self::cancel_in_flight`] instead — that path is
    /// underflow-safe.
    pub fn complete(&mut self) -> bool {
        assert!(
            self.pending > 0,
            "SingleCallGate::complete called with nothing in flight"
        );
        self.pending -= 1;
        self.pending > 0
    }

    /// Cancels the in-flight timer's tracking without scheduling another
    /// one. Use when an error path bails on the worker (e.g. a sleep
    /// reply came back cancelled). Underflow-safe: cancelling on an
    /// idle gate is a no-op.
    pub fn cancel_in_flight(&mut self) {
        self.pending = self.pending.saturating_sub(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fresh_gate_is_idle() {
        let g = SingleCallGate::new();
        assert!(g.is_idle());
        assert_eq!(g.pending(), 0);
    }

    #[test]
    fn first_submit_returns_true_subsequent_return_false() {
        let mut g = SingleCallGate::new();
        assert!(g.submit(), "first submit should schedule the timer");
        assert!(!g.submit(), "second submit must wait");
        assert!(!g.submit(), "third submit must wait");
        assert_eq!(g.pending(), 3);
    }

    #[test]
    fn complete_returns_true_until_queue_drains() {
        let mut g = SingleCallGate::new();
        g.submit();
        g.submit();
        g.submit();
        assert!(g.complete(), "still 2 queued");
        assert!(g.complete(), "still 1 queued");
        assert!(!g.complete(), "drained");
        assert!(g.is_idle());
    }

    #[test]
    fn cancel_in_flight_does_not_underflow() {
        let mut g = SingleCallGate::new();
        g.cancel_in_flight();
        g.cancel_in_flight();
        assert!(g.is_idle());
    }

    #[test]
    fn cancel_in_flight_decrements_when_in_flight() {
        let mut g = SingleCallGate::new();
        g.submit();
        g.submit();
        g.cancel_in_flight();
        assert_eq!(g.pending(), 1);
    }

    #[test]
    #[should_panic(expected = "nothing in flight")]
    fn complete_panics_on_double_complete() {
        let mut g = SingleCallGate::new();
        g.submit();
        g.complete();
        g.complete();
    }

    #[test]
    #[should_panic(expected = "nothing in flight")]
    fn complete_panics_on_idle_gate() {
        let mut g = SingleCallGate::new();
        g.complete();
    }
}
