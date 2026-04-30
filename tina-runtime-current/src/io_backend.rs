//! Betelgeuse-backed completion-driven I/O backend for `tina-runtime-current`.
//!
//! This is the substrate target named in the reopened phase 012 plan:
//! Betelgeuse exposes a `step()`-driven, no-runtime, no-hidden-tasks I/O
//! library with caller-owned typed completion slots. That shape matches
//! Tina's explicit-stepping, runtime-owned-effects discipline.
//!
//! ## Contract with the rest of the runtime
//!
//! - one [`submit`](Self::submit) per [`CallRequest`] issued by an isolate.
//!   Synchronous Betelgeuse operations (`bind`, `close`) are completed in
//!   the same call; async operations (`accept`, `recv`, `send`) push a
//!   pending entry that holds its own boxed completion slot.
//! - one [`advance`](Self::advance) per [`crate::CurrentRuntime::step`]
//!   that drives the Betelgeuse loop forward and harvests any pending
//!   completions whose slots have a result available.
//! - resource ids ([`ListenerId`], [`StreamId`]) are runtime-assigned
//!   monotonic counters, not OS file descriptors. Isolate code never sees
//!   raw fds or `Box<dyn IOSocket>` values.
//!
//! ## Honest scope on this Betelgeuse rev
//!
//! The vendored Betelgeuse copy in this repo exposes `IOSocket::local_addr()`
//! and `IOSocket::peer_addr()`. That is enough for honest:
//!
//! - runtime-owned ephemeral-port discovery on `TcpBind { addr: ...:0 }`
//! - reporting the real accepted stream peer address
//!
//! The `tina` boundary still stays substrate-neutral; only
//! `tina-runtime-current` and the vendored backend know about these socket
//! introspection hooks.

use std::alloc::Global;
use std::net::SocketAddr;

use betelgeuse::{
    AcceptCompletion, IO, IOLoop, IOLoopHandle, IOSocket, RecvCompletion, SendCompletion, io_loop,
};

use crate::call::{CallFailureReason, CallId, CallRequest, CallResult, ListenerId, StreamId};

/// Runtime-owned I/O backend.
///
/// Owns all real socket state. Isolate code only ever sees the runtime's
/// opaque [`ListenerId`] / [`StreamId`] values.
pub(crate) struct IoBackend {
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
}

/// One async operation in flight against Betelgeuse.
///
/// The completion slot is heap-allocated so the backend's stored pointer
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

/// One completion the backend produced during [`IoBackend::advance`].
#[derive(Debug)]
pub(crate) struct CompletedOp {
    pub(crate) call_id: CallId,
    pub(crate) result: CallResult,
}

impl IoBackend {
    pub(crate) fn new() -> Self {
        let io_loop = io_loop(Global)
            .expect("failed to initialise Betelgeuse IO loop for tina-runtime-current");
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
    pub(crate) fn submit(&mut self, call_id: CallId, request: CallRequest) -> Option<CompletedOp> {
        match request {
            CallRequest::TcpBind { addr } => Some(CompletedOp {
                call_id,
                result: self.do_bind(addr),
            }),
            CallRequest::TcpListenerClose { listener } => Some(CompletedOp {
                call_id,
                result: self.do_listener_close(listener),
            }),
            CallRequest::TcpStreamClose { stream } => Some(CompletedOp {
                call_id,
                result: self.do_stream_close(stream),
            }),
            CallRequest::TcpAccept { listener } => match self.arm_accept(listener) {
                Ok(pending) => {
                    self.pending.push(PendingOperation {
                        call_id,
                        kind: pending,
                    });
                    None
                }
                Err(result) => Some(CompletedOp { call_id, result }),
            },
            CallRequest::TcpRead { stream, max_len } => match self.arm_read(stream, max_len) {
                Ok(pending) => {
                    self.pending.push(PendingOperation {
                        call_id,
                        kind: pending,
                    });
                    None
                }
                Err(result) => Some(CompletedOp { call_id, result }),
            },
            CallRequest::TcpWrite { stream, bytes } => match self.arm_write(stream, bytes) {
                Ok(pending) => {
                    self.pending.push(PendingOperation {
                        call_id,
                        kind: pending,
                    });
                    None
                }
                Err(result) => Some(CompletedOp { call_id, result }),
            },
            CallRequest::Sleep { .. } => Some(CompletedOp {
                call_id,
                result: CallResult::Failed(CallFailureReason::Unsupported),
            }),
        }
    }

    /// Advances Betelgeuse by one tick and harvests any pending operations
    /// whose completion slots have a result available. Returned in
    /// submission order.
    pub(crate) fn advance(&mut self) -> Vec<CompletedOp> {
        // One backend tick. Errors here are non-fatal: pending ops still
        // hold their slots and will be checked anyway. We surface a
        // backend-level error only if the loop step itself errored, in
        // which case there is nothing useful to do beyond noting it.
        let _ = self.io_loop.step();

        let mut completed = Vec::new();
        let mut still_pending: Vec<PendingOperation> = Vec::with_capacity(self.pending.len());

        // Drain in submission order so completion ordering is stable
        // relative to submission ordering whenever Betelgeuse permits it.
        for mut op in std::mem::take(&mut self.pending) {
            match self.try_complete(&mut op) {
                Some(result) => completed.push(CompletedOp {
                    call_id: op.call_id,
                    result,
                }),
                None => still_pending.push(op),
            }
        }

        self.pending = still_pending;
        completed
    }

    /// Returns whether the backend has any pending operations. Tests use
    /// this to decide whether stepping further can produce more
    /// completions.
    pub(crate) fn has_pending(&self) -> bool {
        !self.pending.is_empty()
    }

    #[cfg(test)]
    pub(crate) fn pending_count(&self) -> usize {
        self.pending.len()
    }

    fn try_complete(&mut self, op: &mut PendingOperation) -> Option<CallResult> {
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
                            Err(_) => return Some(CallResult::Failed(CallFailureReason::Io)),
                        };
                        let stream_id = StreamId::new(self.next_stream_id);
                        self.next_stream_id += 1;
                        self.streams.push(StreamEntry {
                            id: stream_id,
                            socket,
                        });
                        Some(CallResult::TcpAccepted {
                            stream: stream_id,
                            peer_addr,
                        })
                    }
                    Err(_) => Some(CallResult::Failed(CallFailureReason::Io)),
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
                    Ok(bytes) => Some(CallResult::TcpRead { bytes }),
                    Err(_) => Some(CallResult::Failed(CallFailureReason::Io)),
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
                    Ok(count) => Some(CallResult::TcpWrote { count }),
                    Err(_) => Some(CallResult::Failed(CallFailureReason::Io)),
                }
            }
        }
    }

    fn do_bind(&mut self, addr: SocketAddr) -> CallResult {
        let socket = match self.io_loop.socket() {
            Ok(socket) => socket,
            Err(_) => return CallResult::Failed(CallFailureReason::Io),
        };
        if socket.bind(addr).is_err() {
            return CallResult::Failed(CallFailureReason::Io);
        }
        let local_addr = match socket.local_addr() {
            Ok(addr) => addr,
            Err(_) => return CallResult::Failed(CallFailureReason::Io),
        };

        let id = ListenerId::new(self.next_listener_id);
        self.next_listener_id += 1;
        self.listeners.push(ListenerEntry { id, socket });
        CallResult::TcpBound {
            listener: id,
            local_addr,
        }
    }

    fn do_listener_close(&mut self, listener: ListenerId) -> CallResult {
        match self.listeners.iter().position(|entry| entry.id == listener) {
            Some(index) => {
                let entry = self.listeners.remove(index);
                entry.socket.close();
                CallResult::TcpListenerClosed
            }
            None => CallResult::Failed(CallFailureReason::InvalidResource),
        }
    }

    fn do_stream_close(&mut self, stream: StreamId) -> CallResult {
        match self.streams.iter().position(|entry| entry.id == stream) {
            Some(index) => {
                let entry = self.streams.remove(index);
                entry.socket.close();
                CallResult::TcpStreamClosed
            }
            None => CallResult::Failed(CallFailureReason::InvalidResource),
        }
    }

    fn arm_accept(&mut self, listener: ListenerId) -> Result<PendingKind, CallResult> {
        let entry = self
            .listeners
            .iter()
            .find(|entry| entry.id == listener)
            .ok_or(CallResult::Failed(CallFailureReason::InvalidResource))?;
        let mut completion = Box::new(AcceptCompletion::new());
        if entry.socket.accept(&mut completion).is_err() {
            return Err(CallResult::Failed(CallFailureReason::Io));
        }
        Ok(PendingKind::Accept(completion))
    }

    fn arm_read(&mut self, stream: StreamId, max_len: usize) -> Result<PendingKind, CallResult> {
        let entry = self
            .streams
            .iter()
            .find(|entry| entry.id == stream)
            .ok_or(CallResult::Failed(CallFailureReason::InvalidResource))?;
        let mut completion = Box::new(RecvCompletion::new());
        if entry.socket.recv(&mut completion, max_len).is_err() {
            return Err(CallResult::Failed(CallFailureReason::Io));
        }
        Ok(PendingKind::Read(completion))
    }

    fn arm_write(&mut self, stream: StreamId, bytes: Vec<u8>) -> Result<PendingKind, CallResult> {
        let entry = self
            .streams
            .iter()
            .find(|entry| entry.id == stream)
            .ok_or(CallResult::Failed(CallFailureReason::InvalidResource))?;
        let mut completion = Box::new(SendCompletion::new());
        if entry.socket.send(&mut completion, bytes).is_err() {
            return Err(CallResult::Failed(CallFailureReason::Io));
        }
        Ok(PendingKind::Write(completion))
    }
}

impl std::fmt::Debug for IoBackend {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("IoBackend")
            .field("listeners", &self.listeners.len())
            .field("streams", &self.streams.len())
            .field("pending", &self.pending.len())
            .finish_non_exhaustive()
    }
}
