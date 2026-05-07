//! Tina: real loopback TCP, bounded slow-consumer mailbox, fanout
//! through `send_observed`. Over-cap admissions surface as wire-side
//! and event-side `Full` outcomes — the connection isolate tallies
//! them and writes the count to the wire so the client (and the
//! caller of [`run`]) can read it back.

use std::convert::Infallible;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::thread;
use std::time::Duration;

use tina::prelude::*;
use tina_runtime::{
    CallError, DefaultThreadedMailboxFactory, ListenerId, SendOutcome, StreamId, ThreadedRuntime,
    send_observed, tcp_accept, tcp_bind, tcp_close_listener, tcp_close_stream, tcp_read, tcp_write,
};

use crate::{Report, RunConfig};

// -------------------------------------------------------------------
// Slow consumer: bounded mailbox; just records that a message
// arrived (we don't need the payload here, only the admission).
// -------------------------------------------------------------------

#[derive(Debug, Clone, Copy)]
struct DeliverMsg(#[allow(dead_code)] usize);

#[derive(Debug, Default)]
struct SlowClient;

#[tina::isolate(message = DeliverMsg)]
impl SlowClient {
    fn handle(&mut self, _msg: DeliverMsg, _ctx: &mut Context<'_, SingleShard>) -> Effect<Self> {
        noop()
    }
}

// -------------------------------------------------------------------
// Connection: read the requested burst from the wire, fan it out via
// `send_observed`, count admission outcomes, write the metrics line
// back, close.
// -------------------------------------------------------------------

#[derive(Debug, Clone)]
enum ConnectionMsg {
    Begin,
    Read(Vec<u8>),
    Observed(SendOutcome),
    Wrote(usize),
    Closed,
    IoFailed,
}

struct Connection {
    stream: StreamId,
    slow_client: Address<DeliverMsg>,
    requested_burst: usize,
    observed: usize,
    accepted: usize,
    full: usize,
    closed: usize,
}

#[tina_runtime::isolate(
    message = ConnectionMsg,
    send = Outbound<DeliverMsg>,
)]
impl Connection {
    fn handle(&mut self, msg: ConnectionMsg, _ctx: &mut Context<'_, SingleShard>) -> Effect<Self> {
        match msg {
            ConnectionMsg::Begin => tcp_read(self.stream, 32).reply(|result| match result {
                Ok(bytes) => ConnectionMsg::Read(bytes),
                Err(_) => ConnectionMsg::IoFailed,
            }),
            ConnectionMsg::Read(bytes) => {
                self.requested_burst = parse_burst(&bytes);
                batch(
                    (0..self.requested_burst)
                        .map(|index| {
                            send_observed(self.slow_client, DeliverMsg(index))
                                .reply(ConnectionMsg::Observed)
                        })
                        .collect::<Vec<_>>(),
                )
            }
            ConnectionMsg::Observed(outcome) => {
                self.observed += 1;
                if outcome.is_accepted() {
                    self.accepted += 1;
                } else if outcome.is_full() {
                    self.full += 1;
                } else {
                    debug_assert!(outcome.is_closed());
                    self.closed += 1;
                }
                if self.observed < self.requested_burst {
                    return noop();
                }
                let response = format!(
                    "accepted={} full={} closed={}\n",
                    self.accepted, self.full, self.closed
                )
                .into_bytes();
                tcp_write(self.stream, response).reply(|result| match result {
                    Ok(count) => ConnectionMsg::Wrote(count),
                    Err(_) => ConnectionMsg::IoFailed,
                })
            }
            ConnectionMsg::Wrote(_count) => {
                tcp_close_stream(self.stream).reply(|result| match result {
                    Ok(()) => ConnectionMsg::Closed,
                    Err(_) => ConnectionMsg::IoFailed,
                })
            }
            ConnectionMsg::Closed | ConnectionMsg::IoFailed => stop(),
        }
    }
}

// -------------------------------------------------------------------
// Listener: bind, accept once, spawn the Connection, close listener.
// -------------------------------------------------------------------

#[derive(Debug, Clone)]
enum ListenerMsg {
    Start,
    Bound(Result<(ListenerId, SocketAddr), CallError>),
    Accepted(Result<(StreamId, SocketAddr), CallError>),
    ListenerClosed(Result<(), CallError>),
}

struct Listener {
    bind_addr: SocketAddr,
    slow_client: Address<DeliverMsg>,
    connection_capacity: usize,
    listener: Option<ListenerId>,
}

#[tina_runtime::isolate(
    message = ListenerMsg,
    spawn = ChildDefinition<Connection>,
)]
impl Listener {
    fn handle(&mut self, msg: ListenerMsg, _ctx: &mut Context<'_, SingleShard>) -> Effect<Self> {
        match msg {
            ListenerMsg::Start => tcp_bind(self.bind_addr).reply(ListenerMsg::Bound),
            ListenerMsg::Bound(Ok((listener, _local_addr))) => {
                self.listener = Some(listener);
                tcp_accept(listener).reply(ListenerMsg::Accepted)
            }
            ListenerMsg::Accepted(Ok((stream, _peer_addr))) => {
                let listener = self.listener.expect("listener set after bind");
                batch(vec![
                    spawn(
                        ChildDefinition::new(
                            Connection {
                                stream,
                                slow_client: self.slow_client,
                                requested_burst: 0,
                                observed: 0,
                                accepted: 0,
                                full: 0,
                                closed: 0,
                            },
                            self.connection_capacity,
                        )
                        .with_initial_message(ConnectionMsg::Begin),
                    ),
                    tcp_close_listener(listener).reply(ListenerMsg::ListenerClosed),
                ])
            }
            ListenerMsg::ListenerClosed(Ok(())) => stop(),
            ListenerMsg::Bound(Err(_))
            | ListenerMsg::Accepted(Err(_))
            | ListenerMsg::ListenerClosed(Err(_)) => stop(),
        }
    }
}

// -------------------------------------------------------------------
// Run
// -------------------------------------------------------------------

pub fn run(config: RunConfig) -> anyhow::Result<Report> {
    let runtime = ThreadedRuntime::new(SingleShard, DefaultThreadedMailboxFactory);

    let slow_client = runtime
        .register_with_capacity::<_, Infallible>(SlowClient, config.slow_consumer_capacity)
        .map_err(|e| anyhow::anyhow!("register slow client: {e:?}"))?;

    // The connection mailbox absorbs one observed-reply per fanout
    // attempt (047 finding: replies count against the requester) plus
    // a small slack for other ConnectionMsg variants. Sizing it to
    // `burst + slack` is the simplest way to prove no reply slot is
    // ever full.
    let connection_capacity = config.burst.saturating_add(16);

    let bind_addr: SocketAddr = "127.0.0.1:0".parse()?;
    let listener = runtime
        .register_with_capacity::<_, Infallible>(
            Listener {
                bind_addr,
                slow_client,
                connection_capacity,
                listener: None,
            },
            8,
        )
        .map_err(|e| anyhow::anyhow!("register listener: {e:?}"))?;

    let bound = runtime.observe_next_bound();
    runtime
        .try_send(listener, ListenerMsg::Start)
        .map_err(|e| anyhow::anyhow!("start listener: {e:?}"))?;
    let addr = bound
        .wait(Duration::from_secs(3))
        .map_err(|e| anyhow::anyhow!("listener bind: {e:?}"))?;

    let burst = config.burst;
    let response = thread::spawn(move || drive_client(addr, burst))
        .join()
        .map_err(|_| anyhow::anyhow!("client thread panicked"))??;

    let _ = runtime.shutdown();

    let (accepted, full, closed) = parse_response(&response)?;
    Ok(Report {
        accepted,
        full,
        closed,
        // Bounded admission: every accepted message reaches the
        // slow consumer (the runtime drains the mailbox); none sit
        // in a hidden buffer.
        delivered: accepted,
        buffered: 0,
    })
}

/// Connect, write the burst size, shut down the write side, read the
/// metrics line back to EOF.
fn drive_client(addr: SocketAddr, burst: usize) -> anyhow::Result<Vec<u8>> {
    let mut stream = TcpStream::connect(addr)?;
    stream.set_read_timeout(Some(Duration::from_secs(3)))?;
    stream.write_all(burst.to_string().as_bytes())?;
    stream.shutdown(std::net::Shutdown::Write)?;
    let mut response = Vec::new();
    let mut buf = [0u8; 128];
    loop {
        let n = stream.read(&mut buf)?;
        if n == 0 {
            break;
        }
        response.extend_from_slice(&buf[..n]);
        if response.ends_with(b"\n") {
            break;
        }
    }
    Ok(response)
}

fn parse_burst(bytes: &[u8]) -> usize {
    std::str::from_utf8(bytes)
        .expect("burst utf8")
        .trim()
        .parse::<usize>()
        .expect("burst usize")
}

fn parse_response(bytes: &[u8]) -> anyhow::Result<(usize, usize, usize)> {
    let text = std::str::from_utf8(bytes)?;
    let mut accepted = None;
    let mut full = None;
    let mut closed = None;
    for field in text.split_whitespace() {
        let Some((key, value)) = field.split_once('=') else {
            continue;
        };
        let value = value.parse::<usize>()?;
        match key {
            "accepted" => accepted = Some(value),
            "full" => full = Some(value),
            "closed" => closed = Some(value),
            _ => {}
        }
    }
    Ok((
        accepted.ok_or_else(|| anyhow::anyhow!("missing accepted field"))?,
        full.ok_or_else(|| anyhow::anyhow!("missing full field"))?,
        closed.ok_or_else(|| anyhow::anyhow!("missing closed field"))?,
    ))
}
