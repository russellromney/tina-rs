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
//! if cross shard, say WrongShard.
//! never pretend thing came back by magic.
//! ```
//!
//! # Caller-side timeout
//!
//! There is no `AcquireOutcome::Timeout`. The pool does not enforce a
//! waiter deadline. When a caller's `call(pool, Acquire, timeout)`
//! expires, the runtime delivers `CallOutcome::Timeout` to the caller
//! and closes the pool's deferred reply slot. The pool's next sweep
//! reclaims the waiter slot and bumps `cancel_count` (cancel and
//! caller-timeout share the same in-pool reclaim path; the per-cause
//! breakdown is a runtime trace fact).
//!
//! # Pool mailbox is a second bound
//!
//! `max_waiters` caps parked callers, but the pool isolate's *mailbox
//! capacity* (set at registration) caps how many `Acquire` messages
//! can be in flight before a caller's `call(...)` returns
//! `CallOutcome::Full` from the runtime layer. Size the mailbox to
//! `>= max_waiters + burst` to avoid surprises.

use std::num::NonZeroU64;

/// Pool configuration. `capacity` is resource count; `max_waiters`
/// caps parked callers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PoolConfig {
    /// Number of resources the pool owns. Must be > 0.
    pub capacity: usize,
    /// Maximum number of parked waiters. Zero means shed immediately
    /// when all resources are busy.
    pub max_waiters: usize,
}

impl PoolConfig {
    /// Construct. The concrete pool validates `capacity > 0` at build.
    pub const fn new(capacity: usize, max_waiters: usize) -> Self {
        Self {
            capacity,
            max_waiters,
        }
    }
}

/// Pool identity. Stamped into every lease so a release that names
/// the wrong pool is rejected. Constructed only via
/// [`runtime_internal`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct PoolId(NonZeroU64);

impl PoolId {
    /// Raw id.
    pub const fn get(self) -> NonZeroU64 {
        self.0
    }
}

/// Index of one resource slot inside a pool. Constructed only via
/// [`runtime_internal`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ResourceId(u32);

impl ResourceId {
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
/// Not `Clone`, not `Copy`, no public constructor — only the runtime
/// can mint one. A lease is not a call-cancel handle and cannot
/// release a runtime call; a [`crate::CallHandle`] cannot release a
/// lease.
#[must_use = "PoolLease must be returned via Release or Retire — \
              dropping a lease leaks the resource until pool close"]
pub struct PoolLease<H> {
    pool_id: PoolId,
    resource_id: ResourceId,
    generation: u64,
    handle: H,
}

impl<H> PoolLease<H> {
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

/// Outcome of one acquire. Variants stay distinct.
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
    /// Caller is on a different shard than the pool. First form does
    /// not support cross-shard pool use — the pool isolate runs on
    /// one shard and only same-shard callers can park as waiters.
    WrongShard,
}

impl<H> AcquireOutcome<H> {
    /// Map only the resource handle inside `Acquired`, preserving
    /// lease identity. Other variants pass through.
    pub fn map_acquired<U>(self, f: impl FnOnce(H) -> U) -> AcquireOutcome<U> {
        match self {
            Self::Acquired(lease) => {
                let new_handle = f(lease.handle);
                AcquireOutcome::Acquired(PoolLease {
                    pool_id: lease.pool_id,
                    resource_id: lease.resource_id,
                    generation: lease.generation,
                    handle: new_handle,
                })
            }
            Self::Full => AcquireOutcome::Full,
            Self::Closed => AcquireOutcome::Closed,
            Self::WrongShard => AcquireOutcome::WrongShard,
        }
    }

    /// Reason for non-acquired variants. `None` for `Acquired`.
    pub fn pressure_reason(&self) -> Option<PoolPressureReason> {
        match self {
            Self::Acquired(_) => None,
            Self::Full => Some(PoolPressureReason::Full),
            Self::Closed => Some(PoolPressureReason::Closed),
            Self::WrongShard => Some(PoolPressureReason::WrongShard),
        }
    }
}

impl<H: std::fmt::Debug> std::fmt::Debug for AcquireOutcome<H> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Acquired(lease) => f.debug_tuple("Acquired").field(lease).finish(),
            Self::Full => f.write_str("Full"),
            Self::Closed => f.write_str("Closed"),
            Self::WrongShard => f.write_str("WrongShard"),
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
    /// Caller on a different shard than the pool.
    WrongShard,
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
    /// Pool accepted and dropped the resource. Caller asked for
    /// `Retire`, or pool overrode `Reuse` because it knew the
    /// resource was stale.
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
/// outstanding leases return normally (caller's `Reuse` is honored
/// as `Released`; the resource sits idle but cannot be re-acquired).
///
/// `Force`: stop new acquires, settle waiters as `Closed`, mark
/// outstanding leases stale — late releases get `PoolClosed` and the
/// pool retires the resource immediately.
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
    /// Resources currently idle (and not retired).
    pub available: usize,
    /// Resources currently out on lease.
    pub leased: usize,
    /// Callers parked waiting for a resource.
    pub waiters: usize,
    /// Configured waiter capacity.
    pub max_waiters: usize,
    /// Cumulative `Full` outcomes.
    pub full_count: u64,
    /// Cumulative `Closed` outcomes — both parked-waiter shutdowns
    /// and post-close acquires.
    pub closed_count: u64,
    /// Cumulative `WrongShard` outcomes.
    pub wrong_shard_count: u64,
    /// Cumulative waiters reclaimed because the caller cancelled via
    /// `cancel_call(handle)` or the caller's `call(...)` timeout
    /// fired before the pool replied.
    pub cancel_count: u64,
    /// Cumulative resources dropped via `Retire` or pool override.
    pub retired_count: u64,
    /// Cumulative `Acquire` messages received via plain `send` (no
    /// reply path) and dropped. A non-zero value means a caller is
    /// using the pool API wrong.
    pub no_caller_drops: u64,
    /// Cumulative resources reclaimed because the caller cancelled
    /// between the pool's dispatch and the runtime delivering the
    /// `Acquired` reply (the deferred-reply rejection path). Pool
    /// re-marks the resource as Idle.
    pub dispatch_recovered: u64,
    /// True once a [`CloseMode`] has been applied.
    pub closed: bool,
}

/// Internals exposed for runtime crates.
///
/// Application code should not import this module. Constructors here
/// let only `tina-runtime` (and tests inside this crate's test target)
/// mint pool ids, resource ids, and leases — application code cannot
/// forge them.
pub mod runtime_internal {
    use super::{PoolId, PoolLease, ResourceId};
    use std::num::NonZeroU64;

    /// Mint a new [`PoolId`]. Runtime-internal.
    pub fn pool_id_from_raw(raw: NonZeroU64) -> PoolId {
        PoolId(raw)
    }

    /// Mint a new [`ResourceId`]. Runtime-internal.
    pub fn resource_id_from_raw(raw: u32) -> ResourceId {
        ResourceId(raw)
    }

    /// Mint a new [`PoolLease`]. Runtime-internal.
    pub fn lease_new<H>(
        pool_id: PoolId,
        resource_id: ResourceId,
        generation: u64,
        handle: H,
    ) -> PoolLease<H> {
        PoolLease {
            pool_id,
            resource_id,
            generation,
            handle,
        }
    }

    /// Consume a lease into its parts. Runtime-internal — pools use
    /// this to validate releases.
    pub fn lease_into_parts<H>(lease: PoolLease<H>) -> (PoolId, ResourceId, u64, H) {
        (
            lease.pool_id,
            lease.resource_id,
            lease.generation,
            lease.handle,
        )
    }
}
