//! Process-run rail constructor.

use std::time::Duration;

use super::{CallInput, CallOutput, ProcessRunResult, TypedCall};

/// Returns a typed bounded process-run helper.
pub fn process_run(
    command: impl Into<String>,
    args: Vec<String>,
    timeout: Duration,
    stdout_limit: usize,
    stderr_limit: usize,
) -> TypedCall<ProcessRunResult> {
    TypedCall::new(
        CallInput::ProcessRun {
            command: command.into(),
            args,
            timeout,
            stdout_limit,
            stderr_limit,
        },
        CallOutput::into_process_exited,
    )
}
