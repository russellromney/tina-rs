//! Public runner proof for the cross-shard child-ownership specimen.
//!
//! The crate is binary-only: the documented runner prints two
//! `child_lifecycle` report lines. Characterization pins those lines
//! exactly — parent on shard 1, two children on shards [1, 2], both
//! stopped with no pending remote control. Public smoke exercises the
//! documented binary path.

use std::process::Command;

const LIVE_LINE: &str = "child_lifecycle specimen=cross_shard_child_ownership parent=1 children=2 shards=[1, 2] state=live";
const STOPPED_LINE: &str = "child_lifecycle specimen=cross_shard_child_ownership parent=1 stopped=2 pending_remote_control=0";

fn run_demo() -> String {
    let output = Command::new(env!("CARGO_BIN_EXE_specimen-cross-shard-child-ownership"))
        .output()
        .expect("run cross-shard child-ownership binary");
    assert!(
        output.status.success(),
        "cross-shard demo failed: {}",
        String::from_utf8_lossy(&output.stderr),
    );
    String::from_utf8_lossy(&output.stdout).into_owned()
}

/// Documented public runner path:
/// `cargo run --manifest-path examples/specimen_cross_shard_child_ownership/Cargo.toml`.
#[test]
fn public_smoke() {
    let stdout = run_demo();
    assert!(
        stdout.contains(LIVE_LINE),
        "live lifecycle line missing:\n{stdout}",
    );
    assert!(
        stdout.contains(STOPPED_LINE),
        "stopped lifecycle line missing:\n{stdout}",
    );
}

/// Pins the exact lifecycle report lines and their order.
#[test]
fn public_characterization() {
    let stdout = run_demo();
    let lines: Vec<&str> = stdout.lines().collect();
    assert_eq!(
        lines,
        [LIVE_LINE, STOPPED_LINE],
        "the demo prints exactly its two documented report lines"
    );
}
