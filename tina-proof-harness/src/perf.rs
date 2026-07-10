//! Local performance report helpers.
//!
//! This is the small, boring alpha command shape: run useful bounded
//! work in release mode, print local-machine timing, pressure,
//! capacity, and leak truth. It is performance evidence for this
//! checkout on this machine. Cross-framework comparisons need a
//! separate equivalent-workload baseline.

use std::process::Command;

use crate::load::LoadReport;

/// Environment attached to every perf line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PerfEnvironment {
    pub platform: &'static str,
    pub arch: &'static str,
    pub profile: String,
    pub git_sha: String,
}

impl PerfEnvironment {
    pub fn detect() -> Self {
        Self {
            platform: std::env::consts::OS,
            arch: std::env::consts::ARCH,
            profile: if cfg!(debug_assertions) {
                "debug"
            } else {
                "release"
            }
            .to_string(),
            git_sha: detect_git_sha(),
        }
    }

    pub fn summary_fields(&self) -> String {
        format!(
            "platform={} arch={} profile={} git_sha={}",
            report_value(self.platform),
            report_value(self.arch),
            report_value(&self.profile),
            report_value(&self.git_sha),
        )
    }
}

/// One local performance row.
///
/// `comparison_baseline` is deliberately data, not prose. For the
/// first form it is `none`: these rows answer "how fast did Tina run
/// this bounded workload here?", not "did Tina beat Tokio?".
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PerfReport {
    pub label: &'static str,
    pub kind: &'static str,
    pub comparison_baseline: &'static str,
    pub env: PerfEnvironment,
    pub load: LoadReport,
    pub allocation_scope: &'static str,
    pub allocations: Option<PerfAllocationReport>,
    pub samples: usize,
    pub sample_policy: &'static str,
}

impl PerfReport {
    pub fn from_load(label: &'static str, kind: &'static str, load: LoadReport) -> Self {
        Self {
            label,
            kind,
            comparison_baseline: "none",
            env: PerfEnvironment::detect(),
            load,
            allocation_scope: "none",
            allocations: None,
            samples: 1,
            sample_policy: "single",
        }
    }

    pub fn from_load_with_allocations(
        label: &'static str,
        kind: &'static str,
        load: LoadReport,
        allocations: PerfAllocationReport,
    ) -> Self {
        Self {
            label,
            kind,
            comparison_baseline: "none",
            env: PerfEnvironment::detect(),
            load,
            allocation_scope: allocations.scope,
            allocations: Some(allocations),
            samples: 1,
            sample_policy: "single",
        }
    }

    pub fn with_samples(mut self, samples: usize, sample_policy: &'static str) -> Self {
        self.samples = samples;
        self.sample_policy = sample_policy;
        self
    }

    /// One-line key=value shape for humans, CI logs, and grep.
    pub fn summary_line(&self) -> String {
        format!(
            "perf label={} kind={} comparison_baseline={} samples={} sample_policy={} {} {} {}",
            report_value(self.label),
            report_value(self.kind),
            self.comparison_baseline,
            self.samples,
            report_value(self.sample_policy),
            self.env.summary_fields(),
            self.load.summary_line(),
            self.allocation_summary_fields(),
        )
    }

    /// Tiny JSON shape for tools. Hand-written on purpose: the proof
    /// harness stays dependency-light and the fields are stable enough
    /// for alpha consumers.
    pub fn json_line(&self) -> String {
        format!(
            "{{\"schema\":\"tina.perf_report.v2\",\"label\":{},\"kind\":{},\"comparison_baseline\":{},\"samples\":{},\"sample_policy\":{},\"platform\":{},\"arch\":{},\"profile\":{},\"git_sha\":{},\"workers\":{},\"ops\":{},\"ok\":{},\"err\":{},\"timeout\":{},\"p50_us\":{},\"p90_us\":{},\"p99_us\":{},\"max_us\":{},\"p50_ns\":{},\"p90_ns\":{},\"p99_ns\":{},\"max_ns\":{},\"elapsed_ms\":{},\"leak_checked\":{},\"leak_clean\":{},\"pressure_total\":{},\"pressure_rate_per_mille\":{},\"surfaces\":{},\"unavailable_surfaces\":{},\"allocation_scope\":{},\"allocations\":{},\"allocated_bytes\":{}}}",
            json_string(self.label),
            json_string(self.kind),
            json_string(self.comparison_baseline),
            self.samples,
            json_string(self.sample_policy),
            json_string(self.env.platform),
            json_string(self.env.arch),
            json_string(&self.env.profile),
            json_string(&self.env.git_sha),
            self.load.workers,
            self.load.ops_attempted,
            self.load.ops_ok,
            self.load.ops_err,
            self.load.ops_timeout,
            self.load.latency_p50_us,
            self.load.latency_p90_us,
            self.load.latency_p99_us,
            self.load.latency_max_us,
            self.load.latency_p50_ns,
            self.load.latency_p90_ns,
            self.load.latency_p99_ns,
            self.load.latency_max_ns,
            self.load.elapsed_ms,
            self.load.leak_checked,
            self.load.leak_clean,
            self.load.pressure.total,
            self.load.pressure.rate_per_mille,
            self.load.surface_plateaus.len(),
            self.load.unavailable_surfaces.len(),
            json_string(self.allocation_scope),
            self.allocations
                .map(|allocations| allocations.allocations.to_string())
                .unwrap_or_else(|| "null".to_string()),
            self.allocations
                .map(|allocations| allocations.allocated_bytes.to_string())
                .unwrap_or_else(|| "null".to_string()),
        )
    }

    fn allocation_summary_fields(&self) -> String {
        match self.allocations {
            Some(allocations) => format!(
                "allocation_scope={} allocations={} allocated_bytes={}",
                report_value(allocations.scope),
                allocations.allocations,
                allocations.allocated_bytes
            ),
            None => "allocation_scope=none allocations=unknown allocated_bytes=unknown".to_string(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PerfAllocationReport {
    pub scope: &'static str,
    pub allocations: u64,
    pub allocated_bytes: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SemanticMatch {
    Exact,
    Partial,
    None,
}

impl SemanticMatch {
    fn as_str(self) -> &'static str {
        match self {
            Self::Exact => "exact",
            Self::Partial => "partial",
            Self::None => "none",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PerfComparisonReport {
    pub label: &'static str,
    pub tina: PerfReport,
    pub baseline: PerfReport,
    pub semantic_match: SemanticMatch,
    pub mismatch_reason: &'static str,
    pub samples: usize,
    pub sample_policy: &'static str,
}

impl PerfComparisonReport {
    pub fn new(
        label: &'static str,
        tina: PerfReport,
        baseline: PerfReport,
        semantic_match: SemanticMatch,
        mismatch_reason: &'static str,
    ) -> Self {
        Self {
            label,
            tina,
            baseline,
            semantic_match,
            mismatch_reason,
            samples: 1,
            sample_policy: "single",
        }
    }

    pub fn with_samples(mut self, samples: usize, sample_policy: &'static str) -> Self {
        self.samples = samples;
        self.sample_policy = sample_policy;
        self
    }

    pub fn ratio_per_mille(&self) -> Option<u64> {
        ratio_per_mille(
            self.tina.load.latency_p50_ns,
            self.baseline.load.latency_p50_ns,
        )
    }

    pub fn p99_ratio_per_mille(&self) -> Option<u64> {
        ratio_per_mille(
            self.tina.load.latency_p99_ns,
            self.baseline.load.latency_p99_ns,
        )
    }

    pub fn p90_ratio_per_mille(&self) -> Option<u64> {
        ratio_per_mille(
            self.tina.load.latency_p90_ns,
            self.baseline.load.latency_p90_ns,
        )
    }

    pub fn summary_line(&self) -> String {
        format!(
            "perf-compare label={} semantic_match={} mismatch_reason={} samples={} sample_policy={} tina_p50_us={} baseline_p50_us={} tina_p50_ns={} baseline_p50_ns={} p50_ratio_per_mille={} tina_p90_us={} baseline_p90_us={} tina_p90_ns={} baseline_p90_ns={} p90_ratio_per_mille={} tina_p99_us={} baseline_p99_us={} tina_p99_ns={} baseline_p99_ns={} p99_ratio_per_mille={} tina_allocations={} baseline_allocations={} tina_allocated_bytes={} baseline_allocated_bytes={} tina_pressure={} baseline_pressure={} tina_leak_clean={} baseline_leak_clean={}",
            report_value(self.label),
            self.semantic_match.as_str(),
            report_value(self.mismatch_reason),
            self.samples,
            report_value(self.sample_policy),
            self.tina.load.latency_p50_us,
            self.baseline.load.latency_p50_us,
            self.tina.load.latency_p50_ns,
            self.baseline.load.latency_p50_ns,
            self.ratio_per_mille()
                .map(|value| value.to_string())
                .unwrap_or_else(|| "none".to_string()),
            self.tina.load.latency_p90_us,
            self.baseline.load.latency_p90_us,
            self.tina.load.latency_p90_ns,
            self.baseline.load.latency_p90_ns,
            self.p90_ratio_per_mille()
                .map(|value| value.to_string())
                .unwrap_or_else(|| "none".to_string()),
            self.tina.load.latency_p99_us,
            self.baseline.load.latency_p99_us,
            self.tina.load.latency_p99_ns,
            self.baseline.load.latency_p99_ns,
            self.p99_ratio_per_mille()
                .map(|value| value.to_string())
                .unwrap_or_else(|| "none".to_string()),
            option_allocations(self.tina.allocations, |allocations| allocations.allocations),
            option_allocations(self.baseline.allocations, |allocations| allocations
                .allocations),
            option_allocations(self.tina.allocations, |allocations| allocations
                .allocated_bytes),
            option_allocations(self.baseline.allocations, |allocations| allocations
                .allocated_bytes),
            self.tina.load.pressure.total,
            self.baseline.load.pressure.total,
            self.tina.load.leak_clean,
            self.baseline.load.leak_clean,
        )
    }

    pub fn json_line(&self) -> String {
        format!(
            "{{\"schema\":\"tina.perf_compare.v1\",\"label\":{},\"semantic_match\":{},\"mismatch_reason\":{},\"samples\":{},\"sample_policy\":{},\"tina_p50_us\":{},\"baseline_p50_us\":{},\"tina_p50_ns\":{},\"baseline_p50_ns\":{},\"p50_ratio_per_mille\":{},\"tina_p90_us\":{},\"baseline_p90_us\":{},\"tina_p90_ns\":{},\"baseline_p90_ns\":{},\"p90_ratio_per_mille\":{},\"tina_p99_us\":{},\"baseline_p99_us\":{},\"tina_p99_ns\":{},\"baseline_p99_ns\":{},\"p99_ratio_per_mille\":{},\"tina_allocations\":{},\"baseline_allocations\":{},\"tina_allocated_bytes\":{},\"baseline_allocated_bytes\":{},\"tina_pressure\":{},\"baseline_pressure\":{},\"tina_leak_clean\":{},\"baseline_leak_clean\":{}}}",
            json_string(self.label),
            json_string(self.semantic_match.as_str()),
            json_string(self.mismatch_reason),
            self.samples,
            json_string(self.sample_policy),
            self.tina.load.latency_p50_us,
            self.baseline.load.latency_p50_us,
            self.tina.load.latency_p50_ns,
            self.baseline.load.latency_p50_ns,
            self.ratio_per_mille()
                .map(|value| value.to_string())
                .unwrap_or_else(|| "null".to_string()),
            self.tina.load.latency_p90_us,
            self.baseline.load.latency_p90_us,
            self.tina.load.latency_p90_ns,
            self.baseline.load.latency_p90_ns,
            self.p90_ratio_per_mille()
                .map(|value| value.to_string())
                .unwrap_or_else(|| "null".to_string()),
            self.tina.load.latency_p99_us,
            self.baseline.load.latency_p99_us,
            self.tina.load.latency_p99_ns,
            self.baseline.load.latency_p99_ns,
            self.p99_ratio_per_mille()
                .map(|value| value.to_string())
                .unwrap_or_else(|| "null".to_string()),
            option_allocations_json(self.tina.allocations, |allocations| allocations.allocations),
            option_allocations_json(self.baseline.allocations, |allocations| allocations
                .allocations),
            option_allocations_json(self.tina.allocations, |allocations| allocations
                .allocated_bytes),
            option_allocations_json(self.baseline.allocations, |allocations| allocations
                .allocated_bytes),
            self.tina.load.pressure.total,
            self.baseline.load.pressure.total,
            self.tina.load.leak_clean,
            self.baseline.load.leak_clean,
        )
    }
}

/// One measured boundary inside a hot-path probe, in nanoseconds.
///
/// A stage is a wall-clock gap between two observable points: host submit,
/// a worker-thread trace event (captured live through a `TraceObserver`),
/// or host unblock. The name describes the boundary, not a guess at the
/// cause — `host_submit_to_worker_pickup`, not "wakeup tax".
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HotPathStage {
    pub name: String,
    pub nanos: u64,
}

impl HotPathStage {
    pub fn new(name: impl Into<String>, nanos: u64) -> Self {
        Self {
            name: name.into(),
            nanos,
        }
    }
}

/// Counts from the same instrumented trace window that produced the hot-path
/// stage breakdown.
///
/// `event_stage_count` intentionally mirrors the old `stage_count` field. The
/// other counters split out the facts users care about: handler turns, backend
/// runtime calls, service calls, successful completions, and rejected
/// completions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct HotPathCounters {
    pub event_stage_count: usize,
    pub handler_turn_count: usize,
    pub runtime_call_count: usize,
    pub service_call_count: usize,
    pub completion_count: usize,
    pub rejected_completion_count: usize,
}

impl HotPathCounters {
    pub fn from_stage_count(event_stage_count: usize) -> Self {
        Self {
            event_stage_count,
            ..Self::default()
        }
    }
}

/// A hot-path probe report: end-to-end latency over N iterations plus a
/// single representative per-stage breakdown.
///
/// Prints both nanoseconds and microseconds so a sub-microsecond stage never
/// rounds into a fake zero. The stage breakdown comes from one instrumented
/// run; the p50/min/max come from the full iteration set so a single slow
/// scheduling hiccup does not masquerade as the steady-state cost.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HotPathReport {
    pub label: &'static str,
    pub iterations: u64,
    pub p50_ns: u64,
    /// 90th percentile total. Tail shape, not the median, is what the row
    /// chases: a row can win p50 while p90/p99 stay messy.
    pub p90_ns: u64,
    /// 99th percentile total.
    pub p99_ns: u64,
    pub min_ns: u64,
    pub max_ns: u64,
    /// Whether this row was sampled with a live `TraceObserver` attached. The
    /// untraced row is the honest headline; the traced row exposes observer
    /// overhead. The two must never be confused, so the label is suffixed and
    /// this flag is emitted on every line.
    pub traced: bool,
    /// Gap threshold used to count scheduler gaps in the instrumented run.
    /// Carried as a field so a different platform can tune it without code
    /// changes invalidating older history.
    pub scheduler_gap_threshold_ns: u64,
    /// Number of inter-event gaps in the instrumented timeline at or above
    /// `scheduler_gap_threshold_ns`. A worker park between a ready completion
    /// and the next handler turn shows up here.
    pub scheduler_gap_count: usize,
    /// Largest single inter-event gap in the instrumented timeline.
    pub max_scheduler_gap_ns: u64,
    pub stages: Vec<HotPathStage>,
    /// Allocations the caller's host thread makes per op. Misses anything the
    /// runtime worker thread allocates on the caller's behalf — see
    /// [`HotPathReport::process_allocations`] for the full picture.
    pub allocations: Option<u64>,
    /// Allocations the whole process makes per op: host thread + runtime
    /// worker thread + lane workers. This is the real per-op allocation cost.
    pub process_allocations: Option<u64>,
    /// Counts derived from the same trace window as `stages`.
    pub counters: HotPathCounters,
    pub env: PerfEnvironment,
}

impl HotPathReport {
    /// Builds a report from a non-empty set of per-iteration totals (ns) plus
    /// an optional single-run stage breakdown and allocation count.
    pub fn from_samples(
        label: &'static str,
        totals_ns: Vec<u64>,
        stages: Vec<HotPathStage>,
        allocations: Option<u64>,
    ) -> Self {
        Self::from_samples_with_process_allocations(label, totals_ns, stages, allocations, None)
    }

    /// Builds a report including both host-thread allocations (`allocations`)
    /// and whole-process allocations (`process_allocations`). The latter
    /// captures everything the runtime worker thread allocates on the
    /// caller's behalf, which the host-only count misses.
    pub fn from_samples_with_process_allocations(
        label: &'static str,
        totals_ns: Vec<u64>,
        stages: Vec<HotPathStage>,
        allocations: Option<u64>,
        process_allocations: Option<u64>,
    ) -> Self {
        let counters = HotPathCounters::from_stage_count(stages.len());
        Self::from_samples_with_counters(
            label,
            totals_ns,
            stages,
            allocations,
            process_allocations,
            counters,
        )
    }

    /// Builds a report including explicit trace counters from the same
    /// instrumented window as `stages`.
    pub fn from_samples_with_counters(
        label: &'static str,
        totals_ns: Vec<u64>,
        stages: Vec<HotPathStage>,
        allocations: Option<u64>,
        process_allocations: Option<u64>,
        counters: HotPathCounters,
    ) -> Self {
        assert!(
            !totals_ns.is_empty(),
            "hot-path report needs at least one timing sample"
        );
        assert_eq!(
            counters.event_stage_count,
            stages.len(),
            "event_stage_count must match stage_count"
        );
        let mut sorted_ns = totals_ns;
        sorted_ns.sort_unstable();
        let iterations = sorted_ns.len() as u64;
        let p50_ns = percentile_ns(&sorted_ns, 50);
        let p90_ns = percentile_ns(&sorted_ns, 90);
        let p99_ns = percentile_ns(&sorted_ns, 99);
        let min_ns = *sorted_ns.first().expect("non-empty");
        let max_ns = *sorted_ns.last().expect("non-empty");
        Self {
            label,
            iterations,
            p50_ns,
            p90_ns,
            p99_ns,
            min_ns,
            max_ns,
            traced: false,
            scheduler_gap_threshold_ns: DEFAULT_SCHEDULER_GAP_THRESHOLD_NS,
            scheduler_gap_count: 0,
            max_scheduler_gap_ns: 0,
            stages,
            allocations,
            process_allocations,
            counters,
            env: PerfEnvironment::detect(),
        }
    }

    /// Records scheduler-gap stats measured from the instrumented timeline.
    /// `count` is the number of inter-event gaps at or above `threshold_ns`,
    /// `max_ns` the largest single gap. Chainable on a freshly built report.
    pub fn with_scheduler_gaps(mut self, threshold_ns: u64, count: usize, max_ns: u64) -> Self {
        self.scheduler_gap_threshold_ns = threshold_ns;
        self.scheduler_gap_count = count;
        self.max_scheduler_gap_ns = max_ns;
        self
    }

    /// Marks whether this row was sampled with a live trace observer attached.
    pub fn with_traced(mut self, traced: bool) -> Self {
        self.traced = traced;
        self
    }

    /// `p90 / p50` in per-mille (1000 = p90 equals p50). Lower is tighter.
    pub fn p90_over_p50_per_mille(&self) -> Option<u64> {
        ratio_per_mille(self.p90_ns, self.p50_ns)
    }

    /// `p99 / p50` in per-mille. Lower is tighter.
    pub fn p99_over_p50_per_mille(&self) -> Option<u64> {
        ratio_per_mille(self.p99_ns, self.p50_ns)
    }

    /// `(max - min) / p50` in per-mille: how wide the whole sampled range is
    /// relative to the median.
    pub fn range_over_p50_per_mille(&self) -> Option<u64> {
        ratio_per_mille(self.max_ns.saturating_sub(self.min_ns), self.p50_ns)
    }

    /// One-line key=value shape for humans, CI logs, and grep. Each stage is
    /// its own `stage.<name>_ns=<v>` field so the line stays flat and
    /// greppable.
    pub fn summary_line(&self) -> String {
        let mut line = format!(
            "hotpath label={} iterations={} traced={} p50_us={} p50_ns={} p90_ns={} p99_ns={} min_ns={} max_ns={} p90_over_p50_per_mille={} p99_over_p50_per_mille={} range_over_p50_per_mille={} scheduler_gap_threshold_ns={} scheduler_gap_count={} max_scheduler_gap_ns={} stage_count={} event_stage_count={} handler_turn_count={} runtime_call_count={} service_call_count={} completion_count={} rejected_completion_count={} host_allocations={} process_allocations={} {}",
            report_value(self.label),
            self.iterations,
            self.traced,
            self.p50_ns / 1_000,
            self.p50_ns,
            self.p90_ns,
            self.p99_ns,
            self.min_ns,
            self.max_ns,
            per_mille_field(self.p90_over_p50_per_mille()),
            per_mille_field(self.p99_over_p50_per_mille()),
            per_mille_field(self.range_over_p50_per_mille()),
            self.scheduler_gap_threshold_ns,
            self.scheduler_gap_count,
            self.max_scheduler_gap_ns,
            self.counters.event_stage_count,
            self.counters.event_stage_count,
            self.counters.handler_turn_count,
            self.counters.runtime_call_count,
            self.counters.service_call_count,
            self.counters.completion_count,
            self.counters.rejected_completion_count,
            self.allocations
                .map(|value| value.to_string())
                .unwrap_or_else(|| "unknown".to_string()),
            self.process_allocations
                .map(|value| value.to_string())
                .unwrap_or_else(|| "unknown".to_string()),
            self.env.summary_fields(),
        );
        // Back-compat: keep the original key so existing grep lines still match.
        line.push_str(&format!(
            " allocations={}",
            self.allocations
                .map(|value| value.to_string())
                .unwrap_or_else(|| "unknown".to_string())
        ));
        for stage in &self.stages {
            line.push_str(&format!(
                " stage.{}_ns={}",
                report_value(&stage.name),
                stage.nanos
            ));
        }
        line
    }

    /// Tiny JSON shape for tools. Stages land in a nested object keyed by name.
    pub fn json_line(&self) -> String {
        let mut stages = String::from("{");
        for (index, stage) in self.stages.iter().enumerate() {
            if index > 0 {
                stages.push(',');
            }
            stages.push_str(&format!("{}:{}", json_string(&stage.name), stage.nanos));
        }
        stages.push('}');
        format!(
            "{{\"schema\":\"tina.hotpath.v2\",\"label\":{},\"iterations\":{},\"traced\":{},\"p50_ns\":{},\"p90_ns\":{},\"p99_ns\":{},\"min_ns\":{},\"max_ns\":{},\"p90_over_p50_per_mille\":{},\"p99_over_p50_per_mille\":{},\"range_over_p50_per_mille\":{},\"scheduler_gap_threshold_ns\":{},\"scheduler_gap_count\":{},\"max_scheduler_gap_ns\":{},\"stage_count\":{},\"event_stage_count\":{},\"handler_turn_count\":{},\"runtime_call_count\":{},\"service_call_count\":{},\"completion_count\":{},\"rejected_completion_count\":{},\"host_allocations\":{},\"process_allocations\":{},\"allocations\":{},\"platform\":{},\"arch\":{},\"profile\":{},\"git_sha\":{},\"stages\":{}}}",
            json_string(self.label),
            self.iterations,
            self.traced,
            self.p50_ns,
            self.p90_ns,
            self.p99_ns,
            self.min_ns,
            self.max_ns,
            per_mille_field(self.p90_over_p50_per_mille()),
            per_mille_field(self.p99_over_p50_per_mille()),
            per_mille_field(self.range_over_p50_per_mille()),
            self.scheduler_gap_threshold_ns,
            self.scheduler_gap_count,
            self.max_scheduler_gap_ns,
            self.counters.event_stage_count,
            self.counters.event_stage_count,
            self.counters.handler_turn_count,
            self.counters.runtime_call_count,
            self.counters.service_call_count,
            self.counters.completion_count,
            self.counters.rejected_completion_count,
            self.allocations
                .map(|value| value.to_string())
                .unwrap_or_else(|| "null".to_string()),
            self.process_allocations
                .map(|value| value.to_string())
                .unwrap_or_else(|| "null".to_string()),
            self.allocations
                .map(|value| value.to_string())
                .unwrap_or_else(|| "null".to_string()),
            json_string(self.env.platform),
            json_string(self.env.arch),
            json_string(&self.env.profile),
            json_string(&self.env.git_sha),
            stages,
        )
    }
}

fn ratio_per_mille(a: u64, b: u64) -> Option<u64> {
    if b == 0 {
        return None;
    }
    Some(a.saturating_mul(1000) / b)
}

/// Renders an optional per-mille ratio as the bare number, or `null` when p50
/// is zero (sub-microsecond rows where a ratio is meaningless). The unquoted
/// `null` is valid in both the flat summary line and the JSON shape.
fn per_mille_field(value: Option<u64>) -> String {
    value
        .map(|v| v.to_string())
        .unwrap_or_else(|| "null".to_string())
}

/// Default scheduler-gap threshold for local release rows: 100us. A gap above
/// this between two trace events is a scheduling hiccup worth counting, not
/// ordinary handler cost.
pub const DEFAULT_SCHEDULER_GAP_THRESHOLD_NS: u64 = 100_000;

/// Nearest-rank percentile over an already-sorted, non-empty slice. `p` is a
/// whole percent (50, 90, 99). Matches the legacy p50 (`sorted[len/2]`) for
/// p=50 so existing rows do not move.
fn percentile_ns(sorted_ns: &[u64], p: usize) -> u64 {
    debug_assert!(!sorted_ns.is_empty());
    let len = sorted_ns.len();
    let idx = (len * p / 100).min(len - 1);
    sorted_ns[idx]
}

fn option_allocations(
    allocations: Option<PerfAllocationReport>,
    f: impl FnOnce(PerfAllocationReport) -> u64,
) -> String {
    allocations
        .map(f)
        .map(|value| value.to_string())
        .unwrap_or_else(|| "unknown".to_string())
}

fn option_allocations_json(
    allocations: Option<PerfAllocationReport>,
    f: impl FnOnce(PerfAllocationReport) -> u64,
) -> String {
    allocations
        .map(f)
        .map(|value| value.to_string())
        .unwrap_or_else(|| "null".to_string())
}

fn detect_git_sha() -> String {
    if let Ok(value) = std::env::var("TINA_PERF_GIT_SHA")
        && !value.trim().is_empty()
    {
        return value;
    }
    let Ok(output) = Command::new("git")
        .args(["rev-parse", "--short", "HEAD"])
        .output()
    else {
        return "unknown".to_string();
    };
    if !output.status.success() {
        return "unknown".to_string();
    }
    let mut sha = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if git_tree_is_dirty() {
        sha.push_str("-dirty");
    }
    sha
}

fn git_tree_is_dirty() -> bool {
    let dirty_worktree = Command::new("git")
        .args(["diff", "--quiet", "--ignore-submodules", "--", "."])
        .status()
        .map(|status| !status.success())
        .unwrap_or(false);
    let dirty_index = Command::new("git")
        .args([
            "diff",
            "--cached",
            "--quiet",
            "--ignore-submodules",
            "--",
            ".",
        ])
        .status()
        .map(|status| !status.success())
        .unwrap_or(false);
    let has_untracked = Command::new("git")
        .args(["ls-files", "--others", "--exclude-standard"])
        .output()
        .map(|output| !output.stdout.is_empty())
        .unwrap_or(false);
    dirty_worktree || dirty_index || has_untracked
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

fn json_string(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 2);
    out.push('"');
    for ch in value.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c.is_control() => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::load::{LoadRun, LoadStop, OpOutcome, run};

    #[test]
    fn perf_report_names_comparison_baseline() {
        let load = run(
            LoadRun {
                workers: 1,
                stop: LoadStop::ops(4),
                label: "unit_load",
            },
            |_| OpOutcome::Ok,
            None::<fn() -> bool>,
        );
        let report = PerfReport::from_load("unit_perf", "runtime", load);
        let line = report.summary_line();
        assert!(line.starts_with("perf "), "{line}");
        assert!(line.contains("comparison_baseline=none"), "{line}");
        assert!(line.contains("samples=1"), "{line}");
        assert!(line.contains("sample_policy=single"), "{line}");
        assert!(line.contains("p50_us="), "{line}");
        assert!(line.contains("pressure total=0"), "{line}");
        assert!(
            line.contains("allocation_scope=none allocations=unknown allocated_bytes=unknown"),
            "{line}"
        );
    }

    #[test]
    fn perf_report_json_carries_tool_fields() {
        let load = run(
            LoadRun {
                workers: 1,
                stop: LoadStop::ops(2),
                label: "unit_load",
            },
            |_| OpOutcome::Err { kind: "full" },
            None::<fn() -> bool>,
        );
        let report = PerfReport::from_load("needs quoting", "whole service", load);
        let json = report.json_line();
        assert!(
            json.contains("\"schema\":\"tina.perf_report.v2\""),
            "{json}"
        );
        assert!(json.contains("\"label\":\"needs quoting\""), "{json}");
        assert!(json.contains("\"comparison_baseline\":\"none\""), "{json}");
        assert!(json.contains("\"samples\":1"), "{json}");
        assert!(json.contains("\"sample_policy\":\"single\""), "{json}");
        assert!(json.contains("\"pressure_total\":2"), "{json}");
        assert!(json.contains("\"leak_checked\":"), "{json}");
        assert!(json.contains("\"leak_clean\":"), "{json}");
        assert!(json.contains("\"allocation_scope\":\"none\""), "{json}");
        assert!(json.contains("\"allocations\":null"), "{json}");
        assert!(json.contains("\"allocated_bytes\":null"), "{json}");
    }

    #[test]
    fn comparison_report_carries_ratio_and_semantic_match() {
        let tina = PerfReport::from_load(
            "tina",
            "send",
            run(
                LoadRun {
                    workers: 1,
                    stop: LoadStop::ops(2),
                    label: "tina",
                },
                |_| OpOutcome::Ok,
                None::<fn() -> bool>,
            ),
        );
        let baseline = PerfReport::from_load(
            "tokio",
            "send",
            run(
                LoadRun {
                    workers: 1,
                    stop: LoadStop::ops(2),
                    label: "tokio",
                },
                |_| OpOutcome::Ok,
                None::<fn() -> bool>,
            ),
        );
        let report =
            PerfComparisonReport::new("send", tina, baseline, SemanticMatch::Exact, "none")
                .with_samples(5, "median_p50_after_warmup");
        let line = report.summary_line();
        assert!(line.starts_with("perf-compare "), "{line}");
        assert!(line.contains("semantic_match=exact"), "{line}");
        assert!(line.contains("samples=5"), "{line}");
        assert!(
            line.contains("sample_policy=median_p50_after_warmup"),
            "{line}"
        );
        assert!(line.contains("p99_ratio_per_mille="), "{line}");
        assert!(line.contains("tina_allocations=unknown"), "{line}");
        let json = report.json_line();
        assert!(
            json.contains("\"schema\":\"tina.perf_compare.v1\""),
            "{json}"
        );
        assert!(json.contains("\"p99_ratio_per_mille\":"), "{json}");
        assert!(json.contains("\"samples\":5"), "{json}");
        assert!(
            json.contains("\"sample_policy\":\"median_p50_after_warmup\""),
            "{json}"
        );
        assert!(json.contains("\"tina_allocations\":null"), "{json}");
    }

    #[test]
    fn hotpath_report_prints_ns_us_and_named_stages() {
        let report = HotPathReport::from_samples(
            "hotpath_call_blocking",
            vec![900, 1100, 1000, 1200, 800],
            vec![
                HotPathStage::new("host_submit_to_worker_pickup", 250),
                HotPathStage::new("begin_to_target_handler", 120),
            ],
            Some(7),
        );
        let line = report.summary_line();
        assert!(line.starts_with("hotpath "), "{line}");
        assert!(line.contains("iterations=5"), "{line}");
        assert!(line.contains("p50_ns=1000"), "{line}");
        assert!(line.contains("p50_us=1"), "{line}");
        assert!(line.contains("min_ns=800"), "{line}");
        assert!(line.contains("max_ns=1200"), "{line}");
        // Tail fields: p90/p99 and the ratio + scheduler-gap fields a tail row
        // is judged on. p90/p99 here are the nearest-rank values over the five
        // samples [800,900,1000,1100,1200].
        assert!(line.contains("p90_ns=1200"), "{line}");
        assert!(line.contains("p99_ns=1200"), "{line}");
        assert!(line.contains("p90_over_p50_per_mille=1200"), "{line}");
        assert!(line.contains("p99_over_p50_per_mille=1200"), "{line}");
        assert!(line.contains("range_over_p50_per_mille=400"), "{line}");
        assert!(line.contains("traced=false"), "{line}");
        assert!(line.contains("scheduler_gap_threshold_ns=100000"), "{line}");
        assert!(line.contains("scheduler_gap_count=0"), "{line}");
        assert!(line.contains("max_scheduler_gap_ns=0"), "{line}");
        assert!(line.contains("stage_count=2"), "{line}");
        assert!(line.contains("event_stage_count=2"), "{line}");
        assert!(line.contains("handler_turn_count=0"), "{line}");
        assert!(line.contains("runtime_call_count=0"), "{line}");
        assert!(line.contains("service_call_count=0"), "{line}");
        assert!(line.contains("completion_count=0"), "{line}");
        assert!(line.contains("rejected_completion_count=0"), "{line}");
        assert!(line.contains("allocations=7"), "{line}");
        assert!(
            line.contains("stage.host_submit_to_worker_pickup_ns=250"),
            "{line}"
        );
        assert!(
            line.contains("stage.begin_to_target_handler_ns=120"),
            "{line}"
        );

        let json = report.json_line();
        assert!(json.contains("\"schema\":\"tina.hotpath.v2\""), "{json}");
        assert!(json.contains("\"p50_ns\":1000"), "{json}");
        assert!(json.contains("\"p90_ns\":1200"), "{json}");
        assert!(json.contains("\"p99_ns\":1200"), "{json}");
        assert!(json.contains("\"traced\":false"), "{json}");
        assert!(json.contains("\"scheduler_gap_count\":0"), "{json}");
        assert!(json.contains("\"max_scheduler_gap_ns\":0"), "{json}");
        assert!(json.contains("\"stage_count\":2"), "{json}");
        assert!(json.contains("\"event_stage_count\":2"), "{json}");
        assert!(json.contains("\"handler_turn_count\":0"), "{json}");
        assert!(
            json.contains("\"host_submit_to_worker_pickup\":250"),
            "{json}"
        );
        assert!(json.contains("\"allocations\":7"), "{json}");
    }

    #[test]
    fn hotpath_tail_builders_carry_traced_and_scheduler_gaps() {
        // A traced row with measured scheduler gaps: the builders must surface
        // both the `traced` label and the gap stats so an untraced headline is
        // never confused with a traced one, and a worker park gap is visible.
        let report = HotPathReport::from_samples(
            "hotpath_call_blocking_tail",
            vec![1_000, 1_000, 1_000, 1_000, 4_000],
            vec![HotPathStage::new("host_submit_to_worker_pickup", 250)],
            Some(2),
        )
        .with_traced(true)
        .with_scheduler_gaps(DEFAULT_SCHEDULER_GAP_THRESHOLD_NS, 2, 350_000);

        assert!(report.traced);
        assert_eq!(report.scheduler_gap_count, 2);
        assert_eq!(report.max_scheduler_gap_ns, 350_000);

        let line = report.summary_line();
        assert!(line.contains("traced=true"), "{line}");
        assert!(line.contains("scheduler_gap_count=2"), "{line}");
        assert!(line.contains("max_scheduler_gap_ns=350000"), "{line}");

        let json = report.json_line();
        assert!(json.contains("\"traced\":true"), "{json}");
        assert!(json.contains("\"scheduler_gap_count\":2"), "{json}");
        assert!(json.contains("\"max_scheduler_gap_ns\":350000"), "{json}");
    }

    #[test]
    fn hotpath_ratio_fields_are_null_when_p50_is_zero() {
        // Sub-microsecond rows can have a zero p50; the ratio fields must be
        // `null`, not a divide-by-zero panic or a fake 0.
        let report = HotPathReport::from_samples(
            "hotpath_try_send",
            vec![0, 0, 0, 0, 0],
            vec![HotPathStage::new("host_submit_to_command_accepted", 0)],
            Some(1),
        );
        assert_eq!(report.p50_ns, 0);
        assert_eq!(report.p90_over_p50_per_mille(), None);
        let line = report.summary_line();
        assert!(line.contains("p90_over_p50_per_mille=null"), "{line}");
        let json = report.json_line();
        assert!(json.contains("\"p90_over_p50_per_mille\":null"), "{json}");
    }
}
