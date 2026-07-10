//! End-to-end proofs for [`tina::Deadline`] and [`tina::Context::now`]
//! on the live runtime.
//!
//! The plan's clock rule is the load-bearing constraint: a `Deadline`
//! that secretly used `std::time::Instant::now()` would be silently
//! wrong under DST/replay. These tests prove that the runtime stamps
//! `Context::now()` from its `Clock`, that handlers can build a
//! `Deadline` from that `now`, and that the budget shrinks honestly
//! across handler turns separated by real wall time.

use std::convert::Infallible;
use std::sync::Arc;
use std::time::Duration;

use tina::prelude::*;
use tina_runtime::{
    CallError, CallOutcome, DefaultThreadedMailboxFactory, SleepReply, ThreadedRuntime, dns_lookup,
    sleep,
};

#[derive(Debug, Default, Clone, Copy)]
struct Report {
    /// Difference between two `Context::now()` reads on the same
    /// isolate, with a real `sleep` in between. Must be at least the
    /// sleep duration if `now` is honest.
    delta_ms_lower_bound_ok: bool,
    /// `deadline.expired(ctx.now())` after the budget elapsed.
    expired_after_budget: bool,
    /// `deadline.remaining_or_zero(ctx.now())` is non-zero before the
    /// budget elapses.
    remaining_before_budget_ok: bool,
    exit_clean: bool,
}

#[derive(Debug)]
enum CheckerMsg {
    Begin,
    /// First wakeup; capture `now` again to bound the delta.
    SecondTick(SleepReply),
}

struct Checker {
    deadline: Option<Deadline>,
    first_now: Option<std::time::Instant>,
    report: Report,
}

#[tina_runtime::isolate(message = CheckerMsg)]
impl Checker {
    fn handle(
        &mut self,
        msg: CheckerMsg,
        ctx: &mut Context<'_, SingleShard, Self::Reply>,
    ) -> Effect<Self> {
        match msg {
            CheckerMsg::Begin => {
                let now = ctx.now();
                self.first_now = Some(now);
                // 80ms budget — long enough to be present before the
                // sleep, short enough to be expired after it.
                let deadline = ctx.deadline_after(Duration::from_millis(80));
                self.deadline = Some(deadline);
                self.report.remaining_before_budget_ok =
                    deadline.remaining_or_zero(now) >= Duration::from_millis(70);
                sleep(Duration::from_millis(150)).then(CheckerMsg::SecondTick)
            }
            CheckerMsg::SecondTick(Ok(())) => {
                let now = ctx.now();
                let first = self.first_now.expect("Begin recorded a first now");
                let delta = now.saturating_duration_since(first);
                // Real wall time must have advanced by at least the
                // sleep budget. A clock that returned a frozen value
                // would fail this.
                self.report.delta_ms_lower_bound_ok = delta >= Duration::from_millis(140);

                let deadline = self.deadline.expect("Begin built a deadline");
                self.report.expired_after_budget = deadline.expired(now);
                self.report.exit_clean = true;
                stop_with(self.report)
            }
            CheckerMsg::SecondTick(Err(_)) => stop(),
        }
    }
}

#[test]
fn context_now_is_honest_and_deadline_expires_after_budget() {
    let runtime = Arc::new(ThreadedRuntime::new(
        SingleShard,
        DefaultThreadedMailboxFactory,
    ));

    let checker = runtime
        .register_with_capacity::<_, Infallible>(
            Checker {
                deadline: None,
                first_now: None,
                report: Report::default(),
            },
            8,
        )
        .expect("register checker");

    let result = runtime
        .observe_result::<Report, _, _>(checker)
        .expect("observe_result");

    runtime
        .try_send(checker, CheckerMsg::Begin)
        .expect("send Begin");

    let report = result.wait(Duration::from_secs(5)).expect("report");

    assert!(report.exit_clean);
    assert!(
        report.remaining_before_budget_ok,
        "fresh deadline should report most of its budget remaining",
    );
    assert!(
        report.delta_ms_lower_bound_ok,
        "Context::now() must advance by at least the sleep duration",
    );
    assert!(
        report.expired_after_budget,
        "deadline should be expired against ctx.now() after the budget elapsed",
    );

    if let Ok(rt) = Arc::try_unwrap(runtime) {
        let _ = rt.shutdown();
    }
}

#[test]
fn deadline_value_type_unit_invariants() {
    use std::time::Instant;

    let now = Instant::now();
    let deadline = Deadline::from_instant(now, Duration::from_millis(500));

    assert!(!deadline.expired(now));
    assert_eq!(deadline.remaining(now), Some(Duration::from_millis(500)));
    assert_eq!(deadline.remaining_or_zero(now), Duration::from_millis(500));

    let later = now + Duration::from_millis(250);
    assert!(!deadline.expired(later));
    assert!(deadline.remaining_or_zero(later) <= Duration::from_millis(250));

    let expired = now + Duration::from_secs(1);
    assert!(deadline.expired(expired));
    assert_eq!(deadline.remaining_or_zero(expired), Duration::ZERO);
    assert!(deadline.remaining(expired).is_none());
}

/// Regression: `Duration::MAX` once made the deadline expire
/// immediately because `checked_add` returned `None` and the constructor
/// fell back to `now`. The fix saturates to ~100 years from now, which
/// is "effectively never" for any sane caller.
#[test]
fn deadline_saturates_on_overflow_instead_of_expiring_now() {
    use std::time::Instant;

    let now = Instant::now();
    let deadline = Deadline::from_instant(now, Duration::MAX);

    assert!(
        !deadline.expired(now),
        "overflowed deadline must not expire immediately at `now`",
    );
    // A whole year later is still well inside the 100-year ceiling.
    let one_year_later = now + Duration::from_secs(60 * 60 * 24 * 365);
    assert!(
        !deadline.expired(one_year_later),
        "overflowed deadline should still be live a year out",
    );
    assert!(
        deadline.remaining_or_zero(now) > Duration::from_secs(60 * 60 * 24 * 365 * 50),
        "saturated deadline should report at least 50 years remaining",
    );
}

#[derive(Debug)]
enum MaxSleepMsg {
    Park,
    Probe,
    UnexpectedWake(SleepReply),
}

#[derive(Debug, Default)]
struct MaxSleepProbe;

#[tina_runtime::isolate(message = MaxSleepMsg)]
impl MaxSleepProbe {
    fn handle(
        &mut self,
        msg: MaxSleepMsg,
        _ctx: &mut Context<'_, SingleShard, Self::Reply>,
    ) -> Effect<Self> {
        match msg {
            MaxSleepMsg::Park => sleep(Duration::MAX).then(MaxSleepMsg::UnexpectedWake),
            MaxSleepMsg::Probe => stop_with(true),
            MaxSleepMsg::UnexpectedWake(result) => {
                assert!(result.is_ok(), "maximum-duration timer was cancelled early");
                stop_with(false)
            }
        }
    }
}

#[derive(Debug)]
enum MaxDnsMsg {
    Resolve,
    Done(Result<Vec<std::net::SocketAddr>, CallError>),
}

#[derive(Debug, Default)]
struct MaxDnsProbe;

#[tina_runtime::isolate(message = MaxDnsMsg)]
impl MaxDnsProbe {
    fn handle(
        &mut self,
        msg: MaxDnsMsg,
        _ctx: &mut Context<'_, SingleShard, Self::Reply>,
    ) -> Effect<Self> {
        match msg {
            MaxDnsMsg::Resolve => dns_lookup("localhost", 80, Duration::MAX).then(MaxDnsMsg::Done),
            MaxDnsMsg::Done(result) => stop_with(result.is_ok()),
        }
    }
}

#[derive(Debug)]
enum MaxHostCallMsg {
    Echo(u32),
}

#[derive(Debug, Default)]
struct MaxHostCallProbe;

#[tina_runtime::isolate(message = MaxHostCallMsg, reply = u32)]
impl MaxHostCallProbe {
    fn handle(
        &mut self,
        _msg: MaxHostCallMsg,
        _ctx: &mut Context<'_, SingleShard, Self::Reply>,
    ) -> Effect<Self> {
        noop()
    }

    fn handle_call(&mut self, msg: MaxHostCallMsg, call: CallContext<'_, Self>) -> Effect<Self> {
        match msg {
            MaxHostCallMsg::Echo(value) => call.reply(value),
        }
    }
}

/// User-facing regression: maximum configured waits are accepted at the
/// runtime effect boundary. A parked maximum-duration timer must not panic the
/// shard or prevent an ordinary mailbox message from making progress, and DNS
/// must complete normally under the same timeout.
#[test]
fn maximum_effect_timeouts_do_not_panic_or_stall_the_live_runtime() {
    let runtime = Arc::new(ThreadedRuntime::new(
        SingleShard,
        DefaultThreadedMailboxFactory,
    ));

    let sleeper = runtime
        .register_with_capacity::<_, Infallible>(MaxSleepProbe, 4)
        .expect("register max-duration sleeper");
    let sleep_result = runtime
        .observe_result::<bool, _, _>(sleeper)
        .expect("observe sleeper");
    runtime
        .try_send(sleeper, MaxSleepMsg::Park)
        .expect("park max-duration sleeper");
    runtime
        .try_send(sleeper, MaxSleepMsg::Probe)
        .expect("probe parked sleeper");
    assert!(
        sleep_result
            .wait(Duration::MAX)
            .expect("runtime remains responsive beside max-duration timer")
    );

    let dns = runtime
        .register_with_capacity::<_, Infallible>(MaxDnsProbe, 4)
        .expect("register max-duration DNS probe");
    let dns_result = runtime
        .observe_result::<bool, _, _>(dns)
        .expect("observe DNS probe");
    runtime
        .try_send(dns, MaxDnsMsg::Resolve)
        .expect("start max-duration DNS lookup");
    assert!(
        dns_result
            .wait(Duration::MAX)
            .expect("DNS completes with max timeout")
    );

    let echo = runtime
        .register_with_capacity::<_, Infallible>(MaxHostCallProbe, 4)
        .expect("register maximum-timeout host-call probe");
    assert!(matches!(
        runtime
            .call_blocking(echo, MaxHostCallMsg::Echo(42), Duration::MAX)
            .expect("host call accepts maximum timeout"),
        CallOutcome::Replied(42)
    ));

    if let Ok(runtime) = Arc::try_unwrap(runtime) {
        runtime
            .shutdown()
            .expect("shutdown after max-duration calls");
    }
}
