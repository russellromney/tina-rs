//! A wedged user handler must not hang the host control plane forever.
//!
//! `ThreadedRuntime::call` (and the multi-shard `call_on`) bound their wait on
//! the worker's reply. When a handler never returns, host-control commands
//! queued behind it surface `WorkerUnresponsive` within the configured
//! `control_call_timeout` instead of blocking the host thread indefinitely.

use std::convert::Infallible;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use tina::prelude::*;
use tina_runtime::{
    DefaultThreadedMailboxFactory, ThreadedRuntime, ThreadedRuntimeConfig, ThreadedRuntimeError,
};

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
        .register_with_capacity::<Blocker, Infallible>(Blocker { started: started_tx }, 4)
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
