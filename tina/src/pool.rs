//! Bounded pool vocabulary.
//!
//! Pure data types. The concrete `WorkerPool` lives in `tina-runtime`.
//!
//! ```text
//! borrow thing.
//! use thing.
//! return thing, or retire thing.
//! if no thing, say Full.
//! if pool closed, say Closed.
//! if wait too long, say Timeout.
//! never pretend thing came back by magic.
//! ```

use std::num::NonZeroU64;
use std::time::Duration;

/// Pool configuration. `capacity` is resource count; `max_waiters`
/// caps parked callers. `acquire_timeout` is guidance for the caller's
/// `call(...)` timeout — the pool does not enforce its own.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PoolConfig {
    /// Number of resources the pool owns. Must be > 0.
    pub capacity: usize,
    /// Maximum number of parked waiters. Zero means shed immediately
    /// when all resources are busy.
    pub max_waiters: usize,
    /// Suggested upper bound for callers' `call(...)` timeout.
    pub acquire_timeout: Duration,
}

impl PoolConfig {
    /// Construct. The concrete pool validates `capacity > 0` at build.
    pub const fn new(capacity: usize, max_waiters: usize, acquire_timeout: Duration) -> Self {
        Self {
            capacity,
            max_waiters,
            acquire_timeout,
        }
    }
}

/// Pool identity. Stamped into every lease so a release that names
/// the wrong pool is rejected.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct PoolId(NonZeroU64);

impl PoolId {
    /// Wrap a raw id. Internal.
    #[doc(hidden)]
    pub const fn from_raw(raw: NonZeroU64) -> Self {
        Self(raw)
    }

    /// Raw id.
    pub const fn get(self) -> NonZeroU64 {
        self.0
    }
}

/// Index of one resource slot inside a pool.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ResourceId(u32);

impl ResourceId {
    /// Wrap a raw index. Internal.
    #[doc(hidden)]
    pub const fn from_raw(raw: u32) -> Self {
        Self(raw)
    }

    /// Raw index.
    pub const fn get(self) -> u32 {
        self.0
    }
}

/// Move-only lease for one borrowed resource.
///
/// `H` is the cheap-clone resource handle the user works with (e.g.
/// [`crate::Address`]). The lease holds an owned `H` plus identity:
/// pool id, resource id, generation. Release consumes the lease.
///
/// Not `Clone`, not `Copy`. A lease is not a call-cancel handle and
/// cannot release a runtime call; a [`crate::CallHandle`] cannot
/// release a lease.
#[must_use = "PoolLease must be returned via Release or Retire — \
              dropping a lease leaks the resource until pool close"]
pub struct PoolLease<H> {
    pool_id: PoolId,
    resource_id: ResourceId,
    generation: u64,
    handle: H,
}

impl<H> PoolLease<H> {
    /// Mint a lease. Internal.
    #[doc(hidden)]
    pub fn new(pool_id: PoolId, resource_id: ResourceId, generation: u64, handle: H) -> Self {
        Self {
            pool_id,
            resource_id,
            generation,
            handle,
        }
    }

    /// Pool this lease belongs to.
    pub fn pool_id(&self) -> PoolId {
        self.pool_id
    }

    /// Resource slot this lease names.
    pub fn resource_id(&self) -> ResourceId {
        self.resource_id
    }

    /// Generation under which this lease was minted.
    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// Reference to the resource handle.
    pub fn handle(&self) -> &H {
        &self.handle
    }

    /// Consume into parts. Internal: pools use this to validate releases.
    #[doc(hidden)]
    pub fn into_parts(self) -> (PoolId, ResourceId, u64, H) {
        (self.pool_id, self.resource_id, self.generation, self.handle)
    }
}

impl<H: std::fmt::Debug> std::fmt::Debug for PoolLease<H> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PoolLease")
            .field("pool_id", &self.pool_id)
            .field("resource_id", &self.resource_id)
            .field("generation", &self.generation)
            .field("handle", &self.handle)
            .finish()
    }
}

/// Outcome of one acquire. `Full` / `Closed` / `Timeout` stay distinct.
#[must_use = "AcquireOutcome reports whether the pool actually handed \
              out a resource — ignoring it leaks the lease or hides \
              backpressure"]
pub enum AcquireOutcome<H> {
    /// Got a resource.
    Acquired(PoolLease<H>),
    /// All resources busy, waiter table full (or `max_waiters == 0`).
    /// Caller did not occupy a waiter slot.
    Full,
    /// Pool is closed.
    Closed,
    /// Wait expired before a resource came free.
    Timeout,
}

impl<H> AcquireOutcome<H> {
    /// Map only the resource handle inside `Acquired`, preserving
    /// lease identity. Other variants pass through.
    pub fn map_acquired<U>(self, f: impl FnOnce(H) -> U) -> AcquireOutcome<U> {
        match self {
            Self::Acquired(lease) => {
                let (pool_id, resource_id, generation, handle) = lease.into_parts();
                AcquireOutcome::Acquired(PoolLease::new(
                    pool_id,
                    resource_id,
                    generation,
                    f(handle),
                ))
            }
            Self::Full => AcquireOutcome::Full,
            Self::Closed => AcquireOutcome::Closed,
            Self::Timeout => AcquireOutcome::Timeout,
        }
    }

    /// Reason for non-acquired variants. `None` for `Acquired`.
    pub fn pressure_reason(&self) -> Option<PoolPressureReason> {
        match self {
            Self::Acquired(_) => None,
            Self::Full => Some(PoolPressureReason::Full),
            Self::Closed => Some(PoolPressureReason::Closed),
            Self::Timeout => Some(PoolPressureReason::Timeout),
        }
    }
}

impl<H: std::fmt::Debug> std::fmt::Debug for AcquireOutcome<H> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Acquired(lease) => f.debug_tuple("Acquired").field(lease).finish(),
            Self::Full => f.write_str("Full"),
            Self::Closed => f.write_str("Closed"),
            Self::Timeout => f.write_str("Timeout"),
        }
    }
}

/// Why an acquire did not yield a resource.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PoolPressureReason {
    /// Resources busy, waiter table full.
    Full,
    /// Pool closed.
    Closed,
    /// Wait expired.
    Timeout,
}

/// Caller-owned disposition for a release.
///
/// `Reuse` says caller believes the resource is healthy. `Retire`
/// says it is not. The pool may override `Reuse` to retire a known-bad
/// resource, but reports that override via [`ReleaseOutcome`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReleaseDisposition {
    /// Caller believes resource is healthy. Pool reuses it.
    Reuse,
    /// Caller believes resource is unhealthy. Pool drops it.
    Retire,
}

/// Outcome of one release.
#[must_use = "ReleaseOutcome reports whether the pool actually \
              accepted the release — ignoring it can hide \
              double-release / stale-lease bugs"]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReleaseOutcome {
    /// Pool accepted; resource went back to idle (or to next waiter).
    Released,
    /// Pool accepted and dropped the resource.
    Retired,
    /// Pool id, resource id, or generation did not match anything live.
    StaleLease,
    /// Pool already saw a release for this `(resource_id, generation)`.
    DoubleRelease,
    /// Pool was force-closed; outstanding leases are stale.
    PoolClosed,
}

/// How a pool close behaves.
///
/// `Drain`: stop new acquires, settle waiters as `Closed`, let
/// outstanding leases return normally.
///
/// `Force`: stop new acquires, settle waiters as `Closed`, mark
/// outstanding leases stale.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CloseMode {
    /// Soft close. Outstanding leases return normally.
    Drain,
    /// Hard close. Outstanding leases are stale.
    Force,
}

/// Snapshot of pool pressure.
#[must_use = "PoolPressureReport carries pool-state truth — discarding \
              it hides backpressure visibility"]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PoolPressureReport {
    /// Configured resource capacity.
    pub capacity: usize,
    /// Resources currently idle.
    pub available: usize,
    /// Resources currently out on lease.
    pub leased: usize,
    /// Callers parked waiting for a resource.
    pub waiters: usize,
    /// Configured waiter capacity.
    pub max_waiters: usize,
    /// Cumulative `Full` outcomes.
    pub full_count: u64,
    /// Cumulative `Timeout` outcomes (caller's `call` timeout closed
    /// the deferred slot).
    pub timeout_count: u64,
    /// Cumulative waiters reclaimed because the caller cancelled via
    /// `cancel_call(handle)` before the pool replied.
    pub cancel_count: u64,
    /// Cumulative resources dropped via `Retire` or pool override.
    pub retired_count: u64,
    /// Cumulative `Closed` outcomes settled for parked waiters.
    pub closed_count: u64,
    /// True once a [`CloseMode`] has been applied.
    pub closed: bool,
}
