//! Convenience for examples and quick demos. Production services
//! build their own subscriber stack.
//!
//! Only place this crate touches the global subscriber. Verb is in
//! the function name.

use tracing::dispatcher::SetGlobalDefaultError;
use tracing_subscriber::FmtSubscriber;

/// Installs a basic `FmtSubscriber` as the global default. Errors if
/// another subscriber is already installed in this process.
pub fn install_global_default_subscriber() -> Result<(), SetGlobalDefaultError> {
    tracing::subscriber::set_global_default(FmtSubscriber::new())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn install_returns_err_when_subscriber_already_installed() {
        // First call may succeed or fail depending on test order;
        // second must always return the typed error.
        let first = install_global_default_subscriber();
        let second = install_global_default_subscriber();
        assert!(
            second.is_err(),
            "second install must surface SetGlobalDefaultError, got {first:?} then {second:?}",
        );
    }
}
