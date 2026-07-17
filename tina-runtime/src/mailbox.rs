//! Runtime-owned mailbox factory and the default mailbox implementations.

use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::rc::Rc;
use std::sync::Arc;

use tina::{Mailbox, MailboxReservationError, TrySendError};

/// Runtime-owned mailbox factory for registered and spawned isolate mailboxes.
///
/// The factory lives in `tina-runtime`, not in `tina`, because child
/// mailbox allocation is a runtime concern rather than a trait-crate concern.
pub trait MailboxFactory {
    /// Creates one mailbox with the requested capacity.
    ///
    /// The type parameter is the runtime's storage type for that mailbox.
    /// Runtime-created isolate mailboxes may store erased message boxes
    /// internally even when user code handles a concrete message type.
    /// User-provided mailboxes passed to [`crate::Runtime::register`] remain typed
    /// by the isolate message type.
    fn create<T: 'static>(&self, capacity: usize) -> Box<dyn Mailbox<T>>;
}

/// Blessed in-process bounded mailbox factory for single-threaded runtimes.
///
/// Backed by `Rc<RefCell<VecDeque<T>>>` with explicit capacity, FIFO order,
/// and idempotent close. It is the obvious thing examples and small services
/// reach for when they do not need a custom mailbox implementation. Custom
/// factories still work; this is a default, not a requirement.
///
/// `DefaultMailboxFactory` is `!Send`. Use [`DefaultThreadedMailboxFactory`]
/// when constructing a [`crate::ThreadedRuntime`] or any runtime that requires a
/// `Send + 'static` factory.
#[derive(Debug, Default, Clone, Copy)]
pub struct DefaultMailboxFactory;

impl MailboxFactory for DefaultMailboxFactory {
    fn create<T: 'static>(&self, capacity: usize) -> Box<dyn Mailbox<T>> {
        Box::new(DefaultMailbox::new(capacity))
    }
}

pub(crate) struct DefaultMailbox<T> {
    capacity: usize,
    queue: Rc<RefCell<VecDeque<T>>>,
    reserved: Rc<Cell<usize>>,
    closed: Rc<Cell<bool>>,
}

impl<T> DefaultMailbox<T> {
    fn new(capacity: usize) -> Self {
        Self {
            capacity,
            queue: Rc::new(RefCell::new(VecDeque::with_capacity(capacity))),
            reserved: Rc::new(Cell::new(0)),
            closed: Rc::new(Cell::new(false)),
        }
    }
}

impl<T> Mailbox<T> for DefaultMailbox<T> {
    fn capacity(&self) -> usize {
        self.capacity
    }

    fn try_send(&self, message: T) -> Result<(), TrySendError<T>> {
        if self.closed.get() {
            return Err(TrySendError::Closed(message));
        }
        let mut queue = self.queue.borrow_mut();
        if queue.len().saturating_add(self.reserved.get()) >= self.capacity {
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

    fn is_closed(&self) -> bool {
        self.closed.get()
    }

    fn try_reserve(&self) -> Result<(), MailboxReservationError> {
        if self.closed.get() {
            return Err(MailboxReservationError::Closed);
        }
        if self
            .queue
            .borrow()
            .len()
            .saturating_add(self.reserved.get())
            >= self.capacity
        {
            return Err(MailboxReservationError::Full);
        }
        self.reserved.set(self.reserved.get() + 1);
        Ok(())
    }

    fn release_reserved(&self) -> bool {
        let reserved = self.reserved.get();
        if reserved == 0 {
            return false;
        }
        self.reserved.set(reserved - 1);
        true
    }

    fn try_send_reserved(&self, message: T) -> Result<(), TrySendError<T>> {
        if !self.release_reserved() {
            return Err(TrySendError::Full(message));
        }
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
}

/// Blessed in-process bounded mailbox factory for `Send + 'static` runtimes.
///
/// Backed by `Arc<Mutex<VecDeque<T>>>` so the factory and its mailboxes can
/// cross thread boundaries — required by [`crate::ThreadedRuntime`] and other live
/// substrates that move work onto a worker thread. Capacity is explicit, FIFO
/// is preserved, and close is idempotent. Custom factories still work; this
/// is a default for examples and small services.
///
/// The mailbox uses `Mutex` rather than a lock-free queue on purpose: the
/// default keeps observable semantics obvious (one writer wins, one reader
/// wins) and keeps Tina's bounded-and-visible model intact. Workloads that
/// outgrow the mutex should reach for a custom factory.
#[derive(Debug, Default, Clone, Copy)]
pub struct DefaultThreadedMailboxFactory;

impl MailboxFactory for DefaultThreadedMailboxFactory {
    fn create<T: 'static>(&self, capacity: usize) -> Box<dyn Mailbox<T>> {
        Box::new(DefaultThreadedMailbox::new(capacity))
    }
}

pub(crate) struct DefaultThreadedMailbox<T> {
    capacity: usize,
    state: Arc<std::sync::Mutex<DefaultThreadedMailboxState<T>>>,
}

pub(crate) struct DefaultThreadedMailboxState<T> {
    queue: VecDeque<T>,
    reserved: usize,
    closed: bool,
}

impl<T> DefaultThreadedMailbox<T> {
    fn new(capacity: usize) -> Self {
        Self {
            capacity,
            state: Arc::new(std::sync::Mutex::new(DefaultThreadedMailboxState {
                queue: VecDeque::with_capacity(capacity),
                reserved: 0,
                closed: false,
            })),
        }
    }
}

impl<T> Mailbox<T> for DefaultThreadedMailbox<T> {
    fn capacity(&self) -> usize {
        self.capacity
    }

    fn try_send(&self, message: T) -> Result<(), TrySendError<T>> {
        let mut state = self
            .state
            .lock()
            .expect("DefaultThreadedMailbox mutex poisoned");
        if state.closed {
            return Err(TrySendError::Closed(message));
        }
        if state.queue.len().saturating_add(state.reserved) >= self.capacity {
            return Err(TrySendError::Full(message));
        }
        state.queue.push_back(message);
        Ok(())
    }

    fn recv(&self) -> Option<T> {
        let mut state = self
            .state
            .lock()
            .expect("DefaultThreadedMailbox mutex poisoned");
        state.queue.pop_front()
    }

    fn is_empty(&self) -> bool {
        self.state
            .lock()
            .expect("DefaultThreadedMailbox mutex poisoned")
            .queue
            .is_empty()
    }

    fn close(&self) {
        let mut state = self
            .state
            .lock()
            .expect("DefaultThreadedMailbox mutex poisoned");
        state.closed = true;
    }

    fn is_closed(&self) -> bool {
        self.state
            .lock()
            .expect("DefaultThreadedMailbox mutex poisoned")
            .closed
    }

    fn try_reserve(&self) -> Result<(), MailboxReservationError> {
        let mut state = self
            .state
            .lock()
            .expect("DefaultThreadedMailbox mutex poisoned");
        if state.closed {
            return Err(MailboxReservationError::Closed);
        }
        if state.queue.len().saturating_add(state.reserved) >= self.capacity {
            return Err(MailboxReservationError::Full);
        }
        state.reserved += 1;
        Ok(())
    }

    fn release_reserved(&self) -> bool {
        let mut state = self
            .state
            .lock()
            .expect("DefaultThreadedMailbox mutex poisoned");
        if state.reserved == 0 {
            return false;
        }
        state.reserved -= 1;
        true
    }

    fn try_send_reserved(&self, message: T) -> Result<(), TrySendError<T>> {
        let mut state = self
            .state
            .lock()
            .expect("DefaultThreadedMailbox mutex poisoned");
        if state.reserved == 0 {
            return Err(TrySendError::Full(message));
        }
        state.reserved -= 1;
        if state.closed {
            return Err(TrySendError::Closed(message));
        }
        if state.queue.len() >= self.capacity {
            return Err(TrySendError::Full(message));
        }
        state.queue.push_back(message);
        Ok(())
    }
}
