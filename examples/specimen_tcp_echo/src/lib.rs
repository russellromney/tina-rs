//! A TCP echo server built from Tina isolates.
//!
//! One connection is one isolate. A [`EchoListener`] binds a loopback
//! address, accepts in a bounded loop, and spawns one
//! [`EchoConnection`] per accepted stream. Each connection reads a
//! chunk, writes the identical bytes back (retrying partial writes so
//! the wire is never truncated), and repeats until EOF.
//!
//! The same [`EchoConnection`] source drives two runtimes without
//! change:
//!
//! - live, over a real loopback socket on [`LocalSystem`] (see
//!   [`echo_round_trip`]);
//! - deterministically, inside `tina_sim::Simulator` replayed from a
//!   seed (see `tests/sim_echo.rs`).
//!
//! The runtime's bounded-mailbox contract is separate from the wire: a
//! sequential echo self-paces one read at a time, so the socket can
//! never overflow a connection's mailbox. [`run_load_shed`] shows the
//! contract directly — a host producer that outruns a bounded worker
//! gets a typed `Full` instead of an unbounded queue.

use std::convert::Infallible;
use std::io::{Read, Write};
use std::net::{Shutdown, SocketAddr, TcpStream};
use std::time::Duration;

use tina::prelude::*;
use tina_runtime::{
    CallError, DefaultThreadedMailboxFactory, HostBurstOutcomes, ListenerId, LocalSystem,
    SendOutcome, SingleCallGate, SleepReply, StreamId, TcpReadReply, TcpStreamCloseReply,
    TcpWriteReply, send_observed, sleep, tcp_accept, tcp_bind, tcp_close_listener, tcp_close_stream,
    tcp_read, tcp_write,
};

/// Largest chunk a connection reads from the wire in one call.
pub const MAX_CHUNK: usize = 256;

/// Listener mailbox capacity. Holds the in-flight accept reply plus the
/// self-sent re-arm message.
pub const LISTENER_CAPACITY: usize = 8;

/// Per-connection mailbox capacity. A connection has at most one I/O
/// reply in flight, so this is generous slack.
pub const CONNECTION_CAPACITY: usize = 8;

// -------------------------------------------------------------------
// Connection: read a chunk, echo it back, repeat until EOF. This is
// the isolate the README shows and the isolate the simulator replays.
// -------------------------------------------------------------------

// readme:echo-isolate:begin
/// One connection's lifecycle, one message per I/O completion.
#[derive(Debug, Clone)]
pub enum EchoConnectionMsg {
    /// Kick off the first read.
    Begin,
    /// A read completed (bytes, or an I/O error).
    Read(TcpReadReply),
    /// A write completed (accepted byte count, or an I/O error).
    Wrote(TcpWriteReply),
    /// The stream close completed.
    Closed(TcpStreamCloseReply),
    /// The listener observed this connection's exact terminal result.
    TerminalReported(EchoConnectionTerminal, SendOutcome),
}

/// One accepted TCP stream, echoed back to its peer.
#[derive(Debug)]
pub struct EchoConnection {
    stream: StreamId,
    max_chunk: usize,
    /// Bytes read but not yet fully written back. A partial write
    /// leaves the tail here so the echo is never truncated.
    pending: Vec<u8>,
    listener: Address<EchoListenerMsg>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EchoConnectionTerminal {
    PeerClosedClean,
    ReadFailed(CallError),
    WriteFailed(CallError),
    CloseFailed(CallError),
    InvalidWriteCount { reported: usize, pending: usize },
}

impl EchoConnection {
    fn new(stream: StreamId, max_chunk: usize, listener: Address<EchoListenerMsg>) -> Self {
        Self {
            stream,
            max_chunk,
            pending: Vec::new(),
            listener,
        }
    }
}

#[tina_runtime::isolate(message = EchoConnectionMsg, send = Outbound<EchoListenerMsg>)]
impl EchoConnection {
    fn handle(
        &mut self,
        msg: EchoConnectionMsg,
        _ctx: &mut Context<'_, SingleShard, Self::Reply>,
    ) -> Effect<Self> {
        match msg {
            EchoConnectionMsg::Begin => {
                tcp_read(self.stream, self.max_chunk).then(EchoConnectionMsg::Read)
            }
            EchoConnectionMsg::Read(Ok(bytes)) => {
                if bytes.is_empty() {
                    tcp_close_stream(self.stream).then(EchoConnectionMsg::Closed)
                } else {
                    self.pending = bytes;
                    tcp_write(self.stream, self.pending.clone()).then(EchoConnectionMsg::Wrote)
                }
            }
            EchoConnectionMsg::Wrote(Ok(count)) => {
                if count == 0 || count > self.pending.len() {
                    return self.finish(EchoConnectionTerminal::InvalidWriteCount {
                        reported: count,
                        pending: self.pending.len(),
                    });
                }
                self.pending.drain(..count);
                if self.pending.is_empty() {
                    tcp_read(self.stream, self.max_chunk).then(EchoConnectionMsg::Read)
                } else {
                    tcp_write(self.stream, self.pending.clone()).then(EchoConnectionMsg::Wrote)
                }
            }
            EchoConnectionMsg::Closed(Ok(())) => {
                self.finish(EchoConnectionTerminal::PeerClosedClean)
            }
            EchoConnectionMsg::Read(Err(error)) => {
                self.finish(EchoConnectionTerminal::ReadFailed(error))
            }
            EchoConnectionMsg::Wrote(Err(error)) => {
                self.finish(EchoConnectionTerminal::WriteFailed(error))
            }
            EchoConnectionMsg::Closed(Err(error)) => {
                self.finish(EchoConnectionTerminal::CloseFailed(error))
            }
            EchoConnectionMsg::TerminalReported(terminal, SendOutcome::Accepted) => {
                stop_with(terminal)
            }
            EchoConnectionMsg::TerminalReported(terminal, SendOutcome::Full) => {
                self.finish(terminal)
            }
            EchoConnectionMsg::TerminalReported(
                terminal,
                SendOutcome::Closed | SendOutcome::ForeignSystem { .. },
            ) => stop_with(terminal),
        }
    }
}

impl EchoConnection {
    fn finish(&self, terminal: EchoConnectionTerminal) -> Effect<Self> {
        send_observed(self.listener, EchoListenerMsg::ConnectionStopped(terminal))
            .then(move |outcome| EchoConnectionMsg::TerminalReported(terminal, outcome))
    }
}
// readme:echo-isolate:end

// -------------------------------------------------------------------
// Listener: bind, accept in a bounded loop, spawn one connection per
// stream, close the listener when the target is met.
// -------------------------------------------------------------------

#[derive(Debug, Clone)]
pub enum EchoListenerMsg {
    Start,
    Bound {
        listener: ListenerId,
        addr: SocketAddr,
    },
    AcceptNext,
    Accepted {
        stream: StreamId,
    },
    Close,
    Closed,
    BindFailed(CallError),
    AcceptFailed(CallError),
    CloseFailed(CallError),
    ConnectionStopped(EchoConnectionTerminal),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EchoListenerTerminal {
    ClosedClean { accepted: usize },
    BindFailed(CallError),
    AcceptFailed(CallError),
    CloseFailed(CallError),
    MissingListener,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EchoRunReport {
    pub listener: EchoListenerTerminal,
    pub connections: Vec<EchoConnectionTerminal>,
}

/// Parent that owns the bound listener and spawns one handler per
/// accepted connection.
///
/// `target_accepts` is `None` to accept forever like a production
/// server; `Some(n)` bounds accepts so a test reaches a clean stop.
#[derive(Debug)]
pub struct EchoListener {
    bind_addr: SocketAddr,
    max_chunk: usize,
    target_accepts: Option<usize>,
    accepted: usize,
    listener: Option<ListenerId>,
    listener_terminal: Option<EchoListenerTerminal>,
    connection_terminals: Vec<EchoConnectionTerminal>,
}

impl EchoListener {
    /// A listener that binds `bind_addr` and accepts connections.
    ///
    /// `target_accepts` is `None` to accept forever like a production
    /// server; `Some(n)` bounds accepts so a test reaches a clean stop.
    pub fn new(bind_addr: SocketAddr, target_accepts: Option<usize>) -> Self {
        Self {
            bind_addr,
            max_chunk: MAX_CHUNK,
            target_accepts,
            accepted: 0,
            listener: None,
            listener_terminal: None,
            connection_terminals: Vec::new(),
        }
    }
}

#[tina_runtime::isolate(
    message = EchoListenerMsg,
    send = Outbound<EchoListenerMsg>,
    spawn = ChildDefinition<EchoConnection>,
)]
impl EchoListener {
    fn handle(
        &mut self,
        msg: EchoListenerMsg,
        ctx: &mut Context<'_, SingleShard, Self::Reply>,
    ) -> Effect<Self> {
        match msg {
            EchoListenerMsg::Start => {
                let addr = self.bind_addr;
                tcp_bind(addr).then(|result| match result {
                    Ok((listener, local_addr)) => EchoListenerMsg::Bound {
                        listener,
                        addr: local_addr,
                    },
                    Err(error) => EchoListenerMsg::BindFailed(error),
                })
            }
            EchoListenerMsg::Bound { listener, .. } => {
                self.listener = Some(listener);
                accept_next(listener)
            }
            EchoListenerMsg::AcceptNext => {
                let Some(listener) = self.listener else {
                    return stop_with(EchoRunReport {
                        listener: EchoListenerTerminal::MissingListener,
                        connections: std::mem::take(&mut self.connection_terminals),
                    });
                };
                accept_next(listener)
            }
            EchoListenerMsg::Accepted { stream } => {
                self.accepted += 1;
                // Server-side liveness for the standing server only. A
                // bounded (test/demo) run leaves this quiet so its output
                // stays deterministic.
                if self.target_accepts.is_none() {
                    println!("accepted connection");
                }
                let child = spawn(
                    ChildDefinition::new(
                        EchoConnection::new(stream, self.max_chunk, ctx.me()),
                        CONNECTION_CAPACITY,
                    )
                    .with_initial_message(EchoConnectionMsg::Begin),
                );
                let follow_up = match self.target_accepts {
                    Some(target) if self.accepted >= target => EchoListenerMsg::Close,
                    _ => EchoListenerMsg::AcceptNext,
                };
                batch([child, ctx.send_self(follow_up)])
            }
            EchoListenerMsg::Close => {
                let Some(listener) = self.listener else {
                    return stop_with(EchoRunReport {
                        listener: EchoListenerTerminal::MissingListener,
                        connections: std::mem::take(&mut self.connection_terminals),
                    });
                };
                tcp_close_listener(listener).then(|result| match result {
                    Ok(()) => EchoListenerMsg::Closed,
                    Err(error) => EchoListenerMsg::CloseFailed(error),
                })
            }
            EchoListenerMsg::Closed => {
                self.listener_terminal = Some(EchoListenerTerminal::ClosedClean {
                    accepted: self.accepted,
                });
                self.finish_if_complete()
            }
            EchoListenerMsg::BindFailed(error) => stop_with(EchoRunReport {
                listener: EchoListenerTerminal::BindFailed(error),
                connections: std::mem::take(&mut self.connection_terminals),
            }),
            EchoListenerMsg::AcceptFailed(error) => stop_with(EchoRunReport {
                listener: EchoListenerTerminal::AcceptFailed(error),
                connections: std::mem::take(&mut self.connection_terminals),
            }),
            EchoListenerMsg::CloseFailed(error) => {
                self.listener_terminal = Some(EchoListenerTerminal::CloseFailed(error));
                self.finish_if_complete()
            }
            EchoListenerMsg::ConnectionStopped(terminal) => {
                if self.target_accepts.is_some() {
                    self.connection_terminals.push(terminal);
                }
                self.finish_if_complete()
            }
        }
    }
}

impl EchoListener {
    fn finish_if_complete(&mut self) -> Effect<Self> {
        let Some(target) = self.target_accepts else {
            return noop();
        };
        let Some(listener) = self.listener_terminal else {
            return noop();
        };
        if self.connection_terminals.len() != target {
            return noop();
        }
        stop_with(EchoRunReport {
            listener,
            connections: std::mem::take(&mut self.connection_terminals),
        })
    }
}

fn accept_next(listener: ListenerId) -> Effect<EchoListener> {
    tcp_accept(listener).then(|result| match result {
        Ok((stream, _peer_addr)) => EchoListenerMsg::Accepted { stream },
        Err(error) => EchoListenerMsg::AcceptFailed(error),
    })
}

// -------------------------------------------------------------------
// Live server: bind on an ephemeral loopback port, echo one round
// trip, stop cleanly. The bound address is learned through the
// runtime's `observe_next_bound` handle — no shared-slot polling.
// -------------------------------------------------------------------

/// Starts a one-shot echo server, sends `payload` from a std client,
/// and returns the bytes echoed back.
pub fn echo_round_trip(payload: &[u8]) -> anyhow::Result<Vec<u8>> {
    let app = LocalSystem::single_shard(SingleShard, DefaultThreadedMailboxFactory).try_build()?;
    Ok(app.run_to_shutdown_reported(Duration::from_secs(5), |app| {
        echo_round_trip_application(app, payload)
    })?)
}

fn echo_round_trip_application(
    app: &LocalSystem<SingleShard, DefaultThreadedMailboxFactory>,
    payload: &[u8],
) -> anyhow::Result<Vec<u8>> {
    let bind_addr: SocketAddr = "127.0.0.1:0".parse()?;

    let listener = app
        .register_root::<_, EchoListenerMsg>(
            EchoListener::new(bind_addr, Some(1)),
            LISTENER_CAPACITY,
        )
        .map_err(|e| anyhow::anyhow!("register listener: {e:?}"))?;

    let bound = app
        .observe_next_bound()
        .map_err(|e| anyhow::anyhow!("register bind observer: {e}"))?;
    let listener_result = app
        .observe_result::<EchoRunReport, _, _>(listener)
        .map_err(|e| anyhow::anyhow!("register listener result observer: {e:?}"))?;
    app.try_send(listener, EchoListenerMsg::Start)
        .map_err(|e| anyhow::anyhow!("start listener: {e:?}"))?;

    let addr = bound
        .wait(Duration::from_secs(3))
        .map_err(|e| anyhow::anyhow!("listener bind: {e:?}"))?;

    let echoed = client_round_trip(addr, payload)?;

    let run_report = listener_result
        .wait(Duration::from_secs(5))
        .map_err(|e| anyhow::anyhow!("listener did not stop: {e:?}"))?;
    anyhow::ensure!(
        run_report.listener == (EchoListenerTerminal::ClosedClean { accepted: 1 }),
        "listener terminated unexpectedly: {run_report:?}"
    );
    anyhow::ensure!(
        run_report.connections == [EchoConnectionTerminal::PeerClosedClean],
        "connection terminated unexpectedly: {run_report:?}"
    );
    Ok(echoed)
}

/// Connects, writes `payload`, half-closes the write side, and reads
/// the echo until EOF.
fn client_round_trip(addr: SocketAddr, payload: &[u8]) -> anyhow::Result<Vec<u8>> {
    let mut stream = TcpStream::connect(addr)?;
    stream.set_read_timeout(Some(Duration::from_secs(3)))?;
    stream.write_all(payload)?;
    stream.shutdown(Shutdown::Write)?;
    let mut received = Vec::new();
    let mut buf = [0u8; 256];
    loop {
        let n = stream.read(&mut buf)?;
        if n == 0 {
            break;
        }
        received.extend_from_slice(&buf[..n]);
    }
    Ok(received)
}

// -------------------------------------------------------------------
// Load shed: the bounded-mailbox contract, shown directly. A host
// producer bursts a bounded worker that drains one unit at a time;
// admissions past capacity come back as typed `Full`.
// -------------------------------------------------------------------

/// One bounded worker; each record costs one gated sleep to process.
#[derive(Debug)]
enum MeterMsg {
    Record(#[allow(dead_code)] u32),
    Done(SleepReply),
}

struct Meter {
    work: Duration,
    /// Names the "one unit in flight, N queued" invariant.
    gate: SingleCallGate,
}

#[tina_runtime::isolate(message = MeterMsg)]
impl Meter {
    fn handle(&mut self, msg: MeterMsg, _ctx: &mut Context<'_, SingleShard>) -> Effect<Self> {
        match msg {
            MeterMsg::Record(_) => {
                if self.gate.submit() {
                    sleep(self.work).then(MeterMsg::Done)
                } else {
                    noop()
                }
            }
            MeterMsg::Done(reply) => {
                if reply.is_err() {
                    self.gate.cancel_in_flight();
                    return noop();
                }
                if self.gate.complete() {
                    sleep(self.work).then(MeterMsg::Done)
                } else {
                    noop()
                }
            }
        }
    }
}

/// What a host burst against a bounded worker observed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LoadShedReport {
    /// Records the worker admitted into its mailbox.
    pub admitted: u32,
    /// Records rejected with a typed `Full` (mailbox or ingress).
    pub full: u32,
}

impl LoadShedReport {
    /// Every submitted record is accounted for as admitted or shed.
    pub fn total(&self) -> u32 {
        self.admitted + self.full
    }
}

/// Bursts `burst` records at a worker whose mailbox holds `capacity`.
///
/// The worker drains one record per gated sleep, so a burst larger than
/// its capacity outruns it and the surplus comes back as typed `Full`.
pub fn run_load_shed(burst: u32, capacity: usize) -> anyhow::Result<LoadShedReport> {
    let app = LocalSystem::single_shard(SingleShard, DefaultThreadedMailboxFactory).try_build()?;
    Ok(app.run_to_shutdown_reported(Duration::from_secs(5), move |app| {
        run_load_shed_on(app, burst, capacity)
    })?)
}

fn run_load_shed_on(
    app: &LocalSystem<SingleShard, DefaultThreadedMailboxFactory>,
    burst: u32,
    capacity: usize,
) -> anyhow::Result<LoadShedReport> {
    let worker = app
        .register_root::<Meter, Infallible>(
            Meter {
                work: Duration::from_millis(5),
                gate: SingleCallGate::new(),
            },
            capacity,
        )
        .map_err(|e| anyhow::anyhow!("register meter: {e:?}"))?;

    let outcomes = HostBurstOutcomes::new();
    for n in 0..burst {
        let _ = app.try_send_outcome(worker, MeterMsg::Record(n), &outcomes);
    }
    outcomes
        .wait_complete(Duration::from_secs(2))
        .map_err(|e| anyhow::anyhow!("burst observers: {e}"))?;
    let snap = outcomes.snapshot();

    Ok(LoadShedReport {
        admitted: snap.admitted,
        full: snap.mailbox_full + snap.ingress_full,
    })
}
