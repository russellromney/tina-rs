//! Tina side. The driver uses `call_cancelable` to retain a
//! `CallHandle` per parked waiter — stored in a bounded
//! [`PendingCallSet`] keyed by waiter index — fires `cancel_call` on
//! each, and confirms via the pool's `PressureReport` that the cancels
//! were reclaimed before the retry wave runs.

use std::convert::Infallible;
use std::time::Duration;

use tina::pool::{AcquireFailure, PoolConfig, PoolLease, ReleaseDisposition, ReleaseFailure};
use tina::prelude::*;
use tina_runtime::pool::{
    WorkerPool, WorkerPoolMsg, WorkerPoolReply, acquire_result_effect, acquire_with_handle_effect,
    pressure_effect, release_result_effect, try_acquired,
};
use tina_runtime::{
    BoundedItems, CallOutcome, DefaultThreadedMailboxFactory, LocalSystem, SleepReply,
    bounded_batch, cancel_call, sleep,
};

use crate::{PressureTerminal, Report, WAITERS};

const CALL_TIMEOUT: Duration = Duration::from_secs(5);
const PARK_SETTLE: Duration = Duration::from_millis(20);
const RETRY_SETTLE: Duration = Duration::from_millis(20);

type Resource = u32;

#[derive(Debug)]
enum DriverMsg {
    BeginPrime,
    AcquireReturned {
        wave: Wave,
        result: Result<PoolLease<Resource>, AcquireFailure>,
    },
    CancelAll(SleepReply),
    CancelReturned(CancelOutcome),
    PressureReturned(CallOutcome<WorkerPoolReply<Resource>>),
    ReleaseHeld(SleepReply),
    ReleaseReturned(Result<(), ReleaseFailure>),
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
    release_pending: usize,
    retry_returned: usize,
    pressure_requested: bool,
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
                    DriverMsg::AcquireReturned {
                        wave: Wave::Prime,
                        result,
                    }
                })
            }
            DriverMsg::AcquireReturned { wave, result } => match (wave, result) {
                (Wave::Prime, Ok(lease)) => {
                    self.held = Some(lease);
                    self.begin_waiters()
                }
                // Park-wave acquires should never deliver after cancel.
                // If one races through, hand the resource back to avoid
                // deadlock.
                (Wave::Park, Ok(lease)) => release_result_effect(
                    {
                        self.release_pending += 1;
                        lease
                    },
                    self.pool,
                    ReleaseDisposition::Reuse,
                    CALL_TIMEOUT,
                    DriverMsg::ReleaseReturned,
                ),
                (Wave::Retry, Ok(lease)) => {
                    self.retry_returned += 1;
                    self.report.retried_resourced += 1;
                    self.release_pending += 1;
                    release_result_effect(
                        lease,
                        self.pool,
                        ReleaseDisposition::Reuse,
                        CALL_TIMEOUT,
                        DriverMsg::ReleaseReturned,
                    )
                }
                (Wave::Retry, Err(AcquireFailure::Full)) => {
                    self.retry_returned += 1;
                    self.report.retried_full += 1;
                    self.report.retry_failures.push(AcquireFailure::Full);
                    self.maybe_finish()
                }
                (wave, Err(failure)) => {
                    match wave {
                        Wave::Prime => {
                            self.report.prime_failures.push(failure);
                            return stop_with(self.report.clone());
                        }
                        Wave::Park => self.report.park_failures.push(failure),
                        Wave::Retry => {
                            self.retry_returned += 1;
                            self.report.retry_failures.push(failure);
                        }
                    }
                    self.maybe_finish()
                }
            },
            DriverMsg::CancelAll(result) => {
                if let Err(error) = result {
                    self.report.control_timer_failures.push(error);
                }
                self.cancel_all()
            }
            DriverMsg::CancelReturned(outcome) => {
                self.report.cancel_outcomes.push(outcome);
                if self.report.cancel_outcomes.len() == WAITERS && !self.pressure_requested {
                    self.pressure_requested = true;
                    pressure_effect(self.pool, CALL_TIMEOUT, DriverMsg::PressureReturned)
                } else {
                    noop()
                }
            }
            DriverMsg::PressureReturned(outcome) => {
                self.report.pressure_settled = true;
                match outcome {
                    CallOutcome::Replied(WorkerPoolReply::Pressure(report)) => {
                        self.report.cancelled = report.cancel_count as usize;
                        self.report.waiters_high_water = report.high_water_waiters;
                        self.report.waiters_max = report.max_waiters;
                        let surface = report.to_waiters_capacity_report(
                            "pool.demo.waiters",
                            tina::capacity::CapacityMode::Tuning,
                        );
                        self.report.discovery_line = tina_runtime::format_discovery_line(&surface);
                    }
                    CallOutcome::Replied(_) => {
                        self.report.pressure_terminal = Some(PressureTerminal::WrongReply)
                    }
                    CallOutcome::Full => {
                        self.report.pressure_terminal = Some(PressureTerminal::Full)
                    }
                    CallOutcome::Closed => {
                        self.report.pressure_terminal = Some(PressureTerminal::Closed)
                    }
                    CallOutcome::Timeout => {
                        self.report.pressure_terminal = Some(PressureTerminal::Timeout)
                    }
                    CallOutcome::Rejected(reason) => {
                        self.report.pressure_terminal = Some(PressureTerminal::Rejected(reason))
                    }
                }
                self.begin_retries()
            }
            DriverMsg::ReleaseHeld(result) => {
                if let Err(error) = result {
                    self.report.control_timer_failures.push(error);
                }
                self.release_held()
            }
            DriverMsg::ReleaseReturned(result) => {
                self.release_pending = self
                    .release_pending
                    .checked_sub(1)
                    .expect("every release callback has one outstanding release");
                if let Err(failure) = result {
                    self.report.release_failures.push(failure);
                }
                self.maybe_finish()
            }
        }
    }
}

impl Driver {
    fn begin_waiters(&mut self) -> Effect<Self> {
        let actions = BoundedItems::try_from_iter(WAITERS + 1, 0..=WAITERS)
            .expect("WAITERS plus one control timer is the fixed producer bound");
        bounded_batch(actions.map_effects(|idx| {
            if idx < WAITERS {
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
                effect
            } else {
                sleep(PARK_SETTLE).then(DriverMsg::CancelAll)
            }
        }))
    }

    fn cancel_all(&mut self) -> Effect<Self> {
        let handles = BoundedItems::try_from_iter(WAITERS, self.park_handles.drain())
            .expect("PendingCallSet enforces the cancel fanout bound");
        bounded_batch(
            handles
                .map_effects(|(_idx, handle)| cancel_call(handle).then(DriverMsg::CancelReturned)),
        )
    }

    fn begin_retries(&mut self) -> Effect<Self> {
        let actions = BoundedItems::try_from_iter(WAITERS + 1, 0..=WAITERS)
            .expect("WAITERS plus one control timer is the fixed producer bound");
        self.report.retried_dispatched = WAITERS;
        bounded_batch(actions.map_effects(|idx| {
            if idx < WAITERS {
                acquire_result_effect(self.pool, CALL_TIMEOUT, move |result| {
                    DriverMsg::AcquireReturned {
                        wave: Wave::Retry,
                        result,
                    }
                })
            } else {
                sleep(RETRY_SETTLE).then(DriverMsg::ReleaseHeld)
            }
        }))
    }

    fn release_held(&mut self) -> Effect<Self> {
        if let Some(lease) = self.held.take() {
            self.release_pending += 1;
            release_result_effect(
                lease,
                self.pool,
                ReleaseDisposition::Reuse,
                CALL_TIMEOUT,
                DriverMsg::ReleaseReturned,
            )
        } else {
            self.maybe_finish()
        }
    }

    fn maybe_finish(&mut self) -> Effect<Self> {
        if self.report.cancel_outcomes.len() == WAITERS
            && self.report.pressure_settled
            && self.retry_returned == WAITERS
            && self.release_pending == 0
            && self.park_handles.is_empty()
            && self.held.is_none()
        {
            self.report.exit_clean = true;
            stop_with(self.report.clone())
        } else {
            noop()
        }
    }
}

pub fn run() -> anyhow::Result<Report> {
    let app = LocalSystem::single_shard(SingleShard, DefaultThreadedMailboxFactory).try_build()?;
    Ok(app.run_to_shutdown_reported(Duration::from_secs(5), run_application)?)
}

fn run_application(
    app: &LocalSystem<SingleShard, DefaultThreadedMailboxFactory>,
) -> anyhow::Result<Report> {
    let pool: WorkerPool<Resource, SingleShard> =
        WorkerPool::new(PoolConfig::new(1, WAITERS), vec![1]);
    let pool_addr = app
        .register_root::<_, Infallible>(pool, 64)
        .map_err(|e| anyhow::anyhow!("register pool: {e:?}"))?;

    let driver = app
        .register_root::<_, Infallible>(
            Driver {
                pool: pool_addr,
                held: None,
                park_handles: tina::PendingCallSet::with_capacity(WAITERS),
                report: Report::default(),
                release_pending: 0,
                retry_returned: 0,
                pressure_requested: false,
            },
            64,
        )
        .map_err(|e| anyhow::anyhow!("register driver: {e:?}"))?;

    let result = app
        .observe_result::<Report, _, _>(driver)
        .map_err(|e| anyhow::anyhow!("observe_result: {e:?}"))?;

    app.try_send(driver, DriverMsg::BeginPrime)
        .map_err(|e| anyhow::anyhow!("send BeginPrime: {e:?}"))?;

    result
        .wait(Duration::from_secs(5))
        .map_err(|e| anyhow::anyhow!("driver did not produce a report: {e:?}"))
}
