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

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, Instant};

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

/// Summary of one [`LoadRun`].
///
/// `leak_clean` is `true` when the optional `leak_check` was either not
/// supplied or returned true. Latency is reported as min/p50/p99/max in
/// microseconds — enough to make a slow soak visible, small enough to
/// fit in one printable line.
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
    pub latency_min_us: u64,
    pub latency_p50_us: u64,
    pub latency_p99_us: u64,
    pub latency_max_us: u64,
    pub elapsed_ms: u64,
    pub leak_clean: bool,
    /// Typed pressure summary: rate, burst length, first-error
    /// position, per-kind breakdown. Lets a specimen assert
    /// "pressure stayed under N per mille" or "no burst longer than
    /// K consecutive errors" without parsing the summary line.
    pub pressure: PressureSummary,
}

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
    /// Position of the first non-ok outcome within the run (op index,
    /// 0-based, across all workers — the worker whose first error
    /// had the lowest local index wins ties). `None` when there was
    /// no pressure.
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
            by_kind.push_str(&format!("{k}:{v}"));
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
        format!(
            "load label={} workers={} ops={} ok={} err={} timeout={} \
             min_us={} p50_us={} p99_us={} max_us={} elapsed_ms={} leak_clean={} {}",
            self.label,
            self.workers,
            self.ops_attempted,
            self.ops_ok,
            self.ops_err,
            self.ops_timeout,
            self.latency_min_us,
            self.latency_p50_us,
            self.latency_p99_us,
            self.latency_max_us,
            self.elapsed_ms,
            self.leak_clean,
            self.pressure.summary_line(),
        )
    }
}

/// Run `op` under load.
///
/// `op` is invoked per worker, per iteration. It must be `Send + Sync`
/// because all worker threads share it via `Arc`. `leak_check`, if
/// supplied, runs once after all workers join, on the calling thread.
pub fn run<F, L>(run: LoadRun, op: F, leak_check: Option<L>) -> LoadReport
where
    F: Fn(usize) -> OpOutcome + Send + Sync + 'static,
    L: FnOnce() -> bool,
{
    assert!(run.workers > 0, "LoadRun.workers must be > 0");
    assert!(
        run.stop.op_count.is_some() || run.stop.duration.is_some(),
        "LoadRun.stop needs op_count or duration",
    );

    let op = Arc::new(op);
    let stop_at = run.stop.duration.map(|d| Instant::now() + d);
    let op_cap = run.stop.op_count.unwrap_or(u64::MAX);
    let ops_dispatched = Arc::new(AtomicU64::new(0));
    let halted = Arc::new(AtomicBool::new(false));

    let started = Instant::now();
    let mut handles = Vec::with_capacity(run.workers);
    for worker_id in 0..run.workers {
        let op = Arc::clone(&op);
        let ops_dispatched = Arc::clone(&ops_dispatched);
        let halted = Arc::clone(&halted);
        let handle = thread::spawn(move || -> WorkerObs {
            let mut obs = WorkerObs::default();
            let mut local_op_index: u64 = 0;
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
                obs.latencies_us.push(duration_to_us(dt));
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
                    if obs.first_error_local_index.is_none() {
                        obs.first_error_local_index = Some(local_op_index);
                    }
                    current_streak += 1;
                    if current_streak > obs.max_consecutive {
                        obs.max_consecutive = current_streak;
                    }
                } else {
                    current_streak = 0;
                }
                local_op_index += 1;
            }
            obs
        });
        handles.push(handle);
    }

    let mut combined = WorkerObs::default();
    for handle in handles {
        let obs = handle.join().expect("worker join");
        combined.merge(obs);
    }
    let elapsed = started.elapsed();

    let leak_clean = leak_check.map(|f| f()).unwrap_or(true);

    let mut latencies = combined.latencies_us;
    latencies.sort_unstable();
    let min = latencies.first().copied().unwrap_or(0);
    let max = latencies.last().copied().unwrap_or(0);
    let p50 = percentile(&latencies, 50);
    let p99 = percentile(&latencies, 99);

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
        first_error_op_index: combined.first_error_local_index,
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
        latency_min_us: min,
        latency_p50_us: p50,
        latency_p99_us: p99,
        latency_max_us: max,
        elapsed_ms: elapsed.as_millis() as u64,
        leak_clean,
        pressure,
    }
}

#[derive(Default)]
struct WorkerObs {
    ok: u64,
    err: u64,
    timeout: u64,
    err_kinds: std::collections::BTreeMap<String, u64>,
    latencies_us: Vec<u64>,
    max_consecutive: u64,
    first_error_local_index: Option<u64>,
}

impl WorkerObs {
    fn merge(&mut self, other: WorkerObs) {
        self.ok += other.ok;
        self.err += other.err;
        self.timeout += other.timeout;
        for (k, v) in other.err_kinds {
            *self.err_kinds.entry(k).or_insert(0) += v;
        }
        self.latencies_us.extend(other.latencies_us);
        if other.max_consecutive > self.max_consecutive {
            self.max_consecutive = other.max_consecutive;
        }
        self.first_error_local_index =
            match (self.first_error_local_index, other.first_error_local_index) {
                (None, x) | (x, None) => x,
                (Some(a), Some(b)) => Some(a.min(b)),
            };
    }
}

fn duration_to_us(d: Duration) -> u64 {
    d.as_micros().min(u128::from(u64::MAX)) as u64
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicU64;

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
        assert!(report.leak_clean);
        assert_eq!(counter.load(Ordering::Relaxed), 100);
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
        assert!(!report.leak_clean);
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
}
