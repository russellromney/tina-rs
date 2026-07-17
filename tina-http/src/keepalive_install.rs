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
//! Installation is atomic: a partial registration rolls every installed
//! connection back and returns a typed rollback report. A second install for
//! the same origin on the same system incarnation returns a typed conflict.
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
    CallOutcome, LocalSystem, MailboxFactory, ThreadedRuntime, ThreadedRuntimeError,
};

use crate::keepalive::{
    KeepaliveConnAddr, KeepaliveConnection, KeepaliveConnectionMsg, KeepaliveConnectionStopFailure,
    KeepaliveConnectionStopOutcome, KeepaliveOutcome, KeepalivePoolCloseOutcome,
    KeepalivePoolDrainOutcome, KeepalivePoolHandles, OriginKey,
};
use crate::target::HttpTarget;
use crate::types::HttpClientConfig;

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

/// Failure to install a keepalive pool on a live owner.
#[derive(Debug)]
pub enum KeepalivePoolInstallError {
    /// Config refused before any resource was created.
    InvalidConfig(KeepalivePoolConfigError),
    /// An install for this origin is already live on this system incarnation.
    Conflict {
        /// Origin that already owns an install claim.
        origin: OriginKey,
    },
    /// A registration failed; every previously installed resource was rolled back.
    Register {
        /// Step that failed.
        failed_at: KeepaliveInstallStep,
        /// Underlying registration error.
        source: ThreadedRuntimeError,
        /// Rollback accounting for resources that had already registered.
        rollback: KeepaliveInstallRollbackReport,
    },
}

impl fmt::Display for KeepalivePoolInstallError {
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

impl std::error::Error for KeepalivePoolInstallError {
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
/// consuming [`Self::close_and_drain`].
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
    pub fn close_and_drain(mut self, timeout: Duration) -> KeepaliveCloseAndDrain<S, F> {
        let connections_live = self.handles.connections.len();
        // `leased` is only `Some` after a real pressure sample. Never seed it
        // from connection capacity — that lies when capacity > outstanding leases.
        let pending = |leased: Option<usize>, admission_closed: bool| KeepalivePendingCounts {
            leased,
            connections_live,
            admission_closed,
        };
        // Close-phase failures have not observed pressure yet.
        let unobserved = |admission_closed: bool| pending(None, admission_closed);

        if !self.admission_closed {
            let close = match self.host.call_blocking(
                self.handles.pool,
                WorkerPoolMsg::Close(CloseMode::Drain),
                timeout,
            ) {
                Ok(outcome) => outcome,
                Err(error) => {
                    return map_owner_or_shutdown_error(self, error, unobserved(false));
                }
            };

            match close {
                CallOutcome::Replied(WorkerPoolReply::Closed) => {
                    self.admission_closed = true;
                }
                CallOutcome::Closed => {
                    let settlement = KeepaliveShutdownSettlement {
                        pool_close: KeepalivePoolCloseOutcome::AlreadyClosed,
                        drain: KeepalivePoolDrainOutcome::PoolAlreadyClosed,
                        pending: unobserved(true),
                    };
                    // Drop releases the install claim; shutdown owns settlement.
                    drop(self);
                    return KeepaliveCloseAndDrain::Shutdown(settlement);
                }
                CallOutcome::Timeout => {
                    // Prefer a real pressure sample for exact TimedOut claims.
                    // If observation fails, refuse exact leased (None) — never
                    // substitute pool capacity.
                    let leased = observe_pool_leased(&self.host, &self.handles, timeout);
                    return KeepaliveCloseAndDrain::TimedOut {
                        pending: pending(leased, false),
                        pool: self,
                    };
                }
                CallOutcome::Full => {
                    return KeepaliveCloseAndDrain::OwnerFailed {
                        pending: unobserved(false),
                        error: ThreadedRuntimeError::CommandFull,
                        pool: self,
                    };
                }
                CallOutcome::Rejected(_) | CallOutcome::Replied(_) => {
                    return KeepaliveCloseAndDrain::OwnerFailed {
                        pending: unobserved(false),
                        error: ThreadedRuntimeError::WorkerUnresponsive,
                        pool: self,
                    };
                }
            }
        }

        let drain = match wait_pool_drain(&self.host, &self.handles, timeout) {
            Ok(outcome) => outcome,
            Err(error) => {
                return map_owner_or_shutdown_error(self, error, unobserved(true));
            }
        };

        match drain {
            KeepalivePoolDrainOutcome::Drained => {}
            KeepalivePoolDrainOutcome::TimedOut { leased } => {
                return KeepaliveCloseAndDrain::TimedOut {
                    pending: pending(leased, true),
                    pool: self,
                };
            }
            KeepalivePoolDrainOutcome::PoolAlreadyClosed
            | KeepalivePoolDrainOutcome::PressureUnavailable
            | KeepalivePoolDrainOutcome::SkippedAdmissionNotClosed
            | KeepalivePoolDrainOutcome::NotRequested => {
                // Prefer a late observation when drain could not prove truth.
                let leased = observe_pool_leased(&self.host, &self.handles, timeout);
                let settlement = KeepaliveShutdownSettlement {
                    pool_close: KeepalivePoolCloseOutcome::Closed,
                    drain,
                    pending: pending(leased, true),
                };
                drop(self);
                return KeepaliveCloseAndDrain::Shutdown(settlement);
            }
        }

        let mut stopped = 0usize;
        let mut already_closed = 0usize;
        let requested = self.handles.connections.len();

        for (index, conn) in self.handles.connections.iter().copied().enumerate() {
            let outcome = match self
                .host
                .call_blocking(conn, KeepaliveConnectionMsg::Stop, timeout)
            {
                Ok(CallOutcome::Replied(KeepaliveOutcome::Stopped)) => {
                    KeepaliveConnectionStopOutcome::Stopped
                }
                Ok(CallOutcome::Closed) => KeepaliveConnectionStopOutcome::AlreadyClosed,
                Ok(CallOutcome::Timeout) => {
                    // Drain already proved leased == 0 before stop phase.
                    return KeepaliveCloseAndDrain::TimedOut {
                        pending: pending(Some(0), true),
                        pool: self,
                    };
                }
                Ok(CallOutcome::Full) => {
                    return KeepaliveCloseAndDrain::OwnerFailed {
                        pending: pending(Some(0), true),
                        error: ThreadedRuntimeError::CommandFull,
                        pool: self,
                    };
                }
                Ok(CallOutcome::Rejected(_)) | Ok(CallOutcome::Replied(_)) => {
                    return KeepaliveCloseAndDrain::OwnerFailed {
                        pending: pending(Some(0), true),
                        error: ThreadedRuntimeError::WorkerUnresponsive,
                        pool: self,
                    };
                }
                Err(error) => {
                    return map_owner_or_shutdown_error(self, error, pending(Some(0), true));
                }
            };

            match outcome {
                KeepaliveConnectionStopOutcome::Stopped => stopped += 1,
                KeepaliveConnectionStopOutcome::AlreadyClosed => already_closed += 1,
                other => {
                    // Unexpected non-terminal path: keep the slot index for diagnostics
                    // by mapping into owner-failed without inventing a force path.
                    let _ = (index, other);
                    return KeepaliveCloseAndDrain::OwnerFailed {
                        pending: pending(Some(0), true),
                        error: ThreadedRuntimeError::WorkerUnresponsive,
                        pool: self,
                    };
                }
            }
        }

        // Claim released on drop; resources are settled.
        drop(self);
        KeepaliveCloseAndDrain::Drained(KeepalivePoolSettledReport {
            pool_close: KeepalivePoolCloseOutcome::Closed,
            drain: KeepalivePoolDrainOutcome::Drained,
            requested,
            stopped,
            already_closed,
        })
    }
}

impl<S, F> Drop for InstalledKeepalivePool<S, F>
where
    S: Shard + Send + 'static,
    F: MailboxFactory + Send + 'static,
{
    fn drop(&mut self) {
        self.claim.release();
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
    ) -> Result<InstalledKeepalivePool<Self::Shard, Self::Factory>, KeepalivePoolInstallError>;
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
    ) -> Result<InstalledKeepalivePool<S, F>, KeepalivePoolInstallError> {
        config
            .validate()
            .map_err(KeepalivePoolInstallError::InvalidConfig)?;

        let origin = OriginKey::from_target(&config.target);
        let mut claim = InstallClaim::try_claim(self.system_incarnation(), origin.clone())?;
        let host = self.host_control();

        let mut connections: Vec<KeepaliveConnAddr> =
            Vec::with_capacity(config.pool_config.capacity);

        for index in 0..config.pool_config.capacity {
            let conn = KeepaliveConnection::<S>::new(config.target.clone(), config.client_config);
            match self.register_root::<_, Infallible>(conn, config.connection_mailbox_capacity) {
                Ok(address) => connections.push(address),
                Err(source) => {
                    let rollback =
                        rollback_connections(&host, &connections, Duration::from_secs(2));
                    claim.release();
                    return Err(KeepalivePoolInstallError::Register {
                        failed_at: KeepaliveInstallStep::Connection { index },
                        source,
                        rollback,
                    });
                }
            }
        }

        let pool: WorkerPool<KeepaliveConnAddr, S> =
            WorkerPool::new(config.pool_config, connections.clone());
        let pool_address = match self
            .register_root::<_, Infallible>(pool, config.pool_mailbox_capacity)
        {
            Ok(address) => address,
            Err(source) => {
                let rollback = rollback_connections(&host, &connections, Duration::from_secs(2));
                claim.release();
                return Err(KeepalivePoolInstallError::Register {
                    failed_at: KeepaliveInstallStep::Pool,
                    source,
                    rollback,
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
            _shard: PhantomData,
        })
    }
}

/// Install on a raw [`ThreadedRuntime`]. Same contract as the LocalSystem path.
pub fn install_keepalive_pool_on_runtime<S, F>(
    runtime: &ThreadedRuntime<S, F>,
    config: KeepalivePoolInstallConfig,
) -> Result<InstalledKeepalivePool<S, F>, KeepalivePoolInstallError>
where
    S: Shard + Send + 'static,
    F: MailboxFactory + Send + 'static,
{
    config
        .validate()
        .map_err(KeepalivePoolInstallError::InvalidConfig)?;

    let origin = OriginKey::from_target(&config.target);
    let mut claim = InstallClaim::try_claim(runtime.system_incarnation(), origin.clone())?;
    let host = runtime.host_control();

    let mut connections: Vec<KeepaliveConnAddr> = Vec::with_capacity(config.pool_config.capacity);

    for index in 0..config.pool_config.capacity {
        let conn = KeepaliveConnection::<S>::new(config.target.clone(), config.client_config);
        match runtime
            .register_with_capacity::<_, Infallible>(conn, config.connection_mailbox_capacity)
        {
            Ok(address) => connections.push(address),
            Err(source) => {
                let rollback = rollback_connections(&host, &connections, Duration::from_secs(2));
                claim.release();
                return Err(KeepalivePoolInstallError::Register {
                    failed_at: KeepaliveInstallStep::Connection { index },
                    source,
                    rollback,
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
                let rollback = rollback_connections(&host, &connections, Duration::from_secs(2));
                claim.release();
                return Err(KeepalivePoolInstallError::Register {
                    failed_at: KeepaliveInstallStep::Pool,
                    source,
                    rollback,
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
    fn try_claim(
        system: tina::SystemIncarnation,
        origin: OriginKey,
    ) -> Result<Self, KeepalivePoolInstallError> {
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

impl Drop for InstallClaim {
    fn drop(&mut self) {
        self.release();
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
    host: &ThreadedRuntime<S, F>,
    connections: &[KeepaliveConnAddr],
    timeout: Duration,
) -> KeepaliveInstallRollbackReport
where
    S: Shard + Send + 'static,
    F: MailboxFactory + Send + 'static,
{
    let mut report = KeepaliveInstallRollbackReport {
        connections_registered: connections.len(),
        connections_stopped: 0,
        connections_already_closed: 0,
        connection_stop_failures: Vec::new(),
        pool_registered: false,
    };

    for (index, conn) in connections.iter().copied().enumerate() {
        let outcome = match host.call_blocking(conn, KeepaliveConnectionMsg::Stop, timeout) {
            Ok(CallOutcome::Replied(KeepaliveOutcome::Stopped)) => {
                KeepaliveConnectionStopOutcome::Stopped
            }
            Ok(CallOutcome::Closed) => KeepaliveConnectionStopOutcome::AlreadyClosed,
            Ok(CallOutcome::Timeout) => KeepaliveConnectionStopOutcome::TimedOut,
            Ok(CallOutcome::Full) => KeepaliveConnectionStopOutcome::MailboxFull,
            Ok(CallOutcome::Rejected(reason)) => KeepaliveConnectionStopOutcome::Rejected(reason),
            Ok(CallOutcome::Replied(_)) => KeepaliveConnectionStopOutcome::UnexpectedReply,
            // Owner failure during rollback still records the slot as failed.
            Err(_) => KeepaliveConnectionStopOutcome::TimedOut,
        };
        match outcome {
            KeepaliveConnectionStopOutcome::Stopped => report.connections_stopped += 1,
            KeepaliveConnectionStopOutcome::AlreadyClosed => {
                report.connections_already_closed += 1;
            }
            other => report
                .connection_stop_failures
                .push(KeepaliveConnectionStopFailure {
                    index,
                    outcome: other,
                }),
        }
    }

    report
}

/// Best-effort pressure sample for exact lease counts.
///
/// Returns `Some(leased)` only on a real pressure reply. Never invents a
/// capacity-based guess.
fn observe_pool_leased<S, F>(
    host: &ThreadedRuntime<S, F>,
    handles: &KeepalivePoolHandles,
    timeout: Duration,
) -> Option<usize>
where
    S: Shard + Send + 'static,
    F: MailboxFactory + Send + 'static,
{
    if timeout.is_zero() {
        return None;
    }
    match host.call_blocking(handles.pool, WorkerPoolMsg::PressureReport, timeout) {
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
    timeout: Duration,
) -> Result<KeepalivePoolDrainOutcome, ThreadedRuntimeError>
where
    S: Shard + Send + 'static,
    F: MailboxFactory + Send + 'static,
{
    let deadline = tina::Deadline::from_instant(Instant::now(), timeout).instant();
    // Prefer real pressure observation. Never seed from connections.len()/capacity.
    let mut last_leased: Option<usize> = None;

    loop {
        let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
            return Ok(drain_timeout_outcome(last_leased));
        };
        if remaining.is_zero() {
            return Ok(drain_timeout_outcome(last_leased));
        }

        let pressure =
            match host.call_blocking(handles.pool, WorkerPoolMsg::PressureReport, remaining) {
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
    pool: InstalledKeepalivePool<S, F>,
    error: ThreadedRuntimeError,
    pending: KeepalivePendingCounts,
) -> KeepaliveCloseAndDrain<S, F>
where
    S: Shard + Send + 'static,
    F: MailboxFactory + Send + 'static,
{
    match error {
        ThreadedRuntimeError::WorkerStopped => {
            // Shutdown or worker death: do not pretend a drain completed.
            let settlement = KeepaliveShutdownSettlement {
                pool_close: if pending.admission_closed {
                    KeepalivePoolCloseOutcome::Closed
                } else {
                    KeepalivePoolCloseOutcome::AlreadyClosed
                },
                drain: KeepalivePoolDrainOutcome::PressureUnavailable,
                pending,
            };
            drop(pool);
            KeepaliveCloseAndDrain::Shutdown(settlement)
        }
        other => KeepaliveCloseAndDrain::OwnerFailed {
            pool,
            error: other,
            pending,
        },
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
) -> Result<InstalledKeepalivePool<S, F>, KeepalivePoolInstallError>
where
    S: Shard + Send + 'static,
    F: MailboxFactory + Send + 'static,
{
    config
        .validate()
        .map_err(KeepalivePoolInstallError::InvalidConfig)?;

    let origin = OriginKey::from_target(&config.target);
    let mut claim = InstallClaim::try_claim(system.system_incarnation(), origin.clone())?;
    let host = system.host_control();
    let mut succeeded = 0usize;
    let mut connections: Vec<KeepaliveConnAddr> = Vec::with_capacity(config.pool_config.capacity);

    for index in 0..config.pool_config.capacity {
        if succeeded >= succeed_count {
            let rollback = rollback_connections(&host, &connections, Duration::from_secs(2));
            claim.release();
            return Err(KeepalivePoolInstallError::Register {
                failed_at: KeepaliveInstallStep::Connection { index },
                source: ThreadedRuntimeError::CommandFull,
                rollback,
            });
        }
        let conn = KeepaliveConnection::<S>::new(config.target.clone(), config.client_config);
        match system.register_root::<_, Infallible>(conn, config.connection_mailbox_capacity) {
            Ok(address) => {
                connections.push(address);
                succeeded += 1;
            }
            Err(source) => {
                let rollback = rollback_connections(&host, &connections, Duration::from_secs(2));
                claim.release();
                return Err(KeepalivePoolInstallError::Register {
                    failed_at: KeepaliveInstallStep::Connection { index },
                    source,
                    rollback,
                });
            }
        }
    }

    if succeeded >= succeed_count {
        let rollback = rollback_connections(&host, &connections, Duration::from_secs(2));
        claim.release();
        return Err(KeepalivePoolInstallError::Register {
            failed_at: KeepaliveInstallStep::Pool,
            source: ThreadedRuntimeError::CommandFull,
            rollback,
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
            _shard: PhantomData,
        }),
        Err(source) => {
            let rollback = rollback_connections(&host, &connections, Duration::from_secs(2));
            claim.release();
            Err(KeepalivePoolInstallError::Register {
                failed_at: KeepaliveInstallStep::Pool,
                source,
                rollback,
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
