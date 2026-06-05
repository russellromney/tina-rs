#![feature(allocator_api)]
//! Proofs for the readiness-driven worker park: the worker blocks on real
//! readiness instead of polling on a timer.
//!
//! - a fully idle worker makes ~0 park wakeups over a window (it blocks until a
//!   real wake source fires, rather than waking every `idle_wait`);
//! - a host command still wakes that blocked worker promptly;
//! - a threaded runtime over the *simulated* backend does not spin and still
//!   wakes for host commands (the simulated park sleeps on a condvar doorbell).
//!
//! The HTTP/socket readiness park is exercised end-to-end by the `tina-http`
//! suite (hundreds of real-socket tests over this worker); these tests pin the
//! wake *policy* with the park-wakeup counter.

use std::alloc::Global;
use std::convert::Infallible;
use std::time::{Duration, Instant};

use betelgeuse::io::simulated::SimulatedIO;
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

/// A fully idle worker blocks on the kernel and makes ~0 wakeups over a window.
///
/// With the old timer park the worker woke every `idle_wait` (~1ms), so a 250ms
/// idle window would show ~250 wakeups. The readiness park blocks until a real
/// wake source fires, so an idle worker with no timers, no I/O, and no commands
/// stays flat.
#[test]
fn idle_worker_makes_near_zero_wakeups() {
    let runtime: ThreadedRuntime<TestShard, DefaultThreadedMailboxFactory> =
        ThreadedRuntime::new(TestShard, DefaultThreadedMailboxFactory);
    let echo = runtime
        .register_with_capacity::<_, Infallible>(Echo, CAP)
        .expect("register echo");

    // Warm: one call so the worker has parked at least once after real work.
    assert_eq!(
        runtime
            .call_blocking(echo, EchoMsg::Ping, Duration::from_secs(1))
            .expect("call"),
        CallOutcome::Replied(1)
    );

    // Let it settle into a quiet park, then sample across an idle window.
    std::thread::sleep(Duration::from_millis(20));
    let before = runtime.park_wakeups();
    std::thread::sleep(Duration::from_millis(250));
    let delta = runtime.park_wakeups() - before;

    assert!(
        delta <= 2,
        "idle worker woke {delta} times over 250ms; a timer park would wake ~250 times"
    );

    runtime.shutdown().expect("shutdown");
}

/// The blocked idle worker still wakes promptly for a host command.
#[test]
fn idle_blocked_worker_wakes_for_command() {
    let runtime: ThreadedRuntime<TestShard, DefaultThreadedMailboxFactory> =
        ThreadedRuntime::new(TestShard, DefaultThreadedMailboxFactory);
    let echo = runtime
        .register_with_capacity::<_, Infallible>(Echo, CAP)
        .expect("register echo");

    // Reach a quiet, block-forever park.
    std::thread::sleep(Duration::from_millis(50));

    let started = Instant::now();
    let outcome = runtime
        .call_blocking(echo, EchoMsg::Ping, Duration::from_secs(1))
        .expect("call");
    let elapsed = started.elapsed();

    assert_eq!(outcome, CallOutcome::Replied(1));
    assert!(
        elapsed < Duration::from_millis(200),
        "blocked worker did not wake promptly for a command: {elapsed:?}"
    );

    runtime.shutdown().expect("shutdown");
}

/// A pre-wake race: a command admitted right as the worker decides to park must
/// not sleep forever. Hammer commands at a freshly-settling worker many times;
/// every one must be observed under a tight bound (the doorbell is coalescing,
/// so a wake landing just before the park is still seen).
#[test]
fn command_admitted_around_park_is_never_missed() {
    let runtime: ThreadedRuntime<TestShard, DefaultThreadedMailboxFactory> =
        ThreadedRuntime::new(TestShard, DefaultThreadedMailboxFactory);
    let echo = runtime
        .register_with_capacity::<_, Infallible>(Echo, CAP)
        .expect("register echo");

    for i in 0..200 {
        // A short idle gap most iterations lands the call right around the park
        // boundary; the worker must still wake and reply.
        if i % 2 == 0 {
            std::thread::sleep(Duration::from_micros(50));
        }
        let outcome = runtime
            .call_blocking(echo, EchoMsg::Ping, Duration::from_secs(2))
            .expect("call");
        assert_eq!(outcome, CallOutcome::Replied(1), "missed wake on iter {i}");
    }

    runtime.shutdown().expect("shutdown");
}

/// A threaded runtime over the *simulated* backend must not spin and must still
/// wake for host commands. The simulated park sleeps on a condvar doorbell
/// (bounded cap), so an idle worker stays low-wakeup and a call still completes.
///
/// `tina-sim` deterministic replay is unaffected: it drives the simulated
/// backend with explicit `step()` calls and never uses this live blocking park.
#[test]
fn simulated_threaded_backend_no_spin_and_command_wake() {
    let runtime: ThreadedRuntime<TestShard, DefaultThreadedMailboxFactory> =
        ThreadedRuntime::with_config_and_io_loop_factory(
            TestShard,
            DefaultThreadedMailboxFactory,
            ThreadedRuntimeConfig::default(),
            || SimulatedIO::new().loop_handle(Global),
        );
    let echo = runtime
        .register_with_capacity::<_, Infallible>(Echo, CAP)
        .expect("register echo");

    // Commands wake the simulated park.
    for _ in 0..10 {
        assert_eq!(
            runtime
                .call_blocking(echo, EchoMsg::Ping, Duration::from_secs(1))
                .expect("call"),
            CallOutcome::Replied(1)
        );
    }

    // No spin: the simulated park sleeps (its cap is ~1ms), so an idle window
    // shows far fewer wakeups than a busy-spin (which would be thousands).
    std::thread::sleep(Duration::from_millis(20));
    let before = runtime.park_wakeups();
    std::thread::sleep(Duration::from_millis(200));
    let delta = runtime.park_wakeups() - before;
    assert!(
        delta < 1000,
        "simulated worker appears to spin: {delta} wakeups in 200ms"
    );

    runtime.shutdown().expect("shutdown");
}
