//! Load/soak driver.
//!
//! N worker threads each call a user op fn until either `op_count` ops
//! have been driven or `duration` elapses, whichever comes first.
//!
//! The op returns an [`OpOutcome`] (`Ok`, `Err{kind}`, or `Timeout`),
//! never panics. Latency is sampled per op around the user closure.
//! No `sleep_and_hope`: the harness owns the deadline, the op owns the
//! work.
//!
//! The optional `leak_check` snapshot fires after the run finishes; if
//! it returns false, [`LoadReport::leak_clean`] is `false` and the
//! caller can fail the test with a meaningful capacity reason.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Barrier};
use std::thread;
use std::time::{Duration, Instant};

use tina_runtime::{ServicePressureReport, ServiceSurfaceState};

/// Per-op observation. Latency is included so the user can see
/// timeout-vs-success-vs-error distributions without log scraping.
#[derive(Debug, Clone)]
pub enum OpOutcome {
    Ok,
    Err { kind: &'static str },
    Timeout,
}

/// How the load run should stop. Set both and the first hit wins.
#[derive(Debug, Clone, Copy)]
pub struct LoadStop {
    /// Maximum ops to drive across all workers. `None` = no cap.
    pub op_count: Option<u64>,
    /// Maximum wall-clock time. `None` = no cap.
    pub duration: Option<Duration>,
}

impl LoadStop {
    pub fn ops(n: u64) -> Self {
        Self {
            op_count: Some(n),
            duration: None,
        }
    }

    pub fn for_duration(d: Duration) -> Self {
        Self {
            op_count: None,
            duration: Some(d),
        }
    }
}

/// One configured load run.
pub struct LoadRun {
    /// Concurrency. Must be > 0.
    pub workers: usize,
    /// Stop condition.
    pub stop: LoadStop,
    /// Stable label for the run, included in panics and report.
    pub label: &'static str,
}

/// Phase-121 name for a configured load run. Alias kept so older
/// specimens using `LoadRun` keep compiling.
pub type LoadProfile = LoadRun;

/// One bounded surface observed at the end of a load run.
///
/// The fields mirror the service pressure convention: no hidden
/// buffers, no retries smuggled into the harness, just high-water,
/// final-current, full counts, and an explicit leak verdict.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SurfacePlateau {
    pub name: String,
    pub kind: &'static str,
    pub capacity: Option<usize>,
    pub high_water: usize,
    pub final_current: usize,
    pub full: u64,
    pub max_messages: Option<usize>,
    pub current_messages: usize,
    pub high_water_messages: usize,
    pub max_weight: Option<usize>,
    pub current_weight: Option<usize>,
    pub high_water_weight: Option<usize>,
    pub shared_max_weight: Option<usize>,
    pub shared_current_weight: Option<usize>,
    pub shared_high_water_weight: Option<usize>,
    pub leak_clean: bool,
}

/// A surface the specimen knows exists but cannot measure numerically.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnavailableSurface {
    pub name: String,
    pub kind: &'static str,
    pub reason: String,
}

impl SurfacePlateau {
    /// Project every measured surface in a service pressure report.
    /// Use [`UnavailableSurface::from_service_pressure`] to carry the
    /// explicit unavailable half of the same report.
    pub fn from_service_pressure(report: &ServicePressureReport) -> Vec<Self> {
        report
            .surfaces
            .iter()
            .filter_map(|surface| match &surface.state {
                ServiceSurfaceState::Measured(capacity) => {
                    let high_water = [
                        capacity.high_water_messages,
                        capacity.high_water_weight.unwrap_or(0),
                        capacity.shared_high_water_weight.unwrap_or(0),
                    ]
                    .into_iter()
                    .max()
                    .unwrap_or(0);
                    let final_current = [
                        capacity.current_messages,
                        capacity.current_weight.unwrap_or(0),
                        capacity.shared_current_weight.unwrap_or(0),
                    ]
                    .into_iter()
                    .max()
                    .unwrap_or(0);
                    let capacity_limit = [
                        capacity.max_messages,
                        capacity.max_weight,
                        capacity.shared_max_weight,
                    ]
                    .into_iter()
                    .flatten()
                    .max();
                    let full = capacity
                        .full_count
                        .saturating_add(capacity.weight_full_count)
                        .saturating_add(capacity.shared_weight_full_count);
                    let leak_clean = capacity.current_messages == 0
                        && capacity.current_weight.unwrap_or(0) == 0
                        && capacity.shared_current_weight.unwrap_or(0) == 0;
                    Some(Self {
                        name: surface.name.clone(),
                        kind: surface.kind,
                        capacity: capacity_limit,
                        high_water,
                        final_current,
                        full,
                        max_messages: capacity.max_messages,
                        current_messages: capacity.current_messages,
                        high_water_messages: capacity.high_water_messages,
                        max_weight: capacity.max_weight,
                        current_weight: capacity.current_weight,
                        high_water_weight: capacity.high_water_weight,
                        shared_max_weight: capacity.shared_max_weight,
                        shared_current_weight: capacity.shared_current_weight,
                        shared_high_water_weight: capacity.shared_high_water_weight,
                        leak_clean,
                    })
                }
                ServiceSurfaceState::Unavailable { .. } => None,
            })
            .collect()
    }

    /// One-line key=value shape for specimen output.
    pub fn summary_line(&self) -> String {
        let mut line = format!(
            "surface name={} kind={} capacity={} high_water={} final_current={} full={}",
            report_value(&self.name),
            report_value(self.kind),
            option_usize(self.capacity),
            self.high_water,
            self.final_current,
            self.full,
        );
        line.push_str(&format!(
            " max_messages={} current_messages={} high_water_messages={}",
            option_usize(self.max_messages),
            self.current_messages,
            self.high_water_messages,
        ));
        line.push_str(&format!(
            " max_weight={} current_weight={} high_water_weight={}",
            option_usize(self.max_weight),
            option_usize(self.current_weight),
            option_usize(self.high_water_weight),
        ));
        line.push_str(&format!(
            " shared_max_weight={} shared_current_weight={} shared_high_water_weight={} leak_clean={}",
            option_usize(self.shared_max_weight),
            option_usize(self.shared_current_weight),
            option_usize(self.shared_high_water_weight),
            self.leak_clean,
        ));
        line
    }
}

impl UnavailableSurface {
    /// Project every explicitly unavailable surface in a service pressure
    /// report. This keeps "we cannot measure that" user-visible.
    pub fn from_service_pressure(report: &ServicePressureReport) -> Vec<Self> {
        report
            .surfaces
            .iter()
            .filter_map(|surface| match &surface.state {
                ServiceSurfaceState::Measured(_) => None,
                ServiceSurfaceState::Unavailable { reason } => Some(Self {
                    name: surface.name.clone(),
                    kind: surface.kind,
                    reason: reason.clone(),
                }),
            })
            .collect()
    }

    /// One-line key=value shape for specimen output.
    pub fn summary_line(&self) -> String {
        format!(
            "surface name={} kind={} state=unavailable reason={}",
            report_value(&self.name),
            report_value(self.kind),
            report_value(&self.reason),
        )
    }
}

/// End-of-run observations supplied by the specimen.
///
/// `leak_checked` records whether a leak check actually ran. `leak_clean`
/// is only meaningful when `leak_checked` is true; an unchecked run must
/// not be read as clean.
/// Default is "not checked", not "clean": every field defaults to its empty
/// value, so a missing leak check never reads as a passing leak result.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct LoadObservation {
    /// Whether the specimen actually ran a leak check.
    pub leak_checked: bool,
    pub leak_clean: bool,
    pub surface_plateaus: Vec<SurfacePlateau>,
    pub unavailable_surfaces: Vec<UnavailableSurface>,
    pub trace_hash: Option<u64>,
    pub late: u64,
}

/// Summary of one [`LoadRun`].
///
/// `leak_checked` records whether a leak check actually ran; `leak_clean`
/// is only meaningful when it did. An unchecked run renders as
/// `leak=unchecked`, never `leak_clean=true`. Latency is reported as
/// min/p50/p99/max in nanoseconds for machine comparison and microseconds
/// for compact human summaries.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoadReport {
    pub label: &'static str,
    pub workers: usize,
    pub ops_attempted: u64,
    pub ops_ok: u64,
    pub ops_err: u64,
    pub ops_timeout: u64,
    /// Per-kind error tally (sorted). Kept on the report root so
    /// existing call sites keep compiling; also surfaced via
    /// [`PressureSummary::by_kind`].
    pub err_kinds: Vec<(String, u64)>,
    pub latency_min_ns: u64,
    pub latency_p50_ns: u64,
    pub latency_p90_ns: u64,
    pub latency_p99_ns: u64,
    pub latency_max_ns: u64,
    pub latency_min_us: u64,
    pub latency_p50_us: u64,
    pub latency_p90_us: u64,
    pub latency_p99_us: u64,
    pub latency_max_us: u64,
    pub elapsed_ms: u64,
    /// Whether a leak check actually ran. When `false`, `leak_clean` is not
    /// a verified result and must not be read as one.
    pub leak_checked: bool,
    pub leak_clean: bool,
    pub surface_plateaus: Vec<SurfacePlateau>,
    pub unavailable_surfaces: Vec<UnavailableSurface>,
    pub trace_hash: Option<u64>,
    pub late: u64,
    /// Typed pressure summary: rate, burst length, first-error
    /// position, per-kind breakdown. Lets a specimen assert
    /// "pressure stayed under N per mille" or "no burst longer than
    /// K consecutive errors" without parsing the summary line.
    pub pressure: PressureSummary,
}

/// Phase-121 name for the report returned by [`LoadProfile`]. Alias kept
/// alongside [`LoadReport`] for existing call sites.
pub type LoadRunReport = LoadReport;

/// Pressure summary for one [`LoadReport`].
///
/// "Pressure" means any non-ok outcome (err or timeout). Counts are
/// per-run; `max_consecutive` is per-worker (longest streak observed
/// by any single worker) because workers run concurrently and there
/// is no defensible global order across them.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct PressureSummary {
    /// Total non-ok outcomes (err + timeout).
    pub total: u64,
    /// Pressure rate per mille (out of 1000). `0` when `ops_attempted == 0`.
    pub rate_per_mille: u64,
    /// Longest streak of consecutive non-ok outcomes observed by any
    /// single worker. Highlights bursts (e.g., a brief outage) the
    /// rate alone would smear.
    pub max_consecutive: u64,
    /// Position of the first non-ok outcome within the run's global dispatch
    /// order (op index, 0-based, across all workers). `None` when there was no
    /// pressure.
    pub first_error_op_index: Option<u64>,
    /// Per-kind tally, sorted. Same data as
    /// [`LoadReport::err_kinds`], duplicated here so the pressure
    /// summary is self-contained.
    pub by_kind: Vec<(String, u64)>,
}

impl PressureSummary {
    /// One-line summary, key=value, suitable for test output and grep.
    pub fn summary_line(&self) -> String {
        let mut by_kind = String::new();
        for (i, (k, v)) in self.by_kind.iter().enumerate() {
            if i > 0 {
                by_kind.push(',');
            }
            by_kind.push_str(&format!("{}:{v}", report_value(k)));
        }
        format!(
            "pressure total={} rate_per_mille={} max_consecutive={} first_err_op={} by_kind=[{}]",
            self.total,
            self.rate_per_mille,
            self.max_consecutive,
            self.first_error_op_index
                .map(|i| i.to_string())
                .unwrap_or_else(|| "none".to_string()),
            by_kind,
        )
    }
}

impl LoadReport {
    /// One-line summary, key=value, suitable for test output and grep.
    pub fn summary_line(&self) -> String {
        // Render the honest leak state: `leak=unchecked` when no check ran,
        // otherwise `leak_clean=<bool>`.
        let leak = if self.leak_checked {
            format!("leak_clean={}", self.leak_clean)
        } else {
            "leak=unchecked".to_string()
        };
        format!(
            "load label={} workers={} ops={} ok={} err={} timeout={} \
             min_us={} p50_us={} p90_us={} p99_us={} max_us={} elapsed_ms={} {} late={} trace_hash={} surfaces={} unavailable_surfaces={} {}",
            report_value(self.label),
            self.workers,
            self.ops_attempted,
            self.ops_ok,
            self.ops_err,
            self.ops_timeout,
            self.latency_min_us,
            self.latency_p50_us,
            self.latency_p90_us,
            self.latency_p99_us,
            self.latency_max_us,
            self.elapsed_ms,
            leak,
            self.late,
            self.trace_hash
                .map(|hash| hash.to_string())
                .unwrap_or_else(|| "none".to_string()),
            self.surface_plateaus.len(),
            self.unavailable_surfaces.len(),
            self.pressure.summary_line(),
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoadAssertionFailure {
    pub claim: &'static str,
    pub observed: String,
}

impl std::fmt::Display for LoadAssertionFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} failed; observed {}", self.claim, self.observed)
    }
}

impl std::error::Error for LoadAssertionFailure {}

pub fn cold_work_made_progress(report: &LoadReport) -> Result<(), LoadAssertionFailure> {
    if report.ops_ok > 0 {
        return Ok(());
    }
    Err(LoadAssertionFailure {
        claim: "cold work made progress",
        observed: report.summary_line(),
    })
}

pub fn timer_kept_firing(report: &LoadReport) -> Result<(), LoadAssertionFailure> {
    if report.ops_attempted > 0 && report.ops_timeout == 0 {
        return Ok(());
    }
    Err(LoadAssertionFailure {
        claim: "timer kept firing",
        observed: report.summary_line(),
    })
}

pub fn surface_plateaued_cleanly(
    report: &LoadReport,
    surface_name: &str,
) -> Result<(), LoadAssertionFailure> {
    let Some(surface) = report
        .surface_plateaus
        .iter()
        .find(|surface| surface.name == surface_name)
    else {
        return Err(LoadAssertionFailure {
            claim: "surface plateaued cleanly",
            observed: format!(
                "missing surface {surface_name:?}; report {}",
                report.summary_line()
            ),
        });
    };
    if surface.full == 0 && surface.final_current <= surface.high_water && surface.leak_clean {
        return Ok(());
    }
    Err(LoadAssertionFailure {
        claim: "surface plateaued cleanly",
        observed: surface.summary_line(),
    })
}

pub fn no_leaked_capacity_at_shutdown(report: &LoadReport) -> Result<(), LoadAssertionFailure> {
    // Fail closed when nothing was checked: an unchecked run cannot prove
    // the absence of a leak.
    if !report.leak_checked {
        return Err(LoadAssertionFailure {
            claim: "no leaked capacity at shutdown",
            observed: format!("leak was never checked; report {}", report.summary_line()),
        });
    }
    if report.leak_clean
        && report
            .surface_plateaus
            .iter()
            .all(|surface| surface.leak_clean)
    {
        return Ok(());
    }
    let observed = report
        .surface_plateaus
        .iter()
        .find(|surface| !surface.leak_clean)
        .map(SurfacePlateau::summary_line)
        .unwrap_or_else(|| report.summary_line());
    Err(LoadAssertionFailure {
        claim: "no leaked capacity at shutdown",
        observed,
    })
}

pub fn assert_cold_work_made_progress(report: &LoadReport) {
    cold_work_made_progress(report).unwrap();
}

pub fn assert_timer_kept_firing(report: &LoadReport) {
    timer_kept_firing(report).unwrap();
}

pub fn assert_surface_plateaued_cleanly(report: &LoadReport, surface_name: &str) {
    surface_plateaued_cleanly(report, surface_name).unwrap();
}

pub fn assert_no_leaked_capacity_at_shutdown(report: &LoadReport) {
    no_leaked_capacity_at_shutdown(report).unwrap();
}

/// Run `op` under load.
///
/// `op` is invoked per worker, per iteration. It must be `Send + Sync`
/// because all worker threads share it via `Arc`. Workers are scoped and join
/// before return, so `op` may borrow caller-owned host state. `leak_check`, if
/// supplied, runs once after all workers join, on the calling thread.
pub fn run<F, L>(run: LoadRun, op: F, leak_check: Option<L>) -> LoadReport
where
    F: Fn(usize) -> OpOutcome + Send + Sync,
    L: FnOnce() -> bool,
{
    run_with_observation(
        run,
        op,
        leak_check.map(|check| {
            move || LoadObservation {
                leak_checked: true,
                leak_clean: check(),
                ..LoadObservation::default()
            }
        }),
    )
}

/// Run `op` under load and collect a typed end-of-run observation.
///
/// Use this when a specimen can report capacity high-water/final-current
/// counts, late results, and trace fingerprints. The harness records what
/// the specimen returns; it does not retry, drain hidden queues, or turn
/// `Full` into success. Worker threads are scoped, so `op` may borrow state
/// owned by the caller.
pub fn run_with_observation<F, O>(run: LoadRun, op: F, observation: Option<O>) -> LoadReport
where
    F: Fn(usize) -> OpOutcome + Send + Sync,
    O: FnOnce() -> LoadObservation,
{
    assert!(run.workers > 0, "LoadRun.workers must be > 0");
    assert!(
        run.stop.op_count.is_some() || run.stop.duration.is_some(),
        "LoadRun.stop needs op_count or duration",
    );

    let op = Arc::new(op);
    let stop_after = run.stop.duration;
    let op_cap = run.stop.op_count.unwrap_or(u64::MAX);
    let ops_dispatched = Arc::new(AtomicU64::new(0));
    let halted = Arc::new(AtomicBool::new(false));
    let start_gate = Arc::new(Barrier::new(run.workers + 1));

    let (combined, elapsed) = thread::scope(|scope| {
        let mut handles = Vec::with_capacity(run.workers);
        for worker_id in 0..run.workers {
            let op = Arc::clone(&op);
            let ops_dispatched = Arc::clone(&ops_dispatched);
            let halted = Arc::clone(&halted);
            let start_gate = Arc::clone(&start_gate);
            let latency_capacity = latency_capacity_for_worker(op_cap, run.workers);
            let handle = scope.spawn(move || -> WorkerObs {
                start_gate.wait();
                let stop_at = stop_after.map(|duration| {
                    tina::Deadline::from_instant(Instant::now(), duration).instant()
                });
                let mut obs = WorkerObs::with_latency_capacity(latency_capacity);
                let mut current_streak: u64 = 0;
                loop {
                    if halted.load(Ordering::Acquire) {
                        break;
                    }
                    if let Some(deadline) = stop_at
                        && Instant::now() >= deadline
                    {
                        halted.store(true, Ordering::Release);
                        break;
                    }
                    let prior = ops_dispatched.fetch_add(1, Ordering::AcqRel);
                    if prior >= op_cap {
                        halted.store(true, Ordering::Release);
                        break;
                    }
                    let t0 = Instant::now();
                    let outcome = op(worker_id);
                    let dt = t0.elapsed();
                    obs.latencies_ns.push(duration_to_ns(dt));
                    let pressure = !matches!(outcome, OpOutcome::Ok);
                    match outcome {
                        OpOutcome::Ok => obs.ok += 1,
                        OpOutcome::Err { kind } => {
                            obs.err += 1;
                            *obs.err_kinds.entry(kind.to_string()).or_insert(0) += 1;
                        }
                        OpOutcome::Timeout => obs.timeout += 1,
                    }
                    if pressure {
                        if obs.first_error_global_index.is_none() {
                            obs.first_error_global_index = Some(prior);
                        }
                        current_streak += 1;
                        if current_streak > obs.max_consecutive {
                            obs.max_consecutive = current_streak;
                        }
                    } else {
                        current_streak = 0;
                    }
                }
                obs
            });
            handles.push(handle);
        }

        let started = Instant::now();
        start_gate.wait();
        let mut combined = WorkerObs::default();
        for handle in handles {
            let obs = handle.join().expect("worker join");
            combined.merge(obs);
        }
        (combined, started.elapsed())
    });

    // No observation closure means nothing was checked — including no leak
    // check. The default is unchecked, not clean.
    let observation = observation.map(|f| f()).unwrap_or_default();

    let mut latencies = combined.latencies_ns;
    latencies.sort_unstable();
    let min_ns = latencies.first().copied().unwrap_or(0);
    let max_ns = latencies.last().copied().unwrap_or(0);
    let p50_ns = percentile(&latencies, 50);
    let p90_ns = percentile(&latencies, 90);
    let p99_ns = percentile(&latencies, 99);

    let mut err_kinds: Vec<(String, u64)> = combined.err_kinds.into_iter().collect();
    err_kinds.sort();

    let ops_attempted = combined.ok + combined.err + combined.timeout;
    let pressure_total = combined.err + combined.timeout;
    let rate_per_mille = pressure_total
        .saturating_mul(1000)
        .checked_div(ops_attempted)
        .unwrap_or(0);

    let pressure = PressureSummary {
        total: pressure_total,
        rate_per_mille,
        max_consecutive: combined.max_consecutive,
        first_error_op_index: combined.first_error_global_index,
        by_kind: err_kinds.clone(),
    };

    LoadReport {
        label: run.label,
        workers: run.workers,
        ops_attempted,
        ops_ok: combined.ok,
        ops_err: combined.err,
        ops_timeout: combined.timeout,
        err_kinds,
        latency_min_ns: min_ns,
        latency_p50_ns: p50_ns,
        latency_p90_ns: p90_ns,
        latency_p99_ns: p99_ns,
        latency_max_ns: max_ns,
        latency_min_us: ns_to_us(min_ns),
        latency_p50_us: ns_to_us(p50_ns),
        latency_p90_us: ns_to_us(p90_ns),
        latency_p99_us: ns_to_us(p99_ns),
        latency_max_us: ns_to_us(max_ns),
        elapsed_ms: elapsed.as_millis() as u64,
        leak_checked: observation.leak_checked,
        leak_clean: observation.leak_clean
            && observation
                .surface_plateaus
                .iter()
                .all(|surface| surface.leak_clean),
        surface_plateaus: observation.surface_plateaus,
        unavailable_surfaces: observation.unavailable_surfaces,
        trace_hash: observation.trace_hash,
        late: observation.late,
        pressure,
    }
}

#[derive(Default)]
struct WorkerObs {
    ok: u64,
    err: u64,
    timeout: u64,
    err_kinds: std::collections::BTreeMap<String, u64>,
    latencies_ns: Vec<u64>,
    max_consecutive: u64,
    first_error_global_index: Option<u64>,
}

impl WorkerObs {
    fn with_latency_capacity(capacity: usize) -> Self {
        Self {
            latencies_ns: Vec::with_capacity(capacity),
            ..Self::default()
        }
    }

    fn merge(&mut self, other: WorkerObs) {
        self.ok += other.ok;
        self.err += other.err;
        self.timeout += other.timeout;
        for (k, v) in other.err_kinds {
            *self.err_kinds.entry(k).or_insert(0) += v;
        }
        self.latencies_ns.extend(other.latencies_ns);
        if other.max_consecutive > self.max_consecutive {
            self.max_consecutive = other.max_consecutive;
        }
        self.first_error_global_index = match (
            self.first_error_global_index,
            other.first_error_global_index,
        ) {
            (None, x) | (x, None) => x,
            (Some(a), Some(b)) => Some(a.min(b)),
        };
    }
}

fn latency_capacity_for_worker(op_cap: u64, workers: usize) -> usize {
    if op_cap == u64::MAX {
        return 0;
    }
    let workers = workers.max(1) as u64;
    op_cap.div_ceil(workers).saturating_add(1) as usize
}

fn duration_to_ns(d: Duration) -> u64 {
    d.as_nanos().min(u128::from(u64::MAX)) as u64
}

fn ns_to_us(ns: u64) -> u64 {
    ns / 1_000
}

fn percentile(sorted: &[u64], pct: u32) -> u64 {
    if sorted.is_empty() {
        return 0;
    }
    // Nearest-rank, simple and good enough for proof output.
    let rank = (pct as usize * sorted.len()).div_ceil(100);
    let idx = rank.saturating_sub(1).min(sorted.len() - 1);
    sorted[idx]
}

fn option_usize(value: Option<usize>) -> String {
    value
        .map(|value| value.to_string())
        .unwrap_or_else(|| "none".to_string())
}

fn report_value(value: &str) -> String {
    if value.is_empty()
        || value.chars().any(|c| {
            c.is_whitespace()
                || c.is_control()
                || matches!(c, '=' | '[' | ']' | ',' | ':' | '"' | '\\')
        })
    {
        format!("{value:?}")
    } else {
        value.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicU64;
    use tina::capacity::{CapacityMode, CapacitySurfaceReport};

    #[test]
    fn ops_cap_bounds_work_count() {
        let counter = Arc::new(AtomicU64::new(0));
        let inner = Arc::clone(&counter);
        let report = run(
            LoadRun {
                workers: 4,
                stop: LoadStop::ops(100),
                label: "ops_cap",
            },
            move |_| {
                inner.fetch_add(1, Ordering::Relaxed);
                OpOutcome::Ok
            },
            None::<fn() -> bool>,
        );
        assert_eq!(report.ops_attempted, 100, "{report:?}");
        assert_eq!(report.ops_ok, 100);
        assert_eq!(report.ops_err, 0);
        assert_eq!(report.ops_timeout, 0);
        assert!(!report.leak_checked, "no leak check was supplied");
        assert_eq!(counter.load(Ordering::Relaxed), 100);
    }

    #[test]
    fn operation_can_borrow_a_caller_owned_host() {
        let host = String::from("borrowed host");
        let report = run_with_observation(
            LoadRun {
                workers: 2,
                stop: LoadStop::ops(4),
                label: "borrowed_host",
            },
            |_| {
                assert_eq!(host, "borrowed host");
                OpOutcome::Ok
            },
            None::<fn() -> LoadObservation>,
        );
        assert_eq!(report.ops_ok, 4);
    }

    #[test]
    fn maximum_duration_can_be_combined_with_an_operation_cap() {
        let report = run(
            LoadRun {
                workers: 1,
                stop: LoadStop {
                    op_count: Some(1),
                    duration: Some(Duration::MAX),
                },
                label: "maximum_duration",
            },
            |_| OpOutcome::Ok,
            None::<fn() -> bool>,
        );
        assert_eq!(report.ops_attempted, 1);
        assert_eq!(report.ops_ok, 1);
    }

    #[test]
    fn duration_cap_terminates_quickly() {
        let report = run(
            LoadRun {
                workers: 2,
                stop: LoadStop::for_duration(Duration::from_millis(50)),
                label: "duration_cap",
            },
            |_| OpOutcome::Ok,
            None::<fn() -> bool>,
        );
        assert!(report.ops_attempted >= 1, "{report:?}");
        assert!(
            report.elapsed_ms < 2_000,
            "duration cap did not stop the load run: {report:?}",
        );
    }

    #[test]
    fn err_kinds_are_collected() {
        // Single worker so the per-op alternation is deterministic.
        // Workers can race for early-exit; using one worker removes
        // that race without weakening what the test proves.
        let counter = Arc::new(AtomicU64::new(0));
        let inner = Arc::clone(&counter);
        let report = run(
            LoadRun {
                workers: 1,
                stop: LoadStop::ops(20),
                label: "errs",
            },
            move |_worker| {
                let n = inner.fetch_add(1, Ordering::Relaxed);
                if n % 2 == 0 {
                    OpOutcome::Err { kind: "boom" }
                } else {
                    OpOutcome::Timeout
                }
            },
            None::<fn() -> bool>,
        );
        assert_eq!(report.ops_attempted, 20);
        assert_eq!(report.ops_err, 10, "{report:?}");
        assert_eq!(report.ops_timeout, 10, "{report:?}");
        assert_eq!(
            report.err_kinds.first().map(|(k, _)| k.as_str()),
            Some("boom")
        );
    }

    #[test]
    fn pressure_summary_quotes_error_kinds_that_would_break_key_value_lines() {
        let report = run(
            LoadRun {
                workers: 1,
                stop: LoadStop::ops(1),
                label: "label with space",
            },
            |_| OpOutcome::Err {
                kind: "mailbox full",
            },
            None::<fn() -> bool>,
        );
        let line = report.summary_line();
        assert!(line.contains("label=\"label with space\""), "{line}");
        assert!(line.contains("by_kind=[\"mailbox full\":1]"), "{line}");
    }

    #[test]
    fn leak_check_is_reported() {
        let report = run(
            LoadRun {
                workers: 1,
                stop: LoadStop::ops(1),
                label: "leak",
            },
            |_| OpOutcome::Ok,
            Some(|| false),
        );
        assert!(
            report.leak_checked,
            "supplying a leak check marks it checked"
        );
        assert!(!report.leak_clean);
        assert!(report.summary_line().contains("leak_clean=false"));
    }

    #[test]
    fn unchecked_run_reports_leak_as_unchecked_not_clean() {
        // A run with no leak check must not masquerade as leak-clean: a
        // reader has to be able to tell "verified no leak" from "never
        // looked".
        let report = run(
            LoadRun {
                workers: 1,
                stop: LoadStop::ops(1),
                label: "unchecked",
            },
            |_| OpOutcome::Ok,
            None::<fn() -> bool>,
        );
        assert!(!report.leak_checked, "no leak check means unchecked");
        assert!(
            report.summary_line().contains("leak=unchecked"),
            "summary must render unchecked, got: {}",
            report.summary_line()
        );
        assert!(
            !report.summary_line().contains("leak_clean=true"),
            "an unchecked run must not print leak_clean=true: {}",
            report.summary_line()
        );
    }

    #[test]
    fn load_observation_reports_surfaces_late_and_trace_hash() {
        let report = run_with_observation(
            LoadRun {
                workers: 1,
                stop: LoadStop::ops(2),
                label: "observed",
            },
            |_| OpOutcome::Ok,
            Some(|| LoadObservation {
                leak_checked: true,
                leak_clean: true,
                surface_plateaus: vec![SurfacePlateau {
                    name: "svc.mailbox".to_string(),
                    kind: "mailbox",
                    capacity: Some(4),
                    high_water: 4,
                    final_current: 0,
                    full: 1,
                    max_messages: Some(4),
                    current_messages: 0,
                    high_water_messages: 4,
                    max_weight: None,
                    current_weight: None,
                    high_water_weight: None,
                    shared_max_weight: None,
                    shared_current_weight: None,
                    shared_high_water_weight: None,
                    leak_clean: true,
                }],
                unavailable_surfaces: vec![UnavailableSurface {
                    name: "svc.protocol".to_string(),
                    kind: "protocol",
                    reason: "not exercised".to_string(),
                }],
                trace_hash: Some(42),
                late: 3,
            }),
        );
        assert_eq!(report.ops_ok, 2);
        assert!(report.leak_clean);
        assert_eq!(report.surface_plateaus.len(), 1);
        assert_eq!(report.unavailable_surfaces.len(), 1);
        assert_eq!(report.trace_hash, Some(42));
        assert_eq!(report.late, 3);
        let line = report.summary_line();
        assert!(line.contains("late=3"), "{line}");
        assert!(line.contains("trace_hash=42"), "{line}");
        assert!(line.contains("surfaces=1"), "{line}");
        assert!(line.contains("unavailable_surfaces=1"), "{line}");
    }

    #[test]
    fn surface_plateau_projects_service_pressure_without_hidden_leaks_or_missing_surfaces() {
        let mut pressure = ServicePressureReport::new("svc");
        pressure.add_measured(
            "mailbox",
            CapacitySurfaceReport::count("svc.mailbox", CapacityMode::Fixed, 4, 0, 4, 2)
                .with_shared_scope("body.bytes", 100, 0, 90, 3),
        );
        pressure.add_unavailable("svc.ws", "protocol", "not exercised");

        let plateaus = SurfacePlateau::from_service_pressure(&pressure);
        let unavailable = UnavailableSurface::from_service_pressure(&pressure);
        assert_eq!(plateaus.len(), 1);
        assert_eq!(plateaus[0].name, "svc.mailbox");
        assert_eq!(plateaus[0].capacity, Some(100));
        assert_eq!(plateaus[0].high_water, 90);
        assert_eq!(plateaus[0].high_water_messages, 4);
        assert_eq!(plateaus[0].shared_high_water_weight, Some(90));
        assert_eq!(plateaus[0].final_current, 0);
        assert_eq!(plateaus[0].full, 5);
        assert!(plateaus[0].leak_clean);
        assert_eq!(unavailable.len(), 1);
        assert_eq!(unavailable[0].name, "svc.ws");
        assert_eq!(unavailable[0].reason, "not exercised");
    }

    #[test]
    fn surface_lines_quote_sloppy_names_and_show_each_capacity_axis() {
        let plateau = SurfacePlateau {
            name: "svc mailbox".to_string(),
            kind: "mail box",
            capacity: Some(4),
            high_water: 9,
            final_current: 0,
            full: 1,
            max_messages: Some(4),
            current_messages: 0,
            high_water_messages: 4,
            max_weight: Some(100),
            current_weight: Some(0),
            high_water_weight: Some(9),
            shared_max_weight: None,
            shared_current_weight: None,
            shared_high_water_weight: None,
            leak_clean: true,
        };
        let line = plateau.summary_line();
        assert!(line.contains("name=\"svc mailbox\""), "{line}");
        assert!(line.contains("kind=\"mail box\""), "{line}");
        assert!(line.contains("max_messages=4"), "{line}");
        assert!(line.contains("high_water_weight=9"), "{line}");
    }

    #[test]
    fn nonzero_weight_or_shared_current_marks_surface_leaky() {
        let mut pressure = ServicePressureReport::new("svc");
        pressure.add_measured(
            "body",
            CapacitySurfaceReport::weighted(
                "svc.body",
                CapacityMode::Fixed,
                100,
                3,
                10,
                0,
                "bytes",
            )
            .with_shared_scope("scope", 100, 0, 10, 0),
        );
        let plateaus = SurfacePlateau::from_service_pressure(&pressure);
        assert_eq!(plateaus[0].final_current, 3);
        assert!(!plateaus[0].leak_clean);
    }

    #[test]
    fn aggregate_current_does_not_hide_message_count_when_shared_weight_exists() {
        let mut pressure = ServicePressureReport::new("svc");
        pressure.add_measured(
            "mixed",
            CapacitySurfaceReport::count("svc.mixed", CapacityMode::Fixed, 4, 2, 3, 0)
                .with_shared_scope("scope", 100, 0, 50, 0),
        );
        let plateaus = SurfacePlateau::from_service_pressure(&pressure);
        assert_eq!(plateaus[0].final_current, 2);
        assert_eq!(plateaus[0].high_water, 50);
        assert!(!plateaus[0].leak_clean);
    }

    #[test]
    fn pressure_summary_tracks_rate_burst_and_first_error() {
        // Single worker so the local op index is deterministic.
        // First 2 ops Ok, then 3 errors, then 2 Ok, then 1 err — so
        // first error at op index 2, max_consecutive = 3, total = 4.
        let counter = Arc::new(AtomicU64::new(0));
        let inner = Arc::clone(&counter);
        let report = run(
            LoadRun {
                workers: 1,
                stop: LoadStop::ops(8),
                label: "pressure",
            },
            move |_| {
                let n = inner.fetch_add(1, Ordering::Relaxed);
                match n {
                    0 | 1 | 5 | 6 => OpOutcome::Ok,
                    7 => OpOutcome::Err { kind: "burst_b" },
                    _ => OpOutcome::Err { kind: "burst_a" },
                }
            },
            None::<fn() -> bool>,
        );
        assert_eq!(report.ops_attempted, 8, "{report:?}");
        assert_eq!(report.pressure.total, 4, "{report:?}");
        // rate = 4/8 = 500 per mille
        assert_eq!(report.pressure.rate_per_mille, 500, "{report:?}");
        assert_eq!(report.pressure.max_consecutive, 3, "{report:?}");
        assert_eq!(report.pressure.first_error_op_index, Some(2), "{report:?}");
        assert_eq!(report.pressure.by_kind.len(), 2);
        assert!(
            report.pressure.by_kind.iter().any(|(k, _)| k == "burst_a"),
            "{report:?}"
        );
        assert!(
            report.pressure.by_kind.iter().any(|(k, _)| k == "burst_b"),
            "{report:?}"
        );
    }

    #[test]
    fn pressure_summary_first_error_index_is_global_dispatch_order() {
        let report = run(
            LoadRun {
                workers: 2,
                stop: LoadStop::ops(2),
                label: "global_first_error",
            },
            |_| OpOutcome::Err { kind: "pressure" },
            None::<fn() -> bool>,
        );
        assert_eq!(report.ops_attempted, 2, "{report:?}");
        assert_eq!(report.pressure.first_error_op_index, Some(0));
    }

    #[test]
    fn pressure_summary_is_empty_on_clean_run() {
        let report = run(
            LoadRun {
                workers: 2,
                stop: LoadStop::ops(20),
                label: "clean",
            },
            |_| OpOutcome::Ok,
            None::<fn() -> bool>,
        );
        assert_eq!(report.pressure.total, 0, "{report:?}");
        assert_eq!(report.pressure.rate_per_mille, 0);
        assert_eq!(report.pressure.max_consecutive, 0);
        assert!(report.pressure.first_error_op_index.is_none());
        assert!(report.pressure.by_kind.is_empty());
    }

    #[test]
    #[should_panic(expected = "LoadRun.workers must be > 0")]
    fn zero_workers_panics() {
        let _ = run(
            LoadRun {
                workers: 0,
                stop: LoadStop::ops(1),
                label: "no_workers",
            },
            |_| OpOutcome::Ok,
            None::<fn() -> bool>,
        );
    }

    #[test]
    #[should_panic(expected = "LoadRun.stop needs op_count or duration")]
    fn no_stop_condition_panics() {
        let _ = run(
            LoadRun {
                workers: 1,
                stop: LoadStop {
                    op_count: None,
                    duration: None,
                },
                label: "no_stop",
            },
            |_| OpOutcome::Ok,
            None::<fn() -> bool>,
        );
    }

    #[test]
    fn summary_line_includes_min_us_and_pressure() {
        let report = run(
            LoadRun {
                workers: 1,
                stop: LoadStop::ops(3),
                label: "summary",
            },
            |_| OpOutcome::Ok,
            None::<fn() -> bool>,
        );
        let line = report.summary_line();
        assert!(line.contains("min_us="), "{line}");
        assert!(line.contains("pressure total="), "{line}");
    }

    #[test]
    fn task_shaped_load_assertions_accept_clean_report() {
        let report = run_with_observation(
            LoadRun {
                workers: 1,
                stop: LoadStop::ops(3),
                label: "assertions",
            },
            |_| OpOutcome::Ok,
            Some(|| LoadObservation {
                leak_checked: true,
                leak_clean: true,
                surface_plateaus: vec![SurfacePlateau {
                    name: "svc.mailbox".to_string(),
                    kind: "mailbox",
                    capacity: Some(4),
                    high_water: 2,
                    final_current: 0,
                    full: 0,
                    max_messages: Some(4),
                    current_messages: 0,
                    high_water_messages: 2,
                    max_weight: None,
                    current_weight: None,
                    high_water_weight: None,
                    shared_max_weight: None,
                    shared_current_weight: None,
                    shared_high_water_weight: None,
                    leak_clean: true,
                }],
                ..LoadObservation::default()
            }),
        );

        cold_work_made_progress(&report).unwrap();
        timer_kept_firing(&report).unwrap();
        surface_plateaued_cleanly(&report, "svc.mailbox").unwrap();
        no_leaked_capacity_at_shutdown(&report).unwrap();
    }

    #[test]
    fn load_assertion_failure_names_claim_and_observed_line() {
        let report = run(
            LoadRun {
                workers: 1,
                stop: LoadStop::ops(1),
                label: "failing_assertion",
            },
            |_| OpOutcome::Timeout,
            None::<fn() -> bool>,
        );
        let failure = cold_work_made_progress(&report).unwrap_err();
        assert_eq!(failure.claim, "cold work made progress");
        assert!(failure.observed.contains("load label=failing_assertion"));
    }

    #[test]
    fn capacity_assertions_fail_on_leak_and_missing_surface() {
        let leaky = run_with_observation(
            LoadRun {
                workers: 1,
                stop: LoadStop::ops(1),
                label: "leaky",
            },
            |_| OpOutcome::Ok,
            Some(|| LoadObservation {
                leak_checked: true,
                leak_clean: true,
                surface_plateaus: vec![SurfacePlateau {
                    name: "svc.mailbox".to_string(),
                    kind: "mailbox",
                    capacity: Some(1),
                    high_water: 1,
                    final_current: 1,
                    full: 0,
                    max_messages: Some(1),
                    current_messages: 1,
                    high_water_messages: 1,
                    max_weight: None,
                    current_weight: None,
                    high_water_weight: None,
                    shared_max_weight: None,
                    shared_current_weight: None,
                    shared_high_water_weight: None,
                    leak_clean: false,
                }],
                ..LoadObservation::default()
            }),
        );
        let missing = surface_plateaued_cleanly(&leaky, "missing").unwrap_err();
        assert_eq!(missing.claim, "surface plateaued cleanly");
        assert!(missing.observed.contains("missing surface"));

        let leak = no_leaked_capacity_at_shutdown(&leaky).unwrap_err();
        assert_eq!(leak.claim, "no leaked capacity at shutdown");
        assert!(leak.observed.contains("final_current=1"));
    }
}
