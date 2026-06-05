//! Scheduler fairness proofs through the public `ThreadedRuntime`. The runtime
//! gives each ready isolate one message per round (round-robin), so a hot
//! self-sending isolate cannot pull arbitrarily ahead and a cold isolate is
//! not starved. (These held under the prototype ready scheduler and continue
//! to hold under the full-scan step; see review.md for why the ready scheduler
//! was reverted.)

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
    CheckStarted,
    Tick,
}

struct Spinner {
    index: usize,
    ticks: Arc<Vec<AtomicU64>>,
    started: Arc<AtomicU64>,
    max_spread: Arc<AtomicU64>,
}

#[tina_runtime::isolate(message = SpinMsg, send = Outbound<SpinMsg>, shard = TestShard)]
impl Spinner {
    fn handle(&mut self, msg: SpinMsg, ctx: &mut Context<'_, TestShard, ()>) -> Effect<Self> {
        match msg {
            SpinMsg::Start => {
                self.started.fetch_add(1, Ordering::Relaxed);
                ctx.send_self(SpinMsg::CheckStarted)
            }
            SpinMsg::CheckStarted => {
                if self.started.load(Ordering::Relaxed) < self.ticks.len() as u64 {
                    ctx.send_self(SpinMsg::CheckStarted)
                } else {
                    ctx.send_self(SpinMsg::Tick)
                }
            }
            SpinMsg::Tick => {
                self.ticks[self.index].fetch_add(1, Ordering::Relaxed);
                record_spread(&self.ticks, &self.max_spread);
                ctx.send_self(SpinMsg::Tick)
            }
        }
    }
}

fn record_spread(ticks: &[AtomicU64], max_spread: &AtomicU64) {
    let mut min = u64::MAX;
    let mut max = 0;
    for tick in ticks {
        let value = tick.load(Ordering::Relaxed);
        min = min.min(value);
        max = max.max(value);
    }
    let spread = max.saturating_sub(min);
    let _ = max_spread.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |current| {
        (spread > current).then_some(spread)
    });
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

/// Three continuously-ready isolates advance in lockstep: one message per
/// isolate per round means no spinner pulls more than about one round ahead.
#[test]
fn equal_isolates_advance_in_lockstep_under_load() {
    let runtime: ThreadedRuntime<TestShard, DefaultThreadedMailboxFactory> =
        ThreadedRuntime::with_config(
            TestShard,
            DefaultThreadedMailboxFactory,
            ThreadedRuntimeConfig::default(),
        );
    let counters = Arc::new((0..3).map(|_| AtomicU64::new(0)).collect::<Vec<_>>());
    let started = Arc::new(AtomicU64::new(0));
    let max_spread = Arc::new(AtomicU64::new(0));
    for index in 0..counters.len() {
        let spinner = runtime
            .register_with_capacity::<_, SpinMsg>(
                Spinner {
                    index,
                    ticks: Arc::clone(&counters),
                    started: Arc::clone(&started),
                    max_spread: Arc::clone(&max_spread),
                },
                CAP,
            )
            .expect("register spinner");
        runtime
            .send_and_observe(spinner, SpinMsg::Start)
            .expect("start spinner");
    }

    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        let samples: Vec<u64> = counters.iter().map(|c| c.load(Ordering::Relaxed)).collect();
        if samples.iter().all(|count| *count > 1_000) {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "spinners should all make progress: {samples:?}"
        );
        std::thread::yield_now();
    }
    let samples: Vec<u64> = counters.iter().map(|c| c.load(Ordering::Relaxed)).collect();
    let observed_max_spread = max_spread.load(Ordering::Relaxed);
    runtime.shutdown().expect("shutdown");

    let min = *samples.iter().min().unwrap();
    let max = *samples.iter().max().unwrap();
    assert!(
        min > 100,
        "spinners should have made real progress: {samples:?}"
    );
    assert!(
        observed_max_spread <= 8,
        "round-robin fairness: spinners must stay bounded at handler boundaries, got final {samples:?} (final spread {}, max observed spread {observed_max_spread})",
        max - min
    );
}

/// A cold isolate is serviced promptly while a hot isolate floods itself.
#[test]
fn cold_isolate_served_under_hot_flood() {
    let runtime: ThreadedRuntime<TestShard, DefaultThreadedMailboxFactory> =
        ThreadedRuntime::with_config(
            TestShard,
            DefaultThreadedMailboxFactory,
            ThreadedRuntimeConfig::default(),
        );
    let hot_ticks = Arc::new(vec![AtomicU64::new(0)]);
    let hot = runtime
        .register_with_capacity::<_, SpinMsg>(
            Spinner {
                index: 0,
                ticks: Arc::clone(&hot_ticks),
                started: Arc::new(AtomicU64::new(0)),
                max_spread: Arc::new(AtomicU64::new(0)),
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

    let deadline = Instant::now() + Duration::from_secs(2);
    while hot_ticks[0].load(Ordering::Relaxed) < 1_000 {
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
