//! I/O subsystem interfaces.
//!
//! This module defines the abstract interfaces presented by the I/O subsystem:
//! - [`IOLoop`] advances a concrete backend and retires completed work
//! - [`IO`] submits new operations and allocates backend objects
//! - [`IOFile`] and [`IOSocket`] expose file and socket capabilities
//! - one concrete completion type per operation kind (e.g. [`RecvCompletion`])
//!   provides the caller-owned record for one in-flight operation
//!
//! The subsystem is completion-based. There is one concrete completion type
//! per operation kind ([`RecvCompletion`], [`SendCompletion`], ...). The
//! caller prepares and owns the matching one, passes it to the operation
//! (e.g. [`IO::mkdir`], [`IOSocket::recv`]), and later observes the typed
//! result in the same object. The backend translates abstract operations
//! into concrete kernel work, advances them to completion, and writes the
//! typed result.
//!
//! Separating [`IOLoop`] from [`IO`] preserves a clear distinction between
//! driving the backend and issuing work into it. A single implementation may
//! provide both interfaces, but the contracts remain conceptually distinct:
//! one interface progresses the subsystem, the other requests service from it.

#![feature(allocator_api)]
#![feature(coroutine_trait)]
#![feature(stmt_expr_attributes)]

use std::{
    alloc::Allocator,
    io as stdio,
    net::SocketAddr,
    path::Path,
    rc::Rc,
    sync::{Arc, Condvar, Mutex},
    time::Duration,
};

pub mod completion;
pub mod io;
pub mod op;
pub mod slab;
pub mod task;

pub use completion::{
    AcceptCompletion, AcceptOp, ConnectCompletion, ConnectOp, FsyncCompletion, FsyncOp,
    MkdirCompletion, MkdirOp, PReadCompletion, PReadOp, PWriteCompletion, PWriteOp,
    RecvBufCompletion, RecvCompletion, RecvOp, SendCompletion, SendOp, SendOwnedCompletion,
    SizeCompletion, SizeOp,
};

pub use completion::{CompletionInner, Operation};

/// Drives a concrete I/O backend forward.
///
/// An implementation is expected to:
/// - submit queued work to the kernel as needed
/// - harvest completed operations
/// - complete the caller-owned completion objects associated with them
///
/// The return value indicates whether this tick observed or produced useful
/// work. Callers may use that signal for blocking, idling, or simulation logic.
pub trait IOLoop: IO {
    /// Advances the backend by one iteration.
    ///
    /// Returns `Ok(true)` when the backend submitted or completed at least one
    /// unit of work during this step, and `Ok(false)` when nothing progressed.
    ///
    /// This is the non-blocking drain: it submits queued work and harvests any
    /// already-ready completions, then returns immediately. It never sleeps.
    fn step(&self) -> stdio::Result<bool>;

    /// Advances the backend, then parks until work is ready or `timeout`.
    ///
    /// Same submit + harvest as [`step`](Self::step), but after submitting
    /// queued work the backend may sleep up to `timeout` for a completion or a
    /// doorbell wake (see [`waker`](Self::waker)). `Some(d)` caps the sleep at
    /// `d`; `None` blocks until a real event.
    ///
    /// A `None` wait is only legal because the backend keeps a doorbell armed:
    /// there is always a possible wake source (the doorbell), so `None` cannot
    /// block forever with nothing able to wake it. Callers that pass `None`
    /// must hold a [`waker`](Self::waker) and ring it (e.g. after admitting a
    /// command) so the parked loop wakes promptly.
    ///
    /// The doorbell is coalescing: a `wake()` that lands just before the loop
    /// parks is still observed (it does not sleep through it).
    fn step_blocking(&self, timeout: Option<Duration>) -> stdio::Result<bool>;

    /// Returns a thread-safe handle that wakes this loop from
    /// [`step_blocking`](Self::step_blocking).
    ///
    /// The returned [`IOWaker`] is a **separate OS handle** (an eventfd on
    /// Linux, an `EVFILT_USER` registration on macOS, a condvar on the
    /// simulated backend), not a clone of the `Rc<dyn IOLoop>`. It is
    /// `Send + Sync + Clone` so host threads may hold and ring it, but it can
    /// only wake the loop — it never touches backend state.
    fn waker(&self) -> IOWaker;

    /// Returns how many completion slots the backend still owns by pointer.
    ///
    /// This is a lifecycle hook for adapter layers that own completion
    /// storage. Normal callers should observe typed completion objects. Runtime
    /// adapters use this during shutdown to know when completion slots can be
    /// dropped without leaving backend-owned raw pointers behind.
    fn pending_completion_count(&self) -> usize {
        0
    }

    /// Requests cancellation/release for every backend-owned completion slot.
    ///
    /// Implementations should complete queued operations with an error or
    /// request kernel-side cancellation for submitted work. The caller must
    /// keep the completion slots alive and continue stepping until
    /// [`IOLoop::pending_completion_count`] reaches zero.
    fn cancel_pending_completions(&self) -> stdio::Result<()> {
        Ok(())
    }

    /// Controls whether socket read/write operations may arm kernel-side
    /// readiness waits that are intended for [`step_blocking`](Self::step_blocking).
    ///
    /// The default is `false`: [`step`](Self::step) must remain a non-blocking
    /// drain, so backends should submit socket operations in a non-blocking
    /// form. Threaded runtimes that park on `step_blocking` enable this mode so
    /// pending socket reads/writes wake the parked worker on readiness instead
    /// of completing immediately with `WouldBlock` and requeueing.
    fn set_blocking_socket_io(&self, _enabled: bool) {}
}

/// Thread-safe handle that wakes a parked [`IOLoop::step_blocking`].
///
/// Cloneable and `Send + Sync`. A host thread holds one and calls
/// [`wake`](IOWaker::wake) to interrupt a loop blocked in
/// [`IOLoop::step_blocking`]. It owns only a doorbell (its own OS handle), so
/// it cannot read or mutate any backend state.
#[derive(Clone)]
pub struct IOWaker {
    doorbell: Arc<dyn Doorbell>,
}

impl IOWaker {
    /// Constructs a waker around a backend-provided doorbell.
    pub fn new(doorbell: Arc<dyn Doorbell>) -> Self {
        Self { doorbell }
    }

    /// Wakes the associated loop if it is (or is about to be) parked.
    ///
    /// Coalescing: repeated `wake()`s collapse into a single observed wake, and
    /// a `wake()` that races just ahead of the park is still observed.
    pub fn wake(&self) {
        self.doorbell.wake();
    }
}

impl std::fmt::Debug for IOWaker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("IOWaker")
    }
}

/// Backend doorbell rung by an [`IOWaker`] from any thread.
///
/// Implementations own a single OS-level wake primitive (eventfd,
/// `EVFILT_USER`, condvar). `wake()` must be safe to call from a thread other
/// than the one driving the loop.
pub trait Doorbell: Send + Sync {
    /// Signals the loop to wake from a blocking park.
    fn wake(&self);
}

/// A condvar-backed doorbell + coalescing flag.
///
/// Reusable wake primitive for backends without a kernel fd to block on (the
/// simulated backend and test loops). The `rung` flag coalesces a wake that
/// arrives before the park, so [`CondvarDoorbell::wait`] never sleeps through a
/// signal it has not yet observed.
pub struct CondvarDoorbell {
    rung: Mutex<bool>,
    cv: Condvar,
}

impl CondvarDoorbell {
    /// Creates a shareable doorbell.
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            rung: Mutex::new(false),
            cv: Condvar::new(),
        })
    }

    /// Blocks until woken or `timeout` elapses, then clears the wake flag.
    ///
    /// Returns immediately if a wake is already pending (coalesced). `None`
    /// blocks until a wake; callers that pass `None` must guarantee a `wake()`
    /// will arrive.
    pub fn wait(&self, timeout: Option<Duration>) {
        let mut rung = self.rung.lock().expect("doorbell mutex poisoned");
        if !*rung {
            match timeout {
                Some(d) => {
                    let (guard, _) = self
                        .cv
                        .wait_timeout(rung, d)
                        .expect("doorbell condvar poisoned");
                    rung = guard;
                }
                None => {
                    rung = self.cv.wait(rung).expect("doorbell condvar poisoned");
                }
            }
        }
        *rung = false;
    }
}

impl Doorbell for CondvarDoorbell {
    fn wake(&self) {
        let mut rung = self.rung.lock().expect("doorbell mutex poisoned");
        *rung = true;
        self.cv.notify_all();
    }
}

/// Submits file-system and socket operations to the backend.
///
/// This interface creates backend objects and arms caller-owned typed
/// completion slots. The backend owns syscall translation, retry policy,
/// and kernel-specific details; callers own completion lifetimes and observe
/// typed results via each completion's `take_result`.
pub trait IO {
    /// Opens a file with the requested [`OpenOptions`].
    fn open(&self, path: &Path, options: OpenOptions) -> stdio::Result<Box<dyn IOFile>>;

    /// Creates a new socket object owned by the caller.
    fn socket(&self) -> stdio::Result<Box<dyn IOSocket>>;

    /// Submits a single-directory creation operation.
    fn mkdir(&self, c: &mut MkdirCompletion, path: &Path, mode: u32) -> stdio::Result<()>;

    /// Returns a short backend name for logging and diagnostics.
    fn backend_name(&self) -> &'static str;
}

/// Open flags for [`IO::open`].
#[derive(Debug, Clone, Copy, Default)]
pub struct OpenOptions {
    /// Open the file for reading.
    pub read: bool,
    /// Open the file for writing.
    pub write: bool,
    /// Create the file if it does not already exist.
    pub create: bool,
    /// Truncate the file to zero length on open.
    pub truncate: bool,
}

/// Backend-agnostic file operations.
///
/// A file object is a handle that submits work into caller-owned typed
/// completion slots. The backend later completes those slots with the
/// matching typed result (`Vec<u8>` for reads, `usize` for writes, etc.).
pub trait IOFile {
    /// Reads up to `len` bytes starting at `offset`.
    fn pread(&self, c: &mut PReadCompletion, len: usize, offset: u64) -> stdio::Result<()>;

    /// Writes `buf` starting at `offset`.
    fn pwrite(&self, c: &mut PWriteCompletion, buf: Vec<u8>, offset: u64) -> stdio::Result<()>;

    /// Flushes file data to stable storage.
    fn fsync(&self, c: &mut FsyncCompletion) -> stdio::Result<()>;

    /// Reads the current file size.
    fn size(&self, c: &mut SizeCompletion) -> stdio::Result<()>;
}

/// Backend-agnostic socket operations.
///
/// A socket object follows the same caller-owned completion model as files.
/// Distinct completion slots may be used concurrently for independent socket
/// operations, subject to the rules imposed by the concrete backend.
pub trait IOSocket {
    /// Binds the socket to `addr`.
    fn bind(&self, addr: SocketAddr) -> stdio::Result<()>;

    /// Binds the socket to a Unix-domain `path` and starts listening.
    ///
    /// Unix-domain sockets are sockets: bind/accept/recv/send/close all
    /// follow the same completion model as the internet rail. Only the
    /// addressing differs, so this is a separate entry point rather than an
    /// overloaded [`IOSocket::bind`]. The accepted streams returned by
    /// [`IOSocket::accept`] are ordinary stream sockets and need no
    /// Unix-specific handling. Backends own the socket-file lifecycle: a
    /// stale socket file at `path` is cleared before bind, and the file is
    /// unlinked when the listener is [`IOSocket::close`]d.
    fn bind_unix(&self, path: &Path) -> stdio::Result<()>;

    /// Connects this socket to a Unix-domain `path`.
    fn connect_unix(&self, c: &mut ConnectCompletion, path: &Path) -> stdio::Result<()>;

    /// Returns the socket's local address.
    fn local_addr(&self) -> stdio::Result<SocketAddr>;

    /// Returns the socket's peer address.
    fn peer_addr(&self) -> stdio::Result<SocketAddr>;

    /// Accepts one inbound connection on a listening socket.
    fn accept(&self, c: &mut AcceptCompletion) -> stdio::Result<()>;

    /// Connects this socket to `addr`.
    fn connect(&self, c: &mut ConnectCompletion, addr: SocketAddr) -> stdio::Result<()>;

    /// Receives up to `len` bytes from a connected socket.
    fn recv(&self, c: &mut RecvCompletion, len: usize) -> stdio::Result<()>;

    /// Receives up to `max_len` bytes into caller-owned storage.
    ///
    /// The completion yields the same buffer back with its length truncated to
    /// the bytes read. Backends may grow `buffer` if its capacity is too small,
    /// but callers that keep reusing the returned buffer avoid per-read
    /// allocation.
    fn recv_buf(
        &self,
        c: &mut RecvBufCompletion,
        buffer: Vec<u8>,
        max_len: usize,
    ) -> Result<(), (stdio::Error, Vec<u8>)>;

    /// Sends the contents of `buf` on a connected socket.
    fn send(&self, c: &mut SendCompletion, buf: Vec<u8>) -> stdio::Result<()>;

    /// Sends the contents of `buf` and returns the same buffer with the
    /// accepted byte count.
    fn send_owned(
        &self,
        c: &mut SendOwnedCompletion,
        buf: Vec<u8>,
    ) -> Result<(), (stdio::Error, Vec<u8>)>;

    /// Enables or disables the `TCP_NODELAY` socket option.
    ///
    /// Must be called on a connected stream socket.
    fn set_nodelay(&self, on: bool) -> stdio::Result<()>;

    /// Closes the socket and releases any backend resources it owns.
    fn close(&self);
}

/// Shared handle that drives a concrete I/O backend.
#[derive(Clone)]
pub struct IOLoopHandle<A> {
    inner: Rc<dyn IOLoop>,
    _allocator: A,
}

impl<A> IOLoopHandle<A> {
    pub fn new(inner: Rc<dyn IOLoop>, allocator: A) -> Self {
        Self {
            inner,
            _allocator: allocator,
        }
    }

    pub fn io(&self) -> IOHandle {
        IOHandle {
            io_loop: self.inner.clone(),
        }
    }
}

impl<A> From<(Rc<dyn IOLoop>, A)> for IOLoopHandle<A> {
    fn from((inner, allocator): (Rc<dyn IOLoop>, A)) -> Self {
        Self::new(inner, allocator)
    }
}

impl<A> IO for IOLoopHandle<A> {
    fn open(&self, path: &Path, options: OpenOptions) -> stdio::Result<Box<dyn IOFile>> {
        self.inner.open(path, options)
    }

    fn socket(&self) -> stdio::Result<Box<dyn IOSocket>> {
        self.inner.socket()
    }

    fn mkdir(&self, c: &mut MkdirCompletion, path: &Path, mode: u32) -> stdio::Result<()> {
        self.inner.mkdir(c, path, mode)
    }

    fn backend_name(&self) -> &'static str {
        self.inner.backend_name()
    }
}

impl<A> IOLoop for IOLoopHandle<A> {
    fn step(&self) -> stdio::Result<bool> {
        self.inner.step()
    }

    fn step_blocking(&self, timeout: Option<Duration>) -> stdio::Result<bool> {
        self.inner.step_blocking(timeout)
    }

    fn waker(&self) -> IOWaker {
        self.inner.waker()
    }

    fn pending_completion_count(&self) -> usize {
        self.inner.pending_completion_count()
    }

    fn cancel_pending_completions(&self) -> stdio::Result<()> {
        self.inner.cancel_pending_completions()
    }

    fn set_blocking_socket_io(&self, enabled: bool) {
        self.inner.set_blocking_socket_io(enabled);
    }
}

/// Creates the native backend for the current target OS as a shared loop-capable handle.
pub fn io_loop<A: Allocator + Clone>(allocator: A) -> stdio::Result<IOLoopHandle<A>> {
    #[cfg(target_os = "linux")]
    {
        let inner: Rc<dyn IOLoop> = Rc::new(io::linux::IoUringIO::new()?);
        Ok(IOLoopHandle::new(inner, allocator))
    }
    #[cfg(target_os = "macos")]
    {
        let inner: Rc<dyn IOLoop> = Rc::new(io::darwin::DarwinIO::new()?);
        Ok(IOLoopHandle::new(inner, allocator))
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        let _ = allocator;
        Err(stdio::Error::new(
            stdio::ErrorKind::Unsupported,
            format!(
                "betelgeuse is supported on Linux and macOS only (got {})",
                std::env::consts::OS
            ),
        ))
    }
}

impl<A> From<IOLoopHandle<A>> for IOHandle {
    fn from(io_loop: IOLoopHandle<A>) -> Self {
        Self {
            io_loop: io_loop.inner,
        }
    }
}

#[derive(Clone)]
pub struct IOHandle {
    io_loop: Rc<dyn IOLoop>,
}

impl IOHandle {
    pub fn io_loop(&self) -> Rc<dyn IOLoop> {
        self.io_loop.clone()
    }
}

impl IO for IOHandle {
    fn open(&self, path: &Path, options: OpenOptions) -> stdio::Result<Box<dyn IOFile>> {
        self.io_loop.open(path, options)
    }

    fn socket(&self) -> stdio::Result<Box<dyn IOSocket>> {
        self.io_loop.socket()
    }

    fn mkdir(&self, c: &mut MkdirCompletion, path: &Path, mode: u32) -> stdio::Result<()> {
        self.io_loop.mkdir(c, path, mode)
    }

    fn backend_name(&self) -> &'static str {
        self.io_loop.backend_name()
    }
}
