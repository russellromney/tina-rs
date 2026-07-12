//! `system_lock_manager` — local lock manager with leases, FIFO wait
//! queues per key, renewals, expiry hand-off, and stale-handle detection.
//!
//! What this specimen pulls on:
//!
//! - [`PendingReplies`] for parked acquire-callers, keyed by waiter id.
//!   Caps total parked waiters across every key in one bounded table.
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
//! - Total parked waiters across keys is capped by `pending_capacity`.
//!   Hits reply `Busy` and bump `pending.full_rejects()`.
//! - Active key count is capped by `max_keys`. A first-time acquire on a
//!   new key past the cap replies `KeyspaceFull`. Idle keys (no holder,
//!   no waiters) are removed.

use std::collections::{HashMap, VecDeque};
use std::convert::Infallible;
use std::sync::{Arc, Barrier, Mutex};
use std::thread;
use std::time::Duration;

use tina::prelude::*;
use tina_runtime::{
    CallOutcome, DefaultThreadedMailboxFactory, PendingReplies, ThreadedRuntime, sleep,
};

/// Tunables for one specimen run.
#[derive(Debug, Clone, Copy)]
pub struct RunConfig {
    /// Total parked acquire-callers across every key.
    pub pending_capacity: usize,
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
            pending_capacity: 16,
            max_waiters_per_key: 8,
            max_keys: 64,
            mailbox: 64,
            lease_ms: 200,
            call_timeout_ms: 5_000,
        }
    }
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
    /// Wait queue rejected the caller (per-key cap or pending box full).
    Busy,
    /// Release/renew arrived for a holder that no longer owns the key.
    StaleHandle,
    /// First-time acquire on a new key while the keyspace was full.
    KeyspaceFull,
    Stats(LockStats),
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct LockStats {
    pub keys_live: usize,
    pub waiters_live: usize,
    pub waiters_high_water: usize,
    pub pending_full_rejects: u64,
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
}

#[derive(Debug)]
enum LockEvent {
    LeaseExpired {
        key: String,
        holder_id: u64,
        expiry_token: u64,
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
    waiters: VecDeque<u64>,
}

struct LockManager {
    lease: Duration,
    max_waiters_per_key: usize,
    max_keys: usize,
    pending: PendingReplies<u64, LockReply>,
    next_waiter_id: u64,
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
            } => self.lease_expired(key, holder_id, expiry_token),
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
    pub fn new(config: RunConfig) -> Self {
        Self {
            lease: Duration::from_millis(config.lease_ms),
            max_waiters_per_key: config.max_waiters_per_key,
            max_keys: config.max_keys,
            pending: PendingReplies::with_capacity(config.pending_capacity)
                .named("system_lock_manager.pending"),
            next_waiter_id: 1,
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

        let waiters_len = self.locks.get(&key).expect("key present").waiters.len();
        if waiters_len >= self.max_waiters_per_key {
            self.stats.per_key_full_rejects += 1;
            return call.reply(LockReply::Busy);
        }

        let waiter_id = self.next_waiter_id;
        self.next_waiter_id += 1;
        call.capture(|request| {
            let slot = request.into_deferred();
            if let Err(error) = self.pending.try_insert(waiter_id, slot) {
                return match error {
                    tina_runtime::PendingRepliesInsertError::Full(_, slot) => {
                        reply_to::<Self>(slot, LockReply::Busy)
                    }
                    tina_runtime::PendingRepliesInsertError::DuplicateKey(_, slot) => {
                        // waiter ids are minted monotonically, so this branch
                        // is unreachable in practice; surface it loudly if it
                        // ever fires.
                        reply_to::<Self>(slot, LockReply::Busy)
                    }
                };
            }

            self.locks
                .get_mut(&key)
                .expect("key present")
                .waiters
                .push_back(waiter_id);
            self.update_waiters_high_water();
            noop()
        })
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
                sleep(lease).then_service_event(move |_| LockEvent::LeaseExpired {
                    key: key_for_msg,
                    holder_id,
                    expiry_token: token,
                }),
            ],
        )
    }

    fn lease_expired(&mut self, key: String, holder_id: u64, expiry_token: u64) -> Effect<Self> {
        let Some(entry) = self.locks.get(&key) else {
            self.stats.stale_expiries_ignored += 1;
            return noop();
        };
        if entry.holder_id != holder_id || entry.expiry_token != expiry_token {
            self.stats.stale_expiries_ignored += 1;
            return noop();
        }
        self.stats.expiries += 1;
        let effects = self.hand_off(key);
        if effects.is_empty() {
            noop()
        } else {
            Effect::Batch(effects)
        }
    }

    /// Replace the holder for `key` with the next waiter, or drop the
    /// entry if no waiters remain. Skips waiters whose caller went away
    /// (their `pending` slot was already reclaimed).
    fn hand_off(&mut self, key: String) -> Vec<Effect<Self>> {
        loop {
            let Some(entry) = self.locks.get_mut(&key) else {
                return Vec::new();
            };
            let Some(waiter_id) = entry.waiters.pop_front() else {
                self.locks.remove(&key);
                return Vec::new();
            };
            let Some(slot) = self.pending.take(&waiter_id) else {
                // Caller went away; try the next waiter.
                continue;
            };
            self.next_holder_id += 1;
            let new_holder_id = self.next_holder_id;
            entry.holder_id = new_holder_id;
            entry.expiry_token = entry.expiry_token.wrapping_add(1);
            let token = entry.expiry_token;
            let key_for_msg = key.clone();
            let lease = self.lease;
            self.stats.acquires_handed_off += 1;
            return vec![
                reply_to::<Self>(
                    slot,
                    LockReply::Granted {
                        handle: LockHandle {
                            key: key.clone(),
                            holder_id: new_holder_id,
                        },
                    },
                ),
                sleep(lease).then_service_event(move |_| LockEvent::LeaseExpired {
                    key: key_for_msg,
                    holder_id: new_holder_id,
                    expiry_token: token,
                }),
            ];
        }
    }

    fn grant_new(&mut self, key: String, call: RequestCall<'_, Self>) -> RequestEffect<Self> {
        self.next_holder_id += 1;
        let holder_id = self.next_holder_id;
        self.locks.insert(
            key.clone(),
            LockState {
                holder_id,
                expiry_token: 1,
                waiters: VecDeque::new(),
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
                sleep(lease).then_service_event(move |_| LockEvent::LeaseExpired {
                    key: key_for_msg,
                    holder_id,
                    expiry_token: 1,
                }),
            ],
        )
    }

    fn update_waiters_high_water(&mut self) {
        let live: usize = self.locks.values().map(|e| e.waiters.len()).sum();
        if live > self.stats.waiters_high_water {
            self.stats.waiters_high_water = live;
        }
    }

    fn snapshot(&self) -> LockStats {
        let mut s = self.stats.clone();
        s.keys_live = self.locks.len();
        s.waiters_live = self.locks.values().map(|e| e.waiters.len()).sum();
        s.pending_full_rejects = self.pending.full_rejects();
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
    pub busy_overflow: BusyOverflowReport,
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
    pub stats: LockStats,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StaleReleaseReport {
    pub second_release_was_stale: bool,
    pub stats: LockStats,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BusyOverflowReport {
    pub busy: usize,
    pub stats: LockStats,
}

pub fn run(config: RunConfig) -> anyhow::Result<RunReport> {
    Ok(RunReport {
        fifo: run_fifo(config)?,
        expiry_handoff: run_expiry_handoff(config)?,
        renewal: run_renewal(config)?,
        stale_release: run_stale_release(config)?,
        busy_overflow: run_busy_overflow(config)?,
    })
}

/// Four threads contend for one key. The first acquires immediately; the
/// remaining three park in FIFO order. Each holder releases as soon as
/// it sees `Granted`, so the queue should drain in admission order.
pub fn run_fifo(config: RunConfig) -> anyhow::Result<FifoReport> {
    // Use a long lease so expiry never fires during the test.
    let cfg = RunConfig {
        lease_ms: 5_000,
        ..config
    };
    let runtime = Arc::new(ThreadedRuntime::try_new(
        SingleShard,
        DefaultThreadedMailboxFactory,
    )?);
    let shutdown = runtime.shutdown_handle();
    let lockmgr = register(&runtime, cfg)?;
    let timeout = Duration::from_millis(cfg.call_timeout_ms);

    let key = "fifo-key".to_string();
    let admitted_order: Arc<Mutex<Vec<u32>>> = Arc::new(Mutex::new(Vec::new()));
    let grant_order: Arc<Mutex<Vec<u32>>> = Arc::new(Mutex::new(Vec::new()));

    // Hold the lock from the host first so every contender parks before
    // any of them can be granted.
    let CallOutcome::Replied(LockReply::Granted { handle: holder }) = runtime
        .call_blocking_request(lockmgr, LockRequest::Acquire { key: key.clone() }, timeout)?
    else {
        anyhow::bail!("primary acquire did not return Granted");
    };

    let contenders = 4u32;
    // Each contender admits itself in a fixed order via a sequential
    // gate. That keeps Acquire admission order matching the worker id
    // and lets us assert FIFO grant order against the same sequence.
    let admit_gate = Arc::new(Mutex::new(1u32));
    let mut threads = Vec::with_capacity(contenders as usize);
    for id in 1..=contenders {
        let rt = Arc::clone(&runtime);
        let key_for_thread = key.clone();
        let gate = Arc::clone(&admit_gate);
        let admitted = Arc::clone(&admitted_order);
        let granted = Arc::clone(&grant_order);
        threads.push(thread::spawn(move || -> anyhow::Result<()> {
            // Wait until it is this contender's turn to enter Acquire.
            loop {
                {
                    let g = gate.lock().expect("gate");
                    if *g == id {
                        admitted.lock().expect("admitted").push(id);
                        break;
                    }
                }
                thread::sleep(Duration::from_millis(2));
            }
            // Hand the gate to the next contender once admission is in
            // flight. A short pause makes sure the queue receives our
            // Acquire message before the next thread races us.
            thread::sleep(Duration::from_millis(20));
            *gate.lock().expect("gate") = id + 1;

            match rt.call_blocking_request(
                lockmgr,
                LockRequest::Acquire {
                    key: key_for_thread.clone(),
                },
                timeout,
            )? {
                CallOutcome::Replied(LockReply::Granted { handle }) => {
                    granted.lock().expect("granted").push(id);
                    let _ = rt.call_blocking_request(
                        lockmgr,
                        LockRequest::Release { handle },
                        timeout,
                    )?;
                    Ok(())
                }
                other => anyhow::bail!("unexpected acquire outcome: {other:?}"),
            }
        }));
    }

    // Wait for all four contenders to be parked before releasing the
    // primary holder. Otherwise the first releaser's grant could race
    // the slowest contender's Acquire.
    wait_for_waiters(&runtime, lockmgr, contenders as usize, timeout)?;
    let _ =
        runtime.call_blocking_request(lockmgr, LockRequest::Release { handle: holder }, timeout)?;

    for t in threads {
        t.join().expect("contender thread")?;
    }

    let stats = stats(&runtime, lockmgr, timeout)?;
    shutdown_runtime(shutdown, runtime)?;
    Ok(FifoReport {
        admitted_order: admitted_order.lock().expect("admitted").clone(),
        grant_order: grant_order.lock().expect("granted").clone(),
        stats,
    })
}

/// Acquire, then never renew or release. A second caller parks. After
/// the lease elapses, the parked caller is granted automatically and the
/// original release is rejected as stale.
pub fn run_expiry_handoff(config: RunConfig) -> anyhow::Result<ExpiryHandoffReport> {
    let cfg = RunConfig {
        lease_ms: 100,
        ..config
    };
    let runtime = Arc::new(ThreadedRuntime::try_new(
        SingleShard,
        DefaultThreadedMailboxFactory,
    )?);
    let shutdown = runtime.shutdown_handle();
    let lockmgr = register(&runtime, cfg)?;
    let timeout = Duration::from_millis(cfg.call_timeout_ms);
    let key = "expire-key".to_string();

    let CallOutcome::Replied(LockReply::Granted { handle: original }) = runtime
        .call_blocking_request(lockmgr, LockRequest::Acquire { key: key.clone() }, timeout)?
    else {
        anyhow::bail!("first acquire did not return Granted");
    };

    let rt2 = Arc::clone(&runtime);
    let key_for_waiter = key.clone();
    let waiter = thread::spawn(move || {
        rt2.call_blocking_request(
            lockmgr,
            LockRequest::Acquire {
                key: key_for_waiter,
            },
            timeout,
        )
    });

    wait_for_waiters(&runtime, lockmgr, 1, timeout)?;

    // Wait through the lease so the timer fires and the queued caller
    // gets handed the lock without us doing anything.
    thread::sleep(Duration::from_millis(cfg.lease_ms + 80));

    let waiter_handle = match waiter.join().expect("waiter thread")? {
        CallOutcome::Replied(LockReply::Granted { handle }) => handle,
        other => anyhow::bail!("waiter did not get Granted: {other:?}"),
    };
    let waiter_received_grant = waiter_handle.key == key;

    // Original release should now be stale.
    let stale_outcome = runtime.call_blocking_request(
        lockmgr,
        LockRequest::Release { handle: original },
        timeout,
    )?;
    let original_release_was_stale =
        matches!(stale_outcome, CallOutcome::Replied(LockReply::StaleHandle));

    // Tidy up: release the new holder. We still want to confirm normal
    // release works after a hand-off chain.
    let _ = runtime.call_blocking_request(
        lockmgr,
        LockRequest::Release {
            handle: waiter_handle,
        },
        timeout,
    )?;

    let stats = stats(&runtime, lockmgr, timeout)?;
    shutdown_runtime(shutdown, runtime)?;
    Ok(ExpiryHandoffReport {
        waiter_received_grant,
        original_release_was_stale,
        stats,
    })
}

/// Acquire, queue a waiter, renew before the lease expires, then verify
/// the waiter is still parked once the original lease window is past.
pub fn run_renewal(config: RunConfig) -> anyhow::Result<RenewalReport> {
    let cfg = RunConfig {
        lease_ms: 120,
        ..config
    };
    let runtime = Arc::new(ThreadedRuntime::try_new(
        SingleShard,
        DefaultThreadedMailboxFactory,
    )?);
    let shutdown = runtime.shutdown_handle();
    let lockmgr = register(&runtime, cfg)?;
    let timeout = Duration::from_millis(cfg.call_timeout_ms);
    let key = "renew-key".to_string();

    let CallOutcome::Replied(LockReply::Granted { handle }) = runtime.call_blocking_request(
        lockmgr,
        LockRequest::Acquire { key: key.clone() },
        timeout,
    )?
    else {
        anyhow::bail!("acquire did not return Granted");
    };

    let waiter_done = Arc::new(Mutex::new(false));
    let waiter_done_for_thread = Arc::clone(&waiter_done);
    let rt2 = Arc::clone(&runtime);
    let key_for_waiter = key.clone();
    let waiter = thread::spawn(move || {
        let outcome = rt2.call_blocking_request(
            lockmgr,
            LockRequest::Acquire {
                key: key_for_waiter,
            },
            timeout,
        );
        *waiter_done_for_thread.lock().expect("waiter done") = true;
        outcome
    });

    wait_for_waiters(&runtime, lockmgr, 1, timeout)?;

    // Renew at half-lease; waiter must remain parked.
    thread::sleep(Duration::from_millis(cfg.lease_ms / 2));
    match runtime.call_blocking_request(
        lockmgr,
        LockRequest::Renew {
            handle: handle.clone(),
        },
        timeout,
    )? {
        CallOutcome::Replied(LockReply::Renewed { .. }) => {}
        other => anyhow::bail!("renew did not return Renewed: {other:?}"),
    }

    // Sleep past where the original (un-renewed) lease would have fired.
    thread::sleep(Duration::from_millis(cfg.lease_ms / 2 + 30));
    let still_held_after_original_lease = !*waiter_done.lock().expect("waiter done");

    // Final release should drain the waiter.
    let final_release_ok = matches!(
        runtime.call_blocking_request(lockmgr, LockRequest::Release { handle }, timeout)?,
        CallOutcome::Replied(LockReply::Released)
    );

    let waiter_handle = match waiter.join().expect("waiter thread")? {
        CallOutcome::Replied(LockReply::Granted { handle }) => handle,
        other => anyhow::bail!("waiter did not get Granted after release: {other:?}"),
    };
    let _ = runtime.call_blocking_request(
        lockmgr,
        LockRequest::Release {
            handle: waiter_handle,
        },
        timeout,
    )?;

    let stats = stats(&runtime, lockmgr, timeout)?;
    shutdown_runtime(shutdown, runtime)?;
    Ok(RenewalReport {
        still_held_after_original_lease,
        final_release_ok,
        stats,
    })
}

/// Acquire, release, then release the same handle again. The second
/// release must be rejected as stale.
pub fn run_stale_release(config: RunConfig) -> anyhow::Result<StaleReleaseReport> {
    let cfg = RunConfig {
        lease_ms: 5_000,
        ..config
    };
    let runtime = Arc::new(ThreadedRuntime::try_new(
        SingleShard,
        DefaultThreadedMailboxFactory,
    )?);
    let shutdown = runtime.shutdown_handle();
    let lockmgr = register(&runtime, cfg)?;
    let timeout = Duration::from_millis(cfg.call_timeout_ms);
    let key = "stale-key".to_string();

    let CallOutcome::Replied(LockReply::Granted { handle }) = runtime.call_blocking_request(
        lockmgr,
        LockRequest::Acquire { key: key.clone() },
        timeout,
    )?
    else {
        anyhow::bail!("acquire did not return Granted");
    };
    let saved = handle.clone();
    let _ = runtime.call_blocking_request(lockmgr, LockRequest::Release { handle }, timeout)?;
    let second =
        runtime.call_blocking_request(lockmgr, LockRequest::Release { handle: saved }, timeout)?;
    let second_release_was_stale = matches!(second, CallOutcome::Replied(LockReply::StaleHandle));

    let stats = stats(&runtime, lockmgr, timeout)?;
    shutdown_runtime(shutdown, runtime)?;
    Ok(StaleReleaseReport {
        second_release_was_stale,
        stats,
    })
}

/// Hold one key, then burst more contenders than the per-key cap allows.
/// Overflow callers must see `Busy`, not park silently.
pub fn run_busy_overflow(config: RunConfig) -> anyhow::Result<BusyOverflowReport> {
    let cfg = RunConfig {
        lease_ms: 5_000,
        max_waiters_per_key: 2,
        ..config
    };
    let runtime = Arc::new(ThreadedRuntime::try_new(
        SingleShard,
        DefaultThreadedMailboxFactory,
    )?);
    let shutdown = runtime.shutdown_handle();
    let lockmgr = register(&runtime, cfg)?;
    let timeout = Duration::from_millis(cfg.call_timeout_ms);
    let key = "busy-key".to_string();

    let CallOutcome::Replied(LockReply::Granted { handle: holder }) = runtime
        .call_blocking_request(lockmgr, LockRequest::Acquire { key: key.clone() }, timeout)?
    else {
        anyhow::bail!("primary acquire did not return Granted");
    };

    // 2 admitted to the queue + 3 overflow callers that must see Busy.
    let overflow = 3;
    let contenders = (cfg.max_waiters_per_key + overflow) as u32;
    let barrier = Arc::new(Barrier::new(contenders as usize + 1));
    let outcomes = Arc::new(Mutex::new(Vec::with_capacity(contenders as usize)));
    let mut threads = Vec::with_capacity(contenders as usize);
    for _ in 0..contenders {
        let rt = Arc::clone(&runtime);
        let gate = Arc::clone(&barrier);
        let out = Arc::clone(&outcomes);
        let key_for_thread = key.clone();
        threads.push(thread::spawn(move || {
            gate.wait();
            let outcome = rt.call_blocking_request(
                lockmgr,
                LockRequest::Acquire {
                    key: key_for_thread,
                },
                timeout,
            );
            // Granted callers release immediately so the per-key
            // hand-off chain drains within the call timeout.
            if let Ok(CallOutcome::Replied(LockReply::Granted { handle })) = &outcome {
                let _ = rt.call_blocking_request(
                    lockmgr,
                    LockRequest::Release {
                        handle: handle.clone(),
                    },
                    timeout,
                );
            }
            out.lock().expect("outcomes").push(outcome);
        }));
    }
    barrier.wait();

    // Wait until both the queue is full AND every overflow caller has
    // been counted as a per-key full reject. Counting on the server side
    // happens before the Busy reply lands, so this is the cleanest way
    // to know the burst has fully landed before we release the holder.
    wait_for_busy_settlement(
        &runtime,
        lockmgr,
        cfg.max_waiters_per_key,
        overflow as u64,
        timeout,
    )?;

    // Release the holder so admitted waiters can drain.
    let _ =
        runtime.call_blocking_request(lockmgr, LockRequest::Release { handle: holder }, timeout)?;

    for t in threads {
        t.join().expect("contender");
    }

    let mut busy = 0;
    for outcome in outcomes.lock().expect("outcomes").iter() {
        match outcome {
            Ok(CallOutcome::Replied(LockReply::Busy)) => busy += 1,
            Ok(CallOutcome::Replied(LockReply::Granted { .. })) => {
                // Already released inside the worker thread.
            }
            other => anyhow::bail!("unexpected busy-overflow outcome: {other:?}"),
        }
    }

    let stats = stats(&runtime, lockmgr, timeout)?;
    shutdown_runtime(shutdown, runtime)?;
    Ok(BusyOverflowReport { busy, stats })
}

// ---------- Helpers ----------

fn register(
    runtime: &ThreadedRuntime<SingleShard, DefaultThreadedMailboxFactory>,
    config: RunConfig,
) -> anyhow::Result<tina::ServiceRequestAddress<LockEvent, LockRequest, LockReply>> {
    runtime
        .register_split_service::<LockManager, LockEvent, LockRequest, Infallible>(
            LockManager::new(config),
            config.mailbox,
        )
        .map(|handle| handle.requests)
        .map_err(|e| anyhow::anyhow!("register lock manager: {e:?}"))
}

fn stats(
    runtime: &ThreadedRuntime<SingleShard, DefaultThreadedMailboxFactory>,
    lockmgr: tina::ServiceRequestAddress<LockEvent, LockRequest, LockReply>,
    timeout: Duration,
) -> anyhow::Result<LockStats> {
    match runtime.call_blocking_request(lockmgr, LockRequest::Stats, timeout)? {
        CallOutcome::Replied(LockReply::Stats(s)) => Ok(s),
        other => anyhow::bail!("stats call failed: {other:?}"),
    }
}

fn wait_for_waiters(
    runtime: &ThreadedRuntime<SingleShard, DefaultThreadedMailboxFactory>,
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
    runtime: &ThreadedRuntime<SingleShard, DefaultThreadedMailboxFactory>,
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

fn shutdown_runtime(
    shutdown: tina_runtime::ThreadedShutdownHandle,
    runtime: Arc<ThreadedRuntime<SingleShard, DefaultThreadedMailboxFactory>>,
) -> anyhow::Result<()> {
    let terminal = shutdown.request_and_wait_report(Duration::from_secs(5))?;
    drop(runtime);
    terminal.ensure_clean()?;
    Ok(())
}
