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
use std::time::Duration;

use tina::prelude::*;
use tina_runtime::{
    CallOutcome, DefaultThreadedMailboxFactory, LocalSystem, SleepReply, call_request, sleep,
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
    Done(RequestContext<CReply>, SleepReply),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CReply {
    Ok,
    DomainFailure,
}

struct ServiceC;

#[tina_runtime::isolate(event = CEvent, request = CRequest, reply = CReply)]
impl ServiceC {
    fn handle_event(
        &mut self,
        event: CEvent,
        _ctx: &mut Context<'_, SingleShard, Self::Reply>,
    ) -> Effect<Self> {
        match event {
            CEvent::Done(req, Ok(())) => reply_to(req, CReply::Ok),
            CEvent::Done(req, Err(_)) => reply_to(req, CReply::DomainFailure),
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
    CDone(RequestContext<BReply>, CallOutcome<CReply>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BReply {
    Ok,
    CTimedOut,
    Full,
    Closed,
    Rejected,
    DomainFailure,
}

struct ServiceB {
    c_addr: tina::ServiceRequestAddress<CEvent, CRequest, CReply>,
}

#[tina_runtime::isolate(event = BEvent, request = BRequest, reply = BReply)]
impl ServiceB {
    fn handle_event(
        &mut self,
        event: BEvent,
        _ctx: &mut Context<'_, SingleShard, Self::Reply>,
    ) -> Effect<Self> {
        match event {
            BEvent::CDone(req, outcome) => reply_to(req, map_c_outcome(outcome)),
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

fn map_c_outcome(outcome: CallOutcome<CReply>) -> BReply {
    match outcome {
        CallOutcome::Replied(CReply::Ok) => BReply::Ok,
        CallOutcome::Replied(CReply::DomainFailure) => BReply::DomainFailure,
        CallOutcome::Timeout => BReply::CTimedOut,
        CallOutcome::Full => BReply::Full,
        CallOutcome::Closed => BReply::Closed,
        CallOutcome::Rejected(_) => BReply::Rejected,
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
    Timeout,
    Full,
    Closed,
    Rejected,
    DomainFailure,
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
            AEvent::BDone(req, outcome) => reply_to(req, map_b_outcome(outcome)),
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

fn map_b_outcome(outcome: CallOutcome<BReply>) -> AReply {
    match outcome {
        CallOutcome::Replied(BReply::Ok) => AReply::Success,
        CallOutcome::Replied(BReply::CTimedOut) => AReply::CTimedOut,
        CallOutcome::Replied(BReply::Full) | CallOutcome::Full => AReply::Full,
        CallOutcome::Replied(BReply::Closed) | CallOutcome::Closed => AReply::Closed,
        CallOutcome::Replied(BReply::Rejected) | CallOutcome::Rejected(_) => AReply::Rejected,
        CallOutcome::Replied(BReply::DomainFailure) => AReply::DomainFailure,
        CallOutcome::Timeout => AReply::Timeout,
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
                record_a_outcome(&mut self.report, outcome);
                self.next_iteration += 1;
                self.next_step()
            }
        }
    }
}

fn record_a_outcome(report: &mut Report, outcome: CallOutcome<AReply>) {
    match outcome {
        CallOutcome::Replied(AReply::Success) => report.successful += 1,
        CallOutcome::Replied(AReply::CTimedOut) => report.c_timed_out += 1,
        CallOutcome::Replied(AReply::Timeout) | CallOutcome::Timeout => report.caller_timeout += 1,
        CallOutcome::Replied(AReply::Full) | CallOutcome::Full => report.full += 1,
        CallOutcome::Replied(AReply::Closed) | CallOutcome::Closed => report.closed += 1,
        CallOutcome::Replied(AReply::Rejected) | CallOutcome::Rejected(_) => report.rejected += 1,
        CallOutcome::Replied(AReply::DomainFailure) => report.domain_failure += 1,
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
    let app = LocalSystem::single_shard(SingleShard, DefaultThreadedMailboxFactory).try_build()?;
    Ok(app.run_to_shutdown_reported(Duration::from_secs(5), run_application)?)
}

fn run_application(
    app: &LocalSystem<SingleShard, DefaultThreadedMailboxFactory>,
) -> anyhow::Result<Report> {
    let c_addr = app
        .register_split_service::<ServiceC, CEvent, CRequest, Infallible>(ServiceC, 8)
        .map_err(|e| anyhow::anyhow!("register C: {e:?}"))?
        .requests;
    let b_addr = app
        .register_split_service::<ServiceB, BEvent, BRequest, Infallible>(ServiceB { c_addr }, 8)
        .map_err(|e| anyhow::anyhow!("register B: {e:?}"))?
        .requests;
    let a_addr = app
        .register_split_service::<ServiceA, AEvent, ARequest, Infallible>(
            ServiceA {
                b_addr,
                budget: Duration::from_millis(TOTAL_DEADLINE_MS),
            },
            8,
        )
        .map_err(|e| anyhow::anyhow!("register A: {e:?}"))?
        .requests;
    let driver_addr = app
        .register_root::<_, Infallible>(
            Driver {
                a_addr,
                next_iteration: 0,
                report: Report::default(),
                deadline: Duration::from_millis(TOTAL_DEADLINE_MS),
            },
            8,
        )
        .map_err(|e| anyhow::anyhow!("register driver: {e:?}"))?;

    let waiter = app
        .observe_result::<Report, _, _>(driver_addr)
        .map_err(|e| anyhow::anyhow!("observe_result: {e:?}"))?;

    app.try_send(driver_addr, DriverMsg::Begin)
        .map_err(|e| anyhow::anyhow!("send Begin: {e:?}"))?;

    let report = waiter
        .wait(Duration::from_secs(5))
        .map_err(|e| anyhow::anyhow!("driver did not finish: {e:?}"))?;

    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tina::CallRejectedReason;

    const REJECTED: CallRejectedReason = CallRejectedReason::UnsupportedMessage;

    #[test]
    fn c_outcomes_remain_distinct_at_b() {
        assert_eq!(map_c_outcome(CallOutcome::Replied(CReply::Ok)), BReply::Ok);
        assert_eq!(
            map_c_outcome(CallOutcome::Replied(CReply::DomainFailure)),
            BReply::DomainFailure
        );
        assert_eq!(map_c_outcome(CallOutcome::Timeout), BReply::CTimedOut);
        assert_eq!(map_c_outcome(CallOutcome::Full), BReply::Full);
        assert_eq!(map_c_outcome(CallOutcome::Closed), BReply::Closed);
        assert_eq!(map_c_outcome(CallOutcome::Rejected(REJECTED)), BReply::Rejected);
    }

    #[test]
    fn b_and_runtime_outcomes_remain_distinct_at_a() {
        let cases = [
            (CallOutcome::Replied(BReply::Ok), AReply::Success),
            (CallOutcome::Replied(BReply::CTimedOut), AReply::CTimedOut),
            (CallOutcome::Replied(BReply::Full), AReply::Full),
            (CallOutcome::Replied(BReply::Closed), AReply::Closed),
            (CallOutcome::Replied(BReply::Rejected), AReply::Rejected),
            (
                CallOutcome::Replied(BReply::DomainFailure),
                AReply::DomainFailure,
            ),
            (CallOutcome::Timeout, AReply::Timeout),
            (CallOutcome::Full, AReply::Full),
            (CallOutcome::Closed, AReply::Closed),
            (CallOutcome::Rejected(REJECTED), AReply::Rejected),
        ];
        for (outcome, expected) in cases {
            assert_eq!(map_b_outcome(outcome), expected);
        }
    }

    #[test]
    fn driver_accounts_every_terminal_bucket_independently() {
        let mut report = Report::default();
        for outcome in [
            CallOutcome::Replied(AReply::Success),
            CallOutcome::Replied(AReply::CTimedOut),
            CallOutcome::Replied(AReply::Timeout),
            CallOutcome::Replied(AReply::Full),
            CallOutcome::Replied(AReply::Closed),
            CallOutcome::Replied(AReply::Rejected),
            CallOutcome::Replied(AReply::DomainFailure),
        ] {
            record_a_outcome(&mut report, outcome);
        }
        assert_eq!(
            report,
            Report {
                successful: 1,
                c_timed_out: 1,
                caller_timeout: 1,
                full: 1,
                closed: 1,
                rejected: 1,
                domain_failure: 1,
                exit_clean: false,
            }
        );

        let mut outer = Report::default();
        for outcome in [
            CallOutcome::Timeout,
            CallOutcome::Full,
            CallOutcome::Closed,
            CallOutcome::Rejected(REJECTED),
        ] {
            record_a_outcome(&mut outer, outcome);
        }
        assert_eq!(outer.caller_timeout, 1);
        assert_eq!(outer.full, 1);
        assert_eq!(outer.closed, 1);
        assert_eq!(outer.rejected, 1);
    }
}
