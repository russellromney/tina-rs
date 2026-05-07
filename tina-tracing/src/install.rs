//! Optional convenience for examples and quick demos.
//!
//! Production services should construct their own
//! [`tracing_subscriber`] stack (filter, formatter, exporter). This
//! function exists so a small example or doctest can install a default
//! formatting subscriber in one line.
//!
//! Naming rule: this file is the *only* place where this crate touches
//! the global subscriber, and the verb is in the function name.

use tracing::dispatcher::SetGlobalDefaultError;
use tracing_subscriber::FmtSubscriber;

/// Installs a basic `FmtSubscriber` as the global default.
///
/// Idempotent across this crate but not across the process: if another
/// subscriber has already been installed, the call returns the
/// underlying [`SetGlobalDefaultError`].
pub fn install_global_default_subscriber() -> Result<(), SetGlobalDefaultError> {
    tracing::subscriber::set_global_default(FmtSubscriber::new())
}
