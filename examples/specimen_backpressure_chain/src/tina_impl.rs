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
    CallOutcome, DefaultThreadedMailboxFactory, SleepReply, ThreadedRuntime, call_request, sleep,
};

use crate::{FAST_C_MS, REQUEST_COUNT, Report, SLOW_C_MS, TOTAL_DEADLINE_MS, c_is_slow};

// ---------- Service C: does the slow work ----------

/// Caller-authority request: the only thing an outside caller can ask.
#[derive(Debug)]
enum CRequest {
    Compute { iteration: u32 },
}

/// Internal event: the sleep continuation for a call in flight.
#[derive(Debug)]
enum CEvent {
    Done(RequestContext<()>, SleepReply),
}

struct ServiceC;

#[tina_runtime::isolate(event = CEvent, request = CRequest, reply = ())]
impl ServiceC {
    fn handle_event(
        &mut self,
        event: CEvent,
        _ctx: &mut Context<'_, SingleShard, Self::Reply>,
    ) -> Effect<Self> {
        match event {
            CEvent::Done(req, Ok(())) => reply_to(req, ()),
            CEvent::Done(req, Err(_)) => reply_to(req, ()),
        }
    }

    fn handle_request(
        &mut self,
        request: CRequest,
        call: RequestCall<'_, Self>,
    ) -> RequestEffect<Self> {
        match request {
            CRequest::Compute { iteration } => {
                let work = if c_is_slow(iteration) {
                    Duration::from_millis(SLOW_C_MS)
                } else {
                    Duration::from_millis(FAST_C_MS)
                };
                call.defer(sleep(work)).reply_service_event(CEvent::Done)
            }
        }
    }
}

// ---------- Service B: forwards to C with remaining budget ----------
//
// Split-service form. `RequestCall::now()` (findings-ledger #36) reads B's
// clock for the deadline math while `call` still holds caller authority,
// before `.defer(...)` consumes it.

/// Caller-authority request: the only thing an outside caller can ask.
#[derive(Debug)]
enum BRequest {
    Forward { iteration: u32, deadline: Deadline },
}

/// Internal event: the call-to-C continuation for a request in flight.
#[derive(Debug)]
enum BEvent {
    CDone(RequestContext<BReply>, CallOutcome<()>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BReply {
    Ok,
    CTimedOut,
    Error,
}

struct ServiceB {
    c_addr: tina::ServiceRequestAddress<CEvent, CRequest, ()>,
}

#[tina_runtime::isolate(event = BEvent, request = BRequest, reply = BReply)]
impl ServiceB {
    fn handle_event(
        &mut self,
        event: BEvent,
        _ctx: &mut Context<'_, SingleShard, Self::Reply>,
    ) -> Effect<Self> {
        match event {
            BEvent::CDone(req, outcome) => match outcome {
                CallOutcome::Replied(()) => reply_to(req, BReply::Ok),
                CallOutcome::Timeout => reply_to(req, BReply::CTimedOut),
                CallOutcome::Full | CallOutcome::Closed | CallOutcome::Rejected(_) => {
                    reply_to(req, BReply::Error)
                }
            },
        }
    }

    fn handle_request(
        &mut self,
        request: BRequest,
        call: RequestCall<'_, Self>,
    ) -> RequestEffect<Self> {
        match request {
            BRequest::Forward {
                iteration,
                deadline,
            } => {
                // Read the remaining budget against B's own `now`.
                // Whatever time A's hop spent already shrank the
                // deadline; the call to C waits no longer than what
                // is left. Expired deadline -> `Duration::ZERO`, which
                // surfaces as `CallOutcome::Timeout`.
                let timeout = deadline.remaining_or_zero(call.now());
                call.defer(call_request(
                    self.c_addr,
                    CRequest::Compute { iteration },
                    timeout,
                ))
                .reply_service_event(BEvent::CDone)
            }
        }
    }
}

// ---------- Service A: entry point ----------
//
// Same split-service shape as B: `call.now()` anchors the deadline before
// `.defer(...)` hands the caller's authority to the call-to-B continuation.

/// Caller-authority request: the only thing an outside caller can ask.
#[derive(Debug)]
enum ARequest {
    Submit { iteration: u32 },
}

/// Internal event: the call-to-B continuation for a request in flight.
#[derive(Debug)]
enum AEvent {
    BDone(RequestContext<AReply>, CallOutcome<BReply>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AReply {
    Success,
    CTimedOut,
    ATimedOut,
    Error,
}

struct ServiceA {
    b_addr: tina::ServiceRequestAddress<BEvent, BRequest, BReply>,
    budget: Duration,
}

#[tina_runtime::isolate(event = AEvent, request = ARequest, reply = AReply)]
impl ServiceA {
    fn handle_event(
        &mut self,
        event: AEvent,
        _ctx: &mut Context<'_, SingleShard, Self::Reply>,
    ) -> Effect<Self> {
        match event {
            AEvent::BDone(req, outcome) => match outcome {
                CallOutcome::Replied(BReply::Ok) => reply_to(req, AReply::Success),
                CallOutcome::Replied(BReply::CTimedOut) => reply_to(req, AReply::CTimedOut),
                CallOutcome::Replied(BReply::Error) => reply_to(req, AReply::Error),
                CallOutcome::Timeout => reply_to(req, AReply::ATimedOut),
                CallOutcome::Full | CallOutcome::Closed | CallOutcome::Rejected(_) => {
                    reply_to(req, AReply::Error)
                }
            },
        }
    }

    fn handle_request(
        &mut self,
        request: ARequest,
        call: RequestCall<'_, Self>,
    ) -> RequestEffect<Self> {
        match request {
            ARequest::Submit { iteration } => {
                // Anchor the deadline at A's `now`. Downstream hops
                // read it against their own `now`, so the budget
                // shrinks as it travels.
                let deadline = Deadline::from_instant(call.now(), self.budget);
                call.defer(call_request(
                    self.b_addr,
                    BRequest::Forward {
                        iteration,
                        deadline,
                    },
                    // A's outer timeout is generous: it gives B room
                    // to surface a typed `CTimedOut` reply *after* B's
                    // own `call(C, ..., remaining_or_zero)` fires. If
                    // A's outer timeout raced B's downstream call
                    // exactly, A would observe `CallOutcome::Timeout`
                    // first and lose the per-hop attribution this
                    // specimen exists to teach.
                    self.budget + Duration::from_millis(50),
                ))
                .reply_service_event(AEvent::BDone)
            }
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
    a_addr: tina::ServiceRequestAddress<AEvent, ARequest, AReply>,
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
                    | CallOutcome::Closed
                    | CallOutcome::Rejected(_) => self.report.chain_dropped += 1,
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
        call_request(
            self.a_addr,
            ARequest::Submit {
                iteration: self.next_iteration,
            },
            self.deadline + Duration::from_millis(50),
        )
        .then(DriverMsg::ADone)
    }
}

pub fn run() -> anyhow::Result<Report> {
    let runtime = Arc::new(ThreadedRuntime::try_new(
        SingleShard,
        DefaultThreadedMailboxFactory,
    )?);
    let shutdown = runtime.shutdown_handle();

    let c_addr = runtime
        .register_split_service::<ServiceC, CEvent, CRequest, Infallible>(ServiceC, 8)
        .map_err(|e| anyhow::anyhow!("register C: {e:?}"))?
        .requests;
    let b_addr = runtime
        .register_split_service::<ServiceB, BEvent, BRequest, Infallible>(ServiceB { c_addr }, 8)
        .map_err(|e| anyhow::anyhow!("register B: {e:?}"))?
        .requests;
    let a_addr = runtime
        .register_split_service::<ServiceA, AEvent, ARequest, Infallible>(
            ServiceA {
                b_addr,
                budget: Duration::from_millis(TOTAL_DEADLINE_MS),
            },
            8,
        )
        .map_err(|e| anyhow::anyhow!("register A: {e:?}"))?
        .requests;
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

    let terminal = shutdown.request_and_wait_report(Duration::from_secs(5))?;
    drop(runtime);
    terminal.ensure_clean()?;
    Ok(report)
}
