//! Signal-wait rail constructor.

use std::time::Duration;

use super::{CallInput, CallOutput, TypedCall};

/// Returns a typed signal-wait helper.
pub fn signal_wait(name: impl Into<String>, timeout: Duration) -> TypedCall<String> {
    TypedCall::new(
        CallInput::SignalWait {
            name: name.into(),
            timeout,
        },
        CallOutput::into_signal_received,
    )
}
