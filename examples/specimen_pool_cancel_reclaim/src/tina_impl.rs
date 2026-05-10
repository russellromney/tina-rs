//! Tina side. The driver uses `call_with_handle` to retain a
//! `CallHandle` per parked waiter, fires `cancel_call` on each, and
//! confirms via the pool's `PressureReport` that the cancels were
//! reclaimed before the retry wave runs.

use std::convert::Infallible;
use std::sync::Arc;
use std::time::Duration;

use tina::pool::{PoolConfig, PoolLease, ReleaseDisposition};
use tina::prelude::*;
use tina_runtime::pool::{
    WorkerPool, WorkerPoolMsg, WorkerPoolReply, acquire_effect, acquire_with_handle_effect,
    pressure_effect, release_effect, try_acquired,
};
use tina_runtime::{CallOutcome, DefaultThreadedMailboxFactory, ThreadedRuntime, cancel_call};

use crate::{Report, WAITERS};

const CALL_TIMEOUT: Duration = Duration::from_secs(5);

type Resource = u32;

enum DriverMsg {
    BeginPrime,
    BeginWaiters,
    AcquireReturned {
        wave: Wave,
        outcome: CallOutcome<WorkerPoolReply<Resource>>,
    },
    CancelAll,
    CancelReturned(tina::CancelOutcome),
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

impl std::fmt::Debug for DriverMsg {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::BeginPrime => f.write_str("BeginPrime"),
            Self::BeginWaiters => f.write_str("BeginWaiters"),
            Self::AcquireReturned { wave, .. } => write!(f, "AcquireReturned({wave:?})"),
            Self::CancelAll => f.write_str("CancelAll"),
            Self::CancelReturned(o) => write!(f, "CancelReturned({o:?})"),
            Self::RetryAll => f.write_str("RetryAll"),
            Self::PressureSnapshot => f.write_str("PressureSnapshot"),
            Self::PressureReturned(_) => f.write_str("PressureReturned"),
            Self::ReleaseHeld => f.write_str("ReleaseHeld"),
            Self::ReleaseReturned => f.write_str("ReleaseReturned"),
            Self::Finish => f.write_str("Finish"),
        }
    }
}

struct Driver {
    pool: Address<WorkerPoolMsg<Resource>, WorkerPoolReply<Resource>>,
    held: Option<PoolLease<Resource>>,
    park_handles: Vec<tina::CallHandle<WorkerPoolReply<Resource>>>,
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
            DriverMsg::BeginPrime => acquire_effect(self.pool, CALL_TIMEOUT, move |outcome| {
                DriverMsg::AcquireReturned { wave: Wave::Prime, outcome }
            }),
            DriverMsg::BeginWaiters => {
                let mut effects = Vec::with_capacity(WAITERS);
                for _ in 0..WAITERS {
                    let (effect, handle) =
                        acquire_with_handle_effect(self.pool, CALL_TIMEOUT, move |outcome| {
                            DriverMsg::AcquireReturned { wave: Wave::Park, outcome }
                        });
                    self.park_handles.push(handle);
                    effects.push(effect);
                }
                batch(effects)
            }
            DriverMsg::AcquireReturned { wave, outcome } => match wave {
                Wave::Prime => {
                    if let Ok(lease) = try_acquired(outcome) {
                        self.held = Some(lease);
                    }
                    noop()
                }
                Wave::Park => {
                    // Cancel-reclaim flow: park-wave outcomes should
                    // never deliver. If one races through, hand the
                    // resource back so we don't deadlock.
                    if let Ok(lease) = try_acquired(outcome) {
                        return release_effect(
                            lease,
                            self.pool,
                            ReleaseDisposition::Reuse,
                            CALL_TIMEOUT,
                            |_| DriverMsg::ReleaseReturned,
                        );
                    }
                    noop()
                }
                Wave::Retry => match try_acquired(outcome) {
                    Ok(lease) => {
                        self.report.retried_resourced += 1;
                        release_effect(
                            lease,
                            self.pool,
                            ReleaseDisposition::Reuse,
                            CALL_TIMEOUT,
                            |_| DriverMsg::ReleaseReturned,
                        )
                    }
                    Err(tina::pool::AcquireFailure::Full) => {
                        self.report.retried_full += 1;
                        noop()
                    }
                    Err(_) => noop(),
                },
            },
            DriverMsg::CancelAll => {
                let mut effects = Vec::with_capacity(self.park_handles.len());
                for handle in self.park_handles.drain(..) {
                    effects.push(cancel_call(handle).reply(DriverMsg::CancelReturned));
                }
                batch(effects)
            }
            DriverMsg::CancelReturned(_) => {
                // Per-cancel ack; truth lives in the pool's
                // `cancel_count` (read via PressureSnapshot).
                noop()
            }
            DriverMsg::RetryAll => {
                let mut effects = Vec::with_capacity(WAITERS);
                for _ in 0..WAITERS {
                    effects.push(acquire_effect(self.pool, CALL_TIMEOUT, move |outcome| {
                        DriverMsg::AcquireReturned { wave: Wave::Retry, outcome }
                    }));
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
                    release_effect(
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
                park_handles: Vec::new(),
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
