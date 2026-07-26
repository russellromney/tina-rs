//! Public runner proof for the memory-tier runner (benchmark control).
//!
//! The crate is binary-only, so both tests spawn the documented binary
//! path (`cargo run --manifest-path examples/specimen_mem_run/Cargo.toml --
//! <comparison-manifest> <tiers-mb>`) against the README's default
//! comparison, `specimen_real_io_chat`. Characterization pins the runner's
//! output contract: the header line, the platform-mode note, the baseline
//! result line, and one tier result line per requested tier in order. The
//! wrapped comparison's own stdout is inherited by the runner, so its
//! lines interleave with the runner's; the assertions below filter on the
//! runner's `result label=` lines. Duration values are wall-clock, so
//! only their parseability is pinned.

use std::path::PathBuf;
use std::process::Command;

const BIN: &str = env!("CARGO_BIN_EXE_specimen-mem-run");

/// The README's default comparison manifest, resolved absolutely because
/// the test process working directory is the crate root, not the repo
/// root the binary's relative default assumes.
fn comparison_manifest() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../specimen_real_io_chat/Cargo.toml")
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

fn run_mem_run(tiers: &str) -> String {
    let output = Command::new(BIN)
        .arg(comparison_manifest())
        .arg(tiers)
        .output()
        .expect("spawn specimen-mem-run");
    assert!(
        output.status.success(),
        "specimen-mem-run failed: status={:?}\nstdout:\n{}\nstderr:\n{}",
        output.status.code(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    String::from_utf8(output.stdout).expect("specimen-mem-run stdout is utf-8")
}

/// Splits `result label=... limit_mb=... duration_ms=<n> status=<s>` into
/// (limit_mb, status). `duration_ms` must parse as an integer but its
/// value is wall-clock and not pinned.
fn parse_result_line(line: &str) -> (String, String) {
    let rest = line
        .strip_prefix("result label=")
        .unwrap_or_else(|| panic!("result line prefix, got {line:?}"));
    let limit = rest
        .split("limit_mb=")
        .nth(1)
        .and_then(|s| s.split_whitespace().next())
        .unwrap_or_else(|| panic!("limit_mb field, got {line:?}"));
    rest.split("duration_ms=")
        .nth(1)
        .and_then(|s| s.split_whitespace().next())
        .unwrap_or_else(|| panic!("duration_ms field, got {line:?}"))
        .parse::<u128>()
        .unwrap_or_else(|_| panic!("duration_ms parses as integer, got {line:?}"));
    let status = rest
        .split("status=")
        .nth(1)
        .unwrap_or_else(|| panic!("status field, got {line:?}"));
    (limit.to_string(), status.to_string())
}

/// Pins the full output contract for one run over `tiers`.
fn assert_run_contract(tiers: &[u64]) {
    let tiers_arg = tiers
        .iter()
        .map(u64::to_string)
        .collect::<Vec<_>>()
        .join(",");
    let stdout = run_mem_run(&tiers_arg);
    let mut lines = stdout.lines();

    let header = lines.next().expect("header line");
    assert_eq!(
        header,
        format!(
            "comparison=specimen_mem_run manifest=specimen_real_io_chat tiers_mb={tiers:?} platform={}",
            platform_label(),
        ),
        "header line must name the comparison, echo the tier list, and report the platform mode",
    );

    let note = lines.next().expect("line after header");
    if cfg!(target_os = "linux") {
        assert!(
            !note.starts_with("note: RLIMIT_AS"),
            "Linux applies the cap, so no best-effort note may print: {note:?}",
        );
    } else {
        assert!(
            note.starts_with("note: RLIMIT_AS is only applied on Linux."),
            "non-Linux runs must print the no-cap note, got {note:?}",
        );
    }

    // The wrapped comparison's stdout is inherited, so its lines
    // interleave with the runner's. Both sides of the comparison must
    // have run (tokio and tina) across the warmup/baseline/tier passes.
    assert!(
        stdout.contains("comparison=specimen_real_io_chat side=tokio"),
        "the wrapped tokio side ran: {stdout:?}",
    );
    assert!(
        stdout.contains("comparison=specimen_real_io_chat side=tina"),
        "the wrapped tina side ran: {stdout:?}",
    );

    let results: Vec<&str> = stdout
        .lines()
        .filter(|l| l.starts_with("result label="))
        .collect();
    assert_eq!(
        results.len(),
        tiers.len() + 1,
        "one baseline plus one result line per tier: {results:?}",
    );

    let baseline = results[0];
    assert!(
        baseline.starts_with("result label=baseline   limit_mb=- duration_ms="),
        "baseline line shape, got {baseline:?}",
    );
    let (limit, status) = parse_result_line(baseline);
    assert_eq!(limit, "-", "baseline runs uncapped");
    assert_eq!(
        status, "ok",
        "baseline must pass for tier results to be valid"
    );

    for (line, tier) in results[1..].iter().zip(tiers) {
        assert!(
            line.starts_with(&format!(
                "result label=tier       limit_mb={tier} duration_ms="
            )),
            "tier line shape for limit_mb={tier}, got {line:?}",
        );
        let (limit, status) = parse_result_line(line);
        assert_eq!(limit, tier.to_string());
        if cfg!(target_os = "linux") {
            // Under a real RLIMIT_AS cap a tier failure may be a correct
            // overload signal (the README says so), so only the line
            // structure is pinned on Linux.
            assert!(
                status == "ok" || status.starts_with("fail("),
                "tier status is ok or a typed fail(...), got {status:?}",
            );
        } else {
            assert_eq!(
                status, "ok",
                "no cap is applied off-Linux, so tiers must pass"
            );
        }
    }
}

/// Documented public runner path: the `specimen-mem-run` binary against
/// the README's default comparison. One tier keeps the control fast.
#[test]
fn public_smoke() {
    assert_run_contract(&[512]);
}

/// Pins the workload facts with the README's default tier list:
/// header shape, platform note, clean baseline, and one in-order tier
/// line per requested cap.
#[test]
fn public_characterization() {
    assert_run_contract(&[512, 256, 128]);
}
