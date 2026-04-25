use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use tina::{Mailbox, TrySendError};
use tina_mailbox_spsc::SpscMailbox;

struct CountingAllocator;

static ALLOCATIONS: AtomicUsize = AtomicUsize::new(0);
static DEALLOCATIONS: AtomicUsize = AtomicUsize::new(0);
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
        if MEASURING_THIS_THREAD.with(Cell::get) {
            DEALLOCATIONS.fetch_add(1, Ordering::SeqCst);
        }
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
    deallocations: usize,
    reallocations: usize,
}

fn reset_allocator_counters() {
    ALLOCATIONS.store(0, Ordering::SeqCst);
    DEALLOCATIONS.store(0, Ordering::SeqCst);
    REALLOCATIONS.store(0, Ordering::SeqCst);
}

fn start_measurement() {
    reset_allocator_counters();
    MEASURING_THIS_THREAD.with(|measuring| measuring.set(true));
}

fn stop_measurement() {
    MEASURING_THIS_THREAD.with(|measuring| measuring.set(false));
}

fn measure_allocations<F>(f: F) -> AllocationSnapshot
where
    F: FnOnce(),
{
    start_measurement();
    f();
    stop_measurement();
    allocator_snapshot()
}

fn allocator_snapshot() -> AllocationSnapshot {
    AllocationSnapshot {
        allocations: ALLOCATIONS.load(Ordering::SeqCst),
        deallocations: DEALLOCATIONS.load(Ordering::SeqCst),
        reallocations: REALLOCATIONS.load(Ordering::SeqCst),
    }
}

#[test]
fn steady_state_send_recv_path_does_not_allocate_after_warmup() {
    let _guard = ALLOCATION_TEST_GUARD
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let mailbox = SpscMailbox::new(8);

    let control = measure_allocations(|| {
        let mut checksum = 0u64;

        for value in 0u64..1024 {
            checksum ^= value;
        }

        std::hint::black_box(checksum);
    });

    let hot_path = measure_allocations(|| {
        let mut checksum = 0u64;

        for value in 0u64..1024 {
            match mailbox.try_send(value) {
                Ok(()) => {}
                Err(error) => panic!("unexpected send error during hot path test: {error:?}"),
            }

            let observed = mailbox
                .recv()
                .expect("hot path should be able to read the just-sent value");
            assert_eq!(observed, value);
            checksum ^= observed;
        }

        std::hint::black_box(checksum);
    });

    assert_eq!(control.allocations, 0);
    assert_eq!(control.reallocations, 0);
    assert_eq!(hot_path.allocations, control.allocations);
    assert_eq!(hot_path.reallocations, control.reallocations);
}

#[test]
fn full_and_closed_error_paths_do_not_allocate() {
    let _guard = ALLOCATION_TEST_GUARD
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let full_mailbox = SpscMailbox::new(1);
    assert_eq!(full_mailbox.try_send(1u64), Ok(()));

    let full_path = measure_allocations(|| {
        assert_eq!(full_mailbox.try_send(2u64), Err(TrySendError::Full(2)));
    });
    assert_eq!(
        full_path,
        AllocationSnapshot {
            allocations: 0,
            deallocations: 0,
            reallocations: 0,
        }
    );

    let closed_mailbox = SpscMailbox::new(1);
    closed_mailbox.close();

    let closed_path = measure_allocations(|| {
        assert_eq!(closed_mailbox.try_send(3u64), Err(TrySendError::Closed(3)));
    });
    assert_eq!(
        closed_path,
        AllocationSnapshot {
            allocations: 0,
            deallocations: 0,
            reallocations: 0,
        }
    );
}
