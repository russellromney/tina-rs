use std::cell::RefCell;
use std::convert::Infallible;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tina::{
    Address, Context, Effect, Isolate, IsolateId, Outbound, RestartBudget, RestartPolicy,
    RestartableChildDefinition, Shard, ShardId, batch,
};
use tina_runtime::{
    CallCompletionRejectedReason, CallError, CallInput, CallKind, CallOutput, FileId,
    FileOpenOptions, ListenerId, ProcessRunResult, RuntimeCall, RuntimeEvent, RuntimeEventKind,
    StreamId, UdpSocketId, dns_lookup, process_run, udp_bind, udp_close_socket, udp_recv_from,
    udp_send_to,
};
use tina_sim::{
    Checker, CheckerDecision, FaultConfig, ObservedPeerOutput, ReplayArtifact, ScriptedDnsConfig,
    ScriptedDnsLookupConfig, ScriptedDnsResult, ScriptedListenerConfig, ScriptedPeerConfig,
    ScriptedProcessConfig, ScriptedProcessResult, ScriptedProcessRunConfig, ScriptedTcpConfig,
    ScriptedUdpConfig, ScriptedUdpDatagramConfig, ScriptedUdpSocketConfig, Simulator,
    SimulatorConfig, TcpCompletionFaultMode,
    dst::{DstRun, History, InvariantSuite, ShrinkConfig, assert_replays, delete_shrink},
};
use tina_supervisor::SupervisorConfig;

type FileObserved = Arc<Mutex<Option<(u64, Vec<u8>)>>>;

#[derive(Debug, Default)]
struct TestShard;

impl Shard for TestShard {
    fn id(&self) -> ShardId {
        ShardId::new(81)
    }
}

#[derive(Debug, Clone)]
enum ConnectionMsg {
    Start,
    ReadCompleted(Vec<u8>),
    WriteCompleted { count: usize },
    StreamClosed,
    Failed,
}

#[derive(Debug)]
struct Connection {
    stream: StreamId,
    max_chunk: usize,
    pending_write: Vec<u8>,
}

impl Isolate for Connection {
    type Message = ConnectionMsg;
    type Reply = ();
    type Send = Outbound<Infallible>;
    type Spawn = Infallible;
    type Call = RuntimeCall<ConnectionMsg>;
    type Shard = TestShard;

    fn handle(&mut self, msg: Self::Message, _ctx: &mut Context<'_, Self::Shard>) -> Effect<Self> {
        match msg {
            ConnectionMsg::Start => read_call(self.stream, self.max_chunk),
            ConnectionMsg::ReadCompleted(bytes) => {
                if bytes.is_empty() {
                    close_call(self.stream)
                } else {
                    self.pending_write = bytes;
                    write_call(self.stream, self.pending_write.clone())
                }
            }
            ConnectionMsg::WriteCompleted { count } => {
                if count >= self.pending_write.len() {
                    self.pending_write.clear();
                    read_call(self.stream, self.max_chunk)
                } else {
                    self.pending_write.drain(..count);
                    write_call(self.stream, self.pending_write.clone())
                }
            }
            ConnectionMsg::StreamClosed | ConnectionMsg::Failed => Effect::Stop,
        }
    }
}

#[derive(Debug, Clone)]
enum ClientMsg {
    Start(SocketAddr),
    Connected(Result<(StreamId, SocketAddr, SocketAddr), CallError>),
    Wrote(Result<usize, CallError>),
    Read(Result<Vec<u8>, CallError>),
    Closed(Result<(), CallError>),
}

#[derive(Debug)]
struct Client {
    payload: Vec<u8>,
    pending_write: Vec<u8>,
    stream: Option<StreamId>,
    observed: Arc<Mutex<Option<Vec<u8>>>>,
}

impl Isolate for Client {
    type Message = ClientMsg;
    type Reply = ();
    type Send = Outbound<Infallible>;
    type Spawn = Infallible;
    type Call = RuntimeCall<ClientMsg>;
    type Shard = TestShard;

    fn handle(&mut self, msg: Self::Message, _ctx: &mut Context<'_, Self::Shard>) -> Effect<Self> {
        match msg {
            ClientMsg::Start(addr) => Effect::Call(RuntimeCall::new(
                CallInput::TcpConnect { addr },
                |result| match result {
                    CallOutput::TcpConnected {
                        stream,
                        local_addr,
                        peer_addr,
                    } => ClientMsg::Connected(Ok((stream, local_addr, peer_addr))),
                    CallOutput::Failed(error) => ClientMsg::Connected(Err(error)),
                    other => panic!("unexpected connect result {other:?}"),
                },
            )),
            ClientMsg::Connected(Ok((stream, _local_addr, _peer_addr))) => {
                self.stream = Some(stream);
                self.pending_write = self.payload.clone();
                Effect::Call(RuntimeCall::new(
                    CallInput::TcpWrite {
                        stream,
                        bytes: self.pending_write.clone(),
                    },
                    |result| match result {
                        CallOutput::TcpWrote { count } => ClientMsg::Wrote(Ok(count)),
                        CallOutput::Failed(error) => ClientMsg::Wrote(Err(error)),
                        other => panic!("unexpected write result {other:?}"),
                    },
                ))
            }
            ClientMsg::Wrote(Ok(count)) => {
                let stream = self.stream.expect("stream set after connect");
                if count >= self.pending_write.len() {
                    self.pending_write.clear();
                    Effect::Call(RuntimeCall::new(
                        CallInput::TcpRead { stream, max_len: 4 },
                        |result| match result {
                            CallOutput::TcpRead { bytes } => ClientMsg::Read(Ok(bytes)),
                            CallOutput::Failed(error) => ClientMsg::Read(Err(error)),
                            other => panic!("unexpected read result {other:?}"),
                        },
                    ))
                } else {
                    self.pending_write.drain(..count);
                    Effect::Call(RuntimeCall::new(
                        CallInput::TcpWrite {
                            stream,
                            bytes: self.pending_write.clone(),
                        },
                        |result| match result {
                            CallOutput::TcpWrote { count } => ClientMsg::Wrote(Ok(count)),
                            CallOutput::Failed(error) => ClientMsg::Wrote(Err(error)),
                            other => panic!("unexpected write result {other:?}"),
                        },
                    ))
                }
            }
            ClientMsg::Read(Ok(bytes)) => {
                *self.observed.lock().expect("observed mutex") = Some(bytes);
                let stream = self.stream.expect("stream set after connect");
                Effect::Call(RuntimeCall::new(
                    CallInput::TcpStreamClose { stream },
                    |result| match result {
                        CallOutput::TcpStreamClosed => ClientMsg::Closed(Ok(())),
                        CallOutput::Failed(error) => ClientMsg::Closed(Err(error)),
                        other => panic!("unexpected close result {other:?}"),
                    },
                ))
            }
            ClientMsg::Closed(Ok(())) => Effect::Stop,
            ClientMsg::Connected(Err(_))
            | ClientMsg::Wrote(Err(_))
            | ClientMsg::Read(Err(_))
            | ClientMsg::Closed(Err(_)) => Effect::Stop,
        }
    }
}

#[derive(Debug, Clone)]
enum FileClientMsg {
    Start { dir: PathBuf, path: PathBuf },
    DirectoryMade(Result<(), CallError>, PathBuf),
    Opened(Result<FileId, CallError>),
    Wrote(Result<usize, CallError>),
    Synced(Result<(), CallError>),
    Sized(Result<u64, CallError>),
    Read(Result<Vec<u8>, CallError>),
    Closed(Result<(), CallError>),
}

#[derive(Debug)]
struct FileClient {
    payload: Vec<u8>,
    file: Option<FileId>,
    size: Option<u64>,
    observed: FileObserved,
}

impl Isolate for FileClient {
    type Message = FileClientMsg;
    type Reply = ();
    type Send = Outbound<Infallible>;
    type Spawn = Infallible;
    type Call = RuntimeCall<FileClientMsg>;
    type Shard = TestShard;

    fn handle(&mut self, msg: Self::Message, _ctx: &mut Context<'_, Self::Shard>) -> Effect<Self> {
        match msg {
            FileClientMsg::Start { dir, path } => Effect::Call(RuntimeCall::new(
                CallInput::Mkdir {
                    path: dir,
                    mode: 0o755,
                },
                move |result| match result {
                    CallOutput::DirectoryCreated => FileClientMsg::DirectoryMade(Ok(()), path),
                    CallOutput::Failed(error) => FileClientMsg::DirectoryMade(Err(error), path),
                    other => panic!("unexpected mkdir result {other:?}"),
                },
            )),
            FileClientMsg::DirectoryMade(Ok(()), path) => Effect::Call(RuntimeCall::new(
                CallInput::FileOpen {
                    path,
                    options: FileOpenOptions {
                        read: true,
                        write: true,
                        create: true,
                        truncate: true,
                    },
                },
                |result| match result {
                    CallOutput::FileOpened { file } => FileClientMsg::Opened(Ok(file)),
                    CallOutput::Failed(error) => FileClientMsg::Opened(Err(error)),
                    other => panic!("unexpected file open result {other:?}"),
                },
            )),
            FileClientMsg::Opened(Ok(file)) => {
                self.file = Some(file);
                Effect::Call(RuntimeCall::new(
                    CallInput::FileWriteAt {
                        file,
                        bytes: self.payload.clone(),
                        offset: 0,
                    },
                    |result| match result {
                        CallOutput::FileWrote { count } => FileClientMsg::Wrote(Ok(count)),
                        CallOutput::Failed(error) => FileClientMsg::Wrote(Err(error)),
                        other => panic!("unexpected file write result {other:?}"),
                    },
                ))
            }
            FileClientMsg::Wrote(Ok(_)) => {
                let file = self.file.expect("file set after open");
                Effect::Call(RuntimeCall::new(
                    CallInput::FileFsync { file },
                    |result| match result {
                        CallOutput::FileSynced => FileClientMsg::Synced(Ok(())),
                        CallOutput::Failed(error) => FileClientMsg::Synced(Err(error)),
                        other => panic!("unexpected file fsync result {other:?}"),
                    },
                ))
            }
            FileClientMsg::Synced(Ok(())) => {
                let file = self.file.expect("file set after open");
                Effect::Call(RuntimeCall::new(
                    CallInput::FileSize { file },
                    |result| match result {
                        CallOutput::FileSize { size } => FileClientMsg::Sized(Ok(size)),
                        CallOutput::Failed(error) => FileClientMsg::Sized(Err(error)),
                        other => panic!("unexpected file size result {other:?}"),
                    },
                ))
            }
            FileClientMsg::Sized(Ok(size)) => {
                self.size = Some(size);
                let file = self.file.expect("file set after open");
                Effect::Call(RuntimeCall::new(
                    CallInput::FileReadAt {
                        file,
                        len: size as usize,
                        offset: 0,
                    },
                    |result| match result {
                        CallOutput::FileRead { bytes } => FileClientMsg::Read(Ok(bytes)),
                        CallOutput::Failed(error) => FileClientMsg::Read(Err(error)),
                        other => panic!("unexpected file read result {other:?}"),
                    },
                ))
            }
            FileClientMsg::Read(Ok(bytes)) => {
                let size = self.size.expect("size set before read");
                *self.observed.lock().expect("file observed mutex") = Some((size, bytes));
                let file = self.file.expect("file set after open");
                Effect::Call(RuntimeCall::new(
                    CallInput::FileClose { file },
                    |result| match result {
                        CallOutput::FileClosed => FileClientMsg::Closed(Ok(())),
                        CallOutput::Failed(error) => FileClientMsg::Closed(Err(error)),
                        other => panic!("unexpected file close result {other:?}"),
                    },
                ))
            }
            FileClientMsg::Closed(Ok(())) => Effect::Stop,
            FileClientMsg::DirectoryMade(Err(_), _)
            | FileClientMsg::Opened(Err(_))
            | FileClientMsg::Wrote(Err(_))
            | FileClientMsg::Synced(Err(_))
            | FileClientMsg::Sized(Err(_))
            | FileClientMsg::Read(Err(_))
            | FileClientMsg::Closed(Err(_)) => Effect::Stop,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum UdpClientMsg {
    Start,
    BoundReceiver(Result<(UdpSocketId, SocketAddr), CallError>),
    BoundSender(Result<(UdpSocketId, SocketAddr), CallError>),
    Received(Result<(SocketAddr, Vec<u8>, bool), CallError>),
    Sent(Result<usize, CallError>),
    Closed(Result<(), CallError>),
}

#[derive(Debug)]
struct UdpClient {
    receiver_addr: SocketAddr,
    sender_addr: SocketAddr,
    receiver: Option<UdpSocketId>,
    sender: Option<UdpSocketId>,
    observed: Rc<RefCell<Vec<String>>>,
}

impl Isolate for UdpClient {
    type Message = UdpClientMsg;
    type Reply = ();
    type Send = Outbound<Infallible>;
    type Spawn = Infallible;
    type Call = RuntimeCall<UdpClientMsg>;
    type Shard = TestShard;

    fn handle(&mut self, msg: Self::Message, _ctx: &mut Context<'_, Self::Shard>) -> Effect<Self> {
        match msg {
            UdpClientMsg::Start => udp_bind(self.receiver_addr).reply(UdpClientMsg::BoundReceiver),
            UdpClientMsg::BoundReceiver(Ok((socket, _addr))) => {
                self.receiver = Some(socket);
                udp_bind(self.sender_addr).reply(UdpClientMsg::BoundSender)
            }
            UdpClientMsg::BoundSender(Ok((socket, _addr))) => {
                self.sender = Some(socket);
                batch([
                    udp_recv_from(self.receiver.expect("receiver"), 4)
                        .reply(UdpClientMsg::Received),
                    udp_send_to(socket, self.receiver_addr, b"alpaca".to_vec())
                        .reply(UdpClientMsg::Sent),
                ])
            }
            UdpClientMsg::Sent(Ok(count)) => {
                self.observed.borrow_mut().push(format!("sent:{count}"));
                udp_close_socket(self.sender.expect("sender")).reply(UdpClientMsg::Closed)
            }
            UdpClientMsg::Received(Ok((_peer, bytes, truncated))) => {
                self.observed.borrow_mut().push(format!(
                    "recv:{}:{truncated}",
                    String::from_utf8(bytes).expect("utf8")
                ));
                udp_close_socket(self.receiver.expect("receiver")).reply(UdpClientMsg::Closed)
            }
            UdpClientMsg::Closed(Ok(())) => {
                self.observed.borrow_mut().push("closed".to_string());
                Effect::Noop
            }
            UdpClientMsg::BoundReceiver(Err(_))
            | UdpClientMsg::BoundSender(Err(_))
            | UdpClientMsg::Received(Err(_))
            | UdpClientMsg::Sent(Err(_))
            | UdpClientMsg::Closed(Err(_)) => {
                self.observed.borrow_mut().push("failed".to_string());
                Effect::Noop
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum UdpProbeMsg {
    Bind,
    Bound(Result<(UdpSocketId, SocketAddr), CallError>),
    RecvOne(UdpSocketId),
    RecvTwo(UdpSocketId),
    Close(UdpSocketId),
    Done(Result<(SocketAddr, Vec<u8>, bool), CallError>),
    Closed(Result<(), CallError>),
    Stop,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum DnsProbeMsg {
    Lookup {
        host: String,
        port: u16,
        timeout: Duration,
    },
    Done(Result<Vec<SocketAddr>, CallError>),
}

#[derive(Debug)]
struct DnsProbe {
    observed: Rc<RefCell<Vec<String>>>,
}

impl Isolate for DnsProbe {
    type Message = DnsProbeMsg;
    type Reply = ();
    type Send = Outbound<Infallible>;
    type Spawn = Infallible;
    type Call = RuntimeCall<DnsProbeMsg>;
    type Shard = TestShard;

    fn handle(&mut self, msg: Self::Message, _ctx: &mut Context<'_, Self::Shard>) -> Effect<Self> {
        match msg {
            DnsProbeMsg::Lookup {
                host,
                port,
                timeout,
            } => dns_lookup(host, port, timeout).reply(DnsProbeMsg::Done),
            DnsProbeMsg::Done(Ok(addrs)) => {
                self.observed.borrow_mut().push(format!(
                    "resolved:{}",
                    addrs
                        .iter()
                        .map(ToString::to_string)
                        .collect::<Vec<_>>()
                        .join(",")
                ));
                Effect::Noop
            }
            DnsProbeMsg::Done(Err(error)) => {
                self.observed
                    .borrow_mut()
                    .push(format!("dns-err:{error:?}"));
                Effect::Noop
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum ProcessProbeMsg {
    Run {
        command: String,
        args: Vec<String>,
        timeout: Duration,
        stdout_limit: usize,
        stderr_limit: usize,
    },
    Done(Result<ProcessRunResult, CallError>),
}

#[derive(Debug)]
struct ProcessProbe {
    observed: Rc<RefCell<Vec<String>>>,
}

impl Isolate for ProcessProbe {
    type Message = ProcessProbeMsg;
    type Reply = ();
    type Send = Outbound<Infallible>;
    type Spawn = Infallible;
    type Call = RuntimeCall<ProcessProbeMsg>;
    type Shard = TestShard;

    fn handle(&mut self, msg: Self::Message, _ctx: &mut Context<'_, Self::Shard>) -> Effect<Self> {
        match msg {
            ProcessProbeMsg::Run {
                command,
                args,
                timeout,
                stdout_limit,
                stderr_limit,
            } => process_run(command, args, timeout, stdout_limit, stderr_limit)
                .reply(ProcessProbeMsg::Done),
            ProcessProbeMsg::Done(Ok(result)) => {
                self.observed.borrow_mut().push(format!(
                    "process:{:?}:{}:{}:{}",
                    result.status.code,
                    String::from_utf8_lossy(&result.stdout),
                    result.stdout_truncated,
                    result.stderr_truncated
                ));
                Effect::Noop
            }
            ProcessProbeMsg::Done(Err(error)) => {
                self.observed
                    .borrow_mut()
                    .push(format!("process-err:{error:?}"));
                Effect::Noop
            }
        }
    }
}

#[derive(Debug)]
struct UdpProbe {
    bind_addr: SocketAddr,
    observed: Rc<RefCell<Vec<String>>>,
}

impl Isolate for UdpProbe {
    type Message = UdpProbeMsg;
    type Reply = ();
    type Send = Outbound<Infallible>;
    type Spawn = Infallible;
    type Call = RuntimeCall<UdpProbeMsg>;
    type Shard = TestShard;

    fn handle(&mut self, msg: Self::Message, _ctx: &mut Context<'_, Self::Shard>) -> Effect<Self> {
        match msg {
            UdpProbeMsg::Bind => udp_bind(self.bind_addr).reply(UdpProbeMsg::Bound),
            UdpProbeMsg::Bound(Ok((socket, _))) => {
                self.observed
                    .borrow_mut()
                    .push(format!("bound:{}", socket.get()));
                Effect::Noop
            }
            UdpProbeMsg::RecvOne(socket) => udp_recv_from(socket, 8).reply(UdpProbeMsg::Done),
            UdpProbeMsg::RecvTwo(socket) => batch([
                udp_recv_from(socket, 8).reply(UdpProbeMsg::Done),
                udp_recv_from(socket, 8).reply(UdpProbeMsg::Done),
            ]),
            UdpProbeMsg::Close(socket) => udp_close_socket(socket).reply(UdpProbeMsg::Closed),
            UdpProbeMsg::Done(Ok((_peer, bytes, truncated))) => {
                self.observed.borrow_mut().push(format!(
                    "recv:{}:{truncated}",
                    String::from_utf8(bytes).expect("utf8")
                ));
                Effect::Noop
            }
            UdpProbeMsg::Done(Err(error)) => {
                self.observed
                    .borrow_mut()
                    .push(format!("recv-err:{error:?}"));
                Effect::Noop
            }
            UdpProbeMsg::Closed(Ok(())) => {
                self.observed.borrow_mut().push("closed".to_string());
                Effect::Noop
            }
            UdpProbeMsg::Closed(Err(error)) => {
                self.observed
                    .borrow_mut()
                    .push(format!("close-err:{error:?}"));
                Effect::Noop
            }
            UdpProbeMsg::Bound(Err(error)) => {
                self.observed
                    .borrow_mut()
                    .push(format!("bind-err:{error:?}"));
                Effect::Noop
            }
            UdpProbeMsg::Stop => Effect::Stop,
        }
    }
}

fn read_call(stream: StreamId, max_len: usize) -> Effect<Connection> {
    Effect::Call(RuntimeCall::new(
        CallInput::TcpRead { stream, max_len },
        |result| match result {
            CallOutput::TcpRead { bytes } => ConnectionMsg::ReadCompleted(bytes),
            CallOutput::Failed(_) => ConnectionMsg::Failed,
            other => panic!("unexpected read result {other:?}"),
        },
    ))
}

fn write_call(stream: StreamId, bytes: Vec<u8>) -> Effect<Connection> {
    Effect::Call(RuntimeCall::new(
        CallInput::TcpWrite { stream, bytes },
        |result| match result {
            CallOutput::TcpWrote { count } => ConnectionMsg::WriteCompleted { count },
            CallOutput::Failed(_) => ConnectionMsg::Failed,
            other => panic!("unexpected write result {other:?}"),
        },
    ))
}

fn close_call(stream: StreamId) -> Effect<Connection> {
    Effect::Call(RuntimeCall::new(
        CallInput::TcpStreamClose { stream },
        |result| match result {
            CallOutput::TcpStreamClosed => ConnectionMsg::StreamClosed,
            CallOutput::Failed(_) => ConnectionMsg::Failed,
            other => panic!("unexpected close result {other:?}"),
        },
    ))
}

#[derive(Debug, Clone)]
enum ListenerMsg {
    Bootstrap,
    Bound {
        listener: ListenerId,
        addr: SocketAddr,
    },
    ReArmAccept,
    CloseListener,
    ListenerClosed,
    Accepted {
        stream: StreamId,
    },
    Failed,
}

#[derive(Debug)]
struct Listener {
    bind_addr: SocketAddr,
    bound_addr_slot: Arc<Mutex<Option<SocketAddr>>>,
    max_chunk: usize,
    target_accepts: usize,
    accepted_count: usize,
    self_addr: Option<Address<ListenerMsg>>,
    listener: Option<ListenerId>,
}

impl Isolate for Listener {
    type Message = ListenerMsg;
    type Reply = ();
    type Send = Outbound<ListenerMsg>;
    type Spawn = RestartableChildDefinition<Connection>;
    type Call = RuntimeCall<ListenerMsg>;
    type Shard = TestShard;

    fn handle(&mut self, msg: Self::Message, ctx: &mut Context<'_, Self::Shard>) -> Effect<Self> {
        match msg {
            ListenerMsg::Bootstrap => {
                self.self_addr = Some(ctx.me());
                let addr = self.bind_addr;
                Effect::Call(RuntimeCall::new(
                    CallInput::TcpBind { addr },
                    move |result| match result {
                        CallOutput::TcpBound {
                            listener,
                            local_addr,
                        } => ListenerMsg::Bound {
                            listener,
                            addr: local_addr,
                        },
                        CallOutput::Failed(_) => ListenerMsg::Failed,
                        other => panic!("unexpected bind result {other:?}"),
                    },
                ))
            }
            ListenerMsg::Bound { listener, addr } => {
                self.listener = Some(listener);
                *self.bound_addr_slot.lock().expect("bound addr mutex") = Some(addr);
                Effect::Call(RuntimeCall::new(
                    CallInput::TcpAccept { listener },
                    |result| match result {
                        CallOutput::TcpAccepted { stream, .. } => ListenerMsg::Accepted { stream },
                        CallOutput::Failed(_) => ListenerMsg::Failed,
                        other => panic!("unexpected accept result {other:?}"),
                    },
                ))
            }
            ListenerMsg::ReArmAccept => {
                let listener = self.listener.expect("listener stored before re-arm");
                Effect::Call(RuntimeCall::new(
                    CallInput::TcpAccept { listener },
                    |result| match result {
                        CallOutput::TcpAccepted { stream, .. } => ListenerMsg::Accepted { stream },
                        CallOutput::Failed(_) => ListenerMsg::Failed,
                        other => panic!("unexpected accept result {other:?}"),
                    },
                ))
            }
            ListenerMsg::Accepted { stream } => {
                self.accepted_count += 1;
                let max_chunk = self.max_chunk;
                let spawn = Effect::Spawn(
                    RestartableChildDefinition::new(
                        move || Connection {
                            stream,
                            max_chunk,
                            pending_write: Vec::new(),
                        },
                        8,
                    )
                    .with_initial_message(|| ConnectionMsg::Start),
                );
                let self_addr = self.self_addr.expect("listener captured self address");
                let follow_up = if self.accepted_count < self.target_accepts {
                    ListenerMsg::ReArmAccept
                } else {
                    ListenerMsg::CloseListener
                };
                Effect::Batch(vec![
                    spawn,
                    Effect::Send(Outbound::new(self_addr, follow_up)),
                ])
            }
            ListenerMsg::CloseListener => {
                let listener = self.listener.expect("listener stored before close");
                Effect::Call(RuntimeCall::new(
                    CallInput::TcpListenerClose { listener },
                    |result| match result {
                        CallOutput::TcpListenerClosed => ListenerMsg::ListenerClosed,
                        CallOutput::Failed(_) => ListenerMsg::Failed,
                        other => panic!("unexpected listener close result {other:?}"),
                    },
                ))
            }
            ListenerMsg::ListenerClosed => Effect::Stop,
            ListenerMsg::Failed => Effect::Stop,
        }
    }
}

#[derive(Debug, Clone)]
enum BinderMsg {
    StartBind,
    Bound {
        listener: ListenerId,
        addr: SocketAddr,
    },
}

#[derive(Debug)]
struct Binder {
    bind_addr: SocketAddr,
    listener_slot: Arc<Mutex<Option<ListenerId>>>,
    addr_slot: Arc<Mutex<Option<SocketAddr>>>,
}

impl Isolate for Binder {
    type Message = BinderMsg;
    type Reply = ();
    type Send = Outbound<Infallible>;
    type Spawn = Infallible;
    type Call = RuntimeCall<BinderMsg>;
    type Shard = TestShard;

    fn handle(&mut self, msg: Self::Message, _ctx: &mut Context<'_, Self::Shard>) -> Effect<Self> {
        match msg {
            BinderMsg::StartBind => {
                let addr = self.bind_addr;
                Effect::Call(RuntimeCall::new(
                    CallInput::TcpBind { addr },
                    move |result| match result {
                        CallOutput::TcpBound {
                            listener,
                            local_addr,
                        } => BinderMsg::Bound {
                            listener,
                            addr: local_addr,
                        },
                        other => panic!("expected bind success, got {other:?}"),
                    },
                ))
            }
            BinderMsg::Bound { listener, addr } => {
                *self.listener_slot.lock().expect("listener mutex") = Some(listener);
                *self.addr_slot.lock().expect("addr mutex") = Some(addr);
                Effect::Noop
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProbeMsg {
    StartInvalidAccept,
    StartInvalidRead,
    StartInvalidFileOpen,
    InvalidResourceObserved,
    FileIoObserved,
}

#[derive(Debug)]
struct Probe {
    log: Rc<RefCell<Vec<ProbeMsg>>>,
}

impl Isolate for Probe {
    type Message = ProbeMsg;
    type Reply = ();
    type Send = Outbound<Infallible>;
    type Spawn = Infallible;
    type Call = RuntimeCall<ProbeMsg>;
    type Shard = TestShard;

    fn handle(&mut self, msg: Self::Message, _ctx: &mut Context<'_, Self::Shard>) -> Effect<Self> {
        match msg {
            ProbeMsg::StartInvalidAccept => Effect::Call(RuntimeCall::new(
                CallInput::TcpAccept {
                    listener: ListenerId::new(999),
                },
                |result| match result {
                    CallOutput::Failed(CallError::InvalidResource) => {
                        ProbeMsg::InvalidResourceObserved
                    }
                    other => panic!("expected invalid accept failure, got {other:?}"),
                },
            )),
            ProbeMsg::StartInvalidRead => Effect::Call(RuntimeCall::new(
                CallInput::TcpRead {
                    stream: StreamId::new(999),
                    max_len: 8,
                },
                |result| match result {
                    CallOutput::Failed(CallError::InvalidResource) => {
                        ProbeMsg::InvalidResourceObserved
                    }
                    other => panic!("expected invalid read failure, got {other:?}"),
                },
            )),
            ProbeMsg::StartInvalidFileOpen => Effect::Call(RuntimeCall::new(
                CallInput::FileOpen {
                    path: PathBuf::from("/tmp/tina-sim-invalid-open.txt"),
                    options: FileOpenOptions {
                        read: true,
                        write: false,
                        create: true,
                        truncate: false,
                    },
                },
                |result| match result {
                    CallOutput::Failed(CallError::Io) => ProbeMsg::FileIoObserved,
                    other => panic!("expected invalid file open failure, got {other:?}"),
                },
            )),
            ProbeMsg::InvalidResourceObserved => {
                self.log
                    .borrow_mut()
                    .push(ProbeMsg::InvalidResourceObserved);
                Effect::Noop
            }
            ProbeMsg::FileIoObserved => {
                self.log.borrow_mut().push(ProbeMsg::FileIoObserved);
                Effect::Noop
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WaiterMsg {
    StartAccept,
    StopNow,
    Fill,
    Accepted,
    Failed,
}

#[derive(Debug)]
struct Waiter {
    listener: ListenerId,
    log: Rc<RefCell<Vec<WaiterMsg>>>,
}

impl Isolate for Waiter {
    type Message = WaiterMsg;
    type Reply = ();
    type Send = Outbound<Infallible>;
    type Spawn = Infallible;
    type Call = RuntimeCall<WaiterMsg>;
    type Shard = TestShard;

    fn handle(&mut self, msg: Self::Message, _ctx: &mut Context<'_, Self::Shard>) -> Effect<Self> {
        match msg {
            WaiterMsg::StartAccept => Effect::Call(RuntimeCall::new(
                CallInput::TcpAccept {
                    listener: self.listener,
                },
                |result| match result {
                    CallOutput::TcpAccepted { .. } => WaiterMsg::Accepted,
                    CallOutput::Failed(_) => WaiterMsg::Failed,
                    other => panic!("unexpected accept result {other:?}"),
                },
            )),
            WaiterMsg::StopNow => Effect::Stop,
            WaiterMsg::Fill => Effect::Noop,
            WaiterMsg::Accepted => {
                self.log.borrow_mut().push(WaiterMsg::Accepted);
                Effect::Noop
            }
            WaiterMsg::Failed => {
                self.log.borrow_mut().push(WaiterMsg::Failed);
                Effect::Noop
            }
        }
    }
}

#[derive(Debug, Clone)]
enum PeerAwareAcceptMsg {
    StartAccept,
    Accepted {
        stream: StreamId,
        peer_addr: SocketAddr,
    },
    Failed,
}

#[derive(Debug)]
struct PeerAwareAcceptor {
    listener: ListenerId,
    stream_slot: Arc<Mutex<Option<StreamId>>>,
    peer_slot: Arc<Mutex<Option<SocketAddr>>>,
}

impl Isolate for PeerAwareAcceptor {
    type Message = PeerAwareAcceptMsg;
    type Reply = ();
    type Send = Outbound<Infallible>;
    type Spawn = Infallible;
    type Call = RuntimeCall<PeerAwareAcceptMsg>;
    type Shard = TestShard;

    fn handle(&mut self, msg: Self::Message, _ctx: &mut Context<'_, Self::Shard>) -> Effect<Self> {
        match msg {
            PeerAwareAcceptMsg::StartAccept => Effect::Call(RuntimeCall::new(
                CallInput::TcpAccept {
                    listener: self.listener,
                },
                |result| match result {
                    CallOutput::TcpAccepted { stream, peer_addr } => {
                        PeerAwareAcceptMsg::Accepted { stream, peer_addr }
                    }
                    CallOutput::Failed(_) => PeerAwareAcceptMsg::Failed,
                    other => panic!("unexpected accept result {other:?}"),
                },
            )),
            PeerAwareAcceptMsg::Accepted { stream, peer_addr } => {
                *self.stream_slot.lock().expect("stream mutex") = Some(stream);
                *self.peer_slot.lock().expect("peer mutex") = Some(peer_addr);
                Effect::Noop
            }
            PeerAwareAcceptMsg::Failed => Effect::Noop,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum ReadProbeMsg {
    StartRead,
    ReadCompleted(Vec<u8>),
    Failed,
    StopNow,
}

#[derive(Debug)]
struct ReadProbe {
    stream: StreamId,
    max_len: usize,
    log: Rc<RefCell<Vec<ReadProbeMsg>>>,
}

impl Isolate for ReadProbe {
    type Message = ReadProbeMsg;
    type Reply = ();
    type Send = Outbound<Infallible>;
    type Spawn = Infallible;
    type Call = RuntimeCall<ReadProbeMsg>;
    type Shard = TestShard;

    fn handle(&mut self, msg: Self::Message, _ctx: &mut Context<'_, Self::Shard>) -> Effect<Self> {
        match msg {
            ReadProbeMsg::StartRead => Effect::Call(RuntimeCall::new(
                CallInput::TcpRead {
                    stream: self.stream,
                    max_len: self.max_len,
                },
                |result| match result {
                    CallOutput::TcpRead { bytes } => ReadProbeMsg::ReadCompleted(bytes),
                    CallOutput::Failed(_) => ReadProbeMsg::Failed,
                    other => panic!("unexpected read result {other:?}"),
                },
            )),
            ReadProbeMsg::ReadCompleted(bytes) => {
                self.log
                    .borrow_mut()
                    .push(ReadProbeMsg::ReadCompleted(bytes));
                Effect::Noop
            }
            ReadProbeMsg::Failed => {
                self.log.borrow_mut().push(ReadProbeMsg::Failed);
                Effect::Noop
            }
            ReadProbeMsg::StopNow => Effect::Stop,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum WriteProbeMsg {
    StartWrite,
    Wrote(usize),
    Failed,
    StopNow,
}

#[derive(Debug)]
struct WriteProbe {
    stream: StreamId,
    bytes: Vec<u8>,
    log: Rc<RefCell<Vec<WriteProbeMsg>>>,
}

impl Isolate for WriteProbe {
    type Message = WriteProbeMsg;
    type Reply = ();
    type Send = Outbound<Infallible>;
    type Spawn = Infallible;
    type Call = RuntimeCall<WriteProbeMsg>;
    type Shard = TestShard;

    fn handle(&mut self, msg: Self::Message, _ctx: &mut Context<'_, Self::Shard>) -> Effect<Self> {
        match msg {
            WriteProbeMsg::StartWrite => Effect::Call(RuntimeCall::new(
                CallInput::TcpWrite {
                    stream: self.stream,
                    bytes: self.bytes.clone(),
                },
                |result| match result {
                    CallOutput::TcpWrote { count } => WriteProbeMsg::Wrote(count),
                    CallOutput::Failed(_) => WriteProbeMsg::Failed,
                    other => panic!("unexpected write result {other:?}"),
                },
            )),
            WriteProbeMsg::Wrote(count) => {
                self.log.borrow_mut().push(WriteProbeMsg::Wrote(count));
                Effect::Noop
            }
            WriteProbeMsg::Failed => {
                self.log.borrow_mut().push(WriteProbeMsg::Failed);
                Effect::Noop
            }
            WriteProbeMsg::StopNow => Effect::Stop,
        }
    }
}

#[derive(Debug, Clone)]
enum CloserMsg {
    CloseListener,
    CloseStream,
    ListenerClosed,
    StreamClosed,
    Failed,
}

#[derive(Debug)]
struct ListenerCloser {
    listener: ListenerId,
}

impl Isolate for ListenerCloser {
    type Message = CloserMsg;
    type Reply = ();
    type Send = Outbound<Infallible>;
    type Spawn = Infallible;
    type Call = RuntimeCall<CloserMsg>;
    type Shard = TestShard;

    fn handle(&mut self, msg: Self::Message, _ctx: &mut Context<'_, Self::Shard>) -> Effect<Self> {
        match msg {
            CloserMsg::CloseListener => Effect::Call(RuntimeCall::new(
                CallInput::TcpListenerClose {
                    listener: self.listener,
                },
                |result| match result {
                    CallOutput::TcpListenerClosed => CloserMsg::ListenerClosed,
                    CallOutput::Failed(_) => CloserMsg::Failed,
                    other => panic!("unexpected listener close result {other:?}"),
                },
            )),
            CloserMsg::CloseStream
            | CloserMsg::ListenerClosed
            | CloserMsg::StreamClosed
            | CloserMsg::Failed => Effect::Noop,
        }
    }
}

#[derive(Debug)]
struct StreamCloser {
    stream: StreamId,
}

impl Isolate for StreamCloser {
    type Message = CloserMsg;
    type Reply = ();
    type Send = Outbound<Infallible>;
    type Spawn = Infallible;
    type Call = RuntimeCall<CloserMsg>;
    type Shard = TestShard;

    fn handle(&mut self, msg: Self::Message, _ctx: &mut Context<'_, Self::Shard>) -> Effect<Self> {
        match msg {
            CloserMsg::CloseStream => Effect::Call(RuntimeCall::new(
                CallInput::TcpStreamClose {
                    stream: self.stream,
                },
                |result| match result {
                    CallOutput::TcpStreamClosed => CloserMsg::StreamClosed,
                    CallOutput::Failed(_) => CloserMsg::Failed,
                    other => panic!("unexpected stream close result {other:?}"),
                },
            )),
            CloserMsg::CloseListener
            | CloserMsg::ListenerClosed
            | CloserMsg::StreamClosed
            | CloserMsg::Failed => Effect::Noop,
        }
    }
}

fn count_call_completed(trace: &[RuntimeEvent], kind: CallKind) -> usize {
    trace
        .iter()
        .filter(|event| {
            matches!(
                event.kind(),
                RuntimeEventKind::CallCompleted { call_kind, .. } if call_kind == kind
            )
        })
        .count()
}

fn count_spawned(trace: &[RuntimeEvent]) -> usize {
    trace
        .iter()
        .filter(|event| matches!(event.kind(), RuntimeEventKind::Spawned { .. }))
        .count()
}

fn completed_call_isolates(trace: &[RuntimeEvent], kind: CallKind) -> Vec<IsolateId> {
    trace
        .iter()
        .filter_map(|event| match event.kind() {
            RuntimeEventKind::CallCompleted { call_kind, .. } if call_kind == kind => {
                Some(event.isolate())
            }
            _ => None,
        })
        .collect()
}

fn accept_completion_order_for_two_waiters(
    config: SimulatorConfig,
) -> (Vec<IsolateId>, IsolateId, IsolateId) {
    let mut sim = Simulator::new(
        TestShard,
        SimulatorConfig {
            tcp: ScriptedTcpConfig {
                pending_completion_capacity: 8,
                listeners: vec![
                    ScriptedListenerConfig {
                        bind_addr: bind_addr(),
                        local_addr: local_addr(47000),
                        backlog_capacity: 1,
                        peers: vec![peer_script(
                            5,
                            peer_addr(59001),
                            vec![b"a".to_vec()],
                            None,
                            1,
                        )],
                    },
                    ScriptedListenerConfig {
                        bind_addr: local_addr(47001),
                        local_addr: local_addr(47001),
                        backlog_capacity: 1,
                        peers: vec![peer_script(
                            5,
                            peer_addr(59002),
                            vec![b"b".to_vec()],
                            None,
                            1,
                        )],
                    },
                ],
            },
            ..config
        },
    );
    let first_listener = Arc::new(Mutex::new(None));
    let first_addr = Arc::new(Mutex::new(None));
    let first_binder = sim.register(Binder {
        bind_addr: bind_addr(),
        listener_slot: Arc::clone(&first_listener),
        addr_slot: Arc::clone(&first_addr),
    });
    let second_listener = Arc::new(Mutex::new(None));
    let second_addr = Arc::new(Mutex::new(None));
    let second_binder = sim.register(Binder {
        bind_addr: local_addr(47001),
        listener_slot: Arc::clone(&second_listener),
        addr_slot: Arc::clone(&second_addr),
    });
    sim.try_send(first_binder, BinderMsg::StartBind).unwrap();
    sim.try_send(second_binder, BinderMsg::StartBind).unwrap();
    sim.run_until_quiescent();

    let first_waiter = sim.register(Waiter {
        listener: first_listener
            .lock()
            .expect("listener mutex")
            .expect("listener"),
        log: Rc::new(RefCell::new(Vec::new())),
    });
    let second_waiter = sim.register(Waiter {
        listener: second_listener
            .lock()
            .expect("listener mutex")
            .expect("listener"),
        log: Rc::new(RefCell::new(Vec::new())),
    });
    let first_waiter_id = first_waiter.isolate();
    let second_waiter_id = second_waiter.isolate();
    sim.try_send(first_waiter, WaiterMsg::StartAccept).unwrap();
    sim.try_send(second_waiter, WaiterMsg::StartAccept).unwrap();
    sim.run_until_quiescent();

    (
        completed_call_isolates(sim.trace(), CallKind::TcpAccept),
        first_waiter_id,
        second_waiter_id,
    )
}

fn bind_addr() -> SocketAddr {
    "127.0.0.1:0".parse().expect("loopback bind addr")
}

fn local_addr(port: u16) -> SocketAddr {
    format!("127.0.0.1:{port}")
        .parse()
        .expect("loopback local addr")
}

fn peer_addr(port: u16) -> SocketAddr {
    format!("127.0.0.1:{port}")
        .parse()
        .expect("loopback peer addr")
}

fn peer_script(
    accept_after_step: u64,
    peer_addr: SocketAddr,
    inbound_chunks: Vec<Vec<u8>>,
    read_chunk_cap: Option<usize>,
    write_cap: usize,
) -> ScriptedPeerConfig {
    let inbound_capacity = inbound_chunks.iter().map(Vec::len).sum();
    ScriptedPeerConfig {
        accept_after_step,
        peer_addr,
        inbound_capacity,
        inbound_chunks,
        read_chunk_cap,
        write_cap,
        output_capacity: 1024,
    }
}

fn udp_socket_script(
    bind_addr: SocketAddr,
    local_addr: SocketAddr,
    inbound_datagrams: Vec<ScriptedUdpDatagramConfig>,
) -> ScriptedUdpSocketConfig {
    ScriptedUdpSocketConfig {
        bind_addr,
        local_addr,
        recv_capacity: inbound_datagrams.len().max(4),
        inbound_datagrams,
    }
}

fn udp_datagram(
    deliver_after_step: u64,
    peer_addr: SocketAddr,
    bytes: &[u8],
) -> ScriptedUdpDatagramConfig {
    ScriptedUdpDatagramConfig {
        deliver_after_step,
        peer_addr,
        bytes: bytes.to_vec(),
    }
}

fn run_echo_scenario(
    peers: Vec<ScriptedPeerConfig>,
    max_chunk: usize,
    config: SimulatorConfig,
) -> (ReplayArtifactData, IsolateId) {
    let bound = Arc::new(Mutex::new(None));
    let mut sim = Simulator::new(
        TestShard,
        SimulatorConfig {
            tcp: ScriptedTcpConfig {
                pending_completion_capacity: 32,
                listeners: vec![ScriptedListenerConfig {
                    bind_addr: bind_addr(),
                    local_addr: local_addr(41000),
                    backlog_capacity: peers.len().max(1),
                    peers,
                }],
            },
            ..config
        },
    );

    let listener = sim.register(Listener {
        bind_addr: bind_addr(),
        bound_addr_slot: Arc::clone(&bound),
        max_chunk,
        target_accepts: sim.config().tcp.listeners[0].peers.len(),
        accepted_count: 0,
        self_addr: None,
        listener: None,
    });
    sim.supervise(
        listener,
        SupervisorConfig::new(RestartPolicy::OneForOne, RestartBudget::new(4)),
    );
    sim.try_send(listener, ListenerMsg::Bootstrap).unwrap();
    sim.run_until_quiescent();

    (
        ReplayArtifactData {
            artifact: sim.replay_artifact(),
            bound_addr: *bound.lock().expect("bound mutex"),
        },
        listener.isolate(),
    )
}

fn udp_sim_config() -> SimulatorConfig {
    SimulatorConfig {
        udp: ScriptedUdpConfig {
            pending_completion_capacity: 16,
            sockets: vec![
                udp_socket_script(local_addr(48000), local_addr(48000), Vec::new()),
                udp_socket_script(local_addr(48001), local_addr(48001), Vec::new()),
                udp_socket_script(
                    local_addr(48002),
                    local_addr(48002),
                    vec![udp_datagram(3, peer_addr(58100), b"scripted")],
                ),
            ],
        },
        ..Default::default()
    }
}

fn dns_sim_config() -> SimulatorConfig {
    SimulatorConfig {
        dns: ScriptedDnsConfig {
            pending_completion_capacity: 2,
            lookups: vec![
                ScriptedDnsLookupConfig {
                    host: "llama.local".to_string(),
                    port: 8080,
                    complete_after_step: 3,
                    result: ScriptedDnsResult::Resolved(vec![local_addr(48100)]),
                },
                ScriptedDnsLookupConfig {
                    host: "sad.local".to_string(),
                    port: 8080,
                    complete_after_step: 2,
                    result: ScriptedDnsResult::Failed,
                },
                ScriptedDnsLookupConfig {
                    host: "slow.local".to_string(),
                    port: 8080,
                    complete_after_step: 2,
                    result: ScriptedDnsResult::Timeout,
                },
            ],
        },
        ..Default::default()
    }
}

fn process_sim_config() -> SimulatorConfig {
    SimulatorConfig {
        process: ScriptedProcessConfig {
            pending_completion_capacity: 1,
            runs: vec![
                ScriptedProcessRunConfig {
                    command: "llama-echo".to_string(),
                    args: vec!["feed".to_string()],
                    complete_after_step: 3,
                    result: ScriptedProcessResult::Exited {
                        code: Some(0),
                        stdout: b"llamafeed".to_vec(),
                        stderr: Vec::new(),
                    },
                },
                ScriptedProcessRunConfig {
                    command: "llama-sleep".to_string(),
                    args: Vec::new(),
                    complete_after_step: 6,
                    result: ScriptedProcessResult::Timeout,
                },
            ],
        },
        ..Default::default()
    }
}

#[test]
fn scripted_process_exits_truncates_times_out_and_replays() {
    let observed = Rc::new(RefCell::new(Vec::new()));
    let mut sim = Simulator::new(TestShard, process_sim_config());
    let probe = sim.register(ProcessProbe {
        observed: Rc::clone(&observed),
    });

    sim.try_send(
        probe,
        ProcessProbeMsg::Run {
            command: "llama-echo".to_string(),
            args: vec!["feed".to_string()],
            timeout: Duration::from_millis(50),
            stdout_limit: 5,
            stderr_limit: 4,
        },
    )
    .unwrap();
    sim.run_until_quiescent();

    assert!(
        observed
            .borrow()
            .iter()
            .any(|entry| entry == "process:Some(0):llama:true:false")
    );
    assert_eq!(count_call_completed(sim.trace(), CallKind::ProcessRun), 1);

    sim.try_send(
        probe,
        ProcessProbeMsg::Run {
            command: "llama-sleep".to_string(),
            args: Vec::new(),
            timeout: Duration::from_millis(50),
            stdout_limit: 5,
            stderr_limit: 4,
        },
    )
    .unwrap();
    sim.run_until_quiescent();
    assert!(
        observed
            .borrow()
            .iter()
            .any(|entry| entry == "process-err:Timeout")
    );

    let first = sim.replay_artifact();
    let mut replay = Simulator::new(TestShard, process_sim_config());
    let replay_probe = replay.register(ProcessProbe {
        observed: Rc::new(RefCell::new(Vec::new())),
    });
    replay
        .try_send(
            replay_probe,
            ProcessProbeMsg::Run {
                command: "llama-echo".to_string(),
                args: vec!["feed".to_string()],
                timeout: Duration::from_millis(50),
                stdout_limit: 5,
                stderr_limit: 4,
            },
        )
        .unwrap();
    replay.run_until_quiescent();
    replay
        .try_send(
            replay_probe,
            ProcessProbeMsg::Run {
                command: "llama-sleep".to_string(),
                args: Vec::new(),
                timeout: Duration::from_millis(50),
                stdout_limit: 5,
                stderr_limit: 4,
            },
        )
        .unwrap();
    replay.run_until_quiescent();
    assert_eq!(
        first.event_record(),
        replay.replay_artifact().event_record()
    );
}

#[test]
fn scripted_process_zero_timeout_and_lane_full_are_visible() {
    let observed = Rc::new(RefCell::new(Vec::new()));
    let mut sim = Simulator::new(
        TestShard,
        SimulatorConfig {
            process: ScriptedProcessConfig {
                pending_completion_capacity: 1,
                runs: vec![
                    ScriptedProcessRunConfig {
                        command: "llama-echo".to_string(),
                        args: vec!["feed".to_string()],
                        complete_after_step: 99,
                        result: ScriptedProcessResult::Exited {
                            code: Some(0),
                            stdout: b"llamafeed".to_vec(),
                            stderr: Vec::new(),
                        },
                    },
                    ScriptedProcessRunConfig {
                        command: "llama-sleep".to_string(),
                        args: Vec::new(),
                        complete_after_step: 99,
                        result: ScriptedProcessResult::Timeout,
                    },
                ],
            },
            ..Default::default()
        },
    );
    let probe = sim.register(ProcessProbe {
        observed: Rc::clone(&observed),
    });

    sim.try_send(
        probe,
        ProcessProbeMsg::Run {
            command: "llama-echo".to_string(),
            args: vec!["feed".to_string()],
            timeout: Duration::ZERO,
            stdout_limit: 5,
            stderr_limit: 4,
        },
    )
    .unwrap();
    sim.run_until_quiescent();
    assert!(
        observed
            .borrow()
            .iter()
            .any(|entry| entry == "process-err:Timeout")
    );

    sim.try_send(
        probe,
        ProcessProbeMsg::Run {
            command: "llama-echo".to_string(),
            args: vec!["feed".to_string()],
            timeout: Duration::from_millis(50),
            stdout_limit: 5,
            stderr_limit: 4,
        },
    )
    .unwrap();
    sim.try_send(
        probe,
        ProcessProbeMsg::Run {
            command: "llama-sleep".to_string(),
            args: Vec::new(),
            timeout: Duration::from_millis(50),
            stdout_limit: 5,
            stderr_limit: 4,
        },
    )
    .unwrap();
    sim.run_until_quiescent();
    assert!(
        observed
            .borrow()
            .iter()
            .any(|entry| entry == "process-err:ProcessFull")
    );
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ResourceRailOp {
    DnsOk,
    DnsFail,
    DnsTimeout,
    ProcessOk,
    ProcessTimeout,
    UdpLoopback,
}

fn resource_rail_history(seed: u64) -> History<ResourceRailOp> {
    let choices = [
        ResourceRailOp::DnsOk,
        ResourceRailOp::DnsFail,
        ResourceRailOp::DnsTimeout,
        ResourceRailOp::ProcessOk,
        ResourceRailOp::ProcessTimeout,
        ResourceRailOp::UdpLoopback,
    ];
    let len = 4 + (seed as usize % 4);
    let operations = (0..len)
        .map(|index| choices[((seed as usize) + index * 3) % choices.len()])
        .collect();
    History::new("resource rail matrix", seed, operations)
}

fn resource_rail_config(seed: u64) -> SimulatorConfig {
    SimulatorConfig {
        seed,
        dns: ScriptedDnsConfig {
            pending_completion_capacity: 4,
            lookups: vec![
                ScriptedDnsLookupConfig {
                    host: "ok.local".to_string(),
                    port: 8080,
                    complete_after_step: 2,
                    result: ScriptedDnsResult::Resolved(vec![local_addr(48200)]),
                },
                ScriptedDnsLookupConfig {
                    host: "fail.local".to_string(),
                    port: 8080,
                    complete_after_step: 2,
                    result: ScriptedDnsResult::Failed,
                },
                ScriptedDnsLookupConfig {
                    host: "timeout.local".to_string(),
                    port: 8080,
                    complete_after_step: 2,
                    result: ScriptedDnsResult::Timeout,
                },
            ],
        },
        process: ScriptedProcessConfig {
            pending_completion_capacity: 4,
            runs: vec![
                ScriptedProcessRunConfig {
                    command: "ok".to_string(),
                    args: Vec::new(),
                    complete_after_step: 2,
                    result: ScriptedProcessResult::Exited {
                        code: Some(0),
                        stdout: b"done".to_vec(),
                        stderr: Vec::new(),
                    },
                },
                ScriptedProcessRunConfig {
                    command: "timeout".to_string(),
                    args: Vec::new(),
                    complete_after_step: 2,
                    result: ScriptedProcessResult::Timeout,
                },
            ],
        },
        udp: ScriptedUdpConfig {
            pending_completion_capacity: 4,
            sockets: vec![
                udp_socket_script(local_addr(48201), local_addr(48201), Vec::new()),
                udp_socket_script(local_addr(48202), local_addr(48202), Vec::new()),
            ],
        },
        ..Default::default()
    }
}

fn run_resource_rail_history(
    history: &History<ResourceRailOp>,
) -> DstRun<Vec<String>, ReplayArtifact> {
    let observed = Rc::new(RefCell::new(Vec::new()));
    let mut sim = Simulator::new(TestShard, resource_rail_config(history.seed()));
    let dns = sim.register(DnsProbe {
        observed: Rc::clone(&observed),
    });
    let process = sim.register(ProcessProbe {
        observed: Rc::clone(&observed),
    });

    for op in history.operations() {
        match op {
            ResourceRailOp::DnsOk => sim
                .try_send(
                    dns,
                    DnsProbeMsg::Lookup {
                        host: "ok.local".to_string(),
                        port: 8080,
                        timeout: Duration::from_millis(50),
                    },
                )
                .unwrap(),
            ResourceRailOp::DnsFail => sim
                .try_send(
                    dns,
                    DnsProbeMsg::Lookup {
                        host: "fail.local".to_string(),
                        port: 8080,
                        timeout: Duration::from_millis(50),
                    },
                )
                .unwrap(),
            ResourceRailOp::DnsTimeout => sim
                .try_send(
                    dns,
                    DnsProbeMsg::Lookup {
                        host: "timeout.local".to_string(),
                        port: 8080,
                        timeout: Duration::from_millis(50),
                    },
                )
                .unwrap(),
            ResourceRailOp::ProcessOk => sim
                .try_send(
                    process,
                    ProcessProbeMsg::Run {
                        command: "ok".to_string(),
                        args: Vec::new(),
                        timeout: Duration::from_millis(50),
                        stdout_limit: 8,
                        stderr_limit: 8,
                    },
                )
                .unwrap(),
            ResourceRailOp::ProcessTimeout => sim
                .try_send(
                    process,
                    ProcessProbeMsg::Run {
                        command: "timeout".to_string(),
                        args: Vec::new(),
                        timeout: Duration::from_millis(50),
                        stdout_limit: 8,
                        stderr_limit: 8,
                    },
                )
                .unwrap(),
            ResourceRailOp::UdpLoopback => {
                let client = sim.register(UdpClient {
                    receiver_addr: local_addr(48201),
                    sender_addr: local_addr(48202),
                    receiver: None,
                    sender: None,
                    observed: Rc::clone(&observed),
                });
                sim.try_send(client, UdpClientMsg::Start).unwrap();
            }
        }
        sim.run_until_quiescent();
    }

    let output = observed.borrow().clone();
    DstRun::new(output, sim.replay_artifact())
}

#[test]
fn dst_resource_rails_replay_and_delete_shrink_timeout_cases() {
    for seed in 0..16 {
        let history = resource_rail_history(seed);
        let run = assert_replays(&history, run_resource_rail_history);
        InvariantSuite::standard().assert(run.artifact().event_record());
    }

    let history = History::new(
        "resource rail timeout shrink",
        404,
        vec![
            ResourceRailOp::DnsOk,
            ResourceRailOp::ProcessOk,
            ResourceRailOp::UdpLoopback,
            ResourceRailOp::ProcessTimeout,
        ],
    );
    let shrunk = delete_shrink(
        &history,
        ShrinkConfig { max_attempts: 16 },
        "process timeout remains visible",
        |candidate| {
            run_resource_rail_history(candidate)
                .output()
                .iter()
                .any(|entry| entry == "process-err:Timeout")
        },
    );
    assert_eq!(
        shrunk.shrunk().operations(),
        &[ResourceRailOp::ProcessTimeout]
    );
}

#[test]
fn scripted_dns_resolves_fails_times_out_and_replays() {
    let observed = Rc::new(RefCell::new(Vec::new()));
    let mut sim = Simulator::new(TestShard, dns_sim_config());
    let probe = sim.register(DnsProbe {
        observed: Rc::clone(&observed),
    });

    sim.try_send(
        probe,
        DnsProbeMsg::Lookup {
            host: "llama.local".to_string(),
            port: 8080,
            timeout: Duration::from_millis(50),
        },
    )
    .unwrap();
    sim.try_send(
        probe,
        DnsProbeMsg::Lookup {
            host: "sad.local".to_string(),
            port: 8080,
            timeout: Duration::from_millis(50),
        },
    )
    .unwrap();
    sim.run_until_quiescent();

    assert!(
        observed
            .borrow()
            .iter()
            .any(|entry| entry == "resolved:127.0.0.1:48100")
    );
    assert!(observed.borrow().iter().any(|entry| entry == "dns-err:Io"));
    assert_eq!(count_call_completed(sim.trace(), CallKind::DnsLookup), 1);
    assert!(sim.trace().iter().any(|event| {
        matches!(
            event.kind(),
            RuntimeEventKind::CallFailed {
                call_kind: CallKind::DnsLookup,
                reason: CallError::Io,
                ..
            }
        )
    }));

    let first = sim.replay_artifact();
    let mut replay = Simulator::new(TestShard, dns_sim_config());
    let replay_probe = replay.register(DnsProbe {
        observed: Rc::new(RefCell::new(Vec::new())),
    });
    replay
        .try_send(
            replay_probe,
            DnsProbeMsg::Lookup {
                host: "llama.local".to_string(),
                port: 8080,
                timeout: Duration::from_millis(50),
            },
        )
        .unwrap();
    replay
        .try_send(
            replay_probe,
            DnsProbeMsg::Lookup {
                host: "sad.local".to_string(),
                port: 8080,
                timeout: Duration::from_millis(50),
            },
        )
        .unwrap();
    replay.run_until_quiescent();
    assert_eq!(
        first.event_record(),
        replay.replay_artifact().event_record()
    );
}

#[test]
fn scripted_dns_timeout_zero_and_lane_full_are_visible() {
    let observed = Rc::new(RefCell::new(Vec::new()));
    let mut sim = Simulator::new(
        TestShard,
        SimulatorConfig {
            dns: ScriptedDnsConfig {
                pending_completion_capacity: 1,
                lookups: vec![
                    ScriptedDnsLookupConfig {
                        host: "llama.local".to_string(),
                        port: 8080,
                        complete_after_step: 8,
                        result: ScriptedDnsResult::Resolved(vec![local_addr(48100)]),
                    },
                    ScriptedDnsLookupConfig {
                        host: "slow.local".to_string(),
                        port: 8080,
                        complete_after_step: 8,
                        result: ScriptedDnsResult::Timeout,
                    },
                ],
            },
            ..Default::default()
        },
    );
    let probe = sim.register(DnsProbe {
        observed: Rc::clone(&observed),
    });

    sim.try_send(
        probe,
        DnsProbeMsg::Lookup {
            host: "llama.local".to_string(),
            port: 8080,
            timeout: Duration::ZERO,
        },
    )
    .unwrap();
    sim.run_until_quiescent();
    assert!(
        observed
            .borrow()
            .iter()
            .any(|entry| entry == "dns-err:Timeout")
    );

    sim.try_send(
        probe,
        DnsProbeMsg::Lookup {
            host: "llama.local".to_string(),
            port: 8080,
            timeout: Duration::from_millis(50),
        },
    )
    .unwrap();
    sim.try_send(
        probe,
        DnsProbeMsg::Lookup {
            host: "slow.local".to_string(),
            port: 8080,
            timeout: Duration::from_millis(50),
        },
    )
    .unwrap();
    sim.run_until_quiescent();
    assert!(
        observed
            .borrow()
            .iter()
            .any(|entry| entry == "dns-err:DnsFull"),
        "observed DNS outcomes: {:?}; trace: {:?}",
        observed.borrow(),
        sim.trace()
    );
}

#[test]
fn scripted_udp_loopback_replays_send_recv_truncation_and_close() {
    let observed = Rc::new(RefCell::new(Vec::new()));
    let mut sim = Simulator::new(TestShard, udp_sim_config());
    let client = sim.register(UdpClient {
        receiver_addr: local_addr(48000),
        sender_addr: local_addr(48001),
        receiver: None,
        sender: None,
        observed: Rc::clone(&observed),
    });

    sim.try_send(client, UdpClientMsg::Start).unwrap();
    sim.run_until_quiescent();

    assert!(observed.borrow().iter().any(|entry| entry == "sent:6"));
    assert!(
        observed
            .borrow()
            .iter()
            .any(|entry| entry == "recv:alpa:true")
    );
    assert_eq!(
        observed
            .borrow()
            .iter()
            .filter(|entry| entry.as_str() == "closed")
            .count(),
        2
    );
    assert_eq!(count_call_completed(sim.trace(), CallKind::UdpBind), 2);
    assert_eq!(count_call_completed(sim.trace(), CallKind::UdpSendTo), 1);
    assert_eq!(count_call_completed(sim.trace(), CallKind::UdpRecvFrom), 1);
    assert_eq!(
        count_call_completed(sim.trace(), CallKind::UdpSocketClose),
        2
    );

    let first = sim.replay_artifact();
    let mut replay = Simulator::new(TestShard, udp_sim_config());
    let observed_replay = Rc::new(RefCell::new(Vec::new()));
    let client = replay.register(UdpClient {
        receiver_addr: local_addr(48000),
        sender_addr: local_addr(48001),
        receiver: None,
        sender: None,
        observed: observed_replay,
    });
    replay.try_send(client, UdpClientMsg::Start).unwrap();
    replay.run_until_quiescent();
    assert_eq!(
        first.event_record(),
        replay.replay_artifact().event_record()
    );
}

#[test]
fn scripted_udp_negative_paths_are_visible_and_cancelable() {
    let observed = Rc::new(RefCell::new(Vec::new()));
    let mut sim = Simulator::new(
        TestShard,
        SimulatorConfig {
            udp: ScriptedUdpConfig {
                pending_completion_capacity: 16,
                sockets: vec![udp_socket_script(
                    local_addr(48002),
                    local_addr(48002),
                    Vec::new(),
                )],
            },
            ..Default::default()
        },
    );
    let probe = sim.register(UdpProbe {
        bind_addr: local_addr(48002),
        observed: Rc::clone(&observed),
    });
    sim.try_send(probe, UdpProbeMsg::Bind).unwrap();
    sim.run_until_quiescent();
    assert!(observed.borrow().iter().any(|entry| entry == "bound:1"));

    let socket = UdpSocketId::new(1);
    sim.try_send(probe, UdpProbeMsg::RecvTwo(socket)).unwrap();
    sim.step();
    sim.step();
    assert!(
        observed
            .borrow()
            .iter()
            .any(|entry| entry == "recv-err:ResourceBusy")
    );

    sim.try_send(probe, UdpProbeMsg::Close(socket)).unwrap();
    sim.step();
    sim.step();
    assert!(
        observed
            .borrow()
            .iter()
            .any(|entry| entry == "close-err:ResourceBusy")
    );

    sim.try_send(probe, UdpProbeMsg::Stop).unwrap();
    sim.step();
    assert!(!sim.has_in_flight_calls());
    assert!(sim.trace().iter().any(|event| {
        matches!(
            event.kind(),
            RuntimeEventKind::CallCompletionRejected {
                call_kind: CallKind::UdpRecvFrom,
                reason: CallCompletionRejectedReason::RequesterClosed,
                ..
            }
        )
    }));
}

#[test]
fn scripted_udp_pending_recv_completes_when_datagram_becomes_ready() {
    let observed = Rc::new(RefCell::new(Vec::new()));
    let mut sim = Simulator::new(TestShard, udp_sim_config());
    let probe = sim.register(UdpProbe {
        bind_addr: local_addr(48002),
        observed: Rc::clone(&observed),
    });
    sim.try_send(probe, UdpProbeMsg::Bind).unwrap();
    sim.run_until_quiescent();

    let socket = UdpSocketId::new(1);
    sim.try_send(probe, UdpProbeMsg::RecvOne(socket)).unwrap();
    sim.run_until_quiescent();

    assert!(
        observed
            .borrow()
            .iter()
            .any(|entry| entry == "recv:scripted:false")
    );
    assert_eq!(count_call_completed(sim.trace(), CallKind::UdpRecvFrom), 1);
    assert!(!sim.has_in_flight_calls());
}

#[test]
fn scripted_udp_loopback_recv_capacity_full_is_visible() {
    let observed = Rc::new(RefCell::new(Vec::new()));
    let mut sim = Simulator::new(
        TestShard,
        SimulatorConfig {
            udp: ScriptedUdpConfig {
                pending_completion_capacity: 16,
                sockets: vec![
                    ScriptedUdpSocketConfig {
                        bind_addr: local_addr(48020),
                        local_addr: local_addr(48020),
                        recv_capacity: 1,
                        inbound_datagrams: vec![udp_datagram(99, peer_addr(58120), b"held")],
                    },
                    udp_socket_script(local_addr(48021), local_addr(48021), Vec::new()),
                ],
            },
            ..Default::default()
        },
    );
    let client = sim.register(UdpClient {
        receiver_addr: local_addr(48020),
        sender_addr: local_addr(48021),
        receiver: None,
        sender: None,
        observed: Rc::clone(&observed),
    });

    sim.try_send(client, UdpClientMsg::Start).unwrap();
    sim.run_until_quiescent();

    assert!(
        observed.borrow().iter().any(|entry| entry == "failed"),
        "full scripted UDP receive queue should surface as call failure"
    );
    assert!(sim.trace().iter().any(|event| {
        matches!(
            event.kind(),
            RuntimeEventKind::CallFailed {
                call_kind: CallKind::UdpSendTo,
                reason: CallError::ResourceBusy,
                ..
            }
        )
    }));
}

#[test]
fn scripted_udp_completion_capacity_is_separate_from_tcp_completion_capacity() {
    let observed = Rc::new(RefCell::new(Vec::new()));
    let mut sim = Simulator::new(
        TestShard,
        SimulatorConfig {
            seed: 0,
            faults: FaultConfig {
                tcp_completion: TcpCompletionFaultMode::DelayBySteps {
                    one_in: 1,
                    steps: 8,
                },
                ..Default::default()
            },
            tcp: ScriptedTcpConfig {
                pending_completion_capacity: 1,
                listeners: vec![ScriptedListenerConfig {
                    bind_addr: bind_addr(),
                    local_addr: local_addr(48030),
                    backlog_capacity: 1,
                    peers: vec![peer_script(
                        1,
                        peer_addr(58130),
                        vec![b"tcp".to_vec()],
                        None,
                        8,
                    )],
                }],
            },
            udp: ScriptedUdpConfig {
                pending_completion_capacity: 1,
                sockets: vec![
                    udp_socket_script(local_addr(48031), local_addr(48031), Vec::new()),
                    udp_socket_script(local_addr(48032), local_addr(48032), Vec::new()),
                ],
            },
            dns: Default::default(),
            process: Default::default(),
            storage: Default::default(),
        },
    );

    let (listener, _) = bind_listener(&mut sim, bind_addr());
    let (stream, _) = accept_stream(&mut sim, listener);
    let read_log = Rc::new(RefCell::new(Vec::new()));
    let reader = sim.register(ReadProbe {
        stream,
        max_len: 8,
        log: Rc::clone(&read_log),
    });
    sim.try_send(reader, ReadProbeMsg::StartRead).unwrap();
    sim.step();

    let client = sim.register(UdpClient {
        receiver_addr: local_addr(48031),
        sender_addr: local_addr(48032),
        receiver: None,
        sender: None,
        observed: Rc::clone(&observed),
    });
    sim.try_send(client, UdpClientMsg::Start).unwrap();
    sim.run_until_quiescent();

    assert!(
        observed.borrow().iter().any(|entry| entry == "sent:6"),
        "one pending TCP completion must not exhaust the UDP completion lane"
    );
    assert!(
        read_log
            .borrow()
            .iter()
            .any(|entry| entry == &ReadProbeMsg::ReadCompleted(b"tcp".to_vec()))
    );
}

fn bind_listener(
    sim: &mut Simulator<TestShard>,
    bind_addr: SocketAddr,
) -> (ListenerId, SocketAddr) {
    let listener_slot = Arc::new(Mutex::new(None));
    let addr_slot = Arc::new(Mutex::new(None));
    let binder = sim.register(Binder {
        bind_addr,
        listener_slot: Arc::clone(&listener_slot),
        addr_slot: Arc::clone(&addr_slot),
    });
    sim.try_send(binder, BinderMsg::StartBind).unwrap();
    sim.step();
    sim.step();
    (
        listener_slot
            .lock()
            .expect("listener mutex")
            .expect("listener"),
        addr_slot.lock().expect("addr mutex").expect("local addr"),
    )
}

fn accept_stream(sim: &mut Simulator<TestShard>, listener: ListenerId) -> (StreamId, SocketAddr) {
    let stream_slot = Arc::new(Mutex::new(None));
    let peer_slot = Arc::new(Mutex::new(None));
    let acceptor = sim.register(PeerAwareAcceptor {
        listener,
        stream_slot: Arc::clone(&stream_slot),
        peer_slot: Arc::clone(&peer_slot),
    });
    sim.try_send(acceptor, PeerAwareAcceptMsg::StartAccept)
        .unwrap();
    sim.run_until_quiescent();
    (
        stream_slot
            .lock()
            .expect("stream mutex")
            .expect("accepted stream"),
        peer_slot.lock().expect("peer mutex").expect("peer addr"),
    )
}

#[derive(Debug)]
struct ReplayArtifactData {
    artifact: tina_sim::ReplayArtifact,
    bound_addr: Option<SocketAddr>,
}

#[derive(Debug, Default)]
struct FirstAcceptOrderChecker {
    second_waiter: Option<IsolateId>,
    saw_accept: bool,
}

impl Checker for FirstAcceptOrderChecker {
    fn name(&self) -> &'static str {
        "first-accept-order-checker"
    }

    fn on_event(&mut self, event: &RuntimeEvent) -> CheckerDecision {
        match event.kind() {
            RuntimeEventKind::CallCompleted {
                call_kind: CallKind::TcpAccept,
                ..
            } if !self.saw_accept => {
                self.saw_accept = true;
                if self
                    .second_waiter
                    .is_some_and(|waiter| waiter == event.isolate())
                {
                    CheckerDecision::Fail("second waiter accepted before first".into())
                } else {
                    CheckerDecision::Continue
                }
            }
            _ => CheckerDecision::Continue,
        }
    }
}

#[test]
fn scripted_tcp_connect_client_round_trips_one_peer() {
    let remote_addr = local_addr(42100);
    let observed = Arc::new(Mutex::new(None));
    let mut sim = Simulator::new(
        TestShard,
        SimulatorConfig {
            tcp: ScriptedTcpConfig {
                pending_completion_capacity: 16,
                listeners: vec![ScriptedListenerConfig {
                    bind_addr: remote_addr,
                    local_addr: remote_addr,
                    backlog_capacity: 1,
                    peers: vec![peer_script(
                        1,
                        peer_addr(61001),
                        vec![b"pong".to_vec()],
                        Some(4),
                        2,
                    )],
                }],
            },
            ..SimulatorConfig::default()
        },
    );
    let client = sim.register(Client {
        payload: b"ping".to_vec(),
        pending_write: Vec::new(),
        stream: None,
        observed: Arc::clone(&observed),
    });

    sim.try_send(client, ClientMsg::Start(remote_addr)).unwrap();
    sim.run_until_quiescent();
    let artifact = sim.replay_artifact();

    assert_eq!(
        observed.lock().expect("observed mutex").as_deref(),
        Some(&b"pong"[..])
    );
    assert_eq!(
        artifact
            .observed_peer_output()
            .iter()
            .map(ObservedPeerOutput::bytes)
            .collect::<Vec<_>>(),
        vec![b"ping".as_slice()]
    );
    assert_eq!(count_call_completed(sim.trace(), CallKind::TcpConnect), 1);
    assert_eq!(count_call_completed(sim.trace(), CallKind::TcpWrite), 2);
    assert_eq!(count_call_completed(sim.trace(), CallKind::TcpRead), 1);
    assert_eq!(
        count_call_completed(sim.trace(), CallKind::TcpStreamClose),
        1
    );
}

#[test]
fn simulated_file_client_writes_syncs_reads_and_closes() {
    let payload = b"simulated llama snapshot\n".to_vec();
    let observed = Arc::new(Mutex::new(None));
    let mut sim = Simulator::new(TestShard, SimulatorConfig::default());
    let client = sim.register(FileClient {
        payload: payload.clone(),
        file: None,
        size: None,
        observed: Arc::clone(&observed),
    });

    sim.try_send(
        client,
        FileClientMsg::Start {
            dir: PathBuf::from("/tmp/tina-sim"),
            path: PathBuf::from("/tmp/tina-sim/snapshot.txt"),
        },
    )
    .unwrap();
    sim.run_until_quiescent();

    assert_eq!(
        *observed.lock().expect("file observed mutex"),
        Some((payload.len() as u64, payload))
    );
    assert_eq!(count_call_completed(sim.trace(), CallKind::Mkdir), 1);
    assert_eq!(count_call_completed(sim.trace(), CallKind::FileOpen), 1);
    assert_eq!(count_call_completed(sim.trace(), CallKind::FileWriteAt), 1);
    assert_eq!(count_call_completed(sim.trace(), CallKind::FileFsync), 1);
    assert_eq!(count_call_completed(sim.trace(), CallKind::FileSize), 1);
    assert_eq!(count_call_completed(sim.trace(), CallKind::FileReadAt), 1);
    assert_eq!(count_call_completed(sim.trace(), CallKind::FileClose), 1);
}

#[test]
fn scripted_tcp_echo_round_trips_one_client_payload() {
    let payload = b"hello from simulated tcp".to_vec();
    let (run, listener_isolate) = run_echo_scenario(
        vec![peer_script(
            1,
            peer_addr(51001),
            vec![payload.clone()],
            None,
            payload.len(),
        )],
        256,
        SimulatorConfig::default(),
    );

    assert_eq!(run.bound_addr, Some(local_addr(41000)));
    assert_eq!(
        run.artifact
            .observed_peer_output()
            .iter()
            .map(ObservedPeerOutput::bytes)
            .collect::<Vec<_>>(),
        vec![payload.as_slice()]
    );
    assert_eq!(
        count_call_completed(run.artifact.event_record(), CallKind::TcpBind),
        1
    );
    assert_eq!(
        count_call_completed(run.artifact.event_record(), CallKind::TcpAccept),
        1
    );
    assert!(count_call_completed(run.artifact.event_record(), CallKind::TcpRead) >= 2);
    assert_eq!(
        count_call_completed(run.artifact.event_record(), CallKind::TcpWrite),
        1
    );
    assert_eq!(
        count_call_completed(run.artifact.event_record(), CallKind::TcpStreamClose),
        1
    );
    assert_eq!(
        count_call_completed(run.artifact.event_record(), CallKind::TcpListenerClose),
        1
    );
    assert_eq!(count_spawned(run.artifact.event_record()), 1);
    assert!(run.artifact.event_record().iter().any(|event| {
        event.isolate() == listener_isolate
            && matches!(event.kind(), RuntimeEventKind::IsolateStopped)
    }));
}

#[test]
fn scripted_tcp_echo_handles_overlap_and_partial_io() {
    let first = b"abcdef".to_vec();
    let second = b"uvwxyz".to_vec();
    let (run, _) = run_echo_scenario(
        vec![
            peer_script(1, peer_addr(51011), vec![first.clone()], Some(2), 2),
            peer_script(1, peer_addr(51012), vec![second.clone()], Some(2), 2),
        ],
        256,
        SimulatorConfig::default(),
    );

    let mut observed = run
        .artifact
        .observed_peer_output()
        .iter()
        .map(|output| output.bytes().to_vec())
        .collect::<Vec<_>>();
    observed.sort();
    let mut expected = vec![first, second];
    expected.sort();
    assert_eq!(observed, expected);
    assert_eq!(
        count_call_completed(run.artifact.event_record(), CallKind::TcpAccept),
        2
    );
    assert!(count_call_completed(run.artifact.event_record(), CallKind::TcpWrite) >= 4);
    assert!(count_call_completed(run.artifact.event_record(), CallKind::TcpRead) >= 6);
    assert_eq!(count_spawned(run.artifact.event_record()), 2);
}

#[test]
fn scripted_tcp_echo_handles_tangled_overlap_with_single_byte_drain() {
    let first = b"abcdef".to_vec();
    let second = b"uvwxyz".to_vec();
    let (run, _) = run_echo_scenario(
        vec![
            peer_script(
                1,
                peer_addr(51013),
                vec![b"ab".to_vec(), b"cd".to_vec(), b"ef".to_vec()],
                Some(1),
                1,
            ),
            peer_script(
                1,
                peer_addr(51014),
                vec![b"uv".to_vec(), b"wx".to_vec(), b"yz".to_vec()],
                Some(1),
                1,
            ),
        ],
        256,
        SimulatorConfig {
            seed: 0,
            faults: FaultConfig {
                tcp_completion: TcpCompletionFaultMode::ReorderReady { one_in: 1 },
                ..Default::default()
            },
            ..Default::default()
        },
    );

    let mut observed = run
        .artifact
        .observed_peer_output()
        .iter()
        .map(|output| output.bytes().to_vec())
        .collect::<Vec<_>>();
    observed.sort();
    let mut expected = vec![first.clone(), second.clone()];
    expected.sort();
    assert_eq!(observed, expected);
    assert_eq!(
        count_call_completed(run.artifact.event_record(), CallKind::TcpAccept),
        2
    );
    assert_eq!(
        count_call_completed(run.artifact.event_record(), CallKind::TcpWrite),
        first.len() + second.len()
    );
    assert_eq!(
        count_call_completed(run.artifact.event_record(), CallKind::TcpRead),
        first.len() + second.len() + 2
    );
    assert_eq!(
        count_call_completed(run.artifact.event_record(), CallKind::TcpStreamClose),
        2
    );
    assert_eq!(count_spawned(run.artifact.event_record()), 2);
}

#[test]
fn scripted_tcp_echo_handles_sequential_multi_client_flow() {
    let payloads = vec![
        b"first-sequential".to_vec(),
        b"second-sequential".to_vec(),
        b"third-sequential".to_vec(),
    ];
    let (run, listener_isolate) = run_echo_scenario(
        vec![
            peer_script(1, peer_addr(51021), vec![payloads[0].clone()], Some(4), 3),
            peer_script(5, peer_addr(51022), vec![payloads[1].clone()], Some(4), 3),
            peer_script(9, peer_addr(51023), vec![payloads[2].clone()], Some(4), 3),
        ],
        256,
        SimulatorConfig::default(),
    );

    let observed = run
        .artifact
        .observed_peer_output()
        .iter()
        .map(|output| output.bytes().to_vec())
        .collect::<Vec<_>>();
    assert_eq!(observed, payloads);
    assert_eq!(
        count_call_completed(run.artifact.event_record(), CallKind::TcpBind),
        1
    );
    assert_eq!(
        count_call_completed(run.artifact.event_record(), CallKind::TcpAccept),
        3
    );
    assert_eq!(
        count_call_completed(run.artifact.event_record(), CallKind::TcpListenerClose),
        1
    );
    assert_eq!(
        count_call_completed(run.artifact.event_record(), CallKind::TcpStreamClose),
        3
    );
    assert_eq!(count_spawned(run.artifact.event_record()), 3);
    assert!(run.artifact.event_record().iter().any(|event| {
        event.isolate() == listener_isolate
            && matches!(event.kind(), RuntimeEventKind::IsolateStopped)
    }));
}

#[test]
fn accept_reports_peer_addr_and_read_write_use_live_result_shapes() {
    let mut sim = Simulator::new(
        TestShard,
        SimulatorConfig {
            seed: 0,
            faults: FaultConfig {
                tcp_completion: TcpCompletionFaultMode::DelayBySteps {
                    one_in: 1,
                    steps: 1,
                },
                ..Default::default()
            },
            tcp: ScriptedTcpConfig {
                pending_completion_capacity: 8,
                listeners: vec![ScriptedListenerConfig {
                    bind_addr: bind_addr(),
                    local_addr: local_addr(48000),
                    backlog_capacity: 1,
                    peers: vec![peer_script(
                        1,
                        peer_addr(58010),
                        vec![b"hello".to_vec()],
                        None,
                        3,
                    )],
                }],
            },
            storage: Default::default(),
            udp: Default::default(),
            dns: Default::default(),
            process: Default::default(),
        },
    );

    let (listener, bound_addr) = bind_listener(&mut sim, bind_addr());
    assert_eq!(bound_addr, local_addr(48000));

    let (stream, accepted_peer) = accept_stream(&mut sim, listener);
    assert_eq!(accepted_peer, peer_addr(58010));

    let read_log = Rc::new(RefCell::new(Vec::new()));
    let reader = sim.register(ReadProbe {
        stream,
        max_len: 16,
        log: Rc::clone(&read_log),
    });
    sim.try_send(reader, ReadProbeMsg::StartRead).unwrap();
    sim.run_until_quiescent();
    assert_eq!(
        *read_log.borrow(),
        vec![ReadProbeMsg::ReadCompleted(b"hello".to_vec())]
    );

    let write_log = Rc::new(RefCell::new(Vec::new()));
    let writer = sim.register(WriteProbe {
        stream,
        bytes: b"goodbye".to_vec(),
        log: Rc::clone(&write_log),
    });
    sim.try_send(writer, WriteProbeMsg::StartWrite).unwrap();
    sim.run_until_quiescent();
    assert_eq!(*write_log.borrow(), vec![WriteProbeMsg::Wrote(3)]);
    assert_eq!(
        sim.replay_artifact()
            .observed_peer_output()
            .iter()
            .map(ObservedPeerOutput::bytes)
            .collect::<Vec<_>>(),
        vec![b"goo".as_slice()]
    );
}

#[test]
fn invalid_tcp_resources_surface_failures() {
    let log = Rc::new(RefCell::new(Vec::new()));
    let mut sim = Simulator::new(TestShard, SimulatorConfig::default());
    let probe = sim.register(Probe {
        log: Rc::clone(&log),
    });
    sim.try_send(probe, ProbeMsg::StartInvalidAccept).unwrap();
    sim.try_send(probe, ProbeMsg::StartInvalidRead).unwrap();
    sim.try_send(probe, ProbeMsg::StartInvalidFileOpen).unwrap();
    sim.run_until_quiescent();

    assert_eq!(
        *log.borrow(),
        vec![
            ProbeMsg::InvalidResourceObserved,
            ProbeMsg::InvalidResourceObserved,
            ProbeMsg::FileIoObserved,
        ]
    );
    assert!(sim.trace().iter().any(|event| {
        matches!(
            event.kind(),
            RuntimeEventKind::CallFailed {
                call_kind: CallKind::TcpAccept,
                reason: CallError::InvalidResource,
                ..
            }
        )
    }));
    assert!(sim.trace().iter().any(|event| {
        matches!(
            event.kind(),
            RuntimeEventKind::CallFailed {
                call_kind: CallKind::TcpRead,
                reason: CallError::InvalidResource,
                ..
            }
        )
    }));
    assert!(sim.trace().iter().any(|event| {
        matches!(
            event.kind(),
            RuntimeEventKind::CallFailed {
                call_kind: CallKind::FileOpen,
                reason: CallError::Io,
                ..
            }
        )
    }));
}

#[test]
fn closed_stream_id_surfaces_invalid_resource() {
    let mut sim = Simulator::new(
        TestShard,
        SimulatorConfig {
            seed: 0,
            faults: FaultConfig {
                tcp_completion: TcpCompletionFaultMode::DelayBySteps {
                    one_in: 1,
                    steps: 1,
                },
                ..Default::default()
            },
            tcp: ScriptedTcpConfig {
                pending_completion_capacity: 8,
                listeners: vec![ScriptedListenerConfig {
                    bind_addr: bind_addr(),
                    local_addr: local_addr(48500),
                    backlog_capacity: 1,
                    peers: vec![peer_script(
                        1,
                        peer_addr(58501),
                        vec![b"x".to_vec()],
                        None,
                        1,
                    )],
                }],
            },
            storage: Default::default(),
            udp: Default::default(),
            dns: Default::default(),
            process: Default::default(),
        },
    );

    let (listener, _) = bind_listener(&mut sim, bind_addr());
    let (stream, _) = accept_stream(&mut sim, listener);
    let closer = sim.register(StreamCloser { stream });
    sim.try_send(closer, CloserMsg::CloseStream).unwrap();
    sim.run_until_quiescent();

    let log = Rc::new(RefCell::new(Vec::new()));
    let reader = sim.register(ReadProbe {
        stream,
        max_len: 8,
        log: Rc::clone(&log),
    });
    sim.try_send(reader, ReadProbeMsg::StartRead).unwrap();
    sim.run_until_quiescent();

    assert_eq!(*log.borrow(), vec![ReadProbeMsg::Failed]);
    assert!(sim.trace().iter().any(|event| {
        matches!(
            event.kind(),
            RuntimeEventKind::CallFailed {
                call_kind: CallKind::TcpRead,
                reason: CallError::InvalidResource,
                ..
            }
        )
    }));
}

#[test]
fn pending_completion_capacity_exhaustion_surfaces_io_failure() {
    let mut sim = Simulator::new(
        TestShard,
        SimulatorConfig {
            seed: 0,
            faults: FaultConfig {
                tcp_completion: TcpCompletionFaultMode::DelayBySteps {
                    one_in: 1,
                    steps: 1,
                },
                ..Default::default()
            },
            tcp: ScriptedTcpConfig {
                pending_completion_capacity: 1,
                listeners: vec![ScriptedListenerConfig {
                    bind_addr: bind_addr(),
                    local_addr: local_addr(48550),
                    backlog_capacity: 2,
                    peers: vec![
                        peer_script(1, peer_addr(58550), vec![b"chunk".to_vec()], None, 8),
                        peer_script(1, peer_addr(58551), vec![b"other".to_vec()], None, 8),
                    ],
                }],
            },
            storage: Default::default(),
            udp: Default::default(),
            dns: Default::default(),
            process: Default::default(),
        },
    );

    let (listener, _) = bind_listener(&mut sim, bind_addr());
    let (stream, _) = accept_stream(&mut sim, listener);
    let (other_stream, _) = accept_stream(&mut sim, listener);

    let read_log = Rc::new(RefCell::new(Vec::new()));
    let reader_one = sim.register(ReadProbe {
        stream,
        max_len: 8,
        log: Rc::clone(&read_log),
    });
    let reader_two = sim.register(ReadProbe {
        stream: other_stream,
        max_len: 8,
        log: Rc::clone(&read_log),
    });

    sim.try_send(reader_one, ReadProbeMsg::StartRead).unwrap();
    sim.try_send(reader_two, ReadProbeMsg::StartRead).unwrap();
    sim.run_until_quiescent();

    assert_eq!(
        *read_log.borrow(),
        vec![
            ReadProbeMsg::Failed,
            ReadProbeMsg::ReadCompleted(b"chunk".to_vec())
        ]
    );
    assert!(sim.trace().iter().any(|event| {
        matches!(
            event.kind(),
            RuntimeEventKind::CallFailed {
                call_kind: CallKind::TcpRead,
                reason: CallError::Io,
                ..
            }
        )
    }));
}

#[test]
fn listener_close_while_accept_pending_fails_resource_busy() {
    let mut sim = Simulator::new(
        TestShard,
        SimulatorConfig {
            seed: 0,
            faults: FaultConfig {
                tcp_completion: TcpCompletionFaultMode::DelayBySteps {
                    one_in: 1,
                    steps: 1,
                },
                ..Default::default()
            },
            tcp: ScriptedTcpConfig {
                pending_completion_capacity: 8,
                listeners: vec![ScriptedListenerConfig {
                    bind_addr: bind_addr(),
                    local_addr: local_addr(42000),
                    backlog_capacity: 1,
                    peers: vec![peer_script(
                        10,
                        peer_addr(52001),
                        vec![b"later".to_vec()],
                        None,
                        5,
                    )],
                }],
            },
            storage: Default::default(),
            udp: Default::default(),
            dns: Default::default(),
            process: Default::default(),
        },
    );

    let (listener_resource, _) = bind_listener(&mut sim, bind_addr());
    let log = Rc::new(RefCell::new(Vec::new()));
    let waiter = sim.register(Waiter {
        listener: listener_resource,
        log: Rc::clone(&log),
    });
    let closer = sim.register(ListenerCloser {
        listener: listener_resource,
    });

    sim.try_send(waiter, WaiterMsg::StartAccept).unwrap();
    sim.step();
    sim.try_send(closer, CloserMsg::CloseListener).unwrap();
    sim.run_until_quiescent();

    assert_eq!(*log.borrow(), vec![WaiterMsg::Accepted]);
    assert!(sim.trace().iter().any(|event| {
        matches!(
            event.kind(),
            RuntimeEventKind::CallFailed {
                call_kind: CallKind::TcpListenerClose,
                reason: CallError::ResourceBusy,
                ..
            }
        )
    }));
    assert_eq!(
        count_call_completed(sim.trace(), CallKind::TcpListenerClose),
        0
    );
}

#[test]
fn same_resource_second_read_fails_resource_busy_under_tcp_delay_faults() {
    let mut sim = Simulator::new(
        TestShard,
        SimulatorConfig {
            faults: FaultConfig {
                tcp_completion: TcpCompletionFaultMode::DelayBySteps {
                    one_in: 2,
                    steps: 1,
                },
                ..Default::default()
            },
            tcp: ScriptedTcpConfig {
                pending_completion_capacity: 8,
                listeners: vec![ScriptedListenerConfig {
                    bind_addr: bind_addr(),
                    local_addr: local_addr(48575),
                    backlog_capacity: 1,
                    peers: vec![peer_script(
                        1,
                        peer_addr(58575),
                        vec![b"first".to_vec(), b"second".to_vec()],
                        None,
                        8,
                    )],
                }],
            },
            ..Default::default()
        },
    );

    let (listener, _) = bind_listener(&mut sim, bind_addr());
    let (stream, _) = accept_stream(&mut sim, listener);

    let first_log = Rc::new(RefCell::new(Vec::new()));
    let second_log = Rc::new(RefCell::new(Vec::new()));
    let first_reader = sim.register(ReadProbe {
        stream,
        max_len: 5,
        log: Rc::clone(&first_log),
    });
    let second_reader = sim.register(ReadProbe {
        stream,
        max_len: 6,
        log: Rc::clone(&second_log),
    });

    sim.try_send(first_reader, ReadProbeMsg::StartRead).unwrap();
    sim.try_send(second_reader, ReadProbeMsg::StartRead)
        .unwrap();
    sim.run_until_quiescent();

    assert_eq!(
        *first_log.borrow(),
        vec![ReadProbeMsg::ReadCompleted(b"first".to_vec())]
    );
    assert_eq!(*second_log.borrow(), vec![ReadProbeMsg::Failed]);
    assert!(sim.trace().iter().any(|event| {
        matches!(
            event.kind(),
            RuntimeEventKind::CallFailed {
                call_kind: CallKind::TcpRead,
                reason: CallError::ResourceBusy,
                ..
            }
        )
    }));
}

#[test]
fn read_and_write_on_same_stream_use_separate_lanes() {
    let mut sim = Simulator::new(
        TestShard,
        SimulatorConfig {
            faults: FaultConfig {
                tcp_completion: TcpCompletionFaultMode::DelayBySteps {
                    one_in: 1,
                    steps: 1,
                },
                ..Default::default()
            },
            tcp: ScriptedTcpConfig {
                pending_completion_capacity: 8,
                listeners: vec![ScriptedListenerConfig {
                    bind_addr: bind_addr(),
                    local_addr: local_addr(48590),
                    backlog_capacity: 1,
                    peers: vec![peer_script(
                        1,
                        peer_addr(58590),
                        vec![b"input".to_vec()],
                        None,
                        8,
                    )],
                }],
            },
            ..Default::default()
        },
    );

    let (listener, _) = bind_listener(&mut sim, bind_addr());
    let (stream, _) = accept_stream(&mut sim, listener);

    let read_log = Rc::new(RefCell::new(Vec::new()));
    let write_log = Rc::new(RefCell::new(Vec::new()));
    let reader = sim.register(ReadProbe {
        stream,
        max_len: 8,
        log: Rc::clone(&read_log),
    });
    let writer = sim.register(WriteProbe {
        stream,
        bytes: b"output".to_vec(),
        log: Rc::clone(&write_log),
    });

    sim.try_send(reader, ReadProbeMsg::StartRead).unwrap();
    sim.try_send(writer, WriteProbeMsg::StartWrite).unwrap();
    sim.run_until_quiescent();

    assert_eq!(
        *read_log.borrow(),
        vec![ReadProbeMsg::ReadCompleted(b"input".to_vec())]
    );
    assert_eq!(*write_log.borrow(), vec![WriteProbeMsg::Wrote(6)]);
    assert!(
        !sim.trace().iter().any(|event| matches!(
            event.kind(),
            RuntimeEventKind::CallFailed {
                reason: CallError::ResourceBusy,
                ..
            }
        )),
        "different TCP lanes on one stream must not fail as ResourceBusy"
    );
}

#[test]
fn stream_close_fails_pending_read() {
    let mut sim = Simulator::new(
        TestShard,
        SimulatorConfig {
            faults: FaultConfig {
                tcp_completion: TcpCompletionFaultMode::DelayBySteps {
                    one_in: 1,
                    steps: 1,
                },
                ..Default::default()
            },
            tcp: ScriptedTcpConfig {
                pending_completion_capacity: 8,
                listeners: vec![ScriptedListenerConfig {
                    bind_addr: bind_addr(),
                    local_addr: local_addr(48600),
                    backlog_capacity: 1,
                    peers: vec![peer_script(
                        1,
                        peer_addr(58601),
                        vec![b"soon".to_vec()],
                        None,
                        4,
                    )],
                }],
            },
            ..Default::default()
        },
    );

    let (listener, _) = bind_listener(&mut sim, bind_addr());
    let (stream, _) = accept_stream(&mut sim, listener);

    let read_log = Rc::new(RefCell::new(Vec::new()));
    let reader = sim.register(ReadProbe {
        stream,
        max_len: 8,
        log: Rc::clone(&read_log),
    });
    sim.try_send(reader, ReadProbeMsg::StartRead).unwrap();
    sim.step();

    let closer = sim.register(StreamCloser { stream });
    sim.try_send(closer, CloserMsg::CloseStream).unwrap();
    sim.run_until_quiescent();

    assert_eq!(
        *read_log.borrow(),
        vec![ReadProbeMsg::ReadCompleted(b"soon".to_vec())]
    );
    assert!(sim.trace().iter().any(|event| {
        matches!(
            event.kind(),
            RuntimeEventKind::CallFailed {
                call_kind: CallKind::TcpStreamClose,
                reason: CallError::ResourceBusy,
                ..
            }
        )
    }));
    assert_eq!(
        count_call_completed(sim.trace(), CallKind::TcpStreamClose),
        0
    );
}

#[test]
fn stream_close_fails_pending_write() {
    let mut sim = Simulator::new(
        TestShard,
        SimulatorConfig {
            faults: FaultConfig {
                tcp_completion: TcpCompletionFaultMode::DelayBySteps {
                    one_in: 1,
                    steps: 1,
                },
                ..Default::default()
            },
            tcp: ScriptedTcpConfig {
                pending_completion_capacity: 8,
                listeners: vec![ScriptedListenerConfig {
                    bind_addr: bind_addr(),
                    local_addr: local_addr(48700),
                    backlog_capacity: 1,
                    peers: vec![peer_script(
                        1,
                        peer_addr(58701),
                        vec![b"x".to_vec()],
                        None,
                        8,
                    )],
                }],
            },
            ..Default::default()
        },
    );

    let (listener, _) = bind_listener(&mut sim, bind_addr());
    let (stream, _) = accept_stream(&mut sim, listener);

    let write_log = Rc::new(RefCell::new(Vec::new()));
    let writer = sim.register(WriteProbe {
        stream,
        bytes: b"payload".to_vec(),
        log: Rc::clone(&write_log),
    });
    sim.try_send(writer, WriteProbeMsg::StartWrite).unwrap();
    sim.step();

    let closer = sim.register(StreamCloser { stream });
    sim.try_send(closer, CloserMsg::CloseStream).unwrap();
    sim.run_until_quiescent();

    assert_eq!(*write_log.borrow(), vec![WriteProbeMsg::Wrote(7)]);
    assert!(sim.trace().iter().any(|event| {
        matches!(
            event.kind(),
            RuntimeEventKind::CallFailed {
                call_kind: CallKind::TcpStreamClose,
                reason: CallError::ResourceBusy,
                ..
            }
        )
    }));
    assert_eq!(
        count_call_completed(sim.trace(), CallKind::TcpStreamClose),
        0
    );
}

#[test]
fn stopped_requester_rejects_pending_accept_completion() {
    let log = Rc::new(RefCell::new(Vec::new()));
    let mut sim = Simulator::new(
        TestShard,
        SimulatorConfig {
            tcp: ScriptedTcpConfig {
                pending_completion_capacity: 8,
                listeners: vec![ScriptedListenerConfig {
                    bind_addr: bind_addr(),
                    local_addr: local_addr(43000),
                    backlog_capacity: 1,
                    peers: vec![peer_script(
                        5,
                        peer_addr(53001),
                        vec![b"hi".to_vec()],
                        None,
                        2,
                    )],
                }],
            },
            ..Default::default()
        },
    );

    let listener_slot = Arc::new(Mutex::new(None));
    let addr_slot = Arc::new(Mutex::new(None));
    let binder = sim.register(Binder {
        bind_addr: bind_addr(),
        listener_slot: Arc::clone(&listener_slot),
        addr_slot: Arc::clone(&addr_slot),
    });
    sim.try_send(binder, BinderMsg::StartBind).unwrap();
    sim.step();
    sim.step();

    let listener = listener_slot
        .lock()
        .expect("listener mutex")
        .expect("listener");
    let waiter = sim.register(Waiter {
        listener,
        log: Rc::clone(&log),
    });
    sim.try_send(waiter, WaiterMsg::StartAccept).unwrap();
    sim.step();
    sim.try_send(waiter, WaiterMsg::StopNow).unwrap();
    assert_eq!(sim.step(), 1);
    assert!(
        !sim.has_in_flight_calls(),
        "stopped requester should cancel its pending accept immediately"
    );
    sim.advance_time(Duration::from_millis(10));
    assert_eq!(sim.step(), 0);

    assert!(log.borrow().is_empty());
    assert!(sim.trace().iter().any(|event| {
        matches!(
            event.kind(),
            RuntimeEventKind::CallCompletionRejected {
                call_kind: CallKind::TcpAccept,
                reason: CallCompletionRejectedReason::RequesterClosed,
                ..
            }
        )
    }));
}

#[test]
fn stopped_requester_rejects_pending_read_completion() {
    let mut sim = Simulator::new(
        TestShard,
        SimulatorConfig {
            faults: FaultConfig {
                tcp_completion: TcpCompletionFaultMode::DelayBySteps {
                    one_in: 1,
                    steps: 1,
                },
                ..Default::default()
            },
            tcp: ScriptedTcpConfig {
                pending_completion_capacity: 8,
                listeners: vec![ScriptedListenerConfig {
                    bind_addr: bind_addr(),
                    local_addr: local_addr(48800),
                    backlog_capacity: 1,
                    peers: vec![peer_script(
                        1,
                        peer_addr(58801),
                        vec![b"later".to_vec()],
                        None,
                        5,
                    )],
                }],
            },
            ..Default::default()
        },
    );

    let (listener, _) = bind_listener(&mut sim, bind_addr());
    let (stream, _) = accept_stream(&mut sim, listener);

    let read_log = Rc::new(RefCell::new(Vec::new()));
    let reader = sim.register(ReadProbe {
        stream,
        max_len: 8,
        log: Rc::clone(&read_log),
    });
    sim.try_send(reader, ReadProbeMsg::StartRead).unwrap();
    sim.step();
    sim.try_send(reader, ReadProbeMsg::StopNow).unwrap();
    assert_eq!(sim.step(), 1);
    assert!(
        !sim.has_in_flight_calls(),
        "stopped requester should cancel its pending read immediately"
    );
    for _ in 0..3 {
        assert_eq!(sim.step(), 0);
    }

    assert!(read_log.borrow().is_empty());
    assert!(sim.trace().iter().any(|event| {
        matches!(
            event.kind(),
            RuntimeEventKind::CallCompletionRejected {
                call_kind: CallKind::TcpRead,
                reason: CallCompletionRejectedReason::RequesterClosed,
                ..
            }
        )
    }));
}

#[test]
fn stopped_requester_rejects_pending_write_completion() {
    let mut sim = Simulator::new(
        TestShard,
        SimulatorConfig {
            faults: FaultConfig {
                tcp_completion: TcpCompletionFaultMode::DelayBySteps {
                    one_in: 1,
                    steps: 1,
                },
                ..Default::default()
            },
            tcp: ScriptedTcpConfig {
                pending_completion_capacity: 8,
                listeners: vec![ScriptedListenerConfig {
                    bind_addr: bind_addr(),
                    local_addr: local_addr(48900),
                    backlog_capacity: 1,
                    peers: vec![peer_script(
                        1,
                        peer_addr(58901),
                        vec![b"z".to_vec()],
                        None,
                        8,
                    )],
                }],
            },
            ..Default::default()
        },
    );

    let (listener, _) = bind_listener(&mut sim, bind_addr());
    let (stream, _) = accept_stream(&mut sim, listener);

    let write_log = Rc::new(RefCell::new(Vec::new()));
    let writer = sim.register(WriteProbe {
        stream,
        bytes: b"payload".to_vec(),
        log: Rc::clone(&write_log),
    });
    sim.try_send(writer, WriteProbeMsg::StartWrite).unwrap();
    sim.step();
    sim.try_send(writer, WriteProbeMsg::StopNow).unwrap();
    assert_eq!(sim.step(), 1);
    assert!(
        !sim.has_in_flight_calls(),
        "stopped requester should cancel its pending write immediately"
    );
    for _ in 0..3 {
        assert_eq!(sim.step(), 0);
    }

    assert!(write_log.borrow().is_empty());
    assert!(sim.trace().iter().any(|event| {
        matches!(
            event.kind(),
            RuntimeEventKind::CallCompletionRejected {
                call_kind: CallKind::TcpWrite,
                reason: CallCompletionRejectedReason::RequesterClosed,
                ..
            }
        )
    }));
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CancellationOp {
    Accept,
    Read,
    Write,
}

fn cancellation_history(seed: u64) -> History<CancellationOp> {
    let op = match seed % 3 {
        0 => CancellationOp::Accept,
        1 => CancellationOp::Read,
        _ => CancellationOp::Write,
    };
    History::new("tcp cancellation matrix", seed, vec![op])
}

fn run_seeded_cancellation_case(history: &History<CancellationOp>) -> DstRun<(), ReplayArtifact> {
    let seed = history.seed();
    let mut sim = Simulator::new(
        TestShard,
        SimulatorConfig {
            seed,
            faults: FaultConfig {
                tcp_completion: TcpCompletionFaultMode::DelayBySteps {
                    one_in: 1,
                    steps: 2 + (seed % 3),
                },
                ..Default::default()
            },
            tcp: ScriptedTcpConfig {
                pending_completion_capacity: 8,
                listeners: vec![ScriptedListenerConfig {
                    bind_addr: bind_addr(),
                    local_addr: local_addr(49100 + seed as u16),
                    backlog_capacity: 1,
                    peers: vec![peer_script(
                        5,
                        peer_addr(59100 + seed as u16),
                        vec![b"late-input".to_vec()],
                        Some(4),
                        3,
                    )],
                }],
            },
            storage: Default::default(),
            udp: Default::default(),
            dns: Default::default(),
            process: Default::default(),
        },
    );

    let (listener, _) = bind_listener(&mut sim, bind_addr());
    for op in history.operations() {
        match op {
            CancellationOp::Accept => {
                let log = Rc::new(RefCell::new(Vec::new()));
                let waiter = sim.register(Waiter {
                    listener,
                    log: Rc::clone(&log),
                });
                sim.try_send(waiter, WaiterMsg::StartAccept).unwrap();
                sim.step();
                sim.try_send(waiter, WaiterMsg::StopNow).unwrap();
                sim.step();
                sim.advance_time(Duration::from_millis(20));
                sim.run_until_quiescent();
                assert!(log.borrow().is_empty());
            }
            CancellationOp::Read => {
                let (stream, _) = accept_stream(&mut sim, listener);
                let log = Rc::new(RefCell::new(Vec::new()));
                let reader = sim.register(ReadProbe {
                    stream,
                    max_len: 8,
                    log: Rc::clone(&log),
                });
                sim.try_send(reader, ReadProbeMsg::StartRead).unwrap();
                sim.step();
                sim.try_send(reader, ReadProbeMsg::StopNow).unwrap();
                sim.step();
                sim.run_until_quiescent();
                assert!(log.borrow().is_empty());
            }
            CancellationOp::Write => {
                let (stream, _) = accept_stream(&mut sim, listener);
                let log = Rc::new(RefCell::new(Vec::new()));
                let writer = sim.register(WriteProbe {
                    stream,
                    bytes: b"cancelled-output".to_vec(),
                    log: Rc::clone(&log),
                });
                sim.try_send(writer, WriteProbeMsg::StartWrite).unwrap();
                sim.step();
                sim.try_send(writer, WriteProbeMsg::StopNow).unwrap();
                sim.step();
                sim.run_until_quiescent();
                assert!(log.borrow().is_empty());
            }
        }
    }

    assert!(
        !sim.has_in_flight_calls(),
        "seed {seed} left tombstoned work in flight"
    );
    DstRun::new((), sim.replay_artifact())
}

#[test]
fn seeded_tcp_cancellation_matrix_replays_and_tombstones_late_completions() {
    for seed in 0..18 {
        let history = cancellation_history(seed);
        let first = assert_replays(&history, run_seeded_cancellation_case);
        InvariantSuite::standard().assert(first.artifact().event_record());
        assert!(
            first.artifact().event_record().iter().any(|event| {
                matches!(
                    event.kind(),
                    RuntimeEventKind::CallCompletionRejected {
                        reason: CallCompletionRejectedReason::RequesterClosed,
                        ..
                    }
                )
            }),
            "seed {seed} should visibly reject a cancelled completion"
        );
    }
}

#[test]
fn tcp_completion_rejected_when_requester_mailbox_is_full() {
    let log = Rc::new(RefCell::new(Vec::new()));
    let mut sim = Simulator::new(
        TestShard,
        SimulatorConfig {
            tcp: ScriptedTcpConfig {
                pending_completion_capacity: 8,
                listeners: vec![ScriptedListenerConfig {
                    bind_addr: bind_addr(),
                    local_addr: local_addr(44000),
                    backlog_capacity: 1,
                    peers: vec![peer_script(
                        1,
                        peer_addr(54001),
                        vec![b"mail".to_vec()],
                        None,
                        4,
                    )],
                }],
            },
            ..Default::default()
        },
    );

    let listener_slot = Arc::new(Mutex::new(None));
    let addr_slot = Arc::new(Mutex::new(None));
    let binder = sim.register(Binder {
        bind_addr: bind_addr(),
        listener_slot: Arc::clone(&listener_slot),
        addr_slot: Arc::clone(&addr_slot),
    });
    sim.try_send(binder, BinderMsg::StartBind).unwrap();
    sim.step();
    sim.step();

    let listener = listener_slot
        .lock()
        .expect("listener mutex")
        .expect("listener");
    let waiter = sim.register_with_mailbox_capacity(
        Waiter {
            listener,
            log: Rc::clone(&log),
        },
        1,
    );
    sim.try_send(waiter, WaiterMsg::StartAccept).unwrap();
    sim.step();
    sim.try_send(waiter, WaiterMsg::Fill).unwrap();
    sim.run_until_quiescent();

    assert!(log.borrow().is_empty());
    assert!(sim.trace().iter().any(|event| {
        matches!(
            event.kind(),
            RuntimeEventKind::CallCompletionRejected {
                call_kind: CallKind::TcpAccept,
                reason: CallCompletionRejectedReason::MailboxFull,
                ..
            }
        )
    }));
}

#[test]
fn tcp_replay_preserves_peer_output() {
    let config = SimulatorConfig {
        seed: 22,
        tcp: ScriptedTcpConfig {
            pending_completion_capacity: 32,
            listeners: vec![ScriptedListenerConfig {
                bind_addr: bind_addr(),
                local_addr: local_addr(45000),
                backlog_capacity: 1,
                peers: vec![peer_script(
                    1,
                    peer_addr(55001),
                    vec![b"replay me".to_vec()],
                    Some(3),
                    2,
                )],
            }],
        },
        ..Default::default()
    };
    let (first, _) = run_echo_scenario(
        vec![peer_script(
            1,
            peer_addr(55001),
            vec![b"replay me".to_vec()],
            Some(3),
            2,
        )],
        32,
        config.clone(),
    );
    let (replayed, _) = run_echo_scenario(
        vec![peer_script(
            1,
            peer_addr(55001),
            vec![b"replay me".to_vec()],
            Some(3),
            2,
        )],
        32,
        first.artifact.config().clone(),
    );
    assert_eq!(
        first.artifact.event_record(),
        replayed.artifact.event_record()
    );
    assert_eq!(
        first.artifact.observed_peer_output(),
        replayed.artifact.observed_peer_output()
    );
}

#[test]
fn different_seeds_diverge_under_tcp_delay_faults() {
    let delayed = SimulatorConfig {
        seed: 0,
        faults: FaultConfig {
            tcp_completion: TcpCompletionFaultMode::DelayBySteps {
                one_in: 2,
                steps: 1,
            },
            ..Default::default()
        },
        ..Default::default()
    };
    let preserved = SimulatorConfig {
        seed: 1,
        faults: delayed.faults,
        ..Default::default()
    };

    let (delayed_accept_order, delayed_first_waiter, delayed_second_waiter) =
        accept_completion_order_for_two_waiters(delayed);
    let (preserved_accept_order, preserved_first_waiter, preserved_second_waiter) =
        accept_completion_order_for_two_waiters(preserved);
    assert_eq!(delayed_accept_order.len(), 2);
    assert_eq!(preserved_accept_order.len(), 2);
    assert_eq!(
        delayed_accept_order,
        vec![delayed_second_waiter, delayed_first_waiter],
        "seed 0 should delay the first queued accept and let the second waiter complete first"
    );
    assert_eq!(
        preserved_accept_order,
        vec![preserved_first_waiter, preserved_second_waiter],
        "seed 1 should preserve first-in-first-out accept visibility under DelayBySteps"
    );
    assert_ne!(
        delayed_accept_order, preserved_accept_order,
        "different seeds should flip which waiter completes first under DelayBySteps"
    );
}

#[test]
fn different_seeds_diverge_under_tcp_ready_reordering() {
    let reordered = SimulatorConfig {
        seed: 0,
        faults: FaultConfig {
            tcp_completion: TcpCompletionFaultMode::ReorderReady { one_in: 2 },
            ..Default::default()
        },
        ..Default::default()
    };
    let baseline = SimulatorConfig {
        seed: 1,
        faults: reordered.faults,
        ..Default::default()
    };

    let (reordered_accept_order, reordered_first_waiter, reordered_second_waiter) =
        accept_completion_order_for_two_waiters(reordered);
    let (baseline_accept_order, baseline_first_waiter, baseline_second_waiter) =
        accept_completion_order_for_two_waiters(baseline);
    assert_eq!(reordered_accept_order.len(), 2);
    assert_eq!(baseline_accept_order.len(), 2);
    assert_eq!(
        reordered_accept_order,
        vec![reordered_second_waiter, reordered_first_waiter],
        "seed 0 should reorder tied ready accept completions so the second waiter wins first"
    );
    assert_eq!(
        baseline_accept_order,
        vec![baseline_first_waiter, baseline_second_waiter],
        "seed 1 should preserve request-order accept visibility when ReorderReady does not fire"
    );
    assert_ne!(
        reordered_accept_order, baseline_accept_order,
        "different seeds should flip which waiter completes first under ReorderReady"
    );
}

#[test]
fn tcp_checker_failure_replays_under_ready_reordering() {
    let config = SimulatorConfig {
        seed: 0,
        faults: FaultConfig {
            tcp_completion: TcpCompletionFaultMode::ReorderReady { one_in: 1 },
            ..Default::default()
        },
        tcp: ScriptedTcpConfig {
            pending_completion_capacity: 8,
            listeners: vec![
                ScriptedListenerConfig {
                    bind_addr: bind_addr(),
                    local_addr: local_addr(47000),
                    backlog_capacity: 1,
                    peers: vec![peer_script(
                        5,
                        peer_addr(59001),
                        vec![b"a".to_vec()],
                        None,
                        1,
                    )],
                },
                ScriptedListenerConfig {
                    bind_addr: local_addr(47001),
                    local_addr: local_addr(47001),
                    backlog_capacity: 1,
                    peers: vec![peer_script(
                        5,
                        peer_addr(59002),
                        vec![b"b".to_vec()],
                        None,
                        1,
                    )],
                },
            ],
        },
        storage: Default::default(),
        udp: Default::default(),
        dns: Default::default(),
        process: Default::default(),
    };

    let mut sim = Simulator::new(TestShard, config);
    let first_listener = Arc::new(Mutex::new(None));
    let first_addr = Arc::new(Mutex::new(None));
    let first_binder = sim.register(Binder {
        bind_addr: bind_addr(),
        listener_slot: Arc::clone(&first_listener),
        addr_slot: Arc::clone(&first_addr),
    });
    let second_listener = Arc::new(Mutex::new(None));
    let second_addr = Arc::new(Mutex::new(None));
    let second_binder = sim.register(Binder {
        bind_addr: local_addr(47001),
        listener_slot: Arc::clone(&second_listener),
        addr_slot: Arc::clone(&second_addr),
    });
    sim.try_send(first_binder, BinderMsg::StartBind).unwrap();
    sim.try_send(second_binder, BinderMsg::StartBind).unwrap();
    sim.step();
    sim.step();

    let first_waiter = sim.register(Waiter {
        listener: first_listener
            .lock()
            .expect("listener mutex")
            .expect("listener"),
        log: Rc::new(RefCell::new(Vec::new())),
    });
    let second_waiter = sim.register(Waiter {
        listener: second_listener
            .lock()
            .expect("listener mutex")
            .expect("listener"),
        log: Rc::new(RefCell::new(Vec::new())),
    });
    sim.try_send(first_waiter, WaiterMsg::StartAccept).unwrap();
    sim.try_send(second_waiter, WaiterMsg::StartAccept).unwrap();

    let mut checker = FirstAcceptOrderChecker {
        second_waiter: Some(second_waiter.isolate()),
        saw_accept: false,
    };
    let failure = sim
        .run_until_quiescent_checked(&mut checker)
        .expect("reordered run should trip checker");
    let artifact = sim.replay_artifact();
    assert_eq!(artifact.checker_failure(), Some(&failure));

    let mut replay = Simulator::new(TestShard, artifact.config().clone());
    let first_listener = Arc::new(Mutex::new(None));
    let first_addr = Arc::new(Mutex::new(None));
    let first_binder = replay.register(Binder {
        bind_addr: bind_addr(),
        listener_slot: Arc::clone(&first_listener),
        addr_slot: Arc::clone(&first_addr),
    });
    let second_listener = Arc::new(Mutex::new(None));
    let second_addr = Arc::new(Mutex::new(None));
    let second_binder = replay.register(Binder {
        bind_addr: local_addr(47001),
        listener_slot: Arc::clone(&second_listener),
        addr_slot: Arc::clone(&second_addr),
    });
    replay.try_send(first_binder, BinderMsg::StartBind).unwrap();
    replay
        .try_send(second_binder, BinderMsg::StartBind)
        .unwrap();
    replay.step();
    replay.step();

    let first_waiter = replay.register(Waiter {
        listener: first_listener
            .lock()
            .expect("listener mutex")
            .expect("listener"),
        log: Rc::new(RefCell::new(Vec::new())),
    });
    let second_waiter = replay.register(Waiter {
        listener: second_listener
            .lock()
            .expect("listener mutex")
            .expect("listener"),
        log: Rc::new(RefCell::new(Vec::new())),
    });
    replay
        .try_send(first_waiter, WaiterMsg::StartAccept)
        .unwrap();
    replay
        .try_send(second_waiter, WaiterMsg::StartAccept)
        .unwrap();

    let mut checker = FirstAcceptOrderChecker {
        second_waiter: Some(second_waiter.isolate()),
        saw_accept: false,
    };
    let replayed_failure = replay
        .run_until_quiescent_checked(&mut checker)
        .expect("replay should trip same checker");
    let replayed = replay.replay_artifact();
    assert_eq!(artifact.event_record(), replayed.event_record());
    assert_eq!(
        artifact.observed_peer_output(),
        replayed.observed_peer_output()
    );
    assert_eq!(artifact.checker_failure(), replayed.checker_failure());
    assert_eq!(failure, replayed_failure);
}
