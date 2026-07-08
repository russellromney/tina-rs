//! Worker-loop progress properties.
//!
//! These pin the behavior the hot-path fix is responsible for:
//!
//! - immediate local calls do not pay a fixed per-turn sleep tax;
//! - shutdown is still observed promptly under a continuous local workload.
//!
//! (There is no separate "no hot-spin" test: the worker cannot busy-spin on a
//! pending timer or lane op because the runtime step blocks inside the
//! betelgeuse io_loop while that work is pending — verified during review by
//! holding a call with no pending I/O and measuring near-zero worker CPU.)

use std::convert::Infallible;
use std::time::{Duration, Instant};

use tina::prelude::*;
use tina_runtime::{
    CallOutcome, DefaultThreadedMailboxFactory, ThreadedRuntime, ThreadedRuntimeConfig,
};

#[derive(Debug, Clone, Copy)]
struct TestShard;

impl Shard for TestShard {
    fn id(&self) -> ShardId {
        ShardId::new(0)
    }
}

// Immediate-reply isolate: one call, one reply, no deferred work. A call to
// it needs several runtime turns (deliver the host driver's Begin, deliver the
// target message, deliver the reply back to the driver). The old loop slept
// 1ms after every one of those turns.
#[derive(Debug)]
enum EchoMsg {
    AddOne(u32),
}

struct Echo;

#[tina_runtime::isolate(message = EchoMsg, reply = u32, shard = TestShard)]
impl Echo {
    fn handle(&mut self, _msg: EchoMsg, _ctx: &mut Context<'_, TestShard, u32>) -> Effect<Self> {
        noop()
    }

    fn handle_call(&mut self, msg: EchoMsg, call: CallContext<'_, Self>) -> Effect<Self> {
        match msg {
            EchoMsg::AddOne(value) => call.reply(value + 1),
        }
    }
}

// Self-feeding isolate: every delivered message enqueues one more to itself, so
// the shard always has deliverable work and the worker never parks. This is the
// hot local workload that must not starve a shutdown command.
#[derive(Debug, Clone, Copy)]
enum BusyMsg {
    Tick,
}

struct Busy {
    me: Address<BusyMsg>,
}

#[tina_runtime::isolate(message = BusyMsg, send = Outbound<BusyMsg>, shard = TestShard)]
impl Busy {
    fn handle(
        &mut self,
        msg: BusyMsg,
        _ctx: &mut Context<'_, TestShard, Self::Reply>,
    ) -> Effect<Self> {
        match msg {
            // Re-send to self so there is always one message in flight: deliver
            // one, enqueue one, mailbox never empties. The worker keeps finding
            // deliverable work and never parks.
            BusyMsg::Tick => send(self.me, BusyMsg::Tick),
        }
    }
}

fn runtime() -> ThreadedRuntime<TestShard, DefaultThreadedMailboxFactory> {
    ThreadedRuntime::with_config(
        TestShard,
        DefaultThreadedMailboxFactory,
        ThreadedRuntimeConfig {
            command_capacity: 128,
            idle_wait: Duration::from_millis(1),
            ..Default::default()
        },
    )
}

#[test]
fn immediate_local_calls_are_not_millisecond_scale() {
    let runtime = runtime();
    let echo = runtime
        .register_with_capacity::<Echo, Infallible>(Echo, 4)
        .expect("register echo");

    // Warm the path once so registration cost is not in the timed loop.
    assert_eq!(
        runtime
            .call_blocking(echo, EchoMsg::AddOne(0), Duration::from_secs(1))
            .expect("warm call"),
        CallOutcome::Replied(1)
    );

    const CALLS: u32 = 40;
    let start = Instant::now();
    for value in 0..CALLS {
        let outcome = runtime
            .call_blocking(echo, EchoMsg::AddOne(value), Duration::from_secs(1))
            .expect("call");
        assert_eq!(outcome, CallOutcome::Replied(value + 1));
    }
    let elapsed = start.elapsed();

    // The old loop slept 1ms after every progress step, and each immediate call
    // needs at least three steps, so 40 calls had a ~120ms wall-clock floor from
    // the sleeps alone — regardless of build profile. A loop that does not tax
    // progress finishes in single-digit milliseconds in release.
    //
    // The strict proof is a release property: there the per-call work is so
    // small that the old sleep floor is unmistakable, and 100ms leaves wide
    // margin for a loaded machine. In debug the per-call work itself is in the
    // same millisecond range as the old tax, so an absolute threshold can only
    // guard against a gross hang, not prove the fix — `make perf` and release
    // CI are where this is proven.
    let ceiling = if cfg!(debug_assertions) {
        Duration::from_millis(800)
    } else {
        Duration::from_millis(100)
    };
    assert!(
        elapsed < ceiling,
        "{CALLS} immediate local calls took {elapsed:?} (ceiling {ceiling:?}); expected no fixed per-turn sleep tax"
    );

    runtime.shutdown().expect("shutdown");
}

#[test]
fn shutdown_is_prompt_under_hot_local_workload() {
    let runtime = runtime();
    let busy = runtime
        .register_with_capacity_using::<Busy, BusyMsg, _>(4, |me| Busy { me })
        .expect("register busy");

    // Kick the self-feeding cycle so the shard is continuously busy.
    runtime.try_send(busy, BusyMsg::Tick).expect("kick busy");
    // Let the worker reach steady-state hot looping.
    std::thread::sleep(Duration::from_millis(20));

    let start = Instant::now();
    let report = runtime.shutdown_report();
    let elapsed = start.elapsed();

    assert!(
        report.error().is_none(),
        "hot workload should still shut down cleanly: {:?}",
        report.error()
    );
    assert!(
        elapsed < Duration::from_secs(2),
        "shutdown under a hot local workload took {elapsed:?}; command ingress may be starved"
    );
}
