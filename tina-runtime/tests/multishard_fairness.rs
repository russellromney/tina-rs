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
use tina_runtime::{
    CallOutcome, MailboxFactory, RuntimeCall, ThreadedMultiShardRuntime, ThreadedRuntimeConfig,
    call, sleep,
};

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
    fn is_empty(&self) -> bool {
        self.queue.borrow().is_empty()
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
    /// Cross-shard probe from a different source shard. This is separate
    /// from `Hit` so the test can prove a higher-id source got a turn
    /// while a lower-id source was flooding.
    Probe,
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
        io: RuntimeCall<FloodMsg>,
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
            FloodMsg::Hit | FloodMsg::Probe => noop(),
        }
    }
}

struct Sink {
    hits: Arc<AtomicUsize>,
    probes: Arc<AtomicUsize>,
}

impl Isolate for Sink {
    tina::isolate_types! {
        message: FloodMsg,
        reply: (),
        send: Outbound<FloodMsg>,
        spawn: Infallible,
        io: RuntimeCall<FloodMsg>,
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
            FloodMsg::Probe => {
                self.probes.fetch_add(1, Ordering::Relaxed);
                noop()
            }
            FloodMsg::Tick { .. } => noop(),
        }
    }
}

struct ProbeSource {
    target: Address<FloodMsg>,
}

impl Isolate for ProbeSource {
    tina::isolate_types! {
        message: FloodMsg,
        reply: (),
        send: Outbound<FloodMsg>,
        spawn: Infallible,
        io: RuntimeCall<FloodMsg>,
        shard: AppShard,
    }

    fn handle(
        &mut self,
        msg: Self::Message,
        _ctx: &mut Context<'_, Self::Shard, Self::Reply>,
    ) -> Effect<Self> {
        match msg {
            FloodMsg::Tick { .. } => send(self.target, FloodMsg::Probe),
            FloodMsg::Hit | FloodMsg::Probe => noop(),
        }
    }
}

#[derive(Debug)]
enum TimerMsg {
    Start,
    Done,
}

struct LongTimer;

#[tina_runtime::isolate(message = TimerMsg, shard = AppShard)]
impl LongTimer {
    fn handle(
        &mut self,
        msg: TimerMsg,
        _ctx: &mut Context<'_, AppShard, Self::Reply>,
    ) -> Effect<Self> {
        match msg {
            TimerMsg::Start => sleep(Duration::from_secs(30)).then_event(|| TimerMsg::Done),
            TimerMsg::Done => noop(),
        }
    }
}

#[derive(Debug)]
enum ReplyFloodMsg {
    Start,
    Returned(CallOutcome<ReplyFloodReply>),
}

#[derive(Debug)]
enum ReplyFloodWorkerMsg {
    Ping,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ReplyFloodReply;

struct ReplyFloodCaller {
    worker: Address<ReplyFloodWorkerMsg, ReplyFloodReply>,
    running: Arc<std::sync::atomic::AtomicBool>,
    replies: Arc<AtomicUsize>,
}

#[tina_runtime::isolate(
    message = ReplyFloodMsg,
    send = Outbound<ReplyFloodWorkerMsg>,
    shard = AppShard
)]
impl ReplyFloodCaller {
    fn handle(
        &mut self,
        msg: ReplyFloodMsg,
        _ctx: &mut Context<'_, AppShard, Self::Reply>,
    ) -> Effect<Self> {
        match msg {
            ReplyFloodMsg::Start => self.call_worker(),
            ReplyFloodMsg::Returned(outcome) => {
                assert!(
                    matches!(outcome, CallOutcome::Replied(ReplyFloodReply)),
                    "reply flood call should stay on the terminal reply path"
                );
                self.replies.fetch_add(1, Ordering::Relaxed);
                self.call_worker()
            }
        }
    }
}

impl ReplyFloodCaller {
    fn call_worker(&self) -> Effect<Self> {
        if !self.running.load(Ordering::Relaxed) {
            return noop();
        }
        call(
            self.worker,
            ReplyFloodWorkerMsg::Ping,
            Duration::from_secs(1),
        )
        .then(ReplyFloodMsg::Returned)
    }
}

struct ReplyFloodWorker;

#[tina_runtime::isolate(
    message = ReplyFloodWorkerMsg,
    reply = ReplyFloodReply,
    shard = AppShard
)]
impl ReplyFloodWorker {
    fn handle(
        &mut self,
        _msg: ReplyFloodWorkerMsg,
        _ctx: &mut Context<'_, AppShard, Self::Reply>,
    ) -> Effect<Self> {
        noop()
    }

    fn handle_call(
        &mut self,
        _msg: ReplyFloodWorkerMsg,
        call: CallContext<'_, Self>,
    ) -> Effect<Self> {
        call.reply(ReplyFloodReply)
    }
}

fn make_runtime() -> ThreadedMultiShardRuntime<AppShard, WorkerMailboxFactory> {
    ThreadedMultiShardRuntime::new([AppShard(11), AppShard(22)], WorkerMailboxFactory)
}

fn make_runtime_with_config<const N: usize>(
    shards: [AppShard; N],
    config: ThreadedRuntimeConfig,
) -> ThreadedMultiShardRuntime<AppShard, WorkerMailboxFactory> {
    ThreadedMultiShardRuntime::with_config(shards, WorkerMailboxFactory, config)
}

fn shard_park_wakeups(
    runtime: &ThreadedMultiShardRuntime<AppShard, WorkerMailboxFactory>,
    shard: ShardId,
) -> u64 {
    runtime
        .park_wakeups_on(shard)
        .expect("shard park wakeup metric")
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

#[test]
fn pending_timer_parks_multishard_worker_instead_of_spinning() {
    let config = ThreadedRuntimeConfig {
        idle_wait: Duration::from_secs(1),
        idle_repoll_interval: Duration::from_millis(10),
        ..ThreadedRuntimeConfig::default()
    };
    let runtime = make_runtime_with_config([AppShard(11), AppShard(22)], config);
    let timer = runtime
        .register_with_capacity_on::<LongTimer, _>(ShardId::new(11), LongTimer, 8)
        .expect("register timer");

    runtime
        .try_send(timer, TimerMsg::Start)
        .expect("start long timer");

    std::thread::sleep(Duration::from_millis(30));
    let before = shard_park_wakeups(&runtime, ShardId::new(11));
    std::thread::sleep(Duration::from_millis(180));
    let delta = shard_park_wakeups(&runtime, ShardId::new(11)) - before;

    assert!(
        (5..=40).contains(&delta),
        "pending timer should use bounded parks, not spin or sleep forever: {delta} wakeups"
    );

    runtime.shutdown().expect("shutdown");
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
    let probes = Arc::new(AtomicUsize::new(0));

    let sink = runtime
        .register_with_capacity_on::<Sink, _>(
            ShardId::new(22),
            Sink {
                hits: Arc::clone(&hits),
                probes: Arc::clone(&probes),
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
    let probes = Arc::new(AtomicUsize::new(0));

    let sink = runtime
        .register_with_capacity_on::<Sink, _>(
            ShardId::new(22),
            Sink {
                hits: Arc::clone(&hits),
                probes: Arc::clone(&probes),
            },
            128,
        )
        .expect("register sink");

    let running = Arc::new(std::sync::atomic::AtomicBool::new(true));
    register_flood(&runtime, sink, &running, FLOOD_SOURCES, 128);

    // Wait until the flood is actively in flight.
    std::thread::sleep(Duration::from_millis(10));
    assert!(hits.load(Ordering::Relaxed) > 0, "flood must be in flight");

    let start = Instant::now();
    let trace = runtime.shutdown().expect("shutdown under remote flood");
    let shutdown_latency = start.elapsed();
    assert!(
        trace.iter().any(|event| event.shard() == ShardId::new(11)),
        "source shard trace should be present after shutdown"
    );
    assert!(
        trace.iter().any(|event| event.shard() == ShardId::new(22)),
        "sink shard trace should be present after shutdown"
    );
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
    const N: usize = 500;
    // The producer fires N fire-and-forget cross-shard sends with no
    // per-round sleep, so it can race ahead of the consumer. Size the
    // cross-shard pair queue to hold the whole finite burst; otherwise
    // the unthrottled producer overruns the steady-state default (64)
    // and the overflow lands as typed SendRejected{Full}, not at the
    // sink. The default only sufficed while the removed 1ms sleep
    // happened to pace the producer (also why this test was macOS-flaky).
    let config = ThreadedRuntimeConfig {
        shard_pair_capacity: N + 16,
        ..ThreadedRuntimeConfig::default()
    };
    let runtime = ThreadedMultiShardRuntime::with_config(
        [AppShard(11), AppShard(22)],
        WorkerMailboxFactory,
        config,
    );
    let hits = Arc::new(AtomicUsize::new(0));
    let probes = Arc::new(AtomicUsize::new(0));

    let sink = runtime
        .register_with_capacity_on::<Sink, _>(
            ShardId::new(22),
            Sink {
                hits: Arc::clone(&hits),
                probes: Arc::clone(&probes),
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

    // Every cross-shard message produced by the bounded burst should
    // land at the sink. With the fairness change, the worker still
    // drains its remote inbound budget on every pass, so ordinary
    // throughput makes progress. Keep this as a delivery proof, not a
    // local-machine latency assertion: macOS scheduler noise has made
    // sub-second wall-clock thresholds flap even when all messages
    // arrived well inside the bounded deadline.
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline && hits.load(Ordering::Relaxed) < expected {
        std::thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(
        hits.load(Ordering::Relaxed),
        expected,
        "fairness change must not regress finite cross-shard throughput"
    );

    running.store(false, Ordering::Relaxed);
    let _ = runtime.shutdown();
}

/// Adversarial review I4: remote inbound draining used a single shared
/// budget in fixed source order. A lower-id source could consume the
/// whole target-shard drain budget forever while a higher-id source's
/// already-queued envelope waited behind it.
#[test]
fn remote_inbound_drain_rotates_between_sources_under_flood() {
    let config = ThreadedRuntimeConfig {
        remote_inbound_drain_budget: 1,
        shard_pair_capacity: 64,
        command_capacity: 256,
        ..ThreadedRuntimeConfig::default()
    };
    let runtime = make_runtime_with_config([AppShard(11), AppShard(22), AppShard(33)], config);
    let hits = Arc::new(AtomicUsize::new(0));
    let probes = Arc::new(AtomicUsize::new(0));

    let sink = runtime
        .register_with_capacity_on::<Sink, _>(
            ShardId::new(33),
            Sink {
                hits: Arc::clone(&hits),
                probes: Arc::clone(&probes),
            },
            4096,
        )
        .expect("register target sink");

    let running = Arc::new(std::sync::atomic::AtomicBool::new(true));
    register_flood(&runtime, sink, &running, FLOOD_SOURCES, FLOOD_TICKS);
    std::thread::sleep(Duration::from_millis(50));
    assert!(
        hits.load(Ordering::Relaxed) > 0,
        "lower-id flood must be in flight"
    );

    let probe = runtime
        .register_with_capacity_on::<ProbeSource, _>(
            ShardId::new(22),
            ProbeSource { target: sink },
            8,
        )
        .expect("register probe source");
    runtime
        .try_send(probe, FloodMsg::Tick { remaining: 1 })
        .expect("kick probe");

    let deadline = Instant::now() + Duration::from_millis(750);
    while Instant::now() < deadline && probes.load(Ordering::Relaxed) == 0 {
        std::thread::sleep(Duration::from_millis(5));
    }
    let observed_probes = probes.load(Ordering::Relaxed);
    running.store(false, Ordering::Relaxed);
    let _ = runtime.shutdown();
    assert_eq!(
        observed_probes, 1,
        "source 22 probe starved behind source 11 remote flood"
    );
}

/// Adversarial review C3: terminal replies and ordinary remote sends are
/// separate inbound traffic classes. A steady reply flood into shard 22 must
/// not consume every remote-drain pass and leave ordinary sends stuck until the
/// reply flood ends.
#[test]
fn terminal_reply_flood_does_not_starve_ordinary_remote_sends() {
    let runtime = make_runtime_with_config(
        [AppShard(11), AppShard(22)],
        ThreadedRuntimeConfig {
            command_capacity: 256,
            shard_pair_capacity: 256,
            remote_inbound_drain_budget: 1,
            ..ThreadedRuntimeConfig::default()
        },
    );
    let hits = Arc::new(AtomicUsize::new(0));
    let probes = Arc::new(AtomicUsize::new(0));
    let replies = Arc::new(AtomicUsize::new(0));
    let running = Arc::new(std::sync::atomic::AtomicBool::new(true));

    let sink = runtime
        .register_with_capacity_on::<Sink, _>(
            ShardId::new(22),
            Sink {
                hits: Arc::clone(&hits),
                probes: Arc::clone(&probes),
            },
            128,
        )
        .expect("register sink");
    let worker = runtime
        .register_with_capacity_on::<ReplyFloodWorker, _>(ShardId::new(11), ReplyFloodWorker, 512)
        .expect("register reply worker");

    for _ in 0..64 {
        let caller = runtime
            .register_with_capacity_on::<ReplyFloodCaller, _>(
                ShardId::new(22),
                ReplyFloodCaller {
                    worker,
                    running: Arc::clone(&running),
                    replies: Arc::clone(&replies),
                },
                128,
            )
            .expect("register reply flood caller");
        runtime
            .try_send(caller, ReplyFloodMsg::Start)
            .expect("start reply flood caller");
    }

    let flood_deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < flood_deadline && replies.load(Ordering::Relaxed) < 128 {
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(
        replies.load(Ordering::Relaxed) >= 128,
        "reply flood must be active before probing ordinary traffic"
    );

    let source = runtime
        .register_with_capacity_on::<FloodSource, _>(
            ShardId::new(11),
            FloodSource {
                target: sink,
                running: Arc::clone(&running),
                fanout: 1,
            },
            8,
        )
        .expect("register ordinary source");
    runtime
        .try_send(source, FloodMsg::Tick { remaining: 1 })
        .expect("kick ordinary probe");

    let ordinary_deadline = Instant::now() + Duration::from_millis(750);
    while Instant::now() < ordinary_deadline && hits.load(Ordering::Relaxed) == 0 {
        std::thread::sleep(Duration::from_millis(5));
    }
    let observed_hits = hits.load(Ordering::Relaxed);
    running.store(false, Ordering::Relaxed);
    std::thread::sleep(Duration::from_millis(100));
    let _ = runtime.shutdown();
    assert_eq!(
        observed_hits, 1,
        "ordinary remote send should land while terminal replies are still flooding"
    );
}
