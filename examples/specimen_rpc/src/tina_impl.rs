//! Tina framed RPC, typed server via the `#[service]` macro.
//!
//! `Connection::tiny_pressure()` enforces `max_in_flight = 1` per
//! connection. The first request grabs the slot and gets a `Reply`;
//! the next N-1 arrive while that one is in flight and come back as
//! wire `Error(Full)` — overload becomes a frame, not a stuck queue.
//! The Tina side prints `ok=1 full=N-1 other=0`.

use std::convert::Infallible;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::thread;
use std::time::Duration;

use tina::prelude::*;
use tina_rpc::{
    CloseReason, Connection, ConnectionConfig, ConnectionInit, ConnectionMsg, Encoding, Frame,
    FrameError, FrameKind, FrameLimits, Json, LENGTH_PREFIX_SIZE, PayloadLimits, Registry,
    RegistryMsg, RouterReply, SingleService, decode_body, encode, parse_length_prefix, service,
};
use tina_runtime::{
    DefaultThreadedMailboxFactory, ListenerId, LocalSystem, TcpAcceptReply, TcpBindReply,
    TcpListenerCloseReply, tcp_accept, tcp_bind, tcp_close_listener,
};

use crate::{
    ClientTerminal, ListenerTerminal, MAX_BURST, MAX_REQUEST_BYTES, Report, RunConfig,
    UnexpectedFrame,
};

const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(5);

// -------------------------------------------------------------------
// Typed echo service. The `#[service]` macro emits a dispatcher and
// JSON encode/decode for each method — the server-side service body
// is just the user's `impl`.
// -------------------------------------------------------------------

#[service]
trait Echo {
    // `payload` is reserved by the generated `ping_request` constructor, so
    // the method argument is named `body`.
    fn ping(&mut self, body: Vec<u8>) -> Vec<u8>;
}

struct EchoState;

impl Echo for EchoState {
    fn ping(&mut self, body: Vec<u8>) -> Vec<u8> {
        body
    }
}

// -------------------------------------------------------------------
// Listener: bind, accept once, spawn one Connection isolate with
// `tiny_pressure` (max_in_flight = 1), then close the listener.
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
    router: Address<RegistryMsg, RouterReply>,
    watcher: Address<CloseReason>,
    listener_id: Option<ListenerId>,
}

#[tina_runtime::isolate(
    message = ListenerMsg,
    spawn = ChildDefinition<Connection<SingleShard>>,
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
                self.listener_id = Some(listener);
                tcp_accept(listener).then(ListenerMsg::Accepted)
            }
            ListenerMsg::Accepted(Ok((stream, _peer_addr))) => {
                let Some(listener) = self.listener_id else {
                    return stop_with(ListenerTerminal::MissingListener);
                };
                let connection = Connection::<SingleShard>::new(
                    ConnectionInit::new(stream, self.router)
                        .with_config(ConnectionConfig::tiny_pressure())
                        .with_watcher(self.watcher),
                );
                batch(vec![
                    spawn(
                        ChildDefinition::new(connection, 64)
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

struct ConnectionWatcher;

#[tina_runtime::isolate(message = CloseReason)]
impl ConnectionWatcher {
    fn handle(
        &mut self,
        reason: CloseReason,
        _ctx: &mut Context<'_, SingleShard, Self::Reply>,
    ) -> Effect<Self> {
        stop_with(reason)
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
    // 1. Typed dispatch: the `#[service]` macro emits `EchoService`
    //    with a JSON-tuple decoder for each method's args and a JSON
    //    encoder for each return type.
    let dispatch =
        EchoService::dispatch::<EchoState, SingleShard>(EchoState, PayloadLimits::default());
    let service = app
        .register_root::<_, Infallible>(SingleService::new(dispatch), 16)
        .map_err(|e| anyhow::anyhow!("register service: {e:?}"))?;

    // 2. Registry mapping wire service name to the dispatch isolate.
    let registry_state = Registry::<SingleShard>::builder()
        .service("echo", service)
        .build();
    let registry = app
        .register_root::<_, Infallible>(registry_state, 16)
        .map_err(|e| anyhow::anyhow!("register registry: {e:?}"))?;

    let watcher = app
        .register_root::<_, Infallible>(ConnectionWatcher, 1)
        .map_err(|e| anyhow::anyhow!("register connection watcher: {e:?}"))?;
    let connection_result = app
        .observe_result::<CloseReason, _, _>(watcher)
        .map_err(|error| anyhow::anyhow!("observe connection result: {error:?}"))?;

    // 3. Listener that binds, accepts once, and spawns the Connection
    //    isolate that enforces the in-flight cap on the wire.
    let bind_addr: SocketAddr = "127.0.0.1:0".parse()?;
    let listener = app
        .register_root::<_, Infallible>(
            Listener {
                bind_addr,
                router: registry,
                watcher,
                listener_id: None,
            },
            8,
        )
        .map_err(|e| anyhow::anyhow!("register listener: {e:?}"))?;

    // Register the bound-address waiter *before* triggering the bind so
    // the registration lands in the runtime's command queue ahead of
    // the bind completion.
    let bound = app.observe_next_bound()?;
    let listener_result = app
        .observe_result::<ListenerTerminal, _, _>(listener)
        .map_err(|error| anyhow::anyhow!("observe listener result: {error:?}"))?;
    app.try_send(listener, ListenerMsg::Start)
        .map_err(|e| anyhow::anyhow!("start listener: {e:?}"))?;
    let addr = bound
        .wait(Duration::from_secs(3))
        .map_err(|e| anyhow::anyhow!("listener bind: {e:?}"))?;

    // 4. Drive the burst from a blocking std::net client. The client
    //    encodes args as a JSON tuple `[<payload_bytes>]` to match the
    //    macro's decoder.
    let burst = config.burst;
    let mut report = thread::spawn(move || drive_client(addr, burst))
        .join()
        .map_err(|_| anyhow::anyhow!("client thread panicked"))??;

    report.listener_terminal = Some(
        listener_result
            .wait(Duration::from_secs(3))
            .map_err(|error| anyhow::anyhow!("listener result: {error:?}"))?,
    );
    report.connection_terminal = Some(
        connection_result
            .wait(Duration::from_secs(3))
            .map_err(|error| anyhow::anyhow!("connection result: {error:?}"))?,
    );
    Ok(report)
}

/// Open one TCP connection, write the whole burst, read replies,
/// classify into a `Report`.
fn drive_client(addr: SocketAddr, burst: usize) -> anyhow::Result<Report> {
    if burst == 0 || burst > MAX_BURST {
        anyhow::bail!("burst {burst} is outside 1..={MAX_BURST}");
    }
    let mut stream = TcpStream::connect(addr)?;
    stream.set_read_timeout(Some(Duration::from_secs(5)))?;
    stream.set_write_timeout(Some(Duration::from_secs(5)))?;

    // The macro's args decoder for `fn ping(body: Vec<u8>)` is
    // `(Vec<u8>,)` — a JSON array with one element. Encode it via
    // the same `Encoding::encode` the macro uses on the server, so
    // there's no separate "what does the wire expect" question.
    let payload = Json.encode(&(b"hi".to_vec(),), 1024)?;

    let limits = FrameLimits::default();
    let mut request_bytes = Vec::new();
    for id in 1..=burst as u64 {
        let frame = Frame::request(id, "echo", "ping", payload.clone());
        let encoded = encode(&frame, &limits)?;
        let next_len = request_bytes
            .len()
            .checked_add(encoded.len())
            .ok_or_else(|| anyhow::anyhow!("request burst byte length overflow"))?;
        anyhow::ensure!(
            next_len <= MAX_REQUEST_BYTES,
            "encoded request burst exceeds {MAX_REQUEST_BYTES} bytes"
        );
        request_bytes.extend(encoded);
    }
    stream.write_all(&request_bytes)?;

    let mut report = Report::default();
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    while report.total() < burst {
        match stream.read(&mut chunk) {
            Ok(0) => {
                report.client_terminal = Some(ClientTerminal::Eof);
                report.other += burst - report.total();
                return Ok(report);
            }
            Err(error) => {
                report.client_terminal = Some(ClientTerminal::Read(error.kind()));
                report.other += burst - report.total();
                return Ok(report);
            }
            Ok(n) => buf.extend_from_slice(&chunk[..n]),
        }
        loop {
            if buf.len() < LENGTH_PREFIX_SIZE {
                break;
            }
            let mut prefix = [0u8; LENGTH_PREFIX_SIZE];
            prefix.copy_from_slice(&buf[..LENGTH_PREFIX_SIZE]);
            let body_len = match parse_length_prefix(prefix, &limits) {
                Ok(len) => len,
                Err(error) => {
                    report.decode_errors.push(error);
                    report.other += burst - report.total();
                    return Ok(report);
                }
            };
            let total = LENGTH_PREFIX_SIZE + body_len;
            if buf.len() < total {
                break;
            }
            let body = buf[LENGTH_PREFIX_SIZE..total].to_vec();
            buf.drain(..total);
            match decode_body(&body) {
                Ok(frame) => match (frame.kind, frame.error) {
                    (FrameKind::Reply, _) => report.ok += 1,
                    (FrameKind::Error, Some(FrameError::Full)) => {
                        report.full += 1;
                        report.wire_errors.full += 1;
                    }
                    (FrameKind::Error, Some(FrameError::UnknownService)) => {
                        report.other += 1;
                        report.wire_errors.unknown_service += 1;
                    }
                    (FrameKind::Error, Some(FrameError::UnknownMethod)) => {
                        report.other += 1;
                        report.wire_errors.unknown_method += 1;
                    }
                    (FrameKind::Error, Some(FrameError::Decode)) => {
                        report.other += 1;
                        report.wire_errors.decode += 1;
                    }
                    (FrameKind::Error, Some(FrameError::Protocol)) => {
                        report.other += 1;
                        report.wire_errors.protocol += 1;
                    }
                    (FrameKind::Error, Some(FrameError::Internal)) => {
                        report.other += 1;
                        report.wire_errors.internal += 1;
                    }
                    (kind, error) => {
                        report.other += 1;
                        report
                            .unexpected_frames
                            .push(UnexpectedFrame { kind, error });
                    }
                },
                Err(error) => {
                    report.decode_errors.push(error);
                    report.other += 1;
                }
            }
        }
    }
    Ok(report)
}
