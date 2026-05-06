use std::collections::{BTreeMap, VecDeque};
use std::convert::Infallible;
use std::net::SocketAddr;
use std::thread;
use std::time::{Duration, Instant};

use tina::prelude::*;
use tina_runtime::{
    CallError, CallOutcome, DefaultThreadedMailboxFactory, ListenerId, StreamId, ThreadedRuntime,
    ThreadedRuntimeConfig, call, stable_trace_hash, tcp_accept, tcp_bind, tcp_close_listener,
    tcp_close_stream, tcp_read, tcp_write,
};

use super::{Command, report_from_response, scripted_client};

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

#[tina_runtime::isolate(
    message = StoreMsg,
    reply = StoreReply,
)]
impl Store {
    fn handle(&mut self, msg: StoreMsg, _ctx: &mut Context<'_, SingleShard>) -> Effect<Self> {
        match msg {
            StoreMsg::Set { key, value } => {
                self.values.insert(key, value);
                reply(StoreReply::Ok)
            }
            StoreMsg::Get { key } => match self.values.get(&key) {
                Some(value) => reply(StoreReply::Value(value.clone())),
                None => reply(StoreReply::Miss),
            },
            StoreMsg::Del { key } => {
                if self.values.remove(&key).is_some() {
                    reply(StoreReply::Deleted)
                } else {
                    reply(StoreReply::Miss)
                }
            }
        }
    }
}

#[derive(Debug, Clone)]
enum ConnectionMsg {
    Begin,
    Read(Vec<u8>),
    StoreReturned(CallOutcome<StoreReply>),
    Wrote(usize),
    Closed,
    IoFailed,
}

#[derive(Debug)]
struct Connection {
    stream: StreamId,
    store: Address<StoreMsg, StoreReply>,
    commands: VecDeque<Command>,
    response: Vec<u8>,
}

#[tina_runtime::isolate(message = ConnectionMsg)]
impl Connection {
    fn handle(
        &mut self,
        msg: ConnectionMsg,
        _ctx: &mut Context<'_, SingleShard>,
    ) -> Effect<Self> {
        match msg {
            ConnectionMsg::Begin => tcp_read(self.stream, 4096).reply(|result| match result {
                Ok(bytes) => ConnectionMsg::Read(bytes),
                Err(_) => ConnectionMsg::IoFailed,
            }),
            ConnectionMsg::Read(bytes) => {
                self.commands = super::parse_commands(&bytes).into();
                self.next_effect()
            }
            ConnectionMsg::StoreReturned(outcome) => {
                self.push_store_response(outcome);
                self.next_effect()
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

impl Connection {
    fn next_effect(&mut self) -> Effect<Self> {
        let Some(command) = self.commands.pop_front() else {
            return tcp_write(self.stream, self.response.clone()).reply(|result| match result {
                Ok(count) => ConnectionMsg::Wrote(count),
                Err(_) => ConnectionMsg::IoFailed,
            });
        };

        match command {
            Command::Set { key, value } => call(
                self.store,
                StoreMsg::Set { key, value },
                Duration::from_millis(100),
            )
            .reply(ConnectionMsg::StoreReturned),
            Command::Get { key } => call(
                self.store,
                StoreMsg::Get { key },
                Duration::from_millis(100),
            )
            .reply(ConnectionMsg::StoreReturned),
            Command::Del { key } => call(
                self.store,
                StoreMsg::Del { key },
                Duration::from_millis(100),
            )
            .reply(ConnectionMsg::StoreReturned),
            Command::Quit => {
                self.response.extend_from_slice(b"BYE\n");
                self.next_effect()
            }
        }
    }

    fn push_store_response(&mut self, outcome: CallOutcome<StoreReply>) {
        match outcome.into_result() {
            Ok(StoreReply::Ok) => self.response.extend_from_slice(b"OK\n"),
            Ok(StoreReply::Value(value)) => self
                .response
                .extend_from_slice(format!("VALUE {value}\n").as_bytes()),
            Ok(StoreReply::Miss) => self.response.extend_from_slice(b"MISS\n"),
            Ok(StoreReply::Deleted) => self.response.extend_from_slice(b"DELETED\n"),
            Err(error) => self
                .response
                .extend_from_slice(format!("ERR {error:?}\n").as_bytes()),
        }
    }
}

#[derive(Debug, Clone)]
enum ListenerMsg {
    Start,
    Bound(Result<(ListenerId, SocketAddr), CallError>),
    Accepted(Result<(StreamId, SocketAddr), CallError>),
    ListenerClosed(Result<(), CallError>),
}

#[derive(Debug)]
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
                                store: self.store,
                                commands: VecDeque::new(),
                                response: Vec::new(),
                            },
                            16,
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

pub(crate) fn run() -> super::SideReport {
    let bind_addr: SocketAddr = "127.0.0.1:0".parse().expect("loopback parse");
    let runtime = ThreadedRuntime::with_config(
        SingleShard,
        DefaultThreadedMailboxFactory,
        ThreadedRuntimeConfig {
            command_capacity: 16,
            idle_wait: Duration::from_millis(1),
            ..Default::default()
        },
    );

    let store = runtime
        .register_with_capacity::<Store, Infallible>(Store::default(), 16)
        .expect("register store");
    let listener = runtime
        .register_with_capacity::<Listener, Infallible>(
            Listener {
                bind_addr,
                store,
                listener: None,
            },
            8,
        )
        .expect("register listener");

    let bound = runtime.observe_next_bound();
    runtime
        .try_send(listener, ListenerMsg::Start)
        .expect("start listener");
    let addr = bound
        .wait(Duration::from_secs(3))
        .expect("listener bound address");
    let client = thread::spawn(move || scripted_client(addr));

    // The client thread reads until EOF, which only happens after the
    // server's spawned Connection isolate closes the stream. Joining the
    // client is sufficient — no separate trace polling for `TcpStreamClose`
    // is needed.
    let response = client.join().expect("client thread");
    let trace = runtime.shutdown().expect("runtime shutdown");

    let fingerprint = stable_trace_hash(trace.iter());
    debug_assert!(fingerprint != 0, "trace fingerprint should be populated");

    report_from_response(response)
}
