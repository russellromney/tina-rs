//! Threaded runtime and supervise error types extracted from lib.rs (phase 055).

use tina::ShardId;

/// Error returned by setup/control operations on [`crate::ThreadedRuntime`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThreadedRuntimeError {
    /// The worker thread stopped before it could accept or answer the command.
    WorkerStopped,
    /// A multi-shard owner operation targeted a shard this local system does
    /// not own.
    UnknownShard(ShardId),
    /// The worker could not prove backend completion-slot ownership was
    /// released during shutdown.
    DriverShutdownFailed,
}

/// Error returned by [`crate::Runtime::try_supervise`] and the threaded equivalents.
///
/// Phase 047 Rock 8: replaces a panic on unknown / stale parent registration
/// in `Runtime::supervise` so the explicit-step and threaded surfaces both
/// have a fallible variant. The panicking [`crate::Runtime::supervise`] is kept
/// for setup-time assertions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SuperviseError {
    /// The address did not name a parent registered with this runtime
    /// (unknown isolate id, stale generation, or wrong shard).
    UnknownParent,
}

/// Error returned by [`crate::ThreadedRuntime::try_send`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThreadedTrySendError {
    /// The bounded worker ingress queue is full.
    IngressFull,

    /// The worker thread stopped before it could accept the ingress command.
    WorkerStopped,
}

/// Error returned by [`crate::ThreadedRuntime::send_and_observe`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThreadedSendObservedError {
    /// The bounded worker ingress queue is full.
    IngressFull,

    /// The target isolate mailbox is full.
    MailboxFull,

    /// The target isolate is closed or stale.
    MailboxClosed,

    /// The worker thread stopped before the send could be observed.
    WorkerStopped,
}
