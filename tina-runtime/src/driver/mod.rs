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
use std::io::{ErrorKind, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream, ToSocketAddrs, UdpSocket};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::mpsc::{
    Receiver, SyncSender, TryRecvError, TrySendError as MpscTrySendError, sync_channel,
};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use betelgeuse::{
    AcceptCompletion, ConnectCompletion, FsyncCompletion, IO, IOFile, IOLoop, IOLoopHandle,
    IOSocket, MkdirCompletion, OpenOptions, PReadCompletion, PWriteCompletion, RecvCompletion,
    SendCompletion, SizeCompletion, io_loop,
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

use dns::DnsLane;
#[cfg(test)]
use dns::DnsWorkerLane;
use process::{ProcessCommand, ProcessLane};
pub use signals::os_signal_capture_supported;
use signals::{OsSignalDispatcher, SignalWaitEntry};
use storage::{StorageJob, StorageLane};
use tcp::BetelgeuseTcp;
use tls::TlsLane;
#[cfg(test)]
use tls::{TlsCompletion, TlsCompletionResult, TlsPending, TlsPendingLane, TlsWorkerLane};

const INITIAL_DRIVER_TIMER_CAPACITY: usize = 8;
const INITIAL_DRIVER_RESOURCE_CAPACITY: usize = 4;
const INITIAL_DRIVER_PENDING_CAPACITY: usize = 8;
pub(crate) const DEFAULT_STORAGE_LANE_CAPACITY: usize = 64;
pub(crate) const DEFAULT_DNS_LANE_CAPACITY: usize = 16;
pub(crate) const DEFAULT_TLS_LANE_CAPACITY: usize = 64;
pub(crate) const DEFAULT_PROCESS_LANE_CAPACITY: usize = 16;
pub(crate) const DEFAULT_SIGNAL_CAPACITY: usize = 64;

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
/// Three independent count vocabularies, per Phase 043 plan:
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
    signals: Vec<SignalWaitEntry>,
    signal_capacity: usize,
    timers: Vec<TimerEntry>,
    next_timer_ordinal: u64,
    os_signals: OsSignalDispatcher,
}

/// One pending timer tracked by the driver.
#[derive(Debug)]
struct TimerEntry {
    call_id: CallId,
    deadline: Instant,
    insertion_order: u64,
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
            storage: StorageLane::inline(),
            dns: DnsLane::new(DEFAULT_DNS_LANE_CAPACITY),
            tls: TlsLane::new(DEFAULT_TLS_LANE_CAPACITY),
            process: ProcessLane::new(DEFAULT_PROCESS_LANE_CAPACITY),
            signals: Vec::with_capacity(
                DEFAULT_SIGNAL_CAPACITY.min(INITIAL_DRIVER_PENDING_CAPACITY),
            ),
            signal_capacity: DEFAULT_SIGNAL_CAPACITY,
            timers: Vec::with_capacity(INITIAL_DRIVER_TIMER_CAPACITY),
            next_timer_ordinal: 0,
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
    ) -> Self {
        Self {
            tcp: BetelgeuseTcp::with_io_loop(io_loop),
            storage: StorageLane::new(storage_lane_capacity),
            dns: DnsLane::new(dns_lane_capacity),
            tls: TlsLane::new(tls_lane_capacity),
            process: ProcessLane::new(process_lane_capacity),
            signals: Vec::with_capacity(signal_capacity.min(INITIAL_DRIVER_PENDING_CAPACITY)),
            signal_capacity,
            timers: Vec::with_capacity(INITIAL_DRIVER_TIMER_CAPACITY),
            next_timer_ordinal: 0,
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
                let insertion_order = self.next_timer_ordinal;
                self.next_timer_ordinal += 1;
                self.timers.push(TimerEntry {
                    call_id,
                    deadline: now + after,
                    insertion_order,
                });
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
                timeout,
            } => {
                self.tls
                    .submit_connect(call_id, addr, server_name, root_certificates, timeout, now)
            }
            CallInput::TlsBind {
                addr,
                certificate_chain,
                private_key,
            } => self
                .tls
                .submit_bind(call_id, addr, certificate_chain, private_key, now),
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
            CallInput::TlsWrite {
                stream,
                bytes,
                timeout,
            } => self.tls.submit_write(call_id, stream, bytes, timeout, now),
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
            other => self.tcp.submit(call_id, other),
        }
    }

    fn advance(&mut self, now: Instant, completed: &mut Vec<DriverCompletion>) {
        self.tcp.advance(completed);
        self.storage.advance(completed);
        self.dns.advance(now, completed);
        self.tls.advance(now, completed);
        self.process.advance(completed);
        self.poll_os_signals(completed);
        self.harvest_signals(now, completed);
        self.harvest_timers(now, completed);
    }

    fn take_cancelled_by_close(&mut self) -> Vec<CallId> {
        self.tcp.take_cancelled_by_close()
    }

    fn has_pending(&self) -> bool {
        self.tcp.has_pending()
            || self.storage.has_pending()
            || self.dns.has_pending()
            || self.tls.has_pending()
            || self.process.has_pending()
            || self.signals.iter().any(|entry| !entry.cancelled)
            || !self.timers.is_empty()
    }

    fn cancel_pending(&mut self, deadline: Instant) -> Result<(), DriverShutdownError> {
        self.timers.clear();
        self.signals.clear();
        self.storage.cancel_pending(deadline);
        self.dns.cancel_pending(deadline);
        self.tls.cancel_pending(deadline);
        self.process.cancel_pending(deadline);
        self.tcp.cancel_pending(deadline)
    }

    fn cancel(&mut self, call_id: CallId) -> bool {
        let before = self.timers.len();
        self.timers.retain(|entry| entry.call_id != call_id);
        let signal_before = self.signals.len();
        self.signals.retain(|entry| entry.call_id != call_id);
        before != self.timers.len()
            || signal_before != self.signals.len()
            || self.storage.cancel(call_id)
            || self.dns.cancel(call_id)
            || self.tls.cancel(call_id)
            || self.process.cancel(call_id)
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
            listeners: tcp.listeners + tls.listeners,
            streams: tcp.streams,
            tls_streams: tls.tls_streams,
            udp_sockets: tcp.udp_sockets,
            files: tcp.files,
            // Worker-held: TLS in-flight ops parking cloned listener/stream
            // arcs, plus process calls owning a live Child. DNS/storage
            // workers do not hold a runtime-visible OS handle, so they
            // contribute zero here per the plan's count rules.
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
            deadline: now + timeout,
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
        while let Some(index) = self
            .timers
            .iter()
            .enumerate()
            .filter(|(_, entry)| entry.deadline <= now)
            .min_by(|(_, left), (_, right)| {
                left.deadline
                    .cmp(&right.deadline)
                    .then_with(|| left.insertion_order.cmp(&right.insertion_order))
            })
            .map(|(index, _)| index)
        {
            let entry = self.timers.remove(index);
            completed.push(DriverCompletion {
                call_id: entry.call_id,
                result: CallOutput::TimerFired,
            });
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
        let mut lane = StorageLane::new(1);
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
        started_rx.recv().expect("parked storage job started");

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
        let mut lane = StorageLane::new(1);
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
        started_rx.recv().expect("parked storage job started");
        assert!(lane.cancel(CallId::new(7)));
        assert!(!lane.has_pending());

        release_tx.send(()).expect("release parked storage job");
        let mut completed = Vec::new();
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
    fn tls_lane_connects_writes_reads_and_closes_against_local_rustls_server() {
        use std::io::{Read, Write};
        use std::net::TcpListener;

        let _ = rustls::crypto::ring::default_provider().install_default();
        let certified = rcgen::generate_simple_self_signed(vec!["localhost".to_string()])
            .expect("generate local cert");
        let cert_der = certified.cert.der().to_vec();
        let key_der = certified.key_pair.serialize_der();
        let server_cert = rustls::pki_types::CertificateDer::from(cert_der.clone());
        let server_key = rustls::pki_types::PrivateKeyDer::Pkcs8(
            rustls::pki_types::PrivatePkcs8KeyDer::from(key_der),
        );
        let server_config = rustls::ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(vec![server_cert], server_key)
            .expect("server config");

        let listener = TcpListener::bind("127.0.0.1:0").expect("bind tls test listener");
        let addr = listener.local_addr().expect("local addr");
        let server = thread::spawn(move || {
            let (tcp, _) = listener.accept().expect("accept tls client");
            let connection =
                rustls::ServerConnection::new(Arc::new(server_config)).expect("server conn");
            let mut stream = rustls::StreamOwned::new(connection, tcp);
            let mut request = [0_u8; 4];
            stream.read_exact(&mut request).expect("read request");
            assert_eq!(&request, b"ping");
            stream.write_all(b"pong").expect("write response");
            stream.flush().expect("flush response");
        });

        let mut lane = TlsWorkerLane::new(4);
        assert!(
            lane.submit_connect(
                CallId::new(1),
                addr,
                "localhost".to_string(),
                vec![cert_der],
                Duration::from_secs(1),
                Instant::now(),
            )
            .is_none()
        );
        let stream = wait_for_tls_completion(&mut lane, CallId::new(1), |output| match output {
            CallOutput::TlsConnected { stream } => Some(stream),
            other => panic!("unexpected TLS connect output: {other:?}"),
        });

        assert!(
            lane.submit_write(
                CallId::new(2),
                stream,
                b"ping".to_vec(),
                Duration::from_secs(1),
                Instant::now(),
            )
            .is_none()
        );
        let wrote = wait_for_tls_completion(&mut lane, CallId::new(2), |output| match output {
            CallOutput::TlsWrote { count } => Some(count),
            other => panic!("unexpected TLS write output: {other:?}"),
        });
        assert_eq!(wrote, 4);

        assert!(
            lane.submit_read(
                CallId::new(3),
                stream,
                4,
                Duration::from_secs(1),
                Instant::now()
            )
            .is_none()
        );
        let read = wait_for_tls_completion(&mut lane, CallId::new(3), |output| match output {
            CallOutput::TlsRead { bytes } => Some(bytes),
            other => panic!("unexpected TLS read output: {other:?}"),
        });
        assert_eq!(read, b"pong");

        assert!(
            lane.submit_close(
                CallId::new(4),
                stream,
                Duration::from_secs(1),
                Instant::now()
            )
            .is_none()
        );
        wait_for_tls_completion(&mut lane, CallId::new(4), |output| match output {
            CallOutput::TlsClosed => Some(()),
            other => panic!("unexpected TLS close output: {other:?}"),
        });
        server.join().expect("server thread");
    }

    #[test]
    fn tls_lane_deadline_tombstones_queued_work_until_late_completion() {
        let (command_sender, _command_receiver) = sync_channel(1);
        let (completion_sender, completions) = sync_channel(1);
        let cancelled = Arc::new(AtomicBool::new(false));
        let now = Instant::now();
        let mut lane = TlsWorkerLane {
            capacity: 1,
            sender: Some(command_sender),
            completions,
            handle: None,
            pending: vec![TlsPending {
                call_id: CallId::new(42),
                lane: TlsPendingLane::Connect(CallId::new(42)),
                deadline: now + Duration::from_millis(1),
                cancelled: Arc::clone(&cancelled),
                timed_out: false,
            }],
            listeners: Vec::new(),
            streams: Vec::new(),
            next_listener_id: 1,
            next_stream_id: 1,
            cancelled_by_close: Vec::new(),
        };

        let mut completed = Vec::new();
        lane.advance(now + Duration::from_millis(2), &mut completed);
        assert_eq!(completed.len(), 1);
        assert_eq!(completed[0].call_id, CallId::new(42));
        assert!(matches!(
            completed[0].result,
            CallOutput::Failed(CallError::Timeout)
        ));
        assert!(cancelled.load(Ordering::Acquire));
        assert!(!lane.has_pending());
        assert_eq!(lane.pending.len(), 1);

        completion_sender
            .send(TlsCompletion {
                call_id: CallId::new(42),
                result: TlsCompletionResult::Output(CallOutput::TlsClosed),
            })
            .expect("send late TLS completion");
        completed.clear();
        lane.advance(now + Duration::from_millis(3), &mut completed);
        assert!(completed.is_empty());
        assert!(lane.pending.is_empty());
    }

    fn wait_for_tls_completion<T>(
        lane: &mut TlsWorkerLane,
        call_id: CallId,
        map: impl Fn(CallOutput) -> Option<T>,
    ) -> T {
        let mut completed = Vec::new();
        let deadline = Instant::now() + Duration::from_secs(3);
        while Instant::now() < deadline {
            lane.advance(Instant::now(), &mut completed);
            if let Some(index) = completed
                .iter()
                .position(|completion| completion.call_id == call_id)
            {
                let completion = completed.remove(index);
                return map(completion.result).expect("mapped TLS completion");
            }
            thread::sleep(Duration::from_millis(1));
        }
        panic!("TLS completion {call_id:?} did not arrive");
    }

    #[test]
    fn storage_lane_shutdown_skips_buffered_work_that_never_started() {
        let mut lane = StorageLane::new(2);
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
        first_started_rx
            .recv()
            .expect("first parked storage job started");
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

        lane.cancel(CallId::new(12));
        first_release_tx
            .send(())
            .expect("release first parked storage job");
        lane.cancel_pending(Instant::now());
        assert!(
            queued_started_rx.try_recv().is_err(),
            "queued storage work must not start after shutdown cancellation"
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
        // After Phase 043, the pending count tracks physical entries so
        // cancelled-but-not-yet-drained ops stay visible until the
        // backend releases the completion slot. After a shutdown drain
        // the count must reach zero again.
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

    #[test]
    fn storage_park_counts_as_pending_only() {
        let mut lane = StorageLane::new(1);
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
        started_rx.recv().expect("park job started");
        // Storage contributes to pending_calls but not worker_held: the
        // worker thread does the OS work but the runtime does not see a
        // separate handle.
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

    #[test]
    fn tls_pending_op_contributes_to_worker_held_and_pending() {
        // Drive a TLS connect to a non-listening port so the lane stays
        // in flight long enough to observe the count. The connect will
        // eventually fail, but during the in-flight window the lane
        // reports both worker-held and pending.
        let mut lane = TlsWorkerLane::new(2);
        let unreachable: SocketAddr = "127.0.0.1:1".parse().expect("loopback addr");
        assert!(
            lane.submit_connect(
                CallId::new(1),
                unreachable,
                "localhost".to_string(),
                vec![],
                Duration::from_secs(2),
                Instant::now(),
            )
            .is_none()
        );
        let report = lane.resource_report();
        assert_eq!(
            report.worker_held_resource_count(),
            report.pending_driver_call_count(),
            "TLS in-flight ops contribute equally to worker_held and pending"
        );
        assert!(report.pending_driver_call_count() >= 1);

        // Drain so the lane drop does not race with the live connect.
        let mut completed = Vec::new();
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            lane.advance(Instant::now(), &mut completed);
            if !completed.is_empty() {
                break;
            }
            thread::sleep(Duration::from_millis(5));
        }
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
    // Each lane's `cancel_pending(deadline)` must return inside the
    // budget even when the worker is stuck, surfacing remaining work via
    // the pending count rather than blocking shutdown forever.
    // -------------------------------------------------------------------

    #[test]
    fn storage_lane_shutdown_returns_within_budget_when_worker_stuck() {
        let mut lane = StorageLane::new(1);
        let (started_tx, started_rx) = sync_channel(1);
        // Park job that will not be released — simulates a stuck worker.
        let (_release_tx, release_rx) = sync_channel(1);
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
        started_rx.recv().expect("park job started");

        let budget = Duration::from_millis(50);
        let started = Instant::now();
        lane.cancel_pending(Instant::now() + budget);
        let elapsed = started.elapsed();

        // Bounded: must not exceed budget by an order of magnitude even
        // under load on a busy CI machine.
        assert!(
            elapsed < budget * 10,
            "shutdown took {elapsed:?}, expected to return near {budget:?}"
        );
        // Stuck work stays visible in the physical count.
        assert!(
            lane.physical_pending_count() >= 1,
            "stuck park job must remain visible after bounded shutdown"
        );
    }

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
}
