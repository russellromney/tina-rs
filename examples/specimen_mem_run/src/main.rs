//! Specimen memory-tier runner.
//!
//! Wraps an existing specimen comparison and runs it under a process-level
//! `RLIMIT_AS` (address-space) cap, optionally for several tiers in
//! sequence. Reports duration and exit status per tier so we can see
//! whether Tina-shaped services degrade differently from Tokio-shaped
//! ones under constrained memory.
//!
//! Caveats — read these before believing the numbers:
//!
//! - On Linux this is a real cap; under-the-cap allocations will EAGAIN /
//!   the kernel will refuse mappings.
//! - On macOS `RLIMIT_AS` is best-effort and is not enforced for many
//!   classes of allocation. Use Linux/Fly/Docker for serious runs.
//! - The runner does not (yet) crash-replay or save logs from a
//!   tier-failure run; it only reports pass/fail + wall-clock.
//!
//! Usage:
//!   specimen-mem-run [comparison-manifest] [tiers-mb-comma-separated]
//!
//! Defaults: `examples/specimen_real_io_chat/Cargo.toml` and `512,256,128`.

use std::path::PathBuf;
use std::process::{Command, ExitStatus};
use std::time::Instant;

fn main() {
    let mut args = std::env::args().skip(1);
    let manifest = args
        .next()
        .map(PathBuf::from)
        .unwrap_or_else(default_target_manifest);
    let tiers_mb: Vec<u64> = args
        .next()
        .map(parse_tiers)
        .unwrap_or_else(|| vec![512, 256, 128]);

    let manifest_label = manifest
        .components()
        .rev()
        .nth(1)
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
        .unwrap_or_else(|| manifest.display().to_string());

    println!(
        "comparison=specimen_mem_run manifest={} tiers_mb={:?} platform={}",
        manifest_label,
        tiers_mb,
        platform_label(),
    );
    if !cfg!(target_os = "linux") {
        println!(
            "note: RLIMIT_AS is only applied on Linux. On this platform the runner \
             still spawns the comparison and times it, but no memory cap is enforced. \
             Real memory-tier runs should use Linux + cgroups or Docker."
        );
    }

    let target_binary = build_target(&manifest);

    // Warmup so cold-cache cost doesn't dominate the first labelled tier.
    let _warmup = run_target(&target_binary, None);

    let baseline = run_target(&target_binary, None);
    println!(
        "result label=baseline   limit_mb=- duration_ms={} status={}",
        baseline.duration_ms,
        format_status(baseline.status),
    );

    let mut any_failed_unexpectedly = false;
    for tier_mb in &tiers_mb {
        let tier = run_target(&target_binary, Some(*tier_mb));
        println!(
            "result label=tier       limit_mb={} duration_ms={} status={}",
            tier_mb,
            tier.duration_ms,
            format_status(tier.status),
        );
        if !tier.status.success() && cfg!(target_os = "linux") {
            any_failed_unexpectedly = true;
        }
    }

    assert!(
        baseline.status.success(),
        "baseline run failed; memory-tier results are invalid without a clean baseline",
    );
    if any_failed_unexpectedly {
        eprintln!(
            "note: one or more tiers failed under their RLIMIT_AS cap; that may be a \
             correct overload signal or a regression — inspect each comparison's logs."
        );
    }
}

struct RunResult {
    duration_ms: u128,
    status: ExitStatus,
}

fn run_target(binary: &PathBuf, limit_mb: Option<u64>) -> RunResult {
    let mut command = Command::new(binary);
    command.arg("compare");

    #[cfg(target_os = "linux")]
    if let Some(mb) = limit_mb {
        let bytes: libc::rlim_t =
            mb.checked_mul(1024 * 1024)
                .expect("memory limit fits in rlim_t") as libc::rlim_t;
        unsafe {
            use std::os::unix::process::CommandExt;
            command.pre_exec(move || {
                let limit = libc::rlimit {
                    rlim_cur: bytes,
                    rlim_max: bytes,
                };
                if libc::setrlimit(libc::RLIMIT_AS, &limit) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
    }
    #[cfg(not(target_os = "linux"))]
    let _ = limit_mb;

    let started = Instant::now();
    let status = command.status().expect("spawn target comparison");
    let duration = started.elapsed();
    RunResult {
        duration_ms: duration.as_millis(),
        status,
    }
}

fn build_target(manifest: &PathBuf) -> PathBuf {
    let output = Command::new("cargo")
        .arg("build")
        .arg("--manifest-path")
        .arg(manifest)
        .arg("--message-format=json")
        .stdout(std::process::Stdio::piped())
        .spawn()
        .expect("spawn cargo build")
        .wait_with_output()
        .expect("await cargo build output");
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
    PathBuf::from("examples/specimen_real_io_chat/Cargo.toml")
}

fn parse_tiers(raw: String) -> Vec<u64> {
    raw.split(',')
        .map(|chunk| chunk.trim().parse().expect("tier MB is a u64"))
        .collect()
}

fn platform_label() -> &'static str {
    if cfg!(target_os = "linux") {
        "linux"
    } else if cfg!(target_os = "macos") {
        "macos"
    } else if cfg!(unix) {
        "unix"
    } else {
        "non-unix"
    }
}

fn format_status(status: ExitStatus) -> String {
    if status.success() {
        "ok".to_string()
    } else {
        format!("fail({:?})", status.code())
    }
}
