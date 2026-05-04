//! Runtime-owned substrate driver for `tina-runtime`.
//!
//! Tina keeps isolate scheduling, mailboxes, tracing, supervision, and call
//! outcome delivery in [`crate::Runtime`]. The driver owns only substrate
//! operations: timers, TCP resources, completion readiness, and cancellation.
//!
//! ## Contract with the rest of the runtime
//!
//! - one [`RuntimeDriver::submit`] per [`CallInput`] issued by an isolate.
//! - one [`RuntimeDriver::advance`] per [`crate::Runtime::step`].
//! - the driver returns [`DriverCompletion`] values; `Runtime` translates them
//!   into ordinary later-turn messages and trace events.
//! - resource ids ([`ListenerId`], [`StreamId`]) are runtime-assigned
//!   monotonic counters, not OS file descriptors. Isolate code never sees
//!   raw fds or `Box<dyn IOSocket>` values.
//!
use std::alloc::Global;
use std::net::SocketAddr;
use std::time::Instant;

use betelgeuse::{
    AcceptCompletion, IO, IOLoop, IOLoopHandle, IOSocket, RecvCompletion, SendCompletion, io_loop,
};

use crate::call::{CallError, CallId, CallInput, CallOutput, ListenerId, StreamId};

/// Runtime-owned substrate driver.
pub(crate) trait RuntimeDriver: std::fmt::Debug {
    /// Submits one runtime-owned call.
    fn submit(
        &mut self,
        call_id: CallId,
        request: CallInput,
        now: Instant,
    ) -> Option<DriverCompletion>;

    /// Advances the substrate by one runtime step.
    fn advance(&mut self, now: Instant) -> Vec<DriverCompletion>;

    /// Returns whether substrate completions are still pending.
    fn has_pending(&self) -> bool;

    /// Drops pending substrate operations during runtime shutdown.
    fn cancel_pending(&mut self);

    /// Cancels one runtime-owned call by id.
    fn cancel(&mut self, call_id: CallId) -> bool;

    #[cfg(test)]
    fn io_pending_count(&self) -> usize {
        0
    }
}

/// Betelgeuse-backed runtime driver.
///
/// Betelgeuse exposes a `step()`-driven, no-runtime, no-hidden-tasks I/O
/// library with caller-owned typed completion slots. That shape matches Tina's
/// explicit-stepping, runtime-owned-effects discipline.
pub(crate) struct BetelgeuseDriver {
    tcp: BetelgeuseTcp,
    timers: Vec<TimerEntry>,
    next_timer_ordinal: u64,
}

/// One pending timer tracked by the driver.
#[derive(Debug)]
struct TimerEntry {
    call_id: CallId,
    deadline: Instant,
    insertion_order: u64,
}

/// Runtime-owned Betelgeuse TCP state.
///
/// Owns all real socket state. Isolate code only ever sees the runtime's
/// opaque [`ListenerId`] / [`StreamId`] values.
struct BetelgeuseTcp {
    io_loop: IOLoopHandle<Global>,
    next_listener_id: u64,
    next_stream_id: u64,
    listeners: Vec<ListenerEntry>,
    streams: Vec<StreamEntry>,
    pending: Vec<PendingOperation>,
}

struct ListenerEntry {
    id: ListenerId,
    socket: Box<dyn IOSocket>,
}

struct StreamEntry {
    id: StreamId,
    socket: Box<dyn IOSocket>,
}

struct PendingOperation {
    call_id: CallId,
    kind: PendingKind,
    resource: PendingResource,
    cancelled: bool,
}

/// One async operation in flight against Betelgeuse.
///
/// The completion slot is heap-allocated so Betelgeuse's stored pointer
/// to the inner `CompletionInner` stays valid while the `PendingOperation`
/// itself is moved through the `pending` vector. We do not track the
/// originating listener/stream id on the pending entry: the runtime's
/// `call_id` is the stable handle the rest of the runtime uses, and the
/// completion slot itself carries everything Betelgeuse needs.
enum PendingKind {
    Accept(Box<AcceptCompletion>),
    Read(Box<RecvCompletion>),
    Write(Box<SendCompletion>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PendingResource {
    Listener(ListenerId),
    Stream(StreamId),
}

/// One completion the driver produced during [`RuntimeDriver::advance`].
#[derive(Debug)]
pub(crate) struct DriverCompletion {
    pub(crate) call_id: CallId,
    pub(crate) result: CallOutput,
}

impl BetelgeuseDriver {
    pub(crate) fn new() -> Self {
        let io_loop =
            io_loop(Global).expect("failed to initialise Betelgeuse IO loop for tina-runtime");
        Self::with_io_loop(io_loop)
    }

    pub(crate) fn with_io_loop(io_loop: IOLoopHandle<Global>) -> Self {
        Self {
            tcp: BetelgeuseTcp::with_io_loop(io_loop),
            timers: Vec::new(),
            next_timer_ordinal: 0,
        }
    }
}

impl RuntimeDriver for BetelgeuseDriver {
    fn submit(
        &mut self,
        call_id: CallId,
        request: CallInput,
        now: Instant,
    ) -> Option<DriverCompletion> {
        match request {
            CallInput::Sleep { after } => {
                let insertion_order = self.next_timer_ordinal;
                self.next_timer_ordinal += 1;
                self.timers.push(TimerEntry {
                    call_id,
                    deadline: now + after,
                    insertion_order,
                });
                None
            }
            other => self.tcp.submit(call_id, other),
        }
    }

    fn advance(&mut self, now: Instant) -> Vec<DriverCompletion> {
        let mut completed = self.tcp.advance();
        completed.extend(self.harvest_timers(now));
        completed
    }

    fn has_pending(&self) -> bool {
        self.tcp.has_pending() || !self.timers.is_empty()
    }

    fn cancel_pending(&mut self) {
        self.timers.clear();
        self.tcp.cancel_pending();
    }

    fn cancel(&mut self, call_id: CallId) -> bool {
        let before = self.timers.len();
        self.timers.retain(|entry| entry.call_id != call_id);
        before != self.timers.len() || self.tcp.cancel(call_id)
    }

    #[cfg(test)]
    fn io_pending_count(&self) -> usize {
        self.tcp.pending_count()
    }
}

impl BetelgeuseDriver {
    fn harvest_timers(&mut self, now: Instant) -> Vec<DriverCompletion> {
        let mut due = Vec::new();
        let mut still_pending = Vec::new();
        for entry in std::mem::take(&mut self.timers) {
            if entry.deadline <= now {
                due.push(entry);
            } else {
                still_pending.push(entry);
            }
        }
        self.timers = still_pending;
        due.sort_by(|a, b| {
            a.deadline
                .cmp(&b.deadline)
                .then_with(|| a.insertion_order.cmp(&b.insertion_order))
        });
        due.into_iter()
            .map(|entry| DriverCompletion {
                call_id: entry.call_id,
                result: CallOutput::TimerFired,
            })
            .collect()
    }
}

impl BetelgeuseTcp {
    fn with_io_loop(io_loop: IOLoopHandle<Global>) -> Self {
        Self {
            io_loop,
            next_listener_id: 1,
            next_stream_id: 1,
            listeners: Vec::new(),
            streams: Vec::new(),
            pending: Vec::new(),
        }
    }

    /// Submits one runtime-owned call. Synchronous Betelgeuse ops (bind,
    /// close) finish here and the result is returned inline; async ops
    /// (accept, recv, send) push a pending entry and return [`None`].
    fn submit(&mut self, call_id: CallId, request: CallInput) -> Option<DriverCompletion> {
        match request {
            CallInput::TcpBind { addr } => Some(DriverCompletion {
                call_id,
                result: self.do_bind(addr),
            }),
            CallInput::TcpListenerClose { listener } => Some(DriverCompletion {
                call_id,
                result: self.do_listener_close(listener),
            }),
            CallInput::TcpStreamClose { stream } => Some(DriverCompletion {
                call_id,
                result: self.do_stream_close(stream),
            }),
            CallInput::TcpAccept { listener } => {
                let resource = PendingResource::Listener(listener);
                if self.resource_has_active_pending(resource) {
                    return Some(DriverCompletion {
                        call_id,
                        result: CallOutput::Failed(CallError::ResourceBusy),
                    });
                }
                match self.arm_accept(listener) {
                    Ok(pending) => {
                        self.pending.push(PendingOperation {
                            call_id,
                            kind: pending,
                            resource,
                            cancelled: false,
                        });
                        None
                    }
                    Err(result) => Some(DriverCompletion { call_id, result }),
                }
            }
            CallInput::TcpRead { stream, max_len } => {
                let resource = PendingResource::Stream(stream);
                if self.resource_has_active_pending(resource) {
                    return Some(DriverCompletion {
                        call_id,
                        result: CallOutput::Failed(CallError::ResourceBusy),
                    });
                }
                match self.arm_read(stream, max_len) {
                    Ok(pending) => {
                        self.pending.push(PendingOperation {
                            call_id,
                            kind: pending,
                            resource,
                            cancelled: false,
                        });
                        None
                    }
                    Err(result) => Some(DriverCompletion { call_id, result }),
                }
            }
            CallInput::TcpWrite { stream, bytes } => {
                let resource = PendingResource::Stream(stream);
                if self.resource_has_active_pending(resource) {
                    return Some(DriverCompletion {
                        call_id,
                        result: CallOutput::Failed(CallError::ResourceBusy),
                    });
                }
                match self.arm_write(stream, bytes) {
                    Ok(pending) => {
                        self.pending.push(PendingOperation {
                            call_id,
                            kind: pending,
                            resource,
                            cancelled: false,
                        });
                        None
                    }
                    Err(result) => Some(DriverCompletion { call_id, result }),
                }
            }
            CallInput::Sleep { .. } => Some(DriverCompletion {
                call_id,
                result: CallOutput::Failed(CallError::Unsupported),
            }),
        }
    }

    fn resource_has_active_pending(&self, resource: PendingResource) -> bool {
        self.pending
            .iter()
            .any(|op| op.resource == resource && !op.cancelled)
    }

    /// Advances Betelgeuse by one tick and harvests any pending operations
    /// whose completion slots have a result available. Returned in
    /// submission order.
    fn advance(&mut self) -> Vec<DriverCompletion> {
        // One substrate tick. Errors here are non-fatal: pending ops still
        // hold their slots and will be checked anyway.
        let _ = self.io_loop.step();

        let mut completed = Vec::new();
        let mut still_pending: Vec<PendingOperation> = Vec::with_capacity(self.pending.len());

        // Drain in submission order so completion ordering is stable
        // relative to submission ordering whenever Betelgeuse permits it.
        for mut op in std::mem::take(&mut self.pending) {
            if op.cancelled {
                if op.kind.has_result() {
                    continue;
                }
                still_pending.push(op);
                continue;
            }
            match self.try_complete(&mut op) {
                Some(result) => completed.push(DriverCompletion {
                    call_id: op.call_id,
                    result,
                }),
                None => still_pending.push(op),
            }
        }

        self.pending = still_pending;
        completed
    }

    /// Returns whether TCP has any pending operations. Tests use
    /// this to decide whether stepping further can produce more
    /// completions.
    fn has_pending(&self) -> bool {
        self.pending.iter().any(|op| !op.cancelled)
    }

    /// Drops pending TCP operations during runtime shutdown.
    ///
    /// Tina emits requester-facing shutdown/cancel trace events from
    /// `Runtime`; TCP state only owns the substrate completion slots and
    /// resource handles.
    fn cancel_pending(&mut self) {
        self.pending.clear();
    }

    fn cancel(&mut self, call_id: CallId) -> bool {
        let Some(index) = self
            .pending
            .iter()
            .position(|op| op.call_id == call_id && !op.cancelled)
        else {
            return false;
        };
        let resource = self.pending[index].resource;
        self.close_pending_resource(resource);
        self.pending[index].cancelled = true;
        true
    }

    #[cfg(test)]
    fn pending_count(&self) -> usize {
        self.pending.len()
    }

    fn try_complete(&mut self, op: &mut PendingOperation) -> Option<CallOutput> {
        match &mut op.kind {
            PendingKind::Accept(completion) => {
                if !completion.has_result() {
                    return None;
                }
                let result = completion
                    .take_result()
                    .expect("accept completion advertised a result");
                match result {
                    Ok(socket) => {
                        let peer_addr = match socket.peer_addr() {
                            Ok(addr) => addr,
                            Err(_) => return Some(CallOutput::Failed(CallError::Io)),
                        };
                        let stream_id = StreamId::new(self.next_stream_id);
                        self.next_stream_id += 1;
                        self.streams.push(StreamEntry {
                            id: stream_id,
                            socket,
                        });
                        Some(CallOutput::TcpAccepted {
                            stream: stream_id,
                            peer_addr,
                        })
                    }
                    Err(_) => Some(CallOutput::Failed(CallError::Io)),
                }
            }
            PendingKind::Read(completion) => {
                if !completion.has_result() {
                    return None;
                }
                let result = completion
                    .take_result()
                    .expect("recv completion advertised a result");
                match result {
                    Ok(bytes) => Some(CallOutput::TcpRead { bytes }),
                    Err(_) => Some(CallOutput::Failed(CallError::Io)),
                }
            }
            PendingKind::Write(completion) => {
                if !completion.has_result() {
                    return None;
                }
                let result = completion
                    .take_result()
                    .expect("send completion advertised a result");
                match result {
                    Ok(count) => Some(CallOutput::TcpWrote { count }),
                    Err(_) => Some(CallOutput::Failed(CallError::Io)),
                }
            }
        }
    }

    fn do_bind(&mut self, addr: SocketAddr) -> CallOutput {
        let socket = match self.io_loop.socket() {
            Ok(socket) => socket,
            Err(_) => return CallOutput::Failed(CallError::Io),
        };
        if socket.bind(addr).is_err() {
            return CallOutput::Failed(CallError::Io);
        }
        let local_addr = match socket.local_addr() {
            Ok(addr) => addr,
            Err(_) => return CallOutput::Failed(CallError::Io),
        };

        let id = ListenerId::new(self.next_listener_id);
        self.next_listener_id += 1;
        self.listeners.push(ListenerEntry { id, socket });
        CallOutput::TcpBound {
            listener: id,
            local_addr,
        }
    }

    fn do_listener_close(&mut self, listener: ListenerId) -> CallOutput {
        if self.resource_has_active_pending(PendingResource::Listener(listener)) {
            return CallOutput::Failed(CallError::ResourceBusy);
        }

        match self.listeners.iter().position(|entry| entry.id == listener) {
            Some(index) => {
                let entry = self.listeners.remove(index);
                entry.socket.close();
                CallOutput::TcpListenerClosed
            }
            None => CallOutput::Failed(CallError::InvalidResource),
        }
    }

    fn do_stream_close(&mut self, stream: StreamId) -> CallOutput {
        if self.resource_has_active_pending(PendingResource::Stream(stream)) {
            return CallOutput::Failed(CallError::ResourceBusy);
        }

        match self.streams.iter().position(|entry| entry.id == stream) {
            Some(index) => {
                let entry = self.streams.remove(index);
                entry.socket.close();
                CallOutput::TcpStreamClosed
            }
            None => CallOutput::Failed(CallError::InvalidResource),
        }
    }

    fn arm_accept(&mut self, listener: ListenerId) -> Result<PendingKind, CallOutput> {
        let entry = self
            .listeners
            .iter()
            .find(|entry| entry.id == listener)
            .ok_or(CallOutput::Failed(CallError::InvalidResource))?;
        let mut completion = Box::new(AcceptCompletion::new());
        if entry.socket.accept(&mut completion).is_err() {
            return Err(CallOutput::Failed(CallError::Io));
        }
        Ok(PendingKind::Accept(completion))
    }

    fn arm_read(&mut self, stream: StreamId, max_len: usize) -> Result<PendingKind, CallOutput> {
        let entry = self
            .streams
            .iter()
            .find(|entry| entry.id == stream)
            .ok_or(CallOutput::Failed(CallError::InvalidResource))?;
        let mut completion = Box::new(RecvCompletion::new());
        if entry.socket.recv(&mut completion, max_len).is_err() {
            return Err(CallOutput::Failed(CallError::Io));
        }
        Ok(PendingKind::Read(completion))
    }

    fn arm_write(&mut self, stream: StreamId, bytes: Vec<u8>) -> Result<PendingKind, CallOutput> {
        let entry = self
            .streams
            .iter()
            .find(|entry| entry.id == stream)
            .ok_or(CallOutput::Failed(CallError::InvalidResource))?;
        let mut completion = Box::new(SendCompletion::new());
        if entry.socket.send(&mut completion, bytes).is_err() {
            return Err(CallOutput::Failed(CallError::Io));
        }
        Ok(PendingKind::Write(completion))
    }

    fn close_pending_resource(&mut self, resource: PendingResource) {
        match resource {
            PendingResource::Listener(listener) => {
                if let Some(index) = self.listeners.iter().position(|entry| entry.id == listener) {
                    let entry = self.listeners.remove(index);
                    entry.socket.close();
                }
            }
            PendingResource::Stream(stream) => {
                if let Some(index) = self.streams.iter().position(|entry| entry.id == stream) {
                    let entry = self.streams.remove(index);
                    entry.socket.close();
                }
            }
        }
    }
}

impl PendingKind {
    fn has_result(&self) -> bool {
        match self {
            Self::Accept(completion) => completion.has_result(),
            Self::Read(completion) => completion.has_result(),
            Self::Write(completion) => completion.has_result(),
        }
    }
}

impl std::fmt::Debug for BetelgeuseDriver {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("BetelgeuseDriver")
            .field("tcp", &self.tcp)
            .field("timers", &self.timers.len())
            .finish_non_exhaustive()
    }
}

impl std::fmt::Debug for BetelgeuseTcp {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("BetelgeuseTcp")
            .field("listeners", &self.listeners.len())
            .field("streams", &self.streams.len())
            .field("pending", &self.pending.len())
            .finish_non_exhaustive()
    }
}
