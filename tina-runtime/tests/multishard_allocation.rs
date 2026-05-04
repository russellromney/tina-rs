use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::convert::Infallible;
use std::rc::Rc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use tina::{Mailbox, TrySendError, prelude::*};
use tina_runtime::{
    BetelgeuseRuntime, CallInput, CallOutcome, CallOutput, MailboxFactory, MultiShardRuntime,
    Runtime, RuntimeCall, call,
};

const EXPECTED_MULTISHARD_HOT_PATH: AllocationSnapshot = AllocationSnapshot {
    allocations: 15,
    reallocations: 2,
};
const EXPECTED_ISOLATE_CALL_HOT_PATH: AllocationSnapshot = AllocationSnapshot {
    allocations: 9,
    reallocations: 1,
};
const EXPECTED_BETELGEUSE_INGRESS_HANDOFF: AllocationSnapshot = AllocationSnapshot {
    allocations: 1,
    reallocations: 0,
};
const EXPECTED_DRIVER_TIMER_HOT_PATH: AllocationSnapshot = AllocationSnapshot {
    allocations: 10,
    reallocations: 1,
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

#[derive(Debug)]
struct CallTarget;

#[derive(Debug)]
struct CallClient;

#[derive(Debug)]
struct TimerClient;

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
    let runtime = BetelgeuseRuntime::new(AllocationShard(11), TestMailboxFactory);
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
