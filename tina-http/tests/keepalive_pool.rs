//! Integration tests for the HTTP/1.1 keepalive pool.
//!
//! Drives a hand-rolled scripted server (not [`tina_http::HttpListener`])
//! so the test can count *accepts* — the unambiguous proof that
//! sequential requests reuse one TCP connection. The scripted server
//! supports keepalive and exposes knobs for the stale-close case.

#![allow(dead_code)]

mod common;

use std::convert::Infallible;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use tina::CallContext;
use tina::pool::{
    AcquireOutcome, CloseMode, PoolConfig, PoolLease, PoolPressureReport, ReleaseDisposition,
    ReleaseOutcome,
};
use tina::prelude::*;
use tina_http::{
    HttpClientConfig, HttpLimits, HttpRequest, HttpTarget, KeepaliveConnAddr,
    KeepaliveConnectionMsg, KeepaliveConnectionStopFailure, KeepaliveConnectionStopOutcome,
    KeepaliveOutcome, KeepalivePoolCloseOutcome, KeepalivePoolDrainOutcome, KeepalivePoolHandles,
    OriginKey, TlsTrustRoots, build_keepalive_pool, shutdown_keepalive_pool,
};
use tina_runtime::pool::{WorkerPoolMsg, WorkerPoolReply};
use tina_runtime::{
    CallOutcome, DefaultThreadedMailboxFactory, RuntimeCall, ThreadedRuntime,
    ThreadedRuntimeConfig, call, call_cancelable, cancel_call,
};

use common::TestShard;

// =====================================================================
// Scripted keepalive server
// =====================================================================

#[derive(Debug, Clone, Copy)]
enum ServerPolicy {
    /// Serve as many requests on each accepted connection as the
    /// client cares to send. Closes when the client closes.
    Keepalive,
    /// Serve one request per accept; emit `Connection: close` and
    /// drop the socket after the response.
    CloseAfterFirst,
    /// Accept the connection, serve one request, then close the
    /// socket without writing `Connection: close`. Mimics a server
    /// that decided to close mid-keepalive.
    SilentlyCloseAfterFirst,
    /// Always return one valid chunked body.
    Chunked,
    /// First request returns chunked, later requests return a
    /// content-length response. The client should reconnect after the
    /// chunked response.
    ChunkedThenContentLength,
    /// Return a malformed chunked body.
    MalformedChunked,
    /// Return a chunked body larger than the client body cap.
    LargeChunked,
    /// Return a valid chunked body plus `Connection: close`.
    ChunkedConnectionClose,
    /// Return a valid chunked body followed by bytes shaped like a
    /// second response that no Tina request issued.
    ChunkedSmugglingShape,
    /// Over-declare framing: send MORE body bytes than the declared
    /// `Content-Length` on a reusable connection. The extra bytes are
    /// the leading bytes of a fake next response. The client must treat
    /// this as a framing desync and retire the socket, not pool it.
    ContentLengthOverSend,
}

struct ScriptedServer {
    addr: SocketAddr,
    accepts: Arc<AtomicUsize>,
    requests: Arc<AtomicUsize>,
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
    /// Per-accepted-connection worker threads. Joined on stop/drop
    /// so the test process does not accumulate threads across many
    /// `ScriptedServer` instances.
    workers: Arc<Mutex<Vec<JoinHandle<()>>>>,
}

impl ScriptedServer {
    fn start(policy: ServerPolicy) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind scripted server");
        listener
            .set_nonblocking(false)
            .expect("blocking listener mode");
        let addr = listener.local_addr().expect("local addr");
        let accepts = Arc::new(AtomicUsize::new(0));
        let requests = Arc::new(AtomicUsize::new(0));
        let stop = Arc::new(AtomicBool::new(false));
        // Set a short accept timeout so the server thread polls `stop`
        // and exits cleanly. accept() itself has no timeout, so we use
        // SO_RCVTIMEO via set_nonblocking-and-poll. Simpler: configure
        // the listener as nonblocking and sleep-poll.
        listener
            .set_nonblocking(true)
            .expect("nonblocking listener");
        let workers: Arc<Mutex<Vec<JoinHandle<()>>>> = Arc::new(Mutex::new(Vec::new()));
        let accepts_t = accepts.clone();
        let requests_t = requests.clone();
        let stop_t = stop.clone();
        let workers_t = workers.clone();
        let handle = thread::spawn(move || {
            while !stop_t.load(Ordering::Acquire) {
                match listener.accept() {
                    Ok((stream, _peer)) => {
                        accepts_t.fetch_add(1, Ordering::Relaxed);
                        let req_counter = requests_t.clone();
                        let stop_inner = stop_t.clone();
                        let worker = thread::spawn(move || {
                            handle_connection(stream, policy, req_counter, stop_inner);
                        });
                        workers_t.lock().expect("workers").push(worker);
                    }
                    Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(5));
                    }
                    Err(_) => break,
                }
            }
        });
        Self {
            addr,
            accepts,
            requests,
            stop,
            handle: Some(handle),
            workers,
        }
    }

    fn accepts(&self) -> usize {
        self.accepts.load(Ordering::Relaxed)
    }

    fn requests(&self) -> usize {
        self.requests.load(Ordering::Relaxed)
    }

    fn stop(mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
        self.join_workers();
    }

    fn join_workers(&self) {
        let mut workers = self.workers.lock().expect("workers");
        for worker in workers.drain(..) {
            let _ = worker.join();
        }
    }
}

impl Drop for ScriptedServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
        self.join_workers();
    }
}

fn handle_connection(
    mut stream: TcpStream,
    policy: ServerPolicy,
    requests: Arc<AtomicUsize>,
    stop: Arc<AtomicBool>,
) {
    // Accepted streams may inherit O_NONBLOCK from the listener on
    // some platforms; flip back to blocking so `Read::read` blocks
    // until the next request bytes arrive.
    stream.set_nonblocking(false).expect("blocking stream");
    stream
        .set_read_timeout(Some(Duration::from_secs(3)))
        .expect("read timeout");
    stream
        .set_write_timeout(Some(Duration::from_secs(3)))
        .expect("write timeout");
    let mut buf: Vec<u8> = Vec::with_capacity(4096);
    loop {
        if stop.load(Ordering::Acquire) {
            return;
        }
        let head_end = loop {
            if let Some(pos) = find_double_crlf(&buf) {
                break pos;
            }
            let mut chunk = [0u8; 1024];
            let n = match stream.read(&mut chunk) {
                Ok(0) => return,
                Ok(n) => n,
                Err(_) => return,
            };
            buf.extend_from_slice(&chunk[..n]);
        };
        // Parse content-length from head.
        let head_str = std::str::from_utf8(&buf[..head_end]).unwrap_or("");
        let content_length = parse_content_length(head_str);
        let body_start = head_end;
        let body_end = body_start + content_length;
        while buf.len() < body_end {
            let mut chunk = [0u8; 1024];
            let n = match stream.read(&mut chunk) {
                Ok(0) => return,
                Ok(n) => n,
                Err(_) => return,
            };
            buf.extend_from_slice(&chunk[..n]);
        }
        let request_no = requests.fetch_add(1, Ordering::Relaxed) + 1;

        // Build response.
        let close_after = matches!(policy, ServerPolicy::CloseAfterFirst);
        let mut response = Vec::with_capacity(128);
        response.extend_from_slice(b"HTTP/1.1 200 OK\r\n");
        match policy {
            ServerPolicy::ChunkedThenContentLength if request_no > 1 => {
                response.extend_from_slice(b"Content-Length: 2\r\n\r\nok");
            }
            ServerPolicy::Chunked | ServerPolicy::ChunkedThenContentLength => {
                response.extend_from_slice(b"Transfer-Encoding: chunked\r\n\r\n");
                response.extend_from_slice(b"5\r\nhello\r\n0\r\n\r\n");
            }
            ServerPolicy::MalformedChunked => {
                response.extend_from_slice(b"Transfer-Encoding: chunked\r\n\r\n");
                response.extend_from_slice(b"5\r\nhelloX\r\n0\r\n\r\n");
            }
            ServerPolicy::LargeChunked => {
                response.extend_from_slice(b"Transfer-Encoding: chunked\r\n\r\n");
                response.extend_from_slice(b"20\r\n01234567890123456789012345678901\r\n0\r\n\r\n");
            }
            ServerPolicy::ChunkedConnectionClose => {
                response
                    .extend_from_slice(b"Transfer-Encoding: chunked\r\nConnection: close\r\n\r\n");
                response.extend_from_slice(b"5\r\nhello\r\n0\r\n\r\n");
            }
            ServerPolicy::ChunkedSmugglingShape => {
                response.extend_from_slice(b"Transfer-Encoding: chunked\r\n\r\n");
                response.extend_from_slice(
                    b"5\r\nhello\r\n0\r\n\r\nHTTP/1.1 200 OK\r\nContent-Length: 6\r\n\r\nstale!",
                );
            }
            ServerPolicy::ContentLengthOverSend => {
                // Declares 5 bytes, sends "hello" + extra bytes shaped
                // like a fake next response. The reusable client must
                // detect the over-send and retire the socket.
                response.extend_from_slice(b"Content-Length: 5\r\n\r\n");
                response.extend_from_slice(b"helloHTTP/1.1 200 OK\r\nContent-Length: 6\r\n\r\nstale!");
            }
            _ => {
                let body = b"ok";
                response.extend_from_slice(b"Content-Length: 2\r\n");
                if close_after {
                    response.extend_from_slice(b"Connection: close\r\n");
                }
                response.extend_from_slice(b"\r\n");
                response.extend_from_slice(body);
            }
        }
        if stream.write_all(&response).is_err() {
            return;
        }
        let _ = stream.flush();

        // Drain the consumed request bytes.
        buf.drain(..body_end);

        match policy {
            ServerPolicy::Keepalive | ServerPolicy::ContentLengthOverSend => {}
            ServerPolicy::CloseAfterFirst
            | ServerPolicy::SilentlyCloseAfterFirst
            | ServerPolicy::ChunkedConnectionClose
            | ServerPolicy::ChunkedSmugglingShape => return,
            ServerPolicy::Chunked
            | ServerPolicy::ChunkedThenContentLength
            | ServerPolicy::MalformedChunked
            | ServerPolicy::LargeChunked => {}
        }
    }
}

fn find_double_crlf(buf: &[u8]) -> Option<usize> {
    buf.windows(4).position(|w| w == b"\r\n\r\n").map(|p| p + 4)
}

fn parse_content_length(head: &str) -> usize {
    for line in head.split("\r\n") {
        let lower = line.to_ascii_lowercase();
        if let Some(rest) = lower.strip_prefix("content-length:") {
            if let Ok(n) = rest.trim().parse::<usize>() {
                return n;
            }
        }
    }
    0
}

// =====================================================================
// Driver isolate — exercises the keepalive pool from the runtime side
// =====================================================================

type PoolAddr = Address<WorkerPoolMsg<KeepaliveConnAddr>, WorkerPoolReply<KeepaliveConnAddr>>;

#[derive(Debug, Clone, PartialEq, Eq)]
enum DriverEvent {
    Acquired {
        id: u32,
        generation: u64,
    },
    AcquireFull {
        id: u32,
    },
    AcquireClosed {
        id: u32,
    },
    AcquireCallTimeout {
        id: u32,
    },
    AcquireCallFull {
        id: u32,
    },
    AcquireCallClosed {
        id: u32,
    },
    Request {
        id: u32,
        status: u16,
        body: Vec<u8>,
        must_retire: bool,
    },
    RequestErr {
        id: u32,
        message: String,
    },
    Released {
        id: u32,
        outcome: ReleaseOutcome,
    },
    Stopped {
        id: u32,
    },
    StopFailed {
        id: u32,
        message: String,
    },
    Cancelled,
    PoolClosed,
    Pressure(PoolPressureReport),
}

#[derive(Default)]
struct DriverState {
    events: Vec<DriverEvent>,
    leases: Vec<(u32, PoolLease<KeepaliveConnAddr>)>,
    handles: Vec<(u32, tina::CallHandle<WorkerPoolReply<KeepaliveConnAddr>>)>,
}

enum DriverMsg {
    Acquire {
        id: u32,
        timeout: Duration,
    },
    AcquireWithHandle {
        id: u32,
        timeout: Duration,
    },
    Cancel {
        id: u32,
    },
    Request {
        id: u32,
        request: HttpRequest,
        /// Wall-clock budget the keepalive connection enforces
        /// internally via its `sleep`-armed deadline.
        request_timeout: Duration,
        /// Driver-side `call` timeout. Must be `>= request_timeout`
        /// or the runtime layer will time out before the connection
        /// can surface its typed `Timeout` reply.
        call_timeout: Duration,
    },
    Stop {
        id: u32,
        timeout: Duration,
        /// Stop a connection by its position in the pool's
        /// `connections` Vec instead of by lease id. Used during
        /// shutdown when no lease is held.
        by_index: Option<usize>,
    },
    Release {
        id: u32,
        disposition: ReleaseDisposition,
    },
    Close {
        mode: CloseMode,
        timeout: Duration,
    },
    Pressure {
        timeout: Duration,
    },
    PressureReturned(CallOutcome<WorkerPoolReply<KeepaliveConnAddr>>),
    AcquireReturned {
        id: u32,
        outcome: CallOutcome<WorkerPoolReply<KeepaliveConnAddr>>,
    },
    RequestReturned {
        id: u32,
        outcome: CallOutcome<KeepaliveOutcome>,
    },
    StopReturned {
        id: u32,
        outcome: CallOutcome<KeepaliveOutcome>,
    },
    ReleaseReturned {
        id: u32,
        outcome: CallOutcome<WorkerPoolReply<KeepaliveConnAddr>>,
    },
    CloseReturned(CallOutcome<WorkerPoolReply<KeepaliveConnAddr>>),
    CancelReturned(tina::CancelOutcome),
}

impl std::fmt::Debug for DriverMsg {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Acquire { id, .. } => write!(f, "Acquire({id})"),
            Self::AcquireWithHandle { id, .. } => write!(f, "AcquireWithHandle({id})"),
            Self::Cancel { id } => write!(f, "Cancel({id})"),
            Self::Request { id, .. } => write!(f, "Request({id})"),
            Self::Stop { id, .. } => write!(f, "Stop({id})"),
            Self::Release { id, .. } => write!(f, "Release({id})"),
            Self::Close { .. } => f.write_str("Close"),
            Self::Pressure { .. } => f.write_str("Pressure"),
            Self::PressureReturned(_) => f.write_str("PressureReturned"),
            Self::AcquireReturned { id, .. } => write!(f, "AcquireReturned({id})"),
            Self::RequestReturned { id, .. } => write!(f, "RequestReturned({id})"),
            Self::StopReturned { id, .. } => write!(f, "StopReturned({id})"),
            Self::ReleaseReturned { id, .. } => write!(f, "ReleaseReturned({id})"),
            Self::CloseReturned(_) => f.write_str("CloseReturned"),
            Self::CancelReturned(_) => f.write_str("CancelReturned"),
        }
    }
}

struct Driver {
    pool: PoolAddr,
    /// Snapshot of the pool's per-slot connection addresses, taken
    /// at construction. Tests use this to send `Stop` to a connection
    /// by its slot index after the pool has been closed.
    connections: Vec<KeepaliveConnAddr>,
    state: Arc<Mutex<DriverState>>,
    notify: mpsc::Sender<DriverEvent>,
}

impl Isolate for Driver {
    tina::isolate_types! {
        message: DriverMsg,
        reply: (),
        send: tina::Outbound<Infallible>,
        spawn: Infallible,
        call: RuntimeCall<DriverMsg>,
        shard: TestShard,
    }

    fn handle(
        &mut self,
        msg: DriverMsg,
        _ctx: &mut Context<'_, TestShard, Self::Reply>,
    ) -> Effect<Self> {
        match msg {
            DriverMsg::Acquire { id, timeout } => call(self.pool, WorkerPoolMsg::Acquire, timeout)
                .then(move |outcome| DriverMsg::AcquireReturned { id, outcome }),
            DriverMsg::AcquireWithHandle { id, timeout } => {
                let (effect, handle) = call_cancelable(self.pool, WorkerPoolMsg::Acquire, timeout)
                    .then(move |outcome| DriverMsg::AcquireReturned { id, outcome });
                self.state.lock().expect("state").handles.push((id, handle));
                effect
            }
            DriverMsg::Cancel { id } => {
                let mut st = self.state.lock().expect("state");
                let pos = st.handles.iter().position(|(hid, _)| *hid == id);
                if let Some(pos) = pos {
                    let (_, handle) = st.handles.remove(pos);
                    drop(st);
                    cancel_call(handle).then(DriverMsg::CancelReturned)
                } else {
                    noop()
                }
            }
            DriverMsg::Request {
                id,
                request,
                request_timeout,
                call_timeout,
            } => {
                let leases = self.state.lock().expect("state");
                let addr = *leases
                    .leases
                    .iter()
                    .find(|(lid, _)| *lid == id)
                    .map(|(_, lease)| lease.handle())
                    .expect("lease for id");
                drop(leases);
                call(
                    addr,
                    KeepaliveConnectionMsg::request(request, request_timeout),
                    call_timeout,
                )
                .then(move |outcome| DriverMsg::RequestReturned { id, outcome })
            }
            DriverMsg::Stop {
                id,
                timeout,
                by_index,
            } => {
                let addr = if let Some(idx) = by_index {
                    self.connections[idx]
                } else {
                    let leases = self.state.lock().expect("state");
                    let addr = *leases
                        .leases
                        .iter()
                        .find(|(lid, _)| *lid == id)
                        .map(|(_, lease)| lease.handle())
                        .expect("lease for id");
                    drop(leases);
                    addr
                };
                call(addr, KeepaliveConnectionMsg::Stop, timeout)
                    .then(move |outcome| DriverMsg::StopReturned { id, outcome })
            }
            DriverMsg::Release { id, disposition } => {
                let mut st = self.state.lock().expect("state");
                let pos = st
                    .leases
                    .iter()
                    .position(|(lid, _)| *lid == id)
                    .expect("lease for id");
                let (_, lease) = st.leases.remove(pos);
                drop(st);
                tina_runtime::pool::release_effect(
                    lease,
                    self.pool,
                    disposition,
                    Duration::from_secs(2),
                    move |outcome| DriverMsg::ReleaseReturned { id, outcome },
                )
            }
            DriverMsg::Close { mode, timeout } => {
                call(self.pool, WorkerPoolMsg::Close(mode), timeout).then(DriverMsg::CloseReturned)
            }
            DriverMsg::Pressure { timeout } => {
                call(self.pool, WorkerPoolMsg::PressureReport, timeout)
                    .then(DriverMsg::PressureReturned)
            }
            DriverMsg::PressureReturned(outcome) => {
                let report = match outcome {
                    CallOutcome::Replied(WorkerPoolReply::Pressure(report)) => report,
                    _ => PoolPressureReport::default(),
                };
                let event = DriverEvent::Pressure(report);
                self.state.lock().expect("state").events.push(event.clone());
                let _ = self.notify.send(event);
                noop()
            }

            DriverMsg::AcquireReturned { id, outcome } => {
                let event = match outcome {
                    CallOutcome::Replied(WorkerPoolReply::Acquire(AcquireOutcome::Acquired(
                        lease,
                    ))) => {
                        let generation = lease.generation();
                        self.state.lock().expect("state").leases.push((id, lease));
                        DriverEvent::Acquired { id, generation }
                    }
                    CallOutcome::Replied(WorkerPoolReply::Acquire(AcquireOutcome::Full)) => {
                        DriverEvent::AcquireFull { id }
                    }
                    CallOutcome::Replied(WorkerPoolReply::Acquire(AcquireOutcome::Closed)) => {
                        DriverEvent::AcquireClosed { id }
                    }
                    CallOutcome::Replied(WorkerPoolReply::Acquire(AcquireOutcome::WrongShard)) => {
                        panic!("WrongShard not expected on TestShard")
                    }
                    CallOutcome::Replied(_) => panic!("unexpected reply variant"),
                    CallOutcome::Timeout => DriverEvent::AcquireCallTimeout { id },
                    CallOutcome::Full => DriverEvent::AcquireCallFull { id },
                    CallOutcome::Closed => DriverEvent::AcquireCallClosed { id },
                    CallOutcome::Rejected(_) => DriverEvent::AcquireCallClosed { id },
                };
                self.state.lock().expect("state").events.push(event.clone());
                let _ = self.notify.send(event);
                noop()
            }
            DriverMsg::RequestReturned { id, outcome } => {
                let event = match outcome {
                    CallOutcome::Replied(KeepaliveOutcome::Request {
                        result,
                        must_retire,
                    }) => match result {
                        Ok(response) => DriverEvent::Request {
                            id,
                            status: response.status.as_u16(),
                            body: response.body.as_buffered().unwrap_or(&[]).to_vec(),
                            must_retire,
                        },
                        Err(err) => DriverEvent::RequestErr {
                            id,
                            message: format!("{err:?} (must_retire={must_retire})"),
                        },
                    },
                    CallOutcome::Replied(KeepaliveOutcome::Stopped) => {
                        panic!("Stopped reply on Request call")
                    }
                    other => DriverEvent::RequestErr {
                        id,
                        message: format!("call outcome: {other:?}"),
                    },
                };
                self.state.lock().expect("state").events.push(event.clone());
                let _ = self.notify.send(event);
                noop()
            }
            DriverMsg::StopReturned { id, outcome } => {
                let event = match outcome {
                    CallOutcome::Replied(KeepaliveOutcome::Stopped) => DriverEvent::Stopped { id },
                    other => DriverEvent::StopFailed {
                        id,
                        message: format!("{other:?}"),
                    },
                };
                self.state.lock().expect("state").events.push(event.clone());
                let _ = self.notify.send(event);
                noop()
            }
            DriverMsg::ReleaseReturned { id, outcome } => {
                let event = match outcome {
                    CallOutcome::Replied(WorkerPoolReply::Release(o)) => {
                        DriverEvent::Released { id, outcome: o }
                    }
                    _ => DriverEvent::Released {
                        id,
                        outcome: ReleaseOutcome::StaleLease,
                    },
                };
                self.state.lock().expect("state").events.push(event.clone());
                let _ = self.notify.send(event);
                noop()
            }
            DriverMsg::CloseReturned(_) => {
                self.state
                    .lock()
                    .expect("state")
                    .events
                    .push(DriverEvent::PoolClosed);
                let _ = self.notify.send(DriverEvent::PoolClosed);
                noop()
            }
            DriverMsg::CancelReturned(_) => {
                let _ = self.notify.send(DriverEvent::Cancelled);
                noop()
            }
        }
    }
}

struct RejectingPool;

impl Isolate for RejectingPool {
    tina::isolate_types! {
        message: WorkerPoolMsg<KeepaliveConnAddr>,
        reply: WorkerPoolReply<KeepaliveConnAddr>,
        send: tina::Outbound<Infallible>,
        spawn: Infallible,
        call: RuntimeCall<WorkerPoolMsg<KeepaliveConnAddr>>,
        shard: TestShard,
    }

    fn handle(
        &mut self,
        _msg: WorkerPoolMsg<KeepaliveConnAddr>,
        _ctx: &mut Context<'_, TestShard, Self::Reply>,
    ) -> Effect<Self> {
        noop()
    }

    fn handle_call(
        &mut self,
        _msg: WorkerPoolMsg<KeepaliveConnAddr>,
        call: CallContext<'_, Self>,
    ) -> Effect<Self> {
        call.reject(tina::CallRejectedReason::UnsupportedMessage)
    }
}

// =====================================================================
// Test harness
// =====================================================================

struct TestRig {
    runtime: Option<ThreadedRuntime<TestShard, DefaultThreadedMailboxFactory>>,
}

impl TestRig {
    fn start() -> Self {
        let runtime = ThreadedRuntime::with_config(
            TestShard,
            DefaultThreadedMailboxFactory,
            ThreadedRuntimeConfig {
                command_capacity: 64,
                idle_wait: Duration::from_millis(1),
                ..Default::default()
            },
        );
        Self {
            runtime: Some(runtime),
        }
    }

    fn rt(&self) -> &ThreadedRuntime<TestShard, DefaultThreadedMailboxFactory> {
        self.runtime.as_ref().expect("runtime live")
    }

    fn build_pool(
        &self,
        target: HttpTarget,
        capacity: usize,
        max_waiters: usize,
    ) -> KeepalivePoolHandles {
        self.build_pool_with_config(target, HttpClientConfig::dev(), capacity, max_waiters)
    }

    fn build_pool_with_config(
        &self,
        target: HttpTarget,
        config: HttpClientConfig,
        capacity: usize,
        max_waiters: usize,
    ) -> KeepalivePoolHandles {
        build_keepalive_pool(
            self.rt(),
            target,
            config,
            PoolConfig::new(capacity, max_waiters),
            16,
            32,
        )
        .expect("register pool")
    }

    fn driver(
        &self,
        handles: &KeepalivePoolHandles,
    ) -> (
        Address<DriverMsg>,
        Arc<Mutex<DriverState>>,
        mpsc::Receiver<DriverEvent>,
    ) {
        let state = Arc::new(Mutex::new(DriverState::default()));
        let (tx, rx) = mpsc::channel();
        let driver = Driver {
            pool: handles.pool,
            connections: handles.connections.clone(),
            state: state.clone(),
            notify: tx,
        };
        let address = self
            .rt()
            .register_with_capacity::<Driver, Infallible>(driver, 32)
            .expect("register driver");
        (address, state, rx)
    }

    fn shutdown(mut self) {
        if let Some(rt) = self.runtime.take() {
            let _ = rt.shutdown();
        }
    }
}

fn wait_for_event(
    rx: &mpsc::Receiver<DriverEvent>,
    mut p: impl FnMut(&DriverEvent) -> bool,
) -> DriverEvent {
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    let mut seen: Vec<DriverEvent> = Vec::new();
    loop {
        let remaining = deadline
            .checked_duration_since(std::time::Instant::now())
            .unwrap_or(Duration::from_millis(1));
        match rx.recv_timeout(remaining) {
            Ok(event) => {
                if p(&event) {
                    return event;
                }
                seen.push(event);
            }
            Err(_) => panic!("timed out waiting for event; seen so far: {seen:?}"),
        }
    }
}

fn req() -> HttpRequest {
    HttpRequest::get("/").header("Host", "x").build()
}

// =====================================================================
// Tests
// =====================================================================

#[test]
fn sequential_requests_reuse_one_connection() {
    let server = ScriptedServer::start(ServerPolicy::Keepalive);
    let rig = TestRig::start();
    let pool = rig.build_pool(HttpTarget::http(server.addr), 1, 4);
    let (driver, _state, rx) = rig.driver(&pool);

    for id in 0..3 {
        let _ = rig.rt().try_send(
            driver,
            DriverMsg::Acquire {
                id,
                timeout: Duration::from_secs(2),
            },
        );
        wait_for_event(
            &rx,
            |e| matches!(e, DriverEvent::Acquired { id: i, .. } if *i == id),
        );

        let _ = rig.rt().try_send(
            driver,
            DriverMsg::Request {
                id,
                request: req(),
                request_timeout: Duration::from_secs(2),
                call_timeout: Duration::from_secs(2),
            },
        );
        let event = wait_for_event(
            &rx,
            |e| matches!(e, DriverEvent::Request { id: i, .. } if *i == id),
        );
        match event {
            DriverEvent::Request {
                status,
                must_retire,
                ..
            } => {
                assert_eq!(status, 200);
                assert!(
                    !must_retire,
                    "keepalive server: response should be reusable"
                );
            }
            _ => unreachable!(),
        }

        let _ = rig.rt().try_send(
            driver,
            DriverMsg::Release {
                id,
                disposition: ReleaseDisposition::Reuse,
            },
        );
        wait_for_event(
            &rx,
            |e| matches!(e, DriverEvent::Released { id: i, .. } if *i == id),
        );
    }

    assert_eq!(
        server.requests(),
        3,
        "scripted server should have served 3 requests"
    );
    assert_eq!(
        server.accepts(),
        1,
        "all three requests should ride one TCP connection"
    );

    rig.shutdown();
    server.stop();
}

#[test]
fn keepalive_decodes_chunked_response_and_retires_connection() {
    let server = ScriptedServer::start(ServerPolicy::Chunked);
    let rig = TestRig::start();
    let pool = rig.build_pool(HttpTarget::http(server.addr), 1, 4);
    let (driver, _state, rx) = rig.driver(&pool);

    let _ = rig.rt().try_send(
        driver,
        DriverMsg::Acquire {
            id: 1,
            timeout: Duration::from_secs(2),
        },
    );
    wait_for_event(&rx, |e| matches!(e, DriverEvent::Acquired { id: 1, .. }));
    let _ = rig.rt().try_send(
        driver,
        DriverMsg::Request {
            id: 1,
            request: req(),
            request_timeout: Duration::from_secs(2),
            call_timeout: Duration::from_secs(2),
        },
    );
    let event = wait_for_event(&rx, |e| matches!(e, DriverEvent::Request { id: 1, .. }));
    match event {
        DriverEvent::Request {
            status,
            body,
            must_retire,
            ..
        } => {
            assert_eq!(status, 200);
            assert_eq!(&body, b"hello");
            assert!(must_retire, "chunked first form retires after decoding");
        }
        other => panic!("unexpected event {other:?}"),
    }

    rig.shutdown();
    server.stop();
}

#[test]
fn chunked_then_content_length_requests_do_not_cross_contaminate() {
    let server = ScriptedServer::start(ServerPolicy::ChunkedThenContentLength);
    let rig = TestRig::start();
    let pool = rig.build_pool(HttpTarget::http(server.addr), 1, 4);
    let (driver, _state, rx) = rig.driver(&pool);

    for id in 1..=2 {
        let _ = rig.rt().try_send(
            driver,
            DriverMsg::Acquire {
                id,
                timeout: Duration::from_secs(2),
            },
        );
        wait_for_event(
            &rx,
            |e| matches!(e, DriverEvent::Acquired { id: i, .. } if *i == id),
        );
        let _ = rig.rt().try_send(
            driver,
            DriverMsg::Request {
                id,
                request: req(),
                request_timeout: Duration::from_secs(2),
                call_timeout: Duration::from_secs(2),
            },
        );
        let event = wait_for_event(
            &rx,
            |e| matches!(e, DriverEvent::Request { id: i, .. } if *i == id),
        );
        match event {
            DriverEvent::Request {
                body, must_retire, ..
            } if id == 1 => {
                assert_eq!(&body, b"hello");
                assert!(must_retire);
            }
            DriverEvent::Request {
                body, must_retire, ..
            } => {
                assert_eq!(&body, b"ok");
                assert!(!must_retire);
            }
            other => panic!("unexpected event {other:?}"),
        }
        let _ = rig.rt().try_send(
            driver,
            DriverMsg::Release {
                id,
                disposition: ReleaseDisposition::Reuse,
            },
        );
        wait_for_event(
            &rx,
            |e| matches!(e, DriverEvent::Released { id: i, .. } if *i == id),
        );
    }

    assert_eq!(server.requests(), 2);
    assert_eq!(
        server.accepts(),
        2,
        "chunked response retires so request 2 cannot parse stale bytes on socket 1"
    );

    rig.shutdown();
    server.stop();
}

#[test]
fn malformed_chunked_response_errors_and_retires() {
    let server = ScriptedServer::start(ServerPolicy::MalformedChunked);
    let rig = TestRig::start();
    let pool = rig.build_pool(HttpTarget::http(server.addr), 1, 4);
    let (driver, _state, rx) = rig.driver(&pool);

    let _ = rig.rt().try_send(
        driver,
        DriverMsg::Acquire {
            id: 1,
            timeout: Duration::from_secs(2),
        },
    );
    wait_for_event(&rx, |e| matches!(e, DriverEvent::Acquired { id: 1, .. }));
    let _ = rig.rt().try_send(
        driver,
        DriverMsg::Request {
            id: 1,
            request: req(),
            request_timeout: Duration::from_secs(2),
            call_timeout: Duration::from_secs(2),
        },
    );
    let event = wait_for_event(&rx, |e| matches!(e, DriverEvent::RequestErr { id: 1, .. }));
    match event {
        DriverEvent::RequestErr { message, .. } => {
            assert!(message.contains("MalformedChunkedBody"), "{message}");
            assert!(message.contains("must_retire=true"), "{message}");
        }
        other => panic!("unexpected event {other:?}"),
    }

    rig.shutdown();
    server.stop();
}

#[test]
fn over_cap_chunked_response_errors_and_retires() {
    let server = ScriptedServer::start(ServerPolicy::LargeChunked);
    let rig = TestRig::start();
    let config = HttpClientConfig {
        limits: HttpLimits {
            max_body_bytes: 4,
            ..HttpLimits::default()
        },
        ..HttpClientConfig::dev()
    };
    let pool = rig.build_pool_with_config(HttpTarget::http(server.addr), config, 1, 4);
    let (driver, _state, rx) = rig.driver(&pool);

    let _ = rig.rt().try_send(
        driver,
        DriverMsg::Acquire {
            id: 1,
            timeout: Duration::from_secs(2),
        },
    );
    wait_for_event(&rx, |e| matches!(e, DriverEvent::Acquired { id: 1, .. }));
    let _ = rig.rt().try_send(
        driver,
        DriverMsg::Request {
            id: 1,
            request: req(),
            request_timeout: Duration::from_secs(2),
            call_timeout: Duration::from_secs(2),
        },
    );
    let event = wait_for_event(&rx, |e| matches!(e, DriverEvent::RequestErr { id: 1, .. }));
    match event {
        DriverEvent::RequestErr { message, .. } => {
            assert!(message.contains("BodyTooLarge"), "{message}");
            assert!(message.contains("must_retire=true"), "{message}");
        }
        other => panic!("unexpected event {other:?}"),
    }

    rig.shutdown();
    server.stop();
}

#[test]
fn chunked_connection_close_decodes_and_retires() {
    let server = ScriptedServer::start(ServerPolicy::ChunkedConnectionClose);
    let rig = TestRig::start();
    let pool = rig.build_pool(HttpTarget::http(server.addr), 1, 4);
    let (driver, _state, rx) = rig.driver(&pool);

    let _ = rig.rt().try_send(
        driver,
        DriverMsg::Acquire {
            id: 1,
            timeout: Duration::from_secs(2),
        },
    );
    wait_for_event(&rx, |e| matches!(e, DriverEvent::Acquired { id: 1, .. }));
    let _ = rig.rt().try_send(
        driver,
        DriverMsg::Request {
            id: 1,
            request: req(),
            request_timeout: Duration::from_secs(2),
            call_timeout: Duration::from_secs(2),
        },
    );
    let event = wait_for_event(&rx, |e| matches!(e, DriverEvent::Request { id: 1, .. }));
    match event {
        DriverEvent::Request {
            body, must_retire, ..
        } => {
            assert_eq!(&body, b"hello");
            assert!(must_retire);
        }
        other => panic!("unexpected event {other:?}"),
    }

    rig.shutdown();
    server.stop();
}

#[test]
fn chunked_smuggling_shape_is_retired_before_next_request() {
    let server = ScriptedServer::start(ServerPolicy::ChunkedSmugglingShape);
    let rig = TestRig::start();
    let pool = rig.build_pool(HttpTarget::http(server.addr), 1, 4);
    let (driver, _state, rx) = rig.driver(&pool);

    for id in 1..=2 {
        let _ = rig.rt().try_send(
            driver,
            DriverMsg::Acquire {
                id,
                timeout: Duration::from_secs(2),
            },
        );
        wait_for_event(
            &rx,
            |e| matches!(e, DriverEvent::Acquired { id: i, .. } if *i == id),
        );
        let _ = rig.rt().try_send(
            driver,
            DriverMsg::Request {
                id,
                request: req(),
                request_timeout: Duration::from_secs(2),
                call_timeout: Duration::from_secs(2),
            },
        );
        let event = wait_for_event(
            &rx,
            |e| matches!(e, DriverEvent::Request { id: i, .. } if *i == id),
        );
        match event {
            DriverEvent::Request {
                body, must_retire, ..
            } => {
                assert_eq!(&body, b"hello");
                assert!(must_retire);
            }
            other => panic!("unexpected event {other:?}"),
        }
        let _ = rig.rt().try_send(
            driver,
            DriverMsg::Release {
                id,
                disposition: ReleaseDisposition::Reuse,
            },
        );
        wait_for_event(
            &rx,
            |e| matches!(e, DriverEvent::Released { id: i, .. } if *i == id),
        );
    }

    assert_eq!(server.requests(), 2);
    assert_eq!(
        server.accepts(),
        2,
        "stale fake response bytes after chunk terminator must die with socket 1"
    );

    rig.shutdown();
    server.stop();
}

#[test]
fn content_length_over_send_retires_connection() {
    // Server declares Content-Length: 5 but writes more bytes (the
    // leading bytes of a fake next response). The keepalive client must
    // not silently drop the extra bytes and reuse the desynced socket;
    // it must retire the connection. Proof: request 2 forces a fresh
    // accept rather than parsing the stale fake response on socket 1.
    let server = ScriptedServer::start(ServerPolicy::ContentLengthOverSend);
    let rig = TestRig::start();
    let pool = rig.build_pool(HttpTarget::http(server.addr), 1, 4);
    let (driver, _state, rx) = rig.driver(&pool);

    for id in 1..=2 {
        let _ = rig.rt().try_send(
            driver,
            DriverMsg::Acquire {
                id,
                timeout: Duration::from_secs(2),
            },
        );
        wait_for_event(
            &rx,
            |e| matches!(e, DriverEvent::Acquired { id: i, .. } if *i == id),
        );
        let _ = rig.rt().try_send(
            driver,
            DriverMsg::Request {
                id,
                request: req(),
                request_timeout: Duration::from_secs(2),
                call_timeout: Duration::from_secs(2),
            },
        );
        let event = wait_for_event(
            &rx,
            |e| matches!(e, DriverEvent::Request { id: i, .. } if *i == id),
        );
        match event {
            DriverEvent::Request {
                body, must_retire, ..
            } => {
                assert_eq!(&body, b"hello", "body is exactly the declared length");
                assert!(
                    must_retire,
                    "over-sent body desyncs framing; socket must retire"
                );
            }
            other => panic!("unexpected event {other:?}"),
        }
        let _ = rig.rt().try_send(
            driver,
            DriverMsg::Release {
                id,
                disposition: ReleaseDisposition::Reuse,
            },
        );
        wait_for_event(
            &rx,
            |e| matches!(e, DriverEvent::Released { id: i, .. } if *i == id),
        );
    }

    assert_eq!(server.requests(), 2);
    assert_eq!(
        server.accepts(),
        2,
        "over-sent body must die with socket 1, forcing a reconnect for request 2"
    );

    rig.shutdown();
    server.stop();
}

#[test]
fn different_origin_pools_do_not_share_connections() {
    let server_a = ScriptedServer::start(ServerPolicy::Keepalive);
    let server_b = ScriptedServer::start(ServerPolicy::Keepalive);
    let rig = TestRig::start();
    let pool_a = rig.build_pool(HttpTarget::http(server_a.addr), 1, 4);
    let pool_b = rig.build_pool(HttpTarget::http(server_b.addr), 1, 4);

    let (driver_a, _, rx_a) = rig.driver(&pool_a);
    let (driver_b, _, rx_b) = rig.driver(&pool_b);

    for &(driver, rx) in &[(driver_a, &rx_a), (driver_b, &rx_b)] {
        for id in 0..2 {
            let _ = rig.rt().try_send(
                driver,
                DriverMsg::Acquire {
                    id,
                    timeout: Duration::from_secs(2),
                },
            );
            wait_for_event(rx, |e| matches!(e, DriverEvent::Acquired { .. }));
            let _ = rig.rt().try_send(
                driver,
                DriverMsg::Request {
                    id,
                    request: req(),
                    request_timeout: Duration::from_secs(2),
                    call_timeout: Duration::from_secs(2),
                },
            );
            wait_for_event(rx, |e| matches!(e, DriverEvent::Request { .. }));
            let _ = rig.rt().try_send(
                driver,
                DriverMsg::Release {
                    id,
                    disposition: ReleaseDisposition::Reuse,
                },
            );
            wait_for_event(rx, |e| matches!(e, DriverEvent::Released { .. }));
        }
    }

    assert_eq!(server_a.accepts(), 1, "server A used one TCP connection");
    assert_eq!(server_b.accepts(), 1, "server B used one TCP connection");
    assert_eq!(server_a.requests(), 2);
    assert_eq!(server_b.requests(), 2);

    rig.shutdown();
    server_a.stop();
    server_b.stop();
}

#[test]
fn server_close_after_first_response_marks_must_retire_and_reconnects() {
    let server = ScriptedServer::start(ServerPolicy::CloseAfterFirst);
    let rig = TestRig::start();
    let pool = rig.build_pool(HttpTarget::http(server.addr), 1, 4);
    let (driver, _state, rx) = rig.driver(&pool);

    for id in 0..2 {
        let _ = rig.rt().try_send(
            driver,
            DriverMsg::Acquire {
                id,
                timeout: Duration::from_secs(2),
            },
        );
        wait_for_event(&rx, |e| matches!(e, DriverEvent::Acquired { .. }));
        let _ = rig.rt().try_send(
            driver,
            DriverMsg::Request {
                id,
                request: req(),
                request_timeout: Duration::from_secs(2),
                call_timeout: Duration::from_secs(2),
            },
        );
        let event = wait_for_event(
            &rx,
            |e| matches!(e, DriverEvent::Request { id: i, .. } if *i == id),
        );
        match event {
            DriverEvent::Request {
                status,
                must_retire,
                ..
            } => {
                assert_eq!(status, 200);
                assert!(
                    must_retire,
                    "server sent Connection: close — connection must retire"
                );
            }
            _ => unreachable!(),
        }
        let _ = rig.rt().try_send(
            driver,
            DriverMsg::Release {
                id,
                disposition: ReleaseDisposition::Reuse,
            },
        );
        wait_for_event(&rx, |e| matches!(e, DriverEvent::Released { .. }));
    }

    assert_eq!(server.requests(), 2);
    assert_eq!(
        server.accepts(),
        2,
        "stale-close should force a reconnect for request 2"
    );

    rig.shutdown();
    server.stop();
}

#[test]
fn acquire_full_visible_when_capacity_busy_and_no_waiter_slots() {
    let server = ScriptedServer::start(ServerPolicy::Keepalive);
    let rig = TestRig::start();
    // capacity 1, max_waiters 0 → second Acquire must come back Full
    // immediately.
    let pool = rig.build_pool(HttpTarget::http(server.addr), 1, 0);
    let (driver, _state, rx) = rig.driver(&pool);

    let _ = rig.rt().try_send(
        driver,
        DriverMsg::Acquire {
            id: 1,
            timeout: Duration::from_secs(2),
        },
    );
    wait_for_event(&rx, |e| matches!(e, DriverEvent::Acquired { id: 1, .. }));

    let _ = rig.rt().try_send(
        driver,
        DriverMsg::Acquire {
            id: 2,
            timeout: Duration::from_secs(2),
        },
    );
    let event = wait_for_event(&rx, |e| {
        matches!(
            e,
            DriverEvent::AcquireFull { id: 2 } | DriverEvent::Acquired { id: 2, .. }
        )
    });
    assert!(
        matches!(event, DriverEvent::AcquireFull { id: 2 }),
        "expected AcquireFull, got {event:?}"
    );

    rig.shutdown();
    server.stop();
}

#[test]
fn acquire_call_timeout_reclaims_waiter_capacity() {
    let server = ScriptedServer::start(ServerPolicy::Keepalive);
    let rig = TestRig::start();
    let pool = rig.build_pool(HttpTarget::http(server.addr), 1, 1);
    let (driver, _state, rx) = rig.driver(&pool);

    // Holder takes the only resource and never releases.
    let _ = rig.rt().try_send(
        driver,
        DriverMsg::Acquire {
            id: 1,
            timeout: Duration::from_secs(2),
        },
    );
    wait_for_event(&rx, |e| matches!(e, DriverEvent::Acquired { id: 1, .. }));

    // Waiter parks then call-times-out.
    let _ = rig.rt().try_send(
        driver,
        DriverMsg::Acquire {
            id: 2,
            timeout: Duration::from_millis(150),
        },
    );
    let event = wait_for_event(&rx, |e| {
        matches!(e, DriverEvent::AcquireCallTimeout { id: 2 })
    });
    assert!(matches!(event, DriverEvent::AcquireCallTimeout { id: 2 }));

    // Third waiter must succeed in parking — capacity reclaimed.
    let _ = rig.rt().try_send(
        driver,
        DriverMsg::Acquire {
            id: 3,
            timeout: Duration::from_millis(100),
        },
    );
    let event = wait_for_event(&rx, |e| {
        matches!(e, DriverEvent::AcquireCallTimeout { id: 3 })
    });
    assert!(
        matches!(event, DriverEvent::AcquireCallTimeout { id: 3 }),
        "third waiter parked then timed out, proving the second waiter's slot was reclaimed"
    );

    rig.shutdown();
    server.stop();
}

#[test]
fn acquire_cancel_reclaims_waiter_capacity() {
    let server = ScriptedServer::start(ServerPolicy::Keepalive);
    let rig = TestRig::start();
    let pool = rig.build_pool(HttpTarget::http(server.addr), 1, 1);
    let (driver, _state, rx) = rig.driver(&pool);

    // Holder.
    let _ = rig.rt().try_send(
        driver,
        DriverMsg::Acquire {
            id: 1,
            timeout: Duration::from_secs(2),
        },
    );
    wait_for_event(&rx, |e| matches!(e, DriverEvent::Acquired { id: 1, .. }));

    // Waiter (with cancel handle).
    let _ = rig.rt().try_send(
        driver,
        DriverMsg::AcquireWithHandle {
            id: 2,
            timeout: Duration::from_secs(5),
        },
    );
    // Give the pool a moment to enqueue the waiter.
    thread::sleep(Duration::from_millis(50));

    let _ = rig.rt().try_send(driver, DriverMsg::Cancel { id: 2 });
    wait_for_event(&rx, |e| matches!(e, DriverEvent::Cancelled));

    // Third waiter parks fine — capacity reclaimed.
    let _ = rig.rt().try_send(
        driver,
        DriverMsg::Acquire {
            id: 3,
            timeout: Duration::from_millis(100),
        },
    );
    let event = wait_for_event(&rx, |e| {
        matches!(e, DriverEvent::AcquireCallTimeout { id: 3 })
    });
    assert!(matches!(event, DriverEvent::AcquireCallTimeout { id: 3 }));

    rig.shutdown();
    server.stop();
}

#[test]
fn drain_close_blocks_new_acquires_and_lets_outstanding_release() {
    let server = ScriptedServer::start(ServerPolicy::Keepalive);
    let rig = TestRig::start();
    let pool = rig.build_pool(HttpTarget::http(server.addr), 1, 4);
    let (driver, _state, rx) = rig.driver(&pool);

    let _ = rig.rt().try_send(
        driver,
        DriverMsg::Acquire {
            id: 1,
            timeout: Duration::from_secs(2),
        },
    );
    wait_for_event(&rx, |e| matches!(e, DriverEvent::Acquired { id: 1, .. }));

    // Drain.
    let _ = rig.rt().try_send(
        driver,
        DriverMsg::Close {
            mode: CloseMode::Drain,
            timeout: Duration::from_secs(2),
        },
    );
    wait_for_event(&rx, |e| matches!(e, DriverEvent::PoolClosed));

    // Outstanding release: drain accepts as Released; resource sits idle but
    // the pool will not re-hand it out.
    let _ = rig.rt().try_send(
        driver,
        DriverMsg::Release {
            id: 1,
            disposition: ReleaseDisposition::Reuse,
        },
    );
    let event = wait_for_event(&rx, |e| matches!(e, DriverEvent::Released { id: 1, .. }));
    if let DriverEvent::Released { outcome, .. } = event {
        assert_eq!(outcome, ReleaseOutcome::Released);
    }

    // New acquire should reply Closed.
    let _ = rig.rt().try_send(
        driver,
        DriverMsg::Acquire {
            id: 2,
            timeout: Duration::from_secs(1),
        },
    );
    let event = wait_for_event(&rx, |e| matches!(e, DriverEvent::AcquireClosed { id: 2 }));
    assert!(matches!(event, DriverEvent::AcquireClosed { id: 2 }));

    rig.shutdown();
    server.stop();
}

#[test]
fn force_close_marks_outstanding_lease_stale_on_release() {
    let server = ScriptedServer::start(ServerPolicy::Keepalive);
    let rig = TestRig::start();
    let pool = rig.build_pool(HttpTarget::http(server.addr), 1, 4);
    let (driver, _state, rx) = rig.driver(&pool);

    let _ = rig.rt().try_send(
        driver,
        DriverMsg::Acquire {
            id: 1,
            timeout: Duration::from_secs(2),
        },
    );
    wait_for_event(&rx, |e| matches!(e, DriverEvent::Acquired { id: 1, .. }));

    let _ = rig.rt().try_send(
        driver,
        DriverMsg::Close {
            mode: CloseMode::Force,
            timeout: Duration::from_secs(2),
        },
    );
    wait_for_event(&rx, |e| matches!(e, DriverEvent::PoolClosed));

    // Late release of the outstanding lease — Force says PoolClosed.
    let _ = rig.rt().try_send(
        driver,
        DriverMsg::Release {
            id: 1,
            disposition: ReleaseDisposition::Reuse,
        },
    );
    let event = wait_for_event(&rx, |e| matches!(e, DriverEvent::Released { id: 1, .. }));
    if let DriverEvent::Released { outcome, .. } = event {
        assert_eq!(outcome, ReleaseOutcome::PoolClosed);
    }

    rig.shutdown();
    server.stop();
}

#[test]
fn https_origin_key_includes_sni_and_trust_identity() {
    // We don't run HTTPS traffic here — that's covered elsewhere. We
    // just prove the key fingerprint changes when SNI or trust roots
    // change. The rest is a structural assertion.
    let addr: SocketAddr = "127.0.0.1:443".parse().unwrap();
    let target_a = HttpTarget::https(
        addr,
        "alpha.example",
        TlsTrustRoots::from_der(vec![vec![1, 2, 3]]),
    );
    let target_b = HttpTarget::https(
        addr,
        "beta.example",
        TlsTrustRoots::from_der(vec![vec![1, 2, 3]]),
    );
    let target_c = HttpTarget::https(
        addr,
        "alpha.example",
        TlsTrustRoots::from_der(vec![vec![9, 9, 9]]),
    );

    let key_a = OriginKey::from_target(&target_a);
    let key_b = OriginKey::from_target(&target_b);
    let key_c = OriginKey::from_target(&target_c);

    assert_ne!(key_a, key_b, "different SNI must produce a different key");
    assert_ne!(
        key_a, key_c,
        "different trust roots must produce a different key"
    );

    // HTTP keys differ from HTTPS keys at the same address.
    let http_target = HttpTarget::http(addr);
    let http_key = OriginKey::from_target(&http_target);
    assert_ne!(http_key, key_a);
}

/// Black-hole TCP server: accepts connections, never reads or writes,
/// holds them open until the test drops it. Used to drive the keepalive
/// connection's request_timeout path without ambiguity.
struct BlackHole {
    addr: SocketAddr,
    stop: Arc<AtomicBool>,
    holders: Arc<Mutex<Vec<TcpStream>>>,
    handle: Option<JoinHandle<()>>,
}

impl BlackHole {
    fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind black hole");
        let addr = listener.local_addr().expect("local addr");
        listener.set_nonblocking(true).expect("nonblocking");
        let stop = Arc::new(AtomicBool::new(false));
        let holders = Arc::new(Mutex::new(Vec::new()));
        let stop_t = stop.clone();
        let holders_t = holders.clone();
        let handle = thread::spawn(move || {
            while !stop_t.load(Ordering::Acquire) {
                match listener.accept() {
                    Ok((stream, _)) => {
                        let _ = stream.set_nonblocking(false);
                        holders_t.lock().expect("holders").push(stream);
                    }
                    Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(5));
                    }
                    Err(_) => break,
                }
            }
        });
        Self {
            addr,
            stop,
            holders,
            handle: Some(handle),
        }
    }
}

impl Drop for BlackHole {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        self.holders.lock().expect("holders").clear();
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

#[test]
fn request_timeout_marks_must_retire_then_release_reuse_keeps_pool_capacity() {
    // The recommended consumer pattern is always-Reuse on must_retire.
    // The connection isolate self-heals (drops the bad transport,
    // reconnects on next request); releasing Retire would permanently
    // drop the pool slot. Prove it: time out a request, release Reuse,
    // pool's `available` returns to 1.
    let server = BlackHole::start();
    let rig = TestRig::start();
    let pool = rig.build_pool(HttpTarget::http(server.addr), 1, 4);
    let (driver, _state, rx) = rig.driver(&pool);

    let _ = rig.rt().try_send(
        driver,
        DriverMsg::Acquire {
            id: 1,
            timeout: Duration::from_secs(2),
        },
    );
    wait_for_event(&rx, |e| matches!(e, DriverEvent::Acquired { id: 1, .. }));

    let _ = rig.rt().try_send(
        driver,
        DriverMsg::Request {
            id: 1,
            request: req(),
            request_timeout: Duration::from_millis(150),
            call_timeout: Duration::from_secs(2),
        },
    );
    let event = wait_for_event(&rx, |e| {
        matches!(
            e,
            DriverEvent::Request { id: 1, .. } | DriverEvent::RequestErr { id: 1, .. }
        )
    });
    match event {
        DriverEvent::RequestErr { message, .. } => {
            assert!(
                message.contains("Timeout") && message.contains("must_retire=true"),
                "expected Timeout with must_retire=true; got {message}"
            );
        }
        other => panic!("expected RequestErr (Timeout), got {other:?}"),
    }

    // Recommended: release Reuse. Pool slot stays alive.
    let _ = rig.rt().try_send(
        driver,
        DriverMsg::Release {
            id: 1,
            disposition: ReleaseDisposition::Reuse,
        },
    );
    let event = wait_for_event(&rx, |e| matches!(e, DriverEvent::Released { id: 1, .. }));
    if let DriverEvent::Released { outcome, .. } = event {
        assert_eq!(outcome, ReleaseOutcome::Released);
    }

    // Snapshot: pool reports the slot as available again, ready for
    // the next caller (which will reconnect on first Request).
    let _ = rig.rt().try_send(
        driver,
        DriverMsg::Pressure {
            timeout: Duration::from_secs(1),
        },
    );
    let event = wait_for_event(&rx, |e| matches!(e, DriverEvent::Pressure(_)));
    if let DriverEvent::Pressure(report) = event {
        assert_eq!(report.capacity, 1);
        assert_eq!(report.available, 1, "always-Reuse keeps capacity stable");
        assert_eq!(report.leased, 0);
        assert!(!report.closed);
    }

    rig.shutdown();
}

#[test]
fn explicit_retire_drops_slot_permanently() {
    // The explicit-retire path is deliberately rare — for the case
    // where the consumer believes the connection isolate itself is
    // suspect (panicked, observed protocol violation). Prove the
    // pool honors Retire by removing the slot from circulation.
    let server = ScriptedServer::start(ServerPolicy::Keepalive);
    let rig = TestRig::start();
    let pool = rig.build_pool(HttpTarget::http(server.addr), 2, 4);
    let (driver, _state, rx) = rig.driver(&pool);

    // Acquire one of the two slots.
    let _ = rig.rt().try_send(
        driver,
        DriverMsg::Acquire {
            id: 1,
            timeout: Duration::from_secs(2),
        },
    );
    wait_for_event(&rx, |e| matches!(e, DriverEvent::Acquired { id: 1, .. }));

    // Release Retire — slot is dropped; capacity goes from 2 to 1.
    let _ = rig.rt().try_send(
        driver,
        DriverMsg::Release {
            id: 1,
            disposition: ReleaseDisposition::Retire,
        },
    );
    let event = wait_for_event(&rx, |e| matches!(e, DriverEvent::Released { id: 1, .. }));
    if let DriverEvent::Released { outcome, .. } = event {
        assert_eq!(outcome, ReleaseOutcome::Retired);
    }

    // Pressure: available drops, retired_count incremented.
    let _ = rig.rt().try_send(
        driver,
        DriverMsg::Pressure {
            timeout: Duration::from_secs(1),
        },
    );
    let event = wait_for_event(&rx, |e| matches!(e, DriverEvent::Pressure(_)));
    if let DriverEvent::Pressure(report) = event {
        assert_eq!(report.capacity, 2);
        assert_eq!(report.available, 1, "one slot retired, one remains");
        assert!(report.retired_count >= 1);
    }

    rig.shutdown();
    server.stop();
}

#[test]
fn pressure_report_surfaces_capacity_full_count_and_close_state() {
    let server = ScriptedServer::start(ServerPolicy::Keepalive);
    let rig = TestRig::start();
    let pool = rig.build_pool(HttpTarget::http(server.addr), 1, 0);
    let (driver, _state, rx) = rig.driver(&pool);

    // Initial snapshot: capacity 1, available 1, no Full count yet.
    let _ = rig.rt().try_send(
        driver,
        DriverMsg::Pressure {
            timeout: Duration::from_secs(1),
        },
    );
    let event = wait_for_event(&rx, |e| matches!(e, DriverEvent::Pressure(_)));
    if let DriverEvent::Pressure(report) = event {
        assert_eq!(report.capacity, 1);
        assert_eq!(report.available, 1);
        assert_eq!(report.leased, 0);
        assert_eq!(report.full_count, 0);
        assert!(!report.closed);
    }

    // Consume the only slot, then provoke a Full and snapshot again.
    let _ = rig.rt().try_send(
        driver,
        DriverMsg::Acquire {
            id: 1,
            timeout: Duration::from_secs(1),
        },
    );
    wait_for_event(&rx, |e| matches!(e, DriverEvent::Acquired { id: 1, .. }));
    let _ = rig.rt().try_send(
        driver,
        DriverMsg::Acquire {
            id: 2,
            timeout: Duration::from_secs(1),
        },
    );
    wait_for_event(&rx, |e| matches!(e, DriverEvent::AcquireFull { id: 2 }));

    let _ = rig.rt().try_send(
        driver,
        DriverMsg::Pressure {
            timeout: Duration::from_secs(1),
        },
    );
    let event = wait_for_event(&rx, |e| matches!(e, DriverEvent::Pressure(_)));
    if let DriverEvent::Pressure(report) = event {
        assert_eq!(report.leased, 1);
        assert_eq!(report.available, 0);
        assert!(report.full_count >= 1, "Full was emitted");
        assert!(!report.closed);
    }

    // Close (Drain) and snapshot once more.
    let _ = rig.rt().try_send(
        driver,
        DriverMsg::Close {
            mode: CloseMode::Drain,
            timeout: Duration::from_secs(1),
        },
    );
    wait_for_event(&rx, |e| matches!(e, DriverEvent::PoolClosed));

    let _ = rig.rt().try_send(
        driver,
        DriverMsg::Pressure {
            timeout: Duration::from_secs(1),
        },
    );
    let event = wait_for_event(&rx, |e| matches!(e, DriverEvent::Pressure(_)));
    if let DriverEvent::Pressure(report) = event {
        assert!(report.closed, "pool should report closed=true after Drain");
    }

    rig.shutdown();
    server.stop();
}

// =====================================================================
// Multi-slot tests
// =====================================================================

#[test]
fn capacity_two_pool_serves_two_concurrent_acquires_with_two_connections() {
    // Capacity 2 → two distinct connection isolates → two TCP
    // accepts on the server. Hold both leases concurrently to prove
    // the pool isn't secretly serialising. Pressure mid-flight shows
    // leased=2 / available=0; after release, available=2.
    let server = ScriptedServer::start(ServerPolicy::Keepalive);
    let rig = TestRig::start();
    let pool = rig.build_pool(HttpTarget::http(server.addr), 2, 4);
    let (driver, _state, rx) = rig.driver(&pool);

    for id in [1u32, 2u32] {
        let _ = rig.rt().try_send(
            driver,
            DriverMsg::Acquire {
                id,
                timeout: Duration::from_secs(2),
            },
        );
        wait_for_event(
            &rx,
            |e| matches!(e, DriverEvent::Acquired { id: i, .. } if *i == id),
        );
    }

    // Both leases held concurrently. Issue a request on each so we
    // get a TCP accept on the server for each connection.
    for id in [1u32, 2u32] {
        let _ = rig.rt().try_send(
            driver,
            DriverMsg::Request {
                id,
                request: req(),
                request_timeout: Duration::from_secs(2),
                call_timeout: Duration::from_secs(2),
            },
        );
        wait_for_event(
            &rx,
            |e| matches!(e, DriverEvent::Request { id: i, .. } if *i == id),
        );
    }

    // Mid-flight pressure (still holding both leases): 2 leased.
    let _ = rig.rt().try_send(
        driver,
        DriverMsg::Pressure {
            timeout: Duration::from_secs(1),
        },
    );
    let event = wait_for_event(&rx, |e| matches!(e, DriverEvent::Pressure(_)));
    if let DriverEvent::Pressure(report) = event {
        assert_eq!(report.capacity, 2);
        assert_eq!(report.leased, 2, "two leases concurrent");
        assert_eq!(report.available, 0);
    }

    // Release both. After-pressure: available=2.
    for id in [1u32, 2u32] {
        let _ = rig.rt().try_send(
            driver,
            DriverMsg::Release {
                id,
                disposition: ReleaseDisposition::Reuse,
            },
        );
        wait_for_event(
            &rx,
            |e| matches!(e, DriverEvent::Released { id: i, .. } if *i == id),
        );
    }

    let _ = rig.rt().try_send(
        driver,
        DriverMsg::Pressure {
            timeout: Duration::from_secs(1),
        },
    );
    let event = wait_for_event(&rx, |e| matches!(e, DriverEvent::Pressure(_)));
    if let DriverEvent::Pressure(report) = event {
        assert_eq!(report.available, 2);
        assert_eq!(report.leased, 0);
    }

    assert_eq!(
        server.accepts(),
        2,
        "capacity 2 with concurrent leases used two TCP connections"
    );
    assert_eq!(server.requests(), 2);

    rig.shutdown();
    server.stop();
}

// =====================================================================
// Request-timeout scaling — proves request_timeout actually controls
// =====================================================================

#[test]
fn request_timeout_correlates_with_wall_clock() {
    // Two runs against the black hole, with request_timeout 100ms
    // and 500ms. Wall-clock duration of the failure must scale with
    // the configured timeout (within a generous tolerance for CI
    // jitter). Without this test, a hardcoded internal timeout in
    // KeepaliveConnection would still pass the existing must_retire
    // assertion.
    fn measure(timeout_ms: u64) -> Duration {
        let server = BlackHole::start();
        let rig = TestRig::start();
        let pool = rig.build_pool(HttpTarget::http(server.addr), 1, 4);
        let (driver, _state, rx) = rig.driver(&pool);

        let _ = rig.rt().try_send(
            driver,
            DriverMsg::Acquire {
                id: 1,
                timeout: Duration::from_secs(2),
            },
        );
        wait_for_event(&rx, |e| matches!(e, DriverEvent::Acquired { id: 1, .. }));

        let started = std::time::Instant::now();
        let _ = rig.rt().try_send(
            driver,
            DriverMsg::Request {
                id: 1,
                request: req(),
                request_timeout: Duration::from_millis(timeout_ms),
                call_timeout: Duration::from_secs(5),
            },
        );
        wait_for_event(&rx, |e| matches!(e, DriverEvent::RequestErr { id: 1, .. }));
        let elapsed = started.elapsed();

        rig.shutdown();
        elapsed
    }

    let short = measure(100);
    let long = measure(500);

    // Short run completes well under the long timeout.
    assert!(
        short < Duration::from_millis(400),
        "short run took {short:?}; expected < 400ms (request_timeout was 100ms)"
    );
    // Long run takes at least most of its timeout.
    assert!(
        long >= Duration::from_millis(400),
        "long run took {long:?}; expected >= 400ms (request_timeout was 500ms)"
    );
    // Long is meaningfully longer than short — proves the variable controls.
    assert!(
        long > short + Duration::from_millis(200),
        "long ({long:?}) should exceed short ({short:?}) by >200ms"
    );
}

// =====================================================================
// Drain-with-parked-waiters — extends the basic drain test
// =====================================================================

#[test]
fn drain_with_parked_waiters_settles_waiters_as_closed() {
    // Capacity 1, two waiters parked behind an in-flight lease.
    // Drain. Both waiters should settle as Closed; the lease holder's
    // release lands as Released, but the slot is not re-handed out.
    let server = ScriptedServer::start(ServerPolicy::Keepalive);
    let rig = TestRig::start();
    let pool = rig.build_pool(HttpTarget::http(server.addr), 1, 4);
    let (driver, _state, rx) = rig.driver(&pool);

    let _ = rig.rt().try_send(
        driver,
        DriverMsg::Acquire {
            id: 1,
            timeout: Duration::from_secs(2),
        },
    );
    wait_for_event(&rx, |e| matches!(e, DriverEvent::Acquired { id: 1, .. }));

    // Park two waiters.
    for id in [2u32, 3u32] {
        let _ = rig.rt().try_send(
            driver,
            DriverMsg::Acquire {
                id,
                timeout: Duration::from_secs(5),
            },
        );
    }
    // Give waiters a moment to enqueue.
    thread::sleep(Duration::from_millis(50));

    // Drain.
    let _ = rig.rt().try_send(
        driver,
        DriverMsg::Close {
            mode: CloseMode::Drain,
            timeout: Duration::from_secs(2),
        },
    );

    // Both waiters and the close ack should arrive. Order isn't
    // strictly defined; collect until we have all three signals.
    let mut closed_ids: Vec<u32> = Vec::new();
    let mut pool_closed_seen = false;
    while !(pool_closed_seen && closed_ids.len() == 2) {
        let event = wait_for_event(&rx, |e| {
            matches!(
                e,
                DriverEvent::AcquireClosed { id: 2 }
                    | DriverEvent::AcquireClosed { id: 3 }
                    | DriverEvent::PoolClosed
            )
        });
        match event {
            DriverEvent::AcquireClosed { id } => closed_ids.push(id),
            DriverEvent::PoolClosed => pool_closed_seen = true,
            _ => unreachable!(),
        }
    }
    closed_ids.sort();
    assert_eq!(closed_ids, vec![2, 3], "both waiters got Closed");

    // Holder releases — pool accepts but does not re-hand-out (no waiters left anyway).
    let _ = rig.rt().try_send(
        driver,
        DriverMsg::Release {
            id: 1,
            disposition: ReleaseDisposition::Reuse,
        },
    );
    let event = wait_for_event(&rx, |e| matches!(e, DriverEvent::Released { id: 1, .. }));
    if let DriverEvent::Released { outcome, .. } = event {
        assert_eq!(outcome, ReleaseOutcome::Released);
    }

    rig.shutdown();
    server.stop();
}

// =====================================================================
// Stop integration — proves shutdown actually closes transports
// =====================================================================

#[test]
fn stop_accepts_call_and_reports_stopped_to_host_caller() {
    let server = ScriptedServer::start(ServerPolicy::Keepalive);
    let rig = TestRig::start();
    let pool = rig.build_pool(HttpTarget::http(server.addr), 1, 4);

    match rig
        .rt()
        .call_blocking(
            pool.connections[0],
            KeepaliveConnectionMsg::Stop,
            Duration::from_secs(2),
        )
        .expect("host stop call")
    {
        CallOutcome::Replied(KeepaliveOutcome::Stopped) => {}
        other => panic!("expected Stop to reply Stopped, got {other:?}"),
    }

    match rig
        .rt()
        .call_blocking(
            pool.connections[0],
            KeepaliveConnectionMsg::request(req(), Duration::from_secs(2)),
            Duration::from_secs(2),
        )
        .expect("host request after stop call")
    {
        CallOutcome::Closed => {}
        other => panic!("expected stopped connection address to be closed, got {other:?}"),
    }

    rig.shutdown();
    server.stop();
}

#[test]
fn shutdown_helper_reports_pool_close_and_checked_connection_stops() {
    let server = ScriptedServer::start(ServerPolicy::Keepalive);
    let rig = TestRig::start();
    let pool = rig.build_pool(HttpTarget::http(server.addr), 2, 4);

    let report = shutdown_keepalive_pool(rig.rt(), &pool, CloseMode::Drain, Duration::from_secs(2))
        .expect("shutdown keepalive pool");

    assert_eq!(report.pool_close, KeepalivePoolCloseOutcome::Closed);
    assert_eq!(report.drain, KeepalivePoolDrainOutcome::Drained);
    assert_eq!(report.requested, 2);
    assert_eq!(report.stopped, 2);
    assert_eq!(report.timed_out, 0);
    assert_eq!(report.rejected, 0);
    assert_eq!(report.already_closed, 0);
    assert_eq!(report.connection_failures, Vec::new());

    match rig
        .rt()
        .call_blocking(pool.pool, WorkerPoolMsg::Acquire, Duration::from_secs(2))
        .expect("acquire after helper shutdown")
    {
        CallOutcome::Replied(WorkerPoolReply::Acquire(AcquireOutcome::Closed)) => {}
        other => panic!("expected closed acquire after helper shutdown, got {other:?}"),
    }

    match rig
        .rt()
        .call_blocking(
            pool.pool,
            WorkerPoolMsg::PressureReport,
            Duration::from_secs(2),
        )
        .expect("pressure after helper shutdown")
    {
        CallOutcome::Replied(WorkerPoolReply::Pressure(report)) => {
            assert_eq!(report.capacity, 2);
            assert!(report.closed, "pool pressure must expose closed admission");
        }
        other => panic!("expected pressure report after helper shutdown, got {other:?}"),
    }

    for conn in pool.connections {
        match rig
            .rt()
            .call_blocking(
                conn,
                KeepaliveConnectionMsg::request(req(), Duration::from_secs(2)),
                Duration::from_secs(2),
            )
            .expect("request after helper shutdown")
        {
            CallOutcome::Closed => {}
            other => panic!("expected stopped connection address to be closed, got {other:?}"),
        }
    }

    rig.shutdown();
    server.stop();
}

#[test]
fn drain_shutdown_helper_does_not_stop_leased_connection_on_timeout() {
    let server = ScriptedServer::start(ServerPolicy::Keepalive);
    let rig = TestRig::start();
    let pool = rig.build_pool(HttpTarget::http(server.addr), 1, 4);

    let lease = match rig
        .rt()
        .call_blocking(pool.pool, WorkerPoolMsg::Acquire, Duration::from_secs(2))
        .expect("acquire lease")
    {
        CallOutcome::Replied(WorkerPoolReply::Acquire(AcquireOutcome::Acquired(lease))) => lease,
        other => panic!("expected acquired lease, got {other:?}"),
    };
    let conn = *lease.handle();

    let report = shutdown_keepalive_pool(
        rig.rt(),
        &pool,
        CloseMode::Drain,
        Duration::from_millis(500),
    )
    .expect("drain shutdown with outstanding lease");
    assert_eq!(report.pool_close, KeepalivePoolCloseOutcome::Closed);
    assert_eq!(
        report.drain,
        KeepalivePoolDrainOutcome::TimedOut { leased: 1 }
    );
    assert_eq!(report.requested, 0);
    assert_eq!(report.stopped, 0);
    assert!(report.connection_failures.is_empty());

    match rig
        .rt()
        .call_blocking(
            conn,
            KeepaliveConnectionMsg::request(req(), Duration::from_secs(2)),
            Duration::from_secs(2),
        )
        .expect("leased request after drain timeout")
    {
        CallOutcome::Replied(KeepaliveOutcome::Request { result, .. }) => {
            assert!(result.is_ok(), "leased connection must not be stopped");
        }
        other => panic!("expected leased connection to remain usable, got {other:?}"),
    }

    match rig
        .rt()
        .call_blocking(
            pool.pool,
            WorkerPoolMsg::Release {
                lease,
                disposition: ReleaseDisposition::Reuse,
            },
            Duration::from_secs(2),
        )
        .expect("release outstanding lease")
    {
        CallOutcome::Replied(WorkerPoolReply::Release(ReleaseOutcome::Released)) => {}
        other => panic!("expected released lease, got {other:?}"),
    }

    let cleanup =
        shutdown_keepalive_pool(rig.rt(), &pool, CloseMode::Drain, Duration::from_secs(2))
            .expect("cleanup shutdown");
    assert_eq!(cleanup.drain, KeepalivePoolDrainOutcome::Drained);
    assert_eq!(cleanup.requested, 1);
    assert_eq!(cleanup.stopped, 1);

    rig.shutdown();
    server.stop();
}

#[test]
fn shutdown_helper_does_not_stop_connections_when_pool_close_rejected() {
    let server = ScriptedServer::start(ServerPolicy::Keepalive);
    let rig = TestRig::start();
    let pool = rig.build_pool(HttpTarget::http(server.addr), 1, 4);
    let rejecting_pool = rig
        .rt()
        .register_with_capacity::<RejectingPool, Infallible>(RejectingPool, 4)
        .expect("register rejecting pool");
    let handles = KeepalivePoolHandles {
        pool: rejecting_pool,
        connections: pool.connections.clone(),
    };

    let report =
        shutdown_keepalive_pool(rig.rt(), &handles, CloseMode::Force, Duration::from_secs(2))
            .expect("shutdown with rejecting pool");
    assert_eq!(
        report.pool_close,
        KeepalivePoolCloseOutcome::Rejected(tina::CallRejectedReason::UnsupportedMessage)
    );
    assert_eq!(
        report.drain,
        KeepalivePoolDrainOutcome::SkippedAdmissionNotClosed
    );
    assert_eq!(report.requested, 0);
    assert_eq!(report.stopped, 0);
    assert!(report.connection_failures.is_empty());

    match rig
        .rt()
        .call_blocking(
            pool.connections[0],
            KeepaliveConnectionMsg::request(req(), Duration::from_secs(2)),
            Duration::from_secs(2),
        )
        .expect("request after rejected pool close")
    {
        CallOutcome::Replied(KeepaliveOutcome::Request { result, .. }) => {
            assert!(result.is_ok(), "connection must not be stopped");
        }
        other => panic!("expected connection to remain usable, got {other:?}"),
    }

    let cleanup =
        shutdown_keepalive_pool(rig.rt(), &pool, CloseMode::Drain, Duration::from_secs(2))
            .expect("cleanup shutdown");
    assert_eq!(cleanup.drain, KeepalivePoolDrainOutcome::Drained);
    assert_eq!(cleanup.stopped, 1);

    rig.shutdown();
    server.stop();
}

#[test]
fn repeated_shutdown_helper_names_already_closed_connections_by_slot() {
    let server = ScriptedServer::start(ServerPolicy::Keepalive);
    let rig = TestRig::start();
    let pool = rig.build_pool(HttpTarget::http(server.addr), 1, 4);

    let first = shutdown_keepalive_pool(rig.rt(), &pool, CloseMode::Drain, Duration::from_secs(2))
        .expect("first shutdown keepalive pool");
    assert_eq!(first.stopped, 1);
    assert!(first.connection_failures.is_empty());

    let second = shutdown_keepalive_pool(rig.rt(), &pool, CloseMode::Drain, Duration::from_secs(2))
        .expect("second shutdown keepalive pool");
    assert_eq!(second.pool_close, KeepalivePoolCloseOutcome::Closed);
    assert_eq!(second.drain, KeepalivePoolDrainOutcome::Drained);
    assert_eq!(second.requested, 1);
    assert_eq!(second.stopped, 0);
    assert_eq!(second.timed_out, 0);
    assert_eq!(second.rejected, 0);
    assert_eq!(second.already_closed, 1);
    assert_eq!(
        second.connection_failures,
        vec![KeepaliveConnectionStopFailure {
            index: 0,
            outcome: KeepaliveConnectionStopOutcome::AlreadyClosed,
        }]
    );

    rig.shutdown();
    server.stop();
}

#[test]
fn stop_after_force_close_drops_transport_and_closes_isolate() {
    // Force-close the pool. Without explicit Stop, the connection
    // isolate keeps its transport open — the pool no longer hands it
    // out, but the FD lives on. Send Stop to each connection address;
    // the isolate closes its transport and exits. The proof is that
    // sending another message to the same address after Stop fails
    // with CallOutcome::Closed.
    let server = ScriptedServer::start(ServerPolicy::Keepalive);
    let rig = TestRig::start();
    let pool = rig.build_pool(HttpTarget::http(server.addr), 1, 4);
    let (driver, _state, rx) = rig.driver(&pool);

    // Use the connection at least once so a transport is actually open.
    let _ = rig.rt().try_send(
        driver,
        DriverMsg::Acquire {
            id: 1,
            timeout: Duration::from_secs(2),
        },
    );
    wait_for_event(&rx, |e| matches!(e, DriverEvent::Acquired { id: 1, .. }));
    let _ = rig.rt().try_send(
        driver,
        DriverMsg::Request {
            id: 1,
            request: req(),
            request_timeout: Duration::from_secs(2),
            call_timeout: Duration::from_secs(2),
        },
    );
    wait_for_event(&rx, |e| matches!(e, DriverEvent::Request { id: 1, .. }));
    let _ = rig.rt().try_send(
        driver,
        DriverMsg::Release {
            id: 1,
            disposition: ReleaseDisposition::Reuse,
        },
    );
    wait_for_event(&rx, |e| matches!(e, DriverEvent::Released { id: 1, .. }));

    // Force close the pool.
    let _ = rig.rt().try_send(
        driver,
        DriverMsg::Close {
            mode: CloseMode::Force,
            timeout: Duration::from_secs(2),
        },
    );
    wait_for_event(&rx, |e| matches!(e, DriverEvent::PoolClosed));

    // Stop the only connection isolate. Use by_index since no lease
    // is held.
    let _ = rig.rt().try_send(
        driver,
        DriverMsg::Stop {
            id: 99,
            timeout: Duration::from_secs(2),
            by_index: Some(0),
        },
    );
    wait_for_event(&rx, |e| matches!(e, DriverEvent::Stopped { id: 99 }));

    match rig
        .rt()
        .call_blocking(
            pool.connections[0],
            KeepaliveConnectionMsg::request(req(), Duration::from_secs(2)),
            Duration::from_secs(2),
        )
        .expect("request after stop")
    {
        CallOutcome::Closed => {}
        other => panic!("expected stopped connection address to be closed, got {other:?}"),
    }

    rig.shutdown();
    server.stop();
}

// Drive one idle-retirement sweep on a connection by address; return
// whether it closed an idle socket.
fn maintain_conn(
    rig: &TestRig,
    conn: KeepaliveConnAddr,
    now: std::time::Instant,
    max_idle: Duration,
) -> bool {
    match rig
        .rt()
        .call_blocking(
            conn,
            KeepaliveConnectionMsg::Maintain { now, max_idle },
            Duration::from_secs(2),
        )
        .expect("maintain call")
    {
        CallOutcome::Replied(KeepaliveOutcome::Maintained { closed_idle }) => closed_idle,
        other => panic!("expected Maintained reply, got {other:?}"),
    }
}

fn keepalive_request(rig: &TestRig, conn: KeepaliveConnAddr) -> bool {
    match rig
        .rt()
        .call_blocking(
            conn,
            KeepaliveConnectionMsg::request(req(), Duration::from_secs(2)),
            Duration::from_secs(2),
        )
        .expect("request call")
    {
        CallOutcome::Replied(outcome) => {
            let (result, must_retire) = outcome.into_request_result();
            assert_eq!(result.expect("200 response").status, 200);
            must_retire
        }
        other => panic!("expected request reply, got {other:?}"),
    }
}

#[test]
fn idle_maintenance_closes_socket_and_next_request_reconnects() {
    let server = ScriptedServer::start(ServerPolicy::Keepalive);
    let rig = TestRig::start();
    let handles = rig.build_pool(HttpTarget::http(server.addr), 1, 4);
    let conn = handles.connections[0];
    let t0 = std::time::Instant::now();
    let max_idle = Duration::from_millis(50);

    // First request opens one TCP connection; the response is reusable.
    assert!(
        !keepalive_request(&rig, conn),
        "keepalive response is reusable"
    );
    assert_eq!(server.accepts(), 1, "first request opens one connection");

    // The first sweep only stamps the idle clock — nothing is closed.
    assert!(!maintain_conn(&rig, conn, t0, max_idle));
    assert_eq!(server.accepts(), 1, "stamping sweep keeps the socket");

    // A sweep past max_idle closes the idle socket and reports it.
    assert!(
        maintain_conn(&rig, conn, t0 + Duration::from_millis(100), max_idle),
        "idle past max_idle should retire the socket"
    );

    // The slot is still leasable: the next request reconnects, so the
    // server records a second accept. The idle socket did not silently
    // become a stale-on-next-use failure.
    assert!(!keepalive_request(&rig, conn));
    assert_eq!(
        server.accepts(),
        2,
        "idle retirement forced a clean reconnect"
    );

    rig.shutdown();
    server.stop();
}

#[test]
fn maintenance_leaves_unconnected_and_freshly_used_connections_alone() {
    let server = ScriptedServer::start(ServerPolicy::Keepalive);
    let rig = TestRig::start();
    let handles = rig.build_pool(HttpTarget::http(server.addr), 1, 4);
    let conn = handles.connections[0];
    let t0 = std::time::Instant::now();
    let max_idle = Duration::from_millis(50);

    // A never-connected slot has no socket to close.
    assert!(!maintain_conn(&rig, conn, t0, max_idle));
    assert_eq!(server.accepts(), 0, "no connection opened yet");

    // After a request the connection is idle but freshly used. The first
    // sweep only stamps the idle clock (idle age starts at 0), so it does
    // not close the socket and the next request reuses it. (A connection
    // mid-request is left alone too — `handle_maintain` returns early
    // when `in_flight` is set, before any close.)
    assert!(!keepalive_request(&rig, conn));
    assert_eq!(server.accepts(), 1);
    assert!(!maintain_conn(
        &rig,
        conn,
        t0 + Duration::from_millis(200),
        max_idle
    ));
    assert!(!keepalive_request(&rig, conn));
    assert_eq!(
        server.accepts(),
        1,
        "a freshly-used connection is reused, not retired"
    );

    rig.shutdown();
    server.stop();
}
