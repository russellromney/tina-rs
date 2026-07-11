//! Public-path proof for the README bounded-mailbox workflow.

#[allow(dead_code)]
#[path = "../examples/bounded_mailbox.rs"]
mod bounded_mailbox;

use bounded_mailbox::{Job, ScenarioReport, run_scenario};

const README: &str = include_str!("../../README.md");
const EXAMPLE: &str = include_str!("../examples/bounded_mailbox.rs");
const README_SOURCE_MARKER: &str = "<!-- bounded-mailbox-source -->";
const EXPECTED_TRANSCRIPT: &str = "send Run(3) -> Full(Run(3)); host retains the job\n\
retry Run(3) after one step -> Accepted\n\
send Run(4) after stop -> Closed(Run(4)); host retains the job";

#[test]
fn checked_example_returns_exact_messages_and_transcript() {
    let report = run_scenario();
    assert_eq!(
        report,
        ScenarioReport {
            rejected: Job::Run(3),
            retried: Job::Run(3),
            closed: Job::Run(4),
        }
    );
    assert_eq!(report.to_string(), EXPECTED_TRANSCRIPT);
}

#[test]
fn readme_program_is_the_checked_example_source() {
    let after_marker = README
        .split_once(README_SOURCE_MARKER)
        .expect("README bounded-mailbox marker")
        .1;
    let rust_block = after_marker
        .strip_prefix("\n```rust\n")
        .expect("Rust fence immediately follows marker")
        .split_once("\n```\n")
        .expect("README bounded-mailbox closing fence")
        .0;
    let source = &EXAMPLE[EXAMPLE
        .find("use std::convert::Infallible;")
        .expect("example source start")..];

    assert_eq!(rust_block.trim(), source.trim());
}
