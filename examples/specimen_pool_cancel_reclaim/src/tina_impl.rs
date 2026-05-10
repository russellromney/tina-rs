//! Tina side. The driver uses `call_with_handle` to retain a
//! `CallHandle` per parked waiter — stored in a bounded
//! [`PendingCallSet`] keyed by waiter index — fires `cancel_call` on
//! each, and confirms via the pool's `PressureReport` that the cancels
//! were reclaimed before the retry wave runs.

use std::convert::Infallible;
use std::sync::Arc;
use std::time::Duration;

use tina::pool::{AcquireFailure, PoolConfig, PoolLease, ReleaseDisposition};
use tina::prelude::*;
use tina_runtime::pool::{
    WorkerPool, WorkerPoolMsg, WorkerPoolReply, acquire_result_effect,
    acquire_with_handle_effect, pressure_effect, release_result_effect, try_acquired,
};
use tina_runtime::{CallOutcome, DefaultThreadedMailboxFactory, ThreadedRuntime, cancel_call};

use crate::{Report, WAITERS};

const CALL_TIMEOUT: Duration = Duration::from_secs(5);

type Resource = u32;

#[derive(Debug)]
enum DriverMsg {
    BeginPrime,
    BeginWaiters,
    AcquireReturned {
        wave: Wave,
        result: Result<PoolLease<Resource>, AcquireFailure>,
    },
    CancelAll,
    CancelReturned,
    RetryAll,
    PressureSnapshot,
    PressureReturned(CallOutcome<WorkerPoolReply<Resource>>),
    ReleaseHeld,
    ReleaseReturned,
    Finish,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Wave {
    Prime,
    Park,
    Retry,
}

struct Driver {
    pool: Address<WorkerPoolMsg<Resource>, WorkerPoolReply<Resource>>,
    held: Option<PoolLease<Resource>>,
    /// Bounded park-waiter table. Sized to `WAITERS`; insert-Full is a
    /// configuration bug, not a runtime condition. Drained on
    /// `CancelAll`, the explicit cancel-all-my-pending-calls pattern.
    park_handles: tina::PendingCallSet<u32, WorkerPoolReply<Resource>>,
    report: Report,
}

#[tina_runtime::isolate(message = DriverMsg)]
impl Driver {
    fn handle(
        &mut self,
        msg: DriverMsg,
        _ctx: &mut Context<'_, SingleShard, Self::Reply>,
    ) -> Effect<Self> {
        match msg {
            DriverMsg::BeginPrime => {
                acquire_result_effect(self.pool, CALL_TIMEOUT, move |result| {
                    DriverMsg::AcquireReturned { wave: Wave::Prime, result }
                })
            }
            DriverMsg::BeginWaiters => {
                let mut effects = Vec::with_capacity(WAITERS);
                for idx in 0..WAITERS {
                    let key = idx as u32;
                    let (effect, handle) =
                        acquire_with_handle_effect(self.pool, CALL_TIMEOUT, move |outcome| {
                            DriverMsg::AcquireReturned {
                                wave: Wave::Park,
                                result: try_acquired(outcome),
                            }
                        });
                    self.park_handles
                        .insert(key, handle)
                        .expect("park-handle table sized to WAITERS");
                    effects.push(effect);
                }
                batch(effects)
            }
            DriverMsg::AcquireReturned { wave, result } => match (wave, result) {
                (Wave::Prime, Ok(lease)) => {
                    self.held = Some(lease);
                    noop()
                }
                // Park-wave acquires should never deliver after cancel.
                // If one races through, hand the resource back to avoid
                // deadlock.
                (Wave::Park, Ok(lease)) => release_result_effect(
                    lease,
                    self.pool,
                    ReleaseDisposition::Reuse,
                    CALL_TIMEOUT,
                    |_| DriverMsg::ReleaseReturned,
                ),
                (Wave::Retry, Ok(lease)) => {
                    self.report.retried_resourced += 1;
                    release_result_effect(
                        lease,
                        self.pool,
                        ReleaseDisposition::Reuse,
                        CALL_TIMEOUT,
                        |_| DriverMsg::ReleaseReturned,
                    )
                }
                (Wave::Retry, Err(AcquireFailure::Full)) => {
                    self.report.retried_full += 1;
                    noop()
                }
                _ => noop(),
            },
            DriverMsg::CancelAll => {
                let mut effects = Vec::with_capacity(self.park_handles.len());
                // Bounded drain — same effect shape, slot table is
                // explicit so a stray park-handle could not silently
                // outlive its `(idx)` key.
                for (_idx, handle) in self.park_handles.drain() {
                    effects.push(cancel_call(handle).reply(|_| DriverMsg::CancelReturned));
                }
                batch(effects)
            }
            DriverMsg::CancelReturned => {
                // Per-cancel ack; truth lives in the pool's
                // `cancel_count` (read via PressureSnapshot).
                noop()
            }
            DriverMsg::RetryAll => {
                let mut effects = Vec::with_capacity(WAITERS);
                for _ in 0..WAITERS {
                    effects.push(acquire_result_effect(
                        self.pool,
                        CALL_TIMEOUT,
                        move |result| DriverMsg::AcquireReturned { wave: Wave::Retry, result },
                    ));
                }
                self.report.retried_admitted = WAITERS;
                batch(effects)
            }
            DriverMsg::PressureSnapshot => {
                pressure_effect(self.pool, CALL_TIMEOUT, DriverMsg::PressureReturned)
            }
            DriverMsg::PressureReturned(outcome) => {
                if let CallOutcome::Replied(WorkerPoolReply::Pressure(report)) = outcome {
                    self.report.cancelled = report.cancel_count as usize;
                }
                noop()
            }
            DriverMsg::ReleaseHeld => {
                if let Some(lease) = self.held.take() {
                    release_result_effect(
                        lease,
                        self.pool,
                        ReleaseDisposition::Reuse,
                        CALL_TIMEOUT,
                        |_| DriverMsg::ReleaseReturned,
                    )
                } else {
                    noop()
                }
            }
            DriverMsg::ReleaseReturned => noop(),
            DriverMsg::Finish => {
                self.report.exit_clean = true;
                stop_with(self.report)
            }
        }
    }
}

pub fn run() -> anyhow::Result<Report> {
    let runtime = Arc::new(ThreadedRuntime::new(
        SingleShard,
        DefaultThreadedMailboxFactory,
    ));

    let pool: WorkerPool<Resource, SingleShard> =
        WorkerPool::new(PoolConfig::new(1, WAITERS), vec![1]);
    let pool_addr = runtime
        .register_with_capacity::<_, Infallible>(pool, 64)
        .map_err(|e| anyhow::anyhow!("register pool: {e:?}"))?;

    let driver = runtime
        .register_with_capacity::<_, Infallible>(
            Driver {
                pool: pool_addr,
                held: None,
                park_handles: tina::PendingCallSet::with_capacity(WAITERS),
                report: Report::default(),
            },
            64,
        )
        .map_err(|e| anyhow::anyhow!("register driver: {e:?}"))?;

    let result = runtime
        .observe_result::<Report, _, _>(driver)
        .map_err(|e| anyhow::anyhow!("observe_result: {e:?}"))?;

    runtime
        .try_send(driver, DriverMsg::BeginPrime)
        .map_err(|e| anyhow::anyhow!("send BeginPrime: {e:?}"))?;

    // Give the prime acquire a moment to land.
    std::thread::sleep(Duration::from_millis(20));

    runtime
        .try_send(driver, DriverMsg::BeginWaiters)
        .map_err(|e| anyhow::anyhow!("send BeginWaiters: {e:?}"))?;
    std::thread::sleep(Duration::from_millis(20));

    runtime
        .try_send(driver, DriverMsg::CancelAll)
        .map_err(|e| anyhow::anyhow!("send CancelAll: {e:?}"))?;
    std::thread::sleep(Duration::from_millis(40));

    runtime
        .try_send(driver, DriverMsg::PressureSnapshot)
        .map_err(|e| anyhow::anyhow!("send PressureSnapshot: {e:?}"))?;
    std::thread::sleep(Duration::from_millis(20));

    runtime
        .try_send(driver, DriverMsg::RetryAll)
        .map_err(|e| anyhow::anyhow!("send RetryAll: {e:?}"))?;
    std::thread::sleep(Duration::from_millis(20));

    runtime
        .try_send(driver, DriverMsg::ReleaseHeld)
        .map_err(|e| anyhow::anyhow!("send ReleaseHeld: {e:?}"))?;
    std::thread::sleep(Duration::from_millis(40));

    runtime
        .try_send(driver, DriverMsg::Finish)
        .map_err(|e| anyhow::anyhow!("send Finish: {e:?}"))?;

    let report = result
        .wait(Duration::from_secs(5))
        .map_err(|e| anyhow::anyhow!("driver did not produce a report: {e:?}"))?;

    if let Ok(rt) = Arc::try_unwrap(runtime) {
        let _ = rt.shutdown();
    }

    Ok(report)
}
