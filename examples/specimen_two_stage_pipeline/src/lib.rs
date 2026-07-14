//! Three-stage pipeline. Each request walks parse → validate →
//! execute. The Tina side carries the original caller as a
//! `DeferredReply` through every stage; the Tokio side is one
//! async chain.
//!
//! Some inputs are intentionally malformed: stage one rejects them.
//! The smoke test asserts both sides bucket the same way.

pub mod tina_impl;
pub mod tokio_impl;

pub const REQUESTS: usize = 8;
pub const PARSE_FAILURES: usize = 2;
pub const VALIDATE_FAILURES: usize = 1;

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Report {
    pub requests: usize,
    pub completed: usize,
    pub parse_failed: usize,
    pub validate_failed: usize,
    pub exit_clean: bool,
    pub tina_terminals: Vec<PipelineTerminal>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PipelineTerminal {
    StageFull(PipelineStage),
    StageClosed(PipelineStage),
    StageTimeout(PipelineStage),
    StageRejected {
        stage: PipelineStage,
        reason: tina::CallRejectedReason,
    },
    OuterFull,
    OuterClosed,
    OuterTimeout,
    OuterRejected(tina::CallRejectedReason),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PipelineStage {
    Parse,
    Validate,
    Execute,
}

/// Inputs labeled by index. Index < `PARSE_FAILURES` parses fail;
/// next `VALIDATE_FAILURES` validate fail; rest execute cleanly.
pub fn classify(i: usize) -> Stage {
    if i < PARSE_FAILURES {
        Stage::ParseFailure
    } else if i < PARSE_FAILURES + VALIDATE_FAILURES {
        Stage::ValidateFailure
    } else {
        Stage::Ok
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stage {
    ParseFailure,
    ValidateFailure,
    Ok,
}

pub fn assert_report_invariants(side: &str, r: &Report) {
    assert_eq!(r.requests, REQUESTS, "{side}: {r:?}");
    assert_eq!(r.parse_failed, PARSE_FAILURES, "{side}: {r:?}");
    assert_eq!(r.validate_failed, VALIDATE_FAILURES, "{side}: {r:?}");
    assert_eq!(
        r.completed,
        REQUESTS - PARSE_FAILURES - VALIDATE_FAILURES,
        "{side}: {r:?}",
    );
    assert!(r.exit_clean, "{side}: {r:?}");
    assert!(
        r.tina_terminals.is_empty(),
        "{side}: no transport terminal is expected: {r:?}"
    );
}
