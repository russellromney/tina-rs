//! Tina side. Three isolates — A, B, C — each is its own service.
//!
//! - C does the slow work via `sleep(work).then(Done)`. When `Done`
//!   fires it returns `reply(())`, completing the `IsolateCall` that
//!   B made.
//! - B receives the request from A. The request carries a
//!   [`Deadline`] (runtime/sim-honest absolute time, anchored at
//!   `Context::now()` upstream), so B's call to C uses
//!   `deadline.remaining_or_zero(ctx.now())` — the budget shrinks by
//!   whatever time B's hop spent. B translates C's outcome
//!   (`CallOutcome::Replied / Timeout / ...`) into a typed `BReply`
//!   so A can name where the timeout was observed.
//! - A walks the script. For each iteration it builds a `Deadline`
//!   from `Context::now() + TOTAL_DEADLINE` and forwards it to B.
//!   The reply tells A whether the chain succeeded or which hop ran
//!   out of budget.
//!
//! The `Driver` isolate is the host's bridge: it walks the script,
//! calls A for each iteration, and accumulates a `Report` to publish
//! via `stop_with`.
//!
//! Deadline is a value, not a wish: it does not retry, does not
//! cancel work, and is read against an explicit `now`. Replay-claimed
//! tests see deterministic deadlines because the simulator stamps
//! `Context::now()` from its virtual clock anchor.

use std::convert::Infallible;
use std::sync::Arc;
use std::time::Duration;

use tina::prelude::*;
use tina_runtime::{
    CallOutcome, DefaultThreadedMailboxFactory, SleepReply, ThreadedRuntime, call, sleep,
};

use crate::{FAST_C_MS, REQUEST_COUNT, Report, SLOW_C_MS, TOTAL_DEADLINE_MS, c_is_slow};

// ---------- Service C: does the slow work ----------

#[derive(Debug, Clone)]
enum CMsg {
    Compute { iteration: u32 },
    Done(SleepReply),
}

struct ServiceC;

#[tina_runtime::isolate(message = CMsg)]
impl ServiceC {
    fn handle(&mut self, msg: CMsg, _ctx: &mut Context<'_, SingleShard>) -> Effect<Self> {
        match msg {
            CMsg::Compute { iteration } => {
                let work = if c_is_slow(iteration) {
                    Duration::from_millis(SLOW_C_MS)
                } else {
                    Duration::from_millis(FAST_C_MS)
                };
                sleep(work).then(CMsg::Done)
            }
            // The sleep is plain time; pattern-match on the canonical
            // reply alias so a cancelled timer (runtime shutdown)
            // does not leak as a successful "C completed" response.
            CMsg::Done(Ok(())) => reply(()),
            CMsg::Done(Err(_)) => stop(),
        }
    }
}

// ---------- Service B: forwards to C with remaining budget ----------

#[derive(Debug, Clone)]
enum BMsg {
    Forward {
        iteration: u32,
        deadline: Deadline,
    },
    CDone(CallOutcome<()>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BReply {
    Ok,
    CTimedOut,
    Error,
}

struct ServiceB {
    c_addr: Address<CMsg, ()>,
}

#[tina_runtime::isolate(message = BMsg, reply = BReply)]
impl ServiceB {
    fn handle(&mut self, msg: BMsg, ctx: &mut Context<'_, SingleShard>) -> Effect<Self> {
        match msg {
            BMsg::Forward { iteration, deadline } => {
                // Read the remaining budget against B's own `now`.
                // Whatever time A's hop spent already shrank the
                // deadline; the call to C waits no longer than what
                // is left. Expired deadline -> `Duration::ZERO`, which
                // surfaces as `CallOutcome::Timeout`.
                let timeout = deadline.remaining_or_zero(ctx.now());
                call(self.c_addr, CMsg::Compute { iteration }, timeout).then(BMsg::CDone)
            }
            BMsg::CDone(outcome) => match outcome {
                CallOutcome::Replied(()) => reply(BReply::Ok),
                CallOutcome::Timeout => reply(BReply::CTimedOut),
                CallOutcome::Full | CallOutcome::Closed => reply(BReply::Error),
            },
        }
    }
}

// ---------- Service A: entry point ----------

#[derive(Debug, Clone)]
enum AMsg {
    Submit { iteration: u32 },
    BDone(CallOutcome<BReply>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AReply {
    Success,
    CTimedOut,
    ATimedOut,
    Error,
}

struct ServiceA {
    b_addr: Address<BMsg, BReply>,
    budget: Duration,
}

#[tina_runtime::isolate(message = AMsg, reply = AReply)]
impl ServiceA {
    fn handle(&mut self, msg: AMsg, ctx: &mut Context<'_, SingleShard>) -> Effect<Self> {
        match msg {
            AMsg::Submit { iteration } => {
                // Anchor the deadline at A's `now`. Downstream hops
                // read it against their own `now`, so the budget
                // shrinks as it travels.
                let deadline = ctx.deadline_after(self.budget);
                call(
                    self.b_addr,
                    BMsg::Forward { iteration, deadline },
                    // A's outer timeout is generous: it gives B room
                    // to surface a typed `CTimedOut` reply *after* B's
                    // own `call(C, ..., remaining_or_zero)` fires. If
                    // A's outer timeout raced B's downstream call
                    // exactly, A would observe `CallOutcome::Timeout`
                    // first and lose the per-hop attribution this
                    // specimen exists to teach.
                    self.budget + Duration::from_millis(50),
                )
                .then(AMsg::BDone)
            }
            AMsg::BDone(outcome) => match outcome {
                CallOutcome::Replied(BReply::Ok) => reply(AReply::Success),
                CallOutcome::Replied(BReply::CTimedOut) => reply(AReply::CTimedOut),
                CallOutcome::Replied(BReply::Error) => reply(AReply::Error),
                CallOutcome::Timeout => reply(AReply::ATimedOut),
                CallOutcome::Full | CallOutcome::Closed => reply(AReply::Error),
            },
        }
    }
}

// ---------- Driver: walks the script ----------

#[derive(Debug, Clone)]
enum DriverMsg {
    Begin,
    ADone(CallOutcome<AReply>),
}

struct Driver {
    a_addr: Address<AMsg, AReply>,
    next_iteration: u32,
    report: Report,
    deadline: Duration,
}

#[tina_runtime::isolate(message = DriverMsg)]
impl Driver {
    fn handle(&mut self, msg: DriverMsg, _ctx: &mut Context<'_, SingleShard>) -> Effect<Self> {
        match msg {
            DriverMsg::Begin => self.next_step(),
            DriverMsg::ADone(outcome) => {
                match outcome {
                    CallOutcome::Replied(AReply::Success) => self.report.successful += 1,
                    CallOutcome::Replied(AReply::CTimedOut) => self.report.c_timed_out += 1,
                    CallOutcome::Replied(AReply::ATimedOut) => self.report.chain_dropped += 1,
                    CallOutcome::Replied(AReply::Error) => self.report.chain_dropped += 1,
                    CallOutcome::Timeout
                    | CallOutcome::Full
                    | CallOutcome::Closed => self.report.chain_dropped += 1,
                }
                self.next_iteration += 1;
                self.next_step()
            }
        }
    }
}

impl Driver {
    fn next_step(&mut self) -> Effect<Self> {
        if self.next_iteration >= REQUEST_COUNT {
            self.report.exit_clean = true;
            return stop_with(self.report);
        }
        // The driver's call timeout is generous: A's own internal
        // deadline does the meaningful gating, and we want to observe
        // A's reply (CTimedOut / Success) rather than have the driver
        // race against it.
        call(
            self.a_addr,
            AMsg::Submit {
                iteration: self.next_iteration,
            },
            self.deadline + Duration::from_millis(50),
        )
        .then(DriverMsg::ADone)
    }
}

pub fn run() -> anyhow::Result<Report> {
    let runtime = Arc::new(ThreadedRuntime::new(
        SingleShard,
        DefaultThreadedMailboxFactory,
    ));

    let c_addr = runtime
        .register_with_capacity::<_, Infallible>(ServiceC, 8)
        .map_err(|e| anyhow::anyhow!("register C: {e:?}"))?;
    let b_addr = runtime
        .register_with_capacity::<_, Infallible>(ServiceB { c_addr }, 8)
        .map_err(|e| anyhow::anyhow!("register B: {e:?}"))?;
    let a_addr = runtime
        .register_with_capacity::<_, Infallible>(
            ServiceA {
                b_addr,
                budget: Duration::from_millis(TOTAL_DEADLINE_MS),
            },
            8,
        )
        .map_err(|e| anyhow::anyhow!("register A: {e:?}"))?;
    let driver_addr = runtime
        .register_with_capacity::<_, Infallible>(
            Driver {
                a_addr,
                next_iteration: 0,
                report: Report::default(),
                deadline: Duration::from_millis(TOTAL_DEADLINE_MS),
            },
            8,
        )
        .map_err(|e| anyhow::anyhow!("register driver: {e:?}"))?;

    let waiter = runtime
        .observe_result::<Report, _, _>(driver_addr)
        .map_err(|e| anyhow::anyhow!("observe_result: {e:?}"))?;

    runtime
        .try_send(driver_addr, DriverMsg::Begin)
        .map_err(|e| anyhow::anyhow!("send Begin: {e:?}"))?;

    let report = waiter
        .wait(Duration::from_secs(5))
        .map_err(|e| anyhow::anyhow!("driver did not finish: {e:?}"))?;

    if let Ok(rt) = Arc::try_unwrap(runtime) {
        let _ = rt.shutdown();
    }
    Ok(report)
}
