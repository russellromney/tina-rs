//! Extension smoke crate: a **custom service policy** built on the
//! public [`ServicePolicy`] seam.
//!
//! The policy is a per-key fixed-window rate limiter keyed by an
//! external natural key (a tenant id). It exists to prove the contract a
//! good Tina policy keeps:
//!
//! - **Returns typed decisions; never acts.** [`PerTenantWindow::decide`]
//!   returns an [`AdmissionDecision`]. It never sends a message, spawns
//!   work, sleeps, retries, or hides a queue. A `RateLimited { retry_after
//!   }` decision is advice; the caller owns the wait.
//! - **Replayable.** `decide` is a pure function of
//!   `(config, now, key history)`. It never reads wall-clock time — the
//!   caller passes `now` (from `ctx.now()` live, or the simulator on
//!   replay). The same inputs produce byte-identical decisions, proven by
//!   [`run`].
//! - **Bounded.** Per-key state lives in a fixed-capacity slot table, not
//!   a growing map. A new key when the table is full is a typed `Full`
//!   rejection, not silent eviction.
//! - **Honest reports.** [`PerTenantWindow::report`] reflects real
//!   accumulated state.

use std::time::{Duration, Instant};

use tina::capacity::CapacityMode;
use tina_runtime::{AdmissionDecision, AdmissionReport, ServicePolicy, SurfaceName};

struct Slot {
    key: String,
    window_start: Instant,
    count: u32,
}

/// Per-tenant fixed-window admission policy.
///
/// Each tenant may be admitted up to `limit` times per `window`. Keys are
/// chosen externally (tenant ids), so the table is fixed-capacity and a
/// fresh tenant when full is rejected, never evicted silently.
pub struct PerTenantWindow {
    surface: SurfaceName,
    limit: u32,
    window: Duration,
    slots: Vec<Option<Slot>>,
    high_water: usize,
    full_count: u64,
    rate_limited_count: u64,
    admitted_count: u64,
}

impl PerTenantWindow {
    /// Build a policy that admits `limit` requests per `window` per key,
    /// across at most `max_keys` distinct keys.
    pub fn new(
        surface: impl Into<SurfaceName>,
        max_keys: usize,
        limit: u32,
        window: Duration,
    ) -> Self {
        let mut slots = Vec::with_capacity(max_keys);
        slots.resize_with(max_keys, || None);
        Self {
            surface: surface.into(),
            limit,
            window,
            slots,
            high_water: 0,
            full_count: 0,
            rate_limited_count: 0,
            admitted_count: 0,
        }
    }

    /// Number of distinct keys currently tracked.
    pub fn live_keys(&self) -> usize {
        self.slots.iter().filter(|s| s.is_some()).count()
    }

    /// Cumulative admissions across the policy lifetime.
    pub fn admitted_count(&self) -> u64 {
        self.admitted_count
    }

    fn find(&mut self, key: &str) -> Option<usize> {
        self.slots
            .iter()
            .position(|s| s.as_ref().is_some_and(|slot| slot.key == key))
    }

    fn free_slot(&mut self) -> Option<usize> {
        self.slots.iter().position(|s| s.is_none())
    }
}

impl ServicePolicy for PerTenantWindow {
    type Key = str;
    type Permit = ();

    fn decide(&mut self, key: &str, now: Instant) -> AdmissionDecision<()> {
        if let Some(idx) = self.find(key) {
            let slot = self.slots[idx].as_mut().expect("slot present");
            let elapsed = now.saturating_duration_since(slot.window_start);
            if elapsed >= self.window {
                // New window: reset and admit.
                slot.window_start = now;
                slot.count = 1;
                self.admitted_count += 1;
                return AdmissionDecision::Admitted(());
            }
            if slot.count < self.limit {
                slot.count += 1;
                self.admitted_count += 1;
                return AdmissionDecision::Admitted(());
            }
            // Window full for this key. retry_after is deterministic.
            let retry_after = self.window - elapsed;
            self.rate_limited_count += 1;
            return AdmissionDecision::RateLimited {
                retry_after,
                report: self.report(),
            };
        }

        // New key.
        match self.free_slot() {
            Some(idx) => {
                self.slots[idx] = Some(Slot {
                    key: key.to_string(),
                    window_start: now,
                    count: 1,
                });
                self.high_water = self.high_water.max(self.live_keys());
                self.admitted_count += 1;
                AdmissionDecision::Admitted(())
            }
            None => {
                // Key table full: typed rejection, no eviction.
                self.full_count += 1;
                AdmissionDecision::Full(self.report())
            }
        }
    }

    fn report(&self) -> AdmissionReport {
        AdmissionReport {
            surface: self.surface.clone(),
            mode: CapacityMode::Fixed,
            capacity: self.slots.len(),
            current: self.live_keys(),
            high_water: self.high_water,
            full_count: self.full_count,
            rate_limited_count: self.rate_limited_count,
            wait_count: 0,
            degrade_count: 0,
            closed_count: 0,
            timed_out_count: 0,
            evicted_count: 0,
        }
    }
}

/// A single decision in the scripted load, as a stable label.
fn label(decision: &AdmissionDecision<()>) -> &'static str {
    match decision {
        AdmissionDecision::Admitted(()) => "admit",
        AdmissionDecision::RateLimited { .. } => "rate_limited",
        AdmissionDecision::Full(_) => "full",
        AdmissionDecision::Wait { .. } => "wait",
        AdmissionDecision::Degrade { .. } => "degrade",
        AdmissionDecision::Closed(_) => "closed",
        AdmissionDecision::TimedOut(_) => "timed_out",
    }
}

/// What the smoke run observed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Report {
    /// Decision labels in order for the scripted load.
    pub decisions: Vec<&'static str>,
    /// Re-running the same `(key, now)` script on a fresh policy yields
    /// the exact same decisions — replayable.
    pub replayed_identical: bool,
    /// Times the policy rate-limited a key.
    pub rate_limited: u64,
    /// Times the key table rejected a new tenant as `Full`.
    pub table_full: u64,
}

/// Drive a deterministic scripted load and prove it replays identically.
///
/// Time is supplied, never read from the wall clock: the script is a list
/// of `(tenant, offset)` pairs, and every run uses the same `base` instant
/// plus those offsets, so two runs see byte-identical logical time.
pub fn run() -> Report {
    // limit=2 per 1s window, table holds 2 tenants.
    let script: &[(&str, u64)] = &[
        ("acme", 0),    // admit (new key)
        ("acme", 100),  // admit (2nd in window)
        ("acme", 200),  // rate_limited (window full)
        ("globex", 0),  // admit (new key)
        ("initech", 0), // full (table holds only 2 tenants)
        ("acme", 1100), // admit (window rolled over at 1s)
    ];

    let drive = |base: Instant| -> Vec<&'static str> {
        let mut policy = PerTenantWindow::new("ext.per_tenant", 2, 2, Duration::from_secs(1));
        script
            .iter()
            .map(|(tenant, off)| {
                let now = base + Duration::from_millis(*off);
                label(&policy.decide(tenant, now))
            })
            .collect()
    };

    let base = Instant::now();
    let first = drive(base);
    let second = drive(base);

    // Also keep one policy to read its accumulated report.
    let mut counted = PerTenantWindow::new("ext.per_tenant", 2, 2, Duration::from_secs(1));
    for (tenant, off) in script {
        let _ = counted.decide(tenant, base + Duration::from_millis(*off));
    }
    let report = counted.report();

    Report {
        replayed_identical: first == second,
        decisions: first,
        rate_limited: report.rate_limited_count,
        table_full: report.full_count,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn custom_policy_decisions_are_typed_and_replayable() {
        let report = run();
        assert_eq!(
            report.decisions,
            vec!["admit", "admit", "rate_limited", "admit", "full", "admit"]
        );
        assert!(
            report.replayed_identical,
            "same (key, now) script must replay to identical decisions"
        );
        assert_eq!(report.rate_limited, 1);
        assert_eq!(report.table_full, 1);
    }
}
