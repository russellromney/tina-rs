//! Rock 2 proofs: the bounded worker hot-drain and the pending-work-aware
//! park. These pin behaviour, not wall-clock perf (the perf_native hot-path
//! probes own the timing rows). The idle-CPU wake-count proof lives in the
//! soak/CPU-sanity work; here we prove the park *policy*:
//!
//! - a pending runtime timer is serviced at `idle_repoll_interval`, not the
//!   long `idle_wait` — so runtime-owned work the worker cannot be signalled
//!   about stays low-latency;
//! - a fully idle worker still wakes immediately for a host command even when
//!   `idle_wait` is very long — the park never hides a command;
//! - a tiny `hot_drain_max_rounds` cap never breaks call correctness;
//! - zero budgets are rejected at construction.

use std::convert::Infallible;
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use tina::prelude::*;
use tina_runtime::{
    CallOutcome, DefaultThreadedMailboxFactory, StartupError, ThreadedRuntime,
    ThreadedRuntimeConfig, ThreadedRuntimeConfigError, sleep,
};

#[derive(Debug, Clone, Copy)]
struct TestShard;

impl Shard for TestShard {
    fn id(&self) -> ShardId {
        ShardId::new(0)
    }
}

type Signal = Arc<(Mutex<bool>, Condvar)>;

fn wait_signal(signal: &Signal, timeout: Duration) -> bool {
    let (lock, cvar) = &**signal;
    let mut fired = lock.lock().expect("signal lock");
    let deadline = Instant::now() + timeout;
    while !*fired {
        let now = Instant::now();
        if now >= deadline {
            return false;
        }
        let (next, result) = cvar
            .wait_timeout(fired, deadline - now)
            .expect("signal wait");
        fired = next;
        if result.timed_out() && !*fired {
            return false;
        }
    }
    true
}

fn raise(signal: &Signal) {
    let (lock, cvar) = &**signal;
    *lock.lock().expect("signal lock") = true;
    cvar.notify_all();
}

// --- a timer isolate: on Start, sleep then send itself Fired; on Fired, raise.

#[derive(Debug)]
enum TimerMsg {
    Start,
    Fired,
}

struct Timer {
    sleep_for: Duration,
    fired: Signal,
}

#[tina_runtime::isolate(message = TimerMsg, shard = TestShard)]
impl Timer {
    fn handle(&mut self, msg: TimerMsg, _ctx: &mut Context<'_, TestShard, ()>) -> Effect<Self> {
        match msg {
            TimerMsg::Start => {
                let after = self.sleep_for;
                sleep(after).then_event(|| TimerMsg::Fired)
            }
            TimerMsg::Fired => {
                raise(&self.fired);
                noop()
            }
        }
    }
}

// --- a trivial echo for the responsiveness probe.

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

// --- a spinner that keeps the worker hot by self-sending forever.

#[derive(Debug)]
enum SpinMsg {
    Start,
    Tick,
}

struct Spinner {
    ticks: Arc<std::sync::atomic::AtomicU64>,
}

#[tina_runtime::isolate(message = SpinMsg, send = Outbound<SpinMsg>, shard = TestShard)]
impl Spinner {
    fn handle(&mut self, msg: SpinMsg, ctx: &mut Context<'_, TestShard, ()>) -> Effect<Self> {
        match msg {
            SpinMsg::Start | SpinMsg::Tick => {
                self.ticks
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                // Self-send keeps step() > 0 every round, holding the worker in
                // a sustained hot-drain burst.
                ctx.send_self(SpinMsg::Tick)
            }
        }
    }
}

const CAP: usize = 64;

fn wait_until<F: Fn() -> bool>(predicate: F, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if predicate() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(1));
    }
    predicate()
}

fn config_with(
    idle_wait: Duration,
    idle_repoll: Duration,
    hot_rounds: usize,
) -> ThreadedRuntimeConfig {
    ThreadedRuntimeConfig {
        idle_wait,
        idle_repoll_interval: idle_repoll,
        hot_drain_max_rounds: hot_rounds,
        ..ThreadedRuntimeConfig::default()
    }
}

/// A pending runtime timer must be serviced at `idle_repoll_interval`, not the
/// long `idle_wait`. With idle_wait = 2s and idle_repoll = 2ms, a 15ms timer
/// must fire in well under the idle_wait window; if the park used idle_wait for
/// pending work the timer would not fire for ~2s.
#[test]
fn pending_timer_serviced_at_idle_repoll_not_idle_wait() {
    let fired: Signal = Arc::new((Mutex::new(false), Condvar::new()));
    let runtime: ThreadedRuntime<TestShard, DefaultThreadedMailboxFactory> =
        ThreadedRuntime::with_config(
            TestShard,
            DefaultThreadedMailboxFactory,
            config_with(Duration::from_secs(2), Duration::from_millis(2), 4096),
        );
    let timer = runtime
        .register_with_capacity::<_, Infallible>(
            Timer {
                sleep_for: Duration::from_millis(15),
                fired: fired.clone(),
            },
            CAP,
        )
        .expect("register timer");

    runtime
        .send_and_observe(timer, TimerMsg::Start)
        .expect("send Start");

    // Generous ceiling (300ms) that is still far below the 2s idle_wait: this
    // can only pass if the pending-timer park used idle_repoll_interval.
    assert!(
        wait_signal(&fired, Duration::from_millis(300)),
        "pending timer was not serviced near its deadline; the park used idle_wait, not idle_repoll_interval"
    );

    runtime.shutdown().expect("shutdown");
}

/// A fully idle worker (long idle_wait, nothing pending) must still wake
/// immediately for a host command. Go idle, then issue a blocking call and
/// require it to complete in well under the idle_wait window.
#[test]
fn idle_worker_wakes_immediately_for_a_command() {
    let runtime: ThreadedRuntime<TestShard, DefaultThreadedMailboxFactory> =
        ThreadedRuntime::with_config(
            TestShard,
            DefaultThreadedMailboxFactory,
            config_with(Duration::from_secs(2), Duration::from_millis(2), 4096),
        );
    let echo = runtime
        .register_with_capacity::<_, Infallible>(Echo, CAP)
        .expect("register echo");

    // Let the worker reach a fully-idle park (idle_wait = 2s).
    std::thread::sleep(Duration::from_millis(50));

    let started = Instant::now();
    let outcome = runtime
        .call_blocking(echo, EchoMsg::Ping, Duration::from_secs(1))
        .expect("call");
    let elapsed = started.elapsed();

    assert_eq!(outcome, CallOutcome::Replied(1));
    assert!(
        elapsed < Duration::from_millis(300),
        "idle worker did not wake promptly for a command: {elapsed:?} (idle_wait is 2s)"
    );

    runtime.shutdown().expect("shutdown");
}

/// A tiny `hot_drain_max_rounds` cap must not break correctness: a host call
/// crosses several runtime turns, and with a 1-round burst budget the worker
/// simply re-polls between each, still completing the call.
#[test]
fn tiny_hot_drain_budget_still_completes_calls() {
    let runtime: ThreadedRuntime<TestShard, DefaultThreadedMailboxFactory> =
        ThreadedRuntime::with_config(
            TestShard,
            DefaultThreadedMailboxFactory,
            config_with(Duration::from_millis(1), Duration::from_millis(1), 1),
        );
    let echo = runtime
        .register_with_capacity::<_, Infallible>(Echo, CAP)
        .expect("register echo");

    for _ in 0..50 {
        assert_eq!(
            runtime
                .call_blocking(echo, EchoMsg::Ping, Duration::from_secs(1))
                .expect("call"),
            CallOutcome::Replied(1)
        );
    }

    runtime.shutdown().expect("shutdown");
}

/// A host command issued while the worker is in a sustained hot-drain burst
/// must still be observed and serviced under a bounded latency. A `send_self`
/// spinner holds the worker hot; an interleaved blocking call to a *different*
/// isolate must still reply quickly — proving the per-round command poll inside
/// the drain is not just structurally present but effective.
#[test]
fn command_serviced_during_self_send_storm() {
    let ticks = Arc::new(std::sync::atomic::AtomicU64::new(0));
    let runtime: ThreadedRuntime<TestShard, DefaultThreadedMailboxFactory> =
        ThreadedRuntime::with_config(
            TestShard,
            DefaultThreadedMailboxFactory,
            ThreadedRuntimeConfig::default(),
        );
    let spinner = runtime
        .register_with_capacity::<_, SpinMsg>(
            Spinner {
                ticks: ticks.clone(),
            },
            CAP,
        )
        .expect("register spinner");
    let echo = runtime
        .register_with_capacity::<_, Infallible>(Echo, CAP)
        .expect("register echo");

    runtime
        .send_and_observe(spinner, SpinMsg::Start)
        .expect("start spinner");
    // Confirm the storm is actually running (the worker is hot).
    assert!(
        wait_until(
            || ticks.load(std::sync::atomic::Ordering::Relaxed) > 1_000,
            Duration::from_secs(2)
        ),
        "spinner never got hot"
    );

    let started = Instant::now();
    let outcome = runtime
        .call_blocking(echo, EchoMsg::Ping, Duration::from_secs(2))
        .expect("call during storm");
    let elapsed = started.elapsed();
    assert_eq!(outcome, CallOutcome::Replied(1));
    assert!(
        elapsed < Duration::from_millis(500),
        "command starved by the hot-drain storm: {elapsed:?}"
    );

    runtime.shutdown().expect("shutdown");
}

/// Shutdown must be observed promptly even while the worker is in a sustained
/// hot-drain burst — the inner-loop `try_recv` catches the Shutdown command.
#[test]
fn shutdown_observed_during_self_send_storm() {
    let ticks = Arc::new(std::sync::atomic::AtomicU64::new(0));
    let runtime: ThreadedRuntime<TestShard, DefaultThreadedMailboxFactory> =
        ThreadedRuntime::with_config(
            TestShard,
            DefaultThreadedMailboxFactory,
            ThreadedRuntimeConfig::default(),
        );
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
    assert!(
        wait_until(
            || ticks.load(std::sync::atomic::Ordering::Relaxed) > 1_000,
            Duration::from_secs(2)
        ),
        "spinner never got hot"
    );

    let started = Instant::now();
    runtime.shutdown().expect("shutdown during storm");
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "shutdown starved by the hot-drain storm: {:?}",
        started.elapsed()
    );
}

#[test]
fn zero_hot_drain_rounds_rejected() {
    let error = ThreadedRuntime::<TestShard, DefaultThreadedMailboxFactory>::try_with_config(
        TestShard,
        DefaultThreadedMailboxFactory,
        ThreadedRuntimeConfig {
            hot_drain_max_rounds: 0,
            ..ThreadedRuntimeConfig::default()
        },
    )
    .err()
    .expect("zero rounds must fail");
    assert!(matches!(
        error,
        StartupError::InvalidThreadedConfig(ThreadedRuntimeConfigError::ZeroHotDrainMaxRounds)
    ));
}

#[test]
fn zero_hot_drain_elapsed_rejected() {
    let error = ThreadedRuntime::<TestShard, DefaultThreadedMailboxFactory>::try_with_config(
        TestShard,
        DefaultThreadedMailboxFactory,
        ThreadedRuntimeConfig {
            hot_drain_max_elapsed: Duration::ZERO,
            ..ThreadedRuntimeConfig::default()
        },
    )
    .err()
    .expect("zero elapsed budget must fail");
    assert!(matches!(
        error,
        StartupError::InvalidThreadedConfig(ThreadedRuntimeConfigError::ZeroHotDrainMaxElapsed)
    ));
}

#[test]
fn zero_idle_repoll_rejected() {
    let error = ThreadedRuntime::<TestShard, DefaultThreadedMailboxFactory>::try_with_config(
        TestShard,
        DefaultThreadedMailboxFactory,
        ThreadedRuntimeConfig {
            idle_repoll_interval: Duration::ZERO,
            ..ThreadedRuntimeConfig::default()
        },
    )
    .err()
    .expect("zero idle repoll must fail");
    assert!(matches!(
        error,
        StartupError::InvalidThreadedConfig(ThreadedRuntimeConfigError::ZeroIdleRePollInterval)
    ));
}
