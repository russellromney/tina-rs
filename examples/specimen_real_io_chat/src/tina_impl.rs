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
    BroadcastAssertError, BroadcastRecordError, BroadcastReport, BroadcastTargets,
    BroadcastTargetsError, BroadcastTracker, DefaultThreadedMailboxFactory, ListenerId,
    LocalSystem, SendOutcome, StreamId, TcpAcceptReply, TcpBindReply, TcpListenerCloseReply,
    TcpReadReply, TcpStreamCloseReply, TcpWriteReply, broadcast_observed, tcp_accept, tcp_bind,
    tcp_close_listener, tcp_close_stream, tcp_read, tcp_write,
};

use crate::{MAX_BURST, Report, RunConfig};

const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_BURST_REQUEST_BYTES: usize = 32;

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
    ) -> Result<BroadcastTargets<usize, M>, BroadcastTargetsError>
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
    ) -> Result<Option<(usize, usize, usize)>, FanoutProtocolError> {
        let tracker = self
            .tracker
            .as_mut()
            .ok_or(FanoutProtocolError::MissingTracker)?;
        let Some(report) = tracker
            .record(key, outcome)
            .map_err(FanoutProtocolError::Record)?
        else {
            return Ok(None);
        };
        report
            .assert_all_accounted_for(report.outcomes().len())
            .map_err(FanoutProtocolError::Assert)?;
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
    request_bytes: Vec<u8>,
    pending_write: Vec<u8>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BurstProtocolError {
    InvalidUtf8,
    Empty,
    InvalidInteger,
    Zero,
    TooLarge,
    RequestTooLong,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum ConnectionTerminal {
    ClosedClean,
    Protocol(BurstProtocolError),
    FanoutSetup(BroadcastTargetsError),
    FanoutProtocol(FanoutProtocolError),
    ReadFailed(tina_runtime::CallError),
    WriteFailed(tina_runtime::CallError),
    InvalidWriteCount { pending: usize, written: usize },
    CloseFailed(tina_runtime::CallError),
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum FanoutProtocolError {
    MissingTracker,
    Record(BroadcastRecordError<usize>),
    Assert(BroadcastAssertError),
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
            ConnectionMsg::Read(Ok(bytes)) if !bytes.is_empty() => {
                if let Err(error) = append_request_bytes(&mut self.request_bytes, &bytes) {
                    return stop_with(ConnectionTerminal::Protocol(error));
                }
                tcp_read(self.stream, MAX_BURST_REQUEST_BYTES).then(ConnectionMsg::Read)
            }
            ConnectionMsg::Read(Ok(_)) => {
                let requested_burst = match parse_burst(&self.request_bytes) {
                    Ok(burst) => burst,
                    Err(error) => return stop_with(ConnectionTerminal::Protocol(error)),
                };
                let targets = match self.fanout.start(
                    requested_burst,
                    self.max_broadcast_targets,
                    self.slow_client,
                ) {
                    Ok(targets) => targets,
                    Err(error) => return stop_with(ConnectionTerminal::FanoutSetup(error)),
                };
                if targets.is_empty() {
                    return self.write_counts(0, self.fanout.pre_shed_full, 0);
                }
                broadcast_observed(targets, |index| DeliverMsg(*index), ConnectionMsg::Observed)
            }
            ConnectionMsg::Observed(key, outcome) => {
                let counts = match self.fanout.record(key, outcome) {
                    Ok(Some(counts)) => counts,
                    Ok(None) => return noop(),
                    Err(error) => return stop_with(ConnectionTerminal::FanoutProtocol(error)),
                };
                self.write_counts(counts.0, counts.1, counts.2)
            }
            ConnectionMsg::Wrote(Ok(written)) => {
                let pending = self.pending_write.len();
                if written == 0 || written > pending {
                    return stop_with(ConnectionTerminal::InvalidWriteCount { pending, written });
                }
                self.pending_write.drain(..written);
                if self.pending_write.is_empty() {
                    tcp_close_stream(self.stream).then(ConnectionMsg::Closed)
                } else {
                    self.write_pending()
                }
            }
            ConnectionMsg::Closed(Ok(())) => stop_with(ConnectionTerminal::ClosedClean),
            ConnectionMsg::Read(Err(error)) => stop_with(ConnectionTerminal::ReadFailed(error)),
            ConnectionMsg::Wrote(Err(error)) => stop_with(ConnectionTerminal::WriteFailed(error)),
            ConnectionMsg::Closed(Err(error)) => stop_with(ConnectionTerminal::CloseFailed(error)),
        }
    }
}

impl Connection {
    fn write_counts(&mut self, accepted: usize, full: usize, closed: usize) -> Effect<Self> {
        self.pending_write =
            format!("accepted={accepted} full={full} closed={closed}\n").into_bytes();
        self.write_pending()
    }

    fn write_pending(&self) -> Effect<Self> {
        tcp_write(self.stream, self.pending_write.clone()).then(ConnectionMsg::Wrote)
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ListenerTerminal {
    ClosedClean,
    BindFailed(tina_runtime::CallError),
    AcceptFailed(tina_runtime::CallError),
    CloseFailed(tina_runtime::CallError),
    MissingListener,
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
                let Some(listener) = self.listener else {
                    return stop_with(ListenerTerminal::MissingListener);
                };
                batch(vec![
                    spawn(
                        ChildDefinition::new(
                            Connection {
                                stream,
                                slow_client: self.slow_client,
                                max_broadcast_targets: self.max_broadcast_targets,
                                fanout: FanoutState::default(),
                                request_bytes: Vec::with_capacity(MAX_BURST_REQUEST_BYTES),
                                pending_write: Vec::new(),
                            },
                            self.connection_capacity,
                        )
                        .with_initial_message(ConnectionMsg::Begin),
                    ),
                    tcp_close_listener(listener).then(ListenerMsg::ListenerClosed),
                ])
            }
            ListenerMsg::ListenerClosed(Ok(())) => stop_with(ListenerTerminal::ClosedClean),
            ListenerMsg::Bound(Err(error)) => stop_with(ListenerTerminal::BindFailed(error)),
            ListenerMsg::Accepted(Err(error)) => stop_with(ListenerTerminal::AcceptFailed(error)),
            ListenerMsg::ListenerClosed(Err(error)) => {
                stop_with(ListenerTerminal::CloseFailed(error))
            }
        }
    }
}

// -------------------------------------------------------------------
// Run
// -------------------------------------------------------------------

pub fn run(config: RunConfig) -> anyhow::Result<Report> {
    let config = config.validate()?;
    let app = LocalSystem::single_shard(SingleShard, DefaultThreadedMailboxFactory).try_build()?;
    Ok(app.run_to_shutdown_reported(SHUTDOWN_TIMEOUT, move |app| run_application(app, config))?)
}

fn run_application(
    app: &LocalSystem<SingleShard, DefaultThreadedMailboxFactory>,
    config: RunConfig,
) -> anyhow::Result<Report> {
    let slow_client = app
        .register_root::<_, Infallible>(SlowClient, config.slow_consumer_capacity)
        .map_err(|e| anyhow::anyhow!("register slow client: {e:?}"))?;

    // The connection mailbox absorbs one observed-reply per admitted
    // broadcast target plus a small slack for ordinary connection
    // messages. The request can ask for more; those excess targets
    // are counted as visible Full before they become effects.
    let connection_capacity = config
        .max_broadcast_targets
        .checked_add(16)
        .ok_or_else(|| anyhow::anyhow!("connection mailbox capacity overflow"))?;

    let bind_addr: SocketAddr = "127.0.0.1:0".parse()?;
    let listener = app
        .register_root::<_, Infallible>(
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

    let listener_result = app
        .observe_result::<ListenerTerminal, _, _>(listener)
        .map_err(|error| anyhow::anyhow!("observe listener result: {error:?}"))?;
    let bound = app.observe_next_bound()?;
    app.try_send(listener, ListenerMsg::Start)
        .map_err(|e| anyhow::anyhow!("start listener: {e:?}"))?;
    let addr = bound
        .wait(Duration::from_secs(3))
        .map_err(|e| anyhow::anyhow!("listener bind: {e:?}"))?;

    let burst = config.burst;
    let response = thread::spawn(move || drive_client(addr, burst))
        .join()
        .map_err(|_| anyhow::anyhow!("client thread panicked"))??;
    let listener_terminal = listener_result
        .wait(Duration::from_secs(3))
        .map_err(|error| anyhow::anyhow!("listener terminal: {error:?}"))?;
    anyhow::ensure!(
        listener_terminal == ListenerTerminal::ClosedClean,
        "listener did not close cleanly: {listener_terminal:?}"
    );

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

fn parse_burst(bytes: &[u8]) -> Result<usize, BurstProtocolError> {
    let text = std::str::from_utf8(bytes).map_err(|_| BurstProtocolError::InvalidUtf8)?;
    let text = text.trim();
    if text.is_empty() {
        return Err(BurstProtocolError::Empty);
    }
    let burst = text
        .parse::<usize>()
        .map_err(|_| BurstProtocolError::InvalidInteger)?;
    if burst == 0 {
        return Err(BurstProtocolError::Zero);
    }
    if burst > MAX_BURST {
        return Err(BurstProtocolError::TooLarge);
    }
    Ok(burst)
}

fn append_request_bytes(request: &mut Vec<u8>, chunk: &[u8]) -> Result<(), BurstProtocolError> {
    let next_len = request
        .len()
        .checked_add(chunk.len())
        .ok_or(BurstProtocolError::RequestTooLong)?;
    if next_len > MAX_BURST_REQUEST_BYTES {
        return Err(BurstProtocolError::RequestTooLong);
    }
    request.extend_from_slice(chunk);
    Ok(())
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn burst_protocol_is_exhaustive_and_bounded() {
        assert_eq!(parse_burst(b"7"), Ok(7));
        assert_eq!(parse_burst(b"\xff"), Err(BurstProtocolError::InvalidUtf8));
        assert_eq!(parse_burst(b"  "), Err(BurstProtocolError::Empty));
        assert_eq!(
            parse_burst(b"nope"),
            Err(BurstProtocolError::InvalidInteger)
        );
        assert_eq!(parse_burst(b"0"), Err(BurstProtocolError::Zero));
        assert_eq!(
            parse_burst((MAX_BURST + 1).to_string().as_bytes()),
            Err(BurstProtocolError::TooLarge)
        );
        let mut fragmented = Vec::new();
        append_request_bytes(&mut fragmented, b"12").expect("first fragment");
        append_request_bytes(&mut fragmented, b"34").expect("second fragment");
        assert_eq!(parse_burst(&fragmented), Ok(1234));
        assert_eq!(
            append_request_bytes(&mut fragmented, &[b'1'; MAX_BURST_REQUEST_BYTES]),
            Err(BurstProtocolError::RequestTooLong)
        );
    }
}
