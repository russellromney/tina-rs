//! LocalSystem installation for HTTP/1.1 keepalive pools.
//!
//! User shape:
//!
//! ```text
//! let pool = system.install_keepalive_pool(config)?;
//! // drive pool.pool() / pool.connections()
//! match pool.close_and_drain(timeout) {
//!     KeepaliveCloseAndDrain::Drained(report) => { /* settled */ }
//!     KeepaliveCloseAndDrain::TimedOut { pool, pending } => {
//!         // admitted work still owns the handle; retry later
//!     }
//!     other => { /* owner failure or shutdown settlement */ }
//! }
//! ```
//!
//! Installation is atomic: a partial registration either rolls every installed
//! connection back or returns typed recovery authority while retaining the
//! origin claim. A second install for the same origin on the same system
//! incarnation returns a typed conflict.
//! Close consumes the handle so double-close is unrepresentable. Drain timeout
//! retains the handle with exact pending counts. There is no public force-close
//! path on this facade.

use std::collections::HashSet;
use std::convert::Infallible;
use std::fmt;
use std::marker::PhantomData;
use std::sync::{Mutex, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

use tina::pool::{CloseMode, PoolConfig};
use tina::prelude::*;
use tina_runtime::pool::{WorkerPool, WorkerPoolMsg, WorkerPoolReply};
use tina_runtime::{
    CallOutcome, LiveShardState, LocalSystem, MailboxFactory, ThreadedRuntime, ThreadedRuntimeError,
};

use crate::keepalive::{
    KeepaliveConnAddr, KeepaliveConnection, KeepaliveConnectionMsg, KeepaliveConnectionStopFailure,
    KeepaliveConnectionStopOutcome, KeepaliveOutcome, KeepalivePoolCloseOutcome,
    KeepalivePoolDrainOutcome, KeepalivePoolHandles, OriginKey,
};
use crate::target::HttpTarget;
use crate::types::HttpClientConfig;

#[cfg(test)]
static INSTALL_RESOURCE_BOUNDARY_ENTRIES: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);

fn record_install_resource_boundary() {
    #[cfg(test)]
    INSTALL_RESOURCE_BOUNDARY_ENTRIES.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
}

/// Largest keepalive connection pool accepted by the installation facade.
pub const MAX_KEEPALIVE_POOL_CAPACITY: usize = 1_024;
/// Largest parked-waiter budget accepted by the installation facade.
pub const MAX_KEEPALIVE_POOL_WAITERS: usize = 65_536;
/// Largest mailbox accepted for a keepalive connection or pool isolate.
pub const MAX_KEEPALIVE_MAILBOX_CAPACITY: usize = 65_536;

/// Configuration for [`InstallKeepalivePool::install_keepalive_pool`].
#[derive(Debug, Clone)]
pub struct KeepalivePoolInstallConfig {
    /// Origin every connection isolate is bound to.
    pub target: HttpTarget,
    /// Client parse/timeout policy for each connection isolate.
    pub client_config: HttpClientConfig,
    /// Pool capacity and waiter ceiling. `capacity` must be greater than zero.
    pub pool_config: PoolConfig,
    /// Mailbox size for each connection isolate.
    pub connection_mailbox_capacity: usize,
    /// Mailbox size for the pool isolate.
    pub pool_mailbox_capacity: usize,
}

/// Why a keepalive install config was refused before any resource was created.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeepalivePoolConfigError {
    /// `pool_config.capacity` is zero.
    ZeroCapacity,
    /// `connection_mailbox_capacity` is zero.
    ZeroConnectionMailbox,
    /// `pool_mailbox_capacity` is zero.
    ZeroPoolMailbox,
    /// A finite installation bound was exceeded.
    TooLarge {
        /// Configuration field that exceeded its ceiling.
        field: &'static str,
        /// Requested value.
        requested: usize,
        /// Largest accepted value.
        max: usize,
    },
}

impl fmt::Display for KeepalivePoolConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ZeroCapacity => write!(f, "keepalive pool capacity must be greater than zero"),
            Self::ZeroConnectionMailbox => {
                write!(
                    f,
                    "keepalive connection mailbox capacity must be greater than zero"
                )
            }
            Self::ZeroPoolMailbox => {
                write!(
                    f,
                    "keepalive pool mailbox capacity must be greater than zero"
                )
            }
            Self::TooLarge {
                field,
                requested,
                max,
            } => write!(f, "{field} {requested} exceeds maximum {max}"),
        }
    }
}

impl std::error::Error for KeepalivePoolConfigError {}

impl KeepalivePoolInstallConfig {
    /// Build a config with explicit bounds.
    pub fn new(
        target: HttpTarget,
        client_config: HttpClientConfig,
        pool_config: PoolConfig,
        connection_mailbox_capacity: usize,
        pool_mailbox_capacity: usize,
    ) -> Self {
        Self {
            target,
            client_config,
            pool_config,
            connection_mailbox_capacity,
            pool_mailbox_capacity,
        }
    }

    /// Validate structural bounds before any isolate is registered.
    pub fn validate(&self) -> Result<(), KeepalivePoolConfigError> {
        if self.pool_config.capacity == 0 {
            return Err(KeepalivePoolConfigError::ZeroCapacity);
        }
        if self.connection_mailbox_capacity == 0 {
            return Err(KeepalivePoolConfigError::ZeroConnectionMailbox);
        }
        if self.pool_mailbox_capacity == 0 {
            return Err(KeepalivePoolConfigError::ZeroPoolMailbox);
        }
        validate_max(
            "pool_config.capacity",
            self.pool_config.capacity,
            MAX_KEEPALIVE_POOL_CAPACITY,
        )?;
        validate_max(
            "pool_config.max_waiters",
            self.pool_config.max_waiters,
            MAX_KEEPALIVE_POOL_WAITERS,
        )?;
        validate_max(
            "connection_mailbox_capacity",
            self.connection_mailbox_capacity,
            MAX_KEEPALIVE_MAILBOX_CAPACITY,
        )?;
        validate_max(
            "pool_mailbox_capacity",
            self.pool_mailbox_capacity,
            MAX_KEEPALIVE_MAILBOX_CAPACITY,
        )?;
        // WorkerPool uses u32 protocol identifiers. Keep this checked even if
        // the finite public ceilings are lowered or raised later.
        validate_u32("pool_config.capacity", self.pool_config.capacity)?;
        validate_u32("pool_config.max_waiters", self.pool_config.max_waiters)?;
        Ok(())
    }
}

fn validate_max(
    field: &'static str,
    requested: usize,
    max: usize,
) -> Result<(), KeepalivePoolConfigError> {
    if requested > max {
        Err(KeepalivePoolConfigError::TooLarge {
            field,
            requested,
            max,
        })
    } else {
        Ok(())
    }
}

fn validate_u32(field: &'static str, requested: usize) -> Result<(), KeepalivePoolConfigError> {
    if u32::try_from(requested).is_err() {
        Err(KeepalivePoolConfigError::TooLarge {
            field,
            requested,
            max: u32::MAX as usize,
        })
    } else {
        Ok(())
    }
}

/// Which install step failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeepaliveInstallStep {
    /// A connection isolate registration failed.
    Connection {
        /// Index of the connection that failed (0-based).
        index: usize,
    },
    /// The pool isolate registration failed after all connections registered.
    Pool,
}

/// Accounting for a partial install that rolled back.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeepaliveInstallRollbackReport {
    /// Connection isolates that registered before the failure.
    pub connections_registered: usize,
    /// Connection isolates that replied `Stopped` during rollback.
    pub connections_stopped: usize,
    /// Connection isolates that were already closed when rollback asked them to stop.
    pub connections_already_closed: usize,
    /// Per-slot stop failures that are not clean stops or already-closed.
    pub connection_stop_failures: Vec<KeepaliveConnectionStopFailure>,
    /// Whether the pool isolate had registered before rollback.
    pub pool_registered: bool,
}

/// Retained authority for finishing an installation rollback that could not
/// stop every registered connection on its first attempt.
#[must_use = "incomplete rollback retains live resources and its origin claim"]
pub struct KeepaliveInstallRecovery<S, F = tina_runtime::DefaultThreadedMailboxFactory>
where
    S: Shard + Send + 'static,
    F: MailboxFactory + Send + 'static,
{
    host: ThreadedRuntime<S, F>,
    connections: Vec<KeepaliveConnAddr>,
    settled: Vec<Option<KeepaliveConnectionStopOutcome>>,
    claim: InstallClaim,
    forced_failure: Option<usize>,
}

impl<S, F> fmt::Debug for KeepaliveInstallRecovery<S, F>
where
    S: Shard + Send + 'static,
    F: MailboxFactory + Send + 'static,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("KeepaliveInstallRecovery")
            .field("connections", &self.connections.len())
            .field("connections_live", &self.live_count())
            .finish_non_exhaustive()
    }
}

/// Result of retrying retained installation rollback authority.
#[derive(Debug)]
pub enum KeepaliveRollbackResult<S, F = tina_runtime::DefaultThreadedMailboxFactory>
where
    S: Shard + Send + 'static,
    F: MailboxFactory + Send + 'static,
{
    /// Every registered connection is now terminal and the origin claim was released.
    Recovered(KeepaliveInstallRollbackReport),
    /// Cleanup remains incomplete; authority and the origin claim are retained.
    Retained {
        /// Authority for another bounded retry.
        recovery: Box<KeepaliveInstallRecovery<S, F>>,
        /// Exact cumulative rollback accounting.
        report: KeepaliveInstallRollbackReport,
    },
}

impl<S, F> KeepaliveInstallRecovery<S, F>
where
    S: Shard + Send + 'static,
    F: MailboxFactory + Send + 'static,
{
    /// Retry cleanup within one total timeout.
    pub fn retry(mut self, timeout: Duration) -> KeepaliveRollbackResult<S, F> {
        let deadline = deadline_after(timeout);
        let report = self.attempt(deadline);
        if self.live_count() == 0 {
            self.claim.release();
            KeepaliveRollbackResult::Recovered(report)
        } else {
            KeepaliveRollbackResult::Retained {
                recovery: Box::new(self),
                report,
            }
        }
    }

    fn live_count(&self) -> usize {
        self.settled
            .iter()
            .filter(|outcome| outcome.is_none())
            .count()
    }

    fn attempt(&mut self, deadline: Instant) -> KeepaliveInstallRollbackReport {
        let mut failures = Vec::new();
        for (index, conn) in self.connections.iter().copied().enumerate() {
            if self.settled[index].is_some() {
                continue;
            }
            let outcome = if self.forced_failure == Some(index) {
                self.forced_failure = None;
                KeepaliveConnectionStopOutcome::TimedOut
            } else if let Some(remaining) = remaining(deadline) {
                classify_connection_stop(call_with_deadline(
                    &self.host,
                    conn,
                    KeepaliveConnectionMsg::Stop,
                    remaining,
                ))
            } else {
                KeepaliveConnectionStopOutcome::TimedOut
            };
            match outcome {
                KeepaliveConnectionStopOutcome::Stopped
                | KeepaliveConnectionStopOutcome::AlreadyClosed => {
                    self.settled[index] = Some(outcome);
                }
                other => failures.push(KeepaliveConnectionStopFailure {
                    index,
                    outcome: other,
                }),
            }
        }
        self.report(failures)
    }

    fn report(
        &self,
        connection_stop_failures: Vec<KeepaliveConnectionStopFailure>,
    ) -> KeepaliveInstallRollbackReport {
        KeepaliveInstallRollbackReport {
            connections_registered: self.connections.len(),
            connections_stopped: self
                .settled
                .iter()
                .filter(|v| matches!(v, Some(KeepaliveConnectionStopOutcome::Stopped)))
                .count(),
            connections_already_closed: self
                .settled
                .iter()
                .filter(|v| matches!(v, Some(KeepaliveConnectionStopOutcome::AlreadyClosed)))
                .count(),
            connection_stop_failures,
            pool_registered: false,
        }
    }
}

/// Failure to install a keepalive pool on a live owner.
pub enum KeepalivePoolInstallError<
    S = tina::SingleShard,
    F = tina_runtime::DefaultThreadedMailboxFactory,
> where
    S: Shard + Send + 'static,
    F: MailboxFactory + Send + 'static,
{
    /// Config refused before any resource was created.
    InvalidConfig(KeepalivePoolConfigError),
    /// An install for this origin is already live on this system incarnation.
    Conflict {
        /// Origin that already owns an install claim.
        origin: OriginKey,
    },
    /// A registration failed. `rollback` describes the first cleanup attempt;
    /// `recovery` retains authority when any registered resource remains live.
    Register {
        /// Step that failed.
        failed_at: KeepaliveInstallStep,
        /// Underlying registration error.
        source: ThreadedRuntimeError,
        /// Rollback accounting for resources that had already registered.
        rollback: KeepaliveInstallRollbackReport,
        /// Retained cleanup authority when rollback could not settle every slot.
        recovery: Option<Box<KeepaliveInstallRecovery<S, F>>>,
    },
}

/// Result of installing a keepalive pool on a live owner.
pub type KeepalivePoolInstallResult<S, F = tina_runtime::DefaultThreadedMailboxFactory> =
    Result<InstalledKeepalivePool<S, F>, KeepalivePoolInstallError<S, F>>;

impl<S, F> fmt::Debug for KeepalivePoolInstallError<S, F>
where
    S: Shard + Send + 'static,
    F: MailboxFactory + Send + 'static,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidConfig(error) => f.debug_tuple("InvalidConfig").field(error).finish(),
            Self::Conflict { origin } => {
                f.debug_struct("Conflict").field("origin", origin).finish()
            }
            Self::Register {
                failed_at,
                source,
                rollback,
                recovery,
            } => f
                .debug_struct("Register")
                .field("failed_at", failed_at)
                .field("source", source)
                .field("rollback", rollback)
                .field("recovery", recovery)
                .finish(),
        }
    }
}

impl<S, F> fmt::Display for KeepalivePoolInstallError<S, F>
where
    S: Shard + Send + 'static,
    F: MailboxFactory + Send + 'static,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidConfig(error) => write!(f, "keepalive pool install: {error}"),
            Self::Conflict { origin } => {
                write!(
                    f,
                    "keepalive pool install: origin already installed ({origin:?})"
                )
            }
            Self::Register {
                failed_at,
                source,
                rollback,
                ..
            } => write!(
                f,
                "keepalive pool install failed at {failed_at:?}: {source}; \
                 rolled back {} connection(s) (stopped {}, already_closed {}, failures {})",
                rollback.connections_registered,
                rollback.connections_stopped,
                rollback.connections_already_closed,
                rollback.connection_stop_failures.len()
            ),
        }
    }
}

impl<S, F> std::error::Error for KeepalivePoolInstallError<S, F>
where
    S: Shard + Send + 'static,
    F: MailboxFactory + Send + 'static,
{
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::InvalidConfig(error) => Some(error),
            Self::Conflict { .. } => None,
            Self::Register { source, .. } => Some(source),
        }
    }
}

/// Pending work observed when a drain times out or an owner fails.
///
/// `leased` is exact only when a pressure sample was observed. It is never
/// capacity-seeded: unobserved paths leave `leased` as [`None`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KeepalivePendingCounts {
    /// Outstanding pool leases from a pressure observation.
    ///
    /// `Some(n)` is the exact leased count at observation time. `None` means no
    /// pressure sample was available — never a capacity guess.
    pub leased: Option<usize>,
    /// Connection isolates that have not yet been stopped by this handle.
    pub connections_live: usize,
    /// Whether pool admission has already been closed.
    pub admission_closed: bool,
}

/// Proof that every admitted lease returned and every connection stopped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeepalivePoolSettledReport {
    /// Pool admission close outcome (always `Closed` for a first explicit close).
    pub pool_close: KeepalivePoolCloseOutcome,
    /// Drain outcome (always `Drained` on this report).
    pub drain: KeepalivePoolDrainOutcome,
    /// Number of connection isolates asked to stop.
    pub requested: usize,
    /// Stop calls that replied `Stopped`.
    pub stopped: usize,
    /// Stop calls that found the address already closed (still settled).
    pub already_closed: usize,
}

/// Shutdown settlement when the runtime cancelled or closed the pool path
/// without a full host-driven drain proof.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeepaliveShutdownSettlement {
    /// Pool close observation under shutdown.
    pub pool_close: KeepalivePoolCloseOutcome,
    /// Drain observation under shutdown (never claims a full drain).
    pub drain: KeepalivePoolDrainOutcome,
    /// Pending counts at settlement, if observed.
    pub pending: KeepalivePendingCounts,
}

/// Outcome of a consuming [`InstalledKeepalivePool::close_and_drain`].
#[derive(Debug)]
pub enum KeepaliveCloseAndDrain<S, F = tina_runtime::DefaultThreadedMailboxFactory>
where
    S: Shard + Send + 'static,
    F: MailboxFactory + Send + 'static,
{
    /// Every admitted request/lease settled and every connection stopped.
    Drained(KeepalivePoolSettledReport),
    /// Drain deadline fired. The owned handle is returned so the caller can retry.
    ///
    /// Admitted work was not aborted. `pending.leased` is the exact observed
    /// lease count when a pressure sample landed; it is never capacity-seeded.
    TimedOut {
        /// Handle retained for a later drain attempt.
        pool: InstalledKeepalivePool<S, F>,
        /// Pending counts at the deadline (`leased` only when observed).
        pending: KeepalivePendingCounts,
    },
    /// The live owner failed while closing or draining. Handle retained.
    OwnerFailed {
        /// Handle retained; admitted work may still be live.
        pool: InstalledKeepalivePool<S, F>,
        /// Owner error that interrupted settlement.
        error: ThreadedRuntimeError,
        /// Best-effort pending counts at the failure.
        pending: KeepalivePendingCounts,
    },
    /// System/runtime shutdown cancelled the path. Does not claim a full drain.
    Shutdown(KeepaliveShutdownSettlement),
}

/// Owned keepalive pool installed on a live owner.
///
/// Drive the pool with [`Self::pool`] / [`Self::connections`]. Settle with the
/// consuming [`Self::close_and_drain`]. Dropping an unsettled handle does not
/// free its origin claim: later installs remain in typed conflict until owner
/// shutdown, rather than creating a second pool beside orphaned resources.
#[must_use = "installed keepalive resources must be closed and drained"]
pub struct InstalledKeepalivePool<S, F = tina_runtime::DefaultThreadedMailboxFactory>
where
    S: Shard + Send + 'static,
    F: MailboxFactory + Send + 'static,
{
    handles: KeepalivePoolHandles,
    origin: OriginKey,
    claim: InstallClaim,
    host: ThreadedRuntime<S, F>,
    /// True once pool admission has been closed by this handle.
    admission_closed: bool,
    connection_settled: Vec<Option<KeepaliveConnectionStopOutcome>>,
    _shard: PhantomData<S>,
}

impl<S, F> fmt::Debug for InstalledKeepalivePool<S, F>
where
    S: Shard + Send + 'static,
    F: MailboxFactory + Send + 'static,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("InstalledKeepalivePool")
            .field("origin", &self.origin)
            .field("connections", &self.handles.connections.len())
            .field("admission_closed", &self.admission_closed)
            .field(
                "connections_live",
                &self
                    .connection_settled
                    .iter()
                    .filter(|v| v.is_none())
                    .count(),
            )
            .finish_non_exhaustive()
    }
}

impl<S, F> InstalledKeepalivePool<S, F>
where
    S: Shard + Send + 'static,
    F: MailboxFactory + Send + 'static,
{
    /// Pool address for acquire/release/pressure calls.
    pub fn pool(
        &self,
    ) -> Address<WorkerPoolMsg<KeepaliveConnAddr>, WorkerPoolReply<KeepaliveConnAddr>> {
        self.handles.pool
    }

    /// Connection isolate addresses bound into the pool.
    pub fn connections(&self) -> &[KeepaliveConnAddr] {
        &self.handles.connections
    }

    /// Origin this pool is bound to.
    pub fn origin(&self) -> &OriginKey {
        &self.origin
    }

    /// Lower-level handle bundle for callers that already speak pool vocabulary.
    pub fn handles(&self) -> &KeepalivePoolHandles {
        &self.handles
    }

    /// Close pool admission, wait for every lease to return, then stop every
    /// connection isolate.
    ///
    /// Consumes `self` so a second close is unrepresentable. On drain timeout
    /// the owned handle is returned with exact pending counts so the caller can
    /// retry. There is no force-close path on this facade.
    pub fn close_and_drain(self, timeout: Duration) -> KeepaliveCloseAndDrain<S, F> {
        self.close_and_drain_inner(deadline_after(timeout), None)
    }

    #[doc(hidden)]
    pub fn close_and_drain_with_stop_timeout_at(
        self,
        timeout: Duration,
        index: usize,
    ) -> KeepaliveCloseAndDrain<S, F> {
        self.close_and_drain_inner(deadline_after(timeout), Some(index))
    }

    fn close_and_drain_inner(
        mut self,
        deadline: Instant,
        forced_stop_timeout: Option<usize>,
    ) -> KeepaliveCloseAndDrain<S, F> {
        if !self.admission_closed {
            let Some(budget) = remaining(deadline) else {
                let pending = self.pending(None);
                return KeepaliveCloseAndDrain::TimedOut {
                    pool: self,
                    pending,
                };
            };
            let close = call_with_deadline(
                &self.host,
                self.handles.pool,
                WorkerPoolMsg::Close(CloseMode::Drain),
                budget,
            );
            match close {
                Ok(CallOutcome::Replied(WorkerPoolReply::Closed)) => {
                    self.admission_closed = true;
                }
                Ok(CallOutcome::Closed) => {
                    return classify_closed_pool(
                        self,
                        KeepalivePoolDrainOutcome::PoolAlreadyClosed,
                    );
                }
                Ok(CallOutcome::Timeout) | Err(ThreadedRuntimeError::HostWaitTimeout) => {
                    let leased = observe_pool_leased(&self.host, &self.handles, deadline);
                    let pending = self.pending(leased);
                    return KeepaliveCloseAndDrain::TimedOut {
                        pool: self,
                        pending,
                    };
                }
                Ok(CallOutcome::Full) => {
                    let pending = self.pending(None);
                    return KeepaliveCloseAndDrain::OwnerFailed {
                        pool: self,
                        error: ThreadedRuntimeError::CommandFull,
                        pending,
                    };
                }
                Ok(CallOutcome::Rejected(_)) | Ok(CallOutcome::Replied(_)) => {
                    let pending = self.pending(None);
                    return KeepaliveCloseAndDrain::OwnerFailed {
                        pool: self,
                        error: ThreadedRuntimeError::WorkerUnresponsive,
                        pending,
                    };
                }
                Err(error) => {
                    let pending = self.pending(None);
                    return map_owner_or_shutdown_error(self, error, pending);
                }
            }
        }

        let drain = match wait_pool_drain(&self.host, &self.handles, deadline) {
            Ok(outcome) => outcome,
            Err(error) => {
                let pending = self.pending(None);
                return map_owner_or_shutdown_error(self, error, pending);
            }
        };
        match drain {
            KeepalivePoolDrainOutcome::Drained => {}
            KeepalivePoolDrainOutcome::TimedOut { leased } => {
                let pending = self.pending(leased);
                return KeepaliveCloseAndDrain::TimedOut {
                    pool: self,
                    pending,
                };
            }
            other => return classify_closed_pool(self, other),
        }

        for (index, conn) in self.handles.connections.iter().copied().enumerate() {
            if self.connection_settled[index].is_some() {
                continue;
            }
            if forced_stop_timeout == Some(index) {
                let pending = self.pending(Some(0));
                return KeepaliveCloseAndDrain::TimedOut {
                    pool: self,
                    pending,
                };
            }
            let Some(budget) = remaining(deadline) else {
                let pending = self.pending(Some(0));
                return KeepaliveCloseAndDrain::TimedOut {
                    pool: self,
                    pending,
                };
            };
            match call_with_deadline(&self.host, conn, KeepaliveConnectionMsg::Stop, budget) {
                Ok(CallOutcome::Replied(KeepaliveOutcome::Stopped)) => {
                    self.connection_settled[index] = Some(KeepaliveConnectionStopOutcome::Stopped);
                }
                Ok(CallOutcome::Closed) => {
                    self.connection_settled[index] =
                        Some(KeepaliveConnectionStopOutcome::AlreadyClosed);
                }
                Ok(CallOutcome::Timeout) | Err(ThreadedRuntimeError::HostWaitTimeout) => {
                    let pending = self.pending(Some(0));
                    return KeepaliveCloseAndDrain::TimedOut {
                        pool: self,
                        pending,
                    };
                }
                Ok(CallOutcome::Full) => {
                    let pending = self.pending(Some(0));
                    return KeepaliveCloseAndDrain::OwnerFailed {
                        pool: self,
                        error: ThreadedRuntimeError::CommandFull,
                        pending,
                    };
                }
                Ok(CallOutcome::Rejected(_)) | Ok(CallOutcome::Replied(_)) => {
                    let pending = self.pending(Some(0));
                    return KeepaliveCloseAndDrain::OwnerFailed {
                        pool: self,
                        error: ThreadedRuntimeError::WorkerUnresponsive,
                        pending,
                    };
                }
                Err(error) => {
                    let pending = self.pending(Some(0));
                    return map_owner_or_shutdown_error(self, error, pending);
                }
            }
        }

        let stopped = self
            .connection_settled
            .iter()
            .filter(|v| matches!(v, Some(KeepaliveConnectionStopOutcome::Stopped)))
            .count();
        let already_closed = self
            .connection_settled
            .iter()
            .filter(|v| matches!(v, Some(KeepaliveConnectionStopOutcome::AlreadyClosed)))
            .count();
        let requested = self.handles.connections.len();
        self.claim.release();
        KeepaliveCloseAndDrain::Drained(KeepalivePoolSettledReport {
            pool_close: KeepalivePoolCloseOutcome::Closed,
            drain: KeepalivePoolDrainOutcome::Drained,
            requested,
            stopped,
            already_closed,
        })
    }

    fn pending(&self, leased: Option<usize>) -> KeepalivePendingCounts {
        KeepalivePendingCounts {
            leased,
            connections_live: self
                .connection_settled
                .iter()
                .filter(|value| value.is_none())
                .count(),
            admission_closed: self.admission_closed,
        }
    }
}

/// Extension trait that installs a keepalive pool on a [`LocalSystem`].
pub trait InstallKeepalivePool {
    /// Shard type of the local system.
    type Shard: Shard + Send + 'static;
    /// Mailbox factory of the local system.
    type Factory: MailboxFactory + Send + 'static;

    /// Atomically install a keepalive pool and return an owned handle.
    fn install_keepalive_pool(
        &self,
        config: KeepalivePoolInstallConfig,
    ) -> KeepalivePoolInstallResult<Self::Shard, Self::Factory>;
}

impl<S, F> InstallKeepalivePool for LocalSystem<S, F>
where
    S: Shard + Send + 'static,
    F: MailboxFactory + Send + 'static,
{
    type Shard = S;
    type Factory = F;

    fn install_keepalive_pool(
        &self,
        config: KeepalivePoolInstallConfig,
    ) -> KeepalivePoolInstallResult<S, F> {
        config
            .validate()
            .map_err(KeepalivePoolInstallError::InvalidConfig)?;
        record_install_resource_boundary();

        let origin = OriginKey::from_target(&config.target);
        let claim = InstallClaim::try_claim(self.system_incarnation(), origin.clone())?;
        let host = self.host_control();

        let mut connections: Vec<KeepaliveConnAddr> =
            Vec::with_capacity(config.pool_config.capacity);

        for index in 0..config.pool_config.capacity {
            let conn = KeepaliveConnection::<S>::new(config.target.clone(), config.client_config);
            match self.register_root::<_, Infallible>(conn, config.connection_mailbox_capacity) {
                Ok(address) => connections.push(address),
                Err(source) => {
                    let (rollback, recovery) = rollback_connections(
                        host.host_control(),
                        connections,
                        claim,
                        Duration::from_secs(2),
                        None,
                    );
                    return Err(KeepalivePoolInstallError::Register {
                        failed_at: KeepaliveInstallStep::Connection { index },
                        source,
                        rollback,
                        recovery,
                    });
                }
            }
        }

        let pool: WorkerPool<KeepaliveConnAddr, S> =
            WorkerPool::new(config.pool_config, connections.clone());
        let pool_address =
            match self.register_root::<_, Infallible>(pool, config.pool_mailbox_capacity) {
                Ok(address) => address,
                Err(source) => {
                    let (rollback, recovery) = rollback_connections(
                        host.host_control(),
                        connections,
                        claim,
                        Duration::from_secs(2),
                        None,
                    );
                    return Err(KeepalivePoolInstallError::Register {
                        failed_at: KeepaliveInstallStep::Pool,
                        source,
                        rollback,
                        recovery,
                    });
                }
            };

        Ok(InstalledKeepalivePool {
            handles: KeepalivePoolHandles {
                pool: pool_address,
                connections,
            },
            origin,
            claim,
            host,
            admission_closed: false,
            connection_settled: vec![None; config.pool_config.capacity],
            _shard: PhantomData,
        })
    }
}

/// Install on a raw [`ThreadedRuntime`]. Same contract as the LocalSystem path.
pub fn install_keepalive_pool_on_runtime<S, F>(
    runtime: &ThreadedRuntime<S, F>,
    config: KeepalivePoolInstallConfig,
) -> Result<InstalledKeepalivePool<S, F>, KeepalivePoolInstallError<S, F>>
where
    S: Shard + Send + 'static,
    F: MailboxFactory + Send + 'static,
{
    config
        .validate()
        .map_err(KeepalivePoolInstallError::InvalidConfig)?;
    record_install_resource_boundary();

    let origin = OriginKey::from_target(&config.target);
    let claim = InstallClaim::try_claim(runtime.system_incarnation(), origin.clone())?;
    let host = runtime.host_control();

    let mut connections: Vec<KeepaliveConnAddr> = Vec::with_capacity(config.pool_config.capacity);

    for index in 0..config.pool_config.capacity {
        let conn = KeepaliveConnection::<S>::new(config.target.clone(), config.client_config);
        match runtime
            .register_with_capacity::<_, Infallible>(conn, config.connection_mailbox_capacity)
        {
            Ok(address) => connections.push(address),
            Err(source) => {
                let (rollback, recovery) = rollback_connections(
                    host.host_control(),
                    connections,
                    claim,
                    Duration::from_secs(2),
                    None,
                );
                return Err(KeepalivePoolInstallError::Register {
                    failed_at: KeepaliveInstallStep::Connection { index },
                    source,
                    rollback,
                    recovery,
                });
            }
        }
    }

    let pool: WorkerPool<KeepaliveConnAddr, S> =
        WorkerPool::new(config.pool_config, connections.clone());
    let pool_address =
        match runtime.register_with_capacity::<_, Infallible>(pool, config.pool_mailbox_capacity) {
            Ok(address) => address,
            Err(source) => {
                let (rollback, recovery) = rollback_connections(
                    host.host_control(),
                    connections,
                    claim,
                    Duration::from_secs(2),
                    None,
                );
                return Err(KeepalivePoolInstallError::Register {
                    failed_at: KeepaliveInstallStep::Pool,
                    source,
                    rollback,
                    recovery,
                });
            }
        };

    Ok(InstalledKeepalivePool {
        handles: KeepalivePoolHandles {
            pool: pool_address,
            connections,
        },
        origin,
        claim,
        host,
        admission_closed: false,
        connection_settled: vec![None; config.pool_config.capacity],
        _shard: PhantomData,
    })
}

// ---------------------------------------------------------------------------
// Install claim registry (origin uniqueness per system incarnation)
// ---------------------------------------------------------------------------

#[derive(Clone, PartialEq, Eq, Hash)]
struct ClaimKey {
    system: u64,
    origin: OriginKey,
}

struct InstallClaim {
    key: Option<ClaimKey>,
}

impl InstallClaim {
    fn try_claim<S, F>(
        system: tina::SystemIncarnation,
        origin: OriginKey,
    ) -> Result<Self, KeepalivePoolInstallError<S, F>>
    where
        S: Shard + Send + 'static,
        F: MailboxFactory + Send + 'static,
    {
        let key = ClaimKey {
            system: system.get(),
            origin: origin.clone(),
        };
        let mut set = installed_origins()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if !set.insert(key.clone()) {
            return Err(KeepalivePoolInstallError::Conflict { origin });
        }
        Ok(Self { key: Some(key) })
    }

    fn release(&mut self) {
        if let Some(key) = self.key.take() {
            let mut set = installed_origins()
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            set.remove(&key);
        }
    }
}

fn installed_origins() -> &'static Mutex<HashSet<ClaimKey>> {
    static SET: OnceLock<Mutex<HashSet<ClaimKey>>> = OnceLock::new();
    SET.get_or_init(|| Mutex::new(HashSet::new()))
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn rollback_connections<S, F>(
    host: ThreadedRuntime<S, F>,
    connections: Vec<KeepaliveConnAddr>,
    claim: InstallClaim,
    timeout: Duration,
    forced_failure: Option<usize>,
) -> (
    KeepaliveInstallRollbackReport,
    Option<Box<KeepaliveInstallRecovery<S, F>>>,
)
where
    S: Shard + Send + 'static,
    F: MailboxFactory + Send + 'static,
{
    let count = connections.len();
    let mut recovery = KeepaliveInstallRecovery {
        host,
        connections,
        settled: vec![None; count],
        claim,
        forced_failure,
    };
    let report = recovery.attempt(deadline_after(timeout));
    if recovery.live_count() == 0 {
        recovery.claim.release();
        (report, None)
    } else {
        (report, Some(Box::new(recovery)))
    }
}

/// Best-effort pressure sample for exact lease counts.
///
/// Returns `Some(leased)` only on a real pressure reply. Never invents a
/// capacity-based guess.
fn observe_pool_leased<S, F>(
    host: &ThreadedRuntime<S, F>,
    handles: &KeepalivePoolHandles,
    deadline: Instant,
) -> Option<usize>
where
    S: Shard + Send + 'static,
    F: MailboxFactory + Send + 'static,
{
    let remaining = remaining(deadline)?;
    match call_with_deadline(host, handles.pool, WorkerPoolMsg::PressureReport, remaining) {
        Ok(CallOutcome::Replied(WorkerPoolReply::Pressure(report))) => Some(report.leased),
        _ => None,
    }
}

/// Map the last observed lease count into a drain timeout outcome.
///
/// `None` means no pressure sample landed — refuse to claim an exact leased
/// count (do not substitute pool capacity).
fn drain_timeout_outcome(last_leased: Option<usize>) -> KeepalivePoolDrainOutcome {
    KeepalivePoolDrainOutcome::TimedOut {
        leased: last_leased,
    }
}

fn wait_pool_drain<S, F>(
    host: &ThreadedRuntime<S, F>,
    handles: &KeepalivePoolHandles,
    deadline: Instant,
) -> Result<KeepalivePoolDrainOutcome, ThreadedRuntimeError>
where
    S: Shard + Send + 'static,
    F: MailboxFactory + Send + 'static,
{
    // Prefer real pressure observation. Never seed from connections.len()/capacity.
    let mut last_leased: Option<usize> = None;

    loop {
        let Some(remaining) = remaining(deadline) else {
            return Ok(drain_timeout_outcome(last_leased));
        };

        let pressure = match call_with_deadline(
            host,
            handles.pool,
            WorkerPoolMsg::PressureReport,
            remaining,
        ) {
            Ok(outcome) => outcome,
            Err(ThreadedRuntimeError::HostWaitTimeout) => {
                return Ok(drain_timeout_outcome(last_leased));
            }
            Err(error) => return Err(error),
        };

        match pressure {
            CallOutcome::Replied(WorkerPoolReply::Pressure(report)) => {
                last_leased = Some(report.leased);
                if report.leased == 0 {
                    return Ok(KeepalivePoolDrainOutcome::Drained);
                }
            }
            CallOutcome::Replied(_) => {
                return Ok(KeepalivePoolDrainOutcome::PressureUnavailable);
            }
            CallOutcome::Timeout => {
                // Pressure call timed out before a sample body. Report only if
                // an earlier sample was observed — never capacity.
                return Ok(drain_timeout_outcome(last_leased));
            }
            CallOutcome::Closed => {
                return Ok(KeepalivePoolDrainOutcome::PoolAlreadyClosed);
            }
            CallOutcome::Full | CallOutcome::Rejected(_) => {
                return Ok(KeepalivePoolDrainOutcome::PressureUnavailable);
            }
        }

        let nap = remaining.min(Duration::from_millis(10));
        if !nap.is_zero() {
            thread::sleep(nap);
        }
    }
}

fn map_owner_or_shutdown_error<S, F>(
    mut pool: InstalledKeepalivePool<S, F>,
    error: ThreadedRuntimeError,
    pending: KeepalivePendingCounts,
) -> KeepaliveCloseAndDrain<S, F>
where
    S: Shard + Send + 'static,
    F: MailboxFactory + Send + 'static,
{
    if error == ThreadedRuntimeError::WorkerStopped
        && host_state(&pool.host) == LiveShardState::Stopped
    {
        // A stopped worker is a shutdown settlement only when live
        // topology proves graceful owner shutdown.
        let settlement = KeepaliveShutdownSettlement {
            pool_close: if pending.admission_closed {
                KeepalivePoolCloseOutcome::Closed
            } else {
                KeepalivePoolCloseOutcome::AlreadyClosed
            },
            drain: KeepalivePoolDrainOutcome::PressureUnavailable,
            pending,
        };
        pool.claim.release();
        KeepaliveCloseAndDrain::Shutdown(settlement)
    } else {
        KeepaliveCloseAndDrain::OwnerFailed {
            pool,
            error,
            pending,
        }
    }
}

fn classify_closed_pool<S, F>(
    mut pool: InstalledKeepalivePool<S, F>,
    drain: KeepalivePoolDrainOutcome,
) -> KeepaliveCloseAndDrain<S, F>
where
    S: Shard + Send + 'static,
    F: MailboxFactory + Send + 'static,
{
    let pending = pool.pending(None);
    if host_state(&pool.host) == LiveShardState::Stopped {
        let settlement = KeepaliveShutdownSettlement {
            pool_close: KeepalivePoolCloseOutcome::AlreadyClosed,
            drain,
            pending,
        };
        pool.claim.release();
        KeepaliveCloseAndDrain::Shutdown(settlement)
    } else {
        KeepaliveCloseAndDrain::OwnerFailed {
            pool,
            error: ThreadedRuntimeError::WorkerUnresponsive,
            pending,
        }
    }
}

fn host_state<S, F>(host: &ThreadedRuntime<S, F>) -> LiveShardState
where
    S: Shard + Send + 'static,
    F: MailboxFactory + Send + 'static,
{
    host.topology()
        .shards()
        .first()
        .map_or(LiveShardState::Failed, |shard| shard.state())
}

fn deadline_after(timeout: Duration) -> Instant {
    tina::Deadline::from_instant(Instant::now(), timeout).instant()
}

fn remaining(deadline: Instant) -> Option<Duration> {
    deadline
        .checked_duration_since(Instant::now())
        .filter(|remaining| !remaining.is_zero())
}

fn call_with_deadline<S, F, M, R>(
    host: &ThreadedRuntime<S, F>,
    address: Address<M, R>,
    message: M,
    remaining: Duration,
) -> Result<CallOutcome<R>, ThreadedRuntimeError>
where
    S: Shard + Send + 'static,
    F: MailboxFactory + Send + 'static,
    M: Send + 'static,
    R: Send + 'static,
{
    host.call_blocking_with_host_timeout(address, message, remaining, remaining)
}

fn classify_connection_stop(
    result: Result<CallOutcome<KeepaliveOutcome>, ThreadedRuntimeError>,
) -> KeepaliveConnectionStopOutcome {
    match result {
        Ok(CallOutcome::Replied(KeepaliveOutcome::Stopped)) => {
            KeepaliveConnectionStopOutcome::Stopped
        }
        Ok(CallOutcome::Closed) => KeepaliveConnectionStopOutcome::AlreadyClosed,
        Ok(CallOutcome::Timeout) | Err(ThreadedRuntimeError::HostWaitTimeout) => {
            KeepaliveConnectionStopOutcome::TimedOut
        }
        Ok(CallOutcome::Full) | Err(ThreadedRuntimeError::CommandFull) => {
            KeepaliveConnectionStopOutcome::MailboxFull
        }
        Ok(CallOutcome::Rejected(reason)) => KeepaliveConnectionStopOutcome::Rejected(reason),
        Ok(CallOutcome::Replied(_)) | Err(_) => KeepaliveConnectionStopOutcome::UnexpectedReply,
    }
}

/// Install with a fault injected after `succeed_count` successful registrations.
///
/// Registration order is connection 0..capacity, then the pool isolate.
/// `succeed_count == 0` fails before any registration; `succeed_count == capacity`
/// fails on the pool registration after every connection has registered.
///
/// Intended for direct rollback proofs. Not a production control plane.
#[doc(hidden)]
pub fn install_keepalive_pool_fail_after<S, F>(
    system: &LocalSystem<S, F>,
    config: KeepalivePoolInstallConfig,
    succeed_count: usize,
) -> Result<InstalledKeepalivePool<S, F>, KeepalivePoolInstallError<S, F>>
where
    S: Shard + Send + 'static,
    F: MailboxFactory + Send + 'static,
{
    install_keepalive_pool_fail_after_inner(system, config, succeed_count, None)
}

/// Fault-injected install whose first rollback attempt fails one stop slot.
#[doc(hidden)]
pub fn install_keepalive_pool_fail_after_with_rollback_failure<S, F>(
    system: &LocalSystem<S, F>,
    config: KeepalivePoolInstallConfig,
    succeed_count: usize,
    stop_index: usize,
) -> Result<InstalledKeepalivePool<S, F>, KeepalivePoolInstallError<S, F>>
where
    S: Shard + Send + 'static,
    F: MailboxFactory + Send + 'static,
{
    install_keepalive_pool_fail_after_inner(system, config, succeed_count, Some(stop_index))
}

fn install_keepalive_pool_fail_after_inner<S, F>(
    system: &LocalSystem<S, F>,
    config: KeepalivePoolInstallConfig,
    succeed_count: usize,
    forced_rollback_failure: Option<usize>,
) -> Result<InstalledKeepalivePool<S, F>, KeepalivePoolInstallError<S, F>>
where
    S: Shard + Send + 'static,
    F: MailboxFactory + Send + 'static,
{
    config
        .validate()
        .map_err(KeepalivePoolInstallError::InvalidConfig)?;
    record_install_resource_boundary();

    let origin = OriginKey::from_target(&config.target);
    let claim = InstallClaim::try_claim(system.system_incarnation(), origin.clone())?;
    let host = system.host_control();
    let mut succeeded = 0usize;
    let mut connections: Vec<KeepaliveConnAddr> = Vec::with_capacity(config.pool_config.capacity);

    for index in 0..config.pool_config.capacity {
        if succeeded >= succeed_count {
            let (rollback, recovery) = rollback_connections(
                host.host_control(),
                connections,
                claim,
                Duration::from_secs(2),
                forced_rollback_failure,
            );
            return Err(KeepalivePoolInstallError::Register {
                failed_at: KeepaliveInstallStep::Connection { index },
                source: ThreadedRuntimeError::CommandFull,
                rollback,
                recovery,
            });
        }
        let conn = KeepaliveConnection::<S>::new(config.target.clone(), config.client_config);
        match system.register_root::<_, Infallible>(conn, config.connection_mailbox_capacity) {
            Ok(address) => {
                connections.push(address);
                succeeded += 1;
            }
            Err(source) => {
                let (rollback, recovery) = rollback_connections(
                    host.host_control(),
                    connections,
                    claim,
                    Duration::from_secs(2),
                    forced_rollback_failure,
                );
                return Err(KeepalivePoolInstallError::Register {
                    failed_at: KeepaliveInstallStep::Connection { index },
                    source,
                    rollback,
                    recovery,
                });
            }
        }
    }

    if succeeded >= succeed_count {
        let (rollback, recovery) = rollback_connections(
            host.host_control(),
            connections,
            claim,
            Duration::from_secs(2),
            forced_rollback_failure,
        );
        return Err(KeepalivePoolInstallError::Register {
            failed_at: KeepaliveInstallStep::Pool,
            source: ThreadedRuntimeError::CommandFull,
            rollback,
            recovery,
        });
    }

    let pool: WorkerPool<KeepaliveConnAddr, S> =
        WorkerPool::new(config.pool_config, connections.clone());
    match system.register_root::<_, Infallible>(pool, config.pool_mailbox_capacity) {
        Ok(pool_address) => Ok(InstalledKeepalivePool {
            handles: KeepalivePoolHandles {
                pool: pool_address,
                connections,
            },
            origin,
            claim,
            host,
            admission_closed: false,
            connection_settled: vec![None; config.pool_config.capacity],
            _shard: PhantomData,
        }),
        Err(source) => {
            let (rollback, recovery) = rollback_connections(
                host.host_control(),
                connections,
                claim,
                Duration::from_secs(2),
                forced_rollback_failure,
            );
            Err(KeepalivePoolInstallError::Register {
                failed_at: KeepaliveInstallStep::Pool,
                source,
                rollback,
                recovery,
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn invalid_max_plus_one_never_crosses_resource_boundary() {
        INSTALL_RESOURCE_BOUNDARY_ENTRIES.store(0, std::sync::atomic::Ordering::SeqCst);
        let system =
            LocalSystem::single_shard(SingleShard, tina_runtime::DefaultThreadedMailboxFactory)
                .try_build()
                .expect("system");
        let invalid = KeepalivePoolInstallConfig::new(
            HttpTarget::http("127.0.0.1:9".parse().unwrap()),
            HttpClientConfig::pressure(),
            PoolConfig::new(1, MAX_KEEPALIVE_POOL_WAITERS + 1),
            8,
            8,
        );
        assert!(matches!(
            system.install_keepalive_pool(invalid),
            Err(KeepalivePoolInstallError::InvalidConfig(
                KeepalivePoolConfigError::TooLarge { .. }
            ))
        ));
        assert_eq!(
            INSTALL_RESOURCE_BOUNDARY_ENTRIES.load(std::sync::atomic::Ordering::SeqCst),
            0
        );
        let _ = system.shutdown().join();
    }

    #[test]
    fn drain_timeout_before_pressure_sample_does_not_report_capacity() {
        // capacity >= 2, one lease held. Timeout before any pressure sample
        // (or with pressure CallOutcome::Timeout and no prior sample) must not
        // invent capacity as leased. Re-seed last_leased from capacity to see
        // this fail.
        let capacity = 2usize;
        assert_ne!(
            drain_timeout_outcome(None),
            KeepalivePoolDrainOutcome::TimedOut {
                leased: Some(capacity)
            },
            "unobserved timeout must not report capacity as leased"
        );
        assert_eq!(
            drain_timeout_outcome(None),
            KeepalivePoolDrainOutcome::TimedOut { leased: None }
        );
        // Observed exact lease count under capacity 2.
        assert_eq!(
            drain_timeout_outcome(Some(1)),
            KeepalivePoolDrainOutcome::TimedOut { leased: Some(1) }
        );
    }
}
