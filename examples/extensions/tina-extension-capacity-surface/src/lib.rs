//! Extension smoke crate: a **custom capacity surface** that joins a
//! normal [`CapacitySummary`].
//!
//! The hook here is not a trait. It is data: an extension owns some
//! bounded structure, and it renders that structure as a
//! [`CapacitySurfaceReport`] using the same public constructor every
//! runtime surface uses ([`CapacitySurfaceReport::count`] /
//! [`CapacitySurfaceReport::weighted`]). The report then joins a
//! [`CapacitySummary`] through the public [`CapacitySummary::push`]
//! entry point and shows up in discovery, `surface(name)` lookups, and
//! `any_full()` exactly like a runtime surface.
//!
//! No private runtime state is touched. An extension may *observe and
//! report* capacity; it may not mutate runtime queues.
//!
//! This crate proves owned [`CapacitySurfaceReport`] data is enough —
//! which is why Tina does **not** ship a `CapacitySurface` trait.

use tina::capacity::{CapacityMode, CapacitySurfaceReport};
use tina_runtime::{BoundedEventSink, CapacitySummary, DropPolicy};

/// A small bounded structure an extension might own: a ring of the last
/// `cap` samples. Pushing into a full ring drops the new sample and
/// counts it as a `Full` rejection, so the surface reports overload the
/// same way a bounded mailbox does.
pub struct RecentSamples {
    name: String,
    cap: usize,
    buf: std::collections::VecDeque<u64>,
    high_water: usize,
    full_count: u64,
}

impl RecentSamples {
    /// Build a ring with `cap` slots.
    pub fn new(name: impl Into<String>, cap: usize) -> Self {
        Self {
            name: name.into(),
            cap,
            buf: std::collections::VecDeque::with_capacity(cap),
            high_water: 0,
            full_count: 0,
        }
    }

    /// Record one sample. Returns whether it was accepted. A full ring
    /// rejects (drop-newest), counting the rejection — never grows.
    pub fn record(&mut self, sample: u64) -> bool {
        if self.buf.len() >= self.cap {
            self.full_count += 1;
            return false;
        }
        self.buf.push_back(sample);
        self.high_water = self.high_water.max(self.buf.len());
        true
    }

    /// Drain consumes the ring (e.g. a flush); does not change the cap.
    pub fn drain(&mut self) -> Vec<u64> {
        self.buf.drain(..).collect()
    }

    /// Render as a [`CapacitySurfaceReport`] — the public capacity hook.
    pub fn surface_report(&self) -> CapacitySurfaceReport {
        CapacitySurfaceReport::count(
            self.name.clone(),
            CapacityMode::Fixed,
            self.cap,
            self.buf.len(),
            self.high_water,
            self.full_count,
        )
    }
}

/// What the smoke run observed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Report {
    /// The custom surface appears in the joined summary.
    pub custom_in_summary: bool,
    /// A runtime-owned surface (a [`BoundedEventSink`]) appears too.
    pub runtime_in_summary: bool,
    /// The custom surface reported `Full` after overflow.
    pub custom_full_count: u64,
    /// The summary's aggregate `any_full()` saw the custom overflow.
    pub summary_any_full: bool,
    /// Number of surfaces in the joined summary.
    pub surfaces: usize,
}

/// Build a custom surface, overflow it, join it with a runtime surface
/// in one [`CapacitySummary`], and read the summary back.
pub fn run() -> Report {
    // Custom extension-owned surface: cap 4, push 6 → 2 dropped.
    let mut samples = RecentSamples::new("ext.recent_samples", 4);
    for i in 0..6 {
        samples.record(i);
    }

    // A runtime-owned surface that already speaks the capacity
    // vocabulary, to prove they sit side by side.
    let sink: BoundedEventSink<u64> = BoundedEventSink::new("rt.events", 8, DropPolicy::DropNewest);
    sink.push(1);
    sink.push(2);

    let mut summary = CapacitySummary::new();
    summary
        .push(samples.surface_report())
        .expect("custom surface name is valid and unique");
    summary
        .push(sink.surface_report(CapacityMode::Fixed))
        .expect("runtime surface name is valid and unique");

    let custom = summary.surface("ext.recent_samples").report().ok().cloned();
    let runtime = summary.surface("rt.events").report().ok().cloned();

    Report {
        custom_in_summary: custom.is_some(),
        runtime_in_summary: runtime.is_some(),
        custom_full_count: custom.map(|r| r.full_count).unwrap_or(0),
        summary_any_full: summary.any_full(),
        surfaces: summary.len(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn custom_surface_joins_summary() {
        let report = run();
        assert!(report.custom_in_summary, "custom surface must join summary");
        assert!(report.runtime_in_summary, "runtime surface must join too");
        assert_eq!(report.surfaces, 2);
        assert_eq!(report.custom_full_count, 2, "two samples overflowed cap=4");
        assert!(
            report.summary_any_full,
            "summary any_full() must reflect the custom overflow"
        );
    }
}
