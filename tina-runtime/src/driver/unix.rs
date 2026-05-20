//! Unix-domain socket lane.
//!
//! Tina owns Unix-domain resource identity, lane discipline, and
//! close/cancel semantics; the OS owns the socket mechanics. On Unix
//! platforms this lane runs a single worker thread that owns every
//! `UnixListener` / `UnixStream` as a non-blocking socket and drives a
//! poll loop: commands arrive over a bounded channel, completions go
//! back over another. The runtime, not the worker, assigns
//! [`UnixListenerId`] / [`UnixStreamId`] values, so isolate code never
//! sees a raw fd.
//!
//! On non-Unix platforms there is no worker; every Unix call completes
//! with a typed [`CallError::Unsupported`]. The capability is named, not
//! cfg-silently dropped.
//!
//! Lane discipline mirrors TCP: a listener has one accept lane, a stream
//! has one read lane and one write lane. Duplicate work on a lane is
//! [`CallError::ResourceBusy`]. Close wins over pending work — pending
//! ops on the closed resource are cancelled (the caller's continuation
//! does not fire; the runtime records `ResourceClosed`), and closing a
//! listener removes the underlying socket file.

use super::*;

use crate::call::{UnixListenerId, UnixStreamId};

/// Driver-side handle for the Unix-domain rail.
pub(super) enum UnixLane {
    /// Live OS-backed lane (Unix platforms).
    #[cfg(unix)]
    Live(imp::LiveUnixLane),
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
    pub(super) fn new() -> Self {
        #[cfg(unix)]
        {
            Self::Live(imp::LiveUnixLane::new())
        }
        #[cfg(not(unix))]
        {
            Self::Unsupported
        }
    }

    /// Submits one Unix-domain call. Returns `Some` for a synchronous
    /// outcome (rejection or unsupported), `None` when the op was handed
    /// to the worker and will complete on a later `advance`.
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

    pub(super) fn cancel_pending(&mut self, deadline: Instant) {
        #[cfg(unix)]
        {
            let Self::Live(lane) = self;
            lane.cancel_pending(deadline);
        }
        #[cfg(not(unix))]
        {
            let _ = deadline;
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
}

#[cfg(unix)]
mod imp {
    use std::collections::HashSet;
    use std::io::{ErrorKind, Read, Write};
    use std::os::unix::fs::FileTypeExt;
    use std::os::unix::net::{UnixListener, UnixStream};
    use std::path::PathBuf;
    use std::sync::mpsc::{Receiver, RecvTimeoutError, SyncSender, TryRecvError, sync_channel};
    use std::thread::{self, JoinHandle};
    use std::time::{Duration, Instant};

    use super::super::{CallError, CallId, CallInput, CallOutput, DriverCompletion};
    use super::{UnixListenerId, UnixStreamId};

    /// Bounded command queue depth between the driver and the worker.
    const COMMAND_CAPACITY: usize = 256;
    /// Poll cadence for in-flight non-blocking ops when no command wakes
    /// the worker. Bounds read/accept/write latency for local IPC.
    const POLL_INTERVAL: Duration = Duration::from_millis(1);

    /// Which resource lane a pending op occupies, for `ResourceBusy`.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum LaneKey {
        Accept(u64),
        Read(u64),
        Write(u64),
        // Connect/bind/close are one-shot per call and never reject as busy.
        OneShot,
    }

    struct DriverPending {
        call_id: CallId,
        lane: LaneKey,
    }

    enum WorkerCommand {
        Bind {
            call_id: CallId,
            listener_id: u64,
            path: PathBuf,
        },
        Connect {
            call_id: CallId,
            stream_id: u64,
            path: PathBuf,
        },
        Accept {
            call_id: CallId,
            listener_id: u64,
            new_stream_id: u64,
        },
        Read {
            call_id: CallId,
            stream_id: u64,
            max_len: usize,
        },
        Write {
            call_id: CallId,
            stream_id: u64,
            bytes: Vec<u8>,
        },
        CloseListener {
            call_id: CallId,
            listener_id: u64,
        },
        CloseStream {
            call_id: CallId,
            stream_id: u64,
        },
        CancelOp {
            call_id: CallId,
        },
    }

    struct WorkerCompletion {
        call_id: CallId,
        result: CallOutput,
    }

    /// Driver-side handle. Owns id assignment, the open-set used for
    /// synchronous `InvalidResource` / `ResourceBusy` validation, and the
    /// channels to the worker.
    pub(crate) struct LiveUnixLane {
        next_listener_id: u64,
        next_stream_id: u64,
        open_listeners: HashSet<u64>,
        open_streams: HashSet<u64>,
        pending: Vec<DriverPending>,
        cancelled_by_close: Vec<CallId>,
        commands: Option<SyncSender<WorkerCommand>>,
        completions: Receiver<WorkerCompletion>,
        handle: Option<JoinHandle<()>>,
    }

    impl LiveUnixLane {
        pub(crate) fn new() -> Self {
            let (command_tx, command_rx) = sync_channel(COMMAND_CAPACITY);
            let (completion_tx, completion_rx) = sync_channel(COMMAND_CAPACITY.saturating_add(1));
            let handle = thread::spawn(move || worker_loop(command_rx, completion_tx));
            Self {
                next_listener_id: 1,
                next_stream_id: 1,
                open_listeners: HashSet::new(),
                open_streams: HashSet::new(),
                pending: Vec::new(),
                cancelled_by_close: Vec::new(),
                commands: Some(command_tx),
                completions: completion_rx,
                handle: Some(handle),
            }
        }

        fn lane_busy(&self, lane: LaneKey) -> bool {
            self.pending.iter().any(|pending| pending.lane == lane)
        }

        fn send_pending(
            &mut self,
            pending: DriverPending,
            command: WorkerCommand,
        ) -> Option<DriverCompletion> {
            let call_id = pending.call_id;
            self.pending.push(pending);
            let Some(sender) = &self.commands else {
                self.pending.retain(|pending| pending.call_id != call_id);
                return Some(DriverCompletion {
                    call_id,
                    result: CallOutput::Failed(CallError::Io),
                });
            };
            match sender.try_send(command) {
                Ok(()) => None,
                Err(_) => {
                    self.pending.retain(|pending| pending.call_id != call_id);
                    Some(DriverCompletion {
                        call_id,
                        result: CallOutput::Failed(CallError::Io),
                    })
                }
            }
        }

        pub(crate) fn submit(
            &mut self,
            call_id: CallId,
            request: CallInput,
        ) -> Option<DriverCompletion> {
            match request {
                CallInput::UnixBind { path } => {
                    let listener_id = self.next_listener_id;
                    self.next_listener_id += 1;
                    self.send_pending(
                        DriverPending {
                            call_id,
                            lane: LaneKey::OneShot,
                        },
                        WorkerCommand::Bind {
                            call_id,
                            listener_id,
                            path,
                        },
                    )
                }
                CallInput::UnixConnect { path } => {
                    let stream_id = self.next_stream_id;
                    self.next_stream_id += 1;
                    self.send_pending(
                        DriverPending {
                            call_id,
                            lane: LaneKey::OneShot,
                        },
                        WorkerCommand::Connect {
                            call_id,
                            stream_id,
                            path,
                        },
                    )
                }
                CallInput::UnixAccept { listener } => {
                    if !self.open_listeners.contains(&listener.get()) {
                        return self.fail(call_id, CallError::InvalidResource);
                    }
                    let lane = LaneKey::Accept(listener.get());
                    if self.lane_busy(lane) {
                        return self.fail(call_id, CallError::ResourceBusy);
                    }
                    let new_stream_id = self.next_stream_id;
                    self.next_stream_id += 1;
                    self.send_pending(
                        DriverPending { call_id, lane },
                        WorkerCommand::Accept {
                            call_id,
                            listener_id: listener.get(),
                            new_stream_id,
                        },
                    )
                }
                CallInput::UnixRead { stream, max_len } => {
                    if !self.open_streams.contains(&stream.get()) {
                        return self.fail(call_id, CallError::InvalidResource);
                    }
                    let lane = LaneKey::Read(stream.get());
                    if self.lane_busy(lane) {
                        return self.fail(call_id, CallError::ResourceBusy);
                    }
                    self.send_pending(
                        DriverPending { call_id, lane },
                        WorkerCommand::Read {
                            call_id,
                            stream_id: stream.get(),
                            max_len,
                        },
                    )
                }
                CallInput::UnixWrite { stream, bytes } => {
                    if !self.open_streams.contains(&stream.get()) {
                        return self.fail(call_id, CallError::InvalidResource);
                    }
                    let lane = LaneKey::Write(stream.get());
                    if self.lane_busy(lane) {
                        return self.fail(call_id, CallError::ResourceBusy);
                    }
                    self.send_pending(
                        DriverPending { call_id, lane },
                        WorkerCommand::Write {
                            call_id,
                            stream_id: stream.get(),
                            bytes,
                        },
                    )
                }
                CallInput::UnixListenerClose { listener } => {
                    if !self.open_listeners.contains(&listener.get()) {
                        return self.fail(call_id, CallError::InvalidResource);
                    }
                    let result = self.send_pending(
                        DriverPending {
                            call_id,
                            lane: LaneKey::OneShot,
                        },
                        WorkerCommand::CloseListener {
                            call_id,
                            listener_id: listener.get(),
                        },
                    );
                    if result.is_none() {
                        self.open_listeners.remove(&listener.get());
                        self.cancel_lane_ops(LaneKey::Accept(listener.get()));
                    }
                    result
                }
                CallInput::UnixStreamClose { stream } => {
                    if !self.open_streams.contains(&stream.get()) {
                        return self.fail(call_id, CallError::InvalidResource);
                    }
                    let result = self.send_pending(
                        DriverPending {
                            call_id,
                            lane: LaneKey::OneShot,
                        },
                        WorkerCommand::CloseStream {
                            call_id,
                            stream_id: stream.get(),
                        },
                    );
                    if result.is_none() {
                        self.open_streams.remove(&stream.get());
                        self.cancel_lane_ops(LaneKey::Read(stream.get()));
                        self.cancel_lane_ops(LaneKey::Write(stream.get()));
                    }
                    result
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

        fn fail(&self, call_id: CallId, error: CallError) -> Option<DriverCompletion> {
            Some(DriverCompletion {
                call_id,
                result: CallOutput::Failed(error),
            })
        }

        /// Cancels pending ops on a resource lane that is being closed.
        /// Their continuations must not fire; the runtime records
        /// `ResourceClosed` from `take_cancelled_by_close`.
        fn cancel_lane_ops(&mut self, lane: LaneKey) {
            let mut still = Vec::with_capacity(self.pending.len());
            for pending in std::mem::take(&mut self.pending) {
                if pending.lane == lane {
                    self.cancelled_by_close.push(pending.call_id);
                    if let Some(sender) = &self.commands {
                        let _ = sender.try_send(WorkerCommand::CancelOp {
                            call_id: pending.call_id,
                        });
                    }
                } else {
                    still.push(pending);
                }
            }
            self.pending = still;
        }

        pub(crate) fn advance(&mut self, completed: &mut Vec<DriverCompletion>) {
            loop {
                match self.completions.try_recv() {
                    Ok(completion) => self.finish(completion, completed),
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => {
                        self.commands = None;
                        break;
                    }
                }
            }
        }

        fn finish(&mut self, completion: WorkerCompletion, completed: &mut Vec<DriverCompletion>) {
            let Some(index) = self
                .pending
                .iter()
                .position(|pending| pending.call_id == completion.call_id)
            else {
                // Op was cancelled (per-call or by close); drop the result.
                return;
            };
            self.pending.remove(index);
            // Maintain the open-set from successful create completions.
            match &completion.result {
                CallOutput::UnixBound { listener, .. } => {
                    self.open_listeners.insert(listener.get());
                }
                CallOutput::UnixAccepted { stream } | CallOutput::UnixConnected { stream } => {
                    self.open_streams.insert(stream.get());
                }
                _ => {}
            }
            completed.push(DriverCompletion {
                call_id: completion.call_id,
                result: completion.result,
            });
        }

        pub(crate) fn has_pending(&self) -> bool {
            !self.pending.is_empty()
        }

        pub(crate) fn cancel(&mut self, call_id: CallId) -> bool {
            let Some(index) = self
                .pending
                .iter()
                .position(|pending| pending.call_id == call_id)
            else {
                return false;
            };
            self.pending.remove(index);
            if let Some(sender) = &self.commands {
                let _ = sender.try_send(WorkerCommand::CancelOp { call_id });
            }
            true
        }

        pub(crate) fn take_cancelled_by_close(&mut self) -> Vec<CallId> {
            std::mem::take(&mut self.cancelled_by_close)
        }

        pub(crate) fn cancel_pending(&mut self, deadline: Instant) {
            // Drop the command sender so the worker exits once it returns
            // from its current poll tick; drain completions for the budget.
            self.commands = None;
            let mut sink = Vec::new();
            loop {
                self.advance(&mut sink);
                if self.pending.is_empty() || Instant::now() >= deadline {
                    break;
                }
                thread::sleep(Duration::from_millis(1));
            }
            sink.clear();
            if self.handle.as_ref().is_some_and(JoinHandle::is_finished) {
                if let Some(handle) = self.handle.take() {
                    let _ = handle.join();
                }
            }
        }

        pub(crate) fn listener_count(&self) -> usize {
            self.open_listeners.len()
        }

        pub(crate) fn stream_count(&self) -> usize {
            self.open_streams.len()
        }

        pub(crate) fn pending_call_count(&self) -> usize {
            self.pending.len()
        }
    }

    impl Drop for LiveUnixLane {
        fn drop(&mut self) {
            self.cancel_pending(Instant::now());
        }
    }

    // ---- Worker thread -----------------------------------------------------

    struct Worker {
        listeners: Vec<(u64, UnixListener, PathBuf)>,
        streams: Vec<(u64, UnixStream)>,
        pending: Vec<WorkerPending>,
        completions: SyncSender<WorkerCompletion>,
    }

    enum WorkerOp {
        Accept {
            listener_id: u64,
            new_stream_id: u64,
        },
        Read {
            stream_id: u64,
            max_len: usize,
        },
        Write {
            stream_id: u64,
            bytes: Vec<u8>,
            written: usize,
        },
    }

    struct WorkerPending {
        call_id: CallId,
        op: WorkerOp,
    }

    fn worker_loop(commands: Receiver<WorkerCommand>, completions: SyncSender<WorkerCompletion>) {
        let mut worker = Worker {
            listeners: Vec::new(),
            streams: Vec::new(),
            pending: Vec::new(),
            completions,
        };
        loop {
            // Drain all queued commands first.
            let mut disconnected = false;
            loop {
                match commands.try_recv() {
                    Ok(command) => {
                        if worker.handle(command) {
                            return;
                        }
                    }
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => {
                        disconnected = true;
                        break;
                    }
                }
            }

            let progressed = worker.poll_pending();

            if disconnected {
                // No more commands will arrive. Make one final poll pass so
                // already-accepted reads/writes can drain, then exit when
                // nothing is left to do.
                if worker.pending.is_empty() || !progressed {
                    return;
                }
                continue;
            }

            if progressed {
                continue;
            }

            // Nothing to do this tick: block for a new command, waking on
            // the poll interval to retry in-flight non-blocking ops.
            match commands.recv_timeout(POLL_INTERVAL) {
                Ok(command) => {
                    if worker.handle(command) {
                        return;
                    }
                }
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => {
                    if worker.pending.is_empty() {
                        return;
                    }
                }
            }
        }
    }

    impl Worker {
        /// Handles one command. Returns `true` if the worker should exit.
        fn handle(&mut self, command: WorkerCommand) -> bool {
            match command {
                WorkerCommand::Bind {
                    call_id,
                    listener_id,
                    path,
                } => {
                    let result = self.do_bind(listener_id, path);
                    self.complete(call_id, result)
                }
                WorkerCommand::Connect {
                    call_id,
                    stream_id,
                    path,
                } => {
                    let result = self.do_connect(stream_id, path);
                    self.complete(call_id, result)
                }
                WorkerCommand::Accept {
                    call_id,
                    listener_id,
                    new_stream_id,
                } => {
                    self.pending.push(WorkerPending {
                        call_id,
                        op: WorkerOp::Accept {
                            listener_id,
                            new_stream_id,
                        },
                    });
                    false
                }
                WorkerCommand::Read {
                    call_id,
                    stream_id,
                    max_len,
                } => {
                    self.pending.push(WorkerPending {
                        call_id,
                        op: WorkerOp::Read { stream_id, max_len },
                    });
                    false
                }
                WorkerCommand::Write {
                    call_id,
                    stream_id,
                    bytes,
                } => {
                    self.pending.push(WorkerPending {
                        call_id,
                        op: WorkerOp::Write {
                            stream_id,
                            bytes,
                            written: 0,
                        },
                    });
                    false
                }
                WorkerCommand::CloseListener {
                    call_id,
                    listener_id,
                } => {
                    if let Some(index) = self
                        .listeners
                        .iter()
                        .position(|(id, _, _)| *id == listener_id)
                    {
                        let (_, listener, path) = self.listeners.remove(index);
                        drop(listener);
                        // Remove the socket file the listener owned, unless
                        // user code replaced the path while it was open.
                        let _ = remove_socket_file(&path);
                    }
                    self.complete(call_id, CallOutput::UnixListenerClosed)
                }
                WorkerCommand::CloseStream { call_id, stream_id } => {
                    if let Some(index) = self.streams.iter().position(|(id, _)| *id == stream_id) {
                        self.streams.remove(index);
                    }
                    self.complete(call_id, CallOutput::UnixStreamClosed)
                }
                WorkerCommand::CancelOp { call_id } => {
                    self.pending.retain(|pending| pending.call_id != call_id);
                    false
                }
            }
        }

        fn do_bind(&mut self, listener_id: u64, path: PathBuf) -> CallOutput {
            // Best-effort: clear a stale socket file so a re-bind at the
            // same path after an unclean exit succeeds. Never remove a
            // regular file or symlink at a user-supplied path.
            let _ = remove_socket_file(&path);
            match UnixListener::bind(&path) {
                Ok(listener) => {
                    if listener.set_nonblocking(true).is_err() {
                        return CallOutput::Failed(CallError::Io);
                    }
                    self.listeners.push((listener_id, listener, path.clone()));
                    CallOutput::UnixBound {
                        listener: UnixListenerId::new(listener_id),
                        path,
                    }
                }
                Err(_) => CallOutput::Failed(CallError::Io),
            }
        }

        fn do_connect(&mut self, stream_id: u64, path: PathBuf) -> CallOutput {
            match UnixStream::connect(&path) {
                Ok(stream) => {
                    if stream.set_nonblocking(true).is_err() {
                        return CallOutput::Failed(CallError::Io);
                    }
                    self.streams.push((stream_id, stream));
                    CallOutput::UnixConnected {
                        stream: UnixStreamId::new(stream_id),
                    }
                }
                Err(error) if error.kind() == ErrorKind::NotFound => {
                    CallOutput::Failed(CallError::NotFound)
                }
                Err(_) => CallOutput::Failed(CallError::Io),
            }
        }

        /// Tries every pending op once. Returns whether any op completed
        /// (so the loop polls again immediately instead of sleeping).
        fn poll_pending(&mut self) -> bool {
            let mut completed_any = false;
            let mut index = 0;
            while index < self.pending.len() {
                let outcome = self.try_op(index);
                match outcome {
                    OpOutcome::Pending => index += 1,
                    OpOutcome::Done(result) => {
                        let pending = self.pending.remove(index);
                        completed_any = true;
                        if self.complete(pending.call_id, result) {
                            // Completion channel is gone; nothing more to do.
                            return completed_any;
                        }
                    }
                }
            }
            completed_any
        }

        fn try_op(&mut self, index: usize) -> OpOutcome {
            match &mut self.pending[index].op {
                WorkerOp::Accept {
                    listener_id,
                    new_stream_id,
                } => {
                    let listener_id = *listener_id;
                    let new_stream_id = *new_stream_id;
                    let Some((_, listener, _)) =
                        self.listeners.iter().find(|(id, _, _)| *id == listener_id)
                    else {
                        return OpOutcome::Done(CallOutput::Failed(CallError::InvalidResource));
                    };
                    match listener.accept() {
                        Ok((stream, _addr)) => {
                            if stream.set_nonblocking(true).is_err() {
                                return OpOutcome::Done(CallOutput::Failed(CallError::Io));
                            }
                            self.streams.push((new_stream_id, stream));
                            OpOutcome::Done(CallOutput::UnixAccepted {
                                stream: UnixStreamId::new(new_stream_id),
                            })
                        }
                        Err(error) if error.kind() == ErrorKind::WouldBlock => OpOutcome::Pending,
                        Err(_) => OpOutcome::Done(CallOutput::Failed(CallError::Io)),
                    }
                }
                WorkerOp::Read { stream_id, max_len } => {
                    let stream_id = *stream_id;
                    let max_len = *max_len;
                    let Some((_, stream)) =
                        self.streams.iter_mut().find(|(id, _)| *id == stream_id)
                    else {
                        return OpOutcome::Done(CallOutput::Failed(CallError::InvalidResource));
                    };
                    let mut buffer = vec![0u8; max_len];
                    match stream.read(&mut buffer) {
                        Ok(read) => {
                            buffer.truncate(read);
                            OpOutcome::Done(CallOutput::UnixRead { bytes: buffer })
                        }
                        Err(error) if error.kind() == ErrorKind::WouldBlock => OpOutcome::Pending,
                        Err(error) if error.kind() == ErrorKind::Interrupted => OpOutcome::Pending,
                        Err(_) => OpOutcome::Done(CallOutput::Failed(CallError::Io)),
                    }
                }
                WorkerOp::Write {
                    stream_id,
                    bytes,
                    written,
                } => {
                    let stream_id = *stream_id;
                    let Some((_, stream)) =
                        self.streams.iter_mut().find(|(id, _)| *id == stream_id)
                    else {
                        return OpOutcome::Done(CallOutput::Failed(CallError::InvalidResource));
                    };
                    match stream.write(&bytes[*written..]) {
                        Ok(0) => {
                            // Peer cannot accept more and made no progress.
                            OpOutcome::Done(CallOutput::UnixWrote { count: *written })
                        }
                        Ok(count) => {
                            *written += count;
                            // One write call reports the bytes accepted by
                            // this syscall; the helper loops for the rest.
                            OpOutcome::Done(CallOutput::UnixWrote { count: *written })
                        }
                        Err(error) if error.kind() == ErrorKind::WouldBlock => {
                            if *written > 0 {
                                OpOutcome::Done(CallOutput::UnixWrote { count: *written })
                            } else {
                                OpOutcome::Pending
                            }
                        }
                        Err(error) if error.kind() == ErrorKind::Interrupted => OpOutcome::Pending,
                        Err(_) => OpOutcome::Done(CallOutput::Failed(CallError::Io)),
                    }
                }
            }
        }

        /// Sends a completion. Returns `true` if the channel is gone.
        fn complete(&self, call_id: CallId, result: CallOutput) -> bool {
            self.completions
                .send(WorkerCompletion { call_id, result })
                .is_err()
        }
    }

    enum OpOutcome {
        Pending,
        Done(CallOutput),
    }

    fn remove_socket_file(path: &PathBuf) -> std::io::Result<()> {
        match std::fs::symlink_metadata(path) {
            Ok(metadata) if metadata.file_type().is_socket() => std::fs::remove_file(path),
            Ok(_) => Ok(()),
            Err(error) if error.kind() == ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error),
        }
    }
}
