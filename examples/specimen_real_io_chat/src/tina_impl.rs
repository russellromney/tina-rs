//! Tina: real loopback TCP, bounded slow-consumer mailbox, fanout
//! through `broadcast_observed`. Over-cap admissions surface as
//! wire-side and event-side `Full` outcomes — the connection isolate
//! tallies them and writes the count to the wire so the client (and
//! the caller of [`run`]) can read it back.

use std::convert::Infallible;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::thread;
use std::time::Duration;

use tina::prelude::*;
use tina_runtime::{
    BroadcastReport, BroadcastTargets, BroadcastTracker, DefaultThreadedMailboxFactory, ListenerId,
    SendOutcome, StreamId, TcpAcceptReply, TcpBindReply, TcpListenerCloseReply, TcpReadReply,
    TcpStreamCloseReply, TcpWriteReply, ThreadedRuntime, broadcast_observed, tcp_accept, tcp_bind,
    tcp_close_listener, tcp_close_stream, tcp_read, tcp_write,
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
    fn handle(
        &mut self,
        _msg: DeliverMsg,
        _ctx: &mut Context<'_, SingleShard, Self::Reply>,
    ) -> Effect<Self> {
        noop()
    }
}

// -------------------------------------------------------------------
// Connection: read the requested burst from the wire, cap it at the
// service-owned broadcast target limit, fan it out via
// `broadcast_observed`, count admission outcomes, write the metrics
// line back, close.
// -------------------------------------------------------------------

#[derive(Debug, Clone)]
enum ConnectionMsg {
    Begin,
    Read(TcpReadReply),
    Observed(usize, SendOutcome),
    Wrote(TcpWriteReply),
    Closed(TcpStreamCloseReply),
}

#[derive(Debug, Default)]
struct FanoutState {
    burst: usize,
    pre_shed_full: usize,
    tracker: Option<BroadcastTracker<usize>>,
}

impl FanoutState {
    fn start<M>(
        &mut self,
        requested_burst: usize,
        max_targets: usize,
        slow_client: Address<M>,
    ) -> anyhow::Result<BroadcastTargets<usize, M>>
    where
        M: 'static,
    {
        self.burst = requested_burst;
        let admitted_targets = requested_burst.min(max_targets);
        self.pre_shed_full = requested_burst.saturating_sub(admitted_targets);
        let targets = BroadcastTargets::try_from_iter(
            max_targets,
            (0..admitted_targets).map(|index| (index, slow_client)),
        )?;
        self.tracker = Some(targets.tracker());
        Ok(targets)
    }

    fn record(
        &mut self,
        key: usize,
        outcome: SendOutcome,
    ) -> anyhow::Result<Option<(usize, usize, usize)>> {
        let tracker = self
            .tracker
            .as_mut()
            .ok_or_else(|| anyhow::anyhow!("broadcast outcome without tracker"))?;
        let Some(report) = tracker.record(key, outcome)? else {
            return Ok(None);
        };
        report.assert_all_accounted_for(report.outcomes().len())?;
        let (accepted, full, closed) = counts_from_report(&report, self.pre_shed_full);
        debug_assert_eq!(
            accepted + full + closed,
            self.burst,
            "broadcast accounting must cover admitted and pre-shed targets",
        );
        Ok(Some((accepted, full, closed)))
    }
}

struct Connection {
    stream: StreamId,
    slow_client: Address<DeliverMsg>,
    max_broadcast_targets: usize,
    fanout: FanoutState,
}

#[tina_runtime::isolate(
    message = ConnectionMsg,
    send = Outbound<DeliverMsg>,
)]
impl Connection {
    fn handle(
        &mut self,
        msg: ConnectionMsg,
        _ctx: &mut Context<'_, SingleShard, Self::Reply>,
    ) -> Effect<Self> {
        match msg {
            ConnectionMsg::Begin => tcp_read(self.stream, 32).then(ConnectionMsg::Read),
            ConnectionMsg::Read(Ok(bytes)) => {
                let requested_burst = parse_burst(&bytes);
                let targets = match self.fanout.start(
                    requested_burst,
                    self.max_broadcast_targets,
                    self.slow_client,
                ) {
                    Ok(targets) => targets,
                    Err(_) => return stop(),
                };
                if targets.is_empty() {
                    return write_counts(self.stream, 0, self.fanout.pre_shed_full, 0);
                }
                broadcast_observed(targets, |index| DeliverMsg(*index), ConnectionMsg::Observed)
            }
            ConnectionMsg::Observed(key, outcome) => {
                let counts = match self.fanout.record(key, outcome) {
                    Ok(Some(counts)) => counts,
                    Ok(None) => return noop(),
                    Err(_) => return stop(),
                };
                write_counts(self.stream, counts.0, counts.1, counts.2)
            }
            ConnectionMsg::Wrote(Ok(_)) => {
                tcp_close_stream(self.stream).then(ConnectionMsg::Closed)
            }
            ConnectionMsg::Closed(Ok(())) => stop(),
            ConnectionMsg::Read(Err(_))
            | ConnectionMsg::Wrote(Err(_))
            | ConnectionMsg::Closed(Err(_)) => stop(),
        }
    }
}

// -------------------------------------------------------------------
// Listener: bind, accept once, spawn the Connection, close listener.
// -------------------------------------------------------------------

#[derive(Debug, Clone)]
enum ListenerMsg {
    Start,
    Bound(TcpBindReply),
    Accepted(TcpAcceptReply),
    ListenerClosed(TcpListenerCloseReply),
}

struct Listener {
    bind_addr: SocketAddr,
    slow_client: Address<DeliverMsg>,
    connection_capacity: usize,
    max_broadcast_targets: usize,
    listener: Option<ListenerId>,
}

#[tina_runtime::isolate(
    message = ListenerMsg,
    spawn = ChildDefinition<Connection>,
)]
impl Listener {
    fn handle(
        &mut self,
        msg: ListenerMsg,
        _ctx: &mut Context<'_, SingleShard, Self::Reply>,
    ) -> Effect<Self> {
        match msg {
            ListenerMsg::Start => tcp_bind(self.bind_addr).then(ListenerMsg::Bound),
            ListenerMsg::Bound(Ok((listener, _local_addr))) => {
                self.listener = Some(listener);
                tcp_accept(listener).then(ListenerMsg::Accepted)
            }
            ListenerMsg::Accepted(Ok((stream, _peer_addr))) => {
                let listener = self.listener.expect("listener set after bind");
                batch(vec![
                    spawn(
                        ChildDefinition::new(
                            Connection {
                                stream,
                                slow_client: self.slow_client,
                                max_broadcast_targets: self.max_broadcast_targets,
                                fanout: FanoutState::default(),
                            },
                            self.connection_capacity,
                        )
                        .with_initial_message(ConnectionMsg::Begin),
                    ),
                    tcp_close_listener(listener).then(ListenerMsg::ListenerClosed),
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
    let runtime = ThreadedRuntime::try_new(SingleShard, DefaultThreadedMailboxFactory)?;

    let slow_client = runtime
        .register_with_capacity::<_, Infallible>(SlowClient, config.slow_consumer_capacity)
        .map_err(|e| anyhow::anyhow!("register slow client: {e:?}"))?;

    // The connection mailbox absorbs one observed-reply per admitted
    // broadcast target plus a small slack for ordinary connection
    // messages. The request can ask for more; those excess targets
    // are counted as visible Full before they become effects.
    let connection_capacity = config.max_broadcast_targets.saturating_add(16);

    let bind_addr: SocketAddr = "127.0.0.1:0".parse()?;
    let listener = runtime
        .register_with_capacity::<_, Infallible>(
            Listener {
                bind_addr,
                slow_client,
                connection_capacity,
                max_broadcast_targets: config.max_broadcast_targets,
                listener: None,
            },
            8,
        )
        .map_err(|e| anyhow::anyhow!("register listener: {e:?}"))?;

    let bound = runtime.observe_next_bound()?;
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

    runtime.shutdown_report().ensure_clean()?;

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

fn counts_from_report(
    report: &BroadcastReport<usize>,
    pre_shed_full: usize,
) -> (usize, usize, usize) {
    (
        report.accepted(),
        report.full().saturating_add(pre_shed_full),
        report.closed(),
    )
}

fn write_counts(
    stream: StreamId,
    accepted: usize,
    full: usize,
    closed: usize,
) -> Effect<Connection> {
    let response = format!("accepted={accepted} full={full} closed={closed}\n").into_bytes();
    tcp_write(stream, response).then(ConnectionMsg::Wrote)
}
