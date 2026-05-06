//! Typed, bounded host observation handles for runtime facts.
//!
//! Phase 047 Rock 4: replace `Arc<Mutex<Option<SocketAddr>>>` and
//! `Arc<AtomicBool>` side channels in user code with typed handles that the
//! runtime owns and notifies.
//!
//! Four handles ship in this slice:
//!
//! - [`BoundAddressWaiter`] — next `tcp_bind` completion (slice 1).
//! - [`IsolateCompleteWaiter`] — a specific isolate's `IsolateStopped`.
//! - [`OperationDoneWaiter`] — the next runtime call of a chosen kind on
//!   a chosen isolate that completes (success or failure).
//! - [`ChildRestartedWaiter`] — the next supervised restart of a child
//!   spawned by a chosen parent.
//!
//! Each handle is one bounded slot. Handles do not create secret unbounded
//! queues: the runtime also caps the number of pending observation slots.
//! The trace remains the source of audit truth: handles only surface facts
//! the runtime already records into the trace.

use std::collections::VecDeque;
use std::net::SocketAddr;
use std::sync::mpsc::{self, RecvTimeoutError, SyncSender};
use std::time::Duration;

use tina::{AddressGeneration, IsolateId};

use crate::call::{CallError, CallId};
use crate::trace::CallKind;

/// Outcome of a `wait` call on any of this module's typed waiters.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WaitError {
    /// The waiter timed out before the runtime produced an outcome.
    Timeout,
    /// The runtime stopped (or was dropped) before the awaited fact happened.
    RuntimeStopped,
    /// The awaited runtime call failed; the trace recorded the failure.
    CallFailed(CallError),
    /// The runtime already has its configured number of pending host
    /// observations. The fact may still happen and still be traced, but this
    /// waiter was not registered.
    ObservationFull,
}

#[derive(Debug, Clone, Copy)]
pub(crate) enum BoundAddressOutcome {
    Bound(SocketAddr),
    Failed(CallError),
}

#[derive(Debug, Clone, Copy)]
pub(crate) enum OperationDoneOutcome {
    Completed(CallId),
    Failed {
        // call_id is intentionally retained even though current callers only
        // surface the error: it lets us add a `wait_with_call_id` variant
        // later without a payload change.
        #[allow(dead_code)]
        call_id: CallId,
        error: CallError,
    },
}

/// Outcome the host receives when a `ChildRestartedWaiter` resolves.
///
/// Carries the new child incarnation's identity so the host can update any
/// addresses it holds without polling the trace. The runtime currently
/// assigns each fresh restart incarnation a *new* `IsolateId`, so
/// `new_generation` is typically `0` (the initial generation for the
/// freshly allocated isolate) — `new_isolate` is the value to compare
/// against if the host is asking "did the child id change?". Generation is
/// only load-bearing if a future runtime decides to recycle isolate ids.
///
/// `#[non_exhaustive]` reserves the right to add fields (for example, the
/// old isolate id) without breaking downstream pattern matches.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct ChildRestarted {
    /// The stable per-parent child ordinal that was restarted.
    pub child_ordinal: usize,
    /// The new child incarnation's isolate id.
    pub new_isolate: IsolateId,
    /// The new child incarnation's address generation.
    pub new_generation: AddressGeneration,
}

/// Typed, bounded handle that resolves once the runtime completes the *next*
/// `tcp_bind` call and records its bound `SocketAddr`.
#[derive(Debug)]
pub struct BoundAddressWaiter {
    rx: mpsc::Receiver<BoundAddressOutcome>,
    pre_error: Option<WaitError>,
}

impl BoundAddressWaiter {
    pub(crate) fn from_pair() -> (Self, SyncSender<BoundAddressOutcome>) {
        let (tx, rx) = mpsc::sync_channel(1);
        (
            Self {
                rx,
                pre_error: None,
            },
            tx,
        )
    }

    fn with_error(error: WaitError) -> Self {
        let (_tx, rx) = mpsc::sync_channel(1);
        Self {
            rx,
            pre_error: Some(error),
        }
    }

    /// Waits up to `timeout` for the next `tcp_bind` to complete.
    pub fn wait(self, timeout: Duration) -> Result<SocketAddr, WaitError> {
        if let Some(error) = self.pre_error {
            return Err(error);
        }
        match self.rx.recv_timeout(timeout) {
            Ok(BoundAddressOutcome::Bound(addr)) => Ok(addr),
            Ok(BoundAddressOutcome::Failed(error)) => Err(WaitError::CallFailed(error)),
            Err(RecvTimeoutError::Timeout) => Err(WaitError::Timeout),
            Err(RecvTimeoutError::Disconnected) => Err(WaitError::RuntimeStopped),
        }
    }
}

/// Typed, bounded handle that resolves once a specific isolate emits
/// `IsolateStopped` (the runtime's `RuntimeEventKind::IsolateStopped`
/// event). Replaces `Arc<AtomicBool>` done flags in user code.
#[derive(Debug)]
pub struct IsolateCompleteWaiter {
    rx: mpsc::Receiver<()>,
    pre_error: Option<WaitError>,
}

impl IsolateCompleteWaiter {
    pub(crate) fn from_pair() -> (Self, SyncSender<()>) {
        let (tx, rx) = mpsc::sync_channel(1);
        (
            Self {
                rx,
                pre_error: None,
            },
            tx,
        )
    }

    fn with_error(error: WaitError) -> Self {
        let (_tx, rx) = mpsc::sync_channel(1);
        Self {
            rx,
            pre_error: Some(error),
        }
    }

    /// Waits up to `timeout` for the targeted isolate to stop.
    pub fn wait(self, timeout: Duration) -> Result<(), WaitError> {
        if let Some(error) = self.pre_error {
            return Err(error);
        }
        match self.rx.recv_timeout(timeout) {
            Ok(()) => Ok(()),
            Err(RecvTimeoutError::Timeout) => Err(WaitError::Timeout),
            Err(RecvTimeoutError::Disconnected) => Err(WaitError::RuntimeStopped),
        }
    }
}

/// Typed, bounded handle that resolves on the next runtime call of a chosen
/// kind issued by a chosen isolate that completes (success or failure).
#[derive(Debug)]
pub struct OperationDoneWaiter {
    rx: mpsc::Receiver<OperationDoneOutcome>,
    pre_error: Option<WaitError>,
}

impl OperationDoneWaiter {
    pub(crate) fn from_pair() -> (Self, SyncSender<OperationDoneOutcome>) {
        let (tx, rx) = mpsc::sync_channel(1);
        (
            Self {
                rx,
                pre_error: None,
            },
            tx,
        )
    }

    fn with_error(error: WaitError) -> Self {
        let (_tx, rx) = mpsc::sync_channel(1);
        Self {
            rx,
            pre_error: Some(error),
        }
    }

    /// Waits up to `timeout` for the matching call to complete.
    ///
    /// On success the runtime-assigned `CallId` is returned so the host can
    /// match this completion against any context it kept. On failure the
    /// `CallError` is surfaced via [`WaitError::CallFailed`].
    pub fn wait(self, timeout: Duration) -> Result<CallId, WaitError> {
        if let Some(error) = self.pre_error {
            return Err(error);
        }
        match self.rx.recv_timeout(timeout) {
            Ok(OperationDoneOutcome::Completed(call_id)) => Ok(call_id),
            Ok(OperationDoneOutcome::Failed { error, .. }) => Err(WaitError::CallFailed(error)),
            Err(RecvTimeoutError::Timeout) => Err(WaitError::Timeout),
            Err(RecvTimeoutError::Disconnected) => Err(WaitError::RuntimeStopped),
        }
    }
}

/// Typed, bounded handle that resolves on the next supervised restart of a
/// direct child of the chosen parent isolate.
///
/// Replaces the `Arc<Mutex<Option<Address<...>>>>` + `AtomicU64` generation
/// pair in `eiffel_supervised_worker`-style code.
#[derive(Debug)]
pub struct ChildRestartedWaiter {
    rx: mpsc::Receiver<ChildRestarted>,
    pre_error: Option<WaitError>,
}

impl ChildRestartedWaiter {
    pub(crate) fn from_pair() -> (Self, SyncSender<ChildRestarted>) {
        let (tx, rx) = mpsc::sync_channel(1);
        (
            Self {
                rx,
                pre_error: None,
            },
            tx,
        )
    }

    fn with_error(error: WaitError) -> Self {
        let (_tx, rx) = mpsc::sync_channel(1);
        Self {
            rx,
            pre_error: Some(error),
        }
    }

    /// Waits up to `timeout` for the parent's next direct-child restart.
    pub fn wait(self, timeout: Duration) -> Result<ChildRestarted, WaitError> {
        if let Some(error) = self.pre_error {
            return Err(error);
        }
        match self.rx.recv_timeout(timeout) {
            Ok(restarted) => Ok(restarted),
            Err(RecvTimeoutError::Timeout) => Err(WaitError::Timeout),
            Err(RecvTimeoutError::Disconnected) => Err(WaitError::RuntimeStopped),
        }
    }
}

#[derive(Debug)]
struct IsolateCompleteSlot {
    isolate: IsolateId,
    generation: AddressGeneration,
    sender: SyncSender<()>,
}

#[derive(Debug)]
struct OperationDoneSlot {
    isolate: IsolateId,
    call_kind: CallKind,
    sender: SyncSender<OperationDoneOutcome>,
}

#[derive(Debug)]
struct ChildRestartedSlot {
    parent: IsolateId,
    sender: SyncSender<ChildRestarted>,
}

/// Internal observation state owned by a [`crate::Runtime`].
///
/// Holds typed bounded one-slot senders for each pending observer. The
/// runtime drains the matching front slot when each fact happens; observers
/// register in the order they want to be served.
pub(crate) struct ObservationRegistry {
    pending_bound: VecDeque<SyncSender<BoundAddressOutcome>>,
    pending_isolate_complete: Vec<IsolateCompleteSlot>,
    pending_operation_done: Vec<OperationDoneSlot>,
    pending_child_restarted: Vec<ChildRestartedSlot>,
    max_pending: usize,
}

impl std::fmt::Debug for ObservationRegistry {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ObservationRegistry")
            .field("pending_bound", &self.pending_bound.len())
            .field(
                "pending_isolate_complete",
                &self.pending_isolate_complete.len(),
            )
            .field("pending_operation_done", &self.pending_operation_done.len())
            .field(
                "pending_child_restarted",
                &self.pending_child_restarted.len(),
            )
            .field("max_pending", &self.max_pending)
            .finish()
    }
}

/// Constructs a waiter whose receiver is already disconnected, so the next
/// `wait` resolves immediately to [`WaitError::RuntimeStopped`].
pub(crate) fn stopped_bound_waiter() -> BoundAddressWaiter {
    BoundAddressWaiter::with_error(WaitError::RuntimeStopped)
}

pub(crate) fn stopped_isolate_complete_waiter() -> IsolateCompleteWaiter {
    IsolateCompleteWaiter::with_error(WaitError::RuntimeStopped)
}

pub(crate) fn stopped_operation_done_waiter() -> OperationDoneWaiter {
    OperationDoneWaiter::with_error(WaitError::RuntimeStopped)
}

pub(crate) fn stopped_child_restarted_waiter() -> ChildRestartedWaiter {
    ChildRestartedWaiter::with_error(WaitError::RuntimeStopped)
}

impl ObservationRegistry {
    pub(crate) const DEFAULT_MAX_PENDING: usize = 1024;

    pub(crate) const fn new() -> Self {
        Self::with_max_pending(Self::DEFAULT_MAX_PENDING)
    }

    pub(crate) const fn with_max_pending(max_pending: usize) -> Self {
        Self {
            pending_bound: VecDeque::new(),
            pending_isolate_complete: Vec::new(),
            pending_operation_done: Vec::new(),
            pending_child_restarted: Vec::new(),
            max_pending,
        }
    }

    pub(crate) fn register_bound(&mut self) -> BoundAddressWaiter {
        if self.is_full() {
            return BoundAddressWaiter::with_error(WaitError::ObservationFull);
        }
        let (waiter, sender) = BoundAddressWaiter::from_pair();
        self.pending_bound.push_back(sender);
        waiter
    }

    pub(crate) fn register_isolate_complete(
        &mut self,
        isolate: IsolateId,
        generation: AddressGeneration,
    ) -> IsolateCompleteWaiter {
        if self.is_full() {
            return IsolateCompleteWaiter::with_error(WaitError::ObservationFull);
        }
        let (waiter, sender) = IsolateCompleteWaiter::from_pair();
        self.pending_isolate_complete.push(IsolateCompleteSlot {
            isolate,
            generation,
            sender,
        });
        waiter
    }

    pub(crate) fn register_operation_done(
        &mut self,
        isolate: IsolateId,
        call_kind: CallKind,
    ) -> OperationDoneWaiter {
        if self.is_full() {
            return OperationDoneWaiter::with_error(WaitError::ObservationFull);
        }
        let (waiter, sender) = OperationDoneWaiter::from_pair();
        self.pending_operation_done.push(OperationDoneSlot {
            isolate,
            call_kind,
            sender,
        });
        waiter
    }

    pub(crate) fn register_child_restarted(&mut self, parent: IsolateId) -> ChildRestartedWaiter {
        if self.is_full() {
            return ChildRestartedWaiter::with_error(WaitError::ObservationFull);
        }
        let (waiter, sender) = ChildRestartedWaiter::from_pair();
        self.pending_child_restarted
            .push(ChildRestartedSlot { parent, sender });
        waiter
    }

    fn pending_count(&self) -> usize {
        self.pending_bound.len()
            + self.pending_isolate_complete.len()
            + self.pending_operation_done.len()
            + self.pending_child_restarted.len()
    }

    fn is_full(&self) -> bool {
        self.pending_count() >= self.max_pending
    }

    pub(crate) fn notify_bound(&mut self, outcome: BoundAddressOutcome) {
        while let Some(sender) = self.pending_bound.pop_front() {
            match sender.try_send(outcome) {
                Ok(()) => return,
                Err(mpsc::TrySendError::Full(_)) => continue,
                Err(mpsc::TrySendError::Disconnected(_)) => continue,
            }
        }
    }

    pub(crate) fn notify_isolate_stopped(
        &mut self,
        isolate: IsolateId,
        generation: AddressGeneration,
    ) {
        // Walk all matching slots and notify each one; an isolate stopping
        // is a one-shot fact, but multiple host threads may have asked
        // about the same isolate.
        let mut i = 0;
        while i < self.pending_isolate_complete.len() {
            let slot = &self.pending_isolate_complete[i];
            if slot.isolate == isolate && slot.generation == generation {
                let removed = self.pending_isolate_complete.swap_remove(i);
                let _ = removed.sender.try_send(());
            } else {
                i += 1;
            }
        }
    }

    pub(crate) fn notify_operation_completed(
        &mut self,
        isolate: IsolateId,
        call_kind: CallKind,
        call_id: CallId,
    ) {
        if let Some(index) = self
            .pending_operation_done
            .iter()
            .position(|slot| slot.isolate == isolate && slot.call_kind == call_kind)
        {
            // `remove` (not `swap_remove`) preserves registration order for
            // the remaining slots so a later wait for a different (isolate,
            // kind) sees a deterministic queue.
            let slot = self.pending_operation_done.remove(index);
            let _ = slot
                .sender
                .try_send(OperationDoneOutcome::Completed(call_id));
        }
    }

    pub(crate) fn notify_operation_failed(
        &mut self,
        isolate: IsolateId,
        call_kind: CallKind,
        call_id: CallId,
        error: CallError,
    ) {
        if let Some(index) = self
            .pending_operation_done
            .iter()
            .position(|slot| slot.isolate == isolate && slot.call_kind == call_kind)
        {
            // `remove` (not `swap_remove`) preserves registration order for
            // the remaining slots so a later wait for a different (isolate,
            // kind) sees a deterministic queue.
            let slot = self.pending_operation_done.remove(index);
            let _ = slot
                .sender
                .try_send(OperationDoneOutcome::Failed { call_id, error });
        }
    }

    pub(crate) fn notify_child_restarted(&mut self, parent: IsolateId, restarted: ChildRestarted) {
        if let Some(index) = self
            .pending_child_restarted
            .iter()
            .position(|slot| slot.parent == parent)
        {
            let slot = self.pending_child_restarted.remove(index);
            let _ = slot.sender.try_send(restarted);
        }
    }
}
