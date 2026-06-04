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
        }
    }

    /// One-line key=value shape for humans, CI logs, and grep.
    pub fn summary_line(&self) -> String {
        format!(
            "perf label={} kind={} comparison_baseline={} {} {} {}",
            report_value(self.label),
            report_value(self.kind),
            self.comparison_baseline,
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
            "{{\"schema\":\"tina.perf_report.v1\",\"label\":{},\"kind\":{},\"comparison_baseline\":{},\"platform\":{},\"arch\":{},\"profile\":{},\"git_sha\":{},\"workers\":{},\"ops\":{},\"ok\":{},\"err\":{},\"timeout\":{},\"p50_us\":{},\"p90_us\":{},\"p99_us\":{},\"max_us\":{},\"p50_ns\":{},\"p90_ns\":{},\"p99_ns\":{},\"max_ns\":{},\"elapsed_ms\":{},\"leak_clean\":{},\"pressure_total\":{},\"pressure_rate_per_mille\":{},\"surfaces\":{},\"unavailable_surfaces\":{},\"allocation_scope\":{},\"allocations\":{},\"allocated_bytes\":{}}}",
            json_string(self.label),
            json_string(self.kind),
            json_string(self.comparison_baseline),
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
    pub min_ns: u64,
    pub max_ns: u64,
    pub stages: Vec<HotPathStage>,
    /// Allocations the caller's host thread makes per op. Misses anything the
    /// runtime worker thread allocates on the caller's behalf — see
    /// [`HotPathReport::process_allocations`] for the full picture.
    pub allocations: Option<u64>,
    /// Allocations the whole process makes per op: host thread + runtime
    /// worker thread + lane workers. This is the real per-op allocation cost.
    pub process_allocations: Option<u64>,
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
        mut totals_ns: Vec<u64>,
        stages: Vec<HotPathStage>,
        allocations: Option<u64>,
        process_allocations: Option<u64>,
    ) -> Self {
        assert!(
            !totals_ns.is_empty(),
            "hot-path report needs at least one timing sample"
        );
        totals_ns.sort_unstable();
        let iterations = totals_ns.len() as u64;
        let p50_ns = totals_ns[totals_ns.len() / 2];
        let min_ns = *totals_ns.first().expect("non-empty");
        let max_ns = *totals_ns.last().expect("non-empty");
        Self {
            label,
            iterations,
            p50_ns,
            min_ns,
            max_ns,
            stages,
            allocations,
            process_allocations,
            env: PerfEnvironment::detect(),
        }
    }

    /// One-line key=value shape for humans, CI logs, and grep. Each stage is
    /// its own `stage.<name>_ns=<v>` field so the line stays flat and
    /// greppable.
    pub fn summary_line(&self) -> String {
        let mut line = format!(
            "hotpath label={} iterations={} p50_us={} p50_ns={} min_ns={} max_ns={} stage_count={} host_allocations={} process_allocations={} {}",
            report_value(self.label),
            self.iterations,
            self.p50_ns / 1_000,
            self.p50_ns,
            self.min_ns,
            self.max_ns,
            self.stages.len(),
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
            "{{\"schema\":\"tina.hotpath.v1\",\"label\":{},\"iterations\":{},\"p50_ns\":{},\"min_ns\":{},\"max_ns\":{},\"stage_count\":{},\"host_allocations\":{},\"process_allocations\":{},\"allocations\":{},\"platform\":{},\"arch\":{},\"profile\":{},\"git_sha\":{},\"stages\":{}}}",
            json_string(self.label),
            self.iterations,
            self.p50_ns,
            self.min_ns,
            self.max_ns,
            self.stages.len(),
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
    String::from_utf8_lossy(&output.stdout).trim().to_string()
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
            json.contains("\"schema\":\"tina.perf_report.v1\""),
            "{json}"
        );
        assert!(json.contains("\"label\":\"needs quoting\""), "{json}");
        assert!(json.contains("\"comparison_baseline\":\"none\""), "{json}");
        assert!(json.contains("\"pressure_total\":2"), "{json}");
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
        assert!(line.contains("stage_count=2"), "{line}");
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
        assert!(json.contains("\"schema\":\"tina.hotpath.v1\""), "{json}");
        assert!(json.contains("\"p50_ns\":1000"), "{json}");
        assert!(json.contains("\"stage_count\":2"), "{json}");
        assert!(
            json.contains("\"host_submit_to_worker_pickup\":250"),
            "{json}"
        );
        assert!(json.contains("\"allocations\":7"), "{json}");
    }
}
