//! Unix-domain socket lane.
//!
//! Tina owns Unix-domain resource identity, lane discipline, and
//! close/cancel semantics; the substrate owns the socket mechanics. Unix
//! sockets are sockets, so this lane follows the same rule as TCP/TLS:
//! it rides the per-shard Betelgeuse completion substrate, on the shard
//! thread, with no private worker thread and no blocking `std` socket
//! work. The runtime, not the substrate, assigns [`UnixListenerId`] /
//! [`UnixStreamId`] values, so isolate code never sees a raw fd.
//!
//! The narrow Unix-domain addressing the substrate previously lacked
//! (`bind_unix` / `connect_unix` plus the socket-file lifecycle) was added
//! directly to Betelgeuse rather than left in a hidden worker. Accept,
//! read, write, and close already worked at the substrate's family-agnostic
//! socket layer.
//!
//! On non-Unix platforms there is no backend; every Unix call completes
//! with a typed [`CallError::Unsupported`]. The capability is named, not
//! cfg-silently dropped.
//!
//! Lane discipline mirrors TCP: a listener has one accept lane, a stream
//! has one read lane and one write lane. Duplicate work on a lane is
//! [`CallError::ResourceBusy`]. Close wins over pending work — pending
//! ops on the closed resource are cancelled (the caller's continuation
//! does not fire; the runtime records `ResourceClosed`), and closing a
//! listener removes the underlying socket file (Betelgeuse owns that
//! unlink as part of socket-file lifecycle).

use super::*;

use crate::call::{UnixListenerId, UnixStreamId};

/// Driver-side handle for the Unix-domain rail.
pub(super) enum UnixLane {
    /// Live substrate-backed lane (Unix platforms).
    #[cfg(unix)]
    Live(imp::BetelgeuseUnix),
    /// No live backend on this platform; every call is typed
    /// `Unsupported`.
    #[cfg(not(unix))]
    Unsupported,
}

impl std::fmt::Debug for UnixLane {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("UnixLane")
    }
}

impl UnixLane {
    pub(super) fn new(io_loop: IOLoopHandle<Global>) -> Self {
        #[cfg(unix)]
        {
            Self::Live(imp::BetelgeuseUnix::with_io_loop(io_loop))
        }
        #[cfg(not(unix))]
        {
            let _ = io_loop;
            Self::Unsupported
        }
    }

    /// Submits one Unix-domain call. Returns `Some` for a synchronous
    /// outcome (bind/close, rejection, or unsupported), `None` when the op
    /// was armed against the substrate and will complete on a later
    /// `advance`.
    pub(super) fn submit(
        &mut self,
        call_id: CallId,
        request: CallInput,
    ) -> Option<DriverCompletion> {
        #[cfg(unix)]
        {
            let Self::Live(lane) = self;
            lane.submit(call_id, request)
        }
        #[cfg(not(unix))]
        {
            let _ = request;
            Some(DriverCompletion {
                call_id,
                result: CallOutput::Failed(CallError::Unsupported),
            })
        }
    }

    pub(super) fn advance(&mut self, completed: &mut Vec<DriverCompletion>) {
        #[cfg(unix)]
        {
            let Self::Live(lane) = self;
            lane.advance(completed);
        }
        #[cfg(not(unix))]
        {
            let _ = completed;
        }
    }

    /// Harvest-only reap (no substrate step). See [`BetelgeuseUnix::harvest`]
    /// and the driver's final harvest pass.
    pub(super) fn harvest(&mut self, completed: &mut Vec<DriverCompletion>) {
        #[cfg(unix)]
        {
            let Self::Live(lane) = self;
            lane.harvest(completed);
        }
        #[cfg(not(unix))]
        {
            let _ = completed;
        }
    }

    pub(super) fn has_pending(&self) -> bool {
        #[cfg(unix)]
        {
            let Self::Live(lane) = self;
            lane.has_pending()
        }
        #[cfg(not(unix))]
        {
            false
        }
    }

    pub(super) fn cancel(&mut self, call_id: CallId) -> bool {
        #[cfg(unix)]
        {
            let Self::Live(lane) = self;
            lane.cancel(call_id)
        }
        #[cfg(not(unix))]
        {
            let _ = call_id;
            false
        }
    }

    pub(super) fn cancel_pending(&mut self, deadline: Instant) -> Result<(), DriverShutdownError> {
        #[cfg(unix)]
        {
            let Self::Live(lane) = self;
            lane.cancel_pending(deadline)
        }
        #[cfg(not(unix))]
        {
            let _ = deadline;
            Ok(())
        }
    }

    pub(super) fn take_cancelled_by_close(&mut self) -> Vec<CallId> {
        #[cfg(unix)]
        {
            let Self::Live(lane) = self;
            lane.take_cancelled_by_close()
        }
        #[cfg(not(unix))]
        {
            Vec::new()
        }
    }

    pub(super) fn listener_count(&self) -> usize {
        #[cfg(unix)]
        {
            let Self::Live(lane) = self;
            lane.listener_count()
        }
        #[cfg(not(unix))]
        {
            0
        }
    }

    pub(super) fn stream_count(&self) -> usize {
        #[cfg(unix)]
        {
            let Self::Live(lane) = self;
            lane.stream_count()
        }
        #[cfg(not(unix))]
        {
            0
        }
    }

    pub(super) fn pending_call_count(&self) -> usize {
        #[cfg(unix)]
        {
            let Self::Live(lane) = self;
            lane.pending_call_count()
        }
        #[cfg(not(unix))]
        {
            0
        }
    }

    /// Physical pending-entry count, including tombstoned (cancelled /
    /// close-cancelled) ops the backend may still reference. Tests use this to
    /// prove the table stays bounded and that shutdown reaps every entry.
    #[cfg(test)]
    pub(super) fn physical_pending_len(&self) -> usize {
        #[cfg(unix)]
        {
            let Self::Live(lane) = self;
            lane.physical_pending_len()
        }
        #[cfg(not(unix))]
        {
            0
        }
    }
}

#[cfg(unix)]
mod imp {
    use std::alloc::Global;
    use std::io::ErrorKind;
    use std::path::PathBuf;
    use std::time::Instant;

    use betelgeuse::{
        AcceptCompletion, ConnectCompletion, IO, IOLoop, IOLoopHandle, IOSocket, RecvCompletion,
        SendCompletion,
    };

    use super::super::{
        CallError, CallId, CallInput, CallOutput, DriverCompletion, DriverShutdownError,
        INITIAL_DRIVER_PENDING_CAPACITY, INITIAL_DRIVER_RESOURCE_CAPACITY,
    };
    use super::{UnixListenerId, UnixStreamId};

    /// Runtime-owned Betelgeuse Unix-domain state.
    ///
    /// Shares the per-shard Betelgeuse loop with the TCP/TLS/storage lanes —
    /// a cloned handle, not a second socket stack. Owns all real Unix socket
    /// state; isolate code only ever sees the runtime's opaque
    /// [`UnixListenerId`] / [`UnixStreamId`] values.
    pub(crate) struct BetelgeuseUnix {
        io_loop: IOLoopHandle<Global>,
        next_listener_id: u64,
        next_stream_id: u64,
        listeners: Vec<ListenerEntry>,
        streams: Vec<StreamEntry>,
        pending: Vec<PendingOperation>,
        /// Calls cancelled by resource close. Runtime drains and traces them.
        cancelled_by_close: Vec<CallId>,
    }

    struct ListenerEntry {
        id: UnixListenerId,
        socket: Box<dyn IOSocket>,
    }

    struct StreamEntry {
        id: UnixStreamId,
        socket: Box<dyn IOSocket>,
    }

    /// One async operation in flight against Betelgeuse.
    ///
    /// The completion slot is heap-allocated so Betelgeuse's stored pointer
    /// to the inner `CompletionInner` stays valid while the
    /// `PendingOperation` is moved through the `pending` vector.
    struct PendingOperation {
        call_id: CallId,
        kind: PendingKind,
        lane: PendingLane,
        /// User explicitly cancelled this op via `cancel(call_id)`, or close
        /// won over it. Once set, the op no longer counts as a pending driver
        /// call: the runtime stopped waiting for its result.
        user_cancelled: bool,
        /// Shutdown drain blanket-marked this op for backend pointer release.
        /// Stays counted as a pending driver call so the terminal report can
        /// name work the lane could not finish in budget.
        shutdown_marked: bool,
    }

    enum PendingKind {
        Accept(Box<AcceptCompletion>),
        Connect {
            completion: Box<ConnectCompletion>,
            socket: Option<Box<dyn IOSocket>>,
        },
        Read(Box<RecvCompletion>),
        Write(Box<SendCompletion>),
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum PendingLane {
        ListenerAccept(UnixListenerId),
        Connect(CallId),
        StreamRead(UnixStreamId),
        StreamWrite(UnixStreamId),
    }

    impl BetelgeuseUnix {
        pub(crate) fn with_io_loop(io_loop: IOLoopHandle<Global>) -> Self {
            Self {
                io_loop,
                next_listener_id: 1,
                next_stream_id: 1,
                listeners: Vec::with_capacity(INITIAL_DRIVER_RESOURCE_CAPACITY),
                streams: Vec::with_capacity(INITIAL_DRIVER_RESOURCE_CAPACITY),
                pending: Vec::with_capacity(INITIAL_DRIVER_PENDING_CAPACITY),
                cancelled_by_close: Vec::new(),
            }
        }

        pub(crate) fn submit(
            &mut self,
            call_id: CallId,
            request: CallInput,
        ) -> Option<DriverCompletion> {
            match request {
                CallInput::UnixBind { path } => Some(DriverCompletion {
                    call_id,
                    result: self.do_bind(path),
                }),
                CallInput::UnixListenerClose { listener } => Some(DriverCompletion {
                    call_id,
                    result: self.do_listener_close(listener),
                }),
                CallInput::UnixStreamClose { stream } => Some(DriverCompletion {
                    call_id,
                    result: self.do_stream_close(stream),
                }),
                CallInput::UnixConnect { path } => {
                    let lane = PendingLane::Connect(call_id);
                    match self.arm_connect(&path) {
                        Ok(kind) => {
                            self.push_pending(call_id, kind, lane);
                            None
                        }
                        Err(result) => Some(DriverCompletion { call_id, result }),
                    }
                }
                CallInput::UnixAccept { listener } => {
                    let lane = PendingLane::ListenerAccept(listener);
                    if self.lane_has_pending(lane) {
                        return Some(self.fail(call_id, CallError::ResourceBusy));
                    }
                    match self.arm_accept(listener) {
                        Ok(kind) => {
                            self.push_pending(call_id, kind, lane);
                            None
                        }
                        Err(result) => Some(DriverCompletion { call_id, result }),
                    }
                }
                CallInput::UnixRead { stream, max_len } => {
                    let lane = PendingLane::StreamRead(stream);
                    if self.lane_has_pending(lane) {
                        return Some(self.fail(call_id, CallError::ResourceBusy));
                    }
                    match self.arm_read(stream, max_len) {
                        Ok(kind) => {
                            self.push_pending(call_id, kind, lane);
                            None
                        }
                        Err(result) => Some(DriverCompletion { call_id, result }),
                    }
                }
                CallInput::UnixWrite { stream, bytes } => {
                    let lane = PendingLane::StreamWrite(stream);
                    if self.lane_has_pending(lane) {
                        return Some(self.fail(call_id, CallError::ResourceBusy));
                    }
                    match self.arm_write(stream, bytes) {
                        Ok(kind) => {
                            self.push_pending(call_id, kind, lane);
                            None
                        }
                        Err(result) => Some(DriverCompletion { call_id, result }),
                    }
                }
                other => {
                    // The dispatcher only routes Unix variants here.
                    unreachable!(
                        "non-Unix CallInput reached the Unix lane: {:?}",
                        other.kind()
                    )
                }
            }
        }

        fn fail(&self, call_id: CallId, error: CallError) -> DriverCompletion {
            DriverCompletion {
                call_id,
                result: CallOutput::Failed(error),
            }
        }

        fn push_pending(&mut self, call_id: CallId, kind: PendingKind, lane: PendingLane) {
            self.pending.push(PendingOperation {
                call_id,
                kind,
                lane,
                user_cancelled: false,
                shutdown_marked: false,
            });
        }

        fn lane_has_pending(&self, lane: PendingLane) -> bool {
            self.pending.iter().any(|op| op.lane == lane)
        }

        pub(crate) fn advance(&mut self, completed: &mut Vec<DriverCompletion>) {
            // One substrate tick. Errors here are non-fatal: pending ops still
            // hold their slots and will be checked anyway.
            let _ = self.io_loop.step();
            self.harvest(completed);
        }

        /// Reaps every pending op whose backend completion now has a result,
        /// with no substrate step. Split from [`advance`](Self::advance) so the
        /// driver can run a final harvest after every lane has driven the
        /// *shared* io_loop: a completion this lane's `poll` surfaced but a
        /// sibling lane's `drain` executed would otherwise sit unharvested, and
        /// Unix rides the zero-wakeup park (excluded from
        /// `has_unsignaled_pending`), so nothing would re-poll to collect it.
        pub(crate) fn harvest(&mut self, completed: &mut Vec<DriverCompletion>) {
            // Drain in submission order so completion ordering is stable
            // relative to submission ordering whenever Betelgeuse permits it.
            let mut index = 0;
            while index < self.pending.len() {
                let mut op = self.pending.remove(index);
                if op.user_cancelled || op.shutdown_marked {
                    // A cancelled / close-cancelled op is dropped once its
                    // completion has a result — the backend no longer owns the
                    // slot, so the Box is safe to free. If it has no result
                    // yet (e.g. an accept/read that was already parked on the
                    // event loop when its resource closed), it stays as a
                    // tombstone: freeing it now would dangle the backend's
                    // stored pointer, and Betelgeuse has no per-op cancel. The
                    // bounded shutdown drain releases these via the whole-loop
                    // `cancel_pending_completions`. This matches the TCP/TLS
                    // lanes exactly.
                    if op.kind.has_result() {
                        continue;
                    }
                    self.pending.insert(index, op);
                    index += 1;
                    continue;
                }

                match self.try_complete(&mut op) {
                    Some(result) => completed.push(DriverCompletion {
                        call_id: op.call_id,
                        result,
                    }),
                    None => {
                        self.pending.insert(index, op);
                        index += 1;
                    }
                }
            }
        }

        pub(crate) fn has_pending(&self) -> bool {
            self.pending
                .iter()
                .any(|op| !op.user_cancelled && !op.shutdown_marked)
        }

        pub(crate) fn cancel(&mut self, call_id: CallId) -> bool {
            let Some(index) = self
                .pending
                .iter()
                .position(|op| op.call_id == call_id && !op.user_cancelled)
            else {
                return false;
            };
            self.pending[index].user_cancelled = true;
            true
        }

        pub(crate) fn take_cancelled_by_close(&mut self) -> Vec<CallId> {
            std::mem::take(&mut self.cancelled_by_close)
        }

        /// Cancels pending Unix operations during runtime shutdown.
        ///
        /// Mirrors the TCP lane: marks every pending op `shutdown_marked`
        /// (so stuck work surfaces in `pending_call_count`), asks the shared
        /// loop to release backend-owned slots, closes owned resources, and
        /// drains within the budget. Because the loop is shared with the TCP
        /// lane that performs the final whole-loop release check, this lane
        /// must release its own boxes before that check runs.
        pub(crate) fn cancel_pending(
            &mut self,
            deadline: Instant,
        ) -> Result<(), DriverShutdownError> {
            for op in &mut self.pending {
                op.shutdown_marked = true;
            }
            let cancel_result = self
                .io_loop
                .cancel_pending_completions()
                .map_err(|_| DriverShutdownError::BackendStillOwnsCompletions);
            self.close_all_resources();
            self.drain_marked_pending_for_shutdown(deadline);
            if cancel_result.is_err() || !self.pending.is_empty() {
                return Err(DriverShutdownError::BackendStillOwnsCompletions);
            }
            Ok(())
        }

        fn drain_marked_pending_for_shutdown(&mut self, deadline: Instant) {
            // Step the shared loop until either all pending entries drain or
            // the per-shard shutdown budget elapses. Each step gives the
            // backend a chance to release ownership of completion slots.
            loop {
                if self.pending.is_empty() || Instant::now() >= deadline {
                    return;
                }
                let _ = self.io_loop.step();
                let mut index = 0;
                while index < self.pending.len() {
                    if self.pending[index].kind.has_result() {
                        self.pending.remove(index);
                    } else {
                        index += 1;
                    }
                }
            }
        }

        fn close_all_resources(&mut self) {
            for entry in std::mem::take(&mut self.listeners) {
                entry.socket.close();
            }
            for entry in std::mem::take(&mut self.streams) {
                entry.socket.close();
            }
        }

        pub(crate) fn listener_count(&self) -> usize {
            self.listeners.len()
        }

        pub(crate) fn stream_count(&self) -> usize {
            self.streams.len()
        }

        pub(crate) fn pending_call_count(&self) -> usize {
            // Physical entries the runtime still waits on. User-cancelled work
            // (per-call cancel or close-win) drops out; shutdown-stuck work
            // stays counted so the terminal report names it.
            self.pending.iter().filter(|op| !op.user_cancelled).count()
        }

        #[cfg(test)]
        pub(crate) fn physical_pending_len(&self) -> usize {
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
                            let stream_id = UnixStreamId::new(self.next_stream_id);
                            self.next_stream_id += 1;
                            self.streams.push(StreamEntry {
                                id: stream_id,
                                socket,
                            });
                            Some(CallOutput::UnixAccepted { stream: stream_id })
                        }
                        Err(_) => Some(CallOutput::Failed(CallError::Io)),
                    }
                }
                PendingKind::Connect { completion, socket } => {
                    if !completion.has_result() {
                        return None;
                    }
                    let result = completion
                        .take_result()
                        .expect("connect completion advertised a result");
                    match result {
                        Ok(()) => {
                            let socket = socket.take().expect("connected socket available");
                            let stream_id = UnixStreamId::new(self.next_stream_id);
                            self.next_stream_id += 1;
                            self.streams.push(StreamEntry {
                                id: stream_id,
                                socket,
                            });
                            Some(CallOutput::UnixConnected { stream: stream_id })
                        }
                        Err(error) if error.kind() == ErrorKind::NotFound => {
                            Some(CallOutput::Failed(CallError::NotFound))
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
                        Ok(bytes) => Some(CallOutput::UnixRead { bytes }),
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
                        Ok(count) => Some(CallOutput::UnixWrote { count }),
                        Err(_) => Some(CallOutput::Failed(CallError::Io)),
                    }
                }
            }
        }

        fn do_bind(&mut self, path: PathBuf) -> CallOutput {
            let socket = match self.io_loop.socket() {
                Ok(socket) => socket,
                Err(_) => return CallOutput::Failed(CallError::Io),
            };
            if socket.bind_unix(&path).is_err() {
                return CallOutput::Failed(CallError::Io);
            }
            let id = UnixListenerId::new(self.next_listener_id);
            self.next_listener_id += 1;
            self.listeners.push(ListenerEntry { id, socket });
            CallOutput::UnixBound { listener: id, path }
        }

        fn do_listener_close(&mut self, listener: UnixListenerId) -> CallOutput {
            // Close wins: cancel any pending accept on this listener.
            for op in self.pending.iter_mut() {
                if matches!(op.lane, PendingLane::ListenerAccept(l) if l == listener)
                    && !op.user_cancelled
                {
                    op.user_cancelled = true;
                    self.cancelled_by_close.push(op.call_id);
                }
            }
            match self.listeners.iter().position(|entry| entry.id == listener) {
                Some(index) => {
                    let entry = self.listeners.remove(index);
                    // Betelgeuse unlinks the listener's socket file on close.
                    entry.socket.close();
                    CallOutput::UnixListenerClosed
                }
                None => CallOutput::Failed(CallError::InvalidResource),
            }
        }

        fn do_stream_close(&mut self, stream: UnixStreamId) -> CallOutput {
            // Close wins: cancel any pending read or write on this stream.
            for op in self.pending.iter_mut() {
                let on_this_stream = match op.lane {
                    PendingLane::StreamRead(s) | PendingLane::StreamWrite(s) => s == stream,
                    _ => false,
                };
                if on_this_stream && !op.user_cancelled {
                    op.user_cancelled = true;
                    self.cancelled_by_close.push(op.call_id);
                }
            }
            match self.streams.iter().position(|entry| entry.id == stream) {
                Some(index) => {
                    let entry = self.streams.remove(index);
                    entry.socket.close();
                    CallOutput::UnixStreamClosed
                }
                None => CallOutput::Failed(CallError::InvalidResource),
            }
        }

        fn arm_connect(&mut self, path: &std::path::Path) -> Result<PendingKind, CallOutput> {
            let socket = self
                .io_loop
                .socket()
                .map_err(|_| CallOutput::Failed(CallError::Io))?;
            let mut completion = Box::new(ConnectCompletion::new());
            if socket.connect_unix(&mut completion, path).is_err() {
                return Err(CallOutput::Failed(CallError::Io));
            }
            Ok(PendingKind::Connect {
                completion,
                socket: Some(socket),
            })
        }

        fn arm_accept(&mut self, listener: UnixListenerId) -> Result<PendingKind, CallOutput> {
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

        fn arm_read(
            &mut self,
            stream: UnixStreamId,
            max_len: usize,
        ) -> Result<PendingKind, CallOutput> {
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

        fn arm_write(
            &mut self,
            stream: UnixStreamId,
            bytes: Vec<u8>,
        ) -> Result<PendingKind, CallOutput> {
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
    }

    impl PendingKind {
        fn has_result(&self) -> bool {
            match self {
                Self::Accept(completion) => completion.has_result(),
                Self::Connect { completion, .. } => completion.has_result(),
                Self::Read(completion) => completion.has_result(),
                Self::Write(completion) => completion.has_result(),
            }
        }
    }

    // No `Drop` impl on purpose: the Unix lane shares its Betelgeuse loop
    // with the TCP lane, and `cancel_pending_completions` is a whole-loop
    // operation only safe before any lane has dropped (see the TLS lane's
    // note). On a bare runtime drop the lane's boxes and io_loop handle
    // simply fall; the backend is torn down without dereferencing them.

    impl std::fmt::Debug for BetelgeuseUnix {
        fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            formatter
                .debug_struct("BetelgeuseUnix")
                .field("listeners", &self.listeners.len())
                .field("streams", &self.streams.len())
                .field("pending", &self.pending.len())
                .finish_non_exhaustive()
        }
    }
}
