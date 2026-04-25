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
//! - `close()` does not return while a racing successful send is still
//!   unpublished
//!
//! If more than one producer or more than one consumer enters the mailbox at
//! the same time, the mailbox panics rather than silently widening the
//! concurrency contract.
//!
//! # Implementation Invariants
//!
//! `SpscMailbox` relies on a small set of invariants that the tests and Loom
//! models are meant to defend:
//!
//! - `tail - head <= capacity`
//! - slots in the logical range `[head, tail)` are initialized
//! - slots outside `[head, tail)` are uninitialized and must not be read
//! - each successful send publishes exactly one new initialized slot via the
//!   `tail` release store
//! - each successful recv consumes exactly one initialized slot and advances
//!   `head`
//! - `close()` does not return while a racing successful send is still
//!   unpublished
//! - unread initialized slots are dropped exactly once when the mailbox drops
//!
//! # DST Boundary
//!
//! The ring stores fixed-size slots, so `SpscMailbox<T>` is for sized `T`.
//! Dynamically sized payloads belong behind owning pointers such as `Box<str>`,
//! `Box<[u8]>`, `Arc<str>`, or `Arc<[u8]>`, where the mailbox stores the
//! pointer value rather than trying to store an unsized value inline.

use std::fmt;
use std::mem::MaybeUninit;

use tina::{Mailbox, TrySendError};

use crate::sync::Ordering::{AcqRel, Acquire, Relaxed, Release};
use crate::sync::{AtomicBool, AtomicUsize, UnsafeCell};

#[cfg(feature = "loom")]
mod sync {
    pub(crate) use loom::cell::UnsafeCell;
    pub(crate) use loom::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
}

#[cfg(not(feature = "loom"))]
mod sync {
    pub(crate) use std::cell::UnsafeCell;
    pub(crate) use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
}

/// A bounded single-producer/single-consumer mailbox backed by a preallocated
/// ring buffer.
///
/// Construction allocates the ring buffer once. After warm-up, successful
/// `try_send`/`recv` operations on fixed-size payloads run without per-message
/// allocation.
pub struct SpscMailbox<T> {
    capacity: usize,
    slots: Box<[Slot<T>]>,
    head: AtomicUsize,
    tail: AtomicUsize,
    state: AtomicUsize,
    // Consumer exclusivity does not participate in close/send linearization, so
    // it can stay as a separate guard instead of sharing the producer state
    // word.
    consumer_active: AtomicBool,
}

const STATE_CLOSED: usize = 0b01;
const STATE_PRODUCER_HELD: usize = 0b10;

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
            state: AtomicUsize::new(0),
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

    fn claim_producer(&self) -> Result<ProducerGuard<'_>, ProducerClaimError> {
        loop {
            let state = self.state.load(Acquire);

            if state & STATE_CLOSED != 0 {
                return Err(ProducerClaimError::Closed);
            }

            if state & STATE_PRODUCER_HELD != 0 {
                panic!("SpscMailbox supports only one concurrent producer");
            }

            if self
                .state
                .compare_exchange(state, state | STATE_PRODUCER_HELD, AcqRel, Acquire)
                .is_ok()
            {
                return Ok(ProducerGuard { state: &self.state });
            }
        }
    }
}

impl<T> Mailbox<T> for SpscMailbox<T> {
    fn capacity(&self) -> usize {
        self.capacity
    }

    fn try_send(&self, message: T) -> Result<(), TrySendError<T>> {
        let _producer = match self.claim_producer() {
            Ok(guard) => guard,
            Err(ProducerClaimError::Closed) => return Err(TrySendError::Closed(message)),
        };

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
        loop {
            let state = self.state.load(Acquire);

            if state & STATE_CLOSED != 0 && state & STATE_PRODUCER_HELD == 0 {
                return;
            }

            if state & STATE_PRODUCER_HELD != 0 {
                spin_wait();
                continue;
            }

            if self
                .state
                .compare_exchange(
                    state,
                    state | STATE_CLOSED | STATE_PRODUCER_HELD,
                    AcqRel,
                    Acquire,
                )
                .is_ok()
            {
                break;
            }
        }

        // Release the producer gate after publishing the closed bit so later
        // producer attempts cannot observe an open mailbox after close returns.
        self.state.fetch_and(!STATE_PRODUCER_HELD, Release);
    }
}

impl<T> SpscMailbox<T> {
    fn is_closed(&self) -> bool {
        self.state.load(Relaxed) & STATE_CLOSED != 0
    }
}

impl<T> fmt::Debug for SpscMailbox<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SpscMailbox")
            .field("capacity", &self.capacity)
            .field("head", &self.head.load(Relaxed))
            .field("tail", &self.tail.load(Relaxed))
            .field("closed", &self.is_closed())
            .finish_non_exhaustive()
    }
}

impl<T> Drop for SpscMailbox<T> {
    fn drop(&mut self) {
        let mut cursor = self.head.load(Relaxed);
        let tail = self.tail.load(Relaxed);

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

// Safety: the mailbox uses atomics for coordination, and the producer gate in
// `state` prevents overlapping producer access while still allowing one
// producer and one consumer to operate concurrently. Sharing the mailbox across
// threads is sound as long as stored messages are themselves `Send`.
unsafe impl<T: Send> Send for SpscMailbox<T> {}

// Safety: see the `Send` impl above. The mailbox panics on overlapping
// same-role access instead of permitting an unsound producer or consumer set.
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
        #[cfg(feature = "loom")]
        self.value.with_mut(|ptr| unsafe {
            (*ptr).write(value);
        });

        #[cfg(not(feature = "loom"))]
        unsafe {
            (*self.value.get()).write(value);
        }
    }

    unsafe fn read(&self) -> T {
        // Safety: callers guarantee the slot currently holds an initialized
        // value that is being consumed exactly once.
        #[cfg(feature = "loom")]
        {
            self.value.with(|ptr| unsafe { (*ptr).assume_init_read() })
        }

        #[cfg(not(feature = "loom"))]
        unsafe {
            (*self.value.get()).assume_init_read()
        }
    }

    unsafe fn drop_in_place(&self) {
        // Safety: callers guarantee the slot currently holds an initialized
        // value that has not yet been moved out.
        #[cfg(feature = "loom")]
        self.value.with_mut(|ptr| unsafe {
            (*ptr).assume_init_drop();
        });

        #[cfg(not(feature = "loom"))]
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

struct ProducerGuard<'a> {
    state: &'a AtomicUsize,
}

impl Drop for ProducerGuard<'_> {
    fn drop(&mut self) {
        self.state.fetch_and(!STATE_PRODUCER_HELD, Release);
    }
}

enum ProducerClaimError {
    Closed,
}

#[cfg(feature = "loom")]
fn spin_wait() {
    loom::thread::yield_now();
}

#[cfg(not(feature = "loom"))]
fn spin_wait() {
    std::hint::spin_loop();
}
