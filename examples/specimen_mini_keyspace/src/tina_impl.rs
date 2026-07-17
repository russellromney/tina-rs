//! Tina: real loopback TCP. The `Store` isolate genuinely owns its
//! `BTreeMap`. The `Connection` isolate is an explicit state machine
//! that walks `Begin → Read → (StoreReturned)* → Wrote → Closed`.
//! `call(addr, msg, timeout).then(continuation)` is how each script
//! command crosses the boundary into the store and back.

use std::collections::{BTreeMap, VecDeque};
use std::convert::Infallible;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::thread;
use std::time::Duration;

use tina::prelude::*;
use tina_runtime::{
    CallOutcome, DefaultThreadedMailboxFactory, ListenerId, LocalSystem, StreamId, TcpAcceptReply,
    TcpBindReply, TcpListenerCloseReply, TcpReadReply, TcpStreamCloseReply, TcpWriteReply, call,
    tcp_accept, tcp_bind, tcp_close_listener, tcp_close_stream, tcp_read, tcp_write,
};

use crate::{Command, Report, SCRIPT, parse_commands};

// -------------------------------------------------------------------
// Store: owns the `BTreeMap`. No shared mutable state — the type
// system guarantees the only way to read or change `values` is to
// send the isolate a message.
// -------------------------------------------------------------------

#[derive(Debug, Clone)]
enum StoreMsg {
    Set { key: String, value: String },
    Get { key: String },
    Del { key: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum StoreReply {
    Ok,
    Value(String),
    Miss,
    Deleted,
}

#[derive(Debug, Default)]
struct Store {
    values: BTreeMap<String, String>,
}

#[tina_runtime::isolate(message = StoreMsg, reply = StoreReply)]
impl Store {
    fn handle(
        &mut self,
        msg: StoreMsg,
        _ctx: &mut Context<'_, SingleShard, Self::Reply>,
    ) -> Effect<Self> {
        reply(self.apply(msg))
    }

    fn handle_call(&mut self, msg: StoreMsg, call: tina::CallContext<'_, Self>) -> Effect<Self> {
        call.reply(self.apply(msg))
    }
}

impl Store {
    fn apply(&mut self, msg: StoreMsg) -> StoreReply {
        match msg {
            StoreMsg::Set { key, value } => {
                self.values.insert(key, value);
                StoreReply::Ok
            }
            StoreMsg::Get { key } => match self.values.get(&key) {
                Some(value) => StoreReply::Value(value.clone()),
                None => StoreReply::Miss,
            },
            StoreMsg::Del { key } => {
                if self.values.remove(&key).is_some() {
                    StoreReply::Deleted
                } else {
                    StoreReply::Miss
                }
            }
        }
    }
}

// -------------------------------------------------------------------
// Connection: read script, walk commands one at a time through the
// store, accumulate the response, write it, close.
// -------------------------------------------------------------------

#[derive(Debug, Clone)]
enum ConnectionMsg {
    Begin,
    Read(TcpReadReply),
    StoreReturned(CallOutcome<StoreReply>),
    Wrote(TcpWriteReply),
    Closed(TcpStreamCloseReply),
}

/// State that's empty at spawn. Lives in its own struct so the spawn
/// site reads `state: ConnState::default()` instead of zeroing every
/// field by hand.
#[derive(Debug, Default)]
struct ConnState {
    commands: VecDeque<Command>,
    response: Vec<u8>,
}

struct Connection {
    stream: StreamId,
    store: Address<StoreMsg, StoreReply>,
    state: ConnState,
}

#[tina_runtime::isolate(message = ConnectionMsg)]
impl Connection {
    fn handle(
        &mut self,
        msg: ConnectionMsg,
        _ctx: &mut Context<'_, SingleShard, Self::Reply>,
    ) -> Effect<Self> {
        match msg {
            ConnectionMsg::Begin => tcp_read(self.stream, 4096).then(ConnectionMsg::Read),
            ConnectionMsg::Read(Ok(bytes)) => {
                self.state.commands = parse_commands(&bytes).expect("script parses").into();
                self.next_effect()
            }
            ConnectionMsg::StoreReturned(outcome) => {
                self.push_store_response(outcome);
                self.next_effect()
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

impl Connection {
    /// Pop the next command off the queue and either issue the next
    /// `call(...)` to the store or — if the queue is empty — write
    /// the accumulated response to the wire.
    ///
    /// Recursion via self-sent messages or a method like this is the
    /// current way to express "process a list of things" — there is
    /// no built-in iteration combinator yet (`FINDINGS.md` tracks it
    /// under continuation/pipeline sugar).
    fn next_effect(&mut self) -> Effect<Self> {
        let Some(command) = self.state.commands.pop_front() else {
            return tcp_write(self.stream, std::mem::take(&mut self.state.response))
                .then(ConnectionMsg::Wrote);
        };
        match command {
            Command::Set { key, value } => call(
                self.store,
                StoreMsg::Set { key, value },
                Duration::from_millis(100),
            )
            .then(ConnectionMsg::StoreReturned),
            Command::Get { key } => call(
                self.store,
                StoreMsg::Get { key },
                Duration::from_millis(100),
            )
            .then(ConnectionMsg::StoreReturned),
            Command::Del { key } => call(
                self.store,
                StoreMsg::Del { key },
                Duration::from_millis(100),
            )
            .then(ConnectionMsg::StoreReturned),
            Command::Quit => {
                self.state.response.extend_from_slice(b"BYE\n");
                self.next_effect()
            }
        }
    }

    fn push_store_response(&mut self, outcome: CallOutcome<StoreReply>) {
        match outcome.into_result() {
            Ok(StoreReply::Ok) => self.state.response.extend_from_slice(b"OK\n"),
            Ok(StoreReply::Value(value)) => self
                .state
                .response
                .extend_from_slice(format!("VALUE {value}\n").as_bytes()),
            Ok(StoreReply::Miss) => self.state.response.extend_from_slice(b"MISS\n"),
            Ok(StoreReply::Deleted) => self.state.response.extend_from_slice(b"DELETED\n"),
            Err(error) => self
                .state
                .response
                .extend_from_slice(format!("ERR {error:?}\n").as_bytes()),
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
    store: Address<StoreMsg, StoreReply>,
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
                                store: self.store,
                                state: ConnState::default(),
                            },
                            16,
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

pub fn run() -> anyhow::Result<Report> {
    let app = LocalSystem::single_shard(SingleShard, DefaultThreadedMailboxFactory).try_build()?;
    Ok(app.run_to_shutdown_reported(Duration::from_secs(5), run_application)?)
}

fn run_application(
    app: &LocalSystem<SingleShard, DefaultThreadedMailboxFactory>,
) -> anyhow::Result<Report> {
    let store = app
        .register_root::<_, Infallible>(Store::default(), 16)
        .map_err(|e| anyhow::anyhow!("register store: {e:?}"))?;
    let listener = app
        .register_root::<_, Infallible>(
            Listener {
                bind_addr: "127.0.0.1:0".parse()?,
                store,
                listener: None,
            },
            8,
        )
        .map_err(|e| anyhow::anyhow!("register listener: {e:?}"))?;

    let bound = app.observe_next_bound()?;
    app.try_send(listener, ListenerMsg::Start)
        .map_err(|e| anyhow::anyhow!("start listener: {e:?}"))?;
    let addr = bound
        .wait(Duration::from_secs(3))
        .map_err(|e| anyhow::anyhow!("listener bind: {e:?}"))?;

    let response = thread::spawn(move || drive_client(addr))
        .join()
        .map_err(|_| anyhow::anyhow!("client thread panicked"))??;

    Ok(parse_response(&response))
}

/// Connect, write the script, shut down the write side, read the
/// response to EOF.
fn drive_client(addr: SocketAddr) -> anyhow::Result<Vec<u8>> {
    let mut stream = TcpStream::connect(addr)?;
    stream.set_read_timeout(Some(Duration::from_secs(3)))?;
    stream.write_all(SCRIPT.as_bytes())?;
    stream.shutdown(std::net::Shutdown::Write)?;
    let mut response = Vec::new();
    stream.read_to_end(&mut response)?;
    Ok(response)
}

fn parse_response(bytes: &[u8]) -> Report {
    let text = std::str::from_utf8(bytes).expect("response utf8");
    let mut report = Report::default();
    for line in text.lines() {
        if line == "OK" {
            report.ok += 1;
        } else if line.starts_with("VALUE ") {
            report.values += 1;
        } else if line == "MISS" {
            report.misses += 1;
        } else if line == "DELETED" {
            report.deleted += 1;
        }
    }
    report
}
