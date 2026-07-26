//! Public runner proof for the CPU contention runner specimen.
//!
//! The runner is binary-only: it builds a wrapped comparison, warms it
//! once, then times a baseline (0 spinners) and a contended (N
//! spinners) run. Characterization pins that workload shape — three
//! wrapped invocations, one baseline and one contended result line,
//! and the intercepted pressure lines re-emitted per labelled run.
//! Public smoke exercises the documented binary path against the small
//! real-io-chat comparison with a single spinner so the proof stays
//! short.

use std::path::PathBuf;
use std::process::{Command, Output};

fn run_cpu_runner() -> Output {
    // The default wrapped comparison, kept small: real-io-chat is a
    // fixed-burst loopback TCP chat with a fast clean exit.
    let manifest =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../specimen_real_io_chat/Cargo.toml");
    Command::new(env!("CARGO_BIN_EXE_specimen-cpu-run"))
        .arg(&manifest)
        .arg("1")
        .output()
        .expect("run specimen-cpu-run binary")
}

fn assert_ok_output(output: &Output) -> String {
    assert!(
        output.status.success(),
        "cpu runner failed: {}",
        String::from_utf8_lossy(&output.stderr),
    );
    String::from_utf8_lossy(&output.stdout).into_owned()
}

/// Documented public runner path:
/// `cargo run --manifest-path examples/specimen_cpu_run/Cargo.toml -- [comparison-manifest] [spinner-count]`.
#[test]
fn public_smoke() {
    let stdout = assert_ok_output(&run_cpu_runner());
    assert!(
        stdout.contains("comparison=specimen_cpu_run manifest=specimen_real_io_chat spinners=1"),
        "runner must label the wrapped comparison and spinner count, got:\n{stdout}",
    );
    let baseline = stdout
        .lines()
        .filter(|line| line.contains("label=baseline"))
        .count();
    let contended = stdout
        .lines()
        .filter(|line| line.contains("label=contended"))
        .count();
    assert_eq!(
        (baseline, contended),
        (1, 1),
        "exactly one baseline and one contended result line:\n{stdout}",
    );
    assert!(
        stdout.contains("status=ok"),
        "both labelled runs must pass, got:\n{stdout}",
    );
}

/// Pins the benchmark workload/count facts.
#[test]
fn public_characterization() {
    let stdout = assert_ok_output(&run_cpu_runner());

    // The wrapped comparison runs exactly three times: one warmup plus
    // the labelled baseline and contended runs.
    for side in ["side=tokio", "side=tina"] {
        let marker = format!("comparison=specimen_real_io_chat {side}");
        assert_eq!(
            stdout.matches(marker.as_str()).count(),
            3,
            "warmup + baseline + contended each print one {side} line:\n{stdout}",
        );
    }

    // Baseline is always spinner-free; contended carries the requested
    // spinner count; both must exit ok.
    let baseline_line = stdout
        .lines()
        .find(|line| line.contains("label=baseline"))
        .expect("baseline result line");
    assert!(baseline_line.contains("spinners=0"), "{baseline_line}");
    assert!(baseline_line.contains("status=ok"), "{baseline_line}");
    let contended_line = stdout
        .lines()
        .find(|line| line.contains("label=contended"))
        .expect("contended result line");
    assert!(contended_line.contains("spinners=1"), "{contended_line}");
    assert!(contended_line.contains("status=ok"), "{contended_line}");

    // The runner intercepts the wrapped comparison's `pressure ...`
    // lines and re-emits them under each labelled run (the warmup run's
    // are discarded): one per side per labelled run.
    for label in ["baseline", "contended"] {
        let prefix = format!("{label} pressure ");
        assert_eq!(
            stdout
                .lines()
                .filter(|line| line.starts_with(prefix.as_str()))
                .count(),
            2,
            "one intercepted pressure line per side under {label}:\n{stdout}",
        );
    }
}
