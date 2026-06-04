//! Rock 4 observable proofs for the ready-isolate scheduler, through the
//! public `ThreadedRuntime` API. The internal mechanism (ready queue contents,
//! the missed-mark detector, incarnation safety) is proven directly in the
//! in-crate tests; here we prove the user-visible guarantees the ready queue
//! must preserve:
//!
//! - round-robin fairness: continuously-ready isolates advance in lockstep, so
//!   one hot isolate cannot pull arbitrarily ahead (one message per isolate per
//!   round);
//! - a cold isolate is not starved by a hot self-send flood.

use std::convert::Infallible;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
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

#[derive(Debug)]
enum SpinMsg {
    Start,
    Tick,
}

struct Spinner {
    ticks: Arc<AtomicU64>,
}

#[tina_runtime::isolate(message = SpinMsg, send = Outbound<SpinMsg>, shard = TestShard)]
impl Spinner {
    fn handle(&mut self, msg: SpinMsg, ctx: &mut Context<'_, TestShard, ()>) -> Effect<Self> {
        match msg {
            SpinMsg::Start | SpinMsg::Tick => {
                self.ticks.fetch_add(1, Ordering::Relaxed);
                ctx.send_self(SpinMsg::Tick)
            }
        }
    }
}

#[derive(Debug)]
enum EchoMsg {
    Ping,
}

struct Echo;

#[tina_runtime::isolate(message = EchoMsg, reply = u32, shard = TestShard)]
impl Echo {
    fn handle(&mut self, msg: EchoMsg, _ctx: &mut Context<'_, TestShard, u32>) -> Effect<Self> {
        match msg {
            EchoMsg::Ping => reply(1),
        }
    }

    fn handle_call(&mut self, msg: EchoMsg, call: CallContext<'_, Self>) -> Effect<Self> {
        match msg {
            EchoMsg::Ping => call.reply(1),
        }
    }
}

const CAP: usize = 64;

/// Three continuously-ready isolates must advance in lockstep: one message per
/// isolate per round means no spinner can pull more than about one round ahead
/// of the others. Without round-robin fairness one would run away.
#[test]
fn equal_isolates_advance_in_lockstep_under_load() {
    let runtime: ThreadedRuntime<TestShard, DefaultThreadedMailboxFactory> =
        ThreadedRuntime::with_config(
            TestShard,
            DefaultThreadedMailboxFactory,
            ThreadedRuntimeConfig::default(),
        );
    let counters: Vec<Arc<AtomicU64>> = (0..3).map(|_| Arc::new(AtomicU64::new(0))).collect();
    for ticks in &counters {
        let spinner = runtime
            .register_with_capacity::<_, SpinMsg>(
                Spinner {
                    ticks: ticks.clone(),
                },
                CAP,
            )
            .expect("register spinner");
        runtime
            .send_and_observe(spinner, SpinMsg::Start)
            .expect("start spinner");
    }

    // Let the spinners run, then sample all counters close together.
    std::thread::sleep(Duration::from_millis(100));
    let samples: Vec<u64> = counters.iter().map(|c| c.load(Ordering::Relaxed)).collect();
    runtime.shutdown().expect("shutdown");

    let min = *samples.iter().min().unwrap();
    let max = *samples.iter().max().unwrap();
    assert!(
        min > 100,
        "spinners should have made real progress: {samples:?}"
    );
    // Round-robin keeps them within a round of each other; allow a small margin
    // for non-simultaneous atomic reads mid-round.
    assert!(
        max - min <= 5,
        "round-robin fairness: spinners must stay in lockstep, got {samples:?} (spread {})",
        max - min
    );
}

/// A cold isolate must still be serviced promptly while a hot isolate floods
/// itself — the ready queue schedules the cold isolate as soon as it has a
/// message, regardless of the hot isolate's backlog.
#[test]
fn cold_isolate_served_under_hot_flood() {
    let runtime: ThreadedRuntime<TestShard, DefaultThreadedMailboxFactory> =
        ThreadedRuntime::with_config(
            TestShard,
            DefaultThreadedMailboxFactory,
            ThreadedRuntimeConfig::default(),
        );
    let hot_ticks = Arc::new(AtomicU64::new(0));
    let hot = runtime
        .register_with_capacity::<_, SpinMsg>(
            Spinner {
                ticks: hot_ticks.clone(),
            },
            CAP,
        )
        .expect("register hot");
    let cold = runtime
        .register_with_capacity::<_, Infallible>(Echo, CAP)
        .expect("register cold");
    runtime
        .send_and_observe(hot, SpinMsg::Start)
        .expect("start hot");

    // Wait until the hot isolate is clearly flooding.
    let deadline = Instant::now() + Duration::from_secs(2);
    while hot_ticks.load(Ordering::Relaxed) < 1_000 {
        assert!(Instant::now() < deadline, "hot isolate never got going");
        std::thread::yield_now();
    }

    let started = Instant::now();
    let outcome = runtime
        .call_blocking(cold, EchoMsg::Ping, Duration::from_secs(2))
        .expect("cold call");
    let elapsed = started.elapsed();
    assert_eq!(outcome, CallOutcome::Replied(1));
    assert!(
        elapsed < Duration::from_millis(500),
        "cold isolate starved by hot flood: {elapsed:?}"
    );

    runtime.shutdown().expect("shutdown");
}
