//! A wedged user handler must not hang the host control plane forever.
//!
//! `ThreadedRuntime::call` (and the multi-shard `call_on`) bound their wait on
//! the worker's reply. When a handler never returns, host-control commands
//! queued behind it surface `WorkerUnresponsive` within the configured
//! `control_call_timeout` instead of blocking the host thread indefinitely.

use std::convert::Infallible;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, mpsc};
use std::time::{Duration, Instant};

use tina::prelude::*;
use tina_runtime::{
    DefaultThreadedMailboxFactory, ThreadedMultiShardRuntime, ThreadedRuntime,
    ThreadedRuntimeConfig, ThreadedRuntimeError,
};

#[derive(Debug, Clone, Copy)]
struct MultiShard(u32);

impl Shard for MultiShard {
    fn id(&self) -> ShardId {
        ShardId::new(self.0)
    }
}

#[derive(Debug, Clone, Copy)]
struct TestShard;

impl Shard for TestShard {
    fn id(&self) -> ShardId {
        ShardId::new(0)
    }
}

#[derive(Debug, Clone, Copy)]
enum BlockMsg {
    Wedge,
}

// Handler that signals it has started, then never returns. It monopolises the
// single shard thread, so any host-control command queued behind it can only
// be answered if `call` gives up on its own timer.
struct Blocker {
    started: mpsc::Sender<()>,
}

#[tina_runtime::isolate(message = BlockMsg, reply = u32, shard = TestShard)]
impl Blocker {
    fn handle(&mut self, _msg: BlockMsg, _ctx: &mut Context<'_, TestShard, u32>) -> Effect<Self> {
        let _ = self.started.send(());
        // Wedge the worker. A very long park stands in for an infinite user
        // loop without pegging a CPU for the whole test run.
        std::thread::sleep(Duration::from_secs(3600));
        noop()
    }
}

#[test]
fn host_call_returns_worker_unresponsive_when_handler_wedges() {
    let control_call_timeout = Duration::from_millis(300);
    let runtime = ThreadedRuntime::with_config(
        TestShard,
        DefaultThreadedMailboxFactory,
        ThreadedRuntimeConfig {
            control_call_timeout,
            ..Default::default()
        },
    );

    let (started_tx, started_rx) = mpsc::channel();
    let blocker = runtime
        .register_with_capacity::<Blocker, Infallible>(
            Blocker {
                started: started_tx,
            },
            4,
        )
        .expect("register blocker");

    // Wedge the worker and wait until the handler is actually running, so the
    // next host call is guaranteed to queue behind it.
    runtime
        .try_send(blocker, BlockMsg::Wedge)
        .expect("kick blocker");
    started_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("handler started");

    // A host-control call now cannot be answered. It must give up on its own
    // timer, not hang, and report the wedge distinctly from a stopped worker.
    let start = Instant::now();
    let result = runtime.has_in_flight_calls();
    let elapsed = start.elapsed();

    assert_eq!(result, Err(ThreadedRuntimeError::WorkerUnresponsive));
    assert!(
        elapsed < control_call_timeout * 4,
        "host call must return near the bound, took {elapsed:?}"
    );
    assert!(
        elapsed >= control_call_timeout,
        "host call must actually wait the bound, took {elapsed:?}"
    );
}

// Multi-shard wedge: a handler that blocks one shard's worker forever, so a
// host-control command routed to that shard (`call_on`) must give up on its own
// timer with `WorkerUnresponsive`.
struct MultiBlocker {
    started: mpsc::Sender<()>,
}

#[tina_runtime::isolate(message = BlockMsg, reply = u32, shard = MultiShard)]
impl MultiBlocker {
    fn handle(&mut self, _msg: BlockMsg, _ctx: &mut Context<'_, MultiShard, u32>) -> Effect<Self> {
        let _ = self.started.send(());
        std::thread::sleep(Duration::from_secs(3600));
        noop()
    }
}

#[test]
fn multishard_host_call_returns_worker_unresponsive_when_handler_wedges() {
    let control_call_timeout = Duration::from_millis(300);
    let wedged_shard = ShardId::new(11);
    let runtime = ThreadedMultiShardRuntime::with_config(
        [MultiShard(11), MultiShard(22)],
        DefaultThreadedMailboxFactory,
        ThreadedRuntimeConfig {
            control_call_timeout,
            ..Default::default()
        },
    );

    let (started_tx, started_rx) = mpsc::channel();
    let blocker = runtime
        .register_with_capacity_on::<MultiBlocker, Infallible>(
            wedged_shard,
            MultiBlocker {
                started: started_tx,
            },
            4,
        )
        .expect("register blocker on shard 11");

    // Wedge shard 11's worker and wait until the handler is actually running.
    runtime
        .try_send(blocker, BlockMsg::Wedge)
        .expect("kick blocker");
    started_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("handler started");

    // A host-control call to the wedged shard (`trace_on` routes through the
    // private `call_on`) cannot be answered. It must give up on its own timer.
    let start = Instant::now();
    let result = runtime.trace_on(wedged_shard);
    let elapsed = start.elapsed();

    assert_eq!(result, Err(ThreadedRuntimeError::WorkerUnresponsive));
    // Only the deterministic lower bound: the call must actually wait the whole
    // configured budget before giving up. No upper bound — that would race the
    // OS scheduler under load.
    assert!(
        elapsed >= control_call_timeout,
        "host call must wait at least the control-call timeout, took {elapsed:?}"
    );

    // The other shard is unaffected: a control call to it still answers.
    assert!(
        runtime.trace_on(ShardId::new(22)).is_ok(),
        "the healthy shard must still answer control calls"
    );
}

// A gate whose handler blocks the worker until the test releases it. Unlike the
// 3600s `Blocker`, this one can be unwedged so the test cleans up.
struct Gate {
    started: mpsc::Sender<()>,
    release: mpsc::Receiver<()>,
}

#[tina_runtime::isolate(message = BlockMsg, shard = TestShard)]
impl Gate {
    fn handle(&mut self, _msg: BlockMsg, _ctx: &mut Context<'_, TestShard>) -> Effect<Self> {
        let _ = self.started.send(());
        // Hold the worker until the test releases the gate.
        let _ = self.release.recv();
        noop()
    }
}

#[derive(Debug, Clone, Copy)]
enum SinkMsg {
    Ping,
}

struct Sink;

#[tina_runtime::isolate(message = SinkMsg, shard = TestShard)]
impl Sink {
    fn handle(&mut self, _msg: SinkMsg, _ctx: &mut Context<'_, TestShard>) -> Effect<Self> {
        noop()
    }
}

#[test]
fn send_and_observe_blocks_indefinitely_on_wedged_worker() {
    // Intent pin: `send_and_observe` deliberately uses an UNBOUNDED `recv()`,
    // unlike `call`, which is `recv_timeout`-bounded. Its contract is to report
    // the exact mailbox outcome, so a wedged worker can hang the host thread —
    // by design. This test proves that: against a wedged worker the call stays
    // blocked well past the control-call timeout (a bounded call would have
    // returned by then). A "fix" that bounded it would flip `returned` early
    // and fail here.
    let control_call_timeout = Duration::from_millis(100);
    let runtime = Arc::new(ThreadedRuntime::with_config(
        TestShard,
        DefaultThreadedMailboxFactory,
        ThreadedRuntimeConfig {
            control_call_timeout,
            ..Default::default()
        },
    ));

    let (started_tx, started_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let gate = runtime
        .register_with_capacity::<Gate, Infallible>(
            Gate {
                started: started_tx,
                release: release_rx,
            },
            4,
        )
        .expect("register gate");
    let sink = runtime
        .register_with_capacity::<Sink, Infallible>(Sink, 4)
        .expect("register sink");

    // Wedge the worker and wait until it is actually parked in the handler.
    runtime.try_send(gate, BlockMsg::Wedge).expect("kick gate");
    started_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("gate started");

    let returned = Arc::new(AtomicBool::new(false));
    let observe_handle = {
        let runtime = Arc::clone(&runtime);
        let returned = Arc::clone(&returned);
        std::thread::spawn(move || {
            // Queues behind the wedged handler; with an unbounded recv it cannot
            // return until the worker is freed.
            let outcome = runtime.send_and_observe(sink, SinkMsg::Ping);
            returned.store(true, Ordering::SeqCst);
            outcome
        })
    };

    // Well past the control-call timeout: a bounded wait would have returned by
    // now. The unbounded `send_and_observe` must still be blocked.
    std::thread::sleep(control_call_timeout * 4);
    assert!(
        !returned.load(Ordering::SeqCst),
        "send_and_observe must not return while the worker is wedged (it is intentionally unbounded)"
    );

    // Release the gate; the worker drains the queued observe command and the
    // call finally returns, so the test cleans up its thread.
    release_tx.send(()).expect("release gate");
    let outcome = observe_handle.join().expect("observe thread joins");
    assert!(
        outcome.is_ok(),
        "once the worker is freed, the queued observe reports the mailbox outcome: {outcome:?}"
    );

    if let Ok(rt) = Arc::try_unwrap(runtime) {
        let _ = rt.shutdown();
    }
}
