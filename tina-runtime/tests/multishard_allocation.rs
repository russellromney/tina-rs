#![feature(allocator_api)]

use std::alloc::Global;
use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::convert::Infallible;
use std::net::SocketAddr;
use std::rc::Rc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use betelgeuse::io::simulated::{SimulatedIO, SimulatedPeer};
use tina::{ChildDefinition, Mailbox, RestartableChildDefinition, TrySendError, prelude::*};
use tina_runtime::{
    BetelgeuseBackedRuntime, CallInput, CallOutcome, CallOutput, ListenerId, LocalApp,
    MailboxFactory, MultiShardRuntime, Runtime, RuntimeCall, StreamId, call,
};

const EXPECTED_MULTISHARD_HOT_PATH: AllocationSnapshot = AllocationSnapshot {
    allocations: 1,
    reallocations: 0,
};
const EXPECTED_ISOLATE_CALL_HOT_PATH: AllocationSnapshot = AllocationSnapshot {
    allocations: 2,
    reallocations: 0,
};
const EXPECTED_BETELGEUSE_INGRESS_HANDOFF: AllocationSnapshot = AllocationSnapshot {
    allocations: 1,
    reallocations: 0,
};
const EXPECTED_LOCAL_APP_INGRESS_HANDOFF: AllocationSnapshot = AllocationSnapshot {
    allocations: 1,
    reallocations: 0,
};
const EXPECTED_DRIVER_TIMER_HOT_PATH: AllocationSnapshot = AllocationSnapshot {
    allocations: 2,
    reallocations: 0,
};
const EXPECTED_DRIVER_TCP_READ_HOT_PATH: AllocationSnapshot = AllocationSnapshot {
    allocations: 6,
    reallocations: 0,
};
const EXPECTED_DRIVER_TCP_WRITE_HOT_PATH: AllocationSnapshot = AllocationSnapshot {
    allocations: 6,
    reallocations: 0,
};
const EXPECTED_BATCH_TWO_SEND_HOT_PATH: AllocationSnapshot = AllocationSnapshot {
    allocations: 4,
    reallocations: 0,
};
const EXPECTED_SPAWN_HOT_PATH: AllocationSnapshot = AllocationSnapshot {
    allocations: 6,
    reallocations: 0,
};
const EXPECTED_RESTART_HOT_PATH: AllocationSnapshot = AllocationSnapshot {
    allocations: 4,
    reallocations: 0,
};
const EXPECTED_TRACE_PRESSURE_HOT_PATH: AllocationSnapshot = AllocationSnapshot {
    allocations: 16,
    reallocations: 0,
};
const EXPECTED_HIGH_CARDINALITY_IDLE_STEP: AllocationSnapshot = AllocationSnapshot {
    allocations: 0,
    reallocations: 0,
};
const EXPECTED_SINGLE_SHARD_SEND_ROUND_PROGRESS: &[usize] = &[1, 1, 0];
const EXPECTED_MULTISHARD_SEND_ROUND_PROGRESS: &[usize] = &[1, 1, 0];

struct CountingAllocator;

static ALLOCATIONS: AtomicUsize = AtomicUsize::new(0);
static REALLOCATIONS: AtomicUsize = AtomicUsize::new(0);
static ALLOCATION_TEST_GUARD: Mutex<()> = Mutex::new(());

#[global_allocator]
static GLOBAL_ALLOCATOR: CountingAllocator = CountingAllocator;

thread_local! {
    static MEASURING_THIS_THREAD: Cell<bool> = const { Cell::new(false) };
}

unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        if MEASURING_THIS_THREAD.with(Cell::get) {
            ALLOCATIONS.fetch_add(1, Ordering::SeqCst);
        }
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        if MEASURING_THIS_THREAD.with(Cell::get) {
            REALLOCATIONS.fetch_add(1, Ordering::SeqCst);
        }
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct AllocationSnapshot {
    allocations: usize,
    reallocations: usize,
}

fn measure_allocations<F>(f: F) -> AllocationSnapshot
where
    F: FnOnce(),
{
    ALLOCATIONS.store(0, Ordering::SeqCst);
    REALLOCATIONS.store(0, Ordering::SeqCst);
    MEASURING_THIS_THREAD.with(|measuring| measuring.set(true));
    f();
    MEASURING_THIS_THREAD.with(|measuring| measuring.set(false));
    AllocationSnapshot {
        allocations: ALLOCATIONS.load(Ordering::SeqCst),
        reallocations: REALLOCATIONS.load(Ordering::SeqCst),
    }
}

fn collect_round_progress<F>(mut step: F) -> Vec<usize>
where
    F: FnMut() -> usize,
{
    let mut progress = Vec::new();
    for _ in 0..8 {
        let delivered = step();
        progress.push(delivered);
        if delivered == 0 {
            return progress;
        }
    }
    panic!("runtime did not quiesce within bounded progress probe: {progress:?}");
}

fn wait_until(mut predicate: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < deadline {
        if predicate() {
            return;
        }
        std::thread::yield_now();
    }
    assert!(predicate(), "condition did not become true before timeout");
}

#[derive(Debug, Clone, Copy)]
struct AllocationShard(u32);

impl Shard for AllocationShard {
    fn id(&self) -> ShardId {
        ShardId::new(self.0)
    }
}

struct TestMailbox<T> {
    capacity: usize,
    queue: Rc<RefCell<VecDeque<T>>>,
    closed: Rc<Cell<bool>>,
}

impl<T> TestMailbox<T> {
    fn new(capacity: usize) -> Self {
        Self {
            capacity,
            queue: Rc::new(RefCell::new(VecDeque::new())),
            closed: Rc::new(Cell::new(false)),
        }
    }
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

    fn close(&self) {
        self.closed.set(true);
    }
}

#[derive(Debug, Clone, Copy)]
struct TestMailboxFactory;

impl MailboxFactory for TestMailboxFactory {
    fn create<T: 'static>(&self, capacity: usize) -> Box<dyn Mailbox<T>> {
        Box::new(TestMailbox::new(capacity))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AllocationEvent {
    Kick,
    Arrived,
}

#[derive(Debug)]
struct AllocationSender {
    target: Address<AllocationEvent>,
}

#[derive(Debug)]
struct AllocationSink;

#[derive(Debug)]
struct BatchSender {
    target: Address<AllocationEvent>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SpawnCostMsg {
    Spawn,
    Restart,
}

#[derive(Debug)]
struct SpawnCostParent;

#[derive(Debug)]
struct SpawnCostChild;

impl Isolate for AllocationSender {
    tina::isolate_types! {
        message: AllocationEvent,
        reply: (),
        send: Outbound<AllocationEvent>,
        spawn: Infallible,
        call: Infallible,
        shard: AllocationShard,
    }

    fn handle(&mut self, msg: Self::Message, _ctx: &mut Context<'_, Self::Shard>) -> Effect<Self> {
        match msg {
            AllocationEvent::Kick => send(self.target, AllocationEvent::Arrived),
            AllocationEvent::Arrived => noop(),
        }
    }
}

impl Isolate for AllocationSink {
    tina::isolate_types! {
        message: AllocationEvent,
        reply: (),
        send: Outbound<Infallible>,
        spawn: Infallible,
        call: Infallible,
        shard: AllocationShard,
    }

    fn handle(&mut self, _msg: Self::Message, _ctx: &mut Context<'_, Self::Shard>) -> Effect<Self> {
        noop()
    }
}

impl Isolate for BatchSender {
    tina::isolate_types! {
        message: AllocationEvent,
        reply: (),
        send: Outbound<AllocationEvent>,
        spawn: Infallible,
        call: Infallible,
        shard: AllocationShard,
    }

    fn handle(&mut self, msg: Self::Message, _ctx: &mut Context<'_, Self::Shard>) -> Effect<Self> {
        match msg {
            AllocationEvent::Kick => batch(vec![
                send(self.target, AllocationEvent::Arrived),
                send(self.target, AllocationEvent::Arrived),
            ]),
            AllocationEvent::Arrived => noop(),
        }
    }
}

impl Isolate for SpawnCostParent {
    tina::isolate_types! {
        message: SpawnCostMsg,
        reply: (),
        send: Outbound<Infallible>,
        spawn: RestartableChildDefinition<SpawnCostChild>,
        call: Infallible,
        shard: AllocationShard,
    }

    fn handle(&mut self, msg: Self::Message, _ctx: &mut Context<'_, Self::Shard>) -> Effect<Self> {
        match msg {
            SpawnCostMsg::Spawn => spawn(RestartableChildDefinition::new(|| SpawnCostChild, 8)),
            SpawnCostMsg::Restart => restart_children(),
        }
    }
}

impl Isolate for SpawnCostChild {
    tina::isolate_types! {
        message: SpawnCostMsg,
        reply: (),
        send: Outbound<Infallible>,
        spawn: ChildDefinition<AllocationSink>,
        call: Infallible,
        shard: AllocationShard,
    }

    fn handle(&mut self, msg: Self::Message, _ctx: &mut Context<'_, Self::Shard>) -> Effect<Self> {
        match msg {
            SpawnCostMsg::Spawn | SpawnCostMsg::Restart => noop(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CallRequest {
    Ask,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct CallReply;

#[derive(Debug, Clone, PartialEq, Eq)]
enum CallClientMsg {
    Start(Address<CallRequest, CallReply>),
    Returned(CallOutcome<CallReply>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TimerMsg {
    Start,
    Fired,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum TcpCostMsg {
    Bind,
    Bound {
        listener: ListenerId,
        addr: SocketAddr,
    },
    Accept,
    Accepted(StreamId),
    Read,
    ReadDone(usize),
    Write,
    Wrote(usize),
}

#[derive(Debug)]
struct CallTarget;

#[derive(Debug)]
struct CallClient;

#[derive(Debug)]
struct TimerClient;

#[derive(Debug)]
struct TcpCostClient {
    bind_addr: SocketAddr,
    listener: Option<ListenerId>,
    stream: Option<StreamId>,
    addr_slot: Rc<RefCell<Option<SocketAddr>>>,
    stream_slot: Rc<Cell<Option<StreamId>>>,
    log: Rc<RefCell<Vec<TcpCostMsg>>>,
}

type TcpCostSetup = (
    Runtime<AllocationShard, TestMailboxFactory>,
    Address<TcpCostMsg>,
    SimulatedPeer,
    Rc<RefCell<Vec<TcpCostMsg>>>,
);

impl Isolate for CallTarget {
    tina::isolate_types! {
        message: CallRequest,
        reply: CallReply,
        send: Outbound<Infallible>,
        spawn: Infallible,
        call: RuntimeCall<CallRequest>,
        shard: AllocationShard,
    }

    fn handle(&mut self, _msg: Self::Message, _ctx: &mut Context<'_, Self::Shard>) -> Effect<Self> {
        reply(CallReply)
    }
}

impl Isolate for CallClient {
    tina::isolate_types! {
        message: CallClientMsg,
        reply: (),
        send: Outbound<Infallible>,
        spawn: Infallible,
        call: RuntimeCall<CallClientMsg>,
        shard: AllocationShard,
    }

    fn handle(&mut self, msg: Self::Message, _ctx: &mut Context<'_, Self::Shard>) -> Effect<Self> {
        match msg {
            CallClientMsg::Start(target) => call(
                target,
                CallRequest::Ask,
                std::time::Duration::from_millis(10),
            )
            .reply(CallClientMsg::Returned),
            CallClientMsg::Returned(_) => noop(),
        }
    }
}

impl Isolate for TimerClient {
    tina::isolate_types! {
        message: TimerMsg,
        reply: (),
        send: Outbound<Infallible>,
        spawn: Infallible,
        call: RuntimeCall<TimerMsg>,
        shard: AllocationShard,
    }

    fn handle(&mut self, msg: Self::Message, _ctx: &mut Context<'_, Self::Shard>) -> Effect<Self> {
        match msg {
            TimerMsg::Start => Effect::Call(RuntimeCall::new(
                CallInput::Sleep {
                    after: Duration::ZERO,
                },
                |result| match result {
                    CallOutput::TimerFired => TimerMsg::Fired,
                    other => panic!("expected timer fired, got {other:?}"),
                },
            )),
            TimerMsg::Fired => noop(),
        }
    }
}

impl Isolate for TcpCostClient {
    tina::isolate_types! {
        message: TcpCostMsg,
        reply: (),
        send: Outbound<Infallible>,
        spawn: Infallible,
        call: RuntimeCall<TcpCostMsg>,
        shard: AllocationShard,
    }

    fn handle(&mut self, msg: Self::Message, _ctx: &mut Context<'_, Self::Shard>) -> Effect<Self> {
        match msg {
            TcpCostMsg::Bind => Effect::Call(RuntimeCall::new(
                CallInput::TcpBind {
                    addr: self.bind_addr,
                },
                |result| match result {
                    CallOutput::TcpBound {
                        listener,
                        local_addr,
                    } => TcpCostMsg::Bound {
                        listener,
                        addr: local_addr,
                    },
                    other => panic!("expected TcpBound, got {other:?}"),
                },
            )),
            TcpCostMsg::Bound { listener, addr } => {
                self.listener = Some(listener);
                *self.addr_slot.borrow_mut() = Some(addr);
                noop()
            }
            TcpCostMsg::Accept => Effect::Call(RuntimeCall::new(
                CallInput::TcpAccept {
                    listener: self.listener.expect("listener bound before accept"),
                },
                |result| match result {
                    CallOutput::TcpAccepted { stream, .. } => TcpCostMsg::Accepted(stream),
                    other => panic!("expected TcpAccepted, got {other:?}"),
                },
            )),
            TcpCostMsg::Accepted(stream) => {
                self.stream = Some(stream);
                self.stream_slot.set(Some(stream));
                noop()
            }
            TcpCostMsg::Read => Effect::Call(RuntimeCall::new(
                CallInput::TcpRead {
                    stream: self.stream.expect("stream accepted before read"),
                    max_len: 16,
                },
                |result| match result {
                    CallOutput::TcpRead { bytes } => TcpCostMsg::ReadDone(bytes.len()),
                    other => panic!("expected TcpRead, got {other:?}"),
                },
            )),
            TcpCostMsg::ReadDone(_) | TcpCostMsg::Wrote(_) => {
                self.log.borrow_mut().push(msg);
                noop()
            }
            TcpCostMsg::Write => Effect::Call(RuntimeCall::new(
                CallInput::TcpWrite {
                    stream: self.stream.expect("stream accepted before write"),
                    bytes: b"cost".to_vec(),
                },
                |result| match result {
                    CallOutput::TcpWrote { count } => TcpCostMsg::Wrote(count),
                    other => panic!("expected TcpWrote, got {other:?}"),
                },
            )),
        }
    }
}

fn drive_until<F>(runtime: &mut Runtime<AllocationShard, TestMailboxFactory>, mut done: F)
where
    F: FnMut() -> bool,
{
    for _ in 0..12 {
        if done() {
            return;
        }
        runtime.step();
    }
    panic!("runtime did not reach expected state in bounded drive");
}

fn setup_tcp_cost_runtime() -> TcpCostSetup {
    let simulated_io = SimulatedIO::new();
    let mut runtime = Runtime::with_betelgeuse_io_loop(
        AllocationShard(11),
        TestMailboxFactory,
        simulated_io.loop_handle(Global),
    );
    let addr_slot = Rc::new(RefCell::new(None));
    let stream_slot = Rc::new(Cell::new(None));
    let log = Rc::new(RefCell::new(Vec::new()));
    let worker = runtime.register_with_capacity::<TcpCostClient, Infallible>(
        TcpCostClient {
            bind_addr: "127.0.0.1:0".parse().expect("bind addr"),
            listener: None,
            stream: None,
            addr_slot: Rc::clone(&addr_slot),
            stream_slot: Rc::clone(&stream_slot),
            log: Rc::clone(&log),
        },
        8,
    );

    runtime.try_send(worker, TcpCostMsg::Bind).unwrap();
    drive_until(&mut runtime, || addr_slot.borrow().is_some());
    let peer = simulated_io
        .connect(
            addr_slot
                .borrow()
                .expect("local addr published by TcpCostClient"),
            Vec::new(),
        )
        .expect("simulated peer connects");

    runtime.try_send(worker, TcpCostMsg::Accept).unwrap();
    drive_until(&mut runtime, || stream_slot.get().is_some());

    (runtime, worker, peer, log)
}

#[test]
fn multishard_runtime_path_still_has_allocations_so_the_claim_stays_narrow() {
    let _guard = ALLOCATION_TEST_GUARD
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let mut runtime = MultiShardRuntime::new(
        [AllocationShard(11), AllocationShard(22)],
        TestMailboxFactory,
    );

    let sink = runtime.register_with_capacity_on::<AllocationSink, Infallible>(
        ShardId::new(22),
        AllocationSink,
        8,
    );
    let sender = runtime.register_with_capacity_on::<AllocationSender, AllocationEvent>(
        ShardId::new(11),
        AllocationSender { target: sink },
        8,
    );

    runtime.try_send(sender, AllocationEvent::Kick).unwrap();
    runtime.step();
    runtime.step();

    runtime.try_send(sender, AllocationEvent::Kick).unwrap();
    let hot_path = measure_allocations(|| {
        runtime.step();
        runtime.step();
    });
    assert_eq!(
        hot_path, EXPECTED_MULTISHARD_HOT_PATH,
        "multi-shard hot path allocation count changed; update the runtime allocation claim"
    );
}

#[test]
fn single_shard_send_round_count_keeps_the_cost_claim_named() {
    let mut runtime = Runtime::new(AllocationShard(11), TestMailboxFactory);
    let sink = runtime.register_with_capacity::<AllocationSink, Infallible>(AllocationSink, 8);
    let sender = runtime.register_with_capacity::<AllocationSender, AllocationEvent>(
        AllocationSender { target: sink },
        8,
    );

    runtime.try_send(sender, AllocationEvent::Kick).unwrap();
    assert_eq!(
        collect_round_progress(|| runtime.step()),
        EXPECTED_SINGLE_SHARD_SEND_ROUND_PROGRESS,
        "single-shard send round count changed; this is operation evidence, not a latency claim"
    );
}

#[test]
fn multishard_send_round_count_keeps_the_cost_claim_named() {
    let mut runtime = MultiShardRuntime::new(
        [AllocationShard(11), AllocationShard(22)],
        TestMailboxFactory,
    );
    let sink = runtime.register_with_capacity_on::<AllocationSink, Infallible>(
        ShardId::new(22),
        AllocationSink,
        8,
    );
    let sender = runtime.register_with_capacity_on::<AllocationSender, AllocationEvent>(
        ShardId::new(11),
        AllocationSender { target: sink },
        8,
    );

    runtime.try_send(sender, AllocationEvent::Kick).unwrap();
    assert_eq!(
        collect_round_progress(|| runtime.step()),
        EXPECTED_MULTISHARD_SEND_ROUND_PROGRESS,
        "multi-shard send round count changed; this names coordinator progress only"
    );
}

#[test]
fn two_send_batch_hot_path_allocation_count_is_pinned_after_warmup() {
    let _guard = ALLOCATION_TEST_GUARD
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let mut runtime = Runtime::new(AllocationShard(11), TestMailboxFactory);
    let sink = runtime.register_with_capacity::<AllocationSink, Infallible>(AllocationSink, 8);
    let sender = runtime
        .register_with_capacity::<BatchSender, AllocationEvent>(BatchSender { target: sink }, 8);

    runtime.try_send(sender, AllocationEvent::Kick).unwrap();
    assert_eq!(runtime.step(), 1);
    assert_eq!(runtime.step(), 1);
    assert_eq!(runtime.step(), 1);

    runtime.try_send(sender, AllocationEvent::Kick).unwrap();
    let hot_path = measure_allocations(|| {
        assert_eq!(runtime.step(), 1);
        assert_eq!(runtime.step(), 1);
        assert_eq!(runtime.step(), 1);
    });
    assert_eq!(
        hot_path, EXPECTED_BATCH_TWO_SEND_HOT_PATH,
        "two-send batch hot path allocation count changed; update the runtime allocation claim"
    );
}

#[test]
fn spawn_hot_path_allocation_count_is_pinned_after_warmup() {
    let _guard = ALLOCATION_TEST_GUARD
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let mut runtime = Runtime::new(AllocationShard(11), TestMailboxFactory);
    let parent = runtime.register_with_capacity::<SpawnCostParent, Infallible>(SpawnCostParent, 8);

    runtime.try_send(parent, SpawnCostMsg::Spawn).unwrap();
    assert_eq!(runtime.step(), 1);

    runtime.try_send(parent, SpawnCostMsg::Spawn).unwrap();
    let hot_path = measure_allocations(|| {
        assert_eq!(runtime.step(), 1);
    });
    assert_eq!(
        hot_path, EXPECTED_SPAWN_HOT_PATH,
        "spawn hot path allocation count changed; update the runtime allocation claim"
    );
}

#[test]
fn direct_restart_hot_path_allocation_count_is_pinned_after_warmup() {
    let _guard = ALLOCATION_TEST_GUARD
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let mut runtime = Runtime::new(AllocationShard(11), TestMailboxFactory);
    let parent = runtime.register_with_capacity::<SpawnCostParent, Infallible>(SpawnCostParent, 8);
    runtime.try_send(parent, SpawnCostMsg::Spawn).unwrap();
    assert_eq!(runtime.step(), 1);

    runtime.try_send(parent, SpawnCostMsg::Restart).unwrap();
    assert_eq!(runtime.step(), 1);

    runtime.try_send(parent, SpawnCostMsg::Restart).unwrap();
    let hot_path = measure_allocations(|| {
        assert_eq!(runtime.step(), 1);
    });
    assert_eq!(
        hot_path, EXPECTED_RESTART_HOT_PATH,
        "direct restart hot path allocation count changed; update the runtime allocation claim"
    );
}

#[test]
fn repeated_trace_event_growth_allocation_count_is_pinned() {
    let _guard = ALLOCATION_TEST_GUARD
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let mut runtime = Runtime::new(AllocationShard(11), TestMailboxFactory);
    let sink = runtime.register_with_capacity::<AllocationSink, Infallible>(AllocationSink, 128);

    for _ in 0..16 {
        runtime.try_send(sink, AllocationEvent::Arrived).unwrap();
        assert_eq!(runtime.step(), 1);
    }

    let hot_path = measure_allocations(|| {
        for _ in 0..16 {
            runtime.try_send(sink, AllocationEvent::Arrived).unwrap();
            assert_eq!(runtime.step(), 1);
        }
    });
    assert_eq!(
        hot_path, EXPECTED_TRACE_PRESSURE_HOT_PATH,
        "trace pressure allocation count changed; update the runtime allocation claim"
    );
}

#[test]
fn high_cardinality_idle_step_reuses_round_message_scratch_after_warmup() {
    let _guard = ALLOCATION_TEST_GUARD
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let mut runtime = Runtime::new(AllocationShard(11), TestMailboxFactory);
    for _ in 0..12 {
        runtime.register_with_capacity::<AllocationSink, Infallible>(AllocationSink, 8);
    }

    assert_eq!(runtime.step(), 0);
    let hot_path = measure_allocations(|| {
        assert_eq!(runtime.step(), 0);
    });
    assert_eq!(
        hot_path, EXPECTED_HIGH_CARDINALITY_IDLE_STEP,
        "high-cardinality idle step should reuse round-message scratch after warm-up"
    );
}

#[test]
fn isolate_call_path_still_has_allocations_so_the_claim_stays_narrow() {
    let _guard = ALLOCATION_TEST_GUARD
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let mut runtime = Runtime::new(AllocationShard(11), TestMailboxFactory);
    let target = runtime.register_with_capacity::<CallTarget, Infallible>(CallTarget, 8);
    let client = runtime.register_with_capacity::<CallClient, Infallible>(CallClient, 8);

    runtime
        .try_send(client, CallClientMsg::Start(target))
        .unwrap();
    assert_eq!(runtime.step(), 1);
    assert_eq!(runtime.step(), 1);
    assert_eq!(runtime.step(), 1);

    runtime
        .try_send(client, CallClientMsg::Start(target))
        .unwrap();
    let hot_path = measure_allocations(|| {
        assert_eq!(runtime.step(), 1);
        assert_eq!(runtime.step(), 1);
        assert_eq!(runtime.step(), 1);
    });
    assert_eq!(
        hot_path, EXPECTED_ISOLATE_CALL_HOT_PATH,
        "isolate-call hot path allocation count changed; update the runtime allocation claim"
    );
}

#[test]
fn betelgeuse_ingress_handoff_allocation_count_is_pinned_on_caller_thread() {
    let _guard = ALLOCATION_TEST_GUARD
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let runtime = BetelgeuseBackedRuntime::new(AllocationShard(11), TestMailboxFactory);
    let sink = runtime
        .register_with_capacity::<AllocationSink, Infallible>(AllocationSink, 8)
        .expect("sink register accepts");

    runtime.try_send(sink, AllocationEvent::Arrived).unwrap();
    while runtime.has_in_flight_calls().unwrap() {
        std::thread::yield_now();
    }

    let handoff = measure_allocations(|| {
        runtime.try_send(sink, AllocationEvent::Arrived).unwrap();
    });
    assert_eq!(
        handoff, EXPECTED_BETELGEUSE_INGRESS_HANDOFF,
        "Betelgeuse ingress handoff allocation count changed; this measures the caller thread only"
    );
    let _ = runtime.shutdown();
}

#[test]
fn local_app_ingress_handoff_allocation_count_is_pinned_on_caller_thread() {
    let _guard = ALLOCATION_TEST_GUARD
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let app = LocalApp::single_shard(AllocationShard(11), TestMailboxFactory)
        .ingress_capacity(8)
        .build();
    let sink = app
        .register_root::<AllocationSink, Infallible>(AllocationSink, 8)
        .expect("sink register accepts");

    let warmup_trace_len = app.trace().expect("trace snapshot").len();
    app.try_send(sink, AllocationEvent::Arrived).unwrap();
    wait_until(|| app.trace().expect("trace snapshot").len() > warmup_trace_len);

    let handoff = measure_allocations(|| {
        app.try_send(sink, AllocationEvent::Arrived).unwrap();
    });
    assert_eq!(
        handoff, EXPECTED_LOCAL_APP_INGRESS_HANDOFF,
        "LocalApp ingress handoff allocation count changed; this measures the caller thread only"
    );
    let _ = app.shutdown().drain().join();
}

#[test]
fn driver_timer_hot_path_allocation_count_is_pinned_after_warmup() {
    let _guard = ALLOCATION_TEST_GUARD
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let mut runtime = Runtime::new(AllocationShard(11), TestMailboxFactory);
    let timer = runtime.register_with_capacity::<TimerClient, Infallible>(TimerClient, 8);

    runtime.try_send(timer, TimerMsg::Start).unwrap();
    assert_eq!(runtime.step(), 1);
    assert_eq!(runtime.step(), 1);

    runtime.try_send(timer, TimerMsg::Start).unwrap();
    let hot_path = measure_allocations(|| {
        assert_eq!(runtime.step(), 1);
        assert_eq!(runtime.step(), 1);
    });
    assert_eq!(
        hot_path, EXPECTED_DRIVER_TIMER_HOT_PATH,
        "driver timer hot path allocation count changed; update the runtime allocation claim"
    );
}

#[test]
fn driver_tcp_read_hot_path_allocation_count_is_pinned_after_warmup() {
    let _guard = ALLOCATION_TEST_GUARD
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let (mut runtime, worker, peer, log) = setup_tcp_cost_runtime();

    peer.push_input(b"warm");
    runtime.try_send(worker, TcpCostMsg::Read).unwrap();
    drive_until(&mut runtime, || log.borrow().len() == 1);

    peer.push_input(b"cost");
    runtime.try_send(worker, TcpCostMsg::Read).unwrap();
    let hot_path = measure_allocations(|| {
        drive_until(&mut runtime, || log.borrow().len() == 2);
    });
    assert_eq!(
        hot_path, EXPECTED_DRIVER_TCP_READ_HOT_PATH,
        "driver TCP read hot path allocation count changed; update the runtime allocation claim"
    );
}

#[test]
fn driver_tcp_write_hot_path_allocation_count_is_pinned_after_warmup() {
    let _guard = ALLOCATION_TEST_GUARD
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let (mut runtime, worker, _peer, log) = setup_tcp_cost_runtime();

    runtime.try_send(worker, TcpCostMsg::Write).unwrap();
    drive_until(&mut runtime, || log.borrow().len() == 1);

    runtime.try_send(worker, TcpCostMsg::Write).unwrap();
    let hot_path = measure_allocations(|| {
        drive_until(&mut runtime, || log.borrow().len() == 2);
    });
    assert_eq!(
        hot_path, EXPECTED_DRIVER_TCP_WRITE_HOT_PATH,
        "driver TCP write hot path allocation count changed; update the runtime allocation claim"
    );
}
