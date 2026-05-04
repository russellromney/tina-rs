//! Deterministic in-memory TCP backend for Betelgeuse tests.
//!
//! The backend implements Betelgeuse's socket-facing [`IO`] and [`IOLoop`]
//! traits without kernel I/O. Callers submit ordinary bind/accept/recv/send
//! operations, then explicitly drive completion readiness with [`IOLoop::step`].
//! Tests can connect synthetic peers, delay ready completions, and force
//! partial sends while keeping ownership of completion slots unchanged.
//!
//! This module intentionally stays at the Betelgeuse I/O layer: it models TCP
//! streams, listeners, completion ordering, and peer bytes, not application
//! protocols or a higher-level runtime.

use std::{
    alloc::Allocator,
    collections::{BTreeMap, VecDeque},
    io,
    net::{IpAddr, Ipv4Addr, SocketAddr},
    path::Path,
    sync::{Arc, Mutex},
};

use crate::{
    AcceptCompletion, AcceptOp, IO, IOFile, IOLoop, IOLoopHandle, IOSocket, OpenOptions, Operation,
    RecvCompletion, RecvOp, SendCompletion, SendOp,
};

/// Deterministic simulated I/O backend configuration.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct SimulatedConfig {
    /// Seed used by deterministic fault decisions.
    pub seed: u64,

    /// Optional deterministic completion delay.
    pub completion_delay: SimulatedDelay,

    /// Optional maximum byte count for one simulated socket send completion.
    pub max_send_chunk: Option<usize>,
}

/// Deterministic completion delay mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SimulatedDelay {
    /// No completion delay.
    #[default]
    None,

    /// Delay one out of every `one_in` ready completions by `steps` loop steps.
    Every {
        /// Deterministic frequency. `1` means every ready completion.
        one_in: u64,

        /// Number of extra [`IOLoop::step`] calls before completion.
        steps: u64,
    },
}

/// Cloneable handle to one deterministic simulated I/O backend.
#[derive(Clone)]
pub struct SimulatedIO {
    state: Arc<Mutex<SimulatedState>>,
}

impl SimulatedIO {
    /// Creates a simulated backend with no faults.
    pub fn new() -> Self {
        Self::with_config(SimulatedConfig::default())
    }

    /// Creates a simulated backend with explicit deterministic fault config.
    pub fn with_config(config: SimulatedConfig) -> Self {
        Self {
            state: Arc::new(Mutex::new(SimulatedState {
                config,
                next_socket_id: 1,
                next_port: 40_000,
                next_peer_port: 50_000,
                ready_ordinal: 0,
                sockets: BTreeMap::new(),
                listeners_by_addr: BTreeMap::new(),
                pending: VecDeque::new(),
            })),
        }
    }

    /// Wraps this backend in an [`IOLoopHandle`].
    pub fn loop_handle<A>(&self, allocator: A) -> IOLoopHandle<A>
    where
        A: Allocator + Clone,
    {
        IOLoopHandle::new(std::rc::Rc::new(self.clone()), allocator)
    }

    /// Connects a simulated peer to a bound listener and queues `input` for
    /// the server side to read.
    pub fn connect(&self, listener_addr: SocketAddr, input: Vec<u8>) -> io::Result<SimulatedPeer> {
        let mut state = self.state.lock().expect("simulated io mutex");
        let listener_id = *state.listeners_by_addr.get(&listener_addr).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::ConnectionRefused,
                "simulated listener not found",
            )
        })?;
        let listener = state
            .sockets
            .get(&listener_id)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotConnected, "listener closed"))?;
        if listener.closed || !matches!(listener.kind, SimulatedSocketKind::Listener) {
            return Err(io::Error::new(
                io::ErrorKind::NotConnected,
                "listener closed",
            ));
        }

        let stream_id = state.allocate_socket_id();
        let peer_addr =
            SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), state.allocate_peer_port());
        state.sockets.insert(
            stream_id,
            SimulatedSocketState {
                kind: SimulatedSocketKind::Stream,
                local_addr: Some(listener_addr),
                peer_addr: Some(peer_addr),
                closed: false,
                inbound: VecDeque::from(input),
                peer_input_closed: true,
                peer_output: Vec::new(),
                backlog: VecDeque::new(),
            },
        );
        state
            .sockets
            .get_mut(&listener_id)
            .expect("listener checked above")
            .backlog
            .push_back(stream_id);

        Ok(SimulatedPeer {
            state: Arc::clone(&self.state),
            stream_id,
        })
    }
}

impl Default for SimulatedIO {
    fn default() -> Self {
        Self::new()
    }
}

/// Handle for the peer side of one simulated stream.
#[derive(Clone)]
pub struct SimulatedPeer {
    state: Arc<Mutex<SimulatedState>>,
    stream_id: i32,
}

impl SimulatedPeer {
    /// Returns bytes written by the server side.
    pub fn output(&self) -> Vec<u8> {
        self.state
            .lock()
            .expect("simulated io mutex")
            .sockets
            .get(&self.stream_id)
            .map(|stream| stream.peer_output.clone())
            .unwrap_or_default()
    }

    /// Queues more peer-to-server input.
    pub fn push_input(&self, bytes: &[u8]) {
        let mut state = self.state.lock().expect("simulated io mutex");
        if let Some(stream) = state.sockets.get_mut(&self.stream_id) {
            stream.inbound.extend(bytes.iter().copied());
            stream.peer_input_closed = false;
        }
    }

    /// Marks peer-to-server input as closed.
    pub fn close_input(&self) {
        let mut state = self.state.lock().expect("simulated io mutex");
        if let Some(stream) = state.sockets.get_mut(&self.stream_id) {
            stream.peer_input_closed = true;
        }
    }
}

struct SimulatedState {
    config: SimulatedConfig,
    next_socket_id: i32,
    next_port: u16,
    next_peer_port: u16,
    ready_ordinal: u64,
    sockets: BTreeMap<i32, SimulatedSocketState>,
    listeners_by_addr: BTreeMap<SocketAddr, i32>,
    pending: VecDeque<SimulatedPendingOp>,
}

impl SimulatedState {
    fn allocate_socket_id(&mut self) -> i32 {
        let id = self.next_socket_id;
        self.next_socket_id += 1;
        id
    }

    fn allocate_port(&mut self) -> u16 {
        let port = self.next_port;
        self.next_port += 1;
        port
    }

    fn allocate_peer_port(&mut self) -> u16 {
        let port = self.next_peer_port;
        self.next_peer_port += 1;
        port
    }

    fn delay_for_next_ready(&mut self) -> u64 {
        let SimulatedDelay::Every { one_in, steps } = self.config.completion_delay else {
            return 0;
        };
        if one_in == 0 || steps == 0 {
            return 0;
        }
        let ordinal = self.ready_ordinal;
        self.ready_ordinal += 1;
        if mix(self.config.seed ^ ordinal).is_multiple_of(one_in) {
            steps
        } else {
            0
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SimulatedSocketKind {
    Unbound,
    Listener,
    Stream,
}

struct SimulatedSocketState {
    kind: SimulatedSocketKind,
    local_addr: Option<SocketAddr>,
    peer_addr: Option<SocketAddr>,
    closed: bool,
    inbound: VecDeque<u8>,
    peer_input_closed: bool,
    peer_output: Vec<u8>,
    backlog: VecDeque<i32>,
}

enum SimulatedPendingOp {
    Accept {
        socket_id: i32,
        completion: usize,
        delay: Option<u64>,
    },
    Recv {
        socket_id: i32,
        len: usize,
        completion: usize,
        delay: Option<u64>,
    },
    Send {
        socket_id: i32,
        bytes: Vec<u8>,
        completion: usize,
        delay: Option<u64>,
    },
}

#[derive(Clone)]
struct SimulatedSocket {
    state: Arc<Mutex<SimulatedState>>,
    socket_id: Arc<Mutex<i32>>,
}

impl IOSocket for SimulatedSocket {
    fn bind(&self, addr: SocketAddr) -> io::Result<()> {
        let mut state = self.state.lock().expect("simulated io mutex");
        let socket_id = *self.socket_id.lock().expect("socket id mutex");
        let mut local_addr = addr;
        if local_addr.port() == 0 {
            local_addr.set_port(state.allocate_port());
        }
        if state.listeners_by_addr.contains_key(&local_addr) {
            return Err(io::Error::new(
                io::ErrorKind::AddrInUse,
                "simulated listener address already bound",
            ));
        }
        let socket = state
            .sockets
            .get_mut(&socket_id)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotConnected, "socket missing"))?;
        socket.kind = SimulatedSocketKind::Listener;
        socket.local_addr = Some(local_addr);
        socket.closed = false;
        state.listeners_by_addr.insert(local_addr, socket_id);
        Ok(())
    }

    fn local_addr(&self) -> io::Result<SocketAddr> {
        let state = self.state.lock().expect("simulated io mutex");
        let socket_id = *self.socket_id.lock().expect("socket id mutex");
        state
            .sockets
            .get(&socket_id)
            .and_then(|socket| (!socket.closed).then_some(socket.local_addr).flatten())
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotConnected, "socket has no local addr"))
    }

    fn peer_addr(&self) -> io::Result<SocketAddr> {
        let state = self.state.lock().expect("simulated io mutex");
        let socket_id = *self.socket_id.lock().expect("socket id mutex");
        state
            .sockets
            .get(&socket_id)
            .and_then(|socket| (!socket.closed).then_some(socket.peer_addr).flatten())
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotConnected, "socket has no peer addr"))
    }

    fn accept(&self, c: &mut AcceptCompletion) -> io::Result<()> {
        let state = self.state.lock().expect("simulated io mutex");
        let socket_id = *self.socket_id.lock().expect("socket id mutex");
        let socket = state
            .sockets
            .get(&socket_id)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotConnected, "listener missing"))?;
        if socket.closed || !matches!(socket.kind, SimulatedSocketKind::Listener) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "accept requires open listener",
            ));
        }
        drop(state);
        c.inner_mut()
            .prepare(Operation::Accept(AcceptOp { fd: socket_id }));
        self.state
            .lock()
            .expect("simulated io mutex")
            .pending
            .push_back(SimulatedPendingOp::Accept {
                socket_id,
                completion: c as *mut AcceptCompletion as usize,
                delay: None,
            });
        Ok(())
    }

    fn recv(&self, c: &mut RecvCompletion, len: usize) -> io::Result<()> {
        let state = self.state.lock().expect("simulated io mutex");
        let socket_id = *self.socket_id.lock().expect("socket id mutex");
        let socket = state
            .sockets
            .get(&socket_id)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotConnected, "stream missing"))?;
        if socket.closed || !matches!(socket.kind, SimulatedSocketKind::Stream) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "recv requires open stream",
            ));
        }
        drop(state);
        c.inner_mut().prepare(Operation::Recv(RecvOp {
            fd: socket_id,
            buf: vec![0_u8; len],
            flags: 0,
        }));
        self.state
            .lock()
            .expect("simulated io mutex")
            .pending
            .push_back(SimulatedPendingOp::Recv {
                socket_id,
                len,
                completion: c as *mut RecvCompletion as usize,
                delay: None,
            });
        Ok(())
    }

    fn send(&self, c: &mut SendCompletion, buf: Vec<u8>) -> io::Result<()> {
        let state = self.state.lock().expect("simulated io mutex");
        let socket_id = *self.socket_id.lock().expect("socket id mutex");
        let socket = state
            .sockets
            .get(&socket_id)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotConnected, "stream missing"))?;
        if socket.closed || !matches!(socket.kind, SimulatedSocketKind::Stream) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "send requires open stream",
            ));
        }
        drop(state);
        c.inner_mut().prepare(Operation::Send(SendOp {
            fd: socket_id,
            buf: buf.clone(),
            flags: 0,
        }));
        self.state
            .lock()
            .expect("simulated io mutex")
            .pending
            .push_back(SimulatedPendingOp::Send {
                socket_id,
                bytes: buf,
                completion: c as *mut SendCompletion as usize,
                delay: None,
            });
        Ok(())
    }

    fn set_nodelay(&self, _on: bool) -> io::Result<()> {
        Ok(())
    }

    fn close(&self) {
        let mut state = self.state.lock().expect("simulated io mutex");
        let socket_id = *self.socket_id.lock().expect("socket id mutex");
        if let Some(socket) = state.sockets.get_mut(&socket_id) {
            socket.closed = true;
            if let Some(addr) = socket.local_addr {
                state.listeners_by_addr.remove(&addr);
            }
        }
    }
}

impl IO for SimulatedIO {
    fn open(&self, _path: &Path, _options: OpenOptions) -> io::Result<Box<dyn IOFile>> {
        Err(unsupported("simulated file open"))
    }

    fn socket(&self) -> io::Result<Box<dyn IOSocket>> {
        let mut state = self.state.lock().expect("simulated io mutex");
        let socket_id = state.allocate_socket_id();
        state.sockets.insert(
            socket_id,
            SimulatedSocketState {
                kind: SimulatedSocketKind::Unbound,
                local_addr: None,
                peer_addr: None,
                closed: false,
                inbound: VecDeque::new(),
                peer_input_closed: true,
                peer_output: Vec::new(),
                backlog: VecDeque::new(),
            },
        );
        Ok(Box::new(SimulatedSocket {
            state: Arc::clone(&self.state),
            socket_id: Arc::new(Mutex::new(socket_id)),
        }))
    }

    fn mkdir(&self, _c: &mut crate::MkdirCompletion, _path: &Path, _mode: u32) -> io::Result<()> {
        Err(unsupported("simulated mkdir"))
    }

    fn backend_name(&self) -> &'static str {
        "simulated"
    }
}

impl IOLoop for SimulatedIO {
    fn step(&self) -> io::Result<bool> {
        let mut state = self.state.lock().expect("simulated io mutex");
        let mut progressed = false;
        let mut next_pending = VecDeque::new();

        while let Some(mut pending) = state.pending.pop_front() {
            if !pending.is_ready(&state) {
                next_pending.push_back(pending);
                continue;
            }

            if pending.delay_mut().is_none() {
                *pending.delay_mut() = Some(state.delay_for_next_ready());
            }
            if let Some(delay) = pending.delay_mut()
                && *delay > 0
            {
                *delay -= 1;
                next_pending.push_back(pending);
                progressed = true;
                continue;
            }

            let result = pending
                .ready_result(&mut state)
                .expect("pending op advertised readiness");
            pending.complete(result, &self.state);
            progressed = true;
        }

        state.pending = next_pending;
        Ok(progressed)
    }

    fn pending_completion_count(&self) -> usize {
        self.state.lock().expect("simulated io mutex").pending.len()
    }

    fn cancel_pending_completions(&self) -> io::Result<()> {
        let mut state = self.state.lock().expect("simulated io mutex");
        let pending = std::mem::take(&mut state.pending);
        drop(state);

        for pending in pending {
            pending.complete_cancelled();
        }
        Ok(())
    }
}

impl SimulatedPendingOp {
    fn delay_mut(&mut self) -> &mut Option<u64> {
        match self {
            Self::Accept { delay, .. } | Self::Recv { delay, .. } | Self::Send { delay, .. } => {
                delay
            }
        }
    }

    fn ready_result(&mut self, state: &mut SimulatedState) -> Option<SimulatedReadyResult> {
        match self {
            Self::Accept { socket_id, .. } => {
                let listener = state.sockets.get_mut(socket_id)?;
                if listener.closed {
                    return Some(SimulatedReadyResult::Accept(Err(io::Error::new(
                        io::ErrorKind::NotConnected,
                        "listener closed",
                    ))));
                }
                let stream_id = listener.backlog.pop_front()?;
                Some(SimulatedReadyResult::Accept(Ok(stream_id)))
            }
            Self::Recv { socket_id, len, .. } => {
                let Some(stream) = state.sockets.get_mut(socket_id) else {
                    return Some(SimulatedReadyResult::Recv(Err(io::Error::new(
                        io::ErrorKind::NotConnected,
                        "stream missing",
                    ))));
                };
                if stream.closed {
                    return Some(SimulatedReadyResult::Recv(Err(io::Error::new(
                        io::ErrorKind::NotConnected,
                        "stream closed",
                    ))));
                }
                if stream.inbound.is_empty() && !stream.peer_input_closed {
                    return None;
                }
                let count = (*len).min(stream.inbound.len());
                let bytes = stream.inbound.drain(..count).collect();
                Some(SimulatedReadyResult::Recv(Ok(bytes)))
            }
            Self::Send {
                socket_id, bytes, ..
            } => {
                let max_count = state.config.max_send_chunk.unwrap_or(bytes.len());
                let Some(stream) = state.sockets.get_mut(socket_id) else {
                    return Some(SimulatedReadyResult::Send(Err(io::Error::new(
                        io::ErrorKind::NotConnected,
                        "stream missing",
                    ))));
                };
                if stream.closed {
                    return Some(SimulatedReadyResult::Send(Err(io::Error::new(
                        io::ErrorKind::NotConnected,
                        "stream closed",
                    ))));
                }
                let count = max_count.min(bytes.len());
                stream.peer_output.extend_from_slice(&bytes[..count]);
                Some(SimulatedReadyResult::Send(Ok(count)))
            }
        }
    }

    fn is_ready(&self, state: &SimulatedState) -> bool {
        match self {
            Self::Accept { socket_id, .. } => match state.sockets.get(socket_id) {
                Some(listener) if !listener.closed => !listener.backlog.is_empty(),
                _ => true,
            },
            Self::Recv { socket_id, .. } => match state.sockets.get(socket_id) {
                Some(stream) if !stream.closed => {
                    !stream.inbound.is_empty() || stream.peer_input_closed
                }
                _ => true,
            },
            Self::Send { .. } => true,
        }
    }

    fn complete(self, result: SimulatedReadyResult, state: &Arc<Mutex<SimulatedState>>) {
        match (self, result) {
            (Self::Accept { completion, .. }, SimulatedReadyResult::Accept(Ok(stream_id))) => unsafe {
                let completion = &mut *(completion as *mut AcceptCompletion);
                completion.complete(Ok(Box::new(SimulatedSocket {
                    state: Arc::clone(state),
                    socket_id: Arc::new(Mutex::new(stream_id)),
                })));
            },
            (Self::Accept { completion, .. }, SimulatedReadyResult::Accept(Err(error))) => unsafe {
                (&mut *(completion as *mut AcceptCompletion)).complete(Err(error));
            },
            (Self::Recv { completion, .. }, SimulatedReadyResult::Recv(result)) => unsafe {
                (&mut *(completion as *mut RecvCompletion)).complete(result);
            },
            (Self::Send { completion, .. }, SimulatedReadyResult::Send(result)) => unsafe {
                (&mut *(completion as *mut SendCompletion)).complete(result);
            },
            _ => unreachable!("pending op and ready result mismatch"),
        }
    }

    fn complete_cancelled(self) {
        let error = || io::Error::new(io::ErrorKind::Interrupted, "simulated operation cancelled");
        match self {
            Self::Accept { completion, .. } => unsafe {
                (&mut *(completion as *mut AcceptCompletion)).complete(Err(error()));
            },
            Self::Recv { completion, .. } => unsafe {
                (&mut *(completion as *mut RecvCompletion)).complete(Err(error()));
            },
            Self::Send { completion, .. } => unsafe {
                (&mut *(completion as *mut SendCompletion)).complete(Err(error()));
            },
        }
    }
}

enum SimulatedReadyResult {
    Accept(io::Result<i32>),
    Recv(io::Result<Vec<u8>>),
    Send(io::Result<usize>),
}

fn mix(mut value: u64) -> u64 {
    value ^= value >> 33;
    value = value.wrapping_mul(0xff51afd7ed558ccd);
    value ^= value >> 33;
    value = value.wrapping_mul(0xc4ceb9fe1a85ec53);
    value ^ (value >> 33)
}

fn unsupported(operation: &str) -> io::Error {
    io::Error::new(
        io::ErrorKind::Unsupported,
        format!("{operation} is unsupported by Betelgeuse simulated I/O"),
    )
}
