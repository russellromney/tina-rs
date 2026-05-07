#![forbid(unsafe_code)]
#![deny(missing_docs)]
#![deny(rustdoc::broken_intra_doc_links)]

//! Adapter from `tina-runtime` trace events and live reports to
//! [`tracing`] events.
//!
//! Tina already records the right facts: every [`RuntimeEvent`] carries
//! shard, isolate, generation, call id, kind, and a *typed* reason.
//! `LiveTopologyReport` carries per-shard state and per-queue
//! accepted/full/closed counts. This crate is the boring shim that turns
//! those typed values into structured `tracing` events without flattening
//! them.
//!
//! [`RuntimeEvent`]: tina_runtime::RuntimeEvent
//!
//! # Rule
//!
//! ```text
//! ergonomics may surface truth.
//! ergonomics may not flatten truth.
//! ```
//!
//! - Every typed reason — `Full`, `Closed`, `Timeout`, `CallerClosed`,
//!   `ResourceClosed`, `ReplyPathFull`, `RequesterShardClosed`,
//!   `MailboxFull`, `RequesterClosed`, `NoPendingCall`, `TypeMismatch`,
//!   `BudgetExceeded`, `SupervisorStopped`, `NotRestartable` — is emitted
//!   as a stable string in the `reason` field. They are never collapsed
//!   into a generic `error`.
//! - `event_id`, `cause_id`, `call_id`, `slot_id`, `isolate`,
//!   `generation`, `child_isolate`, `record_index` are **correlation**
//!   fields. They are unbounded cardinality. **Do not** turn them into
//!   metric labels in your subscriber.
//!
//! # Levels
//!
//! - `TRACE` for normal lifecycle and dispatch (mailbox accept, handler
//!   start/finish, send accept, call dispatch, call complete, deferred
//!   reply capture/send/drop, journal append).
//! - `DEBUG` for benign lifecycle (`IsolateStopped`,
//!   `RestartChildSkipped`, `SupervisorRestartTriggered`).
//! - `WARN` for visible rejections (`SendRejected`,
//!   `CallCompletionRejected`, `CallReplyRejected`,
//!   `DeferredReplyRejected`, `SupervisorRestartRejected`,
//!   `JournalAppendFailed`, `SnapshotCommitFailed`). `Closed` is benign
//!   lifecycle truth, not an error; it stays at `WARN` so the *operator*
//!   decides what to alert on.
//! - `ERROR` for `HandlerPanicked`, `RecoveryFailed`, `CallFailed`.
//!
//! # Side effects
//!
//! No function in this crate installs a global subscriber unless its
//! name explicitly says `install_global_*`. The only such function is
//! [`install_global_default_subscriber`], gated behind the `subscriber`
//! Cargo feature.
//!
//! # Layout
//!
//! - [`events`] — per-`RuntimeEvent` emission.
//! - [`live`] — per-`LiveTopologyReport` snapshot emission.
//!
//! # Example
//!
//! ```
//! use tina_runtime::{
//!     CauseId, EventId, RuntimeEvent, RuntimeEventKind, SendRejectedReason,
//! };
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
//! tina_tracing::events::emit_events(events.iter());
//! ```

pub mod events;
pub mod live;
mod observer;

pub use observer::TracingObserver;

#[cfg(feature = "subscriber")]
mod install;

#[cfg(feature = "subscriber")]
pub use install::install_global_default_subscriber;

// Re-exports of the stable-name helpers and per-event entry points so
// callers wiring fields outside the adapter (e.g. correlating bridge
// spans against runtime trace events) can pick up the same vocabulary
// without reaching into a submodule path.
pub use events::{
    call_completion_rejected_reason_name, call_error_name, call_kind_name,
    call_reply_rejected_reason_name, deferred_reply_rejected_reason_name, effect_kind_name,
    emit_event, emit_events, emit_partial_marker, emit_trace_snapshot,
    restart_skipped_reason_name, send_rejected_reason_name,
};
pub use live::{affinity_status_name, emit_snapshot, shard_state_name};

/// Target string used for every Tina-runtime trace event emitted by
/// this crate.
///
/// Subscribers can filter on this target to pick up runtime events
/// without entangling them with bridge-internal spans.
pub const RUNTIME_TRACE_TARGET: &str = "tina_runtime::trace";

/// Target string used for every Tina live-topology snapshot event
/// emitted by this crate.
pub const LIVE_TOPOLOGY_TARGET: &str = "tina_runtime::live";
