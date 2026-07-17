//! Public runner proof for the graceful-shutdown specimen.
//!
//! `public_characterization` pins the public drain facts the runner must
//! preserve. `public_smoke` exercises the documented Tina path. Focused
//! observation-path tests cover late claim, timeout, type mismatch, and
//! host shutdown against a tiny stop_with isolate.

use std::convert::Infallible;
use std::sync::Arc;
use std::time::Duration;

use specimen_graceful_shutdown::{TOTAL_PLANNED_ITEMS, tina_impl};
use tina::prelude::*;
use tina_runtime::{
    DefaultThreadedMailboxFactory, ResultWaitError, ThreadedRuntime, sleep,
};

fn assert_drained(report: specimen_graceful_shutdown::Report) {
    assert!(report.signal_received, "signal must be observed");
    assert_eq!(
        report.items_remaining_in_queue_at_exit, 0,
        "every queued item must drain before exit",
    );
    assert!(
        report.items_produced > 0 && report.items_produced <= TOTAL_PLANNED_ITEMS,
        "producer should have pushed some items but stopped on signal: {report:?}",
    );
    assert_eq!(
        report.items_produced, report.items_processed,
        "every produced item must be processed",
    );
    assert!(report.exit_clean);
}

/// Pins public drain facts before/after host-result migration.
#[test]
fn public_characterization() {
    assert_drained(tina_impl::run().expect("tina side ran"));
}

/// Documented public runner path: `tina_impl::run()`.
#[test]
fn public_smoke() {
    assert_drained(tina_impl::run().expect("tina side ran"));
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct TinyReport {
    n: u32,
}

#[derive(Debug)]
enum TinyMsg {
    Go,
    Done(tina_runtime::SleepReply),
}

struct Tiny;

#[tina_runtime::isolate(message = TinyMsg)]
impl Tiny {
    fn handle(
        &mut self,
        msg: TinyMsg,
        _ctx: &mut Context<'_, SingleShard, Self::Reply>,
    ) -> Effect<Self> {
        match msg {
            TinyMsg::Go => sleep(Duration::from_millis(5)).then(TinyMsg::Done),
            TinyMsg::Done(Ok(())) => stop_with(TinyReport { n: 7 }),
            TinyMsg::Done(Err(_)) => stop_with(TinyReport { n: 0 }),
        }
    }
}

#[derive(Debug)]
enum NeverMsg {
    Hang,
    Tick(tina_runtime::SleepReply),
}

struct Never;

#[tina_runtime::isolate(message = NeverMsg)]
impl Never {
    fn handle(
        &mut self,
        msg: NeverMsg,
        _ctx: &mut Context<'_, SingleShard, Self::Reply>,
    ) -> Effect<Self> {
        match msg {
            NeverMsg::Hang => sleep(Duration::from_secs(30)).then(NeverMsg::Tick),
            NeverMsg::Tick(Ok(())) => sleep(Duration::from_secs(30)).then(NeverMsg::Tick),
            NeverMsg::Tick(Err(_)) => stop(),
        }
    }
}

#[test]
fn observation_registered_too_late() {
    let runtime = ThreadedRuntime::try_new(SingleShard, DefaultThreadedMailboxFactory)
        .expect("runtime");
    let addr = runtime
        .register_with_capacity::<_, Infallible>(Tiny, 4)
        .expect("register");
    let waiter = runtime
        .observe_result::<TinyReport, _, _>(addr)
        .expect("claim");
    runtime.try_send(addr, TinyMsg::Go).expect("kick");
    let report = waiter.wait(Duration::from_secs(2)).expect("report");
    assert_eq!(report, TinyReport { n: 7 });

    // Isolate already stopped: a late claim is rejected eagerly.
    let err = runtime
        .observe_result::<TinyReport, _, _>(addr)
        .expect_err("late claim must fail");
    assert!(
        matches!(err, ResultWaitError::AlreadyStopped),
        "expected AlreadyStopped, got {err:?}"
    );
    runtime.shutdown_report().ensure_clean().expect("clean");
}

#[test]
fn observation_type_mismatch() {
    let runtime = ThreadedRuntime::try_new(SingleShard, DefaultThreadedMailboxFactory)
        .expect("runtime");
    let addr = runtime
        .register_with_capacity::<_, Infallible>(Tiny, 4)
        .expect("register");
    // Claim with the wrong type; stop_with delivers TinyReport.
    let waiter = runtime
        .observe_result::<u64, _, _>(addr)
        .expect("claim wrong type");
    runtime.try_send(addr, TinyMsg::Go).expect("kick");
    let err = waiter.wait(Duration::from_secs(2)).expect_err("type mismatch");
    assert!(
        matches!(err, ResultWaitError::TypeMismatch),
        "expected TypeMismatch, got {err:?}"
    );
    runtime.shutdown_report().ensure_clean().expect("clean");
}

#[test]
fn observation_timeout() {
    let runtime = ThreadedRuntime::try_new(SingleShard, DefaultThreadedMailboxFactory)
        .expect("runtime");
    let addr = runtime
        .register_with_capacity::<_, Infallible>(Never, 4)
        .expect("register");
    let waiter = runtime
        .observe_result::<TinyReport, _, _>(addr)
        .expect("claim");
    runtime.try_send(addr, NeverMsg::Hang).expect("kick");
    let err = waiter
        .wait(Duration::from_millis(20))
        .expect_err("must time out");
    assert!(
        matches!(err, ResultWaitError::Timeout),
        "expected Timeout, got {err:?}"
    );
    // Host shuts down while the isolate still holds a timer; report need not
    // be clean of cancelled work, but shutdown must complete.
    let _ = Arc::new(runtime).shutdown_handle().request_and_wait_report(Duration::from_secs(2));
}

#[test]
fn observation_host_shutdown() {
    let runtime = Arc::new(
        ThreadedRuntime::try_new(SingleShard, DefaultThreadedMailboxFactory).expect("runtime"),
    );
    let shutdown = runtime.shutdown_handle();
    let addr = runtime
        .register_with_capacity::<_, Infallible>(Never, 4)
        .expect("register");
    let waiter = runtime
        .observe_result::<TinyReport, _, _>(addr)
        .expect("claim");
    runtime.try_send(addr, NeverMsg::Hang).expect("kick");

    // Tear the runtime down while the waiter is outstanding.
    let report = shutdown
        .request_and_wait_report(Duration::from_secs(2))
        .expect("shutdown");
    drop(runtime);
    let _ = report; // cancelled work may leave non-clean counters

    let err = waiter
        .wait(Duration::from_millis(50))
        .expect_err("waiter must fail after host shutdown");
    assert!(
        matches!(
            err,
            ResultWaitError::RuntimeStopped | ResultWaitError::Timeout | ResultWaitError::StoppedWithoutResult
        ),
        "expected runtime-stop shaped error, got {err:?}"
    );
}
