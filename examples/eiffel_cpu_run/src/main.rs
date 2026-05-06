//! Eiffel CPU contention runner.
//!
//! Wraps an existing eiffel comparison and runs it twice: once unloaded
//! (baseline) and once with N busy-spinner threads competing for CPU
//! (contended). Prints wall-clock duration and exit status for each run so
//! we can see whether Tina-shaped services degrade differently from
//! Tokio-shaped ones under scheduler pressure.
//!
//! This is a macOS-friendly approximation of "CPU quota" — real Linux/Fly
//! runs should use cgroups/taskset/cpulimit instead.
//!
//! Usage:
//!   eiffel-cpu-run [comparison-manifest] [spinner-count]
//!
//! Defaults: `examples/eiffel_real_io_chat/Cargo.toml` and
//! `2 * num_cpus().saturating_sub(1)` spinners.

use std::path::PathBuf;
use std::process::{Command, ExitStatus};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

fn main() {
    let mut args = std::env::args().skip(1);
    let manifest = args
        .next()
        .map(PathBuf::from)
        .unwrap_or_else(default_target_manifest);
    let spinner_count: usize = args
        .next()
        .map(|raw| raw.parse().expect("spinner count is an integer"))
        .unwrap_or_else(default_spinner_count);

    let manifest_label = manifest
        .components()
        .rev()
        .nth(1)
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
        .unwrap_or_else(|| manifest.display().to_string());

    println!(
        "comparison=eiffel_cpu_run manifest={} spinners={}",
        manifest_label, spinner_count
    );

    let target_binary = build_target(&manifest);

    // Warm the binary (page cache, dyld linker cache, JIT-style first-run
    // costs). The runner is a discovery tool, not a precise benchmark, but
    // without warmup the cold-cache first run dominates and makes the two
    // labelled runs incomparable.
    let _warmup = run_target(&target_binary, 0);
    let baseline = run_target(&target_binary, 0);
    let contended = run_target(&target_binary, spinner_count);

    println!(
        "result label=baseline   spinners=0  duration_ms={} status={} (manifest={})",
        baseline.duration_ms,
        format_status(baseline.status),
        manifest_label,
    );
    println!(
        "result label=contended  spinners={}  duration_ms={} status={} (manifest={})",
        spinner_count,
        contended.duration_ms,
        format_status(contended.status),
        manifest_label,
    );

    let baseline_passed = baseline.status.success();
    let contended_passed = contended.status.success();
    assert!(
        baseline_passed,
        "baseline run failed; CPU contention run is invalid without a clean baseline"
    );
    assert!(
        contended_passed,
        "contended run failed; the comparison did not survive {spinner_count} \
         CPU spinners — this is the kind of thing the runner is supposed to \
         find, but a hard fail makes the run unusable for now",
    );
}

struct RunResult {
    duration_ms: u128,
    status: ExitStatus,
}

fn run_target(binary: &PathBuf, spinner_count: usize) -> RunResult {
    let stop = Arc::new(AtomicBool::new(false));
    let spinners: Vec<_> = (0..spinner_count)
        .map(|_| {
            let stop = Arc::clone(&stop);
            thread::spawn(move || {
                let mut acc: u64 = 0;
                while !stop.load(Ordering::Relaxed) {
                    for i in 0..1024 {
                        acc = acc.wrapping_add(i);
                    }
                    std::hint::black_box(acc);
                }
            })
        })
        .collect();

    let started = Instant::now();
    let status = Command::new(binary)
        .arg("compare")
        .status()
        .expect("spawn target comparison");
    let duration = started.elapsed();

    stop.store(true, Ordering::Relaxed);
    for spinner in spinners {
        let _ = spinner.join();
    }

    // Sleep for one tick to let any IPC settle.
    thread::sleep(Duration::from_millis(10));

    RunResult {
        duration_ms: duration.as_millis(),
        status,
    }
}

fn build_target(manifest: &PathBuf) -> PathBuf {
    let status = Command::new("cargo")
        .arg("build")
        .arg("--manifest-path")
        .arg(manifest)
        .arg("--message-format=json")
        .stdout(std::process::Stdio::piped())
        .spawn()
        .expect("spawn cargo build");
    let output = status.wait_with_output().expect("await cargo build output");
    assert!(
        output.status.success(),
        "cargo build of target comparison failed",
    );
    let mut binary: Option<PathBuf> = None;
    for line in output.stdout.split(|&b| b == b'\n') {
        let text = std::str::from_utf8(line).unwrap_or_default();
        if let Some(path) = extract_executable_path(text) {
            binary = Some(path);
        }
    }
    binary.expect("cargo build emitted at least one executable artifact")
}

fn extract_executable_path(line: &str) -> Option<PathBuf> {
    let needle = "\"executable\":\"";
    let start = line.find(needle)? + needle.len();
    let rest = &line[start..];
    let end = rest.find('"')?;
    Some(PathBuf::from(&rest[..end]))
}

fn default_target_manifest() -> PathBuf {
    PathBuf::from("examples/eiffel_real_io_chat/Cargo.toml")
}

fn default_spinner_count() -> usize {
    let cores = thread::available_parallelism()
        .map(std::num::NonZeroUsize::get)
        .unwrap_or(1);
    cores.saturating_mul(2).saturating_sub(1).max(1)
}

fn format_status(status: ExitStatus) -> String {
    if status.success() {
        "ok".to_string()
    } else {
        format!("fail({:?})", status.code())
    }
}
