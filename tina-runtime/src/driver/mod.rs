//! Runtime-owned substrate driver for `tina-runtime`.
//!
//! Tina keeps isolate scheduling, mailboxes, tracing, supervision, and call
//! outcome delivery in [`crate::Runtime`]. The driver owns only substrate
//! operations: timers, TCP resources, storage work, completion readiness, and
//! cancellation.
//!
//! ## Contract with the rest of the runtime
//!
//! - one [`RuntimeDriver::submit`] per [`CallInput`] issued by an isolate.
//! - one [`RuntimeDriver::advance`] per [`crate::Runtime::step`].
//! - the driver appends [`DriverCompletion`] values into runtime-owned scratch
//!   storage; `Runtime` translates them into ordinary later-turn messages and
//!   trace events.
//! - resource ids ([`ListenerId`], [`StreamId`]) are runtime-assigned
//!   monotonic counters, not OS file descriptors. Isolate code never sees
//!   raw fds or `Box<dyn IOSocket>` values.
//! - operations are classified by blocking shape:
//!   - inline-safe: small runtime bookkeeping such as TCP close;
//!   - driver-completion: Betelgeuse completion-shaped TCP and file calls;
//!   - storage-lane: snapshot/journal helpers that can block on local durable
//!     filesystem work and therefore use a bounded worker lane;
//!   - forbidden-in-handler: direct filesystem or socket work performed by
//!     user handlers instead of returned Tina effects.
//! - shutdown cancellation keeps completion slots alive until the backend
//!   reports that it no longer owns their raw pointers. A driver that cannot
//!   prove release returns [`DriverShutdownError`] instead of pretending
//!   shutdown was clean.
//!
use std::alloc::Global;
use std::collections::BTreeMap;
use std::io::{ErrorKind, Read};
use std::net::{SocketAddr, ToSocketAddrs, UdpSocket};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::mpsc::{
    Receiver, SyncSender, TryRecvError, TrySendError as MpscTrySendError, sync_channel,
};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
// Only lane unit tests build `Arc<Mutex<_>>` resolver fixtures; the live
// driver no longer holds a `Mutex` now that TLS rides the TCP rail.
#[cfg(test)]
use std::sync::Mutex;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use betelgeuse::{
    AcceptCompletion, ConnectCompletion, FsyncCompletion, IO, IOFile, IOLoop, IOLoopHandle,
    IOSocket, MkdirCompletion, OpenOptions, PReadCompletion, PWriteCompletion,
    PWriteOwnedCompletion, RecvBufCompletion, RecvCompletion, SendCompletion, SendOwnedCompletion,
    SizeCompletion, io_loop,
};

use crate::call::{
    CallError, CallId, CallInput, CallOutput, FileId, FileOpenOptions, ListenerId, PathKind,
    PathMetadata, ProcessStatus, StreamId, TlsListenerId, TlsStreamId, UdpSocketId,
};

mod dns;
mod process;
mod signals;
mod storage;
mod tcp;
mod tls;
mod unix;

use dns::DnsLane;
#[cfg(test)]
use dns::DnsWorkerLane;
use process::{ProcessCommand, ProcessLane};
pub use signals::os_signal_capture_supported;
use signals::{OsSignalDispatcher, SignalWaitEntry};
use storage::{StorageJob, StorageLane};
use tcp::BetelgeuseTcp;
use tls::TlsLane;
use unix::UnixLane;

fn deadline_after(now: Instant, timeout: Duration) -> Instant {
    tina::Deadline::from_instant(now, timeout).instant()
}

const INITIAL_DRIVER_RESOURCE_CAPACITY: usize = 4;
const INITIAL_DRIVER_PENDING_CAPACITY: usize = 8;
pub(crate) const DEFAULT_STORAGE_LANE_CAPACITY: usize = 64;
pub(crate) const DEFAULT_DNS_LANE_CAPACITY: usize = 16;
pub(crate) const DEFAULT_TLS_LANE_CAPACITY: usize = 64;
pub(crate) const DEFAULT_PROCESS_LANE_CAPACITY: usize = 16;
pub(crate) const DEFAULT_SIGNAL_CAPACITY: usize = 64;
/// Generous default cap on concurrently armed runtime timers per shard. Timer
/// admission is bounded so a runaway isolate cannot grow the timer lane without
/// limit; a full lane refuses the arm with [`CallError::TimerFull`] instead.
/// Generous enough that healthy workloads never see it.
pub(crate) const DEFAULT_DRIVER_TIMER_CAPACITY: usize = 262_144;
/// Max due timers a single `advance` harvests into the completion carry before
/// yielding the shard. A synchronized batch larger than this fires across
/// several ticks (deterministic order preserved) rather than monopolising one
/// advance. Generous so a normal warm turn drains all its due timers at once.
pub(crate) const DEFAULT_TIMER_HARVEST_BUDGET: usize = 1_024;

/// Runtime-owned substrate driver.
///
/// A driver must not run user isolate code, own isolate mailboxes, or hide an
/// unbounded executor behind Tina. It owns only substrate calls submitted by
/// [`crate::Runtime`], advances them when the runtime asks, and returns typed
/// completions for the runtime to deliver on later turns.
///
/// TCP drivers treat listener accept, stream read, and stream write as
/// separate lanes. Duplicate work on one lane fails with
/// [`CallError::ResourceBusy`]. Close cancels pending work on the
/// resource and closes; the cancelled caller's continuation does not
/// fire. Per-call cancel stops the runtime from waiting on this id;
/// it does not invalidate other lanes.
pub(crate) trait RuntimeDriver: std::fmt::Debug {
    /// Submits one runtime-owned call.
    fn submit(
        &mut self,
        call_id: CallId,
        request: CallInput,
        now: Instant,
    ) -> Option<DriverCompletion>;

    /// Advances the substrate by one runtime step and appends ready
    /// completions in deterministic order.
    fn advance(&mut self, now: Instant, completed: &mut Vec<DriverCompletion>);

    /// Returns whether substrate completions are still pending.
    fn has_pending(&self) -> bool;

    /// Cancels pending substrate operations during runtime shutdown.
    ///
    /// `deadline` bounds how long lane workers may keep draining after
    /// cancellation has been signaled. If the deadline elapses before a
    /// lane finishes, shutdown returns; the lane's residual work shows up
    /// on the next [`resource_report`](Self::resource_report) call so the
    /// terminal shutdown report can name what was left.
    ///
    /// After this returns `Ok(())`, the driver may be dropped without leaving
    /// backend-owned pointers to driver-owned completion storage. `Err` means
    /// shutdown reached a typed lifecycle failure.
    fn cancel_pending(&mut self, deadline: Instant) -> Result<(), DriverShutdownError>;

    /// Cancels one runtime-owned call by id.
    ///
    /// Cancellation removes completion delivery responsibility from the
    /// driver. It is not a promise that an already-submitted substrate side
    /// effect, such as a TCP write handed to the OS, can be undone.
    fn cancel(&mut self, call_id: CallId) -> bool;

    /// Drains call ids the close path silently cancelled. Runtime
    /// drops their tracking and records `ResourceClosed`.
    fn take_cancelled_by_close(&mut self) -> Vec<CallId> {
        Vec::new()
    }

    /// Injects one runtime-owned signal event and appends ready completions.
    fn notify_signal(&mut self, _name: &str, _completed: &mut Vec<DriverCompletion>) {}

    /// Returns the driver-owned resource inventory visible to live reports.
    fn resource_report(&self) -> DriverResourceReport {
        DriverResourceReport::default()
    }

    #[cfg(test)]
    fn io_pending_count(&self) -> usize {
        0
    }
}

/// Driver-owned resource inventory.
///
/// Three independent count vocabularies:
///
/// * **Table-owned resources** (`listeners`, `streams`, `tls_streams`,
///   `udp_sockets`, `files`): runtime-table ids handed back to user code.
///   `owned_resource_count()` sums these.
/// * **Worker-held resources** (`worker_held`): clones of OS handles or
///   `std::process::Child` parked inside in-flight lane work that is not
///   represented by a table id. TLS in-flight ops hold cloned
///   listener/stream `Arc`s; process calls hold a live `Child`.
/// * **Pending driver calls** (`pending_calls`): runtime-owned operations
///   waiting for completion. Includes table-owned ops (TCP read/write,
///   file ops, UDP recv), TLS ops, DNS lookups, storage jobs, process
///   calls, signal waits, and timers.
///
/// Worker-held and pending may overlap (TLS in-flight is both) but
/// table-owned never overlaps the other two.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct DriverResourceReport {
    pub(crate) listeners: usize,
    pub(crate) streams: usize,
    pub(crate) tls_streams: usize,
    pub(crate) udp_sockets: usize,
    pub(crate) files: usize,
    pub(crate) worker_held: usize,
    pub(crate) pending_calls: usize,
}

impl DriverResourceReport {
    pub(crate) const fn owned_resource_count(self) -> usize {
        self.listeners + self.streams + self.tls_streams + self.udp_sockets + self.files
    }

    pub(crate) const fn worker_held_resource_count(self) -> usize {
        self.worker_held
    }

    pub(crate) const fn pending_driver_call_count(self) -> usize {
        self.pending_calls
    }
}

/// Driver-level shutdown failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DriverShutdownError {
    /// The backend still reports ownership of completion slots after Tina's
    /// bounded shutdown drain.
    BackendStillOwnsCompletions,
}

/// Betelgeuse-backed runtime driver.
///
/// Betelgeuse exposes a `step()`-driven, no-runtime, no-hidden-tasks I/O
/// library with caller-owned typed completion slots. That shape matches Tina's
/// explicit-stepping, runtime-owned-effects discipline.
pub(crate) struct BetelgeuseDriver {
    tcp: BetelgeuseTcp,
    storage: StorageLane,
    dns: DnsLane,
    tls: TlsLane,
    process: ProcessLane,
    unix: UnixLane,
    signals: Vec<SignalWaitEntry>,
    signal_capacity: usize,
    /// Pending timers keyed by `(deadline, insertion_order)` so the earliest
    /// due timer is `first_key_value` in O(log n) — no linear scan. The
    /// `insertion_order` tie-break preserves the exact same-deadline pop order
    /// the old linear `min_by(deadline, insertion_order)` scan produced: equal
    /// deadlines fire in submission (FIFO) order. Newly armed timers always
    /// have `deadline >= now` and a strictly larger `insertion_order`, so they
    /// can never sort ahead of an already-due timer — which is why budgeted
    /// harvesting yields byte-identical delivery order to the old all-at-once
    /// harvest.
    timers: BTreeMap<TimerKey, CallId>,
    next_timer_ordinal: u64,
    /// Bounded admission: refuse a new arm once this many timers are pending.
    timer_capacity: usize,
    /// Bounded per-advance harvest work (see [`DEFAULT_TIMER_HARVEST_BUDGET`]).
    timer_harvest_budget: usize,
    os_signals: OsSignalDispatcher,
}

/// Ordered timer key: earliest `deadline` first, then earliest
/// `insertion_order` (submission FIFO) for equal deadlines. `Instant` and `u64`
/// are both `Ord`, and `insertion_order` is a per-driver monotonic counter, so
/// every key is unique and totally ordered.
type TimerKey = (Instant, u64);

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
            // TLS rides the same Betelgeuse loop as plain TCP — a cloned
            // handle, not a second socket stack.
            tls: TlsLane::new(DEFAULT_TLS_LANE_CAPACITY, io_loop.clone()),
            // Unix-domain sockets ride the same Betelgeuse loop as TCP/TLS —
            // a cloned handle, not a second socket stack.
            unix: UnixLane::new(io_loop.clone()),
            tcp: BetelgeuseTcp::with_io_loop(io_loop),
            storage: StorageLane::inline(),
            dns: DnsLane::new(DEFAULT_DNS_LANE_CAPACITY),
            process: ProcessLane::new(DEFAULT_PROCESS_LANE_CAPACITY),
            signals: Vec::with_capacity(
                DEFAULT_SIGNAL_CAPACITY.min(INITIAL_DRIVER_PENDING_CAPACITY),
            ),
            signal_capacity: DEFAULT_SIGNAL_CAPACITY,
            timers: BTreeMap::new(),
            next_timer_ordinal: 0,
            timer_capacity: DEFAULT_DRIVER_TIMER_CAPACITY,
            timer_harvest_budget: DEFAULT_TIMER_HARVEST_BUDGET,
            os_signals: OsSignalDispatcher::install(),
        }
    }

    pub(crate) fn with_io_loop_and_capacities(
        io_loop: IOLoopHandle<Global>,
        storage_lane_capacity: usize,
        dns_lane_capacity: usize,
        tls_lane_capacity: usize,
        process_lane_capacity: usize,
        signal_capacity: usize,
        timer_capacity: usize,
    ) -> Self {
        Self {
            tls: TlsLane::new(tls_lane_capacity, io_loop.clone()),
            unix: UnixLane::new(io_loop.clone()),
            tcp: BetelgeuseTcp::with_io_loop(io_loop.clone()),
            storage: StorageLane::reactor(io_loop, storage_lane_capacity),
            dns: DnsLane::new(dns_lane_capacity),
            process: ProcessLane::new(process_lane_capacity),
            signals: Vec::with_capacity(signal_capacity.min(INITIAL_DRIVER_PENDING_CAPACITY)),
            signal_capacity,
            timers: BTreeMap::new(),
            next_timer_ordinal: 0,
            timer_capacity,
            timer_harvest_budget: DEFAULT_TIMER_HARVEST_BUDGET,
            os_signals: OsSignalDispatcher::install(),
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
                // Bounded admission: a full timer lane refuses the arm with a
                // typed overload outcome rather than growing without bound.
                if self.timers.len() >= self.timer_capacity {
                    return Some(DriverCompletion {
                        call_id,
                        result: CallOutput::Failed(CallError::TimerFull),
                    });
                }
                let insertion_order = self.next_timer_ordinal;
                self.next_timer_ordinal = self
                    .next_timer_ordinal
                    .checked_add(1)
                    .expect("timer insertion ordinal exhausted after 2^64 submissions");
                self.timers
                    .insert((deadline_after(now, after), insertion_order), call_id);
                None
            }
            CallInput::SnapshotCommit {
                path,
                bytes,
                last_journal_index,
            } => self.storage.submit(
                call_id,
                StorageJob::SnapshotCommit {
                    path,
                    bytes,
                    last_journal_index,
                },
            ),
            CallInput::SnapshotLoad { path } => self
                .storage
                .submit(call_id, StorageJob::SnapshotLoad { path }),
            CallInput::JournalAppend {
                path,
                record_index,
                bytes,
            } => self.storage.submit(
                call_id,
                StorageJob::JournalAppend {
                    path,
                    record_index,
                    bytes,
                },
            ),
            CallInput::JournalReplay { path } => self
                .storage
                .submit(call_id, StorageJob::JournalReplay { path }),
            CallInput::PathMetadata { path } => self
                .storage
                .submit(call_id, StorageJob::PathMetadata { path }),
            CallInput::RenameReplace { from, to } => self
                .storage
                .submit(call_id, StorageJob::RenameReplace { from, to }),
            CallInput::RemoveFile { path } => self
                .storage
                .submit(call_id, StorageJob::RemoveFile { path }),
            CallInput::ReadDir { path } => {
                self.storage.submit(call_id, StorageJob::ReadDir { path })
            }
            CallInput::SyncParent { path } => self
                .storage
                .submit(call_id, StorageJob::SyncParent { path }),
            CallInput::DnsLookup {
                host,
                port,
                timeout,
            } => self.dns.submit(call_id, host, port, timeout, now),
            CallInput::TlsConnect {
                addr,
                server_name,
                root_certificates,
                alpn_protocols,
                timeout,
            } => self.tls.submit_connect(
                call_id,
                addr,
                server_name,
                root_certificates,
                alpn_protocols,
                timeout,
                now,
            ),
            CallInput::TlsBind {
                addr,
                certificate_chain,
                private_key,
                alpn_protocols,
            } => self.tls.submit_bind(
                call_id,
                addr,
                certificate_chain,
                private_key,
                alpn_protocols,
                now,
            ),
            CallInput::TlsAccept { listener, timeout } => {
                self.tls.submit_accept(call_id, listener, timeout, now)
            }
            CallInput::TlsListenerClose { listener } => {
                self.tls.submit_listener_close(call_id, listener)
            }
            CallInput::TlsRead {
                stream,
                max_len,
                timeout,
            } => self.tls.submit_read(call_id, stream, max_len, timeout, now),
            CallInput::TlsReadBuf {
                stream,
                buffer,
                max_len,
                timeout,
            } => self
                .tls
                .submit_read_buf(call_id, stream, buffer, max_len, timeout, now),
            CallInput::TlsWrite {
                stream,
                bytes,
                timeout,
            } => self.tls.submit_write(call_id, stream, bytes, timeout, now),
            CallInput::TlsWriteOwned {
                stream,
                bytes,
                timeout,
            } => self
                .tls
                .submit_write_owned(call_id, stream, bytes, timeout, now),
            CallInput::TlsClose { stream, timeout } => {
                self.tls.submit_close(call_id, stream, timeout, now)
            }
            CallInput::SignalWait { name, timeout } => {
                self.submit_signal_wait(call_id, name, timeout, now)
            }
            CallInput::ProcessRun {
                command,
                args,
                timeout,
                stdout_limit,
                stderr_limit,
            } => self.process.submit(
                call_id,
                ProcessCommand {
                    call_id,
                    command,
                    args,
                    timeout,
                    stdout_limit,
                    stderr_limit,
                    cancelled: Arc::new(AtomicBool::new(false)),
                },
            ),
            CallInput::UnixBind { .. }
            | CallInput::UnixAccept { .. }
            | CallInput::UnixConnect { .. }
            | CallInput::UnixRead { .. }
            | CallInput::UnixWrite { .. }
            | CallInput::UnixWriteOwned { .. }
            | CallInput::UnixListenerClose { .. }
            | CallInput::UnixStreamClose { .. } => self.unix.submit(call_id, request),
            other => self.tcp.submit(call_id, other),
        }
    }

    fn advance(&mut self, now: Instant, completed: &mut Vec<DriverCompletion>) {
        self.tcp.advance(completed);
        self.storage.advance(completed);
        self.dns.advance(now, completed);
        self.tls.advance(now, completed);
        self.process.advance(completed);
        self.unix.advance(completed);
        self.poll_os_signals(completed);
        self.harvest_signals(now, completed);
        self.harvest_timers(now, completed);
        // All socket/file lanes share one Betelgeuse io_loop. Each lane's
        // `advance` does a substrate step (drain queued ops, then poll for
        // readiness) and then harvests its own completions. But `poll` only
        // surfaces a ready event into the loop's queue; the matching `drain`
        // that executes it (writing the typed result) may run inside a *later*
        // lane's step. That later lane harvests only its own ops, so a
        // completion surfaced by an earlier lane and executed by a later one is
        // left completed-but-unharvested for this turn. Re-harvest TCP and Unix
        // once, after every lane has driven the shared loop. This touches no
        // syscall; it only reaps slots that already hold a result. Anything
        // still only *queued* (surfaced, not yet executed) is picked up by a
        // later explicit step.
        self.tcp.harvest(completed);
        self.unix.harvest(completed);
    }

    fn take_cancelled_by_close(&mut self) -> Vec<CallId> {
        let mut cancelled = self.tcp.take_cancelled_by_close();
        cancelled.extend(self.unix.take_cancelled_by_close());
        cancelled
    }

    fn has_pending(&self) -> bool {
        self.tcp.has_pending()
            || self.storage.has_pending()
            || self.dns.has_pending()
            || self.tls.has_pending()
            || self.process.has_pending()
            || self.unix.has_pending()
            || self.signals.iter().any(|entry| !entry.cancelled)
            || !self.timers.is_empty()
    }

    fn cancel_pending(&mut self, deadline: Instant) -> Result<(), DriverShutdownError> {
        self.timers.clear();
        self.signals.clear();
        self.storage.cancel_pending(deadline);
        self.dns.cancel_pending(deadline);
        let tls_result = self.tls.cancel_pending(deadline);
        self.process.cancel_pending(deadline);
        // Unix shares the Betelgeuse loop with TCP; it must release its own
        // completion boxes before the TCP lane runs the final whole-loop
        // release check.
        let unix_result = self.unix.cancel_pending(deadline);
        let tcp_result = self.tcp.cancel_pending(deadline);
        tls_result.and(unix_result).and(tcp_result)
    }

    fn cancel(&mut self, call_id: CallId) -> bool {
        let before = self.timers.len();
        self.timers.retain(|_, entry| *entry != call_id);
        let signal_before = self.signals.len();
        self.signals.retain(|entry| entry.call_id != call_id);
        before != self.timers.len()
            || signal_before != self.signals.len()
            || self.storage.cancel(call_id)
            || self.dns.cancel(call_id)
            || self.tls.cancel(call_id)
            || self.process.cancel(call_id)
            || self.unix.cancel(call_id)
            || self.tcp.cancel(call_id)
    }

    #[cfg(test)]
    fn io_pending_count(&self) -> usize {
        self.tcp.pending_count()
    }

    fn notify_signal(&mut self, name: &str, completed: &mut Vec<DriverCompletion>) {
        let mut ready = Vec::new();
        let mut pending = Vec::new();
        for entry in self.signals.drain(..) {
            if !entry.cancelled && entry.name == name {
                ready.push(entry);
            } else {
                pending.push(entry);
            }
        }
        self.signals = pending;
        for entry in ready {
            completed.push(DriverCompletion {
                call_id: entry.call_id,
                result: CallOutput::SignalReceived { name: entry.name },
            });
        }
    }

    fn resource_report(&self) -> DriverResourceReport {
        let tcp = self.tcp.resource_report();
        let tls = self.tls.resource_report();
        let process_pending = self.process.physical_pending_count();
        DriverResourceReport {
            listeners: tcp.listeners + tls.listeners + self.unix.listener_count(),
            streams: tcp.streams + self.unix.stream_count(),
            tls_streams: tls.tls_streams,
            udp_sockets: tcp.udp_sockets,
            files: tcp.files,
            // Worker-held: TLS in-flight ops parking cloned listener/stream
            // arcs, plus process calls owning a live Child. The Unix lane is
            // completion-backed like TCP (its in-flight connect socket is not
            // a separate OS-handle clone), and DNS/storage workers hold no
            // runtime-visible OS handle beyond table-owned ids, so they
            // contribute zero here.
            worker_held: tls.worker_held + process_pending,
            // Pending counts use physical entries (not filtered on the
            // user-cancel flag) so that work the runtime asked to cancel
            // but the lane has not yet drained — including work stuck on
            // a worker after a bounded shutdown drain — stays visible.
            pending_calls: tcp.pending_calls
                + self.storage.physical_pending_count()
                + self.dns.physical_pending_count()
                + tls.pending_calls
                + process_pending
                + self.unix.pending_call_count()
                + self.signals.iter().filter(|entry| !entry.cancelled).count()
                + self.timers.len(),
        }
    }
}

impl BetelgeuseDriver {
    fn submit_signal_wait(
        &mut self,
        call_id: CallId,
        name: String,
        timeout: Duration,
        now: Instant,
    ) -> Option<DriverCompletion> {
        if timeout.is_zero() {
            return Some(DriverCompletion {
                call_id,
                result: CallOutput::Failed(CallError::Timeout),
            });
        }
        if self.signals.iter().filter(|entry| !entry.cancelled).count() >= self.signal_capacity {
            return Some(DriverCompletion {
                call_id,
                result: CallOutput::Failed(CallError::SignalFull),
            });
        }
        self.signals.push(SignalWaitEntry {
            call_id,
            name,
            deadline: deadline_after(now, timeout),
            cancelled: false,
        });
        None
    }

    fn poll_os_signals(&mut self, completed: &mut Vec<DriverCompletion>) {
        // Convert any captured OS signal flag bits into runtime-owned
        // signal completions for parked `signal_wait` calls. Each flag
        // is consumed exactly once per delivery; subsequent SIGINT or
        // SIGTERM events set the flag again and fire the next pending
        // wait.
        if self.os_signals.consume_sigint() {
            self.notify_signal("sigint", completed);
        }
        if self.os_signals.consume_sigterm() {
            self.notify_signal("sigterm", completed);
        }
    }

    fn harvest_signals(&mut self, now: Instant, completed: &mut Vec<DriverCompletion>) {
        let mut ready = Vec::new();
        let mut pending = Vec::new();
        for entry in self.signals.drain(..) {
            if entry.cancelled {
                continue;
            }
            if entry.deadline <= now {
                ready.push(entry);
            } else {
                pending.push(entry);
            }
        }
        self.signals = pending;
        for entry in ready {
            completed.push(DriverCompletion {
                call_id: entry.call_id,
                result: CallOutput::Failed(CallError::Timeout),
            });
        }
    }

    fn harvest_timers(&mut self, now: Instant, completed: &mut Vec<DriverCompletion>) {
        // Fire due timers in `(deadline, insertion_order)` order, at most
        // `timer_harvest_budget` per advance. The map is ordered, so the front
        // entry is always the globally-earliest due timer; a synchronized batch
        // larger than the budget simply fires its tail on the next advance.
        // Delivery order is byte-identical to the old all-at-once harvest: a
        // timer armed after this batch has `deadline >= now` and a larger
        // `insertion_order`, so it can never sort ahead of an already-due timer.
        let mut fired = 0;
        while fired < self.timer_harvest_budget {
            let Some((&key, &call_id)) = self.timers.first_key_value() else {
                break;
            };
            if key.0 > now {
                break;
            }
            self.timers.remove(&key);
            completed.push(DriverCompletion {
                call_id,
                result: CallOutput::TimerFired,
            });
            fired += 1;
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explicit_driver_storage_completes_inline_without_pending_lane() {
        let io_loop =
            io_loop(Global).expect("failed to initialise Betelgeuse IO loop for driver test");
        let mut driver = BetelgeuseDriver::with_io_loop(io_loop);
        let completion = driver
            .submit(
                CallId::new(30),
                CallInput::SnapshotLoad {
                    path: PathBuf::from("missing-snapshot"),
                },
                Instant::now(),
            )
            .expect("explicit driver returns storage completion inline");

        assert_eq!(completion.call_id, CallId::new(30));
        assert!(matches!(
            completion.result,
            CallOutput::SnapshotLoaded { snapshot: None }
        ));
        assert!(!driver.has_pending());
    }

    #[test]
    fn storage_lane_rejects_full_without_sleep_as_proof() {
        let io_loop = io_loop(Global).expect("init io loop");
        let mut lane = StorageLane::reactor(io_loop, 1);
        let (started_tx, started_rx) = sync_channel(1);
        let (release_tx, release_rx) = sync_channel(1);

        assert!(
            lane.submit(
                CallId::new(1),
                StorageJob::Park {
                    started: started_tx,
                    release: release_rx,
                },
            )
            .is_none()
        );
        // The parked job occupies the one capacity slot for its whole life.
        let mut completed = Vec::new();
        lane.advance(&mut completed);
        started_rx.recv().expect("parked storage job started");
        assert!(completed.is_empty());

        let full = lane
            .submit(
                CallId::new(2),
                StorageJob::SnapshotLoad {
                    path: PathBuf::from("full"),
                },
            )
            .expect("second active storage job rejected");
        assert_eq!(full.call_id, CallId::new(2));
        assert!(matches!(
            full.result,
            CallOutput::Failed(CallError::StorageFull)
        ));

        release_tx.send(()).expect("release parked storage job");
        lane.cancel_pending(Instant::now());
    }

    #[test]
    fn storage_lane_cancellation_swallows_late_completion() {
        let io_loop = io_loop(Global).expect("init io loop");
        let mut lane = StorageLane::reactor(io_loop, 1);
        let (started_tx, started_rx) = sync_channel(1);
        let (release_tx, release_rx) = sync_channel(1);

        assert!(
            lane.submit(
                CallId::new(7),
                StorageJob::Park {
                    started: started_tx,
                    release: release_rx,
                },
            )
            .is_none()
        );
        let mut completed = Vec::new();
        lane.advance(&mut completed);
        started_rx.recv().expect("parked storage job started");
        assert!(lane.cancel(CallId::new(7)));
        assert!(!lane.has_pending());

        release_tx.send(()).expect("release parked storage job");
        for _ in 0..64 {
            lane.advance(&mut completed);
            if !completed.is_empty() {
                break;
            }
            thread::yield_now();
        }
        assert!(completed.is_empty());
    }

    #[test]
    fn dns_lane_resolves_with_injected_resolver() {
        let (done_tx, done_rx) = sync_channel(1);
        let addr: SocketAddr = "127.0.0.1:4040".parse().expect("valid test address");
        let mut lane = DnsWorkerLane::new(
            1,
            Arc::new(move |host, port| {
                assert_eq!(host, "llama.test");
                assert_eq!(port, 4040);
                done_tx.send(()).expect("resolver completion observed");
                CallOutput::DnsResolved { addrs: vec![addr] }
            }),
        );
        let now = Instant::now();
        assert!(
            lane.submit(
                CallId::new(1),
                "llama.test".to_string(),
                4040,
                Duration::from_secs(1),
                now,
            )
            .is_none()
        );
        done_rx.recv().expect("resolver ran");

        let mut completed = Vec::new();
        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline {
            lane.advance(now, &mut completed);
            if !completed.is_empty() {
                break;
            }
            thread::yield_now();
        }

        assert_eq!(completed.len(), 1);
        assert_eq!(completed[0].call_id, CallId::new(1));
        assert!(matches!(
            &completed[0].result,
            CallOutput::DnsResolved { addrs } if addrs == &vec![addr]
        ));
        assert!(!lane.has_pending());
    }

    #[test]
    fn dns_lane_timeout_tombstones_and_keeps_capacity_until_late_completion() {
        use std::sync::{Condvar, Mutex};

        let started = Arc::new((Mutex::new(false), Condvar::new()));
        let release = Arc::new((Mutex::new(false), Condvar::new()));
        let finished = Arc::new((Mutex::new(false), Condvar::new()));
        let started_for_resolver = Arc::clone(&started);
        let release_for_resolver = Arc::clone(&release);
        let finished_for_resolver = Arc::clone(&finished);
        let mut lane = DnsWorkerLane::new(
            1,
            Arc::new(move |_, _| {
                let (started_lock, started_cv) = &*started_for_resolver;
                *started_lock.lock().expect("started lock") = true;
                started_cv.notify_one();

                let (release_lock, release_cv) = &*release_for_resolver;
                let mut released = release_lock.lock().expect("release lock");
                while !*released {
                    released = release_cv.wait(released).expect("release wait");
                }
                let (finished_lock, finished_cv) = &*finished_for_resolver;
                *finished_lock.lock().expect("finished lock") = true;
                finished_cv.notify_one();
                CallOutput::DnsResolved {
                    addrs: vec!["127.0.0.1:55".parse().expect("valid test address")],
                }
            }),
        );
        let now = Instant::now();
        assert!(
            lane.submit(
                CallId::new(1),
                "slow.test".to_string(),
                55,
                Duration::from_millis(1),
                now,
            )
            .is_none()
        );

        let (started_lock, started_cv) = &*started;
        let mut started_guard = started_lock.lock().expect("started lock");
        while !*started_guard {
            started_guard = started_cv.wait(started_guard).expect("started wait");
        }
        drop(started_guard);

        let mut completed = Vec::new();
        lane.advance(now + Duration::from_millis(2), &mut completed);
        assert_eq!(completed.len(), 1);
        assert_eq!(completed[0].call_id, CallId::new(1));
        assert!(matches!(
            completed[0].result,
            CallOutput::Failed(CallError::Timeout)
        ));
        assert!(!lane.has_pending());

        let full = lane
            .submit(
                CallId::new(2),
                "other.test".to_string(),
                55,
                Duration::from_secs(1),
                now,
            )
            .expect("timed-out started work still occupies bounded DNS lane");
        assert!(matches!(
            full.result,
            CallOutput::Failed(CallError::DnsFull)
        ));

        let (release_lock, release_cv) = &*release;
        *release_lock.lock().expect("release lock") = true;
        release_cv.notify_one();

        let (finished_lock, finished_cv) = &*finished;
        let mut finished_guard = finished_lock.lock().expect("finished lock");
        while !*finished_guard {
            finished_guard = finished_cv.wait(finished_guard).expect("finished wait");
        }
        drop(finished_guard);

        let mut late = Vec::new();
        for _ in 0..1024 {
            lane.advance(now + Duration::from_millis(3), &mut late);
            if lane.unresolved_pending_count() == 0 {
                break;
            }
            thread::yield_now();
        }
        assert!(late.is_empty());
        assert_eq!(lane.unresolved_pending_count(), 0);
    }

    #[test]
    fn storage_lane_shutdown_skips_buffered_work_that_never_started() {
        // Reactor storage progresses only on `advance` (no worker thread for
        // the durability path). A job cancelled before its first poll never
        // starts: the proof that canceled queued work does not start.
        let io_loop = io_loop(Global).expect("init io loop");
        let mut lane = StorageLane::reactor(io_loop, 2);
        let (first_started_tx, first_started_rx) = sync_channel(1);
        let (first_release_tx, first_release_rx) = sync_channel(1);
        let (queued_started_tx, queued_started_rx) = sync_channel(1);
        let (_queued_release_tx, queued_release_rx) = sync_channel(1);

        assert!(
            lane.submit(
                CallId::new(11),
                StorageJob::Park {
                    started: first_started_tx,
                    release: first_release_rx,
                },
            )
            .is_none()
        );
        assert!(
            lane.submit(
                CallId::new(12),
                StorageJob::Park {
                    started: queued_started_tx,
                    release: queued_release_rx,
                },
            )
            .is_none()
        );

        // Cancel #12 before any advance, so it is never polled.
        lane.cancel(CallId::new(12));
        let mut completed = Vec::new();
        lane.advance(&mut completed);
        first_started_rx
            .recv()
            .expect("first parked storage job started");

        first_release_tx
            .send(())
            .expect("release first parked storage job");
        lane.cancel_pending(Instant::now());
        assert!(
            queued_started_rx.try_recv().is_err(),
            "queued storage work cancelled before its first poll must not start"
        );
    }

    #[test]
    fn udp_recv_lane_rejects_duplicate_and_close_cancels_pending_recv() {
        let io_loop =
            io_loop(Global).expect("failed to initialise Betelgeuse IO loop for driver test");
        let mut driver = BetelgeuseDriver::with_io_loop(io_loop);
        let bound = driver
            .submit(
                CallId::new(1),
                CallInput::UdpBind {
                    addr: "127.0.0.1:0".parse().expect("loopback addr"),
                },
                Instant::now(),
            )
            .expect("udp bind completes inline");
        let socket = match bound.result {
            CallOutput::UdpBound { socket, .. } => socket,
            other => panic!("unexpected udp bind output {other:?}"),
        };

        assert!(
            driver
                .submit(
                    CallId::new(2),
                    CallInput::UdpRecvFrom { socket, max_len: 8 },
                    Instant::now(),
                )
                .is_none()
        );

        let duplicate = driver
            .submit(
                CallId::new(3),
                CallInput::UdpRecvFrom { socket, max_len: 8 },
                Instant::now(),
            )
            .expect("duplicate udp recv rejected inline");
        assert!(matches!(
            duplicate.result,
            CallOutput::Failed(CallError::ResourceBusy)
        ));

        // Close wins. Pending recv is cancelled.
        let closed = driver
            .submit(
                CallId::new(4),
                CallInput::UdpSocketClose { socket },
                Instant::now(),
            )
            .expect("udp close completes inline even with a pending recv");
        assert!(matches!(closed.result, CallOutput::UdpSocketClosed));
        assert!(
            !driver.has_pending(),
            "the pending recv was cancelled by the close and is no longer counted as in-flight"
        );
    }

    // -------------------------------------------------------------------
    // Resource-accounting count rules.
    //
    // One narrow test per lane verifies how each lane contributes to the
    // three independent vocabularies in DriverResourceReport:
    // table-owned, worker-held, and pending. Tests live at the
    // BetelgeuseDriver level where possible so the aggregator at
    // BetelgeuseDriver::resource_report is exercised end to end.
    // -------------------------------------------------------------------

    #[test]
    fn fresh_driver_reports_all_zero_counts() {
        let io_loop = io_loop(Global).expect("init io loop");
        let driver = BetelgeuseDriver::with_io_loop(io_loop);
        let report = driver.resource_report();
        assert_eq!(report.owned_resource_count(), 0);
        assert_eq!(report.worker_held_resource_count(), 0);
        assert_eq!(report.pending_driver_call_count(), 0);
    }

    #[test]
    fn tcp_listener_counts_as_table_owned_only() {
        let io_loop = io_loop(Global).expect("init io loop");
        let mut driver = BetelgeuseDriver::with_io_loop(io_loop);
        let bound = driver
            .submit(
                CallId::new(1),
                CallInput::TcpBind {
                    addr: "127.0.0.1:0".parse().expect("loopback"),
                },
                Instant::now(),
            )
            .expect("bind completes inline");
        let listener = match bound.result {
            CallOutput::TcpBound { listener, .. } => listener,
            other => panic!("unexpected bind result: {other:?}"),
        };

        let report = driver.resource_report();
        assert_eq!(report.owned_resource_count(), 1);
        assert_eq!(report.worker_held_resource_count(), 0);
        assert_eq!(report.pending_driver_call_count(), 0);

        // A pending accept on that listener increments pending only;
        // no extra worker-held resource is created (the listener
        // table id already covers the accept's working state).
        assert!(
            driver
                .submit(
                    CallId::new(2),
                    CallInput::TcpAccept { listener },
                    Instant::now(),
                )
                .is_none()
        );
        let report = driver.resource_report();
        assert_eq!(report.owned_resource_count(), 1);
        assert_eq!(report.worker_held_resource_count(), 0);
        assert_eq!(report.pending_driver_call_count(), 1);

        assert!(driver.cancel(CallId::new(2)));
        // The pending count tracks physical entries, so cancelled-but-not-yet-
        // drained ops stay visible until the backend releases the completion
        // slot. After a shutdown drain the count must reach zero again.
        let _ = driver.cancel_pending(Instant::now() + Duration::from_millis(100));
        assert_eq!(driver.resource_report().pending_driver_call_count(), 0);
    }

    #[test]
    fn udp_recv_pending_increments_pending_only() {
        let io_loop = io_loop(Global).expect("init io loop");
        let mut driver = BetelgeuseDriver::with_io_loop(io_loop);
        let bound = driver
            .submit(
                CallId::new(1),
                CallInput::UdpBind {
                    addr: "127.0.0.1:0".parse().expect("loopback"),
                },
                Instant::now(),
            )
            .expect("udp bind inline");
        let socket = match bound.result {
            CallOutput::UdpBound { socket, .. } => socket,
            other => panic!("unexpected udp bind: {other:?}"),
        };

        assert!(
            driver
                .submit(
                    CallId::new(2),
                    CallInput::UdpRecvFrom { socket, max_len: 8 },
                    Instant::now(),
                )
                .is_none()
        );
        let report = driver.resource_report();
        assert_eq!(report.owned_resource_count(), 1);
        assert_eq!(report.worker_held_resource_count(), 0);
        assert_eq!(report.pending_driver_call_count(), 1);
    }

    #[test]
    fn signal_wait_counts_as_pending_only() {
        let io_loop = io_loop(Global).expect("init io loop");
        let mut driver = BetelgeuseDriver::with_io_loop(io_loop);
        assert!(
            driver
                .submit(
                    CallId::new(1),
                    CallInput::SignalWait {
                        name: "sigterm".to_string(),
                        timeout: Duration::from_secs(60),
                    },
                    Instant::now(),
                )
                .is_none()
        );
        let report = driver.resource_report();
        assert_eq!(report.owned_resource_count(), 0);
        assert_eq!(report.worker_held_resource_count(), 0);
        assert_eq!(report.pending_driver_call_count(), 1);
    }

    #[test]
    fn timer_pending_counts_as_pending_only() {
        let io_loop = io_loop(Global).expect("init io loop");
        let mut driver = BetelgeuseDriver::with_io_loop(io_loop);
        assert!(
            driver
                .submit(
                    CallId::new(1),
                    CallInput::Sleep {
                        after: Duration::from_secs(60),
                    },
                    Instant::now(),
                )
                .is_none()
        );
        let report = driver.resource_report();
        assert_eq!(report.owned_resource_count(), 0);
        assert_eq!(report.worker_held_resource_count(), 0);
        assert_eq!(report.pending_driver_call_count(), 1);
    }

    // Runs a full budgeted harvest of `n` synchronized (equal-deadline) timers
    // with harvest budget `budget`, returning the call-id sequence fired across
    // all ticks plus the per-tick counts.
    fn drain_synchronized_batch(n: u64, budget: usize) -> (Vec<u64>, Vec<usize>) {
        let io_loop = io_loop(Global).expect("init io loop");
        let mut driver = BetelgeuseDriver::with_io_loop(io_loop);
        driver.timer_harvest_budget = budget;
        let now = Instant::now();
        for i in 0..n {
            assert!(
                driver
                    .submit(
                        CallId::new(i),
                        CallInput::Sleep {
                            after: Duration::ZERO
                        },
                        now
                    )
                    .is_none(),
                "arming a timer under capacity must not complete inline"
            );
        }
        let harvest_at = now + Duration::from_millis(1);
        let mut fired = Vec::new();
        let mut per_tick = Vec::new();
        let mut ticks = 0u64;
        while !driver.timers.is_empty() {
            let mut completed = Vec::new();
            driver.harvest_timers(harvest_at, &mut completed);
            per_tick.push(completed.len());
            for completion in &completed {
                assert!(matches!(completion.result, CallOutput::TimerFired));
                fired.push(completion.call_id.get());
            }
            ticks += 1;
            assert!(ticks <= n + 1, "harvest made no progress");
        }
        (fired, per_tick)
    }

    #[test]
    fn synchronized_timer_batch_harvests_within_budget_and_all_fire_in_order() {
        let n = 30u64;
        let budget = 4usize;
        let (fired, per_tick) = drain_synchronized_batch(n, budget);

        // Every tick honours the budget, and non-final ticks fill it. This is
        // the load-bearing budget assertion: revert to an unbudgeted harvest
        // (fire all due at once) and a single tick fires all 30 -> fails here.
        assert!(
            per_tick.iter().all(|&count| count <= budget),
            "a tick exceeded the harvest budget: {per_tick:?}"
        );
        assert!(
            per_tick.len() > 1,
            "budget did not spread the synchronized batch across ticks: {per_tick:?}"
        );
        if let Some((_last, leading)) = per_tick.split_last() {
            assert!(
                leading.iter().all(|&count| count == budget),
                "a non-final tick did not fill the budget: {per_tick:?}"
            );
        }

        // All timers eventually fire, in submission (FIFO) order because their
        // deadlines are equal.
        let expected: Vec<u64> = (0..n).collect();
        assert_eq!(fired, expected, "not every timer fired, or order changed");

        // Same config -> identical sequence on a second independent run.
        let (fired_again, _) = drain_synchronized_batch(n, budget);
        assert_eq!(fired, fired_again, "harvest order was not deterministic");
    }

    #[test]
    fn timer_harvest_order_is_deadline_then_insertion_fifo() {
        // Load-bearing tie-break test: preserves the exact same-deadline order
        // of the old linear `min_by(deadline, insertion_order)` scan. A plain
        // BinaryHeap (no insertion tie-break) or a reversed tie-break reorders
        // the equal-deadline pairs and fails here.
        let io_loop = io_loop(Global).expect("init io loop");
        let mut driver = BetelgeuseDriver::with_io_loop(io_loop);
        driver.timer_harvest_budget = usize::MAX;
        let now = Instant::now();
        let early = Duration::ZERO;
        let late = Duration::from_millis(1);
        // Submission order deliberately differs from harvest order and mixes
        // deadlines: id 10 (late), 11 (early), 12 (early), 13 (late).
        driver.submit(CallId::new(10), CallInput::Sleep { after: late }, now);
        driver.submit(CallId::new(11), CallInput::Sleep { after: early }, now);
        driver.submit(CallId::new(12), CallInput::Sleep { after: early }, now);
        driver.submit(CallId::new(13), CallInput::Sleep { after: late }, now);
        let mut completed = Vec::new();
        driver.harvest_timers(now + late, &mut completed);
        let order: Vec<u64> = completed.iter().map(|c| c.call_id.get()).collect();
        // Earlier deadline first (11 then 12), then the later deadline in
        // submission order (10 then 13).
        assert_eq!(order, vec![11, 12, 10, 13]);
    }

    #[test]
    fn timer_capacity_exceeded_returns_timer_full_not_growth() {
        let io_loop = io_loop(Global).expect("init io loop");
        let mut driver = BetelgeuseDriver::with_io_loop(io_loop);
        driver.timer_capacity = 3;
        let now = Instant::now();
        let after = Duration::from_secs(60);
        for i in 0..3 {
            assert!(
                driver
                    .submit(CallId::new(i), CallInput::Sleep { after }, now)
                    .is_none(),
                "arming under capacity must not complete inline"
            );
        }
        // The fourth arm exceeds the cap. Load-bearing: revert the capacity
        // guard and this returns None while `timers.len()` grows to 4.
        let completion = driver
            .submit(CallId::new(99), CallInput::Sleep { after }, now)
            .expect("a full timer lane refuses the arm inline");
        assert_eq!(completion.call_id, CallId::new(99));
        assert!(
            matches!(completion.result, CallOutput::Failed(CallError::TimerFull)),
            "expected TimerFull, got {:?}",
            completion.result
        );
        assert_eq!(
            driver.timers.len(),
            3,
            "a refused arm must not grow the lane"
        );
    }

    #[test]
    fn arm_then_cancel_loop_far_past_capacity_never_reports_timer_full() {
        // Cancel must actually drop the map entry, not leak it. The BTreeMap has
        // no secondary CallId index, so a cancel that failed to remove the key
        // would leak an entry and — after enough cycles — refill the lane and
        // start refusing arms with TimerFull. Arm+cancel 250x past capacity and
        // prove the lane returns to empty every cycle and still admits at the
        // end. Break `cancel`'s `retain` (e.g. make the predicate always keep)
        // and the in-loop `len == 0` assert fires immediately.
        let io_loop = io_loop(Global).expect("init io loop");
        let mut driver = BetelgeuseDriver::with_io_loop(io_loop);
        driver.timer_capacity = 4;
        let now = Instant::now();
        let after = Duration::from_secs(60);
        for i in 0..1_000u64 {
            assert!(
                driver
                    .submit(CallId::new(i), CallInput::Sleep { after }, now)
                    .is_none(),
                "arming under capacity must not complete inline"
            );
            assert!(
                driver.cancel(CallId::new(i)),
                "cancel must find and drop the just-armed timer"
            );
            assert_eq!(
                driver.timers.len(),
                0,
                "cancel leaked a map entry: lane not empty after cancel"
            );
        }
        assert!(
            driver
                .submit(CallId::new(10_000), CallInput::Sleep { after }, now)
                .is_none(),
            "arm after 1000 arm+cancel cycles must not report TimerFull"
        );
    }

    #[test]
    fn zero_duration_arm_during_budgeted_drain_never_jumps_the_backlog() {
        // The crown-jewel no-reorder invariant under the harvest budget. While a
        // synchronized due backlog drains across ticks, a NEW zero-duration
        // timer armed mid-drain at the same instant as the backlog (the sharpest
        // adversarial case: equal deadline, so only the insertion-order
        // tie-break separates them) must sort BEHIND every not-yet-fired backlog
        // entry. It carries a strictly larger insertion_order, so its key can
        // never sort ahead of an already-due timer, and budgeted delivery order
        // equals the unbudgeted order. Reverse the tie-break and the interloper
        // jumps the queue -> this fails.
        let io_loop = io_loop(Global).expect("init io loop");
        let mut driver = BetelgeuseDriver::with_io_loop(io_loop);
        driver.timer_harvest_budget = 4;
        let t0 = Instant::now();
        for i in 0..20u64 {
            driver.submit(
                CallId::new(i),
                CallInput::Sleep {
                    after: Duration::ZERO,
                },
                t0,
            );
        }
        // Harvest at t1 > t0 so the whole backlog is due; the interloper is armed
        // at t0 (equal deadline to the backlog) after the first budgeted tick.
        let t1 = t0 + Duration::from_millis(1);
        let mut fired = Vec::new();
        let mut interloper_armed = false;
        let mut ticks = 0u64;
        while !driver.timers.is_empty() {
            let mut completed = Vec::new();
            driver.harvest_timers(t1, &mut completed);
            for c in &completed {
                fired.push(c.call_id.get());
            }
            if !interloper_armed {
                driver.submit(
                    CallId::new(999),
                    CallInput::Sleep {
                        after: Duration::ZERO,
                    },
                    t0,
                );
                interloper_armed = true;
            }
            ticks += 1;
            assert!(ticks <= 30, "harvest made no progress");
        }
        assert!(interloper_armed, "interloper was never armed mid-drain");
        // Backlog fires first in FIFO order; the equal-deadline interloper fires
        // last because its insertion_order is the largest.
        let mut expected: Vec<u64> = (0..20).collect();
        expected.push(999);
        assert_eq!(
            fired, expected,
            "a mid-drain zero-duration arm reordered the due backlog"
        );
    }

    #[test]
    fn storage_park_counts_as_pending_only() {
        let io_loop = io_loop(Global).expect("init io loop");
        let mut lane = StorageLane::reactor(io_loop, 1);
        let (started_tx, started_rx) = sync_channel(1);
        let (release_tx, release_rx) = sync_channel(1);
        assert!(
            lane.submit(
                CallId::new(7),
                StorageJob::Park {
                    started: started_tx,
                    release: release_rx,
                },
            )
            .is_none()
        );
        let mut completed = Vec::new();
        lane.advance(&mut completed);
        started_rx.recv().expect("park job started");
        // Storage contributes to pending_calls but not worker_held: the
        // durability path rides the shard reactor, so the runtime does not
        // see a separate handle.
        assert_eq!(lane.physical_pending_count(), 1);

        release_tx.send(()).expect("release park job");
        lane.cancel_pending(Instant::now());
    }

    #[test]
    fn dns_pending_counts_as_pending_only() {
        let (started_tx, started_rx) = sync_channel(1);
        let (release_tx, release_rx) = sync_channel(1);
        let release_rx = Arc::new(Mutex::new(Some(release_rx)));
        let started_tx = Arc::new(Mutex::new(Some(started_tx)));
        let mut lane = DnsWorkerLane::new(
            1,
            Arc::new(move |_, _| {
                if let Some(tx) = started_tx.lock().expect("started lock").take() {
                    let _ = tx.send(());
                }
                if let Some(rx) = release_rx.lock().expect("release lock").take() {
                    let _ = rx.recv();
                }
                CallOutput::DnsResolved { addrs: vec![] }
            }),
        );
        assert!(
            lane.submit(
                CallId::new(1),
                "blocking.test".to_string(),
                4040,
                Duration::from_secs(60),
                Instant::now(),
            )
            .is_none()
        );
        started_rx.recv().expect("resolver started");
        // DNS contributes to pending_calls only.
        assert_eq!(lane.unresolved_pending_count(), 1);

        release_tx.send(()).expect("release dns");
        lane.cancel_pending(Instant::now());
    }

    #[test]
    fn process_pending_contributes_to_worker_held_and_pending() {
        let mut lane = ProcessLane::new(1);
        // Pick a long-running process so the call stays pending while we
        // measure. Use a Unix sleep; on non-Unix the test is skipped.
        if cfg!(not(unix)) {
            return;
        }
        let cancelled = Arc::new(AtomicBool::new(false));
        assert!(
            lane.submit(
                CallId::new(1),
                ProcessCommand {
                    call_id: CallId::new(1),
                    command: "/bin/sleep".to_string(),
                    args: vec!["5".to_string()],
                    timeout: Duration::from_secs(10),
                    stdout_limit: 1024,
                    stderr_limit: 1024,
                    cancelled: Arc::clone(&cancelled),
                },
            )
            .is_none()
        );
        // Process call is both pending and worker-held: the worker thread
        // owns a live std::process::Child while the call is in flight.
        assert_eq!(lane.physical_pending_count(), 1);

        cancelled.store(true, Ordering::Release);
        lane.cancel_pending(Instant::now());
    }

    // -------------------------------------------------------------------
    // OS signal capture.
    //
    // The dispatcher converts process-wide SIGINT/SIGTERM flag bits set
    // by signal-hook into runtime-owned signal completions. On non-Unix
    // the dispatcher is a no-op and `os_signal_capture_supported()`
    // returns false.
    // -------------------------------------------------------------------

    #[test]
    fn os_signal_capture_supported_matches_target() {
        assert_eq!(os_signal_capture_supported(), cfg!(unix));
    }

    // Tests touching the process-global OS signal state must run
    // serially so they do not steal each other's flag bits.
    #[cfg(unix)]
    fn os_signal_test_lock() -> std::sync::MutexGuard<'static, ()> {
        use std::sync::{Mutex, OnceLock};
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    #[cfg(unix)]
    #[test]
    fn dispatcher_consumes_sigint_flag_once_per_delivery() {
        // Private dispatcher: not affected by parallel tests touching
        // the global signal-hook flag.
        let dispatcher = OsSignalDispatcher::private_for_test();
        dispatcher.sigint.store(true, Ordering::Release);
        assert!(dispatcher.consume_sigint());
        assert!(!dispatcher.consume_sigint());
    }

    #[cfg(unix)]
    #[test]
    fn betelgeuse_driver_fires_sigint_signal_when_dispatcher_flag_set() {
        // Use a private (non-shared) dispatcher so this test cannot race
        // with other drivers in parallel test runs. The wiring from
        // dispatcher → poll_os_signals → notify_signal is the same.
        let io_loop = io_loop(Global).expect("init io loop");
        let mut driver = BetelgeuseDriver::with_io_loop(io_loop);
        driver.os_signals = OsSignalDispatcher::private_for_test();
        assert!(
            driver
                .submit(
                    CallId::new(1),
                    CallInput::SignalWait {
                        name: "sigint".to_string(),
                        timeout: Duration::from_secs(60),
                    },
                    Instant::now(),
                )
                .is_none()
        );
        driver.os_signals.sigint.store(true, Ordering::Release);
        let mut completed = Vec::new();
        driver.advance(Instant::now(), &mut completed);
        assert!(
            completed.iter().any(|c| matches!(
                &c.result,
                CallOutput::SignalReceived { name } if name == "sigint"
            )),
            "advance must deliver a SignalReceived completion when the dispatcher fires"
        );
    }

    #[cfg(unix)]
    #[test]
    fn raised_sigint_reaches_runtime_owned_signal_wait() {
        let _guard = os_signal_test_lock();
        let io_loop = io_loop(Global).expect("init io loop");
        let mut driver = BetelgeuseDriver::with_io_loop(io_loop);
        let _ = driver.os_signals.consume_sigint();
        assert!(
            driver
                .submit(
                    CallId::new(1),
                    CallInput::SignalWait {
                        name: "sigint".to_string(),
                        timeout: Duration::from_secs(5),
                    },
                    Instant::now(),
                )
                .is_none()
        );
        // Use signal-hook's raise helper so the real OS signal path fires
        // the registered flag handler. Cargo's test runner survives
        // because our handler only sets the flag and does not terminate.
        signal_hook::low_level::raise(signal_hook::consts::SIGINT).expect("raise SIGINT to self");
        let deadline = Instant::now() + Duration::from_secs(2);
        let mut completed = Vec::new();
        while Instant::now() < deadline {
            driver.advance(Instant::now(), &mut completed);
            if completed.iter().any(|c| {
                matches!(
                    &c.result,
                    CallOutput::SignalReceived { name } if name == "sigint"
                )
            }) {
                return;
            }
            thread::sleep(Duration::from_millis(5));
        }
        panic!("raised SIGINT did not reach the runtime-owned signal wait");
    }

    // -------------------------------------------------------------------
    // Bounded shutdown drain.
    //
    // Each lane's `cancel_pending(deadline)` must return inside the budget
    // even when work is stuck, surfacing remaining work via the pending
    // count rather than blocking forever. The storage reactor's bounded-
    // shutdown proof lives in `driver::storage` tests, where a fault
    // backend can hold a Betelgeuse completion past the budget.
    // -------------------------------------------------------------------

    #[test]
    fn betelgeuse_tcp_shutdown_returns_within_budget() {
        // Even with an in-flight TCP accept, bounded shutdown must
        // return promptly. The backend may not release the completion
        // slot inside the budget, but the call must not hang.
        let io_loop = io_loop(Global).expect("init io loop");
        let mut driver = BetelgeuseDriver::with_io_loop(io_loop);
        let bound = driver
            .submit(
                CallId::new(1),
                CallInput::TcpBind {
                    addr: "127.0.0.1:0".parse().expect("loopback"),
                },
                Instant::now(),
            )
            .expect("bind inline");
        let listener = match bound.result {
            CallOutput::TcpBound { listener, .. } => listener,
            other => panic!("unexpected bind: {other:?}"),
        };
        assert!(
            driver
                .submit(
                    CallId::new(2),
                    CallInput::TcpAccept { listener },
                    Instant::now(),
                )
                .is_none()
        );

        let budget = Duration::from_millis(50);
        let started = Instant::now();
        let _ = driver.cancel_pending(Instant::now() + budget);
        let elapsed = started.elapsed();
        assert!(
            elapsed < budget * 10,
            "TCP shutdown took {elapsed:?}, expected to return near {budget:?}"
        );
    }

    #[test]
    fn cancel_drains_do_not_double_count() {
        let io_loop = io_loop(Global).expect("init io loop");
        let mut driver = BetelgeuseDriver::with_io_loop(io_loop);
        assert!(
            driver
                .submit(
                    CallId::new(1),
                    CallInput::Sleep {
                        after: Duration::from_secs(60),
                    },
                    Instant::now(),
                )
                .is_none()
        );
        assert_eq!(driver.resource_report().pending_driver_call_count(), 1);
        assert!(driver.cancel(CallId::new(1)));
        // Once a pending op is cancelled it must not contribute to the
        // pending count, and the cancelled signal/timer entry must drop
        // out cleanly without double counting on advance.
        let report = driver.resource_report();
        assert_eq!(report.pending_driver_call_count(), 0);
        let mut completed = Vec::new();
        driver.advance(Instant::now(), &mut completed);
        assert_eq!(driver.resource_report().pending_driver_call_count(), 0);
    }

    // -------------------------------------------------------------------
    // Unix-domain rail (now completion-backed over the shared Betelgeuse
    // loop). These drive the live driver directly to prove lane discipline,
    // close-wins cancellation, and bounded shutdown — the parts the
    // higher-level echo round-trip does not isolate.
    // -------------------------------------------------------------------
    #[cfg(unix)]
    mod unix_lane {
        use super::*;
        use crate::call::{UnixListenerId, UnixStreamId};
        use std::os::unix;
        use std::sync::atomic::AtomicU64;

        fn unique_sock(label: &str) -> std::path::PathBuf {
            static SEQ: AtomicU64 = AtomicU64::new(0);
            let n = SEQ.fetch_add(1, Ordering::Relaxed);
            std::env::temp_dir().join(format!(
                "tina-unixlane-{label}-{}-{n}.sock",
                std::process::id()
            ))
        }

        /// Advances the driver until `done` is satisfied or a 2s safety
        /// deadline elapses; returns every completion observed.
        fn advance_until(
            driver: &mut BetelgeuseDriver,
            done: impl Fn(&[DriverCompletion]) -> bool,
        ) -> Vec<DriverCompletion> {
            let deadline = Instant::now() + Duration::from_secs(2);
            let mut all = Vec::new();
            loop {
                driver.advance(Instant::now(), &mut all);
                if done(&all) || Instant::now() >= deadline {
                    return all;
                }
                thread::sleep(Duration::from_millis(1));
            }
        }

        // The substrate unlinks the listener socket file on close and clears a
        // stale file before bind, so these tests need no `std::fs` cleanup.
        fn bind_listener(driver: &mut BetelgeuseDriver, call: u64) -> UnixListenerId {
            let path = unique_sock("lane");
            let bound = driver
                .submit(
                    CallId::new(call),
                    CallInput::UnixBind { path },
                    Instant::now(),
                )
                .expect("bind completes inline");
            match bound.result {
                CallOutput::UnixBound { listener, .. } => listener,
                other => panic!("unexpected bind result: {other:?}"),
            }
        }

        /// Binds a listener and returns both its id and the path, for callers
        /// that also need to connect a client to it.
        fn bind_listener_with_path(
            driver: &mut BetelgeuseDriver,
            call: u64,
        ) -> (UnixListenerId, PathBuf) {
            let path = unique_sock("lane");
            let bound = driver
                .submit(
                    CallId::new(call),
                    CallInput::UnixBind { path: path.clone() },
                    Instant::now(),
                )
                .expect("bind completes inline");
            match bound.result {
                CallOutput::UnixBound { listener, .. } => (listener, path),
                other => panic!("unexpected bind result: {other:?}"),
            }
        }

        /// Binds, accepts, and connects an external peer to produce a live
        /// server stream through the substrate. These close/read tests are
        /// about server-stream lane behavior; UnixConnect has its own proof, so
        /// the helper avoids coupling them to same-loop accept/connect ordering.
        fn connected_pair(
            driver: &mut BetelgeuseDriver,
        ) -> (UnixListenerId, UnixStreamId, unix::net::UnixStream) {
            let (listener, path) = bind_listener_with_path(driver, 100);
            assert!(
                driver
                    .submit(
                        CallId::new(101),
                        CallInput::UnixAccept { listener },
                        Instant::now()
                    )
                    .is_none()
            );
            let client = unix::net::UnixStream::connect(&path).expect("connect external peer");
            let completed = advance_until(driver, |all| {
                all.iter().any(|c| c.call_id == CallId::new(101))
            });
            let server = completed
                .iter()
                .find_map(|c| match (&c.call_id, &c.result) {
                    (id, CallOutput::UnixAccepted { stream }) if *id == CallId::new(101) => {
                        Some(*stream)
                    }
                    _ => None,
                })
                .unwrap_or_else(|| panic!("accept produced no server stream: {completed:?}"));
            (listener, server, client)
        }

        #[test]
        fn duplicate_lane_work_is_resource_busy_and_invalid_ids_are_typed() {
            let io_loop = io_loop(Global).expect("init io loop");
            let mut driver = BetelgeuseDriver::with_io_loop(io_loop);
            let listener = bind_listener(&mut driver, 1);

            // First accept arms a pending op; a second on the same listener
            // lane is rejected ResourceBusy, not silently queued.
            assert!(
                driver
                    .submit(
                        CallId::new(2),
                        CallInput::UnixAccept { listener },
                        Instant::now()
                    )
                    .is_none()
            );
            let busy = driver
                .submit(
                    CallId::new(3),
                    CallInput::UnixAccept { listener },
                    Instant::now(),
                )
                .expect("duplicate accept is synchronous");
            assert!(matches!(
                busy.result,
                CallOutput::Failed(CallError::ResourceBusy)
            ));

            // Work on a resource id that the lane never handed out is typed
            // InvalidResource, not a panic or a hang.
            let bogus_listener = UnixListenerId::new(9_999);
            let invalid = driver
                .submit(
                    CallId::new(4),
                    CallInput::UnixAccept {
                        listener: bogus_listener,
                    },
                    Instant::now(),
                )
                .expect("invalid accept is synchronous");
            assert!(matches!(
                invalid.result,
                CallOutput::Failed(CallError::InvalidResource)
            ));
            let bogus_stream = UnixStreamId::new(9_999);
            let invalid_read = driver
                .submit(
                    CallId::new(5),
                    CallInput::UnixRead {
                        stream: bogus_stream,
                        max_len: 16,
                    },
                    Instant::now(),
                )
                .expect("invalid read is synchronous");
            assert!(matches!(
                invalid_read.result,
                CallOutput::Failed(CallError::InvalidResource)
            ));

            let _ = driver.cancel_pending(Instant::now() + Duration::from_millis(100));
        }

        #[test]
        fn duplicate_read_lane_is_resource_busy() {
            let io_loop = io_loop(Global).expect("init io loop");
            let mut driver = BetelgeuseDriver::with_io_loop(io_loop);
            let (_listener, server, _client) = connected_pair(&mut driver);

            // No bytes are queued, so the first read stays pending.
            assert!(
                driver
                    .submit(
                        CallId::new(200),
                        CallInput::UnixRead {
                            stream: server,
                            max_len: 16
                        },
                        Instant::now()
                    )
                    .is_none()
            );
            let busy = driver
                .submit(
                    CallId::new(201),
                    CallInput::UnixRead {
                        stream: server,
                        max_len: 16,
                    },
                    Instant::now(),
                )
                .expect("duplicate read is synchronous");
            assert!(matches!(
                busy.result,
                CallOutput::Failed(CallError::ResourceBusy)
            ));

            let _ = driver.cancel_pending(Instant::now() + Duration::from_millis(100));
        }

        #[test]
        fn close_listener_cancels_pending_accept_without_hang_or_leak() {
            let io_loop = io_loop(Global).expect("init io loop");
            let mut driver = BetelgeuseDriver::with_io_loop(io_loop);
            let listener = bind_listener(&mut driver, 1);
            assert!(
                driver
                    .submit(
                        CallId::new(2),
                        CallInput::UnixAccept { listener },
                        Instant::now()
                    )
                    .is_none()
            );
            assert_eq!(driver.resource_report().listeners, 1);

            // Close wins over the pending accept.
            let closed = driver
                .submit(
                    CallId::new(3),
                    CallInput::UnixListenerClose { listener },
                    Instant::now(),
                )
                .expect("close completes inline");
            assert!(matches!(closed.result, CallOutput::UnixListenerClosed));

            // The runtime drains the close-cancelled accept id; its
            // continuation must not fire as a fresh completion.
            let cancelled = driver.take_cancelled_by_close();
            assert!(cancelled.contains(&CallId::new(2)));

            let mut spurious = Vec::new();
            for _ in 0..16 {
                driver.advance(Instant::now(), &mut spurious);
                thread::sleep(Duration::from_millis(1));
            }
            assert!(
                !spurious.iter().any(|c| c.call_id == CallId::new(2)),
                "cancelled accept must not deliver a completion"
            );
            // Listener gone; the cancelled accept no longer counts as pending.
            assert_eq!(driver.resource_report().listeners, 0);
            assert_eq!(driver.resource_report().pending_driver_call_count(), 0);
            assert!(!driver.unix.has_pending());

            let _ = driver.cancel_pending(Instant::now() + Duration::from_millis(100));
        }

        #[test]
        fn close_stream_cancels_pending_read() {
            let io_loop = io_loop(Global).expect("init io loop");
            let mut driver = BetelgeuseDriver::with_io_loop(io_loop);
            let (_listener, server, _client) = connected_pair(&mut driver);
            assert!(
                driver
                    .submit(
                        CallId::new(300),
                        CallInput::UnixRead {
                            stream: server,
                            max_len: 16
                        },
                        Instant::now()
                    )
                    .is_none()
            );

            let closed = driver
                .submit(
                    CallId::new(301),
                    CallInput::UnixStreamClose { stream: server },
                    Instant::now(),
                )
                .expect("close completes inline");
            assert!(matches!(closed.result, CallOutput::UnixStreamClosed));
            assert!(driver.take_cancelled_by_close().contains(&CallId::new(300)));

            let mut spurious = Vec::new();
            for _ in 0..16 {
                driver.advance(Instant::now(), &mut spurious);
                thread::sleep(Duration::from_millis(1));
            }
            assert!(!spurious.iter().any(|c| c.call_id == CallId::new(300)));
            assert!(!driver.unix.has_pending());

            let _ = driver.cancel_pending(Instant::now() + Duration::from_millis(100));
        }

        #[test]
        fn shutdown_after_pending_unix_work_is_clean_within_budget() {
            let io_loop = io_loop(Global).expect("init io loop");
            let mut driver = BetelgeuseDriver::with_io_loop(io_loop);
            let listener = bind_listener(&mut driver, 1);
            // In-flight accept with no peer: nothing will complete it.
            assert!(
                driver
                    .submit(
                        CallId::new(2),
                        CallInput::UnixAccept { listener },
                        Instant::now()
                    )
                    .is_none()
            );
            assert!(driver.unix.has_pending());

            let budget = Duration::from_millis(100);
            let started = Instant::now();
            let result = driver.cancel_pending(Instant::now() + budget);
            let elapsed = started.elapsed();
            assert!(
                elapsed < budget * 10,
                "unix shutdown took {elapsed:?}, expected to return near {budget:?}"
            );
            // The shared-loop release either drains the slot (Ok) or reports
            // the exact driver truth; on the local backend the cancel is
            // synchronous, so it must be clean and leave nothing pending.
            assert!(
                result.is_ok(),
                "local backend cancel is synchronous; shutdown should be clean"
            );
            assert_eq!(driver.resource_report().pending_driver_call_count(), 0);
        }

        #[test]
        fn parked_then_closed_accept_does_not_strand_a_pending_call_and_reaps_by_shutdown() {
            // The hard case: arm an accept, ADVANCE so the backend parks it
            // (watched on the event loop), THEN close the listener. The
            // close-cancelled op must (a) immediately stop counting as a
            // pending driver call, (b) never deliver a spurious completion,
            // and (c) leave no physical pending entry once the bounded
            // shutdown drain releases the backend's completion slot.
            let io_loop = io_loop(Global).expect("init io loop");
            let mut driver = BetelgeuseDriver::with_io_loop(io_loop);
            let listener = bind_listener(&mut driver, 1);
            assert!(
                driver
                    .submit(
                        CallId::new(2),
                        CallInput::UnixAccept { listener },
                        Instant::now()
                    )
                    .is_none()
            );
            // Park it: with no peer, the accept registers with the event loop.
            let mut sink = Vec::new();
            for _ in 0..4 {
                driver.advance(Instant::now(), &mut sink);
            }
            assert!(sink.is_empty(), "no peer, so accept must not complete yet");
            assert_eq!(driver.unix.physical_pending_len(), 1);

            // Close wins over the parked accept.
            let closed = driver
                .submit(
                    CallId::new(3),
                    CallInput::UnixListenerClose { listener },
                    Instant::now(),
                )
                .expect("close completes inline");
            assert!(matches!(closed.result, CallOutput::UnixListenerClosed));
            assert!(driver.take_cancelled_by_close().contains(&CallId::new(2)));
            // Immediately uncounted as a pending driver call.
            assert_eq!(driver.resource_report().pending_driver_call_count(), 0);

            let mut spurious = Vec::new();
            for _ in 0..16 {
                driver.advance(Instant::now(), &mut spurious);
                thread::sleep(Duration::from_millis(1));
            }
            assert!(
                !spurious.iter().any(|c| c.call_id == CallId::new(2)),
                "close-cancelled accept must never deliver a completion"
            );

            // Bounded shutdown must reap every physical entry: the backend
            // releases the completion slot and the lane drops the tombstone.
            let result = driver.cancel_pending(Instant::now() + Duration::from_millis(200));
            assert!(result.is_ok(), "shutdown should release the backend slot");
            assert_eq!(
                driver.unix.physical_pending_len(),
                0,
                "shutdown must leave no stranded pending entry"
            );
        }

        #[test]
        fn many_connect_close_cycles_keep_logical_pending_zero_and_shutdown_reaps_all() {
            // Open/connect/close many streams. Each cycle reads with no data
            // (parking the read on the event loop) then closes the stream.
            //
            // Two honest invariants, matching the TCP/TLS lanes:
            //  - The *logical* pending count (work the runtime still waits on)
            //    stays at zero across cycles — a close-cancelled read never
            //    counts and never delivers a completion.
            //  - A close-cancelled read that was already parked leaves a
            //    physical tombstone Box: it cannot be freed while the backend
            //    still holds a pointer to it, and Betelgeuse exposes no per-op
            //    cancel (only whole-loop `cancel_pending_completions`). Those
            //    tombstones are released by the bounded shutdown drain — so
            //    physical pending may rise during the run but is fully reaped
            //    at shutdown, never permanently stranded.
            let io_loop = io_loop(Global).expect("init io loop");
            let mut driver = BetelgeuseDriver::with_io_loop(io_loop);
            let mut next = 1_000u64;
            for _ in 0..24 {
                let (_listener, server, _client) = connected_pair(&mut driver);
                assert!(
                    driver
                        .submit(
                            CallId::new(next),
                            CallInput::UnixRead {
                                stream: server,
                                max_len: 16
                            },
                            Instant::now()
                        )
                        .is_none()
                );
                next += 1;
                // Park the read.
                let mut sink = Vec::new();
                driver.advance(Instant::now(), &mut sink);
                // Close wins over the parked read.
                let closed = driver
                    .submit(
                        CallId::new(next),
                        CallInput::UnixStreamClose { stream: server },
                        Instant::now(),
                    )
                    .expect("close completes inline");
                assert!(matches!(closed.result, CallOutput::UnixStreamClosed));
                next += 1;
                let _ = driver.take_cancelled_by_close();
                driver.advance(Instant::now(), &mut sink);
                // The runtime never waits on a close-cancelled read.
                assert_eq!(driver.resource_report().pending_driver_call_count(), 0);
                assert!(!driver.unix.has_pending());
            }

            // Bounded shutdown reaps any close-cancelled tombstones the backend
            // still referenced, leaving nothing stranded.
            let result = driver.cancel_pending(Instant::now() + Duration::from_millis(500));
            assert!(result.is_ok(), "shutdown should release all backend slots");
            assert_eq!(driver.unix.physical_pending_len(), 0);
        }

        #[test]
        fn connect_to_missing_path_is_typed_not_found() {
            // A connect to a path with no listener must surface a typed
            // NotFound, not Io or a hang.
            let io_loop = io_loop(Global).expect("init io loop");
            let mut driver = BetelgeuseDriver::with_io_loop(io_loop);
            let missing = unique_sock("missing");
            assert!(
                driver
                    .submit(
                        CallId::new(1),
                        CallInput::UnixConnect {
                            path: missing.clone()
                        },
                        Instant::now()
                    )
                    .is_none()
            );
            let completed = advance_until(&mut driver, |all| {
                all.iter().any(|c| c.call_id == CallId::new(1))
            });
            let result = completed
                .iter()
                .find(|c| c.call_id == CallId::new(1))
                .map(|c| &c.result)
                .expect("connect to missing path completes");
            assert!(
                matches!(result, CallOutput::Failed(CallError::NotFound)),
                "expected NotFound for a missing path, got {result:?}"
            );

            let _ = driver.cancel_pending(Instant::now() + Duration::from_millis(100));
        }
    }
}
