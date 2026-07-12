//! Threaded runtime and supervise error types extracted from lib.rs.

use std::error::Error;
use std::fmt;

use tina::ShardId;

/// Error returned by explicit-step and simulated runtime ingress.
///
/// Unlike the mailbox-local [`tina::TrySendError`], this routing boundary can
/// reject an address before selecting a mailbox. Every variant returns message
/// ownership to the caller.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IngressSendError<T> {
    /// The target belongs to another runtime/system incarnation.
    ForeignSystem {
        /// Incarnation owned by the routing runtime.
        expected: tina::SystemIncarnation,
        /// Incarnation carried by the target address.
        actual: tina::SystemIncarnation,
        /// Message ownership returned to the caller.
        message: T,
    },
    /// The target mailbox is currently at capacity.
    Full(T),
    /// The target mailbox is closed, stale, or unknown.
    Closed(T),
}

impl<T> From<tina::TrySendError<T>> for IngressSendError<T> {
    fn from(error: tina::TrySendError<T>) -> Self {
        match error {
            tina::TrySendError::Full(message) => Self::Full(message),
            tina::TrySendError::Closed(message) => Self::Closed(message),
        }
    }
}

/// Invalid bounded worker configuration supplied at startup.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThreadedRuntimeConfigError {
    /// A live owner cannot use the zero marker reserved for manual addresses.
    UnscopedSystemIncarnation,
    /// `command_capacity` is zero.
    ZeroCommandCapacity,
    /// `shard_pair_capacity` is zero.
    ZeroShardPairCapacity,
    /// `remote_inbound_drain_budget` is zero.
    ZeroRemoteInboundDrainBudget,
    /// `storage_lane_capacity` is zero.
    ZeroStorageLaneCapacity,
    /// `dns_lane_capacity` is zero.
    ZeroDnsLaneCapacity,
    /// `tls_lane_capacity` is zero.
    ZeroTlsLaneCapacity,
    /// `process_lane_capacity` is zero.
    ZeroProcessLaneCapacity,
    /// `signal_capacity` is zero.
    ZeroSignalCapacity,
    /// `timer_capacity` is zero.
    ZeroTimerCapacity,
    /// `hot_drain_max_rounds` is zero.
    ZeroHotDrainMaxRounds,
    /// `hot_drain_max_elapsed` is zero.
    ZeroHotDrainMaxElapsed,
    /// `idle_repoll_interval` is zero.
    ZeroIdleRepollInterval,
    /// `idle_wait` is zero.
    ZeroIdleWait,
    /// `control_call_timeout` is zero.
    ZeroControlCallTimeout,
    /// `driver_completion_drain_budget` is zero.
    ZeroDriverCompletionDrainBudget,
}

impl fmt::Display for ThreadedRuntimeConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let field = match self {
            Self::UnscopedSystemIncarnation => {
                return write!(f, "system_incarnation must be nonzero");
            }
            Self::ZeroCommandCapacity => "command_capacity",
            Self::ZeroShardPairCapacity => "shard_pair_capacity",
            Self::ZeroRemoteInboundDrainBudget => "remote_inbound_drain_budget",
            Self::ZeroStorageLaneCapacity => "storage_lane_capacity",
            Self::ZeroDnsLaneCapacity => "dns_lane_capacity",
            Self::ZeroTlsLaneCapacity => "tls_lane_capacity",
            Self::ZeroProcessLaneCapacity => "process_lane_capacity",
            Self::ZeroSignalCapacity => "signal_capacity",
            Self::ZeroTimerCapacity => "timer_capacity",
            Self::ZeroHotDrainMaxRounds => "hot_drain_max_rounds",
            Self::ZeroHotDrainMaxElapsed => "hot_drain_max_elapsed",
            Self::ZeroIdleRepollInterval => "idle_repoll_interval",
            Self::ZeroIdleWait => "idle_wait",
            Self::ZeroControlCallTimeout => "control_call_timeout",
            Self::ZeroDriverCompletionDrainBudget => "driver_completion_drain_budget",
        };
        write!(f, "{field} must be greater than zero")
    }
}

impl Error for ThreadedRuntimeConfigError {}

/// Failure to construct a live threaded runtime.
#[derive(Debug)]
pub enum StartupError {
    /// A low-level threaded runtime setting is invalid.
    InvalidThreadedConfig(ThreadedRuntimeConfigError),
    /// A local-system setting is invalid.
    InvalidLocalSystemConfig(crate::LocalSystemConfigError),
    /// A multi-shard topology contains no shards.
    NoShards,
    /// A multi-shard topology repeats a shard id.
    DuplicateShard(ShardId),
    /// Per-worker core assignment overflowed `usize`.
    ConfiguredCoreOverflow {
        /// Requested core for the first worker.
        base: usize,
        /// Stable worker ordinal being assigned.
        ordinal: usize,
    },
    /// Betelgeuse could not initialize its I/O loop.
    IoLoopInitialization {
        /// Shard whose worker was starting.
        shard: ShardId,
        /// Underlying platform I/O error.
        source: std::io::Error,
    },
    /// The operating system refused to create a worker thread.
    ThreadSpawn {
        /// Shard whose worker was being spawned.
        shard: ShardId,
        /// Underlying thread creation error.
        source: std::io::Error,
    },
    /// Startup code panicked while preparing a worker.
    WorkerStartupPanicked {
        /// Shard whose worker panicked.
        shard: ShardId,
        /// Captured panic message.
        message: String,
    },
    /// The worker exited without publishing a startup result.
    WorkerHandshakeDisconnected(ShardId),
    /// The worker did not publish a startup result within the bounded wait.
    ///
    /// Tina requests shutdown and briefly waits for cleanup before returning.
    /// Rust cannot cancel a thread blocked indefinitely in user-supplied
    /// startup code, so such a thread may outlive this error until that code
    /// returns.
    WorkerHandshakeTimeout {
        /// Shard whose startup did not complete.
        shard: ShardId,
        /// Constructor-side wait budget.
        timeout: std::time::Duration,
    },
}

impl fmt::Display for StartupError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidThreadedConfig(error) => {
                write!(f, "invalid threaded runtime config: {error}")
            }
            Self::InvalidLocalSystemConfig(error) => {
                write!(f, "invalid local system config: {error}")
            }
            Self::NoShards => write!(f, "multi-shard runtime requires at least one shard"),
            Self::DuplicateShard(shard) => write!(f, "duplicate shard id {}", shard.get()),
            Self::ConfiguredCoreOverflow { base, ordinal } => write!(
                f,
                "configured core assignment overflowed for base {base} and worker ordinal {ordinal}"
            ),
            Self::IoLoopInitialization { shard, source } => write!(
                f,
                "failed to initialize Betelgeuse I/O loop for shard {}: {source}",
                shard.get()
            ),
            Self::ThreadSpawn { shard, source } => write!(
                f,
                "failed to spawn worker thread for shard {}: {source}",
                shard.get()
            ),
            Self::WorkerStartupPanicked { shard, message } => {
                write!(f, "startup for shard {} panicked: {message}", shard.get())
            }
            Self::WorkerHandshakeDisconnected(shard) => write!(
                f,
                "worker for shard {} stopped before publishing its startup handshake",
                shard.get()
            ),
            Self::WorkerHandshakeTimeout { shard, timeout } => write!(
                f,
                "worker for shard {} did not finish startup within {timeout:?}",
                shard.get()
            ),
        }
    }
}

impl Error for StartupError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::InvalidThreadedConfig(error) => Some(error),
            Self::InvalidLocalSystemConfig(error) => Some(error),
            Self::IoLoopInitialization { source, .. } | Self::ThreadSpawn { source, .. } => {
                Some(source)
            }
            _ => None,
        }
    }
}

impl From<ThreadedRuntimeConfigError> for StartupError {
    fn from(value: ThreadedRuntimeConfigError) -> Self {
        Self::InvalidThreadedConfig(value)
    }
}

impl From<crate::LocalSystemConfigError> for StartupError {
    fn from(value: crate::LocalSystemConfigError) -> Self {
        Self::InvalidLocalSystemConfig(value)
    }
}

/// Error returned by setup/control operations on [`crate::ThreadedRuntime`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThreadedRuntimeError {
    /// The target address belongs to another runtime/system incarnation.
    ForeignSystem {
        /// Incarnation owned by this runtime.
        expected: tina::SystemIncarnation,
        /// Incarnation carried by the target.
        actual: tina::SystemIncarnation,
    },
    /// The worker thread stopped before it could accept or answer the command.
    WorkerStopped,
    /// The addressed lifecycle parent is stopped, unknown, or stale.
    ParentStopped,
    /// A multi-shard owner operation targeted a shard this local system does
    /// not own.
    UnknownShard(ShardId),
    /// The worker could not prove backend completion-slot ownership was
    /// released during shutdown.
    DriverShutdownFailed,
    /// A worker park path failed while waiting for work.
    ///
    /// Kept for API compatibility with the removed readiness-driven park path.
    DriverParkFailed,
    /// The bounded worker command queue could not accept the host-control
    /// command immediately. The host call did not block and no work was
    /// admitted; the caller can retry once the queue has drained.
    CommandFull,
    /// The host-side wait budget elapsed before the target call's terminal
    /// outcome arrived. The target call remains governed by its own call
    /// deadline.
    HostWaitTimeout,
    /// The worker accepted a host-control command but did not answer it within
    /// the control-call timeout — a wedged or runaway handler is monopolising
    /// the shard thread. The shard is marked `Failed`; the command may still be
    /// running on the worker.
    WorkerUnresponsive,
}

impl fmt::Display for ThreadedRuntimeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ForeignSystem { expected, actual } => write!(
                f,
                "address system {} does not match runtime system {}",
                actual.get(),
                expected.get()
            ),
            Self::WorkerStopped => {
                write!(
                    f,
                    "worker thread stopped before it could process the command"
                )
            }
            Self::ParentStopped => write!(f, "parent is stopped or stale"),
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
            Self::DriverParkFailed => write!(f, "worker park failed while waiting for work"),
            Self::CommandFull => {
                write!(
                    f,
                    "worker command queue is full; host-control command not admitted"
                )
            }
            Self::HostWaitTimeout => {
                write!(
                    f,
                    "host wait budget elapsed before target call outcome was delivered"
                )
            }
            Self::WorkerUnresponsive => {
                write!(
                    f,
                    "worker did not answer a host-control command within the control-call timeout"
                )
            }
        }
    }
}

impl From<crate::ChildLifecycleReportError> for ThreadedRuntimeError {
    fn from(error: crate::ChildLifecycleReportError) -> Self {
        match error {
            crate::ChildLifecycleReportError::ForeignSystem { expected, actual } => {
                Self::ForeignSystem { expected, actual }
            }
            crate::ChildLifecycleReportError::ParentShardUnavailable(shard) => {
                Self::UnknownShard(shard)
            }
            crate::ChildLifecycleReportError::ParentStopped => Self::ParentStopped,
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
/// mailbox capacity or surface the failure. The reserved isolate identifier is
/// not reused, matching failed constructor registration and simulator replay
/// determinism.
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
            Self::Full(_) => write!(f, "bootstrap prefill refused: mailbox reported full"),
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
/// Adds host-control admission and worker-lifecycle outcomes to the typed
/// mailbox-prefill refusals. Every failure before command admission returns the
/// untouched bootstrap message. [`Self::WorkerStopped`] and
/// [`Self::WorkerUnresponsive`] are message-less only after admission, when
/// the worker may already have consumed the authority.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ThreadedRegisterBootstrapError<M> {
    /// The mailbox refused the bootstrap message because it was already full.
    Full(M),
    /// The mailbox refused the bootstrap message because it had been closed.
    Closed(M),
    /// The bounded worker command queue was full. No registration command was
    /// admitted and the bootstrap message is returned untouched.
    CommandFull(M),
    /// The worker command channel was disconnected before enqueue. No
    /// registration command was admitted and the bootstrap message is
    /// returned untouched.
    CommandClosed(M),
    /// The worker stopped after command admission but before the host received
    /// a reply. The command may already have consumed the isolate and
    /// bootstrap message, so no message authority can be returned.
    WorkerStopped,
    /// The worker accepted the command but did not answer within the bounded
    /// control-call timeout. The command may still register and bootstrap the
    /// isolate later, so no message authority can be returned.
    WorkerUnresponsive,
    /// A multi-shard operation targeted a shard this runtime does not own.
    UnknownShard(ShardId, M),
}

impl<M> fmt::Display for ThreadedRegisterBootstrapError<M> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Full(_) => write!(f, "bootstrap prefill refused: mailbox reported full"),
            Self::Closed(_) => write!(
                f,
                "bootstrap prefill refused: mailbox already closed before registration"
            ),
            Self::CommandFull(_) => write!(
                f,
                "worker command queue full before register-and-bootstrap admission"
            ),
            Self::CommandClosed(_) => write!(
                f,
                "worker command channel closed before register-and-bootstrap admission"
            ),
            Self::WorkerStopped => write!(
                f,
                "worker stopped before answering accepted register-and-bootstrap"
            ),
            Self::WorkerUnresponsive => write!(
                f,
                "worker did not answer register-and-bootstrap within the control-call timeout"
            ),
            Self::UnknownShard(shard, _) => write!(
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

/// Error returned by threaded bounded ingress sends.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThreadedTrySendError {
    /// The target address belongs to another runtime/system incarnation.
    ForeignSystem {
        /// Incarnation owned by this runtime.
        expected: tina::SystemIncarnation,
        /// Incarnation carried by the target.
        actual: tina::SystemIncarnation,
    },
    /// The address names a shard not owned by the multi-shard runtime.
    UnknownShard(ShardId),

    /// The bounded worker ingress queue is full.
    IngressFull,

    /// The worker thread stopped before it could accept the ingress command.
    WorkerStopped,
}

impl fmt::Display for ThreadedTrySendError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ForeignSystem { expected, actual } => write!(
                f,
                "address system {} does not match runtime system {}",
                actual.get(),
                expected.get()
            ),
            Self::UnknownShard(shard) => {
                write!(
                    f,
                    "target shard {} is not owned by this runtime",
                    shard.get()
                )
            }
            Self::IngressFull => write!(f, "worker ingress queue is full"),
            Self::WorkerStopped => write!(f, "worker thread stopped before ingress was accepted"),
        }
    }
}

impl Error for ThreadedTrySendError {}

/// Error returned by threaded observed ingress sends.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThreadedSendObservedError {
    /// The target address belongs to another runtime/system incarnation.
    ForeignSystem {
        /// Incarnation owned by this runtime.
        expected: tina::SystemIncarnation,
        /// Incarnation carried by the target.
        actual: tina::SystemIncarnation,
    },
    /// The address names a shard not owned by the multi-shard runtime.
    UnknownShard(ShardId),

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
            Self::ForeignSystem { expected, actual } => write!(
                f,
                "address system {} does not match runtime system {}",
                actual.get(),
                expected.get()
            ),
            Self::UnknownShard(shard) => {
                write!(
                    f,
                    "target shard {} is not owned by this runtime",
                    shard.get()
                )
            }
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

/// Error returned by deadline-bounded threaded observed ingress sends.
///
/// Retry helper. Retries on `MailboxFull` and `IngressFull` until the
/// caller-supplied deadline; a deadline miss surfaces as [`Self::Timeout`].
/// `Closed` and `WorkerStopped` are returned eagerly because the target/worker
/// is no longer accepting at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SendObservedUntilError {
    /// The target address belongs to another runtime/system incarnation.
    ForeignSystem {
        /// Incarnation owned by this runtime.
        expected: tina::SystemIncarnation,
        /// Incarnation carried by the target.
        actual: tina::SystemIncarnation,
    },
    /// The address names a shard not owned by the multi-shard runtime.
    UnknownShard(ShardId),
    /// Deadline elapsed while still racing the mailbox/ingress for a slot.
    /// The timed-out attempt no longer owns delivery authority and cannot
    /// deliver later.
    Timeout,
    /// Target isolate mailbox reported closed/stale.
    Closed,
    /// Worker thread stopped before the send could be observed.
    WorkerStopped,
}

impl fmt::Display for SendObservedUntilError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ForeignSystem { expected, actual } => write!(
                f,
                "address system {} does not match runtime system {}",
                actual.get(),
                expected.get()
            ),
            Self::UnknownShard(shard) => {
                write!(
                    f,
                    "target shard {} is not owned by this runtime",
                    shard.get()
                )
            }
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

/// Error returned by
/// [`crate::ThreadedShutdownHandle::request_and_wait_report`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShutdownAndWaitError {
    /// The total timeout elapsed while shutdown admission was still being
    /// retried. `last` preserves the final bounded request failure.
    RequestTimeout {
        /// Last request error observed before the total deadline elapsed.
        last: ShutdownRequestError,
    },
    /// Shutdown was admitted, but terminal-report observation failed.
    Wait(ShutdownWaitError),
}

impl fmt::Display for ShutdownAndWaitError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::RequestTimeout { last } => {
                write!(f, "shutdown request timed out after: {last}")
            }
            Self::Wait(error) => write!(f, "shutdown terminal-report wait failed: {error}"),
        }
    }
}

impl Error for ShutdownAndWaitError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::RequestTimeout { last } => Some(last),
            Self::Wait(error) => Some(error),
        }
    }
}

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
        assert_error(ThreadedRuntimeError::DriverParkFailed, "worker park");
        assert_error(ThreadedRuntimeError::CommandFull, "command queue is full");
        assert_error(ThreadedRuntimeError::HostWaitTimeout, "host wait budget");
        assert_error(
            ThreadedRuntimeError::WorkerUnresponsive,
            "control-call timeout",
        );
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
    fn shutdown_and_wait_error_preserves_phase_source() {
        let request = ShutdownAndWaitError::RequestTimeout {
            last: ShutdownRequestError::CommandFull { shard: None },
        };
        assert!(request.to_string().contains("request timed out"));
        assert!(
            request
                .source()
                .expect("request source")
                .to_string()
                .contains("command queue is full")
        );

        let wait = ShutdownAndWaitError::Wait(ShutdownWaitError::WorkerStopped);
        assert!(wait.to_string().contains("terminal-report wait"));
        assert!(
            wait.source()
                .expect("wait source")
                .to_string()
                .contains("joiner stopped")
        );
    }

    #[test]
    fn supervise_error_implements_display_and_error() {
        assert_error(SuperviseError::UnknownParent, "parent");
    }

    #[test]
    fn threaded_try_send_error_implements_display_and_error() {
        assert_error(
            ThreadedTrySendError::UnknownShard(ShardId::new(17)),
            "shard 17",
        );
        assert_error(ThreadedTrySendError::IngressFull, "ingress");
        assert_error(ThreadedTrySendError::WorkerStopped, "worker thread");
    }

    #[test]
    fn threaded_send_observed_error_implements_display_and_error() {
        assert_error(
            ThreadedSendObservedError::UnknownShard(ShardId::new(17)),
            "shard 17",
        );
        assert_error(ThreadedSendObservedError::IngressFull, "ingress");
        assert_error(ThreadedSendObservedError::MailboxFull, "mailbox is full");
        assert_error(ThreadedSendObservedError::MailboxClosed, "closed or stale");
        assert_error(
            ThreadedSendObservedError::WorkerStopped,
            "worker thread stopped",
        );
    }
}
