//! Sharded service contracts (phase 053).
//!
//! Small Seastar-shaped local multi-shard primitives. These helpers are
//! contracts only: a deterministic key-to-shard map, a typed `ShardId ->
//! Address` table, a partial-aggregate report shape for bounded
//! scatter/gather, and a caller-owned hot-key attempt report. They do **not**
//! carry distributed-database, consensus, remoting, retry, or rebalancing
//! semantics. Placement, pressure, and partial failure stay user-visible.
//!
//! See `docs/tina-user-guide/10-service-patterns.md` for the surrounding
//! shape: the registry maps a service name to one address; the address may
//! resolve through a [`ShardServiceTable`].
//!
//! Near-grug: key bytes choose shard. Shard owns state. Owner re-checks the
//! key before mutate. Fanout is bounded. Aggregate says partial truth.

use std::collections::{BTreeMap, BTreeSet};
use std::convert::Infallible;
use std::error::Error;
use std::fmt;
use std::marker::PhantomData;
use std::time::Duration;

use tina::{
    Address, Context, Effect, Isolate, Outbound as TinaOutbound, Shard, ShardId, prelude::send,
};

use crate::call::RuntimeCall;

// ---------------------------------------------------------------
// Placement
// ---------------------------------------------------------------

/// Hashing scheme used by [`ShardPlacement::owner_for_bytes`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShardHashScheme {
    /// FNV-1a 64-bit on canonical key bytes, modulo the ordered shard list.
    ///
    /// FNV-1a is chosen because it is small, sync, no-alloc, and produces
    /// byte-identical mappings on live and simulated runtimes. It is **not**
    /// a cryptographic hash and **not** stable under shard-count changes.
    Fnv1a64ModCount,
}

impl ShardHashScheme {
    /// Version of the hash scheme. Bumped if the byte-level mapping changes.
    pub const VERSION: u32 = 1;
}

/// Errors when constructing a [`ShardPlacement`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ShardPlacementError {
    /// The shard list was empty. Placement requires at least one owner.
    Empty,
    /// The shard list contained the same id more than once.
    DuplicateShard(ShardId),
}

impl fmt::Display for ShardPlacementError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => write!(f, "shard placement requires at least one shard"),
            Self::DuplicateShard(id) => {
                write!(f, "shard placement saw duplicate shard id {}", id.get())
            }
        }
    }
}

impl Error for ShardPlacementError {}

/// Static report describing a [`ShardPlacement`].
///
/// Carries the placement name, hash scheme/version, and the ordered shard
/// list so live and simulator code can prove byte-identical placement.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShardPlacementReport {
    name: String,
    scheme: ShardHashScheme,
    version: u32,
    shards: Vec<ShardId>,
}

impl ShardPlacementReport {
    /// Returns the placement name.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Returns the hash scheme.
    pub fn scheme(&self) -> ShardHashScheme {
        self.scheme
    }

    /// Returns the hash scheme version.
    pub fn version(&self) -> u32 {
        self.version
    }

    /// Returns the ordered shard list.
    pub fn shards(&self) -> &[ShardId] {
        &self.shards
    }
}

/// Deterministic key-to-shard map over an explicit ordered shard list.
///
/// Placement is visible: the shard list, hash scheme, and version are part of
/// the public API. The map is stable while the shard list is unchanged. It
/// is **not** stable under shard-count changes; that requires a consistent-
/// hashing scheme which is not provided here.
///
/// Input keys are canonical bytes (or strings). Arbitrary `Hash` values are
/// not accepted, because their byte representation is not portable across
/// platforms or `Hasher` implementations.
#[derive(Debug, Clone)]
pub struct ShardPlacement {
    name: String,
    shards: Vec<ShardId>,
    scheme: ShardHashScheme,
}

impl ShardPlacement {
    /// Creates a placement over the given ordered shard list.
    ///
    /// Returns [`ShardPlacementError::Empty`] when the list is empty and
    /// [`ShardPlacementError::DuplicateShard`] on repeated ids. The shard
    /// list order is preserved verbatim (no sort).
    pub fn new(name: impl Into<String>, shards: Vec<ShardId>) -> Result<Self, ShardPlacementError> {
        if shards.is_empty() {
            return Err(ShardPlacementError::Empty);
        }
        let mut seen = BTreeSet::new();
        for shard in &shards {
            if !seen.insert(*shard) {
                return Err(ShardPlacementError::DuplicateShard(*shard));
            }
        }
        Ok(Self {
            name: name.into(),
            shards,
            scheme: ShardHashScheme::Fnv1a64ModCount,
        })
    }

    /// Returns the placement name.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Returns the ordered shard list.
    pub fn shards(&self) -> &[ShardId] {
        &self.shards
    }

    /// Returns the hash scheme.
    pub fn scheme(&self) -> ShardHashScheme {
        self.scheme
    }

    /// Returns the owner shard for a canonical byte key.
    ///
    /// The mapping is `fnv1a64(key) % shards.len()` over the ordered shard
    /// list. Live and simulator runtimes must call this on the same bytes
    /// to get byte-identical placement.
    pub fn owner_for_bytes(&self, key: &[u8]) -> ShardId {
        let h = fnv1a64(key);
        let idx = (h % self.shards.len() as u64) as usize;
        self.shards[idx]
    }

    /// Returns the owner shard for a string key (UTF-8 bytes).
    pub fn owner_for_str(&self, key: &str) -> ShardId {
        self.owner_for_bytes(key.as_bytes())
    }

    /// Returns `Ok(owner)` if `actual_shard` is the placement-derived
    /// owner of `key`, or `Err(WrongShard { expected, actual })` if not.
    ///
    /// Owner re-check is the load-bearing safety invariant for sharded
    /// service isolates: every keyed handler must check this before
    /// mutating shard-local state, otherwise a routing bug becomes
    /// silent state corruption. This helper folds the canonical
    /// `if owner != ctx.shard_id() { return Err(WrongShard { ... }) }`
    /// pattern into one call.
    pub fn require_owner_bytes(
        &self,
        key: &[u8],
        actual_shard: ShardId,
    ) -> Result<ShardId, WrongShard> {
        let owner = self.owner_for_bytes(key);
        if owner == actual_shard {
            Ok(owner)
        } else {
            Err(WrongShard {
                expected: owner,
                actual: actual_shard,
            })
        }
    }

    /// Returns `Ok(owner)` if `actual_shard` is the placement-derived
    /// owner of `key`, or `Err(WrongShard { expected, actual })` if not.
    /// String form of [`Self::require_owner_bytes`].
    pub fn require_owner_str(
        &self,
        key: &str,
        actual_shard: ShardId,
    ) -> Result<ShardId, WrongShard> {
        self.require_owner_bytes(key.as_bytes(), actual_shard)
    }

    /// Returns a static placement report.
    pub fn report(&self) -> ShardPlacementReport {
        ShardPlacementReport {
            name: self.name.clone(),
            scheme: self.scheme,
            version: ShardHashScheme::VERSION,
            shards: self.shards.clone(),
        }
    }
}

fn fnv1a64(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in bytes {
        h ^= *b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

// ---------------------------------------------------------------
// Service table
// ---------------------------------------------------------------

/// Returned when an isolate processes a key that does not belong to it.
///
/// Owners must re-check `placement.owner_for_bytes(key) == ctx.shard_id()`
/// before mutating keyed state. If the check fails, return this typed error
/// to the caller. The runtime cannot prove key ownership for the isolate;
/// only the isolate's handler can.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WrongShard {
    /// The shard the placement says should own the key.
    pub expected: ShardId,
    /// The shard that actually received the message.
    pub actual: ShardId,
}

impl fmt::Display for WrongShard {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "wrong shard for key: expected {}, actual {}",
            self.expected.get(),
            self.actual.get()
        )
    }
}

impl Error for WrongShard {}

/// Returned when a [`ShardServiceTable`] lookup names a shard that is not in
/// the table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MissingShard(pub ShardId);

impl fmt::Display for MissingShard {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "shard {} is not in the service table", self.0.get())
    }
}

impl Error for MissingShard {}

/// Errors when constructing a [`ShardServiceTable`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ShardServiceTableError {
    /// The entries list shard ids did not match the placement shard list,
    /// either by membership or by order.
    ShardListMismatch {
        /// The placement's ordered shard list.
        placement: Vec<ShardId>,
        /// The entries' ordered shard list.
        entries: Vec<ShardId>,
    },
}

impl fmt::Display for ShardServiceTableError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ShardListMismatch { placement, entries } => {
                write!(
                    f,
                    "shard service table entries {entries:?} do not match placement {placement:?}"
                )
            }
        }
    }
}

impl Error for ShardServiceTableError {}

/// Errors returned by [`ShardServiceTable::try_from_placement`].
///
/// `E` is the registration-closure error type (e.g.
/// `ThreadedRuntimeError`). `Register(e)` carries it through;
/// `Table(e)` is a structural mismatch from
/// [`ShardServiceTable::new`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ServiceTableBuildError<E> {
    /// One of the per-shard registrations failed.
    Register(E),
    /// The structural `new` invariant rejected the assembled entries.
    Table(ShardServiceTableError),
}

impl<E: fmt::Display> fmt::Display for ServiceTableBuildError<E> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Register(e) => write!(f, "shard registration failed: {e}"),
            Self::Table(e) => write!(f, "{e}"),
        }
    }
}

impl<E: Error + 'static> Error for ServiceTableBuildError<E> {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Register(e) => Some(e),
            Self::Table(e) => Some(e),
        }
    }
}

impl<E> From<ShardServiceTableError> for ServiceTableBuildError<E> {
    fn from(err: ShardServiceTableError) -> Self {
        Self::Table(err)
    }
}

/// Explicit ordered map from [`ShardId`] to a typed service [`Address`].
///
/// The table holds the same shard list, in the same order, as its placement.
/// There is no hidden registry thread, no `Arc<Mutex<HashMap<...>>>` side
/// table, and no automatic refresh. Addresses are captured at construction
/// time. If a service isolate restarts with a new generation, it is the
/// caller's responsibility to rebuild the table.
///
/// Lookup is O(log n) via an internal `BTreeMap<ShardId, usize>` index
/// built from `placement.shards()` at construction. `address_for_bytes`
/// is total over correct construction: every owner returned by
/// `placement.owner_for_bytes` is by definition one of `placement.shards()`,
/// and `new` builds the index from those exact shards. The implementation
/// still pessimistically guards the lookup with `expect`, so a future
/// refactor that breaks the parallel-array invariant fails loudly.
#[derive(Debug, Clone)]
pub struct ShardServiceTable<M: 'static, R: 'static = ()> {
    placement: ShardPlacement,
    addresses: Vec<Address<M, R>>,
    index: BTreeMap<ShardId, usize>,
}

impl<M: 'static, R: 'static> ShardServiceTable<M, R> {
    /// Creates a service table over the given placement and entries.
    ///
    /// The entries' shard ids must match the placement's shard list in the
    /// same order. Returns [`ShardServiceTableError::ShardListMismatch`]
    /// otherwise. This is a build-time invariant, not a runtime probe.
    pub fn new(
        placement: ShardPlacement,
        entries: Vec<(ShardId, Address<M, R>)>,
    ) -> Result<Self, ShardServiceTableError> {
        let entry_shards: Vec<ShardId> = entries.iter().map(|(s, _)| *s).collect();
        if entry_shards != placement.shards() {
            return Err(ShardServiceTableError::ShardListMismatch {
                placement: placement.shards().to_vec(),
                entries: entry_shards,
            });
        }
        let mut addresses = Vec::with_capacity(entries.len());
        let mut index = BTreeMap::new();
        for (i, (shard, address)) in entries.into_iter().enumerate() {
            addresses.push(address);
            index.insert(shard, i);
        }
        Ok(Self {
            placement,
            addresses,
            index,
        })
    }

    /// Builds a service table by registering one address per shard via
    /// the given closure. Use this with the explicit-step
    /// [`crate::MultiShardRuntime`], whose `register_with_capacity_on`
    /// returns `Address<M, R>` directly.
    ///
    /// ```ignore
    /// let table = ShardServiceTable::from_placement(placement, |shard| {
    ///     runtime.register_with_capacity_on::<Store, _>(shard, Store::new(), 32)
    /// })?;
    /// ```
    pub fn from_placement<F>(
        placement: ShardPlacement,
        mut register: F,
    ) -> Result<Self, ShardServiceTableError>
    where
        F: FnMut(ShardId) -> Address<M, R>,
    {
        let entries: Vec<(ShardId, Address<M, R>)> = placement
            .shards()
            .iter()
            .copied()
            .map(|s| (s, register(s)))
            .collect();
        Self::new(placement, entries)
    }

    /// Same as [`Self::from_placement`] but the registration closure
    /// can fail. Use this with [`crate::ThreadedMultiShardRuntime`],
    /// whose `register_with_capacity_on` returns
    /// `Result<Address<M, R>, ThreadedRuntimeError>`.
    ///
    /// ```ignore
    /// let table = ShardServiceTable::try_from_placement(placement, |shard| {
    ///     runtime.register_with_capacity_on::<Store, _>(shard, Store::new(), 32)
    /// })?;
    /// ```
    pub fn try_from_placement<F, E>(
        placement: ShardPlacement,
        mut register: F,
    ) -> Result<Self, ServiceTableBuildError<E>>
    where
        F: FnMut(ShardId) -> Result<Address<M, R>, E>,
    {
        let mut entries = Vec::with_capacity(placement.shards().len());
        for shard in placement.shards().iter().copied() {
            let address = register(shard).map_err(ServiceTableBuildError::Register)?;
            entries.push((shard, address));
        }
        Self::new(placement, entries).map_err(ServiceTableBuildError::Table)
    }

    /// Returns the underlying placement.
    pub fn placement(&self) -> &ShardPlacement {
        &self.placement
    }

    /// Returns the ordered address list, parallel to
    /// `placement().shards()`.
    pub fn addresses(&self) -> &[Address<M, R>] {
        &self.addresses
    }

    /// Looks up the address for `shard`, or returns [`MissingShard`].
    pub fn address_for(&self, shard: ShardId) -> Result<Address<M, R>, MissingShard>
    where
        Address<M, R>: Copy,
    {
        match self.index.get(&shard) {
            Some(i) => Ok(self.addresses[*i]),
            None => Err(MissingShard(shard)),
        }
    }

    /// Returns the service address that owns the given byte key.
    ///
    /// Total in correct usage: `placement.owner_for_bytes(key)` returns
    /// some `s in placement.shards()`, and `new` populated `self.index`
    /// with exactly those shards. The `expect` below is therefore
    /// unreachable when the constructor invariant holds, but it is kept
    /// as a structural guard rather than `unreachable!()` so a future
    /// refactor that breaks the invariant panics with a clear message
    /// instead of silently returning a stale address.
    pub fn address_for_bytes(&self, key: &[u8]) -> Address<M, R>
    where
        Address<M, R>: Copy,
    {
        let owner = self.placement.owner_for_bytes(key);
        let i = *self
            .index
            .get(&owner)
            .expect("placement owner is in service table index");
        self.addresses[i]
    }

    /// Returns the service address that owns the given string key.
    pub fn address_for_str(&self, key: &str) -> Address<M, R>
    where
        Address<M, R>: Copy,
    {
        self.address_for_bytes(key.as_bytes())
    }
}

// ---------------------------------------------------------------
// Scatter / gather
// ---------------------------------------------------------------

/// Errors when validating a [`ScatterGatherConfig`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScatterGatherConfigError {
    /// `max_targets` was zero. A scatter with no targets is a programmer
    /// error.
    MaxTargetsZero,
    /// The collector mailbox capacity was below `max_targets`. Reply
    /// collection must not silently grow; capacity must cover the worst
    /// case so partial failure stays visible.
    CollectorCapacityBelowMaxTargets {
        /// Configured collector capacity.
        collector_capacity: usize,
        /// Configured max targets.
        max_targets: usize,
    },
    /// `per_target_timeout` was zero. The runtime cannot prove timeouts
    /// against a zero deadline.
    PerTargetTimeoutZero,
    /// `aggregate_timeout` was below `per_target_timeout`. The aggregate
    /// must give at least one target deadline of room or partial reports
    /// become meaningless.
    AggregateTimeoutBelowPerTargetTimeout {
        /// Configured aggregate timeout.
        aggregate_timeout: Duration,
        /// Configured per-target timeout.
        per_target_timeout: Duration,
    },
}

impl fmt::Display for ScatterGatherConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MaxTargetsZero => write!(f, "scatter/gather max_targets must be > 0"),
            Self::CollectorCapacityBelowMaxTargets {
                collector_capacity,
                max_targets,
            } => write!(
                f,
                "scatter/gather collector_capacity {collector_capacity} < max_targets {max_targets}"
            ),
            Self::PerTargetTimeoutZero => {
                write!(f, "scatter/gather per_target_timeout must be > 0")
            }
            Self::AggregateTimeoutBelowPerTargetTimeout {
                aggregate_timeout,
                per_target_timeout,
            } => write!(
                f,
                "scatter/gather aggregate_timeout {aggregate_timeout:?} < per_target_timeout {per_target_timeout:?}"
            ),
        }
    }
}

impl Error for ScatterGatherConfigError {}

/// Bounded scatter/gather configuration.
///
/// All knobs are explicit. There is no automatic backoff and no hidden
/// reply queue. Callers that want retry must own the retry loop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScatterGatherConfig {
    /// Maximum number of targets per scatter. The result vector capacity is
    /// bounded by this value.
    pub max_targets: usize,
    /// Mailbox capacity for the reply-collecting isolate. Must be at least
    /// `max_targets` to keep partial failure visible.
    pub collector_capacity: usize,
    /// Per-target deadline. After this duration, a missing reply becomes
    /// [`ScatterGatherTargetOutcome::Timeout`].
    pub per_target_timeout: Duration,
    /// Aggregate deadline. After this duration, any still-missing replies
    /// become [`ScatterGatherTargetOutcome::AggregateTimeout`] and the
    /// fanout caller stops waiting.
    pub aggregate_timeout: Duration,
}

impl ScatterGatherConfig {
    /// Validates the config. Returns the typed error on bad combinations.
    pub fn validate(&self) -> Result<(), ScatterGatherConfigError> {
        if self.max_targets == 0 {
            return Err(ScatterGatherConfigError::MaxTargetsZero);
        }
        if self.collector_capacity < self.max_targets {
            return Err(ScatterGatherConfigError::CollectorCapacityBelowMaxTargets {
                collector_capacity: self.collector_capacity,
                max_targets: self.max_targets,
            });
        }
        if self.per_target_timeout.is_zero() {
            return Err(ScatterGatherConfigError::PerTargetTimeoutZero);
        }
        if self.aggregate_timeout < self.per_target_timeout {
            return Err(
                ScatterGatherConfigError::AggregateTimeoutBelowPerTargetTimeout {
                    aggregate_timeout: self.aggregate_timeout,
                    per_target_timeout: self.per_target_timeout,
                },
            );
        }
        Ok(())
    }
}

/// One target's outcome inside a [`ScatterGatherReport`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScatterGatherTargetOutcome<T> {
    /// Target replied within its per-target timeout.
    Replied(T),
    /// Target's mailbox was full at fanout time.
    Full,
    /// Target's address was closed at fanout time, or its shard was
    /// quarantined.
    Closed,
    /// Target did not reply within its per-target timeout.
    Timeout,
    /// The aggregate deadline fired before this target replied.
    AggregateTimeout,
    /// Target name was not present in the service table; see
    /// [`MissingShard`].
    MissingShard,
}

/// Partial-aggregate report from a scatter/gather fanout.
///
/// `outcomes` length is bounded by `config.max_targets`. There is no
/// hidden buffering; what is not in this report did not happen.
///
/// **Ordering contract:** `outcomes` preserves caller-supplied target
/// order. If the coordinator was started with target list
/// `[shard_a, shard_b, shard_c]`, the report's `outcomes` is `[(shard_a,
/// outcome_a), (shard_b, outcome_b), (shard_c, outcome_c)]` regardless of
/// the order in which replies arrived. This makes per-target results
/// addressable by index without a `BTreeMap` lookup, and keeps log
/// output deterministic across runs and seeds. Coordinator
/// implementations must preserve this order at emission time.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScatterGatherReport<T> {
    /// Config the fanout ran under.
    pub config: ScatterGatherConfig,
    /// Per-target outcomes in caller-supplied target-list order. See
    /// the struct doc for the ordering contract.
    ///
    /// The field is `pub` so callers can pattern-match, destructure,
    /// and construct reports directly in tests. Reordering this Vec
    /// after the coord emits the report **breaks the ordering
    /// contract**; downstream consumers must treat it as read-only,
    /// even when they own the `ScatterGatherReport` value.
    pub outcomes: Vec<(ShardId, ScatterGatherTargetOutcome<T>)>,
}

impl<T> ScatterGatherReport<T> {
    /// Returns the number of targets that replied.
    pub fn replied_count(&self) -> usize {
        self.outcomes
            .iter()
            .filter(|(_, o)| matches!(o, ScatterGatherTargetOutcome::Replied(_)))
            .count()
    }

    /// Returns the number of targets that returned [`Full`](ScatterGatherTargetOutcome::Full).
    pub fn full_count(&self) -> usize {
        self.outcomes
            .iter()
            .filter(|(_, o)| matches!(o, ScatterGatherTargetOutcome::Full))
            .count()
    }

    /// Returns the number of targets that returned [`Closed`](ScatterGatherTargetOutcome::Closed).
    pub fn closed_count(&self) -> usize {
        self.outcomes
            .iter()
            .filter(|(_, o)| matches!(o, ScatterGatherTargetOutcome::Closed))
            .count()
    }

    /// Returns the number of targets that returned [`Timeout`](ScatterGatherTargetOutcome::Timeout).
    pub fn timeout_count(&self) -> usize {
        self.outcomes
            .iter()
            .filter(|(_, o)| matches!(o, ScatterGatherTargetOutcome::Timeout))
            .count()
    }

    /// Returns the number of targets cut off by the aggregate timeout.
    pub fn aggregate_timeout_count(&self) -> usize {
        self.outcomes
            .iter()
            .filter(|(_, o)| matches!(o, ScatterGatherTargetOutcome::AggregateTimeout))
            .count()
    }

    /// Returns the number of targets that named a shard not in the service
    /// table.
    pub fn missing_shard_count(&self) -> usize {
        self.outcomes
            .iter()
            .filter(|(_, o)| matches!(o, ScatterGatherTargetOutcome::MissingShard))
            .count()
    }

    /// Iterator over `(ShardId, &T)` for targets that replied.
    pub fn replied(&self) -> impl Iterator<Item = (ShardId, &T)> {
        self.outcomes.iter().filter_map(|(s, o)| match o {
            ScatterGatherTargetOutcome::Replied(t) => Some((*s, t)),
            _ => None,
        })
    }
}

// ---------------------------------------------------------------
// Hot-key attempt report (caller-owned retry)
// ---------------------------------------------------------------

/// Per-attempt outcome for keyed sends/calls under hot-key pressure.
///
/// Retry is caller-owned. This enum names the cases an explicit retry loop
/// must distinguish so the report stays honest. Both intermediate
/// [`FullRetry`](Self::FullRetry) attempts and terminal outcomes
/// ([`RetrySucceeded`](Self::RetrySucceeded) /
/// [`RetryExhausted`](Self::RetryExhausted)) are recorded so the report
/// shows how much retry pressure the loop saw, not just the final verdict.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HotKeyAttemptOutcome {
    /// First-attempt accept.
    Accepted,
    /// First attempt was rejected `Full`.
    FullFirstAttempt,
    /// A retry was rejected `Full`.
    FullRetry,
    /// Target was closed during the attempt.
    Closed,
    /// Caller-owned deadline fired before the attempt was accepted.
    Timeout,
    /// Retry succeeded after one or more `Full` rejections.
    RetrySucceeded,
    /// Retry budget was exhausted before the attempt was accepted.
    RetryExhausted,
}

/// Summary report for a caller-owned retry loop against a hot owner shard.
///
/// All retry policy is explicit and visible. The runtime never retries on
/// the caller's behalf. Counters are partitioned so a non-zero
/// `full_retry_total` proves the loop actually retried (and was not
/// silently treated as a single attempt).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct HotKeyAttemptReport {
    /// First-attempt accepts (no retry needed).
    pub accepted_first_attempt: u32,
    /// First-attempt `Full` rejections (one per retry-loop entry that hit
    /// pressure on its first probe).
    pub full_first_attempt: u32,
    /// Total `Full` rejections observed during retry rounds (every
    /// in-loop retry that came back `Full`). Does not include the
    /// first-attempt rejection.
    pub full_retry_total: u32,
    /// Retry-loop entries that succeeded after one or more `Full`
    /// rejections.
    pub retry_succeeded: u32,
    /// Retry-loop entries that exhausted the retry budget.
    pub retry_exhausted: u32,
    /// Retry-loop entries that hit the caller-owned deadline.
    pub timeout: u32,
    /// Retry-loop entries that saw `Closed`.
    pub closed: u32,
}

impl HotKeyAttemptReport {
    /// Records one attempt outcome into the report.
    pub fn record(&mut self, outcome: HotKeyAttemptOutcome) {
        match outcome {
            HotKeyAttemptOutcome::Accepted => self.accepted_first_attempt += 1,
            HotKeyAttemptOutcome::FullFirstAttempt => self.full_first_attempt += 1,
            HotKeyAttemptOutcome::FullRetry => self.full_retry_total += 1,
            HotKeyAttemptOutcome::RetrySucceeded => self.retry_succeeded += 1,
            HotKeyAttemptOutcome::RetryExhausted => self.retry_exhausted += 1,
            HotKeyAttemptOutcome::Timeout => self.timeout += 1,
            HotKeyAttemptOutcome::Closed => self.closed += 1,
        }
    }

    /// Number of top-level retry-loop entries (one per hot-key attempt the
    /// caller drove). Each loop entry produces exactly one terminal
    /// outcome: `Accepted`, `RetrySucceeded`, `RetryExhausted`, `Timeout`,
    /// or `Closed`. Intermediate `FullRetry` records do **not** count
    /// here — they are tracked separately in `full_retry_total` so retry
    /// pressure stays visible.
    pub fn loop_entries(&self) -> u32 {
        self.accepted_first_attempt
            + self.retry_succeeded
            + self.retry_exhausted
            + self.timeout
            + self.closed
    }
}

// ---------------------------------------------------------------
// Reply adapter
// ---------------------------------------------------------------

/// Generic isolate that translates one message type into another and
/// forwards.
///
/// The structural reply-typing problem: a coordinator sends `M` to a
/// shard service that replies with `R`, but the coordinator's mailbox
/// is typed for some richer message type `T` that wraps `R` (e.g.
/// `T::Reply(R)`). Tina's typed addresses don't let an `Address<R>`
/// accept replies for `Address<T>`, so a small bridge isolate has to
/// translate. `ReplyAdapter<M, T, S>` is that bridge as a shipped
/// primitive instead of hand-written boilerplate.
///
/// `M: Into<T>` — the user provides one `impl From<M> for T` and the
/// adapter does the rest. `register_on` registers the adapter on a
/// shard with explicit mailbox capacity and returns the
/// `Address<M>` callers send to. There is no hidden queue beyond the
/// adapter's mailbox; capacity is explicit and `Full`/`Closed` surface
/// through the runtime's normal send behavior.
///
/// Same-shard use is the common case (a coord registers its adapter
/// on its own shard so reply translation is local). Cross-shard use is
/// supported as long as both `M` and `T` are `Send`.
pub struct ReplyAdapter<M, T, S>
where
    M: 'static,
    T: 'static + From<M>,
    S: Shard + 'static,
{
    target: Address<T>,
    // `fn(M)` rather than `M`: the adapter consumes M (via .into()) but
    // never stores one. `fn(M)` is auto-Send/Sync regardless of M's
    // own thread-safety, which is correct since the adapter never holds
    // an M across a .await/handler boundary. Wrapping with `S` keeps
    // the shard parameter visible to the type system without imposing
    // S: Send unconditionally.
    _marker: PhantomData<(fn(M), S)>,
}

impl<M, T, S> ReplyAdapter<M, T, S>
where
    M: 'static,
    T: 'static + From<M>,
    S: Shard + 'static,
{
    /// Creates a new adapter pointing at `target`. Pair with the
    /// runtime's `register_with_capacity_on(...)` to put it on a shard.
    ///
    /// The `T: From<M>` bound is repeated here (not just on
    /// `impl Isolate`) so misconfigured types fail at the call site,
    /// not at the later registration call.
    pub fn new(target: Address<T>) -> Self
    where
        T: From<M>,
    {
        Self {
            target,
            _marker: PhantomData,
        }
    }
}

impl<M, T, S> Isolate for ReplyAdapter<M, T, S>
where
    M: 'static,
    T: 'static + From<M>,
    S: Shard + 'static,
{
    type Message = M;
    type Reply = ();
    type Send = TinaOutbound<T>;
    type Spawn = Infallible;
    // `RuntimeCall<M>` (rather than `Infallible`) so the same primitive
    // is accepted by the simulator, the explicit-step `MultiShardRuntime`,
    // and `ThreadedMultiShardRuntime` without per-runtime variants. The
    // adapter never actually issues a runtime call; the channel exists to
    // satisfy the simulator's `RuntimeCallable` bound.
    type Call = RuntimeCall<M>;
    type Shard = S;

    fn handle(&mut self, msg: M, _ctx: &mut Context<'_, S>) -> Effect<Self> {
        send(self.target, msg.into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shards(raws: &[u32]) -> Vec<ShardId> {
        raws.iter().copied().map(ShardId::new).collect()
    }

    #[test]
    fn placement_rejects_empty_shard_list() {
        assert_eq!(
            ShardPlacement::new("p", vec![]).unwrap_err(),
            ShardPlacementError::Empty
        );
    }

    #[test]
    fn placement_rejects_duplicate_shard_ids() {
        let err = ShardPlacement::new("p", shards(&[1, 7, 7])).unwrap_err();
        assert_eq!(err, ShardPlacementError::DuplicateShard(ShardId::new(7)));
    }

    #[test]
    fn placement_preserves_explicit_order() {
        let p = ShardPlacement::new("p", shards(&[7, 1, 22])).unwrap();
        assert_eq!(
            p.shards(),
            &[ShardId::new(7), ShardId::new(1), ShardId::new(22)]
        );
    }

    /// Canonical frozen mapping for the `dst-fanout` placement.
    ///
    /// This is the single source of truth for the FNV-1a 64 mod-count
    /// scheme at version 1. `tina-runtime` integration tests
    /// (`tests/sharded_primitives.rs`) and `tina-sim` DST tests
    /// (`tests/sharded_dst.rs`) both freeze the same constants. If the
    /// hash scheme or version is changed deliberately, this fixture
    /// fails first; bump `ShardHashScheme::VERSION` and update the
    /// downstream test files in lockstep.
    pub(crate) const FROZEN_DST_KEYS: &[&str] = &[
        "alpha", "beta", "gamma", "delta", "epsilon", "zeta", "eta", "theta", "iota", "kappa",
        "lambda", "mu",
    ];
    pub(crate) const FROZEN_DST_SHARDS: [u32; 3] = [11, 22, 33];
    pub(crate) const FROZEN_DST_OWNERS: [u32; 12] =
        [11, 33, 33, 22, 33, 33, 11, 11, 11, 33, 33, 11];

    #[test]
    fn placement_owner_mapping_is_frozen_at_version_1() {
        let p = ShardPlacement::new(
            "dst-fanout",
            FROZEN_DST_SHARDS
                .iter()
                .copied()
                .map(ShardId::new)
                .collect(),
        )
        .unwrap();
        let owners: Vec<u32> = FROZEN_DST_KEYS
            .iter()
            .map(|k| p.owner_for_str(k).get())
            .collect();
        assert_eq!(owners, FROZEN_DST_OWNERS.to_vec());
        // Sanity: every shard in the placement is reachable by at least
        // one frozen key. This protects against trivial bugs that send
        // every key to one shard.
        let unique_owners: BTreeSet<u32> = owners.iter().copied().collect();
        assert_eq!(unique_owners.len(), FROZEN_DST_SHARDS.len());
    }

    #[test]
    fn placement_owner_for_non_contiguous_shard_ids_is_in_set() {
        let shard_set = shards(&[3, 17, 91]);
        let p = ShardPlacement::new("p", shard_set.clone()).unwrap();
        for n in 0..32u32 {
            let key = format!("k-{n}");
            let owner = p.owner_for_str(&key);
            assert!(shard_set.contains(&owner));
        }
    }

    #[test]
    fn placement_reports_carry_name_scheme_version_and_shards() {
        let p = ShardPlacement::new("by-key", shards(&[3, 17, 91])).unwrap();
        let report = p.report();
        assert_eq!(p.name(), "by-key");
        assert_eq!(report.name(), "by-key");
        assert_eq!(report.scheme(), ShardHashScheme::Fnv1a64ModCount);
        assert_eq!(report.version(), ShardHashScheme::VERSION);
        assert_eq!(
            report.shards(),
            &[ShardId::new(3), ShardId::new(17), ShardId::new(91)]
        );
    }

    #[test]
    fn placement_owner_for_str_matches_owner_for_bytes() {
        let p = ShardPlacement::new("p", shards(&[2, 5])).unwrap();
        for key in ["", "a", "hello world", "the quick brown fox"] {
            assert_eq!(p.owner_for_str(key), p.owner_for_bytes(key.as_bytes()));
        }
    }

    #[test]
    fn placement_require_owner_returns_ok_when_actual_matches_placement() {
        // Frozen: under FNV-1a 64 mod 3 over [3, 17, 91], "delta" hashes to
        // shard 17. Pinning the key removes the linear-search-through-keys
        // pattern (fragile against future hash changes).
        let p = ShardPlacement::new("p", shards(&[3, 17, 91])).unwrap();
        assert_eq!(
            p.owner_for_str("delta"),
            ShardId::new(17),
            "frozen-key invariant: delta must map to shard 17 under v1"
        );
        assert_eq!(
            p.require_owner_str("delta", ShardId::new(17)).unwrap(),
            ShardId::new(17)
        );
        assert_eq!(
            p.require_owner_bytes("delta".as_bytes(), ShardId::new(17))
                .unwrap(),
            ShardId::new(17)
        );
    }

    #[test]
    fn placement_require_owner_returns_wrong_shard_on_mismatch() {
        // Frozen: under FNV-1a 64 mod 2 over [3, 17], "gamma" hashes to
        // shard 3. We probe it on shard 17 to force a mismatch.
        let p = ShardPlacement::new("p", shards(&[3, 17])).unwrap();
        assert_eq!(
            p.owner_for_str("gamma"),
            ShardId::new(3),
            "frozen-key invariant: gamma must map to shard 3 under v1"
        );
        let err = p
            .require_owner_str("gamma", ShardId::new(17))
            .expect_err("wrong shard");
        assert_eq!(
            err,
            WrongShard {
                expected: ShardId::new(3),
                actual: ShardId::new(17),
            }
        );
        // Bytes form agrees with the str form.
        let err_bytes = p
            .require_owner_bytes("gamma".as_bytes(), ShardId::new(17))
            .expect_err("wrong shard (bytes)");
        assert_eq!(err, err_bytes);
    }

    #[test]
    fn placement_accepts_runtime_derived_name() {
        // Plain `String` is accepted, not just `&'static str`.
        let dynamic_name = format!("dynamic-{}", 42);
        let p = ShardPlacement::new(dynamic_name.clone(), shards(&[1])).unwrap();
        assert_eq!(p.name(), dynamic_name);
    }

    #[test]
    fn placement_error_implements_std_error() {
        let err: Box<dyn Error> = Box::new(ShardPlacement::new("p", vec![]).unwrap_err());
        assert!(err.to_string().contains("at least one"));
    }

    #[test]
    fn service_table_rejects_mismatched_shard_list() {
        let placement = ShardPlacement::new("p", shards(&[3, 17])).unwrap();
        let entries = vec![
            (ShardId::new(17), addr::<u32>(17, 1)),
            (ShardId::new(3), addr::<u32>(3, 2)),
        ];
        let err = ShardServiceTable::new(placement, entries).unwrap_err();
        match err {
            ShardServiceTableError::ShardListMismatch { placement, entries } => {
                assert_eq!(placement, shards(&[3, 17]));
                assert_eq!(entries, shards(&[17, 3]));
            }
        }
    }

    #[test]
    fn service_table_address_for_missing_shard_is_typed() {
        let placement = ShardPlacement::new("p", shards(&[3, 17])).unwrap();
        let entries = vec![
            (ShardId::new(3), addr::<u32>(3, 1)),
            (ShardId::new(17), addr::<u32>(17, 2)),
        ];
        let table = ShardServiceTable::new(placement, entries).unwrap();
        assert_eq!(
            table.address_for(ShardId::new(99)).unwrap_err(),
            MissingShard(ShardId::new(99))
        );
    }

    #[test]
    fn service_table_from_placement_registers_one_address_per_shard() {
        let placement = ShardPlacement::new("p", shards(&[3, 17, 91])).unwrap();
        let mut next_isolate = 100u64;
        let table = ShardServiceTable::<u32>::from_placement(placement, |shard| {
            let addr = addr::<u32>(shard.get(), next_isolate);
            next_isolate += 1;
            addr
        })
        .unwrap();
        // Addresses are parallel to placement.shards() in caller-supplied
        // order; isolate ids match the order the closure was called.
        assert_eq!(table.placement().shards(), &shards(&[3, 17, 91]));
        assert_eq!(
            table.address_for(ShardId::new(3)).unwrap().isolate(),
            tina::IsolateId::new(100)
        );
        assert_eq!(
            table.address_for(ShardId::new(17)).unwrap().isolate(),
            tina::IsolateId::new(101)
        );
        assert_eq!(
            table.address_for(ShardId::new(91)).unwrap().isolate(),
            tina::IsolateId::new(102)
        );
    }

    #[test]
    fn service_table_try_from_placement_propagates_registration_error() {
        let placement = ShardPlacement::new("p", shards(&[3, 17])).unwrap();
        let err = ShardServiceTable::<u32>::try_from_placement(placement, |shard| {
            if shard == ShardId::new(17) {
                Err("dead worker")
            } else {
                Ok(addr::<u32>(shard.get(), 1))
            }
        })
        .unwrap_err();
        match err {
            ServiceTableBuildError::Register(e) => assert_eq!(e, "dead worker"),
            other => panic!("expected Register, got {other:?}"),
        }
    }

    #[test]
    fn service_table_try_from_placement_succeeds_when_every_shard_registers() {
        let placement = ShardPlacement::new("p", shards(&[3, 17, 91])).unwrap();
        let table = ShardServiceTable::<u32>::try_from_placement::<_, ()>(placement, |shard| {
            Ok(addr::<u32>(shard.get(), 7))
        })
        .unwrap();
        assert_eq!(table.placement().shards().len(), 3);
        for shard in [3, 17, 91] {
            assert_eq!(
                table.address_for(ShardId::new(shard)).unwrap().isolate(),
                tina::IsolateId::new(7)
            );
        }
    }

    #[test]
    fn service_table_address_for_bytes_routes_through_placement() {
        let placement = ShardPlacement::new("p", shards(&[3, 17, 91])).unwrap();
        let entries = vec![
            (ShardId::new(3), addr::<u32>(3, 11)),
            (ShardId::new(17), addr::<u32>(17, 22)),
            (ShardId::new(91), addr::<u32>(91, 33)),
        ];
        let placement_clone = placement.clone();
        let table = ShardServiceTable::new(placement, entries).unwrap();
        for key in ["alpha", "beta", "gamma", "delta"] {
            let owner = placement_clone.owner_for_str(key);
            assert_eq!(table.address_for_str(key).shard(), owner);
        }
    }

    #[test]
    fn service_table_error_implements_std_error() {
        let placement = ShardPlacement::new("p", shards(&[3, 17])).unwrap();
        let bad_entries = vec![(ShardId::new(99), addr::<u32>(99, 1))];
        let err: Box<dyn Error> =
            Box::new(ShardServiceTable::new(placement, bad_entries).unwrap_err());
        assert!(err.to_string().contains("placement"));
    }

    #[test]
    fn missing_shard_implements_std_error() {
        let err: Box<dyn Error> = Box::new(MissingShard(ShardId::new(7)));
        assert!(err.to_string().contains("not in"));
    }

    #[test]
    fn wrong_shard_implements_std_error() {
        let err: Box<dyn Error> = Box::new(WrongShard {
            expected: ShardId::new(3),
            actual: ShardId::new(7),
        });
        assert!(err.to_string().contains("expected 3"));
    }

    #[test]
    fn scatter_gather_config_validates_max_targets_nonzero() {
        let cfg = ScatterGatherConfig {
            max_targets: 0,
            collector_capacity: 4,
            per_target_timeout: Duration::from_millis(10),
            aggregate_timeout: Duration::from_millis(20),
        };
        assert_eq!(
            cfg.validate().unwrap_err(),
            ScatterGatherConfigError::MaxTargetsZero
        );
    }

    #[test]
    fn scatter_gather_config_rejects_collector_capacity_below_max_targets() {
        let cfg = ScatterGatherConfig {
            max_targets: 8,
            collector_capacity: 4,
            per_target_timeout: Duration::from_millis(10),
            aggregate_timeout: Duration::from_millis(20),
        };
        assert_eq!(
            cfg.validate().unwrap_err(),
            ScatterGatherConfigError::CollectorCapacityBelowMaxTargets {
                collector_capacity: 4,
                max_targets: 8,
            }
        );
    }

    #[test]
    fn scatter_gather_config_rejects_zero_per_target_timeout() {
        let cfg = ScatterGatherConfig {
            max_targets: 4,
            collector_capacity: 4,
            per_target_timeout: Duration::ZERO,
            aggregate_timeout: Duration::from_millis(10),
        };
        assert_eq!(
            cfg.validate().unwrap_err(),
            ScatterGatherConfigError::PerTargetTimeoutZero
        );
    }

    #[test]
    fn scatter_gather_config_rejects_aggregate_below_per_target() {
        let cfg = ScatterGatherConfig {
            max_targets: 4,
            collector_capacity: 4,
            per_target_timeout: Duration::from_millis(50),
            aggregate_timeout: Duration::from_millis(10),
        };
        assert_eq!(
            cfg.validate().unwrap_err(),
            ScatterGatherConfigError::AggregateTimeoutBelowPerTargetTimeout {
                aggregate_timeout: Duration::from_millis(10),
                per_target_timeout: Duration::from_millis(50),
            }
        );
    }

    #[test]
    fn scatter_gather_config_error_implements_std_error() {
        let err: Box<dyn Error> = Box::new(ScatterGatherConfigError::MaxTargetsZero);
        assert!(err.to_string().contains("max_targets"));
    }

    #[test]
    fn scatter_gather_report_counts_outcomes_separately() {
        let cfg = ScatterGatherConfig {
            max_targets: 6,
            collector_capacity: 6,
            per_target_timeout: Duration::from_millis(10),
            aggregate_timeout: Duration::from_millis(20),
        };
        let report: ScatterGatherReport<u64> = ScatterGatherReport {
            config: cfg,
            outcomes: vec![
                (ShardId::new(1), ScatterGatherTargetOutcome::Replied(7)),
                (ShardId::new(2), ScatterGatherTargetOutcome::Full),
                (ShardId::new(3), ScatterGatherTargetOutcome::Closed),
                (ShardId::new(4), ScatterGatherTargetOutcome::Timeout),
                (
                    ShardId::new(5),
                    ScatterGatherTargetOutcome::AggregateTimeout,
                ),
                (ShardId::new(6), ScatterGatherTargetOutcome::MissingShard),
            ],
        };
        assert_eq!(report.replied_count(), 1);
        assert_eq!(report.full_count(), 1);
        assert_eq!(report.closed_count(), 1);
        assert_eq!(report.timeout_count(), 1);
        assert_eq!(report.aggregate_timeout_count(), 1);
        assert_eq!(report.missing_shard_count(), 1);
        let replied: Vec<_> = report.replied().collect();
        assert_eq!(replied, vec![(ShardId::new(1), &7)]);
    }

    #[test]
    fn hot_key_attempt_report_tracks_intermediate_and_terminal_outcomes() {
        let mut report = HotKeyAttemptReport::default();
        report.record(HotKeyAttemptOutcome::Accepted);
        report.record(HotKeyAttemptOutcome::FullFirstAttempt);
        report.record(HotKeyAttemptOutcome::FullRetry);
        report.record(HotKeyAttemptOutcome::FullRetry);
        report.record(HotKeyAttemptOutcome::RetrySucceeded);
        report.record(HotKeyAttemptOutcome::RetryExhausted);
        report.record(HotKeyAttemptOutcome::Timeout);
        report.record(HotKeyAttemptOutcome::Closed);
        assert_eq!(report.accepted_first_attempt, 1);
        assert_eq!(report.full_first_attempt, 1);
        // FullRetry is now counted explicitly so callers can see retry
        // pressure rather than only the terminal verdict.
        assert_eq!(report.full_retry_total, 2);
        assert_eq!(report.retry_succeeded, 1);
        assert_eq!(report.retry_exhausted, 1);
        assert_eq!(report.timeout, 1);
        assert_eq!(report.closed, 1);
        assert_eq!(report.loop_entries(), 5);
    }

    fn addr<M: 'static>(shard: u32, isolate: u64) -> Address<M> {
        Address::new(ShardId::new(shard), tina::IsolateId::new(isolate))
    }

    // ---- Proptest: placement properties over random shard lists + keys ----

    use proptest::collection::{btree_set, vec};
    use proptest::prelude::*;

    fn arb_shard_list() -> impl Strategy<Value = Vec<ShardId>> {
        // Up to 8 distinct shard ids in random u32 range; shuffled so the
        // ordered list is non-monotonic.
        btree_set(0u32..1000, 1..=8)
            .prop_flat_map(|set| {
                let v: Vec<u32> = set.into_iter().collect();
                Just(v).prop_shuffle()
            })
            .prop_map(|raws| raws.into_iter().map(ShardId::new).collect())
    }

    fn arb_keys() -> impl Strategy<Value = Vec<String>> {
        vec("[a-z]{1,8}", 1..64)
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(64))]

        /// Property 1: `owner_for_bytes` is closed over `placement.shards()`.
        /// Property 2: same key always maps to same shard (deterministic).
        #[test]
        fn placement_owner_is_in_shard_list_and_deterministic(
            shards in arb_shard_list(),
            keys in arb_keys(),
        ) {
            let placement = ShardPlacement::new("p", shards.clone()).unwrap();
            let shard_set: BTreeSet<ShardId> = shards.iter().copied().collect();
            for key in &keys {
                let owner_a = placement.owner_for_str(key);
                let owner_b = placement.owner_for_str(key);
                prop_assert!(shard_set.contains(&owner_a));
                prop_assert_eq!(owner_a, owner_b);
            }
        }

        /// Property 3: with enough random keys, every shard in a
        /// reasonably-sized placement is reachable. The key budget scales
        /// with shard count so the chance of a missed shard under FNV-1a
        /// stays vanishingly small.
        #[test]
        fn placement_reaches_every_shard_with_enough_random_keys(
            shards in arb_shard_list(),
        ) {
            let placement = ShardPlacement::new("p", shards.clone()).unwrap();
            let key_budget = (shards.len() * 50).max(50);
            let mut reached = BTreeSet::new();
            for n in 0..key_budget {
                let key = format!("k-{n}");
                reached.insert(placement.owner_for_str(&key));
            }
            prop_assert_eq!(reached.len(), shards.len());
        }
    }
}
