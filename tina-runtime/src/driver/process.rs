//! Process call lane: spawn a child process, capture stdout/stderr,
//! enforce a timeout, and report status back to the driver.

use super::*;

#[cfg(unix)]
use std::os::fd::AsRawFd;
#[cfg(unix)]
use std::os::unix::process::CommandExt;

const PROCESS_DRAIN_JOIN_TIMEOUT: Duration = Duration::from_millis(100);
const PROCESS_DRAIN_CANCEL_JOIN_TIMEOUT: Duration = Duration::from_millis(20);
const PROCESS_DRAIN_POLL_SLEEP: Duration = Duration::from_millis(1);

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

struct DrainHandle {
    handle: JoinHandle<(Vec<u8>, bool)>,
    cancel: Arc<AtomicBool>,
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

#[cfg(target_os = "linux")]
#[allow(unsafe_code)]
fn child_has_exited(child: &mut std::process::Child) -> std::io::Result<bool> {
    // SAFETY: zeroing a POD `siginfo_t` for use as an out-parameter.
    let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
    // SAFETY: `P_PID` + this child's pid; `WNOWAIT` reports exit without
    // consuming the zombie. That keeps the leader pid (also the process-group
    // id) reserved until `process_exited` has cleaned up descendants.
    let result = unsafe {
        libc::waitid(
            libc::P_PID,
            child.id() as libc::id_t,
            &mut info,
            libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
        )
    };
    if result == -1 {
        return Err(std::io::Error::last_os_error());
    }
    // With `WNOHANG`, waitid returns 0 even when no child is ready. A zero
    // si_pid means "not exited yet"; nonzero means the child is waitable.
    // SAFETY: `info` was initialized and passed to a successful `waitid` call;
    // libc's accessor reads the active `si_pid` field without outliving it.
    let si_pid = unsafe { info.si_pid() };
    Ok(si_pid != 0)
}

#[cfg(not(target_os = "linux"))]
fn child_has_exited(child: &mut std::process::Child) -> std::io::Result<bool> {
    // Non-Linux has no portable WNOWAIT equivalent here. `try_wait` reaps, and
    // `Child::wait` later returns the cached status.
    Ok(child.try_wait()?.is_some())
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

    loop {
        if command.cancelled.load(Ordering::Acquire) {
            return kill_and_reap(child, stdout, stderr, CallError::Timeout);
        }
        if started.elapsed() >= command.timeout {
            return kill_and_reap(child, stdout, stderr, CallError::Timeout);
        }
        match child_has_exited(&mut child) {
            Ok(true) => break,
            Ok(false) => thread::sleep(Duration::from_millis(1)),
            Err(_) => return kill_and_reap(child, stdout, stderr, CallError::KillUncertain),
        }
    }

    process_exited(child, stdout, stderr, process_group)
}

fn kill_and_reap(
    mut child: std::process::Child,
    stdout: Option<DrainHandle>,
    stderr: Option<DrainHandle>,
    fallback: CallError,
) -> CallOutput {
    // SIGKILL the leader directly first, while its pid is live. A process-
    // group kill alone is not enough: it is best-effort and does not reliably
    // reach the leader on every platform/pgid setup. When it "succeeds" but
    // misses the leader, the leader keeps running until natural exit — the old
    // unbounded `wait()` silently masked that by blocking until then; the
    // bounded reap below would otherwise report `KillUncertain` for a child we
    // never actually killed.
    let killed_leader = child.kill().is_ok();
    // Best-effort: kill the whole group so descendants that inherited the
    // pipes die too. Done before reaping, so the leader's pid (== pgid) is
    // still reserved and we cannot signal a recycled group.
    #[cfg(unix)]
    let _ = kill_process_group(child.id());

    if !killed_leader {
        // The direct kill failed, almost always because the child had already
        // exited on its own. Report its real status if we can reap it;
        // otherwise fall back to the caller's typed terminal.
        let process_group = child.id();
        return match child.try_wait() {
            Ok(Some(_)) => process_exited(child, stdout, stderr, process_group),
            // Child still alive (kill and reap both failed). Spend the bounded
            // drain budget so the drain threads observe EOF or time out instead
            // of being detached forever and leaking a thread + pipe fd each.
            Ok(None) => kill_uncertain(stdout, stderr),
            Err(_) => kill_uncertain_with(stdout, stderr, fallback),
        };
    }

    // Bound the post-kill reap. A blocking `child.wait()` has no deadline, so a
    // wedged reap (D-state child, or a SIGKILL that did not land) would pin the
    // process worker thread forever; on Drop that thread is detached and leaks.
    let reap_deadline = Instant::now() + PROCESS_KILL_REAP_TIMEOUT;
    loop {
        match child.try_wait() {
            // We reaped it, or it is gone. `Err` is the runtime's own child
            // reaper winning the race after our kill — `try_wait` then has no
            // child to wait on. A vanished child after a kill is a completed
            // kill, so both cases settle as the caller's typed `fallback`.
            Ok(Some(_)) | Err(_) => break,
            Ok(None) => {
                if Instant::now() >= reap_deadline {
                    // Same leak hazard as above: bound the drains before giving up.
                    return kill_uncertain(stdout, stderr);
                }
                thread::sleep(Duration::from_millis(1));
            }
        }
    }
    drain_and_discard(stdout, stderr);
    CallOutput::Failed(fallback)
}

/// Spend the bounded drain budget on the stdout/stderr handles, then report
/// `KillUncertain`. Every `KillUncertain` exit must route through here (or
/// `kill_uncertain_with`) so the drain threads are cancelled and joined rather
/// than dropped. Dropping a `JoinHandle` detaches a thread that, with the child
/// still alive, can block in `read()` forever and leak a pipe fd.
fn kill_uncertain(stdout: Option<DrainHandle>, stderr: Option<DrainHandle>) -> CallOutput {
    kill_uncertain_with(stdout, stderr, CallError::KillUncertain)
}

fn kill_uncertain_with(
    stdout: Option<DrainHandle>,
    stderr: Option<DrainHandle>,
    error: CallError,
) -> CallOutput {
    drain_and_discard(stdout, stderr);
    CallOutput::Failed(error)
}

/// Join both drain handles under the bounded budget and throw the bytes away.
/// Used by the terminal failure paths, which report a typed error, not output.
fn drain_and_discard(stdout: Option<DrainHandle>, stderr: Option<DrainHandle>) {
    let _ = join_drain_bounded(stdout, PROCESS_DRAIN_JOIN_TIMEOUT);
    let _ = join_drain_bounded(stderr, PROCESS_DRAIN_JOIN_TIMEOUT);
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
#[allow(unsafe_code)]
fn kill_process_group(pid: u32) -> std::io::Result<()> {
    let pgid = libc::pid_t::try_from(pid)
        .map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidInput, "pid overflow"))?;
    // SAFETY: `pgid` is a positive process-group identifier derived from the
    // live child created by this lane; `killpg` borrows no Rust memory.
    let result = unsafe { libc::killpg(pgid, libc::SIGKILL) };
    if result == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

fn process_exited(
    mut child: std::process::Child,
    stdout: Option<DrainHandle>,
    stderr: Option<DrainHandle>,
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
            //
            // On Linux, normal completion only peeked the leader exit with
            // `waitid(WNOWAIT)`, so the leader stays as a zombie and its pid
            // still reserves this process-group id while we signal the group.
            // On other Unix platforms the poll path already reaped the leader,
            // so a narrow pid-reuse race remains documented here.
            let _ = kill_process_group(process_group);
        }
    }
    let code = child.wait().ok().and_then(|status| status.code());
    CallOutput::ProcessExited {
        status: ProcessStatus { code },
        stdout,
        stderr,
        stdout_truncated,
        stderr_truncated,
    }
}

#[cfg(unix)]
#[allow(unsafe_code)]
fn set_nonblocking_fd(fd: std::os::fd::RawFd) -> std::io::Result<()> {
    // SAFETY: `fd` is borrowed from a live pipe/stream owned by the drain
    // reader. `fcntl` does not take ownership.
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags < 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: same borrowed fd; this only updates the descriptor flags.
    let result = unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) };
    if result < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(unix)]
fn spawn_drain_limited<R>(reader: R, limit: usize) -> DrainHandle
where
    R: Read + AsRawFd + Send + 'static,
{
    if set_nonblocking_fd(reader.as_raw_fd()).is_err() {
        return finished_drain(Vec::new(), true);
    }
    spawn_drain_thread(reader, limit)
}

#[cfg(not(unix))]
fn spawn_drain_limited<R>(reader: R, limit: usize) -> DrainHandle
where
    R: Read + Send + 'static,
{
    spawn_drain_thread(reader, limit)
}

fn finished_drain(bytes: Vec<u8>, truncated: bool) -> DrainHandle {
    DrainHandle {
        handle: thread::spawn(move || (bytes, truncated)),
        cancel: Arc::new(AtomicBool::new(false)),
    }
}

fn spawn_drain_thread<R>(mut reader: R, limit: usize) -> DrainHandle
where
    R: Read + Send + 'static,
{
    let cancel = Arc::new(AtomicBool::new(false));
    let thread_cancel = Arc::clone(&cancel);
    let handle = thread::spawn(move || {
        let mut captured = Vec::with_capacity(limit.min(8192));
        let mut truncated = false;
        let mut buffer = [0_u8; 8192];
        loop {
            if thread_cancel.load(Ordering::Acquire) {
                truncated = true;
                break;
            }
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
                Err(error)
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::Interrupted
                    ) =>
                {
                    thread::sleep(PROCESS_DRAIN_POLL_SLEEP);
                }
                Err(_) => {
                    truncated = true;
                    break;
                }
            }
        }
        (captured, truncated)
    });
    DrainHandle { handle, cancel }
}

fn join_drain_bounded(handle: Option<DrainHandle>, timeout: Duration) -> (Vec<u8>, bool) {
    let Some(handle) = handle else {
        return (Vec::new(), false);
    };
    let deadline = deadline_after(Instant::now(), timeout);
    let mut handle = Some(handle);
    while Instant::now() < deadline {
        if handle
            .as_ref()
            .is_some_and(|drain| drain.handle.is_finished())
        {
            return handle.take().unwrap().handle.join().unwrap_or_default();
        }
        thread::sleep(Duration::from_millis(1));
    }
    let Some(drain) = handle.take() else {
        return (Vec::new(), true);
    };
    drain.cancel.store(true, Ordering::Release);
    let cancel_deadline = Instant::now() + PROCESS_DRAIN_CANCEL_JOIN_TIMEOUT;
    let mut drain = Some(drain);
    while Instant::now() < cancel_deadline {
        if drain
            .as_ref()
            .is_some_and(|drain| drain.handle.is_finished())
        {
            return drain.take().unwrap().handle.join().unwrap_or_default();
        }
        thread::sleep(Duration::from_millis(1));
    }
    (Vec::new(), true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    use std::os::unix::net::UnixStream;

    #[cfg(unix)]
    fn idle_pipe_drain() -> (DrainHandle, UnixStream) {
        let (reader, writer) = UnixStream::pair().expect("unix stream pair");
        (spawn_drain_limited(reader, 1024), writer)
    }

    // The KillUncertain path must cancel and join the stdout/stderr drains, not
    // drop (detach) them. The writer stays open, so EOF never arrives; returning
    // proves the drain observed cancellation and the thread was joined.
    #[cfg(unix)]
    #[test]
    fn kill_uncertain_cancels_and_joins_idle_drains() {
        let (stdout, _stdout_writer) = idle_pipe_drain();
        let (stderr, _stderr_writer) = idle_pipe_drain();

        let started = Instant::now();
        let out = kill_uncertain(Some(stdout), Some(stderr));
        let elapsed = started.elapsed();

        assert!(matches!(out, CallOutput::Failed(CallError::KillUncertain)));
        assert!(
            elapsed >= PROCESS_DRAIN_JOIN_TIMEOUT,
            "expected bounded join to spend the drain budget, took {elapsed:?}",
        );
        assert!(
            elapsed < PROCESS_DRAIN_JOIN_TIMEOUT * 3,
            "cancelled drains should join promptly, took {elapsed:?}",
        );
    }

    // The typed-fallback uncertain path (try_wait Err with the child alive) must
    // also cancel and join the drains rather than detach them.
    #[cfg(unix)]
    #[test]
    fn kill_uncertain_with_fallback_cancels_and_joins_idle_drains() {
        let (stdout, _stdout_writer) = idle_pipe_drain();
        let (stderr, _stderr_writer) = idle_pipe_drain();

        let started = Instant::now();
        let out = kill_uncertain_with(Some(stdout), Some(stderr), CallError::Timeout);
        let elapsed = started.elapsed();

        assert!(matches!(out, CallOutput::Failed(CallError::Timeout)));
        assert!(
            elapsed >= PROCESS_DRAIN_JOIN_TIMEOUT,
            "expected bounded join to spend the drain budget, took {elapsed:?}",
        );
        assert!(
            elapsed < PROCESS_DRAIN_JOIN_TIMEOUT * 3,
            "cancelled drains should join promptly, took {elapsed:?}",
        );
    }

    // A finished drain handle is joined immediately and its bytes recovered —
    // the bounded join is not a fixed sleep, it observes completion.
    #[test]
    fn finished_drain_joins_without_spending_full_budget() {
        let handle = finished_drain(b"out".to_vec(), false);
        // Give the thread a moment to finish so the join sees it ready.
        while !handle.handle.is_finished() {
            thread::sleep(Duration::from_millis(1));
        }
        let started = Instant::now();
        let (bytes, truncated) = join_drain_bounded(Some(handle), PROCESS_DRAIN_JOIN_TIMEOUT);
        assert_eq!(bytes, b"out");
        assert!(!truncated);
        assert!(started.elapsed() < PROCESS_DRAIN_JOIN_TIMEOUT);
    }
}
