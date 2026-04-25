#![deny(missing_docs)]
#![deny(rustdoc::broken_intra_doc_links)]
#![deny(unsafe_op_in_unsafe_fn)]

//! Bounded single-producer/single-consumer mailbox for `tina-rs`.
//!
//! `SpscMailbox` preallocates its ring buffer at construction time and then
//! reuses those slots for every message. Once the mailbox is warm, successful
//! `try_send`/`recv` operations do not allocate per message.
//!
//! This crate focuses on bounded queue semantics first:
//!
//! - FIFO delivery
//! - explicit [`TrySendError::Full`] and [`TrySendError::Closed`] results
//! - no hidden overflow queue
//! - one concurrent producer and one concurrent consumer
//!
//! If more than one producer or more than one consumer enters the mailbox at
//! the same time, the mailbox panics rather than silently widening the
//! concurrency contract.

use std::cell::UnsafeCell;
use std::fmt;
use std::mem::MaybeUninit;
use std::sync::atomic::Ordering::{AcqRel, Acquire, Relaxed, Release};
use std::sync::atomic::{AtomicBool, AtomicUsize};

use tina::{Mailbox, TrySendError};

/// A bounded single-producer/single-consumer mailbox backed by a preallocated
/// ring buffer.
pub struct SpscMailbox<T> {
    capacity: usize,
    slots: Box<[Slot<T>]>,
    head: AtomicUsize,
    tail: AtomicUsize,
    closed: AtomicBool,
    producer_active: AtomicBool,
    consumer_active: AtomicBool,
}

impl<T> SpscMailbox<T> {
    /// Creates a new mailbox with a fixed bounded capacity.
    ///
    /// # Panics
    ///
    /// Panics when `capacity` is zero.
    pub fn new(capacity: usize) -> Self {
        assert!(
            capacity > 0,
            "SpscMailbox capacity must be greater than zero"
        );

        let mut slots = Vec::with_capacity(capacity);
        for _ in 0..capacity {
            slots.push(Slot::new());
        }

        Self {
            capacity,
            slots: slots.into_boxed_slice(),
            head: AtomicUsize::new(0),
            tail: AtomicUsize::new(0),
            closed: AtomicBool::new(false),
            producer_active: AtomicBool::new(false),
            consumer_active: AtomicBool::new(false),
        }
    }

    fn slot(&self, cursor: usize) -> &Slot<T> {
        &self.slots[cursor % self.capacity]
    }

    fn claim<'a>(claimed: &'a AtomicBool, role: &'static str) -> ActiveGuard<'a> {
        if claimed
            .compare_exchange(false, true, AcqRel, Acquire)
            .is_err()
        {
            panic!("SpscMailbox supports only one concurrent {role}");
        }

        ActiveGuard { claimed }
    }
}

impl<T> Mailbox<T> for SpscMailbox<T> {
    fn capacity(&self) -> usize {
        self.capacity
    }

    fn try_send(&self, message: T) -> Result<(), TrySendError<T>> {
        let _producer = Self::claim(&self.producer_active, "producer");

        if self.closed.load(Acquire) {
            return Err(TrySendError::Closed(message));
        }

        let tail = self.tail.load(Relaxed);
        let head = self.head.load(Acquire);

        if tail.wrapping_sub(head) >= self.capacity {
            return Err(TrySendError::Full(message));
        }

        // Safety: the producer claim guarantees exclusive write access among
        // producers, and the queue is known not to be full, so this slot is not
        // concurrently owned by the consumer.
        unsafe {
            self.slot(tail).write(message);
        }
        self.tail.store(tail.wrapping_add(1), Release);

        Ok(())
    }

    fn recv(&self) -> Option<T> {
        let _consumer = Self::claim(&self.consumer_active, "consumer");

        let head = self.head.load(Relaxed);
        let tail = self.tail.load(Acquire);

        if head == tail {
            return None;
        }

        // Safety: the consumer claim guarantees exclusive read access among
        // consumers, and `head != tail` means a producer has fully published a
        // value into this slot.
        let value = unsafe { self.slot(head).read() };
        self.head.store(head.wrapping_add(1), Release);

        Some(value)
    }

    fn close(&self) {
        self.closed.store(true, Release);
    }
}

impl<T> fmt::Debug for SpscMailbox<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SpscMailbox")
            .field("capacity", &self.capacity)
            .field("head", &self.head.load(Relaxed))
            .field("tail", &self.tail.load(Relaxed))
            .field("closed", &self.closed.load(Relaxed))
            .finish_non_exhaustive()
    }
}

impl<T> Drop for SpscMailbox<T> {
    fn drop(&mut self) {
        let mut cursor = *self.head.get_mut();
        let tail = *self.tail.get_mut();

        while cursor != tail {
            let index = cursor % self.capacity;

            // Safety: drop runs with exclusive access to the mailbox, and the
            // range `[head, tail)` contains only initialized elements that have
            // not yet been received.
            unsafe {
                self.slots[index].drop_in_place();
            }

            cursor = cursor.wrapping_add(1);
        }
    }
}

// Safety: the mailbox uses atomics for coordination and runtime-enforced
// producer/consumer claims to prevent overlapping access within the same role.
// Sharing the mailbox across threads is sound as long as stored messages are
// themselves `Send`.
unsafe impl<T: Send> Send for SpscMailbox<T> {}

// Safety: see the `Send` impl above. Concurrent use beyond one producer and
// one consumer panics rather than causing undefined behavior.
unsafe impl<T: Send> Sync for SpscMailbox<T> {}

struct Slot<T> {
    value: UnsafeCell<MaybeUninit<T>>,
}

impl<T> Slot<T> {
    fn new() -> Self {
        Self {
            value: UnsafeCell::new(MaybeUninit::uninit()),
        }
    }

    unsafe fn write(&self, value: T) {
        // Safety: callers guarantee exclusive ownership of the slot for writes.
        unsafe {
            (*self.value.get()).write(value);
        }
    }

    unsafe fn read(&self) -> T {
        // Safety: callers guarantee the slot currently holds an initialized
        // value that is being consumed exactly once.
        unsafe { (*self.value.get()).assume_init_read() }
    }

    unsafe fn drop_in_place(&self) {
        // Safety: callers guarantee the slot currently holds an initialized
        // value that has not yet been moved out.
        unsafe {
            (*self.value.get()).assume_init_drop();
        }
    }
}

struct ActiveGuard<'a> {
    claimed: &'a AtomicBool,
}

impl Drop for ActiveGuard<'_> {
    fn drop(&mut self) {
        self.claimed.store(false, Release);
    }
}
