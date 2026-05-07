//! Threaded runtime and supervise error types extracted from lib.rs (phase 055).

use std::error::Error;
use std::fmt;

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

impl fmt::Display for ThreadedRuntimeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::WorkerStopped => {
                write!(
                    f,
                    "worker thread stopped before it could process the command"
                )
            }
            Self::UnknownShard(shard) => {
                write!(
                    f,
                    "shard {} is not owned by this multi-shard runtime",
                    shard.get()
                )
            }
            Self::DriverShutdownFailed => {
                write!(
                    f,
                    "driver shutdown failed: completion-slot ownership not released"
                )
            }
        }
    }
}

impl Error for ThreadedRuntimeError {}

/// Error returned by [`crate::Runtime::try_supervise`] and the threaded equivalents.
///
/// Replaces a panic on unknown / stale parent registration
/// in `Runtime::supervise` so the explicit-step and threaded surfaces both
/// have a fallible variant. The panicking [`crate::Runtime::supervise`] is kept
/// for setup-time assertions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SuperviseError {
    /// The address did not name a parent registered with this runtime
    /// (unknown isolate id, stale generation, or wrong shard).
    UnknownParent,
}

impl fmt::Display for SuperviseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnknownParent => write!(
                f,
                "supervise target is not a parent registered with this runtime"
            ),
        }
    }
}

impl Error for SuperviseError {}

/// Error returned by [`crate::ThreadedRuntime::try_send`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThreadedTrySendError {
    /// The bounded worker ingress queue is full.
    IngressFull,

    /// The worker thread stopped before it could accept the ingress command.
    WorkerStopped,
}

impl fmt::Display for ThreadedTrySendError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::IngressFull => write!(f, "worker ingress queue is full"),
            Self::WorkerStopped => write!(f, "worker thread stopped before ingress was accepted"),
        }
    }
}

impl Error for ThreadedTrySendError {}

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

impl fmt::Display for ThreadedSendObservedError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::IngressFull => write!(f, "worker ingress queue is full"),
            Self::MailboxFull => write!(f, "target isolate mailbox is full"),
            Self::MailboxClosed => write!(f, "target isolate mailbox is closed or stale"),
            Self::WorkerStopped => {
                write!(f, "worker thread stopped before the send could be observed")
            }
        }
    }
}

impl Error for ThreadedSendObservedError {}
