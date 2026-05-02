use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::convert::Infallible;
use std::rc::Rc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use tina::{Mailbox, TrySendError, prelude::*};
use tina_runtime::{MailboxFactory, MultiShardRuntime};

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

    assert!(
        hot_path.allocations > 0 || hot_path.reallocations > 0,
        "if the multi-shard runtime path becomes allocation-free, update the runtime allocation claim"
    );
}
