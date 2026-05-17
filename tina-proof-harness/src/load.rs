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
    pub err_kinds: Vec<(String, u64)>,
    pub latency_min_us: u64,
    pub latency_p50_us: u64,
    pub latency_p99_us: u64,
    pub latency_max_us: u64,
    pub elapsed_ms: u64,
    pub leak_clean: bool,
}

impl LoadReport {
    /// One-line summary, key=value, suitable for test output and grep.
    pub fn summary_line(&self) -> String {
        format!(
            "load label={} workers={} ops={} ok={} err={} timeout={} \
             p50_us={} p99_us={} max_us={} elapsed_ms={} leak_clean={}",
            self.label,
            self.workers,
            self.ops_attempted,
            self.ops_ok,
            self.ops_err,
            self.ops_timeout,
            self.latency_p50_us,
            self.latency_p99_us,
            self.latency_max_us,
            self.elapsed_ms,
            self.leak_clean,
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
                match outcome {
                    OpOutcome::Ok => obs.ok += 1,
                    OpOutcome::Err { kind } => {
                        obs.err += 1;
                        *obs.err_kinds.entry(kind.to_string()).or_insert(0) += 1;
                    }
                    OpOutcome::Timeout => obs.timeout += 1,
                }
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

    LoadReport {
        label: run.label,
        workers: run.workers,
        ops_attempted: combined.ok + combined.err + combined.timeout,
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
    }
}

#[derive(Default)]
struct WorkerObs {
    ok: u64,
    err: u64,
    timeout: u64,
    err_kinds: std::collections::BTreeMap<String, u64>,
    latencies_us: Vec<u64>,
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
        let report = run(
            LoadRun {
                workers: 2,
                stop: LoadStop::ops(20),
                label: "errs",
            },
            |worker| {
                if worker == 0 {
                    OpOutcome::Err { kind: "boom" }
                } else {
                    OpOutcome::Timeout
                }
            },
            None::<fn() -> bool>,
        );
        assert_eq!(report.ops_attempted, 20);
        // The harness can race on the early-exit check, but both kinds
        // must appear (per worker assignment above).
        assert!(report.ops_err > 0, "{report:?}");
        assert!(report.ops_timeout > 0, "{report:?}");
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
}
