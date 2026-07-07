//! Live multishard trace-hash honesty.
//!
//! The proof-harness used to claim "sort the captured live trace by id,
//! hash it, and the hash is stable regardless of cross-shard arrival
//! order." That claim was false for live multishard for two reasons:
//!
//! 1. Event ids came from one shared `AtomicU64(Relaxed)` counter across
//!    worker threads, so the id of a logical event was decided by a
//!    `fetch_add` race (now fixed: ids are per-shard-local).
//! 2. Even with stable ids, a free-running multishard runtime interleaves
//!    each shard's inbound cross-shard deliveries with its local handler
//!    work by wall-clock timing, so the per-shard event *sequence* itself
//!    differs between runs. No id scheme makes that hash stable.
//!
//! So the honest behavior is: the proof-grade snapshot **fails closed** on
//! a multishard trace, while a single-shard live trace stays stable across
//! runs. This test pins both. The deterministic simulator multishard hash
//! (proven stable by the DST replay-case tests) is the contrast.

use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::convert::Infallible;
use std::time::{Duration, Instant};

use tina::{Address, Mailbox, TrySendError, prelude::*};
use tina_proof_harness::{LiveTrace, LiveTraceDrain, LiveTraceProofError};
use tina_runtime::{MailboxFactory, RuntimeCall, ThreadedMultiShardRuntime, ThreadedRuntimeConfig};

#[derive(Debug, Clone, Copy)]
struct AppShard(u32);

impl Shard for AppShard {
    fn id(&self) -> ShardId {
        ShardId::new(self.0)
    }
}

struct TestMailbox<T> {
    capacity: usize,
    queue: RefCell<VecDeque<T>>,
    closed: Cell<bool>,
}

impl<T> Mailbox<T> for TestMailbox<T> {
    fn capacity(&self) -> usize {
        self.capacity
    }

    fn try_send(&self, message: T) -> Result<(), TrySendError<T>> {
        if self.closed.get() {
            return Err(TrySendError::Closed(message));
        }
        let mut queue = self.queue.borrow_mut();
        if queue.len() >= self.capacity {
            return Err(TrySendError::Full(message));
        }
        queue.push_back(message);
        Ok(())
    }

    fn recv(&self) -> Option<T> {
        self.queue.borrow_mut().pop_front()
    }

    fn is_empty(&self) -> bool {
        self.queue.borrow().is_empty()
    }

    fn close(&self) {
        self.closed.set(true);
    }
}

#[derive(Debug, Clone, Copy)]
struct TestMailboxFactory;

impl MailboxFactory for TestMailboxFactory {
    fn create<T: 'static>(&self, capacity: usize) -> Box<dyn Mailbox<T>> {
        Box::new(TestMailbox {
            capacity,
            queue: RefCell::new(VecDeque::new()),
            closed: Cell::new(false),
        })
    }
}

/// One isolate that pings a cross-shard peer a fixed number of times and
/// counts inbound pings. Symmetric traffic on both shards interleaves the
/// two worker threads.
#[derive(Debug)]
enum PingMsg {
    /// Wire the peer address and self-kick the first tick.
    Start {
        peer: Address<PingMsg>,
        remaining: usize,
    },
    /// Self-kick: emit one cross-shard ping, then re-kick `remaining - 1`.
    Tick { remaining: usize },
    /// A cross-shard ping landed.
    Ping,
}

struct Pinger {
    peer: Option<Address<PingMsg>>,
}

impl Isolate for Pinger {
    tina::isolate_types! {
        message: PingMsg,
        reply: (),
        send: Outbound<PingMsg>,
        spawn: Infallible,
        io: RuntimeCall<PingMsg>,
        shard: AppShard,
    }

    fn handle(
        &mut self,
        msg: Self::Message,
        ctx: &mut Context<'_, Self::Shard, Self::Reply>,
    ) -> Effect<Self> {
        match msg {
            PingMsg::Start { peer, remaining } => {
                self.peer = Some(peer);
                let me: Address<PingMsg> = ctx.me();
                send(me, PingMsg::Tick { remaining })
            }
            PingMsg::Tick { remaining } => {
                if remaining == 0 {
                    return noop();
                }
                let peer = self.peer.expect("peer wired by Start");
                let me: Address<PingMsg> = ctx.me();
                batch(vec![
                    send(peer, PingMsg::Ping),
                    send(
                        me,
                        PingMsg::Tick {
                            remaining: remaining - 1,
                        },
                    ),
                ])
            }
            PingMsg::Ping => noop(),
        }
    }
}

const PINGS_PER_SHARD: usize = 40;

/// Wait until the trace stops growing, then capture before shutdown so the
/// event set is fully drained.
fn drain(trace: &LiveTrace) {
    let mut stable_len = 0;
    let mut stable_since = Instant::now();
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let len = trace.len();
        if len == stable_len {
            if stable_since.elapsed() > Duration::from_millis(100) {
                break;
            }
        } else {
            stable_len = len;
            stable_since = Instant::now();
        }
        if Instant::now() > deadline {
            break;
        }
        std::thread::sleep(Duration::from_millis(2));
    }
}

/// Run the fixed 2-shard cross-shard workload once and return the captured
/// live trace.
fn run_multishard() -> LiveTrace {
    let trace = LiveTrace::new();
    let config = ThreadedRuntimeConfig {
        shard_pair_capacity: PINGS_PER_SHARD + 16,
        ..ThreadedRuntimeConfig::default()
    };
    let runtime = ThreadedMultiShardRuntime::with_config_and_trace_observer(
        [AppShard(11), AppShard(22)],
        TestMailboxFactory,
        config,
        trace.observer(),
    );

    let a = runtime
        .register_with_capacity_on::<Pinger, _>(ShardId::new(11), Pinger { peer: None }, 256)
        .expect("register pinger a");
    let b = runtime
        .register_with_capacity_on::<Pinger, _>(ShardId::new(22), Pinger { peer: None }, 256)
        .expect("register pinger b");

    runtime
        .try_send(
            a,
            PingMsg::Start {
                peer: b,
                remaining: PINGS_PER_SHARD,
            },
        )
        .expect("start a");
    runtime
        .try_send(
            b,
            PingMsg::Start {
                peer: a,
                remaining: PINGS_PER_SHARD,
            },
        )
        .expect("start b");

    drain(&trace);
    let _ = runtime.shutdown();
    trace
}

/// Run a single-shard self-ping workload and return the captured trace.
/// One shard, one thread: the event sequence is deterministic.
fn run_single_shard() -> LiveTrace {
    let trace = LiveTrace::new();
    let runtime = ThreadedMultiShardRuntime::with_config_and_trace_observer(
        [AppShard(11)],
        TestMailboxFactory,
        ThreadedRuntimeConfig::default(),
        trace.observer(),
    );

    let sink = runtime
        .register_with_capacity_on::<Pinger, _>(ShardId::new(11), Pinger { peer: None }, 256)
        .expect("register sink");
    let source = runtime
        .register_with_capacity_on::<Pinger, _>(ShardId::new(11), Pinger { peer: None }, 256)
        .expect("register source");
    runtime
        .try_send(
            source,
            PingMsg::Start {
                peer: sink,
                remaining: PINGS_PER_SHARD,
            },
        )
        .expect("start source");

    drain(&trace);
    let _ = runtime.shutdown();
    trace
}

#[test]
fn live_multishard_proof_snapshot_fails_closed() {
    // A free-running multishard live trace has a timing-ordered per-shard
    // event sequence, so its hash is not a stable proof. The proof-grade
    // snapshot must refuse it rather than return a hash that flaps.
    for iteration in 0..20 {
        let trace = run_multishard();
        assert!(trace.shards().len() >= 2, "workload must span both shards");
        let err = trace
            .snapshot_complete(LiveTraceDrain::direct())
            .expect_err("multishard proof snapshot must fail closed");
        assert!(
            matches!(err, LiveTraceProofError::Multishard { .. }),
            "iteration {iteration}: expected multishard rejection, got {err:?}",
        );
    }
}

#[test]
fn live_single_shard_trace_hash_is_stable_across_runs() {
    // The contrast: one shard, one worker thread, so the event sequence is
    // deterministic and the proof snapshot is stable across runs.
    let first = run_single_shard()
        .snapshot_complete(LiveTraceDrain::direct())
        .expect("single-shard proof snapshot");
    for iteration in 0..20 {
        let again = run_single_shard()
            .snapshot_complete(LiveTraceDrain::direct())
            .expect("single-shard proof snapshot");
        assert_eq!(
            first, again,
            "single-shard live snapshot flapped on iteration {iteration}: {first:?} vs {again:?}",
        );
    }
}
