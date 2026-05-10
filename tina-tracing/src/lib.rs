#![forbid(unsafe_code)]
#![deny(missing_docs)]
#![deny(rustdoc::broken_intra_doc_links)]

//! Adapter from [`tina_runtime`] trace events and live reports to
//! [`tracing`] events. Boring shim. No flattening.
//!
//! # Rule
//!
//! ```text
//! ergonomics may surface truth.
//! ergonomics may not flatten truth.
//! ```
//!
//! - Every typed reason (`Full`, `Closed`, `Timeout`, `CallerClosed`,
//!   `ResourceClosed`, `ReplyPathFull`, `RequesterShardClosed`,
//!   `MailboxFull`, `RequesterClosed`, `NoPendingCall`, `TypeMismatch`,
//!   `BudgetExceeded`, `SupervisorStopped`, `NotRestartable`) is a
//!   stable string in `reason`. Never collapsed into a generic `error`.
//! - `event_id`, `cause_id`, `call_id`, `slot_id`, `isolate`,
//!   `generation`, `child_isolate`, `record_index` are correlation
//!   fields. Unbounded cardinality. **Do not** use them as metric
//!   labels.
//!
//! # Levels
//!
//! - `TRACE`: lifecycle and dispatch (mailbox accept, handler
//!   start/finish, sends, call dispatch/complete, deferred slots,
//!   journal append).
//! - `DEBUG`: benign lifecycle (`IsolateStopped`,
//!   `RestartChildSkipped`, `SupervisorRestartTriggered`).
//! - `WARN`: visible rejections. `Closed` is lifecycle truth, not an
//!   error; the operator decides what to alert on.
//! - `ERROR`: `HandlerPanicked`, `RecoveryFailed`, `CallFailed`.
//!
//! # Side effects
//!
//! No function installs a global subscriber unless the name says
//! `install_global_*`. Only `install_global_default_subscriber`
//! does, behind the `subscriber` feature.
//!
//! # Layout
//!
//! - [`events`] — per-event emission.
//! - [`live`] — per-snapshot emission.
//!
//! # Live wiring (preferred)
//!
//! `TracingObserver` is a [`tina_runtime::TraceObserver`]. Wire it at
//! build time and every event flows into the subscriber as it happens.
//! See `eiffel_tracing_demo` for a runnable version.
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
//! # End-of-run dump
//!
//! For tests and tools. Sample fmt-subscriber output for the event below:
//!
//! ```text
//! WARN tina_runtime::trace: kind="send_rejected" event_id=1 cause_id=- shard=0 isolate=7
//!     target_shard=0 target_isolate=8 target_generation=0 reason="Full"
//! ```
//!
//! ```
//! use tina_runtime::{EventId, RuntimeEvent, RuntimeEventKind, SendRejectedReason};
//! use tina::{AddressGeneration, IsolateId, ShardId};
//!
//! let events = vec![
//!     RuntimeEvent::new(
//!         EventId::new(1),
//!         None,
//!         ShardId::new(0),
//!         IsolateId::new(7),
//!         RuntimeEventKind::SendRejected {
//!             target_shard: ShardId::new(0),
//!             target_isolate: IsolateId::new(8),
//!             target_generation: AddressGeneration::new(0),
//!             reason: SendRejectedReason::Full,
//!         },
//!     ),
//! ];
//! tina_tracing::emit_events(events.iter());
//! ```

pub mod events;
pub mod live;
mod observer;

pub use observer::TracingObserver;

#[cfg(feature = "subscriber")]
mod install;

#[cfg(feature = "subscriber")]
pub use install::install_global_default_subscriber;

// Stable-name helpers and entry points re-exported at the root so
// callers don't need to reach through submodule paths.
pub use events::{
    call_completion_rejected_reason_name, call_error_name, call_kind_name,
    call_reply_rejected_reason_name, cancel_cause_name, deferred_reply_rejected_reason_name,
    effect_kind_name, emit_event, emit_events, emit_partial_marker, emit_trace_snapshot,
    restart_skipped_reason_name, send_rejected_reason_name,
};
pub use live::{affinity_status_name, emit_snapshot, shard_state_name};

/// Target for every runtime trace event this crate emits.
pub const RUNTIME_TRACE_TARGET: &str = "tina_runtime::trace";

/// Target for every live-topology snapshot event this crate emits.
pub const LIVE_TOPOLOGY_TARGET: &str = "tina_runtime::live";
