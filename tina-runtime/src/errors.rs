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
    /// The bounded worker command queue could not accept the host-control
    /// command immediately. The host call did not block and no work was
    /// admitted; the caller can retry once the queue has drained.
    CommandFull,
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
            Self::CommandFull => {
                write!(
                    f,
                    "worker command queue is full; host-control command not admitted"
                )
            }
        }
    }
}

impl Error for ThreadedRuntimeError {}

/// Error returned by `register_with_capacity_and_bootstrap` (and its mirrors)
/// when the bootstrap message could not be prefilled into the newly allocated
/// mailbox before the isolate entry was inserted.
///
/// No isolate is registered and no address is returned. The bootstrap message
/// is returned to the caller so it can decide whether to retry with a larger
/// mailbox capacity or surface the failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RegisterBootstrapError<M> {
    /// The mailbox refused the bootstrap message because it was already full.
    /// This is impossible for a default mailbox of capacity >= 1 (the mailbox
    /// is empty before the prefill), but a user-supplied mailbox can still
    /// refuse. The message is returned untouched.
    Full(M),
    /// The mailbox refused the bootstrap message because it had already been
    /// closed. The message is returned untouched.
    Closed(M),
}

impl<M> fmt::Display for RegisterBootstrapError<M> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Full(_) => write!(
                f,
                "bootstrap prefill refused: mailbox full (capacity must be >= 1)"
            ),
            Self::Closed(_) => write!(
                f,
                "bootstrap prefill refused: mailbox already closed before registration"
            ),
        }
    }
}

impl<M: fmt::Debug> Error for RegisterBootstrapError<M> {}

/// Threaded mirror of [`RegisterBootstrapError`].
///
/// Adds [`Self::WorkerStopped`] so the threaded runtime surface can report a
/// command-channel failure separately from a typed mailbox-prefill refusal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ThreadedRegisterBootstrapError<M> {
    /// The mailbox refused the bootstrap message because it was already full.
    Full(M),
    /// The mailbox refused the bootstrap message because it had been closed.
    Closed(M),
    /// The worker thread stopped before it could process the command.
    WorkerStopped,
    /// A multi-shard operation targeted a shard this runtime does not own.
    UnknownShard(ShardId),
}

impl<M> fmt::Display for ThreadedRegisterBootstrapError<M> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Full(_) => write!(
                f,
                "bootstrap prefill refused: mailbox full (capacity must be >= 1)"
            ),
            Self::Closed(_) => write!(
                f,
                "bootstrap prefill refused: mailbox already closed before registration"
            ),
            Self::WorkerStopped => write!(
                f,
                "worker thread stopped before it could process register-and-bootstrap"
            ),
            Self::UnknownShard(shard) => write!(
                f,
                "shard {} is not owned by this multi-shard runtime",
                shard.get()
            ),
        }
    }
}

impl<M: fmt::Debug> Error for ThreadedRegisterBootstrapError<M> {}

impl<M> ThreadedRegisterBootstrapError<M> {
    pub(crate) fn from_register(err: RegisterBootstrapError<M>) -> Self {
        match err {
            RegisterBootstrapError::Full(m) => Self::Full(m),
            RegisterBootstrapError::Closed(m) => Self::Closed(m),
        }
    }
}

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

/// Error returned by [`crate::ThreadedRuntime::send_observed_until`].
///
/// Phase 062 Rock 4 retry helper. The helper retries on `MailboxFull`
/// and `IngressFull` until the caller-supplied deadline; a deadline
/// miss surfaces as [`Self::Timeout`]. `Closed` and `WorkerStopped`
/// are returned eagerly — the helper does not retry those because the
/// target/worker is no longer accepting at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SendObservedUntilError {
    /// Deadline elapsed while still racing the mailbox/ingress for a slot.
    Timeout,
    /// Target isolate mailbox reported closed/stale.
    Closed,
    /// Worker thread stopped before the send could be observed.
    WorkerStopped,
}

impl fmt::Display for SendObservedUntilError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Timeout => write!(f, "deadline elapsed before mailbox accepted the message"),
            Self::Closed => write!(f, "target isolate mailbox is closed or stale"),
            Self::WorkerStopped => {
                write!(f, "worker thread stopped before the send could be observed")
            }
        }
    }
}

impl Error for SendObservedUntilError {}

/// Error returned by [`crate::ThreadedShutdownHandle::request_shutdown`].
///
/// The handle must not block forever behind a full command queue. Both
/// variants name the offending shard on multi-shard runtimes; single-shard
/// runtimes use `None`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShutdownRequestError {
    /// The worker command queue could not accept the shutdown command
    /// immediately. The caller can retry once the queue has drained.
    CommandFull {
        /// Owning shard id on multi-shard runtimes; `None` on single-shard.
        shard: Option<ShardId>,
    },
    /// The worker thread had already stopped before the shutdown command
    /// could be enqueued.
    WorkerStopped {
        /// Owning shard id on multi-shard runtimes; `None` on single-shard.
        shard: Option<ShardId>,
    },
}

impl fmt::Display for ShutdownRequestError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::CommandFull { shard: Some(s) } => write!(
                f,
                "shard {} command queue is full; shutdown request not admitted",
                s.get()
            ),
            Self::CommandFull { shard: None } => {
                write!(f, "command queue is full; shutdown request not admitted")
            }
            Self::WorkerStopped { shard: Some(s) } => write!(
                f,
                "shard {} worker thread already stopped before shutdown could be requested",
                s.get()
            ),
            Self::WorkerStopped { shard: None } => write!(
                f,
                "worker thread already stopped before shutdown could be requested"
            ),
        }
    }
}

impl Error for ShutdownRequestError {}

/// Error returned by [`crate::ThreadedShutdownHandle::wait_report`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShutdownWaitError {
    /// The timeout elapsed before a terminal report could be produced.
    /// Possible reasons: no caller has requested shutdown (and the runtime
    /// has not been dropped); shutdown is in progress but did not complete
    /// in time.
    Timeout,
    /// The joiner thread that produces the terminal report stopped
    /// abnormally (typically a panic on the joiner). The terminal report
    /// is unavailable from this handle.
    WorkerStopped,
}

impl fmt::Display for ShutdownWaitError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Timeout => write!(f, "shutdown wait timed out before a terminal report"),
            Self::WorkerStopped => write!(
                f,
                "shutdown joiner stopped abnormally before a terminal report"
            ),
        }
    }
}

impl Error for ShutdownWaitError {}

#[cfg(test)]
mod tests {
    use super::*;

    fn assert_error<E: Error + 'static>(err: E, must_contain: &str) {
        let msg = err.to_string();
        assert!(
            msg.contains(must_contain),
            "Display message {msg:?} should contain {must_contain:?}"
        );
        // Round-trip into Box<dyn Error> proves the trait impls compose.
        let _: Box<dyn Error> = Box::new(err);
    }

    #[test]
    fn threaded_runtime_error_implements_display_and_error() {
        assert_error(ThreadedRuntimeError::WorkerStopped, "worker thread");
        assert_error(
            ThreadedRuntimeError::UnknownShard(tina::ShardId::new(7)),
            "shard 7",
        );
        assert_error(
            ThreadedRuntimeError::DriverShutdownFailed,
            "driver shutdown",
        );
        assert_error(ThreadedRuntimeError::CommandFull, "command queue is full");
    }

    #[test]
    fn shutdown_request_error_implements_display_and_error() {
        assert_error(
            ShutdownRequestError::CommandFull { shard: None },
            "command queue is full",
        );
        assert_error(
            ShutdownRequestError::CommandFull {
                shard: Some(tina::ShardId::new(3)),
            },
            "shard 3",
        );
        assert_error(
            ShutdownRequestError::WorkerStopped { shard: None },
            "worker thread already stopped",
        );
        assert_error(
            ShutdownRequestError::WorkerStopped {
                shard: Some(tina::ShardId::new(9)),
            },
            "shard 9",
        );
    }

    #[test]
    fn shutdown_wait_error_implements_display_and_error() {
        assert_error(ShutdownWaitError::Timeout, "timed out");
        assert_error(ShutdownWaitError::WorkerStopped, "joiner stopped");
    }

    #[test]
    fn supervise_error_implements_display_and_error() {
        assert_error(SuperviseError::UnknownParent, "parent");
    }

    #[test]
    fn threaded_try_send_error_implements_display_and_error() {
        assert_error(ThreadedTrySendError::IngressFull, "ingress");
        assert_error(ThreadedTrySendError::WorkerStopped, "worker thread");
    }

    #[test]
    fn threaded_send_observed_error_implements_display_and_error() {
        assert_error(ThreadedSendObservedError::IngressFull, "ingress");
        assert_error(ThreadedSendObservedError::MailboxFull, "mailbox is full");
        assert_error(ThreadedSendObservedError::MailboxClosed, "closed or stale");
        assert_error(
            ThreadedSendObservedError::WorkerStopped,
            "worker thread stopped",
        );
    }
}
