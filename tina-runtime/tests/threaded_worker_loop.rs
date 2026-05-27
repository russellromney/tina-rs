//! Worker-loop progress properties (phase 145).
//!
//! These pin the behavior the hot-path fix is responsible for:
//!
//! - immediate local calls do not pay a fixed per-turn sleep tax;
//! - a pending timer parks the worker instead of hot-spinning a core;
//! - shutdown is still observed promptly under a continuous local workload.

use std::convert::Infallible;
use std::time::{Duration, Instant};

use tina::prelude::*;
use tina::{RequestContext, reply_to_request};
use tina_runtime::{
    CallOutcome, DefaultThreadedMailboxFactory, RuntimeCall, ThreadedRuntime, ThreadedRuntimeConfig,
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

// Deferred-reply isolate: the call parks on a runtime timer before replying.
// While the timer is pending the worker has no deliverable work, so it must
// park on the command queue, not spin.
#[derive(Debug)]
enum SleeperMsg {
    Start,
    Wake(RequestContext<u32>),
}

struct Sleeper {
    nap: Duration,
}

#[tina_runtime::isolate(
    message = SleeperMsg,
    reply = u32,
    call = RuntimeCall<SleeperMsg>,
    shard = TestShard
)]
impl Sleeper {
    fn handle(&mut self, msg: SleeperMsg, _ctx: &mut Context<'_, TestShard, u32>) -> Effect<Self> {
        match msg {
            SleeperMsg::Start => noop(),
            SleeperMsg::Wake(req) => reply_to_request(req, 1),
        }
    }

    fn handle_call(&mut self, msg: SleeperMsg, call: CallContext<'_, Self>) -> Effect<Self> {
        match msg {
            SleeperMsg::Start => {
                let req = call.into_request_context();
                tina_runtime::sleep(self.nap).then(move |_| SleeperMsg::Wake(req))
            }
            SleeperMsg::Wake(_) => call.reject(tina::CallRejectedReason::UnsupportedMessage),
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

/// Sum of user + system CPU time charged to the whole process so far. During a
/// window where only the shard worker can run, the delta is the worker's CPU
/// cost: a parked worker spends almost none, a spinning worker spends ~wall.
fn process_cpu_time() -> Duration {
    // SAFETY: `getrusage` fills a caller-owned `rusage`; zero-init is valid.
    let mut usage: libc::rusage = unsafe { std::mem::zeroed() };
    let rc = unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut usage) };
    assert_eq!(rc, 0, "getrusage(RUSAGE_SELF) failed");
    let user = Duration::new(
        usage.ru_utime.tv_sec as u64,
        (usage.ru_utime.tv_usec as u32) * 1000,
    );
    let system = Duration::new(
        usage.ru_stime.tv_sec as u64,
        (usage.ru_stime.tv_usec as u32) * 1000,
    );
    user + system
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
fn pending_timer_does_not_hot_spin_the_worker() {
    let nap = Duration::from_millis(300);
    let runtime = ThreadedRuntime::with_config(
        TestShard,
        DefaultThreadedMailboxFactory,
        ThreadedRuntimeConfig {
            command_capacity: 128,
            idle_wait: Duration::from_millis(1),
            ..Default::default()
        },
    );
    let sleeper = runtime
        .register_with_capacity::<Sleeper, Infallible>(Sleeper { nap }, 4)
        .expect("register sleeper");

    let cpu_before = process_cpu_time();
    let wall_before = Instant::now();
    // Host wait budget comfortably above the nap so the call returns Replied,
    // not a host timeout. The worker spends the nap waiting on a runtime timer
    // with nothing to deliver.
    let outcome = runtime
        .call_blocking(sleeper, SleeperMsg::Start, nap + Duration::from_secs(1))
        .expect("call");
    let wall = wall_before.elapsed();
    let cpu = process_cpu_time().saturating_sub(cpu_before);

    assert_eq!(outcome, CallOutcome::Replied(1));
    assert!(
        wall >= nap,
        "call should have waited out the timer, waited {wall:?}"
    );
    // A spinning worker burns ~one core for the whole nap (cpu ~= wall). A
    // parked worker wakes ~once per idle_wait to re-poll and otherwise sleeps,
    // so its CPU is a small fraction of wall time.
    assert!(
        cpu < wall / 3,
        "worker burned {cpu:?} CPU over {wall:?} wall while a timer was pending; looks like a hot spin"
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
