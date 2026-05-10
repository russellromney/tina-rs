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
use tina_runtime::{DefaultThreadedMailboxFactory, SleepReply, ThreadedRuntime, sleep};

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
                sleep(Duration::from_millis(150)).reply(CheckerMsg::SecondTick)
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
