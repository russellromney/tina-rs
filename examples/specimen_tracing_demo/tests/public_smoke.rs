//! Public runner proof for the tracing demo.
//!
//! The crate is binary-only, so the tests spawn the documented binary
//! path (`cargo run --manifest-path examples/specimen_tracing_demo/Cargo.toml`,
//! plus the README's `RUST_LOG=tina_runtime=trace` variant for the trace
//! facts). Characterization pins the same accounting invariant the
//! crate's own unit tests pin — `delivered + timer_failures +
//! completion_mailbox_full == fanout` — and that runtime events reach
//! the fmt subscriber. The exact delivered/full split is
//! scheduler-dependent by design (the demo reports observed pressure
//! rather than assuming an interleaving), so it is not pinned exactly.

use std::process::Command;

const BIN: &str = env!("CARGO_BIN_EXE_specimen-tracing-demo");

struct Observed {
    fanout: u64,
    delivered: u64,
    timer_failures: u64,
    completion_mailbox_full: u64,
    completion_requester_closed: u64,
    stopped_with_result: bool,
}

fn run_demo(rust_log: Option<&str>) -> (String, String) {
    let mut command = Command::new(BIN);
    if let Some(filter) = rust_log {
        command.env("RUST_LOG", filter);
    }
    let output = command.output().expect("spawn specimen-tracing-demo");
    assert!(
        output.status.success(),
        "specimen-tracing-demo failed: status={:?}\nstdout:\n{}\nstderr:\n{}",
        output.status.code(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    (
        String::from_utf8(output.stdout).expect("stdout is utf-8"),
        String::from_utf8(output.stderr).expect("stderr is utf-8"),
    )
}

fn scalar_u64(line: &str, key: &str) -> u64 {
    let start = line
        .find(key)
        .unwrap_or_else(|| panic!("field {key:?} in {line:?}"))
        + key.len();
    let rest = &line[start..];
    let end = rest.find([',', '}']).unwrap_or(rest.len());
    rest[..end]
        .trim()
        .parse()
        .unwrap_or_else(|_| panic!("field {key:?} parses as u64 in {line:?}"))
}

/// Counts elements of a `key: [ ... ]` debug vec, tolerating nested
/// parens/brackets inside elements.
fn vec_len(line: &str, key: &str) -> u64 {
    let start = line
        .find(key)
        .unwrap_or_else(|| panic!("field {key:?} in {line:?}"))
        + key.len();
    let rest = &line[start..];
    let open = rest
        .find('[')
        .unwrap_or_else(|| panic!("field {key:?} opens a vec in {line:?}"));
    let mut depth = 0_i32;
    let mut items = 0_u64;
    let mut saw_element = false;
    for c in rest[open..].chars() {
        match c {
            '[' | '(' | '{' => depth += 1,
            ']' | ')' | '}' => {
                depth -= 1;
                if depth == 0 {
                    return items + u64::from(saw_element);
                }
            }
            ',' if depth == 1 => items += 1,
            c if depth == 1 && !c.is_whitespace() => saw_element = true,
            _ => {}
        }
    }
    panic!("field {key:?} vec closes in {line:?}")
}

fn parse_report(stderr: &str) -> Observed {
    assert!(
        stderr.contains("--- pressure summary ---"),
        "pressure banner on stderr, got {stderr:?}",
    );
    let line = stderr
        .lines()
        .find(|l| l.starts_with("Report {"))
        .unwrap_or_else(|| panic!("Report debug line on stderr, got {stderr:?}"));
    Observed {
        fanout: scalar_u64(line, "fanout: "),
        delivered: scalar_u64(line, "delivered: "),
        timer_failures: vec_len(line, "timer_failures: "),
        completion_mailbox_full: scalar_u64(line, "completion_mailbox_full: "),
        completion_requester_closed: scalar_u64(line, "completion_requester_closed: "),
        stopped_with_result: line.contains("stopped_with_result: true"),
    }
}

/// The accounting invariant the crate's own unit tests pin.
fn assert_report_facts(stderr: &str) {
    let report = parse_report(stderr);
    assert_eq!(report.fanout, 6, "DEFAULT_FANOUT");
    assert_eq!(
        report.delivered + report.timer_failures + report.completion_mailbox_full,
        report.fanout,
        "every fanned-out sleep completion is accounted for",
    );
    assert_eq!(report.completion_requester_closed, 0);
    assert!(report.stopped_with_result);
}

/// Documented public runner path: the `specimen-tracing-demo` binary,
/// plain invocation.
#[test]
fn public_smoke() {
    let (_stdout, stderr) = run_demo(None);
    assert_report_facts(&stderr);
}

/// Pins the report facts plus the trace facts under the README's
/// `RUST_LOG=tina_runtime=trace` variant: runtime events become
/// structured fmt-subscriber events with their typed `kind` field.
#[test]
fn public_characterization() {
    let (stdout, stderr) = run_demo(Some("tina_runtime=trace"));
    assert_report_facts(&stderr);
    assert!(
        stdout.contains("tina_runtime::trace"),
        "runtime trace events reach the fmt subscriber, got {stdout:?}",
    );
    assert!(
        stdout.contains("kind="),
        "typed event fields survive into the fmt output, got {stdout:?}",
    );
}
