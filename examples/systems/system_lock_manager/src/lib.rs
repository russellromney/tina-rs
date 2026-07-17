//! `system_lock_manager` — local lock manager with leases, FIFO wait
//! queues per key, renewals, expiry hand-off, and stale-handle detection.
//!
//! What this specimen pulls on:
//!
//! - [`SharedWork`] for bounded FIFO acquire waiters, keyed by lock key.
//! - [`RequestCall`] for `Acquire` / `Release` / `Renew` / `Stats` caller
//!   authority, with private internal `LeaseExpired` events outside the public
//!   request lane.
//! - `sleep(d).then(...)` for runtime-owned lease expiry. Each scheduled
//!   wake carries an `expiry_token` so a renewed lease silently ignores
//!   the stale wake without needing cancellation.
//!
//! Per-key invariants:
//!
//! - At most one holder per key. A holder is identified by
//!   `(key, holder_id)`; `holder_id` is process-monotonic, never reused,
//!   and a stale handle reliably loses the equality check.
//! - Waiters per key are FIFO. The next waiter on release/expiry is the
//!   one parked first.
//! - Per-key wait queue length is capped (`max_waiters_per_key`). Hits
//!   reply `Busy` without consuming a `pending` slot.
//! - Total parked waiters across keys is capped by `waiter_capacity`.
//!   Hits reply `Busy(GlobalFull)`.
//! - Active key count is capped by `max_keys`. A first-time acquire on a
//!   new key past the cap replies `KeyspaceFull`. Idle keys (no holder,
//!   no waiters) are removed.

use std::collections::HashMap;
use std::convert::Infallible;
use std::error::Error;
use std::fmt;
use std::sync::{Barrier, Mutex};
use std::thread;
use std::time::Duration;

#[cfg(test)]
use std::sync::Arc;

use tina::prelude::*;
use tina_runtime::{
    CallError, CallOutcome, DefaultThreadedMailboxFactory, LocalSystem, SharedWork,
    SharedWorkError, SleepReply, request_effect_after_shared_wait, sleep,
};

type App = LocalSystem<SingleShard, DefaultThreadedMailboxFactory>;

/// Upper bound on parked waiter and keyspace capacities.
pub const MAX_WAITERS: usize = 65_536;
/// Upper bound on active keys.
pub const MAX_KEYS: usize = 65_536;
/// Upper bound on the lock-manager mailbox.
pub const MAX_MAILBOX: usize = 65_536;
/// Upper bound on lease length and host call timeouts.
pub const MAX_DURATION_MS: u64 = 60_000;

/// Tunables for one specimen run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RunConfig {
    /// Total parked acquire-callers across every key.
    pub waiter_capacity: usize,
    /// Maximum waiters parked behind a single held key.
    pub max_waiters_per_key: usize,
    /// Maximum number of keys with a holder or waiters at once.
    pub max_keys: usize,
    /// Lock-manager mailbox capacity.
    pub mailbox: usize,
    /// Lease length applied to every grant and renewal.
    pub lease_ms: u64,
    /// Host-side blocking call timeout.
    pub call_timeout_ms: u64,
}

impl Default for RunConfig {
    fn default() -> Self {
        Self {
            waiter_capacity: 16,
            max_waiters_per_key: 8,
            max_keys: 64,
            mailbox: 64,
            lease_ms: 200,
            call_timeout_ms: 5_000,
        }
    }
}

/// Typed rejection of an unsafe public configuration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunConfigError {
    Zero {
        field: &'static str,
    },
    TooLarge {
        field: &'static str,
        value: usize,
        max: usize,
    },
    DurationTooLarge {
        field: &'static str,
        value_ms: u64,
        max_ms: u64,
    },
}

impl fmt::Display for RunConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Zero { field } => write!(f, "{field} must be greater than zero"),
            Self::TooLarge { field, value, max } => {
                write!(f, "{field} {value} exceeds maximum {max}")
            }
            Self::DurationTooLarge {
                field,
                value_ms,
                max_ms,
            } => write!(f, "{field} {value_ms}ms exceeds maximum {max_ms}ms"),
        }
    }
}

impl Error for RunConfigError {}

impl RunConfig {
    /// Rejects zero and oversized public counts before runtime or
    /// `SharedWork` construction.
    pub fn validate(self) -> Result<Self, RunConfigError> {
        nonzero_bounded("waiter_capacity", self.waiter_capacity, MAX_WAITERS)?;
        nonzero_bounded("max_waiters_per_key", self.max_waiters_per_key, MAX_WAITERS)?;
        nonzero_bounded("max_keys", self.max_keys, MAX_KEYS)?;
        nonzero_bounded("mailbox", self.mailbox, MAX_MAILBOX)?;
        nonzero_duration("lease_ms", self.lease_ms, MAX_DURATION_MS)?;
        nonzero_duration("call_timeout_ms", self.call_timeout_ms, MAX_DURATION_MS)?;
        Ok(self)
    }
}

fn nonzero_bounded(field: &'static str, value: usize, max: usize) -> Result<(), RunConfigError> {
    if value == 0 {
        return Err(RunConfigError::Zero { field });
    }
    if value > max {
        return Err(RunConfigError::TooLarge { field, value, max });
    }
    Ok(())
}

fn nonzero_duration(field: &'static str, value_ms: u64, max_ms: u64) -> Result<(), RunConfigError> {
    if value_ms == 0 {
        return Err(RunConfigError::Zero { field });
    }
    if value_ms > max_ms {
        return Err(RunConfigError::DurationTooLarge {
            field,
            value_ms,
            max_ms,
        });
    }
    Ok(())
}

/// Opaque holder token. Stale tokens are rejected on release/renew.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct LockHandle {
    pub key: String,
    pub holder_id: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LockReply {
    Granted {
        handle: LockHandle,
    },
    Released,
    Renewed {
        holder_id: u64,
    },
    /// Wait queue rejected the caller at a typed boundedness rail.
    Busy(WaitBusyReason),
    /// Release/renew arrived for a holder that no longer owns the key.
    StaleHandle,
    /// First-time acquire on a new key while the keyspace was full.
    KeyspaceFull,
    Stats(LockStats),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WaitBusyReason {
    /// Total parked-caller capacity across every key is exhausted.
    GlobalFull,
    /// This key's FIFO is at its configured limit.
    KeyFull,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct LockStats {
    pub keys_live: usize,
    pub waiters_live: usize,
    pub waiters_high_water: usize,
    pub global_full_rejects: u64,
    pub per_key_full_rejects: u64,
    pub keyspace_full_rejects: u64,
    pub acquires_granted: u64,
    pub acquires_handed_off: u64,
    pub releases: u64,
    pub renewals: u64,
    pub stale_release_rejects: u64,
    pub stale_renew_rejects: u64,
    pub expiries: u64,
    pub stale_expiries_ignored: u64,
    pub timer_errors: u64,
    pub last_timer_error: Option<CallError>,
    pub waiters_reclaimed: u64,
}

#[derive(Debug)]
enum LockEvent {
    LeaseExpired {
        key: String,
        holder_id: u64,
        expiry_token: u64,
        result: SleepReply,
    },
}

enum LockRequest {
    Acquire { key: String },
    Release { handle: LockHandle },
    Renew { handle: LockHandle },
    Stats,
}

#[derive(Debug)]
struct LockState {
    holder_id: u64,
    expiry_token: u64,
}

struct LockManager {
    lease: Duration,
    max_keys: usize,
    waiters: SharedWork<String, LockReply>,
    next_holder_id: u64,
    locks: HashMap<String, LockState>,
    stats: LockStats,
}

#[tina_runtime::isolate(event = LockEvent, request = LockRequest, reply = LockReply)]
impl LockManager {
    fn handle_event(
        &mut self,
        event: LockEvent,
        _ctx: &mut Context<'_, SingleShard, Self::Reply>,
    ) -> Effect<Self> {
        match event {
            LockEvent::LeaseExpired {
                key,
                holder_id,
                expiry_token,
                result,
            } => self.lease_expired(key, holder_id, expiry_token, result),
        }
    }

    fn handle_request(
        &mut self,
        request: LockRequest,
        call: RequestCall<'_, Self>,
    ) -> RequestEffect<Self> {
        match request {
            LockRequest::Acquire { key } => self.acquire(key, call),
            LockRequest::Release { handle } => self.release(handle, call),
            LockRequest::Renew { handle } => self.renew(handle, call),
            LockRequest::Stats => call.reply(LockReply::Stats(self.snapshot())),
        }
    }
}

impl LockManager {
    fn new(config: RunConfig) -> Self {
        Self {
            lease: Duration::from_millis(config.lease_ms),
            max_keys: config.max_keys,
            waiters: SharedWork::with_key_limit(config.waiter_capacity, config.max_waiters_per_key)
                .named("system_lock_manager.waiters"),
            next_holder_id: 0,
            locks: HashMap::new(),
            stats: LockStats::default(),
        }
    }

    fn acquire(&mut self, key: String, call: RequestCall<'_, Self>) -> RequestEffect<Self> {
        if !self.locks.contains_key(&key) {
            if self.locks.len() >= self.max_keys {
                self.stats.keyspace_full_rejects += 1;
                return call.reply(LockReply::KeyspaceFull);
            }
            return self.grant_new(key, call);
        }

        match self.waiters.wait(key, call) {
            Ok((_ticket, permit)) => request_effect_after_shared_wait(permit, noop()),
            Err(SharedWorkError::Full { call, .. }) => {
                call.reply(LockReply::Busy(WaitBusyReason::GlobalFull))
            }
            Err(SharedWorkError::KeyFull { call, .. }) => {
                call.reply(LockReply::Busy(WaitBusyReason::KeyFull))
            }
        }
    }

    fn release(&mut self, handle: LockHandle, call: RequestCall<'_, Self>) -> RequestEffect<Self> {
        let holder_matches = self
            .locks
            .get(&handle.key)
            .map(|entry| entry.holder_id == handle.holder_id)
            .unwrap_or(false);
        if !holder_matches {
            self.stats.stale_release_rejects += 1;
            return call.reply(LockReply::StaleHandle);
        }
        self.stats.releases += 1;
        let effects = self.hand_off(handle.key);
        call.reply_and(LockReply::Released, effects)
    }

    fn renew(&mut self, handle: LockHandle, call: RequestCall<'_, Self>) -> RequestEffect<Self> {
        let Some(entry) = self.locks.get_mut(&handle.key) else {
            self.stats.stale_renew_rejects += 1;
            return call.reply(LockReply::StaleHandle);
        };
        if entry.holder_id != handle.holder_id {
            self.stats.stale_renew_rejects += 1;
            return call.reply(LockReply::StaleHandle);
        }
        entry.expiry_token = entry.expiry_token.wrapping_add(1);
        let token = entry.expiry_token;
        let holder_id = entry.holder_id;
        self.stats.renewals += 1;
        let key_for_msg = handle.key.clone();
        let lease = self.lease;
        call.reply_and(
            LockReply::Renewed { holder_id },
            vec![
                sleep(lease).then_service_event(move |result| LockEvent::LeaseExpired {
                    key: key_for_msg,
                    holder_id,
                    expiry_token: token,
                    result,
                }),
            ],
        )
    }

    fn lease_expired(
        &mut self,
        key: String,
        holder_id: u64,
        expiry_token: u64,
        result: SleepReply,
    ) -> Effect<Self> {
        let timer_error = result.err();
        if let Some(error) = timer_error {
            self.stats.timer_errors += 1;
            self.stats.last_timer_error = Some(error);
        }
        let Some(entry) = self.locks.get(&key) else {
            self.stats.stale_expiries_ignored += 1;
            return noop();
        };
        if entry.holder_id != holder_id || entry.expiry_token != expiry_token {
            self.stats.stale_expiries_ignored += 1;
            return noop();
        }
        if timer_error.is_none() {
            self.stats.expiries += 1;
        }
        let effects = self.hand_off(key);
        if effects.is_empty() {
            noop()
        } else {
            Effect::Batch(effects)
        }
    }

    /// Replace the holder for `key` with the oldest live waiter, or drop the
    /// entry if none remain. `SharedWork` reclaims closed callers while
    /// selecting the next FIFO slot.
    fn hand_off(&mut self, key: String) -> Vec<Effect<Self>> {
        let Some(slot) = self.waiters.take_next(&key) else {
            self.locks.remove(&key);
            return Vec::new();
        };
        self.install_handoff(key, slot)
    }

    fn install_handoff(
        &mut self,
        key: String,
        slot: DeferredReply<LockReply>,
    ) -> Vec<Effect<Self>> {
        let entry = self.locks.get_mut(&key).expect("held key remains present");
        self.next_holder_id += 1;
        let new_holder_id = self.next_holder_id;
        entry.holder_id = new_holder_id;
        entry.expiry_token = entry.expiry_token.wrapping_add(1);
        let token = entry.expiry_token;
        let key_for_msg = key.clone();
        let lease = self.lease;
        self.stats.acquires_handed_off += 1;
        vec![
            reply_to::<Self>(
                slot,
                LockReply::Granted {
                    handle: LockHandle {
                        key: key.clone(),
                        holder_id: new_holder_id,
                    },
                },
            ),
            sleep(lease).then_service_event(move |result| LockEvent::LeaseExpired {
                key: key_for_msg,
                holder_id: new_holder_id,
                expiry_token: token,
                result,
            }),
        ]
    }

    fn grant_new(&mut self, key: String, call: RequestCall<'_, Self>) -> RequestEffect<Self> {
        self.next_holder_id += 1;
        let holder_id = self.next_holder_id;
        self.locks.insert(
            key.clone(),
            LockState {
                holder_id,
                expiry_token: 1,
            },
        );
        self.stats.acquires_granted += 1;
        let key_for_msg = key.clone();
        let lease = self.lease;
        call.reply_and(
            LockReply::Granted {
                handle: LockHandle { key, holder_id },
            },
            vec![
                sleep(lease).then_service_event(move |result| LockEvent::LeaseExpired {
                    key: key_for_msg,
                    holder_id,
                    expiry_token: 1,
                    result,
                }),
            ],
        )
    }

    fn snapshot(&self) -> LockStats {
        let mut s = self.stats.clone();
        s.keys_live = self.locks.len();
        s.waiters_live = self.waiters.len();
        s.waiters_high_water = self.waiters.high_water();
        s.global_full_rejects = self.waiters.full_rejects();
        s.per_key_full_rejects = self.waiters.key_full_rejects();
        s.waiters_reclaimed = self.waiters.reclaimed();
        s
    }
}

// ---------- Run scenarios ----------

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunReport {
    pub fifo: FifoReport,
    pub expiry_handoff: ExpiryHandoffReport,
    pub renewal: RenewalReport,
    pub stale_release: StaleReleaseReport,
    pub per_key_overflow: PerKeyOverflowReport,
    pub global_overflow: GlobalOverflowReport,
    pub caller_gone_refill: CallerGoneRefillReport,
    pub keyspace_overflow: KeyspaceOverflowReport,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FifoReport {
    /// Order each waiter was admitted; expected to match grant order.
    pub admitted_order: Vec<u32>,
    pub grant_order: Vec<u32>,
    pub stats: LockStats,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExpiryHandoffReport {
    pub waiter_received_grant: bool,
    pub original_release_was_stale: bool,
    pub stats: LockStats,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RenewalReport {
    pub still_held_after_original_lease: bool,
    pub final_release_ok: bool,
    pub old_handle_renew_was_stale: bool,
    pub stats: LockStats,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StaleReleaseReport {
    pub second_release_was_stale: bool,
    pub stats: LockStats,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PerKeyOverflowReport {
    pub busy: usize,
    pub stats: LockStats,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GlobalOverflowReport {
    pub global_full: bool,
    pub stats: LockStats,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CallerGoneRefillReport {
    pub first_timed_out: bool,
    pub next_waiter_granted: bool,
    pub stats: LockStats,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyspaceOverflowReport {
    pub keyspace_full: bool,
    pub stats: LockStats,
}

pub fn run(config: RunConfig) -> anyhow::Result<RunReport> {
    validate_config(config)?;
    Ok(RunReport {
        fifo: run_fifo(config)?,
        expiry_handoff: run_expiry_handoff(config)?,
        renewal: run_renewal(config)?,
        stale_release: run_stale_release(config)?,
        per_key_overflow: run_per_key_overflow(config)?,
        global_overflow: run_global_overflow(config)?,
        caller_gone_refill: run_caller_gone_refill(config)?,
        keyspace_overflow: run_keyspace_overflow(config)?,
    })
}

/// Four threads contend for one key. The first acquires immediately; the
/// remaining three park in FIFO order. Each holder releases as soon as
/// it sees `Granted`, so the queue should drain in admission order.
pub fn run_fifo(config: RunConfig) -> anyhow::Result<FifoReport> {
    validate_config(config)?;
    // Use a long lease so expiry never fires during the test.
    let cfg = RunConfig {
        lease_ms: 5_000,
        ..config
    };
    run_local(cfg, |runtime| {
        let admitted_order: Vec<u32> = (1..=4).collect();
        let grant_order = Mutex::new(Vec::new());
        thread::scope(|scope| {
            let lockmgr = register(runtime, cfg)?;
            let timeout = Duration::from_millis(cfg.call_timeout_ms);

            let key = "fifo-key".to_string();

            // Hold the lock from the host first so every contender parks before
            // any of them can be granted.
            let holder = acquire_handle(runtime, lockmgr, &key, timeout)?;

            let contenders = 4u32;
            let mut threads = Vec::with_capacity(contenders as usize);
            for id in 1..=contenders {
                let key_for_thread = key.clone();
                let granted = &grant_order;
                threads.push(scope.spawn(move || -> anyhow::Result<()> {
                    let handle = expect_granted(runtime.call_blocking_request(
                        lockmgr,
                        LockRequest::Acquire {
                            key: key_for_thread.clone(),
                        },
                        timeout,
                    )?)?;
                    granted.lock().expect("granted").push(id);
                    expect_released(runtime.call_blocking_request(
                        lockmgr,
                        LockRequest::Release { handle },
                        timeout,
                    )?)
                }));
                // Observe this request parked before admitting the next one.
                // The server-side waiter count is the FIFO admission proof;
                // host scheduling delays cannot reorder the cohort.
                wait_for_waiters(runtime, lockmgr, id as usize, timeout)?;
            }

            expect_released(runtime.call_blocking_request(
                lockmgr,
                LockRequest::Release { handle: holder },
                timeout,
            )?)?;

            for t in threads {
                t.join()
                    .map_err(|_| anyhow::anyhow!("contender thread panicked"))??;
            }

            let stats = stats(runtime, lockmgr, timeout)?;
            Ok(FifoReport {
                admitted_order,
                grant_order: grant_order.lock().expect("granted").clone(),
                stats,
            })
        })
    })
}

/// Acquire, then never renew or release. A second caller parks. After
/// the lease elapses, the parked caller is granted automatically and the
/// original release is rejected as stale.
pub fn run_expiry_handoff(config: RunConfig) -> anyhow::Result<ExpiryHandoffReport> {
    validate_config(config)?;
    let cfg = RunConfig {
        lease_ms: 100,
        ..config
    };
    run_local(cfg, |runtime| {
        thread::scope(|scope| {
            let lockmgr = register(runtime, cfg)?;
            let timeout = Duration::from_millis(cfg.call_timeout_ms);
            let key = "expire-key".to_string();

            let original = acquire_handle(runtime, lockmgr, &key, timeout)?;

            let key_for_waiter = key.clone();
            let waiter = scope.spawn(move || {
                runtime.call_blocking_request(
                    lockmgr,
                    LockRequest::Acquire {
                        key: key_for_waiter,
                    },
                    timeout,
                )
            });

            wait_for_waiters(runtime, lockmgr, 1, timeout)?;

            // Wait through the lease so the timer fires and the queued caller
            // gets handed the lock without us doing anything.
            thread::sleep(Duration::from_millis(cfg.lease_ms + 80));

            let waiter_handle = expect_granted(
                waiter
                    .join()
                    .map_err(|_| anyhow::anyhow!("waiter thread panicked"))??,
            )?;
            let waiter_received_grant = waiter_handle.key == key;

            // Original release should now be stale.
            let stale_reply = expect_reply(
                runtime.call_blocking_request(
                    lockmgr,
                    LockRequest::Release { handle: original },
                    timeout,
                )?,
                "stale release",
            )?;
            let original_release_was_stale = matches!(stale_reply, LockReply::StaleHandle);

            // Tidy up: release the new holder. We still want to confirm normal
            // release works after a hand-off chain.
            expect_released(runtime.call_blocking_request(
                lockmgr,
                LockRequest::Release {
                    handle: waiter_handle,
                },
                timeout,
            )?)?;

            let stats = stats(runtime, lockmgr, timeout)?;
            Ok(ExpiryHandoffReport {
                waiter_received_grant,
                original_release_was_stale,
                stats,
            })
        })
    })
}

/// Acquire, queue a waiter, renew before the lease expires, then verify
/// the waiter is still parked once the original lease window is past.
pub fn run_renewal(config: RunConfig) -> anyhow::Result<RenewalReport> {
    validate_config(config)?;
    let cfg = RunConfig {
        lease_ms: 120,
        ..config
    };
    run_local(cfg, |runtime| {
        let waiter_done = Mutex::new(false);
        thread::scope(|scope| {
            let lockmgr = register(runtime, cfg)?;
            let timeout = Duration::from_millis(cfg.call_timeout_ms);
            let key = "renew-key".to_string();

            let handle = acquire_handle(runtime, lockmgr, &key, timeout)?;

            let waiter_done_for_thread = &waiter_done;
            let key_for_waiter = key.clone();
            let waiter = scope.spawn(move || {
                let outcome = runtime.call_blocking_request(
                    lockmgr,
                    LockRequest::Acquire {
                        key: key_for_waiter,
                    },
                    timeout,
                );
                *waiter_done_for_thread.lock().expect("waiter done") = true;
                outcome
            });

            wait_for_waiters(runtime, lockmgr, 1, timeout)?;

            // Renew at half-lease; waiter must remain parked.
            thread::sleep(Duration::from_millis(cfg.lease_ms / 2));
            match expect_reply(
                runtime.call_blocking_request(
                    lockmgr,
                    LockRequest::Renew {
                        handle: handle.clone(),
                    },
                    timeout,
                )?,
                "renew",
            )? {
                LockReply::Renewed { .. } => {}
                other => anyhow::bail!("renew did not return Renewed: {other:?}"),
            }

            // Sleep past where the original (un-renewed) lease would have fired.
            thread::sleep(Duration::from_millis(cfg.lease_ms / 2 + 30));
            let still_held_after_original_lease = !*waiter_done.lock().expect("waiter done");

            // Final release should drain the waiter.
            let saved = handle.clone();
            expect_released(runtime.call_blocking_request(
                lockmgr,
                LockRequest::Release { handle },
                timeout,
            )?)?;
            let final_release_ok = true;

            let old_handle_renew_was_stale = matches!(
                expect_reply(
                    runtime.call_blocking_request(
                        lockmgr,
                        LockRequest::Renew { handle: saved },
                        timeout
                    )?,
                    "stale renew",
                )?,
                LockReply::StaleHandle
            );

            let waiter_handle = expect_granted(
                waiter
                    .join()
                    .map_err(|_| anyhow::anyhow!("waiter thread panicked"))??,
            )?;
            expect_released(runtime.call_blocking_request(
                lockmgr,
                LockRequest::Release {
                    handle: waiter_handle,
                },
                timeout,
            )?)?;

            let stats = stats(runtime, lockmgr, timeout)?;
            Ok(RenewalReport {
                still_held_after_original_lease,
                final_release_ok,
                old_handle_renew_was_stale,
                stats,
            })
        })
    })
}

/// Acquire, release, then release the same handle again. The second
/// release must be rejected as stale.
pub fn run_stale_release(config: RunConfig) -> anyhow::Result<StaleReleaseReport> {
    validate_config(config)?;
    let cfg = RunConfig {
        lease_ms: 5_000,
        ..config
    };
    run_local(cfg, |runtime| {
        let lockmgr = register(runtime, cfg)?;
        let timeout = Duration::from_millis(cfg.call_timeout_ms);
        let key = "stale-key".to_string();

        let handle = acquire_handle(runtime, lockmgr, &key, timeout)?;
        let saved = handle.clone();
        expect_released(runtime.call_blocking_request(
            lockmgr,
            LockRequest::Release { handle },
            timeout,
        )?)?;
        let second = expect_reply(
            runtime.call_blocking_request(
                lockmgr,
                LockRequest::Release { handle: saved },
                timeout,
            )?,
            "second release",
        )?;
        let second_release_was_stale = matches!(second, LockReply::StaleHandle);

        let stats = stats(runtime, lockmgr, timeout)?;
        Ok(StaleReleaseReport {
            second_release_was_stale,
            stats,
        })
    })
}

/// Hold one key, then burst more contenders than the per-key cap allows.
/// Overflow callers must see `Busy`, not park silently.
pub fn run_per_key_overflow(config: RunConfig) -> anyhow::Result<PerKeyOverflowReport> {
    validate_config(config)?;
    let cfg = RunConfig {
        lease_ms: 5_000,
        max_waiters_per_key: 2,
        ..config
    };
    run_local(cfg, |runtime| {
        let overflow = 3;
        let contenders = (cfg.max_waiters_per_key + overflow) as u32;
        let barrier = Barrier::new(contenders as usize + 1);
        thread::scope(|scope| {
            let lockmgr = register(runtime, cfg)?;
            let timeout = Duration::from_millis(cfg.call_timeout_ms);
            let key = "busy-key".to_string();

            let holder = acquire_handle(runtime, lockmgr, &key, timeout)?;

            // 2 admitted to the queue + 3 overflow callers that must see Busy.
            let mut threads = Vec::with_capacity(contenders as usize);
            for _ in 0..contenders {
                let gate = &barrier;
                let key_for_thread = key.clone();
                threads.push(
                    scope.spawn(move || -> anyhow::Result<CallOutcome<LockReply>> {
                        gate.wait();
                        let outcome = runtime.call_blocking_request(
                            lockmgr,
                            LockRequest::Acquire {
                                key: key_for_thread,
                            },
                            timeout,
                        )?;
                        // Granted callers release immediately so the per-key
                        // hand-off chain drains within the call timeout.
                        if let CallOutcome::Replied(LockReply::Granted { handle }) = &outcome {
                            expect_released(runtime.call_blocking_request(
                                lockmgr,
                                LockRequest::Release {
                                    handle: handle.clone(),
                                },
                                timeout,
                            )?)?;
                        }
                        Ok(outcome)
                    }),
                );
            }
            barrier.wait();

            // Wait until both the queue is full AND every overflow caller has
            // been counted as a per-key full reject. Counting on the server side
            // happens before the Busy reply lands, so this is the cleanest way
            // to know the burst has fully landed before we release the holder.
            wait_for_busy_settlement(
                runtime,
                lockmgr,
                cfg.max_waiters_per_key,
                overflow as u64,
                timeout,
            )?;

            // Release the holder so admitted waiters can drain.
            expect_released(runtime.call_blocking_request(
                lockmgr,
                LockRequest::Release { handle: holder },
                timeout,
            )?)?;

            let mut busy = 0;
            for contender in threads {
                match contender
                    .join()
                    .map_err(|_| anyhow::anyhow!("overflow contender panicked"))??
                {
                    CallOutcome::Replied(LockReply::Busy(WaitBusyReason::KeyFull)) => busy += 1,
                    CallOutcome::Replied(LockReply::Busy(WaitBusyReason::GlobalFull)) => {
                        anyhow::bail!("per-key overflow unexpectedly hit the global cap")
                    }
                    CallOutcome::Replied(LockReply::Granted { .. }) => {
                        // Already released inside the worker thread.
                    }
                    CallOutcome::Replied(reply) => {
                        anyhow::bail!("unexpected per-key overflow reply: {reply:?}")
                    }
                    CallOutcome::Full => anyhow::bail!("per-key overflow acquire mailbox was full"),
                    CallOutcome::Closed => {
                        anyhow::bail!("lock manager closed during per-key overflow")
                    }
                    CallOutcome::Timeout => anyhow::bail!("per-key overflow acquire timed out"),
                    CallOutcome::Rejected(reason) => {
                        anyhow::bail!("per-key overflow acquire rejected: {reason:?}")
                    }
                }
            }

            let stats = stats(runtime, lockmgr, timeout)?;
            Ok(PerKeyOverflowReport { busy, stats })
        })
    })
}

/// Saturate the global waiter table across two held keys. The second key is
/// below its per-key limit, so rejection must remain specifically global.
pub fn run_global_overflow(config: RunConfig) -> anyhow::Result<GlobalOverflowReport> {
    validate_config(config)?;
    let cfg = RunConfig {
        lease_ms: 5_000,
        waiter_capacity: 1,
        max_waiters_per_key: 2,
        ..config
    };
    run_local(cfg, |runtime| {
        thread::scope(|scope| {
            let lockmgr = register(runtime, cfg)?;
            let timeout = Duration::from_millis(cfg.call_timeout_ms);

            let first = acquire_handle(runtime, lockmgr, "global-a", timeout)?;
            let second = acquire_handle(runtime, lockmgr, "global-b", timeout)?;
            let waiter = scope.spawn(move || {
                runtime.call_blocking_request(
                    lockmgr,
                    LockRequest::Acquire {
                        key: "global-a".into(),
                    },
                    timeout,
                )
            });
            wait_for_waiters(runtime, lockmgr, 1, timeout)?;

            let global_full = matches!(
                expect_reply(
                    runtime.call_blocking_request(
                        lockmgr,
                        LockRequest::Acquire {
                            key: "global-b".into(),
                        },
                        timeout,
                    )?,
                    "global overflow acquire",
                )?,
                LockReply::Busy(WaitBusyReason::GlobalFull)
            );
            expect_released(runtime.call_blocking_request(
                lockmgr,
                LockRequest::Release { handle: first },
                timeout,
            )?)?;
            let granted = expect_granted(
                waiter
                    .join()
                    .map_err(|_| anyhow::anyhow!("global waiter panicked"))??,
            )?;
            expect_released(runtime.call_blocking_request(
                lockmgr,
                LockRequest::Release { handle: granted },
                timeout,
            )?)?;
            expect_released(runtime.call_blocking_request(
                lockmgr,
                LockRequest::Release { handle: second },
                timeout,
            )?)?;

            let stats = stats(runtime, lockmgr, timeout)?;
            Ok(GlobalOverflowReport { global_full, stats })
        })
    })
}

/// A timed-out FIFO head is reclaimed on refill. With capacity one, admitting
/// the replacement proves the closed caller released its exact table slot.
pub fn run_caller_gone_refill(config: RunConfig) -> anyhow::Result<CallerGoneRefillReport> {
    validate_config(config)?;
    let cfg = RunConfig {
        lease_ms: 5_000,
        waiter_capacity: 1,
        max_waiters_per_key: 1,
        ..config
    };
    run_local(cfg, |runtime| {
        thread::scope(|scope| {
            let lockmgr = register(runtime, cfg)?;
            let timeout = Duration::from_millis(cfg.call_timeout_ms);
            let holder = acquire_handle(runtime, lockmgr, "refill", timeout)?;

            let gone = scope.spawn(move || {
                runtime.call_blocking_request(
                    lockmgr,
                    LockRequest::Acquire {
                        key: "refill".into(),
                    },
                    Duration::from_millis(40),
                )
            });
            wait_for_waiters(runtime, lockmgr, 1, timeout)?;
            let first_timed_out = match gone
                .join()
                .map_err(|_| anyhow::anyhow!("timed-out waiter panicked"))??
            {
                CallOutcome::Timeout => true,
                CallOutcome::Replied(reply) => {
                    anyhow::bail!("caller-gone probe unexpectedly replied: {reply:?}")
                }
                CallOutcome::Full => anyhow::bail!("caller-gone probe mailbox was full"),
                CallOutcome::Closed => {
                    anyhow::bail!("lock manager closed during caller-gone probe")
                }
                CallOutcome::Rejected(reason) => {
                    anyhow::bail!("caller-gone probe rejected: {reason:?}")
                }
            };

            let replacement = scope.spawn(move || {
                runtime.call_blocking_request(
                    lockmgr,
                    LockRequest::Acquire {
                        key: "refill".into(),
                    },
                    timeout,
                )
            });
            wait_for_reclaimed(runtime, lockmgr, 1, timeout)?;
            expect_released(runtime.call_blocking_request(
                lockmgr,
                LockRequest::Release { handle: holder },
                timeout,
            )?)?;
            let replacement_handle = expect_granted(
                replacement
                    .join()
                    .map_err(|_| anyhow::anyhow!("replacement waiter panicked"))??,
            )?;
            let next_waiter_granted = replacement_handle.key == "refill";
            expect_released(runtime.call_blocking_request(
                lockmgr,
                LockRequest::Release {
                    handle: replacement_handle,
                },
                timeout,
            )?)?;

            let stats = stats(runtime, lockmgr, timeout)?;
            Ok(CallerGoneRefillReport {
                first_timed_out,
                next_waiter_granted,
                stats,
            })
        })
    })
}

pub fn run_keyspace_overflow(config: RunConfig) -> anyhow::Result<KeyspaceOverflowReport> {
    validate_config(config)?;
    let cfg = RunConfig {
        lease_ms: 5_000,
        max_keys: 1,
        ..config
    };
    run_local(cfg, |runtime| {
        let lockmgr = register(runtime, cfg)?;
        let timeout = Duration::from_millis(cfg.call_timeout_ms);
        let holder = acquire_handle(runtime, lockmgr, "only-key", timeout)?;
        let keyspace_full = matches!(
            expect_reply(
                runtime.call_blocking_request(
                    lockmgr,
                    LockRequest::Acquire {
                        key: "second-key".into(),
                    },
                    timeout,
                )?,
                "keyspace overflow acquire",
            )?,
            LockReply::KeyspaceFull
        );
        expect_released(runtime.call_blocking_request(
            lockmgr,
            LockRequest::Release { handle: holder },
            timeout,
        )?)?;
        let stats = stats(runtime, lockmgr, timeout)?;
        Ok(KeyspaceOverflowReport {
            keyspace_full,
            stats,
        })
    })
}

// ---------- Helpers ----------

fn register(
    runtime: &App,
    config: RunConfig,
) -> anyhow::Result<tina::ServiceRequestAddress<LockEvent, LockRequest, LockReply>> {
    validate_config(config)?;
    runtime
        .register_split_service::<LockManager, LockEvent, LockRequest, Infallible>(
            LockManager::new(config),
            config.mailbox,
        )
        .map(|handle| handle.requests)
        .map_err(|e| anyhow::anyhow!("register lock manager: {e:?}"))
}

fn validate_config(config: RunConfig) -> anyhow::Result<()> {
    config.validate()?;
    Ok(())
}

fn acquire_handle(
    runtime: &App,
    lockmgr: tina::ServiceRequestAddress<LockEvent, LockRequest, LockReply>,
    key: &str,
    timeout: Duration,
) -> anyhow::Result<LockHandle> {
    expect_granted(runtime.call_blocking_request(
        lockmgr,
        LockRequest::Acquire { key: key.into() },
        timeout,
    )?)
}

fn expect_granted(outcome: CallOutcome<LockReply>) -> anyhow::Result<LockHandle> {
    match expect_reply(outcome, "acquire")? {
        LockReply::Granted { handle } => Ok(handle),
        reply => anyhow::bail!("expected Granted, got {reply:?}"),
    }
}

fn expect_released(outcome: CallOutcome<LockReply>) -> anyhow::Result<()> {
    match expect_reply(outcome, "release")? {
        LockReply::Released => Ok(()),
        reply => anyhow::bail!("expected Released, got {reply:?}"),
    }
}

fn expect_reply(outcome: CallOutcome<LockReply>, operation: &str) -> anyhow::Result<LockReply> {
    match outcome {
        CallOutcome::Replied(reply) => Ok(reply),
        CallOutcome::Full => anyhow::bail!("{operation} mailbox was full"),
        CallOutcome::Closed => anyhow::bail!("lock manager closed during {operation}"),
        CallOutcome::Timeout => anyhow::bail!("{operation} timed out"),
        CallOutcome::Rejected(reason) => anyhow::bail!("{operation} rejected: {reason:?}"),
    }
}

fn stats(
    runtime: &App,
    lockmgr: tina::ServiceRequestAddress<LockEvent, LockRequest, LockReply>,
    timeout: Duration,
) -> anyhow::Result<LockStats> {
    match expect_reply(
        runtime.call_blocking_request(lockmgr, LockRequest::Stats, timeout)?,
        "stats",
    )? {
        LockReply::Stats(s) => Ok(s),
        reply => anyhow::bail!("stats returned unexpected reply: {reply:?}"),
    }
}

fn wait_for_waiters(
    runtime: &App,
    lockmgr: tina::ServiceRequestAddress<LockEvent, LockRequest, LockReply>,
    target: usize,
    timeout: Duration,
) -> anyhow::Result<()> {
    let start = std::time::Instant::now();
    loop {
        let s = stats(runtime, lockmgr, timeout)?;
        if s.waiters_live >= target {
            return Ok(());
        }
        if start.elapsed() > timeout {
            anyhow::bail!(
                "wait_for_waiters({target}) timed out at waiters_live={}",
                s.waiters_live
            );
        }
        thread::sleep(Duration::from_millis(2));
    }
}

fn wait_for_busy_settlement(
    runtime: &App,
    lockmgr: tina::ServiceRequestAddress<LockEvent, LockRequest, LockReply>,
    waiters_target: usize,
    busy_target: u64,
    timeout: Duration,
) -> anyhow::Result<()> {
    let start = std::time::Instant::now();
    loop {
        let s = stats(runtime, lockmgr, timeout)?;
        if s.waiters_live >= waiters_target && s.per_key_full_rejects >= busy_target {
            return Ok(());
        }
        if start.elapsed() > timeout {
            anyhow::bail!(
                "wait_for_busy_settlement timed out at waiters_live={}, per_key_full_rejects={}",
                s.waiters_live,
                s.per_key_full_rejects
            );
        }
        thread::sleep(Duration::from_millis(2));
    }
}

fn wait_for_reclaimed(
    runtime: &App,
    lockmgr: tina::ServiceRequestAddress<LockEvent, LockRequest, LockReply>,
    target: u64,
    timeout: Duration,
) -> anyhow::Result<()> {
    let start = std::time::Instant::now();
    loop {
        let s = stats(runtime, lockmgr, timeout)?;
        if s.waiters_reclaimed >= target {
            return Ok(());
        }
        if start.elapsed() > timeout {
            anyhow::bail!(
                "wait_for_reclaimed({target}) timed out at reclaimed={}",
                s.waiters_reclaimed
            );
        }
        thread::sleep(Duration::from_millis(2));
    }
}

fn start(config: RunConfig) -> anyhow::Result<App> {
    validate_config(config)?;
    Ok(LocalSystem::single_shard(SingleShard, DefaultThreadedMailboxFactory).try_build()?)
}

fn run_local<T>(
    config: RunConfig,
    workload: impl FnOnce(&App) -> anyhow::Result<T>,
) -> anyhow::Result<T> {
    Ok(start(config)?.run_to_shutdown_reported(Duration::from_secs(5), workload)?)
}

#[cfg(test)]
mod unit_tests {
    use super::*;
    use std::any::TypeId;

    fn fake_reply(slot_id: u64) -> (DeferredReply<LockReply>, Arc<tina::DeferredSlotShared>) {
        let shared = Arc::new(tina::DeferredSlotShared::new(
            slot_id,
            TypeId::of::<LockReply>(),
        ));
        let reply = tina::runtime_internal::deferred_from_handle(
            tina::runtime_internal::handle_from_shared(Arc::clone(&shared)),
        );
        (reply, shared)
    }

    #[test]
    fn current_timer_failure_retires_the_unenforceable_lease() {
        let mut manager = LockManager::new(RunConfig::default());
        manager.locks.insert(
            "held".into(),
            LockState {
                holder_id: 1,
                expiry_token: 1,
            },
        );
        let effect =
            manager.lease_expired("held".into(), 1, 1, Err(tina_runtime::CallError::TimerFull));
        assert!(matches!(effect, Effect::Noop));
        assert_eq!(manager.stats.timer_errors, 1);
        assert_eq!(manager.stats.last_timer_error, Some(CallError::TimerFull));
        assert_eq!(manager.stats.expiries, 0);
        assert_eq!(manager.stats.stale_expiries_ignored, 0);
        assert!(manager.locks.is_empty());
    }

    #[test]
    fn stale_timer_failure_is_typed_without_revoking_the_current_holder() {
        let mut manager = LockManager::new(RunConfig::default());
        manager.locks.insert(
            "held".into(),
            LockState {
                holder_id: 2,
                expiry_token: 3,
            },
        );
        let effect =
            manager.lease_expired("held".into(), 1, 1, Err(tina_runtime::CallError::TimerFull));
        assert!(matches!(effect, Effect::Noop));
        assert_eq!(manager.stats.timer_errors, 1);
        assert_eq!(manager.stats.stale_expiries_ignored, 1);
        assert_eq!(manager.locks["held"].holder_id, 2);
    }

    #[test]
    fn selected_waiter_late_close_keeps_exclusivity_until_lease_expiry_rolls_back() {
        let mut manager = LockManager::new(RunConfig::default());
        manager.locks.insert(
            "held".into(),
            LockState {
                holder_id: 1,
                expiry_token: 1,
            },
        );
        manager.next_holder_id = 1;
        let (slot, shared) = fake_reply(7);
        shared.set_state(tina::DeferredSlotState::Closed);

        let effects = manager.install_handoff("held".into(), slot);
        assert_eq!(effects.len(), 2, "reply and expiry must remain paired");
        let holder = manager.locks["held"].holder_id;
        let token = manager.locks["held"].expiry_token;
        assert_ne!(holder, 1, "the old holder loses authority at handoff");

        let expiry = manager.lease_expired("held".into(), holder, token, Ok(()));
        assert!(matches!(expiry, Effect::Noop));
        assert!(
            manager.locks.is_empty(),
            "ghost holder retires at lease expiry"
        );
    }

    #[test]
    fn shutdown_closes_a_parked_caller_and_reports_clean() {
        // The test must initiate shutdown before the caller returns, which is
        // the lower-level control-flow case outside run_to_shutdown's contract.
        let cfg = RunConfig {
            lease_ms: 5_000,
            ..RunConfig::default()
        };
        let runtime = Arc::new(start(cfg).expect("start"));
        let shutdown = runtime.shutdown_handle();
        let lockmgr = register(&runtime, cfg).expect("register");
        let timeout = Duration::from_secs(2);
        let _holder = acquire_handle(&runtime, lockmgr, "shutdown", timeout).expect("holder");
        let caller_runtime = Arc::clone(&runtime);
        let caller = thread::spawn(move || {
            caller_runtime.call_blocking_request(
                lockmgr,
                LockRequest::Acquire {
                    key: "shutdown".into(),
                },
                timeout,
            )
        });
        wait_for_waiters(&runtime, lockmgr, 1, timeout).expect("waiter parked");

        let terminal = shutdown
            .request_and_wait_report(Duration::from_secs(5))
            .expect("shutdown report");
        let outcome = caller.join().expect("caller thread");
        assert!(matches!(
            outcome,
            Err(tina_runtime::ThreadedRuntimeError::WorkerStopped)
        ));
        drop(runtime);
        terminal.ensure_clean().expect("clean terminal report");
    }
}
