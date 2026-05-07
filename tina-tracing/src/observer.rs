//! [`TraceObserver`] impl that turns each runtime event into a
//! `tracing::Event` via [`crate::events::emit_event`].
//!
//! Wire it on at runtime build time so no events fire before the
//! hook lands:
//!
//! ```ignore
//! use std::sync::Arc;
//! use tina_runtime::{ThreadedRuntime, ThreadedRuntimeConfig};
//! use tina_tracing::TracingObserver;
//!
//! let runtime = ThreadedRuntime::with_config_and_trace_observer(
//!     shard,
//!     factory,
//!     ThreadedRuntimeConfig::default(),
//!     Arc::new(TracingObserver::new()),
//! );
//! ```
//!
//! Hook contract is the runtime's. See
//! [`tina_runtime::TraceObserver`] for the rules.

use tina_runtime::{RuntimeEvent, TraceObserver};

use crate::events::emit_event;

/// Runtime trace observer that emits one structured `tracing::Event`
/// per recorded `RuntimeEvent`.
///
/// Stateless. `Clone` and `Default` are derived so callers can park
/// it in a `static` if they want.
#[derive(Debug, Clone, Copy, Default)]
pub struct TracingObserver;

impl TracingObserver {
    /// Builds a new observer. Cheap; no state.
    pub const fn new() -> Self {
        Self
    }
}

impl TraceObserver for TracingObserver {
    fn on_event(&self, event: &RuntimeEvent) {
        emit_event(event);
    }
}
