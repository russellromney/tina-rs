//! Process call lane: spawn a child process, capture stdout/stderr,
//! enforce a timeout, and report status back to the driver.

use super::*;

#[cfg(unix)]
use std::os::unix::process::CommandExt;

const PROCESS_DRAIN_JOIN_TIMEOUT: Duration = Duration::from_millis(100);

/// Budget for reaping a child after we SIGKILL it. A reap that does not
/// complete in this window (D-state child, or a kill that did not land) must
/// not pin the process worker forever; we give up and report `KillUncertain`.
const PROCESS_KILL_REAP_TIMEOUT: Duration = Duration::from_secs(2);

pub(super) enum ProcessLane {
    Worker(ProcessWorkerLane),
}

pub(super) struct ProcessWorkerLane {
    capacity: usize,
    sender: Option<SyncSender<ProcessCommand>>,
    completions: Receiver<ProcessCompletion>,
    handle: Option<JoinHandle<()>>,
    pending: Vec<ProcessPending>,
}

struct ProcessPending {
    call_id: CallId,
    cancelled: Arc<AtomicBool>,
}

pub(super) struct ProcessCommand {
    pub(super) call_id: CallId,
    pub(super) command: String,
    pub(super) args: Vec<String>,
    pub(super) timeout: Duration,
    pub(super) stdout_limit: usize,
    pub(super) stderr_limit: usize,
    pub(super) cancelled: Arc<AtomicBool>,
}

struct ProcessCompletion {
    call_id: CallId,
    result: CallOutput,
}

impl ProcessLane {
    pub(super) fn new(capacity: usize) -> Self {
        Self::Worker(ProcessWorkerLane::new(capacity))
    }

    pub(super) fn submit(
        &mut self,
        call_id: CallId,
        command: ProcessCommand,
    ) -> Option<DriverCompletion> {
        match self {
            Self::Worker(lane) => lane.submit(call_id, command),
        }
    }

    pub(super) fn advance(&mut self, completed: &mut Vec<DriverCompletion>) {
        match self {
            Self::Worker(lane) => lane.advance(completed),
        }
    }

    pub(super) fn has_pending(&self) -> bool {
        match self {
            Self::Worker(lane) => lane.has_pending(),
        }
    }

    pub(super) fn cancel(&mut self, call_id: CallId) -> bool {
        match self {
            Self::Worker(lane) => lane.cancel(call_id),
        }
    }

    pub(super) fn cancel_pending(&mut self, deadline: Instant) {
        match self {
            Self::Worker(lane) => lane.cancel_pending(deadline),
        }
    }

    pub(super) fn physical_pending_count(&self) -> usize {
        match self {
            Self::Worker(lane) => lane.physical_pending_count(),
        }
    }
}

impl Drop for ProcessLane {
    fn drop(&mut self) {
        self.cancel_pending(Instant::now());
    }
}

impl ProcessWorkerLane {
    fn new(capacity: usize) -> Self {
        assert!(capacity > 0, "process lane capacity must be > 0");
        let (sender, receiver) = sync_channel(capacity);
        let (completion_sender, completions) = sync_channel(capacity.saturating_add(1));
        let handle = thread::spawn(move || process_worker_loop(receiver, completion_sender));
        Self {
            capacity,
            sender: Some(sender),
            completions,
            handle: Some(handle),
            pending: Vec::with_capacity(capacity.min(INITIAL_DRIVER_PENDING_CAPACITY)),
        }
    }

    fn submit(&mut self, call_id: CallId, mut command: ProcessCommand) -> Option<DriverCompletion> {
        let Some(sender) = &self.sender else {
            return Some(DriverCompletion {
                call_id,
                result: CallOutput::Failed(CallError::ProcessClosed),
            });
        };
        if self.active_pending_count() >= self.capacity {
            return Some(DriverCompletion {
                call_id,
                result: CallOutput::Failed(CallError::ProcessFull),
            });
        }

        let cancelled = Arc::clone(&command.cancelled);
        command.call_id = call_id;
        match sender.try_send(command) {
            Ok(()) => {
                self.pending.push(ProcessPending { call_id, cancelled });
                None
            }
            Err(MpscTrySendError::Full(command)) => Some(DriverCompletion {
                call_id: command.call_id,
                result: CallOutput::Failed(CallError::ProcessFull),
            }),
            Err(MpscTrySendError::Disconnected(command)) => Some(DriverCompletion {
                call_id: command.call_id,
                result: CallOutput::Failed(CallError::ProcessClosed),
            }),
        }
    }

    fn advance(&mut self, completed: &mut Vec<DriverCompletion>) {
        loop {
            match self.completions.try_recv() {
                Ok(completion) => self.finish_completion(completion, completed),
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    self.sender = None;
                    break;
                }
            }
        }
    }

    fn finish_completion(
        &mut self,
        completion: ProcessCompletion,
        completed: &mut Vec<DriverCompletion>,
    ) {
        let Some(index) = self
            .pending
            .iter()
            .position(|entry| entry.call_id == completion.call_id)
        else {
            return;
        };
        let pending = self.pending.remove(index);
        if pending.cancelled.load(Ordering::Acquire) {
            return;
        }
        completed.push(DriverCompletion {
            call_id: completion.call_id,
            result: completion.result,
        });
    }

    fn has_pending(&self) -> bool {
        self.active_pending_count() > 0
    }

    fn cancel(&mut self, call_id: CallId) -> bool {
        let Some(pending) = self
            .pending
            .iter_mut()
            .find(|entry| entry.call_id == call_id && !entry.cancelled.load(Ordering::Acquire))
        else {
            return false;
        };
        pending.cancelled.store(true, Ordering::Release);
        true
    }

    fn cancel_pending(&mut self, deadline: Instant) {
        // Signal cancellation to in-flight commands (the worker checks
        // the flag and aborts the child cooperatively). Drop the command
        // sender so the worker thread can exit. Drain completions for
        // the budget; surviving `self.pending` is stuck work that holds
        // a live `std::process::Child` and will surface as both
        // `worker_held` and `pending_calls` in `resource_report`.
        for pending in &mut self.pending {
            pending.cancelled.store(true, Ordering::Release);
        }
        self.sender = None;
        let mut sink = Vec::new();
        loop {
            self.drain_into_sink(&mut sink);
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

    fn drain_into_sink(&mut self, sink: &mut Vec<DriverCompletion>) {
        while let Ok(completion) = self.completions.try_recv() {
            self.finish_completion(completion, sink);
        }
    }

    fn active_pending_count(&self) -> usize {
        self.pending
            .iter()
            .filter(|entry| !entry.cancelled.load(Ordering::Acquire))
            .count()
    }

    fn physical_pending_count(&self) -> usize {
        self.pending.len()
    }
}

impl Drop for ProcessWorkerLane {
    fn drop(&mut self) {
        self.cancel_pending(Instant::now());
    }
}

fn process_worker_loop(
    receiver: Receiver<ProcessCommand>,
    completions: SyncSender<ProcessCompletion>,
) {
    while let Ok(command) = receiver.recv() {
        if command.cancelled.load(Ordering::Acquire) {
            continue;
        }
        let call_id = command.call_id;
        let result = execute_process_command(command);
        let completion = ProcessCompletion { call_id, result };
        if completions.send(completion).is_err() {
            break;
        }
    }
}

fn execute_process_command(command: ProcessCommand) -> CallOutput {
    if command.timeout.is_zero() {
        return CallOutput::Failed(CallError::Timeout);
    }

    let mut child = match spawn_process(&command) {
        Ok(child) => child,
        Err(_) => return CallOutput::Failed(CallError::Io),
    };

    let stdout = child
        .stdout
        .take()
        .map(|pipe| spawn_drain_limited(pipe, command.stdout_limit));
    let stderr = child
        .stderr
        .take()
        .map(|pipe| spawn_drain_limited(pipe, command.stderr_limit));
    let process_group = child.id();
    let started = Instant::now();

    let status = loop {
        if command.cancelled.load(Ordering::Acquire) {
            return kill_and_reap(child, stdout, stderr, CallError::Timeout);
        }
        if started.elapsed() >= command.timeout {
            return kill_and_reap(child, stdout, stderr, CallError::Timeout);
        }
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => thread::sleep(Duration::from_millis(1)),
            Err(_) => return kill_and_reap(child, stdout, stderr, CallError::KillUncertain),
        }
    };

    process_exited(status, stdout, stderr, process_group)
}

fn kill_and_reap(
    mut child: std::process::Child,
    stdout: Option<JoinHandle<(Vec<u8>, bool)>>,
    stderr: Option<JoinHandle<(Vec<u8>, bool)>>,
    fallback: CallError,
) -> CallOutput {
    #[cfg(unix)]
    let killed_group = kill_process_group(child.id()).is_ok();
    #[cfg(not(unix))]
    let killed_group = false;

    if !killed_group && child.kill().is_err() {
        return match child.try_wait() {
            Ok(Some(status)) => process_exited(status, stdout, stderr, child.id()),
            Ok(None) | Err(_) => CallOutput::Failed(CallError::KillUncertain),
        };
    }
    // Bound the post-kill reap. A blocking `child.wait()` has no deadline, so a
    // wedged reap (D-state child, or a SIGKILL that did not land) would pin the
    // process worker thread forever; on Drop that thread is detached and leaks.
    let reap_deadline = Instant::now() + PROCESS_KILL_REAP_TIMEOUT;
    loop {
        match child.try_wait() {
            // We reaped it, or it is simply gone. `Err` here is the
            // runtime's own child reaper winning the race after we issued
            // the kill — `try_wait` then has no child to wait on. A vanished
            // child after a kill is a completed kill, not an uncertain one,
            // so both cases fall through to the caller's typed `fallback`.
            Ok(Some(_)) | Err(_) => break,
            Ok(None) => {
                if Instant::now() >= reap_deadline {
                    return CallOutput::Failed(CallError::KillUncertain);
                }
                thread::sleep(Duration::from_millis(1));
            }
        }
    }
    let _ = join_drain_bounded(stdout, PROCESS_DRAIN_JOIN_TIMEOUT);
    let _ = join_drain_bounded(stderr, PROCESS_DRAIN_JOIN_TIMEOUT);
    CallOutput::Failed(fallback)
}

fn spawn_process(command: &ProcessCommand) -> std::io::Result<std::process::Child> {
    let mut builder = Command::new(&command.command);
    builder
        .args(&command.args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(unix)]
    {
        // Put the child in its own process group so timeout/cancel can
        // kill descendants that inherited stdout/stderr.
        builder.process_group(0);
    }
    builder.spawn()
}

#[cfg(unix)]
fn kill_process_group(pid: u32) -> std::io::Result<()> {
    let status = Command::new("kill")
        .arg("-KILL")
        .arg(format!("-{pid}"))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()?;
    if status.success() {
        Ok(())
    } else {
        Err(std::io::Error::other("kill process group failed"))
    }
}

fn process_exited(
    status: std::process::ExitStatus,
    stdout: Option<JoinHandle<(Vec<u8>, bool)>>,
    stderr: Option<JoinHandle<(Vec<u8>, bool)>>,
    process_group: u32,
) -> CallOutput {
    let (stdout, stdout_truncated) = join_drain_bounded(stdout, PROCESS_DRAIN_JOIN_TIMEOUT);
    let (stderr, stderr_truncated) = join_drain_bounded(stderr, PROCESS_DRAIN_JOIN_TIMEOUT);
    #[cfg(unix)]
    {
        if stdout_truncated || stderr_truncated {
            // The process we spawned has exited, but descendants may still
            // hold stdout/stderr pipes open. `process_run` owns the whole
            // process group; do not let a background grandchild escape the
            // runtime rail after the bounded drain budget is spent.
            let _ = kill_process_group(process_group);
        }
    }
    CallOutput::ProcessExited {
        status: ProcessStatus {
            code: status.code(),
        },
        stdout,
        stderr,
        stdout_truncated,
        stderr_truncated,
    }
}

fn spawn_drain_limited<R>(mut reader: R, limit: usize) -> JoinHandle<(Vec<u8>, bool)>
where
    R: Read + Send + 'static,
{
    thread::spawn(move || {
        let mut captured = Vec::with_capacity(limit.min(8192));
        let mut truncated = false;
        let mut buffer = [0_u8; 8192];
        loop {
            match reader.read(&mut buffer) {
                Ok(0) => break,
                Ok(count) => {
                    let remaining = limit.saturating_sub(captured.len());
                    let take = remaining.min(count);
                    captured.extend_from_slice(&buffer[..take]);
                    if take < count {
                        truncated = true;
                    }
                }
                Err(_) => {
                    truncated = true;
                    break;
                }
            }
        }
        (captured, truncated)
    })
}

fn join_drain_bounded(
    handle: Option<JoinHandle<(Vec<u8>, bool)>>,
    timeout: Duration,
) -> (Vec<u8>, bool) {
    let Some(handle) = handle else {
        return (Vec::new(), false);
    };
    let deadline = Instant::now() + timeout;
    let mut handle = Some(handle);
    while Instant::now() < deadline {
        if handle.as_ref().is_some_and(JoinHandle::is_finished) {
            return handle.take().unwrap().join().unwrap_or_default();
        }
        thread::sleep(Duration::from_millis(1));
    }
    (Vec::new(), true)
}
