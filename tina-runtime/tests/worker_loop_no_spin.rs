//! Worker-loop no-hot-spin proofs (phase 145).
//!
//! These measure process CPU time across a window where a shard worker is
//! waiting on a runtime timer with nothing to deliver. A parked worker spends
//! almost no CPU; a worker that spins (the old single-shard `1ms` sleep loop or
//! the old multi-shard `yield_now` branch) burns ~one core for the whole wait.
//!
//! `getrusage(RUSAGE_SELF)` is process-wide, so these tests live in their own
//! binary (separate process from the spin-heavy `host_control_ergonomics`
//! suite) and serialize with each other through `SERIAL` so one measurement
//! never charges another test's CPU.

use std::convert::Infallible;
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant};

use tina::prelude::*;
use tina::{CallRejectedReason, RequestContext, reply_to_request};
use tina_runtime::{
    CallOutcome, DefaultThreadedMailboxFactory, RuntimeCall, ThreadedMultiShardRuntime,
    ThreadedRuntime, ThreadedRuntimeConfig, sleep,
};

/// Serializes the CPU measurements in this binary so two windows never overlap
/// (the waiting test is blocked on the lock, spending no CPU).
static SERIAL: Mutex<()> = Mutex::new(());

fn serial() -> MutexGuard<'static, ()> {
    SERIAL
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Sum of user + system CPU time charged to the whole process so far.
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

// ---- single shard ---------------------------------------------------------

#[derive(Debug)]
enum SleeperMsg {
    Start,
    Wake(RequestContext<u32>),
}

struct Sleeper {
    nap: Duration,
}

#[tina_runtime::isolate(message = SleeperMsg, reply = u32, call = RuntimeCall<SleeperMsg>)]
impl Sleeper {
    fn handle(
        &mut self,
        msg: SleeperMsg,
        _ctx: &mut Context<'_, SingleShard, u32>,
    ) -> Effect<Self> {
        match msg {
            SleeperMsg::Start => noop(),
            SleeperMsg::Wake(req) => reply_to_request(req, 1),
        }
    }

    fn handle_call(&mut self, msg: SleeperMsg, call: CallContext<'_, Self>) -> Effect<Self> {
        match msg {
            SleeperMsg::Start => {
                let req = call.into_request_context();
                sleep(self.nap).then(move |_| SleeperMsg::Wake(req))
            }
            SleeperMsg::Wake(_) => call.reject(CallRejectedReason::UnsupportedMessage),
        }
    }
}

#[test]
fn single_shard_pending_timer_does_not_hot_spin_the_worker() {
    let _guard = serial();
    let nap = Duration::from_millis(300);
    let runtime = ThreadedRuntime::with_config(
        SingleShard,
        DefaultThreadedMailboxFactory,
        ThreadedRuntimeConfig {
            command_capacity: 64,
            idle_wait: Duration::from_millis(1),
            ..Default::default()
        },
    );
    let sleeper = runtime
        .register_with_capacity::<Sleeper, Infallible>(Sleeper { nap }, 4)
        .expect("register sleeper");

    let cpu_before = process_cpu_time();
    let wall_before = Instant::now();
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
    assert!(
        cpu < wall / 3,
        "worker burned {cpu:?} CPU over {wall:?} wall while a timer was pending; looks like a hot spin"
    );

    runtime.shutdown().expect("shutdown");
}

// ---- multi shard ----------------------------------------------------------

#[derive(Debug, Clone, Copy)]
struct TestShard(u32);

impl Shard for TestShard {
    fn id(&self) -> ShardId {
        ShardId::new(self.0)
    }
}

#[derive(Debug)]
enum HeldMsg {
    Hold,
    Done(RequestContext<u32>),
}

struct HeldMS;

#[tina_runtime::isolate(message = HeldMsg, reply = u32, call = RuntimeCall<HeldMsg>, shard = TestShard)]
impl HeldMS {
    fn handle(&mut self, msg: HeldMsg, _ctx: &mut Context<'_, TestShard, u32>) -> Effect<Self> {
        match msg {
            HeldMsg::Done(req) => reply_to_request(req, 0),
            HeldMsg::Hold => noop(),
        }
    }

    fn handle_call(&mut self, msg: HeldMsg, call: CallContext<'_, Self>) -> Effect<Self> {
        match msg {
            HeldMsg::Hold => {
                let req = call.into_request_context();
                sleep(Duration::from_millis(500)).then(move |_| HeldMsg::Done(req))
            }
            HeldMsg::Done(_) => call.reject(CallRejectedReason::UnsupportedMessage),
        }
    }
}

#[test]
fn multi_shard_held_call_does_not_hot_spin_the_worker() {
    let _guard = serial();
    let runtime = ThreadedMultiShardRuntime::with_config(
        [TestShard(1), TestShard(2)],
        DefaultThreadedMailboxFactory,
        ThreadedRuntimeConfig {
            command_capacity: 64,
            shard_pair_capacity: 64,
            idle_wait: Duration::from_millis(1),
            ..Default::default()
        },
    );
    let held = runtime
        .register_with_capacity_on::<HeldMS, Infallible>(ShardId::new(1), HeldMS, 4)
        .expect("register held");
    let hold = Duration::from_millis(300);

    let cpu_before = process_cpu_time();
    let wall_before = Instant::now();
    // HeldMS::Hold defers on a 500ms timer; a 300ms target deadline fires
    // first, so shard 1 spends ~300ms holding an in-flight call with a pending
    // timer and nothing to deliver. The old loop spun on `thread::yield_now()`.
    let outcome = runtime
        .call_blocking(held, HeldMsg::Hold, hold)
        .expect("call_blocking");
    let wall = wall_before.elapsed();
    let cpu = process_cpu_time().saturating_sub(cpu_before);

    assert_eq!(outcome, CallOutcome::Timeout);
    assert!(
        wall >= hold,
        "call should have waited out the target deadline, waited {wall:?}"
    );
    assert!(
        cpu < wall / 3,
        "shard worker burned {cpu:?} CPU over {wall:?} wall while a call was held; looks like a hot spin"
    );

    let _ = runtime.shutdown();
}
