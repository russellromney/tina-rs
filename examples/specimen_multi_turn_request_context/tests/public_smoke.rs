//! Public runner proof for the multi-turn request-context specimen.
//!
//! Characterization pins caller-authority-across-turns behavior: ready
//! when both deferred dependencies answer, not_ready on either timeout.
//! Public smoke exercises the documented binary runner path.

use std::process::Command;

use specimen_multi_turn_request_context::{TinaConfig, tina_run};

fn config(probe_delay_ms: u64, db_delay_ms: u64) -> TinaConfig {
    TinaConfig {
        probe_delay_ms,
        db_delay_ms,
    }
}

/// README runner path: `cargo run ... -- tina` prints the readiness
/// replies and exits 0.
#[test]
fn public_smoke() {
    let out = Command::new(env!("CARGO_BIN_EXE_specimen-multi-turn-request-context"))
        .arg("tina")
        .output()
        .expect("run tina binary");
    assert!(
        out.status.success(),
        "tina runner failed: {}",
        String::from_utf8_lossy(&out.stderr),
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains(r#"tina: ["ready"]"#),
        "tina runner must report ready for the default-fast config, got:\n{stdout}",
    );
}

/// Pins caller authority across turns: the deferred two-step readiness
/// flow answers the original caller with the exact readiness outcome.
#[test]
fn public_characterization() {
    let report = tina_run(config(10, 10)).expect("tina side ran");
    assert_eq!(report.replies, ["ready"]);

    let report = tina_run(config(60, 10)).expect("probe-timeout tina side ran");
    assert_eq!(report.replies, ["not_ready"]);

    let report = tina_run(config(10, 60)).expect("db-timeout tina side ran");
    assert_eq!(report.replies, ["not_ready"]);
}
