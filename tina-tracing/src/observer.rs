//! [`TraceObserver`] that emits one `tracing::Event` per recorded
//! `RuntimeEvent`. Hook rules: [`tina_runtime::TraceObserver`].
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

use tina_runtime::{RuntimeEvent, TraceObserver};

use crate::events::emit_event;

/// Stateless [`TraceObserver`] that calls [`emit_event`] per record.
#[derive(Debug, Clone, Copy, Default)]
pub struct TracingObserver;

impl TracingObserver {
    /// New observer. No state.
    pub const fn new() -> Self {
        Self
    }
}

impl TraceObserver for TracingObserver {
    fn on_event(&self, event: &RuntimeEvent) {
        emit_event(event);
    }
}
