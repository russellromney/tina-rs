//! Bounded tombstone timers for request scopes.
//!
//! Plain [`sleep`](crate::sleep) is not `CallHandle`-cancelable: once a
//! timer is armed the runtime will fire it. That is fine for "wake me
//! later", but a request scope needs "this timer belongs to a request; if
//! the request goes away, the timer must not run user work."
//!
//! This module ships the honest answer. A [`ScopedTimerSet`] hands out a
//! ticketed [`ScopedTimer`] when a request arms a timer. Cancelling the
//! request *tombstones* the ticket — it does not stop the physical timer.
//! When the runtime sleep fires later, the continuation calls
//! [`ScopedTimerSet::observe_fire`] with the ticket and gets back the
//! truth:
//!
//! - [`ScopedTimerFire::Run`] — the timer is live; do the user work.
//! - [`ScopedTimerFire::IgnoredLate`] — the request cancelled this timer
//!   before it fired; skip user work and count it. The physical timer
//!   really did fire; we are choosing to ignore it, and we say so.
//! - [`ScopedTimerFire::Unknown`] — no entry for this ticket (already
//!   observed, or never armed); skip user work.
//!
//! There is no pretending physical cancellation happened. The set's
//! [`ScopedTimerSet::ignored_late`] counter is the visible truth of how
//! many late timers were ignored, and it feeds
//! [`ScopedRequestReport::timers_ignored_late`](crate::scope::ScopedRequestReport::timers_ignored_late).

use std::sync::atomic::{AtomicU64, Ordering};

/// Stable identifier for one armed scoped timer.
///
/// Process-monotonic via [`ScopedTimerId::alloc`]. A reused request key
/// never reuses a timer id, so a late fire for an old timer cannot be
/// confused with a new one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ScopedTimerId(u64);

static NEXT_TIMER_ID: AtomicU64 = AtomicU64::new(1);

impl ScopedTimerId {
    fn alloc() -> Self {
        Self(NEXT_TIMER_ID.fetch_add(1, Ordering::Relaxed))
    }

    /// Returns the raw timer identifier. Carry this into the sleep
    /// continuation so the fire can be matched back to its ticket.
    pub const fn get(self) -> u64 {
        self.0
    }
}

/// A request-owned timer ticket returned by [`ScopedTimerSet::arm`].
///
/// Named for the user action ("a timer for this request"), not for the
/// runtime sleep mechanics. The continuation that the runtime sleep fires
/// only needs the [`ScopedTimer::id`]; carry it as a small `u64` if you do
/// not want to hold the whole struct.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScopedTimer {
    id: ScopedTimerId,
    label: &'static str,
}

impl ScopedTimer {
    /// The ticket id to carry into the sleep continuation.
    pub const fn id(self) -> ScopedTimerId {
        self.id
    }

    /// Service-supplied label, e.g. `"request_deadline"`.
    pub const fn label(self) -> &'static str {
        self.label
    }
}

/// What [`ScopedTimerSet::observe_fire`] decided when a runtime sleep
/// fired.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScopedTimerFire {
    /// The timer is live. Run the user work attached to this deadline.
    Run {
        /// The timer's label.
        label: &'static str,
    },
    /// The timer was tombstoned (the request cancelled it) before the
    /// physical sleep fired. Skip user work. This is the honest
    /// "ignored late timer" outcome — the sleep really fired.
    IgnoredLate {
        /// The timer's label.
        label: &'static str,
    },
    /// No entry exists for this ticket. Either it was already observed or
    /// it was never armed in this set. Skip user work.
    Unknown,
}

impl ScopedTimerFire {
    /// `true` only for [`ScopedTimerFire::Run`].
    pub fn should_run(self) -> bool {
        matches!(self, Self::Run { .. })
    }
}

/// Reason [`ScopedTimerSet::arm`] declined.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScopedTimerArmError {
    /// The set is at capacity. The service armed more concurrent timers
    /// than it budgeted; refuse the extra one rather than growing.
    Full {
        /// Configured timer cap.
        cap: usize,
    },
}

#[derive(Debug)]
struct ScopedTimerEntry {
    id: ScopedTimerId,
    label: &'static str,
    cancelled: bool,
}

/// Bounded fixed-capacity storage of armed scoped timers.
///
/// One entry per live (or tombstoned-but-not-yet-fired) timer. The cap is
/// the maximum number of timers in flight at once; arming past the cap is
/// refused, never silently grown. Cancelling tombstones an entry; the
/// entry survives until its physical sleep fires and the continuation
/// calls [`Self::observe_fire`].
#[derive(Debug)]
pub struct ScopedTimerSet {
    entries: Vec<ScopedTimerEntry>,
    capacity: usize,
    ignored_late: u64,
}

impl ScopedTimerSet {
    /// Builds an empty set with fixed `capacity`.
    ///
    /// Panics when `capacity == 0`: a zero-cap timer set refuses every
    /// arm, which is never the intent.
    pub fn with_capacity(capacity: usize) -> Self {
        assert!(
            capacity > 0,
            "ScopedTimerSet requires capacity > 0; a zero-cap set refuses every arm",
        );
        Self {
            entries: Vec::with_capacity(capacity),
            capacity,
            ignored_late: 0,
        }
    }

    /// Configured timer capacity.
    pub const fn capacity(&self) -> usize {
        self.capacity
    }

    /// Number of timers currently tracked (live or tombstoned).
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether no timers are tracked.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Whether the next arm would be refused.
    pub fn is_full(&self) -> bool {
        self.entries.len() >= self.capacity
    }

    /// Cumulative count of late timers that fired after cancel and were
    /// ignored. This is the visible "we did not physically cancel; we
    /// ignored the late fire" truth.
    pub fn ignored_late(&self) -> u64 {
        self.ignored_late
    }

    /// Arms a timer and returns its ticket.
    ///
    /// The caller then issues a `sleep(d).then(move |_| Msg { id })`
    /// carrying `ticket.id().get()`; the continuation routes back through
    /// [`Self::observe_fire`].
    pub fn arm(&mut self, label: &'static str) -> Result<ScopedTimer, ScopedTimerArmError> {
        if self.is_full() {
            return Err(ScopedTimerArmError::Full { cap: self.capacity });
        }
        let id = ScopedTimerId::alloc();
        self.entries.push(ScopedTimerEntry {
            id,
            label,
            cancelled: false,
        });
        Ok(ScopedTimer { id, label })
    }

    /// Tombstones the timer with `id`. The physical sleep is not stopped;
    /// when it fires, [`Self::observe_fire`] will report
    /// [`ScopedTimerFire::IgnoredLate`]. Returns whether a live entry was
    /// found and tombstoned.
    pub fn cancel(&mut self, id: ScopedTimerId) -> bool {
        for entry in &mut self.entries {
            if entry.id == id {
                if entry.cancelled {
                    return false;
                }
                entry.cancelled = true;
                return true;
            }
        }
        false
    }

    /// Tombstones every live timer. Used when a scope is cancelled and all
    /// of its timers should be ignored when they fire. Returns the number
    /// of timers newly tombstoned.
    pub fn cancel_all(&mut self) -> usize {
        let mut tombstoned = 0;
        for entry in &mut self.entries {
            if !entry.cancelled {
                entry.cancelled = true;
                tombstoned += 1;
            }
        }
        tombstoned
    }

    /// Observes a physical timer fire for `id` and removes the entry.
    ///
    /// Returns [`ScopedTimerFire::Run`] for a live timer,
    /// [`ScopedTimerFire::IgnoredLate`] for a tombstoned one (incrementing
    /// [`Self::ignored_late`]), or [`ScopedTimerFire::Unknown`] if no
    /// entry matched.
    pub fn observe_fire(&mut self, id: ScopedTimerId) -> ScopedTimerFire {
        let Some(pos) = self.entries.iter().position(|entry| entry.id == id) else {
            return ScopedTimerFire::Unknown;
        };
        let entry = self.entries.swap_remove(pos);
        if entry.cancelled {
            self.ignored_late += 1;
            ScopedTimerFire::IgnoredLate { label: entry.label }
        } else {
            ScopedTimerFire::Run { label: entry.label }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn arm_then_live_fire_runs() {
        let mut timers = ScopedTimerSet::with_capacity(2);
        let t = timers.arm("deadline").expect("arm");
        assert_eq!(timers.len(), 1);
        let fire = timers.observe_fire(t.id());
        assert_eq!(fire, ScopedTimerFire::Run { label: "deadline" });
        assert!(fire.should_run());
        assert!(timers.is_empty(), "fire removes the entry");
        assert_eq!(timers.ignored_late(), 0);
    }

    #[test]
    fn cancel_then_late_fire_is_ignored_and_counted() {
        let mut timers = ScopedTimerSet::with_capacity(2);
        let t = timers.arm("deadline").expect("arm");
        assert!(timers.cancel(t.id()), "first cancel tombstones");
        assert!(!timers.cancel(t.id()), "second cancel is a no-op");
        let fire = timers.observe_fire(t.id());
        assert_eq!(fire, ScopedTimerFire::IgnoredLate { label: "deadline" });
        assert!(!fire.should_run(), "ignored timer must not run user work");
        assert_eq!(timers.ignored_late(), 1, "ignored late fire is counted");
        assert!(timers.is_empty());
    }

    #[test]
    fn unknown_ticket_does_not_run() {
        let mut timers = ScopedTimerSet::with_capacity(1);
        let t = timers.arm("deadline").expect("arm");
        let _ = timers.observe_fire(t.id());
        // A second fire for the same (already-observed) ticket is Unknown.
        assert_eq!(timers.observe_fire(t.id()), ScopedTimerFire::Unknown);
    }

    #[test]
    fn cancel_all_tombstones_every_live_timer() {
        let mut timers = ScopedTimerSet::with_capacity(4);
        let a = timers.arm("a").expect("a");
        let b = timers.arm("b").expect("b");
        assert!(timers.cancel(a.id()));
        // cancel_all tombstones the remaining live one (b), not the
        // already-tombstoned a.
        assert_eq!(timers.cancel_all(), 1);
        assert_eq!(
            timers.observe_fire(a.id()),
            ScopedTimerFire::IgnoredLate { label: "a" }
        );
        assert_eq!(
            timers.observe_fire(b.id()),
            ScopedTimerFire::IgnoredLate { label: "b" }
        );
        assert_eq!(timers.ignored_late(), 2);
    }

    #[test]
    fn arm_past_cap_is_refused() {
        let mut timers = ScopedTimerSet::with_capacity(1);
        let _a = timers.arm("a").expect("a");
        match timers.arm("b") {
            Err(ScopedTimerArmError::Full { cap }) => assert_eq!(cap, 1),
            other => panic!("expected Full, got {other:?}"),
        }
    }

    #[test]
    fn reused_request_does_not_reuse_timer_id() {
        let mut timers = ScopedTimerSet::with_capacity(2);
        let a = timers.arm("a").expect("a");
        let _ = timers.observe_fire(a.id());
        let b = timers.arm("a").expect("b");
        assert_ne!(a.id(), b.id(), "a fresh arm gets a fresh id");
    }
}
