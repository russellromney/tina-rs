//! Phase 124 Rock 4 — multi-shard local-command fairness under sustained
//! remote inbound pressure.
//!
//! The threaded multi-shard worker loop drains a bounded number of
//! cross-shard inbound envelopes per pass and then must service at least
//! one local `ThreadedCommand` if one is waiting. Without that fairness
//! step, a steady flood of cross-shard messages keeps `remote_delivered`
//! nonzero forever and `Run` / `Shutdown` commands stay unread.
//!
//! These tests pin three user-visible obligations:
//! - `call_on`-style local commands complete promptly under flood;
//! - `shutdown` completes promptly under flood;
//! - normal cross-shard throughput still progresses after the fairness
//!   change (no regression).

use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::convert::Infallible;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use tina::{Address, Mailbox, TrySendError, prelude::*};
use tina_runtime::{MailboxFactory, RuntimeCall, ThreadedMultiShardRuntime};

#[derive(Debug, Clone, Copy)]
struct AppShard(u32);

impl Shard for AppShard {
    fn id(&self) -> ShardId {
        ShardId::new(self.0)
    }
}

struct WorkerMailbox<T> {
    capacity: usize,
    queue: RefCell<VecDeque<T>>,
    closed: Cell<bool>,
}

impl<T> Mailbox<T> for WorkerMailbox<T> {
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

    fn close(&self) {
        self.closed.set(true);
    }
}

#[derive(Debug, Clone, Copy)]
struct WorkerMailboxFactory;

impl MailboxFactory for WorkerMailboxFactory {
    fn create<T: 'static>(&self, capacity: usize) -> Box<dyn Mailbox<T>> {
        Box::new(WorkerMailbox {
            capacity,
            queue: RefCell::new(VecDeque::new()),
            closed: Cell::new(false),
        })
    }
}

/// Shared message type so the flood source and the sink can both be
/// addressed with one `Outbound<FloodMsg>`. The source consumes `Tick`
/// and the sink consumes `Hit`; each isolate ignores the variants meant
/// for the other.
#[derive(Debug)]
enum FloodMsg {
    /// Source self-tick. Each tick fans out one `Hit` to the sink and
    /// one self-tick with `remaining - 1`.
    Tick { remaining: usize },
    /// Cross-shard delivery counted by the sink.
    Hit,
}

struct FloodSource {
    target: Address<FloodMsg>,
    /// Test-visible stop flag. Setting this to `false` lets the source
    /// drop the next self-tick without emitting more cross-shard sends —
    /// avoiding cross-shard emissions during shutdown drain, which is a
    /// separate (pre-existing) failure mode and not what these tests
    /// pin.
    running: Arc<std::sync::atomic::AtomicBool>,
    /// Cross-shard hits per tick. Tests pick a wide fanout (well above
    /// the pair queue capacity) to keep the destination's remote inbound
    /// fed; a narrow fanout (1) reproduces ordinary throughput shape.
    fanout: usize,
}

impl Isolate for FloodSource {
    tina::isolate_types! {
        message: FloodMsg,
        reply: (),
        send: Outbound<FloodMsg>,
        spawn: Infallible,
        call: RuntimeCall<FloodMsg>,
        shard: AppShard,
    }

    fn handle(
        &mut self,
        msg: Self::Message,
        ctx: &mut Context<'_, Self::Shard, Self::Reply>,
    ) -> Effect<Self> {
        match msg {
            FloodMsg::Tick { remaining } => {
                if remaining == 0 || !self.running.load(Ordering::Relaxed) {
                    return noop();
                }
                let me: Address<FloodMsg> = ctx.me();
                // Emit a wide fanout of cross-shard outbounds per tick so
                // the destination pair queue stays full between B's drain
                // passes — that is the condition under which the original
                // worker loop kept `remote_delivered > 0` indefinitely.
                let mut effects = Vec::with_capacity(self.fanout + 1);
                for _ in 0..self.fanout {
                    effects.push(send(self.target, FloodMsg::Hit));
                }
                effects.push(send(
                    me,
                    FloodMsg::Tick {
                        remaining: remaining.saturating_sub(1),
                    },
                ));
                batch(effects)
            }
            FloodMsg::Hit => noop(),
        }
    }
}

struct Sink {
    hits: Arc<AtomicUsize>,
}

impl Isolate for Sink {
    tina::isolate_types! {
        message: FloodMsg,
        reply: (),
        send: Outbound<FloodMsg>,
        spawn: Infallible,
        call: RuntimeCall<FloodMsg>,
        shard: AppShard,
    }

    fn handle(
        &mut self,
        msg: Self::Message,
        _ctx: &mut Context<'_, Self::Shard, Self::Reply>,
    ) -> Effect<Self> {
        match msg {
            FloodMsg::Hit => {
                self.hits.fetch_add(1, Ordering::Relaxed);
                noop()
            }
            FloodMsg::Tick { .. } => noop(),
        }
    }
}

fn make_runtime() -> ThreadedMultiShardRuntime<AppShard, WorkerMailboxFactory> {
    ThreadedMultiShardRuntime::new([AppShard(11), AppShard(22)], WorkerMailboxFactory)
}

const FLOOD_TICKS: usize = 100_000;
const FANOUT_PER_TICK: usize = 512;
/// Number of independent flood sources on shard 11. With several in
/// flight, the destination's pair queue stays full between B's bounded
/// drain passes — the precondition for the starvation case A12 names.
const FLOOD_SOURCES: usize = 4;

fn register_flood(
    runtime: &ThreadedMultiShardRuntime<AppShard, WorkerMailboxFactory>,
    sink: Address<FloodMsg>,
    running: &Arc<std::sync::atomic::AtomicBool>,
    n_sources: usize,
    ticks: usize,
) {
    for _ in 0..n_sources {
        let source = runtime
            .register_with_capacity_on::<FloodSource, _>(
                ShardId::new(11),
                FloodSource {
                    target: sink,
                    running: Arc::clone(running),
                    fanout: FANOUT_PER_TICK,
                },
                128,
            )
            .expect("register source");
        runtime
            .try_send(source, FloodMsg::Tick { remaining: ticks })
            .expect("kick flood");
    }
}

/// Required test (Rock 4, bullet 1): a sustained cross-shard inbound
/// flood on the target shard must not starve a local host `Run` command.
/// `observe_result` routes the registration through
/// `ThreadedCommand::Run`, which is the same queue `call_blocking` setup
/// and direct `Run` commands use.
#[test]
fn remote_flood_does_not_starve_local_run_command() {
    let runtime = make_runtime();
    let hits = Arc::new(AtomicUsize::new(0));

    let sink = runtime
        .register_with_capacity_on::<Sink, _>(
            ShardId::new(22),
            Sink {
                hits: Arc::clone(&hits),
            },
            128,
        )
        .expect("register sink");

    let running = Arc::new(std::sync::atomic::AtomicBool::new(true));
    register_flood(&runtime, sink, &running, FLOOD_SOURCES, FLOOD_TICKS);

    // Give the flood time to fill the cross-shard pair channel and the
    // target's mailbox so shard 22's worker is in steady-state drain.
    std::thread::sleep(Duration::from_millis(50));
    assert!(hits.load(Ordering::Relaxed) > 0, "flood must be in flight");

    // Local `Run` command to shard 22: bounded latency despite the
    // sustained remote inbound on shard 22. `observe_result` routes the
    // registration through `ThreadedCommand::Run`.
    let start = Instant::now();
    let waiter = runtime
        .observe_result::<u64, _, _>(sink)
        .expect("observe_result returns through local command queue");
    drop(waiter);
    let local_latency = start.elapsed();
    assert!(
        local_latency < Duration::from_secs(2),
        "local Run command starved by remote flood (took {:?})",
        local_latency
    );

    running.store(false, Ordering::Relaxed);
    std::thread::sleep(Duration::from_millis(50));
    let _ = runtime.shutdown();
}

/// Required test (Rock 4, bullet 3): shutdown under sustained remote
/// inbound flood completes within a bounded amount of wall time. The
/// fairness fix guarantees the worker reads its command queue between
/// every bounded remote-drain pass, so the `Shutdown` command does not
/// have to wait for the flood to drain.
#[test]
fn shutdown_under_remote_flood_completes_bounded() {
    let runtime = make_runtime();
    let hits = Arc::new(AtomicUsize::new(0));

    let sink = runtime
        .register_with_capacity_on::<Sink, _>(
            ShardId::new(22),
            Sink {
                hits: Arc::clone(&hits),
            },
            128,
        )
        .expect("register sink");

    let running = Arc::new(std::sync::atomic::AtomicBool::new(true));
    register_flood(&runtime, sink, &running, FLOOD_SOURCES, FLOOD_TICKS);

    // Wait until the flood is actively in flight.
    std::thread::sleep(Duration::from_millis(50));
    assert!(hits.load(Ordering::Relaxed) > 0, "flood must be in flight");

    let start = Instant::now();
    // Stop the source first so shutdown-time `runtime.step()` does not
    // try to route cross-shard sends (that path lacks a remote handler
    // and panics — separate pre-existing failure mode, not A12).
    running.store(false, Ordering::Relaxed);
    let _report = runtime.shutdown();
    let shutdown_latency = start.elapsed();
    assert!(
        shutdown_latency < Duration::from_secs(3),
        "shutdown starved by remote flood (took {:?})",
        shutdown_latency
    );
}

/// Required test (Rock 4, bullet 4): the fairness change does not break
/// ordinary cross-shard throughput — finite cross-shard work still
/// drains and reaches the target.
#[test]
fn ordinary_remote_throughput_still_progresses() {
    let runtime = make_runtime();
    let hits = Arc::new(AtomicUsize::new(0));
    const N: usize = 500;

    let sink = runtime
        .register_with_capacity_on::<Sink, _>(
            ShardId::new(22),
            Sink {
                hits: Arc::clone(&hits),
            },
            N + 16,
        )
        .expect("register sink");

    let running = Arc::new(std::sync::atomic::AtomicBool::new(true));
    // Narrow fanout (1 cross-shard per tick) so every send fits the pair
    // queue and no message is silently dropped by Full backpressure. The
    // flood tests use a wide fanout to stress the drain budget; this
    // test pins ordinary, bounded throughput.
    let source = runtime
        .register_with_capacity_on::<FloodSource, _>(
            ShardId::new(11),
            FloodSource {
                target: sink,
                running: Arc::clone(&running),
                fanout: 1,
            },
            128,
        )
        .expect("register source");

    runtime
        .try_send(source, FloodMsg::Tick { remaining: N })
        .expect("kick burst");
    let expected = N;

    let start = Instant::now();
    // Every cross-shard message produced by the bounded burst should
    // land at the sink. With the fairness change, the worker still
    // drains its remote inbound budget on every pass, so ordinary
    // throughput is unchanged.
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline && hits.load(Ordering::Relaxed) < expected {
        std::thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(
        hits.load(Ordering::Relaxed),
        expected,
        "fairness change must not regress finite cross-shard throughput"
    );
    let elapsed = start.elapsed();
    assert!(
        elapsed < Duration::from_millis(300),
        "productive cross-shard backlog paid a fixed sleep per round (took {:?})",
        elapsed
    );

    running.store(false, Ordering::Relaxed);
    let _ = runtime.shutdown();
}
