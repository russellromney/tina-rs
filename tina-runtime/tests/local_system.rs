use std::collections::VecDeque;
use std::convert::Infallible;
use std::fs;
use std::io::{Read, Write};
use std::net::{Shutdown, SocketAddr, TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::{Arc, Condvar, Mutex, mpsc};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use tina::{Mailbox, TrySendError, prelude::*};
use tina_runtime::{
    AffinityStatus, CallError, CallKind, CallOutcome, CancellationSupport, FileId, ListenerId,
    LiveShardReport, LiveShardState, LocalSystem, LocalSystemConfig, LocalSystemConfigError,
    LocalSystemState, MailboxFactory, PathKind, PathMetadata, PersistenceSupportLevel,
    PreallocationConfig, ResourceExecutionShape, ResourceSupport, RuntimeEventKind,
    ShutdownSupport, ShutdownUncleanReason, SnapshotImage, StreamId, ThreadedRuntimeError,
    ThreadedTrySendError, TlsListenerId, TlsStreamId, TraceRetention, UdpSocketId, call,
    dns_lookup, file_close, file_create, file_fsync, file_read, file_size, file_write,
    journal_append, journal_replay, path_metadata, process_run, read_dir, remove_file,
    rename_replace, signal_wait, sleep, snapshot_load, sync_parent, tcp_accept, tcp_bind,
    tcp_close_listener, tcp_close_stream, tcp_read, tcp_write, tls_accept, tls_bind, tls_close,
    tls_close_listener, tls_connect, tls_read, tls_write, udp_bind, udp_close_socket,
    udp_recv_from, udp_send_to,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct AppShard(u32);

impl Shard for AppShard {
    fn id(&self) -> ShardId {
        ShardId::new(self.0)
    }
}

struct AppMailbox<T> {
    capacity: usize,
    queue: Mutex<VecDeque<T>>,
    closed: Mutex<bool>,
}

impl<T> AppMailbox<T> {
    fn new(capacity: usize) -> Self {
        Self {
            capacity,
            queue: Mutex::new(VecDeque::new()),
            closed: Mutex::new(false),
        }
    }
}

impl<T> Mailbox<T> for AppMailbox<T> {
    fn capacity(&self) -> usize {
        self.capacity
    }

    fn try_send(&self, message: T) -> Result<(), TrySendError<T>> {
        if *self.closed.lock().expect("closed lock") {
            return Err(TrySendError::Closed(message));
        }

        let mut queue = self.queue.lock().expect("queue lock");
        if queue.len() >= self.capacity {
            return Err(TrySendError::Full(message));
        }

        queue.push_back(message);
        Ok(())
    }

    fn recv(&self) -> Option<T> {
        self.queue.lock().expect("queue lock").pop_front()
    }
    fn is_empty(&self) -> bool {
        self.queue.lock().expect("queue lock").is_empty()
    }

    fn close(&self) {
        *self.closed.lock().expect("closed lock") = true;
    }
}

#[derive(Debug, Clone, Copy)]
struct AppMailboxFactory;

impl MailboxFactory for AppMailboxFactory {
    fn create<T: 'static>(&self, capacity: usize) -> Box<dyn Mailbox<T>> {
        Box::new(AppMailbox::new(capacity))
    }
}

#[derive(Debug, Clone, Copy)]
struct PanickingMailboxFactory;

impl MailboxFactory for PanickingMailboxFactory {
    fn create<T: 'static>(&self, _capacity: usize) -> Box<dyn Mailbox<T>> {
        panic!("test mailbox factory panic")
    }
}

#[derive(Debug, Clone, Copy)]
struct CapacityPanicMailboxFactory {
    panic_capacity: usize,
}

impl MailboxFactory for CapacityPanicMailboxFactory {
    fn create<T: 'static>(&self, capacity: usize) -> Box<dyn Mailbox<T>> {
        if capacity == self.panic_capacity {
            panic!("test mailbox factory panic for capacity {capacity}");
        }
        Box::new(AppMailbox::new(capacity))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LlamaMsg {
    Feed(u64),
}

#[derive(Debug)]
struct LlamaService {
    seen: Arc<Mutex<Vec<u64>>>,
}

#[tina_runtime::isolate(message = LlamaMsg, shard = AppShard)]
impl LlamaService {
    fn handle(
        &mut self,
        msg: LlamaMsg,
        _ctx: &mut Context<'_, AppShard, Self::Reply>,
    ) -> Effect<Self> {
        match msg {
            LlamaMsg::Feed(value) => {
                self.seen.lock().expect("seen lock").push(value);
                noop()
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum CrossShardCallWorkerMsg {
    Echo(Vec<u8>),
    NoReply,
    NoReplyId(u8),
    ReleaseHeld,
    Stop,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct CrossShardCallReply(Vec<u8>);

#[derive(Debug, Default)]
struct CrossShardCallWorker {
    held: Vec<(tina::RequestContext<CrossShardCallReply>, Vec<u8>)>,
    held_count: Option<Arc<Mutex<usize>>>,
}

#[tina_runtime::isolate(
    message = CrossShardCallWorkerMsg,
    reply = CrossShardCallReply,
    shard = AppShard
)]
impl CrossShardCallWorker {
    fn handle(
        &mut self,
        msg: CrossShardCallWorkerMsg,
        _ctx: &mut Context<'_, AppShard, Self::Reply>,
    ) -> Effect<Self> {
        match msg {
            CrossShardCallWorkerMsg::Echo(bytes) => reply(CrossShardCallReply(bytes)),
            CrossShardCallWorkerMsg::NoReply => noop(),
            CrossShardCallWorkerMsg::NoReplyId(_) => noop(),
            CrossShardCallWorkerMsg::ReleaseHeld => {
                let effects = self
                    .held
                    .drain(..)
                    .map(|(reply_to, bytes)| tina::reply_to(reply_to, CrossShardCallReply(bytes)))
                    .collect::<Vec<_>>();
                batch(effects)
            }
            CrossShardCallWorkerMsg::Stop => stop(),
        }
    }

    fn handle_call(
        &mut self,
        msg: CrossShardCallWorkerMsg,
        call: tina::CallContext<'_, Self>,
    ) -> Effect<Self> {
        match msg {
            CrossShardCallWorkerMsg::Echo(bytes) => call.reply(CrossShardCallReply(bytes)),
            CrossShardCallWorkerMsg::NoReply => {
                self.held
                    .push((call.into_request_context(), b"held".to_vec()));
                noop()
            }
            CrossShardCallWorkerMsg::NoReplyId(id) => {
                self.held.push((call.into_request_context(), vec![id]));
                if let Some(count) = &self.held_count {
                    *count.lock().expect("held count") += 1;
                }
                noop()
            }
            CrossShardCallWorkerMsg::ReleaseHeld => {
                call.reject(tina::CallRejectedReason::UnsupportedMessage)
            }
            CrossShardCallWorkerMsg::Stop => {
                call.reject(tina::CallRejectedReason::UnsupportedMessage)
            }
        }
    }
}

#[derive(Debug, Clone)]
enum CrossShardCallClientMsg {
    Start(Address<CrossShardCallWorkerMsg, CrossShardCallReply>),
    StartTimeout(Address<CrossShardCallWorkerMsg, CrossShardCallReply>),
    StartHeldOne(Address<CrossShardCallWorkerMsg, CrossShardCallReply>, u8),
    Returned(CallOutcome<CrossShardCallReply>),
}

#[derive(Debug)]
struct CrossShardCallClient {
    outcomes: Arc<Mutex<Vec<CallOutcome<CrossShardCallReply>>>>,
}

#[tina_runtime::isolate(message = CrossShardCallClientMsg, shard = AppShard)]
impl CrossShardCallClient {
    fn handle(
        &mut self,
        msg: CrossShardCallClientMsg,
        _ctx: &mut Context<'_, AppShard, Self::Reply>,
    ) -> Effect<Self> {
        match msg {
            CrossShardCallClientMsg::Start(worker) => call(
                worker,
                CrossShardCallWorkerMsg::Echo(b"llama".to_vec()),
                Duration::from_secs(2),
            )
            .then(CrossShardCallClientMsg::Returned),
            CrossShardCallClientMsg::StartTimeout(worker) => call(
                worker,
                CrossShardCallWorkerMsg::NoReply,
                Duration::from_millis(20),
            )
            .then(CrossShardCallClientMsg::Returned),
            CrossShardCallClientMsg::StartHeldOne(worker, id) => call(
                worker,
                CrossShardCallWorkerMsg::NoReplyId(id),
                Duration::from_millis(500),
            )
            .then(CrossShardCallClientMsg::Returned),
            CrossShardCallClientMsg::Returned(outcome) => {
                self.outcomes.lock().expect("outcomes lock").push(outcome);
                noop()
            }
        }
    }
}

#[derive(Debug, Clone)]
enum BlockingMsg {
    Hold(Arc<(Mutex<bool>, Condvar)>, Arc<(Mutex<bool>, Condvar)>),
    Note(u64),
}

#[derive(Debug)]
struct BlockingService {
    seen: Arc<Mutex<Vec<u64>>>,
}

#[tina_runtime::isolate(message = BlockingMsg, shard = AppShard)]
impl BlockingService {
    fn handle(
        &mut self,
        msg: BlockingMsg,
        _ctx: &mut Context<'_, AppShard, Self::Reply>,
    ) -> Effect<Self> {
        match msg {
            BlockingMsg::Hold(entered, release) => {
                let (entered_lock, entered_cv) = &*entered;
                *entered_lock.lock().expect("entered lock") = true;
                entered_cv.notify_all();

                let (release_lock, release_cv) = &*release;
                let mut released = release_lock.lock().expect("release lock");
                while !*released {
                    released = release_cv.wait(released).expect("release wait");
                }
                noop()
            }
            BlockingMsg::Note(value) => {
                self.seen.lock().expect("blocking seen lock").push(value);
                noop()
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BurstMsg {
    SendTwo,
}

#[derive(Debug)]
struct BurstService {
    target: Address<BlockingMsg>,
}

#[tina_runtime::isolate(
    message = BurstMsg,
    send = Outbound<BlockingMsg>,
    shard = AppShard
)]
impl BurstService {
    fn handle(
        &mut self,
        msg: BurstMsg,
        _ctx: &mut Context<'_, AppShard, Self::Reply>,
    ) -> Effect<Self> {
        match msg {
            BurstMsg::SendTwo => batch(vec![
                send(self.target, BlockingMsg::Note(1)),
                send(self.target, BlockingMsg::Note(2)),
            ]),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TimerMsg {
    Start,
    Finished(Result<(), CallError>),
}

#[derive(Debug)]
struct TimerService {
    seen: Arc<Mutex<Vec<&'static str>>>,
}

#[tina_runtime::isolate(message = TimerMsg, shard = AppShard)]
impl TimerService {
    fn handle(
        &mut self,
        msg: TimerMsg,
        _ctx: &mut Context<'_, AppShard, Self::Reply>,
    ) -> Effect<Self> {
        match msg {
            TimerMsg::Start => sleep(Duration::from_millis(1)).then(TimerMsg::Finished),
            TimerMsg::Finished(Ok(())) => {
                self.seen.lock().expect("timer seen lock").push("finished");
                noop()
            }
            TimerMsg::Finished(Err(_)) => {
                self.seen.lock().expect("timer seen lock").push("failed");
                noop()
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum UdpLoopbackMsg {
    Start,
    ReceiverBound(Result<(UdpSocketId, SocketAddr), CallError>),
    SenderBound(Result<(UdpSocketId, SocketAddr), CallError>),
    Received(Result<(SocketAddr, Vec<u8>, bool), CallError>),
    Sent(Result<usize, CallError>),
    Closed(Result<(), CallError>),
}

#[derive(Debug)]
struct UdpLoopbackService {
    receiver: Option<UdpSocketId>,
    sender: Option<UdpSocketId>,
    receiver_addr: Option<SocketAddr>,
    observed: Arc<Mutex<Vec<String>>>,
}

#[tina_runtime::isolate(message = UdpLoopbackMsg, shard = AppShard)]
impl UdpLoopbackService {
    fn handle(
        &mut self,
        msg: UdpLoopbackMsg,
        _ctx: &mut Context<'_, AppShard, Self::Reply>,
    ) -> Effect<Self> {
        match msg {
            UdpLoopbackMsg::Start => udp_bind("127.0.0.1:0".parse().expect("loopback addr"))
                .then(UdpLoopbackMsg::ReceiverBound),
            UdpLoopbackMsg::ReceiverBound(Ok((socket, addr))) => {
                self.receiver = Some(socket);
                self.receiver_addr = Some(addr);
                udp_bind("127.0.0.1:0".parse().expect("loopback addr"))
                    .then(UdpLoopbackMsg::SenderBound)
            }
            UdpLoopbackMsg::SenderBound(Ok((socket, _addr))) => {
                self.sender = Some(socket);
                let receiver = self.receiver.expect("receiver socket");
                let receiver_addr = self.receiver_addr.expect("receiver addr");
                batch(vec![
                    udp_recv_from(receiver, 4).then(UdpLoopbackMsg::Received),
                    udp_send_to(socket, receiver_addr, b"llamas".to_vec())
                        .then(UdpLoopbackMsg::Sent),
                ])
            }
            UdpLoopbackMsg::Sent(Ok(count)) => {
                self.observed
                    .lock()
                    .expect("udp observed lock")
                    .push(format!("sent:{count}"));
                udp_close_socket(self.sender.expect("sender socket")).then(UdpLoopbackMsg::Closed)
            }
            UdpLoopbackMsg::Received(Ok((_peer, bytes, truncated))) => {
                self.observed
                    .lock()
                    .expect("udp observed lock")
                    .push(format!(
                        "recv:{}:{truncated}",
                        String::from_utf8(bytes).expect("utf8 udp payload")
                    ));
                udp_close_socket(self.receiver.expect("receiver socket"))
                    .then(UdpLoopbackMsg::Closed)
            }
            UdpLoopbackMsg::Closed(Ok(())) => {
                self.observed
                    .lock()
                    .expect("udp observed lock")
                    .push("closed".to_string());
                noop()
            }
            UdpLoopbackMsg::ReceiverBound(Err(_))
            | UdpLoopbackMsg::SenderBound(Err(_))
            | UdpLoopbackMsg::Received(Err(_))
            | UdpLoopbackMsg::Sent(Err(_))
            | UdpLoopbackMsg::Closed(Err(_)) => {
                self.observed
                    .lock()
                    .expect("udp observed lock")
                    .push("failed".to_string());
                noop()
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum DnsLookupMsg {
    Start,
    Done(Result<Vec<SocketAddr>, CallError>),
}

#[derive(Debug)]
struct DnsLookupService {
    observed: Arc<Mutex<Vec<String>>>,
}

#[tina_runtime::isolate(message = DnsLookupMsg, shard = AppShard)]
impl DnsLookupService {
    fn handle(
        &mut self,
        msg: DnsLookupMsg,
        _ctx: &mut Context<'_, AppShard, Self::Reply>,
    ) -> Effect<Self> {
        match msg {
            DnsLookupMsg::Start => {
                dns_lookup("localhost", 80, Duration::from_secs(1)).then(DnsLookupMsg::Done)
            }
            DnsLookupMsg::Done(Ok(addrs)) => {
                self.observed
                    .lock()
                    .expect("dns observed lock")
                    .push(format!("resolved:{}", addrs.len()));
                noop()
            }
            DnsLookupMsg::Done(Err(error)) => {
                self.observed
                    .lock()
                    .expect("dns observed lock")
                    .push(format!("err:{error:?}"));
                noop()
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum TlsClientMsg {
    Start { addr: SocketAddr, cert_der: Vec<u8> },
    Connected(Result<TlsStreamId, CallError>),
    Wrote(Result<usize, CallError>),
    Read(Result<Vec<u8>, CallError>),
    Closed(Result<(), CallError>),
}

#[derive(Debug)]
struct TlsClientService {
    observed: Arc<Mutex<Vec<String>>>,
    stream: Option<TlsStreamId>,
}

#[tina_runtime::isolate(message = TlsClientMsg, shard = AppShard)]
impl TlsClientService {
    fn handle(
        &mut self,
        msg: TlsClientMsg,
        _ctx: &mut Context<'_, AppShard, Self::Reply>,
    ) -> Effect<Self> {
        match msg {
            TlsClientMsg::Start { addr, cert_der } => {
                tls_connect(addr, "localhost", vec![cert_der], Duration::from_secs(2))
                    .then(TlsClientMsg::Connected)
            }
            TlsClientMsg::Connected(Ok(stream)) => {
                self.stream = Some(stream);
                self.observed
                    .lock()
                    .expect("tls observed lock")
                    .push("connected".to_string());
                tls_write(stream, b"ping".to_vec(), Duration::from_secs(2))
                    .then(TlsClientMsg::Wrote)
            }
            TlsClientMsg::Connected(Err(error)) => {
                self.observed
                    .lock()
                    .expect("tls observed lock")
                    .push(format!("connect-err:{error:?}"));
                noop()
            }
            TlsClientMsg::Wrote(Ok(count)) => {
                self.observed
                    .lock()
                    .expect("tls observed lock")
                    .push(format!("wrote:{count}"));
                let stream = self.stream.expect("TLS stream should be remembered");
                tls_read(stream, 4, Duration::from_secs(2)).then(TlsClientMsg::Read)
            }
            TlsClientMsg::Wrote(Err(error)) => {
                self.observed
                    .lock()
                    .expect("tls observed lock")
                    .push(format!("write-err:{error:?}"));
                noop()
            }
            TlsClientMsg::Read(Ok(bytes)) => {
                self.observed
                    .lock()
                    .expect("tls observed lock")
                    .push(String::from_utf8(bytes).unwrap_or_else(|error| format!("utf8:{error}")));
                let stream = self.stream.expect("TLS stream should be remembered");
                tls_close(stream, Duration::from_secs(2)).then(TlsClientMsg::Closed)
            }
            TlsClientMsg::Read(Err(error)) => {
                self.observed
                    .lock()
                    .expect("tls observed lock")
                    .push(format!("read-err:{error:?}"));
                noop()
            }
            TlsClientMsg::Closed(Ok(())) => {
                self.observed
                    .lock()
                    .expect("tls observed lock")
                    .push("closed".to_string());
                noop()
            }
            TlsClientMsg::Closed(Err(error)) => {
                self.observed
                    .lock()
                    .expect("tls observed lock")
                    .push(format!("close-err:{error:?}"));
                noop()
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum TlsServerMsg {
    Start,
    Bound(Result<(TlsListenerId, SocketAddr), CallError>),
    Accepted(Result<(TlsStreamId, SocketAddr), CallError>, TlsListenerId),
    Read(Result<Vec<u8>, CallError>, TlsListenerId, TlsStreamId),
    Wrote(Result<usize, CallError>, TlsListenerId, TlsStreamId),
    StreamClosed(Result<(), CallError>, TlsListenerId),
    ListenerClosed(Result<(), CallError>),
}

#[derive(Debug)]
struct TlsServerService {
    observed: Arc<Mutex<Vec<String>>>,
    cert_der: Vec<u8>,
    key_der: Vec<u8>,
}

#[tina_runtime::isolate(message = TlsServerMsg, shard = AppShard)]
impl TlsServerService {
    fn handle(
        &mut self,
        msg: TlsServerMsg,
        _ctx: &mut Context<'_, AppShard, Self::Reply>,
    ) -> Effect<Self> {
        match msg {
            TlsServerMsg::Start => tls_bind(
                "127.0.0.1:0".parse().expect("loopback"),
                vec![self.cert_der.clone()],
                self.key_der.clone(),
            )
            .then(TlsServerMsg::Bound),
            TlsServerMsg::Bound(Ok((listener, local_addr))) => {
                self.observed
                    .lock()
                    .expect("tls server observed lock")
                    .push(format!("bound:{local_addr}"));
                tls_accept(listener, Duration::from_secs(2))
                    .then(move |result| TlsServerMsg::Accepted(result, listener))
            }
            TlsServerMsg::Accepted(Ok((stream, peer)), listener) => {
                self.observed
                    .lock()
                    .expect("tls server observed lock")
                    .push(format!("accepted:{peer}"));
                tls_read(stream, 4, Duration::from_secs(2))
                    .then(move |result| TlsServerMsg::Read(result, listener, stream))
            }
            TlsServerMsg::Read(Ok(bytes), listener, stream) if bytes == b"ping" => {
                tls_write(stream, b"pong".to_vec(), Duration::from_secs(2))
                    .then(move |result| TlsServerMsg::Wrote(result, listener, stream))
            }
            TlsServerMsg::Wrote(Ok(4), listener, stream) => {
                tls_close(stream, Duration::from_secs(2))
                    .then(move |result| TlsServerMsg::StreamClosed(result, listener))
            }
            TlsServerMsg::StreamClosed(Ok(()), listener) => {
                tls_close_listener(listener).then(TlsServerMsg::ListenerClosed)
            }
            TlsServerMsg::ListenerClosed(Ok(())) => {
                self.observed
                    .lock()
                    .expect("tls server observed lock")
                    .push("closed".to_string());
                noop()
            }
            TlsServerMsg::Bound(Err(error)) => {
                self.observed
                    .lock()
                    .expect("tls server observed lock")
                    .push(format!("failed:{error:?}"));
                noop()
            }
            TlsServerMsg::Accepted(Err(error), listener) => {
                self.observed
                    .lock()
                    .expect("tls server observed lock")
                    .push(format!("failed:{error:?}"));
                tls_close_listener(listener).then(TlsServerMsg::ListenerClosed)
            }
            TlsServerMsg::Read(Err(error), _, _)
            | TlsServerMsg::Wrote(Err(error), _, _)
            | TlsServerMsg::StreamClosed(Err(error), _)
            | TlsServerMsg::ListenerClosed(Err(error)) => {
                self.observed
                    .lock()
                    .expect("tls server observed lock")
                    .push(format!("failed:{error:?}"));
                noop()
            }
            TlsServerMsg::Read(Ok(_), _, _) | TlsServerMsg::Wrote(Ok(_), _, _) => {
                self.observed
                    .lock()
                    .expect("tls server observed lock")
                    .push("failed:unexpected-shape".to_string());
                noop()
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum MultiTlsServerMsg {
    Start,
    Bound(Result<(TlsListenerId, SocketAddr), CallError>),
    Accepted(Result<(TlsStreamId, SocketAddr), CallError>, TlsListenerId),
    Read(Result<Vec<u8>, CallError>, TlsStreamId),
    Wrote(Result<usize, CallError>, TlsStreamId),
    Closed(Result<(), CallError>),
}

#[derive(Debug)]
struct MultiTlsServerService {
    observed: Arc<Mutex<Vec<String>>>,
    cert_der: Vec<u8>,
    key_der: Vec<u8>,
    accepts: usize,
}

#[tina_runtime::isolate(message = MultiTlsServerMsg, shard = AppShard)]
impl MultiTlsServerService {
    fn handle(
        &mut self,
        msg: MultiTlsServerMsg,
        _ctx: &mut Context<'_, AppShard, Self::Reply>,
    ) -> Effect<Self> {
        match msg {
            MultiTlsServerMsg::Start => tls_bind(
                "127.0.0.1:0".parse().expect("loopback"),
                vec![self.cert_der.clone()],
                self.key_der.clone(),
            )
            .then(MultiTlsServerMsg::Bound),
            MultiTlsServerMsg::Bound(Ok((listener, local_addr))) => {
                self.observed
                    .lock()
                    .expect("multi tls observed lock")
                    .push(format!("bound:{local_addr}"));
                tls_accept(listener, Duration::from_secs(5))
                    .then(move |result| MultiTlsServerMsg::Accepted(result, listener))
            }
            MultiTlsServerMsg::Accepted(Ok((stream, peer)), listener) => {
                self.accepts += 1;
                self.observed
                    .lock()
                    .expect("multi tls observed lock")
                    .push(format!("accepted:{}:{peer}", self.accepts));
                let read = tls_read(stream, 4, Duration::from_secs(5))
                    .then(move |result| MultiTlsServerMsg::Read(result, stream));
                if self.accepts == 1 {
                    // The first stream intentionally goes quiet after TLS
                    // handshake. Put its read before the next accept so the
                    // old serial TLS worker head-of-line-blocks the second
                    // connection.
                    batch(vec![
                        read,
                        tls_accept(listener, Duration::from_secs(5))
                            .then(move |result| MultiTlsServerMsg::Accepted(result, listener)),
                    ])
                } else {
                    read
                }
            }
            MultiTlsServerMsg::Read(Ok(bytes), stream) if bytes == b"ping" => {
                tls_write(stream, b"pong".to_vec(), Duration::from_secs(5))
                    .then(move |result| MultiTlsServerMsg::Wrote(result, stream))
            }
            MultiTlsServerMsg::Wrote(Ok(4), stream) => {
                self.observed
                    .lock()
                    .expect("multi tls observed lock")
                    .push("wrote:4".to_string());
                tls_close(stream, Duration::from_secs(5)).then(MultiTlsServerMsg::Closed)
            }
            MultiTlsServerMsg::Closed(Ok(())) => noop(),
            MultiTlsServerMsg::Bound(Err(error))
            | MultiTlsServerMsg::Accepted(Err(error), _)
            | MultiTlsServerMsg::Read(Err(error), _)
            | MultiTlsServerMsg::Wrote(Err(error), _)
            | MultiTlsServerMsg::Closed(Err(error)) => {
                self.observed
                    .lock()
                    .expect("multi tls observed lock")
                    .push(format!("failed:{error:?}"));
                noop()
            }
            MultiTlsServerMsg::Read(Ok(_), _) | MultiTlsServerMsg::Wrote(Ok(_), _) => {
                self.observed
                    .lock()
                    .expect("multi tls observed lock")
                    .push("failed:unexpected-shape".to_string());
                noop()
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum SignalUnsupportedMsg {
    Start,
    Done(Result<String, CallError>),
}

#[derive(Debug)]
struct SignalUnsupportedService {
    observed: Arc<Mutex<Vec<String>>>,
}

#[tina_runtime::isolate(message = SignalUnsupportedMsg, shard = AppShard)]
impl SignalUnsupportedService {
    fn handle(
        &mut self,
        msg: SignalUnsupportedMsg,
        _ctx: &mut Context<'_, AppShard, Self::Reply>,
    ) -> Effect<Self> {
        match msg {
            SignalUnsupportedMsg::Start => {
                signal_wait("shutdown", Duration::from_secs(60)).then(SignalUnsupportedMsg::Done)
            }
            SignalUnsupportedMsg::Done(Ok(name)) => {
                self.observed
                    .lock()
                    .expect("signal observed lock")
                    .push(format!("signal:{name}"));
                noop()
            }
            SignalUnsupportedMsg::Done(Err(error)) => {
                self.observed
                    .lock()
                    .expect("signal observed lock")
                    .push(format!("signal-err:{error:?}"));
                noop()
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum ProcessMsg {
    Echo,
    Sleep,
    GrandchildStdout,
    Done(Result<tina_runtime::ProcessRunResult, CallError>),
}

#[derive(Debug)]
struct ProcessService {
    observed: Arc<Mutex<Vec<String>>>,
}

#[tina_runtime::isolate(message = ProcessMsg, shard = AppShard)]
impl ProcessService {
    fn handle(
        &mut self,
        msg: ProcessMsg,
        _ctx: &mut Context<'_, AppShard, Self::Reply>,
    ) -> Effect<Self> {
        match msg {
            ProcessMsg::Echo => process_run(
                "/bin/echo",
                vec!["llamas".to_string()],
                Duration::from_secs(2),
                4,
                16,
            )
            .then(ProcessMsg::Done),
            ProcessMsg::Sleep => process_run(
                "/bin/sleep",
                vec!["5".to_string()],
                Duration::from_millis(10),
                16,
                16,
            )
            .then(ProcessMsg::Done),
            ProcessMsg::GrandchildStdout => process_run(
                "/bin/sh",
                vec!["-c".to_string(), "sleep 5 & exit 0".to_string()],
                Duration::from_secs(2),
                16,
                16,
            )
            .then(ProcessMsg::Done),
            ProcessMsg::Done(Ok(result)) => {
                self.observed
                    .lock()
                    .expect("process observed lock")
                    .push(format!(
                        "exit:{:?}:{}:{}",
                        result.status.code,
                        String::from_utf8_lossy(&result.stdout),
                        result.stdout_truncated
                    ));
                noop()
            }
            ProcessMsg::Done(Err(error)) => {
                self.observed
                    .lock()
                    .expect("process observed lock")
                    .push(format!("err:{error:?}"));
                noop()
            }
        }
    }
}

#[cfg(unix)]
#[derive(Debug, Clone, PartialEq, Eq)]
enum ComposedIoMsg {
    Start(PathBuf),
    ReceiverBound(Result<(UdpSocketId, SocketAddr), CallError>),
    SenderBound(Result<(UdpSocketId, SocketAddr), CallError>),
    Received(Result<(SocketAddr, Vec<u8>, bool), CallError>),
    Sent(Result<usize, CallError>),
    Processed(Result<tina_runtime::ProcessRunResult, CallError>),
    Journaled(Result<(), CallError>),
    Closed(Result<(), CallError>),
}

#[cfg(unix)]
#[derive(Debug)]
struct ComposedIoService {
    journal: Option<PathBuf>,
    receiver: Option<UdpSocketId>,
    sender: Option<UdpSocketId>,
    receiver_addr: Option<SocketAddr>,
    udp_payload: Option<Vec<u8>>,
    closed: usize,
    observed: Arc<Mutex<Vec<String>>>,
}

#[cfg(unix)]
#[tina_runtime::isolate(message = ComposedIoMsg, shard = AppShard)]
impl ComposedIoService {
    fn handle(
        &mut self,
        msg: ComposedIoMsg,
        _ctx: &mut Context<'_, AppShard, Self::Reply>,
    ) -> Effect<Self> {
        match msg {
            ComposedIoMsg::Start(path) => {
                self.journal = Some(path);
                udp_bind("127.0.0.1:0".parse().expect("loopback"))
                    .then(ComposedIoMsg::ReceiverBound)
            }
            ComposedIoMsg::ReceiverBound(Ok((socket, addr))) => {
                self.receiver = Some(socket);
                self.receiver_addr = Some(addr);
                udp_bind("127.0.0.1:0".parse().expect("loopback")).then(ComposedIoMsg::SenderBound)
            }
            ComposedIoMsg::SenderBound(Ok((socket, _))) => {
                self.sender = Some(socket);
                batch(vec![
                    udp_recv_from(self.receiver.expect("receiver"), 16)
                        .then(ComposedIoMsg::Received),
                    udp_send_to(
                        socket,
                        self.receiver_addr.expect("receiver addr"),
                        b"llama".to_vec(),
                    )
                    .then(ComposedIoMsg::Sent),
                ])
            }
            ComposedIoMsg::Sent(Ok(count)) => {
                self.observed
                    .lock()
                    .expect("composed observed lock")
                    .push(format!("udp-sent:{count}"));
                noop()
            }
            ComposedIoMsg::Received(Ok((_peer, bytes, truncated))) => {
                self.observed
                    .lock()
                    .expect("composed observed lock")
                    .push(format!("udp-recv:{truncated}"));
                self.udp_payload = Some(bytes);
                process_run(
                    "/bin/echo",
                    vec!["fed".to_string()],
                    Duration::from_secs(2),
                    16,
                    16,
                )
                .then(ComposedIoMsg::Processed)
            }
            ComposedIoMsg::Processed(Ok(result)) => {
                let mut bytes = self.udp_payload.take().expect("udp before process");
                bytes.extend_from_slice(&result.stdout);
                journal_append(self.journal.clone().expect("journal path"), 1, bytes)
                    .then(ComposedIoMsg::Journaled)
            }
            ComposedIoMsg::Journaled(Ok(())) => batch(vec![
                udp_close_socket(self.receiver.expect("receiver")).then(ComposedIoMsg::Closed),
                udp_close_socket(self.sender.expect("sender")).then(ComposedIoMsg::Closed),
            ]),
            ComposedIoMsg::Closed(Ok(())) => {
                self.closed += 1;
                if self.closed == 2 {
                    self.observed
                        .lock()
                        .expect("composed observed lock")
                        .push("committed".to_string());
                }
                noop()
            }
            ComposedIoMsg::ReceiverBound(Err(error))
            | ComposedIoMsg::SenderBound(Err(error))
            | ComposedIoMsg::Received(Err(error))
            | ComposedIoMsg::Sent(Err(error))
            | ComposedIoMsg::Processed(Err(error))
            | ComposedIoMsg::Journaled(Err(error))
            | ComposedIoMsg::Closed(Err(error)) => {
                self.observed
                    .lock()
                    .expect("composed observed lock")
                    .push(format!("failed:{error:?}"));
                noop()
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum FileMsg {
    Start(PathBuf),
    Opened(Result<FileId, CallError>, PathBuf),
    Wrote(Result<usize, CallError>, PathBuf, FileId),
    Synced(Result<(), CallError>, PathBuf, FileId),
    Sized(Result<u64, CallError>, PathBuf, FileId),
    Read(Result<Vec<u8>, CallError>, PathBuf, FileId),
    Closed(Result<(), CallError>, PathBuf, Vec<u8>),
    Metadata(Result<PathMetadata, CallError>, PathBuf, Vec<u8>),
    Renamed(Result<(), CallError>, PathBuf, Vec<u8>),
    DirectoryRead(Result<Vec<PathBuf>, CallError>, PathBuf, Vec<u8>),
    ParentSynced(Result<(), CallError>, PathBuf, Vec<u8>),
    Removed(Result<(), CallError>, Vec<u8>),
}

#[derive(Debug)]
struct FileService {
    seen: Arc<Mutex<Vec<String>>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum HeldFileMsg {
    Start(PathBuf),
    Opened(Result<FileId, CallError>),
}

#[derive(Debug)]
struct HeldFileService {
    seen: Arc<Mutex<Vec<String>>>,
}

const FILE_SERVICE_PAYLOAD: &[u8] = b"local system file";

#[tina_runtime::isolate(message = FileMsg, shard = AppShard)]
impl FileService {
    fn handle(
        &mut self,
        msg: FileMsg,
        _ctx: &mut Context<'_, AppShard, Self::Reply>,
    ) -> Effect<Self> {
        match msg {
            FileMsg::Start(path) => {
                file_create(path.clone()).then(move |result| FileMsg::Opened(result, path))
            }
            FileMsg::Opened(Ok(file), path) => file_write(file, FILE_SERVICE_PAYLOAD.to_vec())
                .then(move |result| FileMsg::Wrote(result, path, file)),
            FileMsg::Wrote(Ok(count), path, file) if count == FILE_SERVICE_PAYLOAD.len() => {
                file_fsync(file).then(move |result| FileMsg::Synced(result, path, file))
            }
            FileMsg::Synced(Ok(()), path, file) => {
                file_size(file).then(move |result| FileMsg::Sized(result, path, file))
            }
            FileMsg::Sized(Ok(size), path, file) => {
                file_read(file, size as usize).then(move |result| FileMsg::Read(result, path, file))
            }
            FileMsg::Read(Ok(bytes), path, file) => {
                file_close(file).then(move |result| FileMsg::Closed(result, path, bytes))
            }
            FileMsg::Closed(Ok(()), path, bytes) => path_metadata(path.clone())
                .then(move |result| FileMsg::Metadata(result, path, bytes)),
            FileMsg::Metadata(Ok(metadata), path, bytes)
                if metadata.kind == PathKind::File
                    && metadata.len == Some(FILE_SERVICE_PAYLOAD.len() as u64) =>
            {
                let renamed = path.with_extension("renamed");
                rename_replace(path, renamed.clone())
                    .then(move |result| FileMsg::Renamed(result, renamed, bytes))
            }
            FileMsg::Renamed(Ok(()), renamed, bytes) => {
                let parent = renamed
                    .parent()
                    .expect("renamed file has parent")
                    .to_path_buf();
                read_dir(parent).then(move |result| FileMsg::DirectoryRead(result, renamed, bytes))
            }
            FileMsg::DirectoryRead(Ok(entries), renamed, bytes) if entries.contains(&renamed) => {
                sync_parent(renamed.clone())
                    .then(move |result| FileMsg::ParentSynced(result, renamed, bytes))
            }
            FileMsg::ParentSynced(Ok(()), renamed, bytes) => {
                remove_file(renamed).then(move |result| FileMsg::Removed(result, bytes))
            }
            FileMsg::Removed(Ok(()), bytes) => {
                self.seen
                    .lock()
                    .expect("file seen lock")
                    .push(String::from_utf8(bytes).expect("file payload utf8"));
                noop()
            }
            FileMsg::Opened(Err(_), _)
            | FileMsg::Wrote(Err(_), _, _)
            | FileMsg::Wrote(Ok(_), _, _)
            | FileMsg::Synced(Err(_), _, _)
            | FileMsg::Sized(Err(_), _, _)
            | FileMsg::Read(Err(_), _, _)
            | FileMsg::Closed(Err(_), _, _)
            | FileMsg::Metadata(Err(_), _, _)
            | FileMsg::Metadata(Ok(_), _, _)
            | FileMsg::Renamed(Err(_), _, _)
            | FileMsg::DirectoryRead(Err(_), _, _)
            | FileMsg::DirectoryRead(Ok(_), _, _)
            | FileMsg::ParentSynced(Err(_), _, _)
            | FileMsg::Removed(Err(_), _) => {
                self.seen
                    .lock()
                    .expect("file seen lock")
                    .push("failed".to_string());
                noop()
            }
        }
    }
}

#[tina_runtime::isolate(message = HeldFileMsg, shard = AppShard)]
impl HeldFileService {
    fn handle(
        &mut self,
        msg: HeldFileMsg,
        _ctx: &mut Context<'_, AppShard, Self::Reply>,
    ) -> Effect<Self> {
        match msg {
            HeldFileMsg::Start(path) => file_create(path).then(HeldFileMsg::Opened),
            HeldFileMsg::Opened(Ok(_file)) => {
                self.seen
                    .lock()
                    .expect("held file seen lock")
                    .push("opened".to_string());
                noop()
            }
            HeldFileMsg::Opened(Err(error)) => {
                self.seen
                    .lock()
                    .expect("held file seen lock")
                    .push(format!("failed:{error:?}"));
                noop()
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum DurableWorkerMsg {
    Recover,
    SnapshotLoaded(Result<Option<SnapshotImage>, CallError>),
    JournalLoaded(Result<tina_runtime::JournalReplay, CallError>),
    Feed(String),
    Durable(Result<(), CallError>, u64, Vec<u8>),
}

#[derive(Debug)]
struct DurableWorker {
    snapshot_path: PathBuf,
    journal_path: PathBuf,
    values: Vec<String>,
    last_journal_index: u64,
    observed: Arc<Mutex<Vec<String>>>,
}

#[tina_runtime::isolate(message = DurableWorkerMsg, shard = AppShard)]
impl DurableWorker {
    fn handle(
        &mut self,
        msg: DurableWorkerMsg,
        _ctx: &mut Context<'_, AppShard, Self::Reply>,
    ) -> Effect<Self> {
        match msg {
            DurableWorkerMsg::Recover => {
                snapshot_load(self.snapshot_path.clone()).then(DurableWorkerMsg::SnapshotLoaded)
            }
            DurableWorkerMsg::SnapshotLoaded(Ok(Some(snapshot))) => {
                self.values = decode_durable_values(&snapshot.bytes);
                self.last_journal_index = snapshot.last_journal_index;
                journal_replay(self.journal_path.clone()).then(DurableWorkerMsg::JournalLoaded)
            }
            DurableWorkerMsg::SnapshotLoaded(Ok(None)) => {
                journal_replay(self.journal_path.clone()).then(DurableWorkerMsg::JournalLoaded)
            }
            DurableWorkerMsg::SnapshotLoaded(Err(error)) => {
                self.observed
                    .lock()
                    .expect("durable observed lock")
                    .push(format!("snapshot-error:{error:?}"));
                noop()
            }
            DurableWorkerMsg::JournalLoaded(Ok(replay)) => {
                for record in replay.records {
                    if record.index > self.last_journal_index {
                        self.values
                            .push(String::from_utf8(record.bytes).expect("durable journal utf8"));
                        self.last_journal_index = record.index;
                    }
                }
                self.record_state();
                noop()
            }
            DurableWorkerMsg::JournalLoaded(Err(error)) => {
                self.observed
                    .lock()
                    .expect("durable observed lock")
                    .push(format!("journal-error:{error:?}"));
                noop()
            }
            DurableWorkerMsg::Feed(value) => {
                let index = self.last_journal_index + 1;
                let record = value.into_bytes();
                journal_append(self.journal_path.clone(), index, record.clone())
                    .then(move |result| DurableWorkerMsg::Durable(result, index, record))
            }
            DurableWorkerMsg::Durable(Ok(()), index, record) => {
                self.values
                    .push(String::from_utf8(record).expect("durable record utf8"));
                self.last_journal_index = index;
                self.record_state();
                noop()
            }
            DurableWorkerMsg::Durable(Err(error), _, _) => {
                self.observed
                    .lock()
                    .expect("durable observed lock")
                    .push(format!("append-error:{error:?}"));
                noop()
            }
        }
    }
}

impl DurableWorker {
    fn new(
        snapshot_path: PathBuf,
        journal_path: PathBuf,
        observed: Arc<Mutex<Vec<String>>>,
    ) -> Self {
        Self {
            snapshot_path,
            journal_path,
            values: Vec::new(),
            last_journal_index: 0,
            observed,
        }
    }

    fn record_state(&self) {
        self.observed
            .lock()
            .expect("durable observed lock")
            .push(self.values.join(","));
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum FrontendMsg {
    Feed(String),
}

#[derive(Debug)]
struct Frontend {
    worker: Address<DurableWorkerMsg>,
}

#[tina_runtime::isolate(
    message = FrontendMsg,
    send = Outbound<DurableWorkerMsg>,
    shard = AppShard
)]
impl Frontend {
    fn handle(
        &mut self,
        msg: FrontendMsg,
        _ctx: &mut Context<'_, AppShard, Self::Reply>,
    ) -> Effect<Self> {
        match msg {
            FrontendMsg::Feed(value) => send(self.worker, DurableWorkerMsg::Feed(value)),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum TcpJournalMsg {
    Start(PathBuf),
    Bound(Result<(ListenerId, SocketAddr), CallError>),
    Accepted(Result<(StreamId, SocketAddr), CallError>),
    Read(Result<Vec<u8>, CallError>, StreamId),
    Durable(Result<(), CallError>, StreamId, Vec<u8>, u64),
    Wrote(Result<usize, CallError>, StreamId),
    StreamClosed(Result<(), CallError>),
    ListenerClosed(Result<(), CallError>),
}

#[derive(Debug)]
struct TcpJournalService {
    published: Arc<Mutex<Option<SocketAddr>>>,
    observed: Arc<Mutex<Vec<String>>>,
    listener: Option<ListenerId>,
    journal_path: Option<PathBuf>,
    last_journal_index: u64,
}

#[tina_runtime::isolate(message = TcpJournalMsg, shard = AppShard)]
impl TcpJournalService {
    fn handle(
        &mut self,
        msg: TcpJournalMsg,
        _ctx: &mut Context<'_, AppShard, Self::Reply>,
    ) -> Effect<Self> {
        match msg {
            TcpJournalMsg::Start(journal_path) => {
                self.journal_path = Some(journal_path);
                tcp_bind("127.0.0.1:0".parse().expect("valid loopback")).then(TcpJournalMsg::Bound)
            }
            TcpJournalMsg::Bound(Ok((listener, addr))) => {
                self.listener = Some(listener);
                *self.published.lock().expect("published lock") = Some(addr);
                tcp_accept(listener).then(TcpJournalMsg::Accepted)
            }
            TcpJournalMsg::Accepted(Ok((stream, _peer))) => {
                tcp_read(stream, 64).then(move |result| TcpJournalMsg::Read(result, stream))
            }
            TcpJournalMsg::Read(Ok(bytes), stream) if bytes.is_empty() => {
                tcp_close_stream(stream).then(TcpJournalMsg::StreamClosed)
            }
            TcpJournalMsg::Read(Ok(bytes), stream) => {
                let index = self.last_journal_index + 1;
                let journal_path = self
                    .journal_path
                    .clone()
                    .expect("journal path set before read");
                journal_append(journal_path, index, bytes.clone())
                    .then(move |result| TcpJournalMsg::Durable(result, stream, bytes, index))
            }
            TcpJournalMsg::Durable(Ok(()), stream, bytes, index) => {
                self.last_journal_index = index;
                self.observed
                    .lock()
                    .expect("observed lock")
                    .push(String::from_utf8(bytes).expect("client payload utf8"));
                tcp_write(stream, b"journaled\n".to_vec())
                    .then(move |result| TcpJournalMsg::Wrote(result, stream))
            }
            TcpJournalMsg::Wrote(Ok(_), stream) => {
                tcp_close_stream(stream).then(TcpJournalMsg::StreamClosed)
            }
            TcpJournalMsg::StreamClosed(Ok(())) => match self.listener.take() {
                Some(listener) => tcp_close_listener(listener).then(TcpJournalMsg::ListenerClosed),
                None => stop(),
            },
            TcpJournalMsg::ListenerClosed(Ok(())) => {
                self.observed
                    .lock()
                    .expect("observed lock")
                    .push("closed".to_owned());
                stop()
            }
            TcpJournalMsg::Bound(Err(error))
            | TcpJournalMsg::Accepted(Err(error))
            | TcpJournalMsg::Read(Err(error), _)
            | TcpJournalMsg::Durable(Err(error), _, _, _)
            | TcpJournalMsg::Wrote(Err(error), _)
            | TcpJournalMsg::StreamClosed(Err(error))
            | TcpJournalMsg::ListenerClosed(Err(error)) => {
                self.observed
                    .lock()
                    .expect("observed lock")
                    .push(format!("failed:{error:?}"));
                stop()
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum StoragePressureMsg {
    Start(PathBuf),
    First(Result<(), CallError>),
    Second(Result<(), CallError>),
}

#[derive(Debug)]
struct StoragePressureService {
    observed: Arc<Mutex<Vec<String>>>,
}

#[tina_runtime::isolate(message = StoragePressureMsg, shard = AppShard)]
impl StoragePressureService {
    fn handle(
        &mut self,
        msg: StoragePressureMsg,
        _ctx: &mut Context<'_, AppShard, Self::Reply>,
    ) -> Effect<Self> {
        match msg {
            StoragePressureMsg::Start(path) => batch(vec![
                journal_append(path.clone(), 1, b"first".to_vec()).then(StoragePressureMsg::First),
                journal_append(path, 2, b"second".to_vec()).then(StoragePressureMsg::Second),
            ]),
            StoragePressureMsg::First(result) => {
                self.observed
                    .lock()
                    .expect("storage pressure observed lock")
                    .push(format!("first:{result:?}"));
                noop()
            }
            StoragePressureMsg::Second(result) => {
                self.observed
                    .lock()
                    .expect("storage pressure observed lock")
                    .push(format!("second:{result:?}"));
                noop()
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum CrossShardTcpMsg {
    Start,
    Bound(Result<(ListenerId, SocketAddr), CallError>),
    Accepted(Result<(StreamId, SocketAddr), CallError>),
    Read(Result<Vec<u8>, CallError>, StreamId),
    Persisted(Result<(), CallError>, Vec<u8>),
    Wrote(Result<usize, CallError>, StreamId),
    StreamClosed(Result<(), CallError>),
    ListenerClosed(Result<(), CallError>),
}

#[derive(Debug)]
struct CrossShardTcpFrontend {
    worker: Address<CrossShardPersistMsg>,
    published: Arc<Mutex<Option<SocketAddr>>>,
    observed: Arc<Mutex<Vec<String>>>,
    listener: Option<ListenerId>,
    stream: Option<StreamId>,
}

#[tina_runtime::isolate(
    message = CrossShardTcpMsg,
    send = Outbound<CrossShardPersistMsg>,
    shard = AppShard
)]
impl CrossShardTcpFrontend {
    fn handle(
        &mut self,
        msg: CrossShardTcpMsg,
        ctx: &mut Context<'_, AppShard, Self::Reply>,
    ) -> Effect<Self> {
        match msg {
            CrossShardTcpMsg::Start => tcp_bind("127.0.0.1:0".parse().expect("valid loopback"))
                .then(CrossShardTcpMsg::Bound),
            CrossShardTcpMsg::Bound(Ok((listener, addr))) => {
                self.listener = Some(listener);
                *self.published.lock().expect("published lock") = Some(addr);
                tcp_accept(listener).then(CrossShardTcpMsg::Accepted)
            }
            CrossShardTcpMsg::Accepted(Ok((stream, _peer))) => {
                tcp_read(stream, 64).then(move |result| CrossShardTcpMsg::Read(result, stream))
            }
            CrossShardTcpMsg::Read(Ok(bytes), stream) if bytes.is_empty() => {
                tcp_close_stream(stream).then(CrossShardTcpMsg::StreamClosed)
            }
            CrossShardTcpMsg::Read(Ok(bytes), stream) => {
                self.stream = Some(stream);
                send(
                    self.worker,
                    CrossShardPersistMsg::Persist {
                        bytes,
                        reply_to: ctx.me(),
                    },
                )
            }
            CrossShardTcpMsg::Persisted(Ok(()), bytes) => {
                self.observed
                    .lock()
                    .expect("cross shard tcp observed lock")
                    .push(String::from_utf8(bytes.clone()).expect("client payload utf8"));
                let stream = self.stream.take().expect("stream pending durable ack");
                let mut reply = b"persisted:".to_vec();
                reply.extend(bytes);
                reply.push(b'\n');
                tcp_write(stream, reply).then(move |result| CrossShardTcpMsg::Wrote(result, stream))
            }
            CrossShardTcpMsg::Wrote(Ok(_), stream) => {
                tcp_close_stream(stream).then(CrossShardTcpMsg::StreamClosed)
            }
            CrossShardTcpMsg::StreamClosed(Ok(())) => match self.listener.take() {
                Some(listener) => {
                    tcp_close_listener(listener).then(CrossShardTcpMsg::ListenerClosed)
                }
                None => stop(),
            },
            CrossShardTcpMsg::ListenerClosed(Ok(())) => {
                self.observed
                    .lock()
                    .expect("cross shard tcp observed lock")
                    .push("closed".to_string());
                stop()
            }
            CrossShardTcpMsg::Bound(Err(error))
            | CrossShardTcpMsg::Accepted(Err(error))
            | CrossShardTcpMsg::Read(Err(error), _)
            | CrossShardTcpMsg::Persisted(Err(error), _)
            | CrossShardTcpMsg::Wrote(Err(error), _)
            | CrossShardTcpMsg::StreamClosed(Err(error))
            | CrossShardTcpMsg::ListenerClosed(Err(error)) => {
                self.observed
                    .lock()
                    .expect("cross shard tcp observed lock")
                    .push(format!("failed:{error:?}"));
                stop()
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum CrossShardPersistMsg {
    Persist {
        bytes: Vec<u8>,
        reply_to: Address<CrossShardTcpMsg>,
    },
    Durable(
        Result<(), CallError>,
        Vec<u8>,
        Address<CrossShardTcpMsg>,
        u64,
    ),
}

#[derive(Debug)]
struct CrossShardPersistWorker {
    journal_path: PathBuf,
    last_journal_index: u64,
}

#[tina_runtime::isolate(
    message = CrossShardPersistMsg,
    send = Outbound<CrossShardTcpMsg>,
    shard = AppShard
)]
impl CrossShardPersistWorker {
    fn handle(
        &mut self,
        msg: CrossShardPersistMsg,
        _ctx: &mut Context<'_, AppShard, Self::Reply>,
    ) -> Effect<Self> {
        match msg {
            CrossShardPersistMsg::Persist { bytes, reply_to } => {
                let index = self.last_journal_index + 1;
                journal_append(self.journal_path.clone(), index, bytes.clone()).then(
                    move |result| CrossShardPersistMsg::Durable(result, bytes, reply_to, index),
                )
            }
            CrossShardPersistMsg::Durable(Ok(()), bytes, reply_to, index) => {
                self.last_journal_index = index;
                send(reply_to, CrossShardTcpMsg::Persisted(Ok(()), bytes))
            }
            CrossShardPersistMsg::Durable(Err(error), bytes, reply_to, _) => {
                send(reply_to, CrossShardTcpMsg::Persisted(Err(error), bytes))
            }
        }
    }
}

fn decode_durable_values(bytes: &[u8]) -> Vec<String> {
    if bytes.is_empty() {
        return Vec::new();
    }
    String::from_utf8(bytes.to_vec())
        .expect("durable snapshot utf8")
        .split('\n')
        .map(str::to_owned)
        .collect()
}

fn wait_until(mut predicate: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < deadline {
        if predicate() {
            return;
        }
        thread::sleep(Duration::from_millis(5));
    }
    assert!(predicate(), "condition did not become true before timeout");
}

/// OS CPU ids this process is allowed to run on. Tests pick pin targets from
/// this mask instead of assuming CPU 0 exists or that ids are dense.
#[cfg(target_os = "linux")]
fn allowed_cores() -> Vec<usize> {
    // SAFETY: a zeroed `cpu_set_t` is a valid empty out-parameter; pid 0 reads
    // the calling thread's allowed mask.
    let mut set: libc::cpu_set_t = unsafe { std::mem::zeroed() };
    let rc =
        unsafe { libc::sched_getaffinity(0, std::mem::size_of::<libc::cpu_set_t>(), &mut set) };
    assert_eq!(
        rc,
        0,
        "sched_getaffinity failed: {}",
        std::io::Error::last_os_error()
    );
    (0..(libc::CPU_SETSIZE as usize))
        // SAFETY: `cpu` is below CPU_SETSIZE and `set` was filled above.
        .filter(|&cpu| unsafe { libc::CPU_ISSET(cpu, &set) })
        .collect()
}

/// Asserts a shard configured to `core` reports the right pin outcome: a proven
/// Linux pin (`Applied` + observed core) when `core` is in the allowed mask,
/// `Failed` when it is not, and `Unsupported` on platforms without a hard pin.
fn assert_pin_outcome(shard: &LiveShardReport, core: usize) {
    #[cfg(target_os = "linux")]
    {
        if allowed_cores().contains(&core) {
            assert_eq!(shard.affinity_status(), &AffinityStatus::Applied);
            assert_eq!(shard.observed_core(), Some(core));
        } else {
            assert!(
                matches!(shard.affinity_status(), AffinityStatus::Failed(_)),
                "core {core} outside the allowed mask must fail loudly, got {:?}",
                shard.affinity_status()
            );
            assert_eq!(shard.observed_core(), None);
        }
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = core;
        assert_eq!(shard.affinity_status(), &AffinityStatus::Unsupported);
        assert_eq!(shard.observed_core(), None);
    }
}

fn wait_until_debug(mut predicate: impl FnMut() -> bool, debug: impl Fn() -> String) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        if predicate() {
            return;
        }
        thread::sleep(Duration::from_millis(5));
    }
    assert!(
        predicate(),
        "condition did not become true before timeout; {}",
        debug()
    );
}

fn wait_for_addr(published: &Arc<Mutex<Option<SocketAddr>>>) -> SocketAddr {
    let deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < deadline {
        if let Some(addr) = *published.lock().expect("published lock") {
            return addr;
        }
        thread::sleep(Duration::from_millis(5));
    }
    published
        .lock()
        .expect("published lock")
        .expect("service did not publish address before timeout")
}

fn connect_rustls_client(
    addr: SocketAddr,
    cert_der: Vec<u8>,
) -> rustls::StreamOwned<rustls::ClientConnection, TcpStream> {
    let tcp = TcpStream::connect(addr).expect("connect tls server");
    let mut roots = rustls::RootCertStore::empty();
    roots
        .add(rustls::pki_types::CertificateDer::from(cert_der))
        .expect("add root cert");
    let config = rustls::ClientConfig::builder()
        .with_root_certificates(roots)
        .with_no_client_auth();
    let server_name = rustls::pki_types::ServerName::try_from("localhost").expect("server name");
    let connection =
        rustls::ClientConnection::new(Arc::new(config), server_name).expect("client connection");
    let mut stream = rustls::StreamOwned::new(connection, tcp);
    while stream.conn.is_handshaking() {
        stream
            .conn
            .complete_io(&mut stream.sock)
            .expect("client handshake");
    }
    stream
}

fn wait_for_flag(flag: &Arc<(Mutex<bool>, Condvar)>) {
    let deadline = Instant::now() + Duration::from_secs(2);
    let (lock, cv) = &**flag;
    let mut guard = lock.lock().expect("flag lock");
    while !*guard {
        let now = Instant::now();
        assert!(now < deadline, "flag did not become true before timeout");
        let remaining = deadline.saturating_duration_since(now);
        let (next, _) = cv.wait_timeout(guard, remaining).expect("flag wait");
        guard = next;
    }
}

fn release_flag(flag: &Arc<(Mutex<bool>, Condvar)>) {
    let (lock, cv) = &**flag;
    *lock.lock().expect("release flag lock") = true;
    cv.notify_all();
}

fn unique_dir(label: &str) -> PathBuf {
    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock after epoch")
        .as_nanos();
    std::env::temp_dir().join(format!("tina-{label}-{}-{unique}", std::process::id()))
}

#[test]
fn local_system_single_shard_is_canonical_live_owner() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let app = LocalSystem::single_shard(AppShard(34), AppMailboxFactory)
        .ingress_capacity(8)
        .trace_retention(TraceRetention::Bounded(64))
        .build();

    assert_eq!(app.state(), LocalSystemState::Accepting);
    let address = app
        .register_root::<LlamaService, Infallible>(
            LlamaService {
                seen: Arc::clone(&seen),
            },
            8,
        )
        .expect("register root");

    app.try_send(address, LlamaMsg::Feed(7))
        .expect("bounded handoff");
    wait_until(|| seen.lock().expect("seen lock").as_slice() == [7]);

    let terminal = app
        .shutdown()
        .drain()
        .join()
        .expect("local system shutdown");
    assert_eq!(terminal.state(), LocalSystemState::Closed);
    assert!(terminal.trace().iter().any(|event| {
        matches!(
            event.kind(),
            RuntimeEventKind::HandlerFinished {
                effect: tina_runtime::EffectKind::Noop
            }
        )
    }));
}

// LocalSystem.topology() must surface every bounded lane capacity,
// the worker-held / pending-call counts, and the unclean_reason on
// the terminal report.
#[test]
fn local_system_topology_report_exposes_all_lane_capacities_and_new_counts() {
    let config = LocalSystemConfig {
        ingress_capacity: 4,
        shard_pair_capacity: 4,
        storage_lane_capacity: 5,
        dns_lane_capacity: 6,
        tls_lane_capacity: 7,
        process_lane_capacity: 8,
        signal_capacity: 9,
        ..LocalSystemConfig::default()
    };
    let app = LocalSystem::single_shard(AppShard(91), AppMailboxFactory)
        .config(config)
        .build();

    let topology = app.topology();
    let shard = topology
        .shard(ShardId::new(91))
        .expect("single shard report exists");
    // All six bounded lanes must be reachable from the topology.
    assert_eq!(shard.ingress().capacity(), 4);
    assert_eq!(shard.storage_lane().capacity(), 5);
    assert_eq!(shard.dns_lane().capacity(), 6);
    assert_eq!(shard.tls_lane().capacity(), 7);
    assert_eq!(shard.process_lane().capacity(), 8);
    assert_eq!(shard.signal_lane().capacity(), 9);
    // Resource-count fields are populated and start at zero.
    assert_eq!(shard.owned_resource_count(), 0);
    assert_eq!(shard.worker_held_resource_count(), 0);
    assert_eq!(shard.pending_driver_call_count(), 0);

    let terminal = app.shutdown().drain().join().expect("clean shutdown");
    let report = terminal.shutdown_report();
    assert!(report.clean());
    assert!(report.unclean_reason().is_none());
    assert_eq!(report.remaining_owned_resource_count(), 0);
    assert_eq!(report.remaining_worker_held_resource_count(), 0);
    assert_eq!(report.remaining_pending_driver_call_count(), 0);
}

#[test]
fn local_system_builder_exposes_complete_budget_manifest_without_low_level_config() {
    let app = LocalSystem::single_shard(AppShard(92), AppMailboxFactory)
        .ingress_capacity(3)
        .storage_lane_capacity(4)
        .dns_lane_capacity(5)
        .tls_lane_capacity(6)
        .process_lane_capacity(7)
        .signal_capacity(8)
        .remote_inbound_drain_budget(9)
        .shutdown_lane_drain_timeout(Duration::from_millis(17))
        .trace_retention(TraceRetention::Bounded(33))
        .build();

    let topology = app.topology();
    let shard = topology
        .shard(ShardId::new(92))
        .expect("single shard report exists");
    assert_eq!(shard.ingress().capacity(), 3);
    assert_eq!(shard.storage_lane().capacity(), 4);
    assert_eq!(shard.dns_lane().capacity(), 5);
    assert_eq!(shard.tls_lane().capacity(), 6);
    assert_eq!(shard.process_lane().capacity(), 7);
    assert_eq!(shard.signal_lane().capacity(), 8);
    assert_eq!(shard.remote_inbound_drain_budget(), 9);
    assert_eq!(
        shard.shutdown_lane_drain_timeout(),
        Duration::from_millis(17)
    );
    assert_eq!(shard.trace_retention(), TraceRetention::Bounded(33));

    app.shutdown().drain().join().expect("clean shutdown");
}

#[test]
fn local_multi_shard_builder_exposes_complete_budget_manifest_without_low_level_config() {
    let app = LocalSystem::<AppShard, AppMailboxFactory>::multi_shard(AppMailboxFactory)
        .shard(AppShard(93))
        .shard(AppShard(94))
        .ingress_capacity(3)
        .shard_pair_capacity(4)
        .storage_lane_capacity(5)
        .dns_lane_capacity(6)
        .tls_lane_capacity(7)
        .process_lane_capacity(8)
        .signal_capacity(9)
        .remote_inbound_drain_budget(10)
        .shutdown_lane_drain_timeout(Duration::from_millis(19))
        .trace_retention(TraceRetention::Bounded(35))
        .build();

    let topology = app.topology();
    for shard_id in [ShardId::new(93), ShardId::new(94)] {
        let shard = topology.shard(shard_id).expect("shard report exists");
        assert_eq!(shard.ingress().capacity(), 3);
        assert_eq!(shard.storage_lane().capacity(), 5);
        assert_eq!(shard.dns_lane().capacity(), 6);
        assert_eq!(shard.tls_lane().capacity(), 7);
        assert_eq!(shard.process_lane().capacity(), 8);
        assert_eq!(shard.signal_lane().capacity(), 9);
        assert_eq!(shard.remote_inbound_drain_budget(), 10);
        assert_eq!(
            shard.shutdown_lane_drain_timeout(),
            Duration::from_millis(19)
        );
        assert_eq!(shard.trace_retention(), TraceRetention::Bounded(35));
    }
    assert!(
        topology
            .remote_queues()
            .iter()
            .all(|queue| queue.queue().capacity() == 4)
    );

    app.shutdown().drain().join().expect("clean shutdown");
}

#[test]
fn local_system_topology_report_before_and_after_shutdown() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let app = LocalSystem::single_shard(AppShard(35), AppMailboxFactory)
        .ingress_capacity(3)
        .storage_lane_capacity(2)
        .remote_inbound_drain_budget(5)
        .trace_retention(TraceRetention::Bounded(64))
        .build();
    wait_until(|| {
        app.topology()
            .shard(ShardId::new(35))
            .and_then(|shard| shard.worker_thread_id())
            .is_some()
    });

    let running = app.topology();
    let shard = running
        .shard(ShardId::new(35))
        .expect("single shard report exists");
    assert_eq!(shard.state(), LiveShardState::Running);
    assert_eq!(shard.ingress().capacity(), 3);
    assert_eq!(shard.ingress().depth(), None);
    assert_eq!(shard.storage_lane().capacity(), 2);
    assert_eq!(shard.storage_lane().accepted(), None);
    assert_eq!(shard.storage_lane().rejected_full(), None);
    assert_eq!(shard.remote_inbound_drain_budget(), 5);
    assert_eq!(shard.trace_retention(), TraceRetention::Bounded(64));
    assert_eq!(shard.worker_name(), Some("tina-shard-35"));
    assert!(shard.worker_thread_id().is_some());
    assert_eq!(shard.configured_core(), None);
    assert_eq!(shard.observed_core(), None);
    assert_eq!(shard.affinity_status(), &AffinityStatus::NotRequested);

    let address = app
        .register_root::<LlamaService, Infallible>(
            LlamaService {
                seen: Arc::clone(&seen),
            },
            8,
        )
        .expect("register root");
    app.try_send(address, LlamaMsg::Feed(11))
        .expect("bounded handoff");
    wait_until(|| seen.lock().expect("seen lock").as_slice() == [11]);

    let terminal = app.shutdown().drain().join().expect("shutdown app");
    assert_eq!(terminal.state(), LocalSystemState::Closed);
    assert_eq!(
        terminal.shutdown_report().final_state(),
        LocalSystemState::Closed
    );
    assert!(terminal.shutdown_report().clean());
    assert_eq!(terminal.shutdown_report().failed_shards(), []);
    assert_eq!(
        terminal.shutdown_report().remaining_owned_resource_count(),
        0
    );
    let terminal_topology = terminal.topology().expect("terminal topology");
    assert_eq!(
        terminal_topology
            .shard(ShardId::new(35))
            .expect("terminal shard")
            .state(),
        LiveShardState::Stopped
    );
}

#[test]
fn local_system_topology_reports_pin_outcome_for_configured_core() {
    // Headline proof: on Linux pick a core from the process's allowed affinity
    // mask, pin to it, and assert the worker observes itself there (Applied +
    // observed == requested). Off Linux the same request reports Unsupported.
    #[cfg(target_os = "linux")]
    let core = *allowed_cores()
        .first()
        .expect("the process has at least one allowed core");
    #[cfg(not(target_os = "linux"))]
    let core = 0usize;

    let app = LocalSystem::single_shard(AppShard(36), AppMailboxFactory)
        .configured_core(core)
        .build();
    // The worker records its thread id only after the pin runs, so waiting on
    // the thread id means the affinity status below is the proven outcome.
    wait_until(|| {
        app.topology()
            .shard(ShardId::new(36))
            .and_then(|shard| shard.worker_thread_id())
            .is_some()
    });

    let topology = app.topology();
    let shard = topology
        .shard(ShardId::new(36))
        .expect("single shard report exists");
    assert_eq!(shard.worker_name(), Some("tina-shard-36"));
    assert!(shard.worker_thread_id().is_some());
    assert_eq!(shard.configured_core(), Some(core));
    assert_pin_outcome(shard, core);

    // A pinned shard still serves: register an isolate and prove a message is
    // handled on the pinned worker.
    let seen = Arc::new(Mutex::new(Vec::new()));
    let address = app
        .register_root::<LlamaService, Infallible>(
            LlamaService {
                seen: Arc::clone(&seen),
            },
            8,
        )
        .expect("register root on pinned shard");
    app.try_send(address, LlamaMsg::Feed(7))
        .expect("bounded handoff to pinned shard");
    wait_until(|| seen.lock().expect("seen lock").as_slice() == [7]);
    assert_eq!(
        app.topology()
            .shard(ShardId::new(36))
            .expect("shard report")
            .state(),
        LiveShardState::Running
    );

    app.shutdown().drain().join().expect("clean shutdown");
}

#[test]
#[cfg(target_os = "linux")]
fn local_system_unavailable_core_reports_failed_and_keeps_serving() {
    // A requested core outside the process's allowed mask must fail loudly and
    // keep the shard serving unpinned — never crash, never silently mis-pin.
    let allowed = allowed_cores();
    let max = allowed.iter().copied().max().unwrap_or(0);
    let absent = (0..=max + 1)
        .find(|cpu| !allowed.contains(cpu))
        .expect("an unallowed core id exists at or below max+1");

    let seen = Arc::new(Mutex::new(Vec::new()));
    let app = LocalSystem::single_shard(AppShard(37), AppMailboxFactory)
        .configured_core(absent)
        .build();
    let address = app
        .register_root::<LlamaService, Infallible>(
            LlamaService {
                seen: Arc::clone(&seen),
            },
            8,
        )
        .expect("register root despite failed pin");
    app.try_send(address, LlamaMsg::Feed(13))
        .expect("bounded handoff to unpinned shard");
    wait_until(|| seen.lock().expect("seen lock").as_slice() == [13]);

    let topology = app.topology();
    let shard = topology.shard(ShardId::new(37)).expect("shard report");
    assert!(
        matches!(shard.affinity_status(), AffinityStatus::Failed(_)),
        "an unavailable core must report Failed, got {:?}",
        shard.affinity_status()
    );
    assert_eq!(shard.observed_core(), None);
    assert_eq!(shard.configured_core(), Some(absent));
    assert_eq!(shard.state(), LiveShardState::Running);

    app.shutdown().drain().join().expect("clean shutdown");
}

#[test]
fn local_multi_shard_topology_reports_pin_outcome_per_shard() {
    // Prefer two contiguous allowed ids so both shards prove Applied; fall back
    // to the lowest allowed id (per-shard outcome still asserted). Never assume
    // CPU 0 exists or that allowed ids are dense.
    #[cfg(target_os = "linux")]
    let base = {
        let allowed = allowed_cores();
        (0..)
            .take(libc::CPU_SETSIZE as usize)
            .find(|c| allowed.contains(c) && allowed.contains(&(c + 1)))
            .or_else(|| allowed.first().copied())
            .expect("the process has at least one allowed core")
    };
    #[cfg(not(target_os = "linux"))]
    let base = 8usize;

    let app = LocalSystem::multi_shard(AppMailboxFactory)
        .shard(AppShard(42))
        .shard(AppShard(40))
        .configured_core(base)
        .build();
    wait_until(|| {
        let topology = app.topology();
        topology
            .shards()
            .iter()
            .all(|shard| shard.worker_thread_id().is_some())
    });

    let topology = app.topology();
    let first = topology.shard(ShardId::new(40)).expect("first shard");
    let second = topology.shard(ShardId::new(42)).expect("second shard");
    // Stable shard order pins shard 40 (ordinal 0) to base, shard 42 to base+1.
    assert_eq!(first.configured_core(), Some(base));
    assert_eq!(second.configured_core(), Some(base + 1));
    assert_pin_outcome(first, base);
    assert_pin_outcome(second, base + 1);

    app.shutdown().drain().join().expect("clean shutdown");
}

#[test]
fn local_system_topology_reports_runtime_metadata_preallocation() {
    let preallocation = PreallocationConfig {
        entry_capacity: 17,
        child_record_capacity: 11,
        supervisor_capacity: 7,
        trace_capacity: 333,
        call_capacity: 19,
        round_scratch_capacity: 23,
    };
    let app = LocalSystem::single_shard(AppShard(38), AppMailboxFactory)
        .preallocation(preallocation)
        .build();

    let topology = app.topology();
    let shard = topology
        .shard(ShardId::new(38))
        .expect("single shard report exists");
    assert_eq!(shard.preallocation(), preallocation);

    app.shutdown().drain().join().expect("clean shutdown");
}

#[test]
fn blue_whale_combined_e2e_core_preallocation_and_cross_shard_call() {
    let outcomes = Arc::new(Mutex::new(Vec::new()));
    let preallocation = PreallocationConfig {
        entry_capacity: 12,
        child_record_capacity: 4,
        supervisor_capacity: 2,
        trace_capacity: 256,
        call_capacity: 16,
        round_scratch_capacity: 12,
    };
    let app = LocalSystem::<AppShard, AppMailboxFactory>::multi_shard(AppMailboxFactory)
        .shard(AppShard(45))
        .shard(AppShard(46))
        .configured_core(3)
        .remote_inbound_drain_budget(1)
        .preallocation(preallocation)
        .trace_retention(TraceRetention::Bounded(256))
        .build();
    wait_until(|| {
        app.topology()
            .shards()
            .iter()
            .all(|shard| shard.worker_thread_id().is_some())
    });

    let topology = app.topology();
    assert_eq!(
        topology
            .shard(ShardId::new(45))
            .expect("client shard")
            .configured_core(),
        Some(3)
    );
    assert_eq!(
        topology
            .shard(ShardId::new(46))
            .expect("worker shard")
            .configured_core(),
        Some(4)
    );
    assert!(
        topology
            .shards()
            .iter()
            .all(|shard| shard.preallocation() == preallocation)
    );
    assert!(
        topology
            .shards()
            .iter()
            .all(|shard| shard.remote_inbound_drain_budget() == 1)
    );

    let worker = app
        .register_root_on::<CrossShardCallWorker, Infallible>(
            ShardId::new(46),
            CrossShardCallWorker::default(),
            8,
        )
        .expect("register worker");
    let client = app
        .register_root_on::<CrossShardCallClient, Infallible>(
            ShardId::new(45),
            CrossShardCallClient {
                outcomes: Arc::clone(&outcomes),
            },
            8,
        )
        .expect("register client");

    app.try_send(client, CrossShardCallClientMsg::Start(worker))
        .expect("start handoff");
    wait_until(|| {
        outcomes.lock().expect("outcomes").iter().any(|outcome| {
            matches!(
                outcome,
                CallOutcome::Replied(CrossShardCallReply(bytes)) if bytes == b"llama"
            )
        })
    });

    let terminal = app.shutdown().drain().join().expect("clean shutdown");
    assert!(terminal.shutdown_report().clean());
}

#[test]
fn live_cross_shard_terminal_replies_do_not_degrade_to_timeout_under_reply_queue_pressure() {
    let outcomes = Arc::new(Mutex::new(Vec::new()));
    let held_count = Arc::new(Mutex::new(0_usize));
    let app = LocalSystem::<AppShard, AppMailboxFactory>::multi_shard(AppMailboxFactory)
        .shard(AppShard(145))
        .shard(AppShard(146))
        .ingress_capacity(16)
        .shard_pair_capacity(1)
        .trace_retention(TraceRetention::Bounded(512))
        .build();

    let worker = app
        .register_root_on::<CrossShardCallWorker, Infallible>(
            ShardId::new(146),
            CrossShardCallWorker {
                held: Vec::new(),
                held_count: Some(Arc::clone(&held_count)),
            },
            16,
        )
        .expect("register pressure worker");
    let client = app
        .register_root_on::<CrossShardCallClient, Infallible>(
            ShardId::new(145),
            CrossShardCallClient {
                outcomes: Arc::clone(&outcomes),
            },
            16,
        )
        .expect("register pressure client");

    for id in 0..3 {
        app.try_send(client, CrossShardCallClientMsg::StartHeldOne(worker, id))
            .expect("start held call");
        wait_until_debug(
            || *held_count.lock().expect("held count") == usize::from(id) + 1,
            || format!("held={}", *held_count.lock().expect("held count")),
        );
    }

    app.try_send(worker, CrossShardCallWorkerMsg::ReleaseHeld)
        .expect("release held calls");

    wait_until_debug(
        || outcomes.lock().expect("outcomes").len() == 3,
        || format!("outcomes={:?}", outcomes.lock().expect("outcomes")),
    );
    let got = outcomes.lock().expect("outcomes").clone();
    let mut replied = got
        .into_iter()
        .map(|outcome| match outcome {
            CallOutcome::Replied(CrossShardCallReply(bytes)) => bytes,
            other => panic!("terminal reply degraded under pressure: {other:?}"),
        })
        .collect::<Vec<_>>();
    replied.sort();
    assert_eq!(replied, vec![vec![0], vec![1], vec![2]]);

    let terminal = app
        .shutdown()
        .drain()
        .join()
        .expect("shutdown pressure app");
    assert!(!terminal.trace().iter().any(|event| {
        matches!(
            event.kind(),
            RuntimeEventKind::CallReplyRejected {
                reason: tina_runtime::CallReplyRejectedReason::ReplyPathFull,
                ..
            }
        )
    }));
}

#[test]
fn local_system_capabilities_name_supported_and_unsupported_resource_families() {
    let config = LocalSystemConfig {
        storage_lane_capacity: 7,
        dns_lane_capacity: 3,
        tls_lane_capacity: 5,
        process_lane_capacity: 2,
        signal_capacity: 4,
        ..LocalSystemConfig::default()
    };
    let app = LocalSystem::single_shard(AppShard(37), AppMailboxFactory)
        .config(config)
        .build();

    let capabilities = app.capabilities();
    assert_eq!(capabilities.timers.support(), ResourceSupport::Supported);
    assert_eq!(
        capabilities.timers.execution(),
        ResourceExecutionShape::Inline
    );
    assert_eq!(
        capabilities.timers.cancellation(),
        CancellationSupport::CancelableBeforeStart
    );
    assert_eq!(capabilities.timers.shutdown(), ShutdownSupport::Canceled);

    assert_eq!(capabilities.tcp.support(), ResourceSupport::Supported);
    assert_eq!(
        capabilities.tcp.execution(),
        ResourceExecutionShape::CompletionBacked
    );
    assert_eq!(
        capabilities.tcp.cancellation(),
        CancellationSupport::TombstonedAfterStart
    );

    assert_eq!(
        capabilities.local_file.execution(),
        ResourceExecutionShape::CompletionBacked
    );
    // Live durability rides the Betelgeuse completion rail; the ops
    // Betelgeuse lacks fall to the named off-shard fallback worker.
    assert_eq!(
        capabilities.storage_lane.execution(),
        ResourceExecutionShape::CompletionBacked
    );
    assert_eq!(
        capabilities.local_persistence.execution(),
        ResourceExecutionShape::CompletionBacked
    );
    assert_eq!(
        capabilities.storage_metadata_fallback.execution(),
        ResourceExecutionShape::LaneBackedBlocking
    );
    assert_eq!(capabilities.storage_lane.capacity(), Some(7));
    assert_eq!(capabilities.local_persistence.capacity(), Some(7));
    assert_eq!(capabilities.storage_metadata_fallback.capacity(), Some(7));

    assert_eq!(capabilities.dns.support(), ResourceSupport::Supported);
    assert_eq!(
        capabilities.dns.execution(),
        ResourceExecutionShape::LaneBackedBlocking
    );
    assert_eq!(
        capabilities.dns.cancellation(),
        CancellationSupport::TombstonedAfterStart
    );
    assert_eq!(capabilities.dns.capacity(), Some(3));
    assert_eq!(capabilities.udp.support(), ResourceSupport::Supported);
    assert_eq!(
        capabilities.udp.execution(),
        ResourceExecutionShape::PollBacked
    );
    assert_eq!(capabilities.process.support(), ResourceSupport::Supported);
    assert_eq!(
        capabilities.process.execution(),
        ResourceExecutionShape::LaneBackedBlocking
    );
    assert_eq!(capabilities.process.capacity(), Some(2));
    assert_eq!(capabilities.signal.support(), ResourceSupport::Supported);
    assert_eq!(
        capabilities.signal.execution(),
        ResourceExecutionShape::PollBacked
    );
    assert_eq!(capabilities.signal.capacity(), Some(4));
    assert_eq!(capabilities.tls.support(), ResourceSupport::Supported);
    // TLS rides the Betelgeuse TCP rail now (rustls sans-I/O on the shard), so
    // it is completion-backed like TCP — not a blocking worker lane.
    assert_eq!(
        capabilities.tls.execution(),
        ResourceExecutionShape::CompletionBacked
    );
    assert_eq!(
        capabilities.tls.cancellation(),
        CancellationSupport::TombstonedAfterStart
    );
    // tls_lane_capacity stays the shard-total cap on in-flight TLS ops.
    assert_eq!(capabilities.tls.capacity(), Some(5));

    assert_eq!(
        capabilities.durability.file_fsync,
        PersistenceSupportLevel::Supported
    );
    assert!(capabilities.durability.commit_uncertain_possible);
    #[cfg(unix)]
    assert_eq!(
        capabilities.durability.directory_fsync_after_rename,
        PersistenceSupportLevel::Supported
    );
    #[cfg(not(unix))]
    assert_eq!(
        capabilities.durability.directory_fsync_after_rename,
        PersistenceSupportLevel::NotClaimed
    );

    // Unix-domain rail: live substrate-backed lane on Unix (rides the
    // Betelgeuse completion loop like TCP), typed unsupported (not
    // cfg-silent) elsewhere.
    #[cfg(unix)]
    {
        assert_eq!(capabilities.unix.support(), ResourceSupport::Supported);
        assert_eq!(
            capabilities.unix.execution(),
            ResourceExecutionShape::CompletionBacked
        );
        assert_eq!(
            capabilities.unix.cancellation(),
            CancellationSupport::TombstonedAfterStart
        );
    }
    #[cfg(not(unix))]
    {
        assert_eq!(capabilities.unix.support(), ResourceSupport::Unsupported);
        assert_eq!(
            capabilities.unix.execution(),
            ResourceExecutionShape::NotApplicable
        );
    }

    app.shutdown()
        .drain()
        .join()
        .expect("local system shutdown");
}

#[test]
fn local_system_dns_rail_resolves_through_bounded_runtime_lane() {
    let observed = Arc::new(Mutex::new(Vec::new()));
    let app = LocalSystem::single_shard(AppShard(39), AppMailboxFactory)
        .trace_retention(TraceRetention::Bounded(64))
        .build();
    let address = app
        .register_root::<DnsLookupService, Infallible>(
            DnsLookupService {
                observed: Arc::clone(&observed),
            },
            8,
        )
        .expect("register dns service");

    app.try_send(address, DnsLookupMsg::Start)
        .expect("start dns service");
    wait_until(|| {
        observed
            .lock()
            .expect("dns observed lock")
            .iter()
            .any(|entry| entry.starts_with("resolved:"))
    });

    let terminal = app
        .shutdown()
        .drain()
        .join()
        .expect("local system shutdown");
    assert!(terminal.trace().iter().any(|event| {
        matches!(
            event.kind(),
            RuntimeEventKind::CallCompleted {
                call_kind: CallKind::DnsLookup,
                ..
            }
        )
    }));
}

#[test]
fn local_system_tls_rail_connects_writes_reads_and_closes() {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let certified = rcgen::generate_simple_self_signed(vec!["localhost".to_string()])
        .expect("generate local cert");
    let cert_der = certified.cert.der().to_vec();
    let key_der = certified.key_pair.serialize_der();
    let server_cert = rustls::pki_types::CertificateDer::from(cert_der.clone());
    let server_key = rustls::pki_types::PrivateKeyDer::Pkcs8(
        rustls::pki_types::PrivatePkcs8KeyDer::from(key_der),
    );
    let server_config = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(vec![server_cert], server_key)
        .expect("server config");

    let listener = TcpListener::bind("127.0.0.1:0").expect("bind tls test listener");
    let addr = listener.local_addr().expect("local addr");
    let server = thread::spawn(move || {
        let (tcp, _) = listener.accept().expect("accept tls client");
        let connection =
            rustls::ServerConnection::new(Arc::new(server_config)).expect("server connection");
        let mut stream = rustls::StreamOwned::new(connection, tcp);
        let mut request = [0_u8; 4];
        stream.read_exact(&mut request).expect("read request");
        assert_eq!(&request, b"ping");
        stream.write_all(b"pong").expect("write response");
        stream.flush().expect("flush response");
    });

    let observed = Arc::new(Mutex::new(Vec::new()));
    let app = LocalSystem::single_shard(AppShard(42), AppMailboxFactory)
        .trace_retention(TraceRetention::Bounded(128))
        .build();
    let address = app
        .register_root::<TlsClientService, Infallible>(
            TlsClientService {
                observed: Arc::clone(&observed),
                stream: None,
            },
            8,
        )
        .expect("register tls service");

    app.try_send(address, TlsClientMsg::Start { addr, cert_der })
        .expect("start tls service");
    wait_until(|| {
        observed.lock().expect("tls observed lock").as_slice()
            == ["connected", "wrote:4", "pong", "closed"]
    });

    let terminal = app
        .shutdown()
        .drain()
        .join()
        .expect("local system shutdown");
    server.join().expect("server thread");
    for expected in [
        CallKind::TlsConnect,
        CallKind::TlsWrite,
        CallKind::TlsRead,
        CallKind::TlsClose,
    ] {
        assert!(
            terminal.trace().iter().any(|event| {
                matches!(
                    event.kind(),
                    RuntimeEventKind::CallCompleted { call_kind, .. } if call_kind == expected
                )
            }),
            "missing {expected:?} completion in trace: {:?}",
            terminal.trace()
        );
    }
}

// The #16 closure: a Tina TLS *server* and a Tina TLS *client* on the same
// shard in one runtime. The old single-worker lane deadlocked both sides of one
// handshake; the on-shard sans-I/O pump interleaves them. No std socket peer,
// no second runtime — both ends are Tina.
#[test]
fn local_system_tls_client_and_server_share_one_runtime() {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let certified = rcgen::generate_simple_self_signed(vec!["localhost".to_string()])
        .expect("generate local cert");
    let cert_der = certified.cert.der().to_vec();
    let key_der = certified.key_pair.serialize_der();

    let server_seen = Arc::new(Mutex::new(Vec::new()));
    let client_seen = Arc::new(Mutex::new(Vec::new()));
    let app = LocalSystem::single_shard(AppShard(60), AppMailboxFactory)
        .trace_retention(TraceRetention::Bounded(256))
        .build();

    let server = app
        .register_root::<TlsServerService, Infallible>(
            TlsServerService {
                observed: Arc::clone(&server_seen),
                cert_der: cert_der.clone(),
                key_der,
            },
            8,
        )
        .expect("register tls server");
    app.try_send(server, TlsServerMsg::Start)
        .expect("start tls server");

    // Discover the kernel-assigned port the server bound.
    wait_until(|| {
        server_seen
            .lock()
            .expect("server seen lock")
            .iter()
            .any(|entry| entry.starts_with("bound:"))
    });
    let addr: SocketAddr = server_seen
        .lock()
        .expect("server seen lock")
        .iter()
        .find_map(|entry| entry.strip_prefix("bound:").map(str::to_string))
        .expect("bound addr")
        .parse()
        .expect("parse bound addr");

    // The Tina client connects to the Tina server in the same runtime.
    let client = app
        .register_root::<TlsClientService, Infallible>(
            TlsClientService {
                observed: Arc::clone(&client_seen),
                stream: None,
            },
            8,
        )
        .expect("register tls client");
    app.try_send(client, TlsClientMsg::Start { addr, cert_der })
        .expect("start tls client");

    wait_until(|| {
        client_seen.lock().expect("client seen lock").as_slice()
            == ["connected", "wrote:4", "pong", "closed"]
    });
    wait_until(|| {
        server_seen
            .lock()
            .expect("server seen lock")
            .iter()
            .any(|entry| entry == "closed")
    });

    let terminal = app.shutdown().drain().join().expect("clean shutdown");
    // The pump serializes its own TCP ops, so an internal write-during-read
    // (handshake flights, alerts) never collides with the rail's one-pending-op
    // rule: no TLS call fails with ResourceBusy.
    assert!(
        !terminal.trace().iter().any(|event| matches!(
            event.kind(),
            RuntimeEventKind::CallFailed {
                reason: CallError::ResourceBusy,
                ..
            }
        )),
        "no TLS op should fail with ResourceBusy on the same-runtime path"
    );
}

// A TLS client that reads until the peer hangs up, recording whether the
// connection ended with a clean `close_notify` (an empty read) or an abrupt
// truncation (a typed error) — the two must stay distinct.
#[derive(Debug, Clone)]
enum TlsProbeMsg {
    Start { addr: SocketAddr, cert_der: Vec<u8> },
    Connected(Result<TlsStreamId, CallError>),
    Read(Result<Vec<u8>, CallError>),
}

#[derive(Debug)]
struct TlsProbeClient {
    terminal: Arc<Mutex<Option<String>>>,
    stream: Option<TlsStreamId>,
}

#[tina_runtime::isolate(message = TlsProbeMsg, shard = AppShard)]
impl TlsProbeClient {
    fn handle(
        &mut self,
        msg: TlsProbeMsg,
        _ctx: &mut Context<'_, AppShard, Self::Reply>,
    ) -> Effect<Self> {
        match msg {
            TlsProbeMsg::Start { addr, cert_der } => {
                tls_connect(addr, "localhost", vec![cert_der], Duration::from_secs(5))
                    .then(TlsProbeMsg::Connected)
            }
            TlsProbeMsg::Connected(Ok(stream)) => {
                self.stream = Some(stream);
                tls_read(stream, 64, Duration::from_secs(5)).then(TlsProbeMsg::Read)
            }
            TlsProbeMsg::Connected(Err(error)) => {
                *self.terminal.lock().expect("probe lock") = Some(format!("connect-err:{error:?}"));
                noop()
            }
            TlsProbeMsg::Read(Ok(bytes)) if bytes.is_empty() => {
                // Empty read == clean close_notify.
                *self.terminal.lock().expect("probe lock") = Some("clean-eof".to_string());
                noop()
            }
            TlsProbeMsg::Read(Ok(_)) => {
                // Got application data; read again to reach the close.
                let stream = self.stream.expect("probe stream");
                tls_read(stream, 64, Duration::from_secs(5)).then(TlsProbeMsg::Read)
            }
            TlsProbeMsg::Read(Err(error)) => {
                *self.terminal.lock().expect("probe lock") = Some(format!("read-err:{error:?}"));
                noop()
            }
        }
    }
}

/// Spawn a one-shot rustls server that writes `hello`, then closes the way
/// `clean` selects: a clean `close_notify` alert, or an abrupt TCP teardown
/// (truncation) with no alert.
fn spawn_probe_peer(
    cert_der: Vec<u8>,
    key_der: Vec<u8>,
    clean: bool,
) -> (SocketAddr, std::thread::JoinHandle<()>) {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let server_cert = rustls::pki_types::CertificateDer::from(cert_der);
    let server_key = rustls::pki_types::PrivateKeyDer::Pkcs8(
        rustls::pki_types::PrivatePkcs8KeyDer::from(key_der),
    );
    let config = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(vec![server_cert], server_key)
        .expect("probe server config");
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind probe listener");
    let addr = listener.local_addr().expect("probe local addr");
    let handle = thread::spawn(move || {
        let (tcp, _) = listener.accept().expect("probe accept");
        let connection =
            rustls::ServerConnection::new(Arc::new(config)).expect("probe server connection");
        let mut stream = rustls::StreamOwned::new(connection, tcp);
        stream.write_all(b"hello").expect("probe write");
        stream.flush().expect("probe flush");
        if clean {
            // Clean close: send the close_notify alert before dropping.
            stream.conn.send_close_notify();
            let _ = stream.flush();
        } else {
            // Truncation: tear the TCP connection down with no close_notify.
            let _ = stream.sock.shutdown(std::net::Shutdown::Both);
        }
        drop(stream);
    });
    (addr, handle)
}

fn run_probe(clean: bool) -> String {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let certified = rcgen::generate_simple_self_signed(vec!["localhost".to_string()])
        .expect("generate local cert");
    let cert_der = certified.cert.der().to_vec();
    let key_der = certified.key_pair.serialize_der();
    let (addr, peer) = spawn_probe_peer(cert_der.clone(), key_der, clean);

    let terminal = Arc::new(Mutex::new(None));
    let app = LocalSystem::single_shard(AppShard(61), AppMailboxFactory)
        .trace_retention(TraceRetention::Bounded(128))
        .build();
    let probe = app
        .register_root::<TlsProbeClient, Infallible>(
            TlsProbeClient {
                terminal: Arc::clone(&terminal),
                stream: None,
            },
            8,
        )
        .expect("register probe client");
    app.try_send(probe, TlsProbeMsg::Start { addr, cert_der })
        .expect("start probe");
    wait_until(|| terminal.lock().expect("probe lock").is_some());
    let outcome = terminal
        .lock()
        .expect("probe lock")
        .clone()
        .expect("probe terminal");
    app.shutdown().drain().join().expect("probe shutdown");
    peer.join().expect("probe peer thread");
    outcome
}

#[test]
fn local_system_tls_clean_close_notify_reads_empty() {
    assert_eq!(run_probe(true), "clean-eof");
}

#[test]
fn local_system_tls_truncation_is_a_typed_error_not_a_clean_eof() {
    let outcome = run_probe(false);
    assert!(
        outcome.starts_with("read-err:"),
        "truncation must surface as a typed read error, got {outcome:?}"
    );
}

#[test]
fn local_system_tls_server_accepts_reads_writes_and_closes() {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let certified = rcgen::generate_simple_self_signed(vec!["localhost".to_string()])
        .expect("generate local cert");
    let cert_der = certified.cert.der().to_vec();
    let key_der = certified.key_pair.serialize_der();

    let observed = Arc::new(Mutex::new(Vec::new()));
    let app = LocalSystem::single_shard(AppShard(43), AppMailboxFactory)
        .trace_retention(TraceRetention::Bounded(128))
        .build();
    let address = app
        .register_root::<TlsServerService, Infallible>(
            TlsServerService {
                observed: Arc::clone(&observed),
                cert_der: cert_der.clone(),
                key_der,
            },
            8,
        )
        .expect("register tls server service");

    app.try_send(address, TlsServerMsg::Start)
        .expect("start tls server service");
    wait_until(|| {
        observed
            .lock()
            .expect("tls server observed lock")
            .iter()
            .any(|entry| entry.starts_with("bound:"))
    });
    let addr: SocketAddr = observed
        .lock()
        .expect("tls server observed lock")
        .iter()
        .find_map(|entry| entry.strip_prefix("bound:").map(str::to_string))
        .expect("bound addr")
        .parse()
        .expect("parse bound addr");
    wait_until(|| {
        app.topology()
            .shard(ShardId::new(43))
            .expect("tls server shard")
            .owned_resource_count()
            >= 1
    });

    let client = thread::spawn(move || {
        let tcp = TcpStream::connect(addr).expect("connect tls server");
        let mut roots = rustls::RootCertStore::empty();
        roots
            .add(rustls::pki_types::CertificateDer::from(cert_der))
            .expect("add root cert");
        let config = rustls::ClientConfig::builder()
            .with_root_certificates(roots)
            .with_no_client_auth();
        let server_name =
            rustls::pki_types::ServerName::try_from("localhost").expect("server name");
        let connection =
            rustls::ClientConnection::new(Arc::new(config), server_name).expect("client conn");
        let mut stream = rustls::StreamOwned::new(connection, tcp);
        while stream.conn.is_handshaking() {
            stream
                .conn
                .complete_io(&mut stream.sock)
                .expect("client handshake");
        }
        stream.write_all(b"ping").expect("write ping");
        stream.flush().expect("flush ping");
        let mut response = [0_u8; 4];
        stream.read_exact(&mut response).expect("read pong");
        assert_eq!(&response, b"pong");
    });

    wait_until(|| {
        let entries = observed.lock().expect("tls server observed lock");
        entries.len() == 3
            && entries[0] == format!("bound:{addr}")
            && entries[1].starts_with("accepted:")
            && entries[2] == "closed"
    });

    client.join().expect("tls client thread");
    let terminal = app
        .shutdown()
        .drain()
        .join()
        .expect("local system shutdown");
    for expected in [
        CallKind::TlsBind,
        CallKind::TlsAccept,
        CallKind::TlsRead,
        CallKind::TlsWrite,
        CallKind::TlsClose,
        CallKind::TlsListenerClose,
    ] {
        assert!(
            terminal.trace().iter().any(|event| {
                matches!(
                    event.kind(),
                    RuntimeEventKind::CallCompleted { call_kind, .. } if call_kind == expected
                )
            }),
            "missing {expected:?} completion in trace: {:?}",
            terminal.trace()
        );
    }
}

#[test]
fn local_system_tls_quiet_stream_does_not_block_second_connection() {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let certified = rcgen::generate_simple_self_signed(vec!["localhost".to_string()])
        .expect("generate local cert");
    let cert_der = certified.cert.der().to_vec();
    let key_der = certified.key_pair.serialize_der();

    let observed = Arc::new(Mutex::new(Vec::new()));
    let config = LocalSystemConfig {
        tls_lane_capacity: 4,
        ..LocalSystemConfig::default()
    };
    let app = LocalSystem::single_shard(AppShard(430), AppMailboxFactory)
        .config(config)
        .trace_retention(TraceRetention::Bounded(256))
        .build();
    let address = app
        .register_root::<MultiTlsServerService, Infallible>(
            MultiTlsServerService {
                observed: Arc::clone(&observed),
                cert_der: cert_der.clone(),
                key_der,
                accepts: 0,
            },
            16,
        )
        .expect("register multi tls server service");

    app.try_send(address, MultiTlsServerMsg::Start)
        .expect("start multi tls server");
    wait_until(|| {
        observed
            .lock()
            .expect("multi tls observed lock")
            .iter()
            .any(|entry| entry.starts_with("bound:"))
    });
    let addr: SocketAddr = observed
        .lock()
        .expect("multi tls observed lock")
        .iter()
        .find_map(|entry| entry.strip_prefix("bound:").map(str::to_string))
        .expect("bound addr")
        .parse()
        .expect("parse bound addr");

    let (release_slow, slow_release) = mpsc::channel();
    let slow_cert = cert_der.clone();
    let slow = thread::spawn(move || {
        let _stream = connect_rustls_client(addr, slow_cert);
        let _ = slow_release.recv_timeout(Duration::from_secs(5));
    });
    wait_until(|| {
        observed
            .lock()
            .expect("multi tls observed lock")
            .iter()
            .any(|entry| entry.starts_with("accepted:1:"))
    });

    let (done, finished) = mpsc::channel();
    let fast = thread::spawn(move || {
        let mut stream = connect_rustls_client(addr, cert_der);
        stream.write_all(b"ping").expect("write fast ping");
        stream.flush().expect("flush fast ping");
        let mut response = [0_u8; 4];
        stream.read_exact(&mut response).expect("read fast pong");
        done.send(response).expect("send fast result");
    });
    let response = finished
        .recv_timeout(Duration::from_millis(750))
        .expect("second TLS client must not wait behind quiet first stream");
    assert_eq!(&response, b"pong");

    release_slow.send(()).expect("release slow client");
    slow.join().expect("slow tls client");
    fast.join().expect("fast tls client");
    let terminal = app
        .shutdown()
        .drain()
        .join()
        .expect("local system shutdown");
    assert!(
        terminal
            .trace()
            .iter()
            .filter(|event| {
                matches!(
                    event.kind(),
                    RuntimeEventKind::CallCompleted {
                        call_kind: CallKind::TlsAccept,
                        ..
                    }
                )
            })
            .count()
            >= 2,
        "second TLS accept must complete; trace: {:?}",
        terminal.trace()
    );
    assert!(
        terminal.trace().iter().any(|event| {
            matches!(
                event.kind(),
                RuntimeEventKind::CallCompleted {
                    call_kind: CallKind::TlsRead,
                    ..
                }
            )
        }),
        "fast TLS read must complete; trace: {:?}",
        terminal.trace()
    );
}

#[test]
fn local_system_tls_bind_rejects_invalid_key_without_leaking_resources() {
    let certified = rcgen::generate_simple_self_signed(vec!["localhost".to_string()])
        .expect("generate local cert");
    let cert_der = certified.cert.der().to_vec();

    let observed = Arc::new(Mutex::new(Vec::new()));
    let app = LocalSystem::single_shard(AppShard(44), AppMailboxFactory)
        .trace_retention(TraceRetention::Bounded(128))
        .build();
    let address = app
        .register_root::<TlsServerService, Infallible>(
            TlsServerService {
                observed: Arc::clone(&observed),
                cert_der,
                key_der: b"not-a-private-key".to_vec(),
            },
            8,
        )
        .expect("register tls server service");

    app.try_send(address, TlsServerMsg::Start)
        .expect("start tls server service");
    wait_until(|| {
        observed
            .lock()
            .expect("tls invalid key observed lock")
            .iter()
            .any(|entry| entry == "failed:TlsCertificate")
    });
    assert_eq!(
        app.topology()
            .shard(ShardId::new(44))
            .expect("invalid key shard")
            .owned_resource_count(),
        0
    );

    let terminal = app.shutdown().drain().join().expect("invalid key shutdown");
    assert_eq!(
        terminal.shutdown_report().remaining_owned_resource_count(),
        0
    );
    assert!(terminal.trace().iter().any(|event| {
        matches!(
            event.kind(),
            RuntimeEventKind::CallFailed {
                call_kind: CallKind::TlsBind,
                reason: CallError::TlsCertificate,
                ..
            }
        )
    }));
}

#[test]
fn local_system_tls_failed_handshake_closes_listener_and_leaks_no_stream() {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let certified = rcgen::generate_simple_self_signed(vec!["localhost".to_string()])
        .expect("generate local cert");
    let cert_der = certified.cert.der().to_vec();
    let key_der = certified.key_pair.serialize_der();

    let observed = Arc::new(Mutex::new(Vec::new()));
    let app = LocalSystem::single_shard(AppShard(45), AppMailboxFactory)
        .trace_retention(TraceRetention::Bounded(128))
        .build();
    let address = app
        .register_root::<TlsServerService, Infallible>(
            TlsServerService {
                observed: Arc::clone(&observed),
                cert_der,
                key_der,
            },
            8,
        )
        .expect("register tls server service");

    app.try_send(address, TlsServerMsg::Start)
        .expect("start tls server service");
    wait_until(|| {
        observed
            .lock()
            .expect("tls failed handshake observed lock")
            .iter()
            .any(|entry| entry.starts_with("bound:"))
    });
    let addr: SocketAddr = observed
        .lock()
        .expect("tls failed handshake observed lock")
        .iter()
        .find_map(|entry| entry.strip_prefix("bound:").map(str::to_string))
        .expect("bound addr")
        .parse()
        .expect("parse bound addr");

    let mut raw = TcpStream::connect(addr).expect("connect raw tcp to tls listener");
    raw.write_all(b"not tls").expect("write invalid tls bytes");
    raw.shutdown(Shutdown::Both).expect("shutdown raw tcp");

    wait_until(|| {
        let entries = observed.lock().expect("tls failed handshake observed lock");
        entries
            .iter()
            .any(|entry| entry == "failed:TlsHandshake" || entry == "failed:Timeout")
            && entries.iter().any(|entry| entry == "closed")
    });
    wait_until(|| {
        app.topology()
            .shard(ShardId::new(45))
            .expect("failed handshake shard")
            .owned_resource_count()
            == 0
    });

    let terminal = app
        .shutdown()
        .drain()
        .join()
        .expect("failed handshake shutdown");
    assert_eq!(
        terminal.shutdown_report().remaining_owned_resource_count(),
        0
    );
    assert!(terminal.trace().iter().any(|event| {
        matches!(
            event.kind(),
            RuntimeEventKind::CallFailed {
                call_kind: CallKind::TlsAccept,
                reason: CallError::TlsHandshake | CallError::Timeout,
                ..
            }
        )
    }));
}

#[test]
fn local_system_shutdown_reports_in_flight_tls_handshake_resource() {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let certified = rcgen::generate_simple_self_signed(vec!["localhost".to_string()])
        .expect("generate local cert");
    let cert_der = certified.cert.der().to_vec();
    let key_der = certified.key_pair.serialize_der();

    let observed = Arc::new(Mutex::new(Vec::new()));
    let app = LocalSystem::single_shard(AppShard(46), AppMailboxFactory)
        .trace_retention(TraceRetention::Bounded(128))
        .build();
    let address = app
        .register_root::<TlsServerService, Infallible>(
            TlsServerService {
                observed: Arc::clone(&observed),
                cert_der,
                key_der,
            },
            8,
        )
        .expect("register tls server service");

    app.try_send(address, TlsServerMsg::Start)
        .expect("start tls server service");
    wait_until(|| {
        observed
            .lock()
            .expect("tls in-flight observed lock")
            .iter()
            .any(|entry| entry.starts_with("bound:"))
    });
    let addr: SocketAddr = observed
        .lock()
        .expect("tls in-flight observed lock")
        .iter()
        .find_map(|entry| entry.strip_prefix("bound:").map(str::to_string))
        .expect("bound addr")
        .parse()
        .expect("parse bound addr");
    let raw = TcpStream::connect(addr).expect("connect raw tcp to tls listener");

    // While the server is mid-handshake (raw peer never sends a ClientHello),
    // the in-flight op is visible: the accept op holds an un-tabled socket
    // (worker_held) and the handshake recv is a pending driver call.
    wait_until(|| {
        let topology = app.topology();
        let shard = topology
            .shard(ShardId::new(46))
            .expect("tls in-flight shard");
        shard.worker_held_resource_count() > 0 && shard.pending_driver_call_count() > 0
    });

    // TLS now rides the Betelgeuse rail, so a stuck handshake is cancellable
    // on the substrate — shutdown drains it cleanly to zero rather than
    // parking an unkillable worker thread (this is the #16 closure). The
    // in-flight handshake recv is released by the substrate's pending-
    // completion cancellation.
    let terminal = app.shutdown().drain().join_report();
    assert!(
        terminal.shutdown_report().clean(),
        "in-flight TLS handshake must drain cleanly on the TCP rail at shutdown"
    );
    assert_eq!(
        terminal.shutdown_report().remaining_owned_resource_count(),
        0
    );
    assert_eq!(
        terminal
            .shutdown_report()
            .remaining_worker_held_resource_count(),
        0
    );
    assert_eq!(
        terminal
            .shutdown_report()
            .remaining_pending_driver_call_count(),
        0
    );
    drop(raw);
}

#[test]
fn local_system_tls_lane_full_is_visible_with_configured_capacity() {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let certified = rcgen::generate_simple_self_signed(vec!["localhost".to_string()])
        .expect("generate local cert");
    let cert_der = certified.cert.der().to_vec();
    let key_der = certified.key_pair.serialize_der();

    let first_seen = Arc::new(Mutex::new(Vec::new()));
    let second_seen = Arc::new(Mutex::new(Vec::new()));
    let config = LocalSystemConfig {
        tls_lane_capacity: 1,
        ..LocalSystemConfig::default()
    };
    let app = LocalSystem::single_shard(AppShard(47), AppMailboxFactory)
        .config(config)
        .trace_retention(TraceRetention::Bounded(128))
        .build();
    let first = app
        .register_root::<TlsServerService, Infallible>(
            TlsServerService {
                observed: Arc::clone(&first_seen),
                cert_der: cert_der.clone(),
                key_der: key_der.clone(),
            },
            8,
        )
        .expect("register first tls server service");
    let second = app
        .register_root::<TlsServerService, Infallible>(
            TlsServerService {
                observed: Arc::clone(&second_seen),
                cert_der,
                key_der,
            },
            8,
        )
        .expect("register second tls server service");

    app.try_send(first, TlsServerMsg::Start)
        .expect("start first tls server");
    wait_until(|| {
        first_seen
            .lock()
            .expect("first tls seen lock")
            .iter()
            .any(|entry| entry.starts_with("bound:"))
    });
    app.try_send(second, TlsServerMsg::Start)
        .expect("start second tls server");
    wait_until(|| {
        second_seen
            .lock()
            .expect("second tls seen lock")
            .iter()
            .any(|entry| entry == "failed:TlsFull")
    });

    let terminal = app.shutdown().drain().join_report();
    // `tls_bind` now completes inline (it just creates a listener), so it does
    // not consume a lane slot. `tls_lane_capacity` bounds in-flight async TLS
    // ops, so the second server's parked `tls_accept` is what hits `TlsFull`
    // while the first server's accept holds the only slot.
    assert!(terminal.trace().iter().any(|event| {
        matches!(
            event.kind(),
            RuntimeEventKind::CallFailed {
                call_kind: CallKind::TlsAccept,
                reason: CallError::TlsFull,
                ..
            }
        )
    }));
}

// On non-Unix platforms the live driver has no Unix-domain backend and
// every Unix call completes with typed `CallError::Unsupported`. This
// pins that typed answer so the capability stays named, not cfg-silent.
#[cfg(not(unix))]
mod unix_unsupported_pin {
    use super::*;
    use tina_runtime::unix_bind;

    #[derive(Debug)]
    enum UnixProbeMsg {
        Start,
        Bound(Result<(tina_runtime::UnixListenerId, PathBuf), CallError>),
    }

    #[derive(Debug)]
    struct UnixProbeService {
        observed: Arc<Mutex<Option<Result<(), CallError>>>>,
    }

    #[tina_runtime::isolate(message = UnixProbeMsg, shard = AppShard)]
    impl UnixProbeService {
        fn handle(
            &mut self,
            msg: UnixProbeMsg,
            _ctx: &mut Context<'_, AppShard, Self::Reply>,
        ) -> Effect<Self> {
            match msg {
                UnixProbeMsg::Start => {
                    unix_bind("/tmp/tina-unix-unsupported-pin.sock").then(UnixProbeMsg::Bound)
                }
                UnixProbeMsg::Bound(result) => {
                    *self.observed.lock().expect("observed lock") =
                        Some(result.map(|(_listener, _path)| ()));
                    noop()
                }
            }
        }
    }

    #[test]
    fn local_system_unix_rails_report_typed_unsupported_on_non_unix() {
        let observed = Arc::new(Mutex::new(None));
        let app = LocalSystem::single_shard(AppShard(43), AppMailboxFactory)
            .trace_retention(TraceRetention::Bounded(64))
            .build();
        let address = app
            .register_root::<UnixProbeService, Infallible>(
                UnixProbeService {
                    observed: Arc::clone(&observed),
                },
                8,
            )
            .expect("register unix probe");

        app.try_send(address, UnixProbeMsg::Start)
            .expect("start unix probe");
        wait_until(|| observed.lock().expect("observed lock").is_some());

        let result = *observed.lock().expect("observed lock");
        assert_eq!(result, Some(Err(CallError::Unsupported)));

        let _ = app.shutdown().drain().join().expect("shutdown");
    }
}

// On Unix platforms the live driver runs a real OS-backed Unix-domain
// lane. This drives a full bind/accept/connect/write/read/close echo
// round-trip — the same framed-protocol shape the simulator covers — to
// prove the live rail works, not just compiles.
#[cfg(unix)]
mod unix_live_echo {
    use super::*;
    use tina_runtime::{
        UnixAcceptReply, UnixBindReply, UnixConnectReply, UnixReadReply, UnixStreamId,
        UnixWriteReply, unix_accept, unix_bind, unix_close_listener, unix_close_stream,
        unix_connect, unix_read, unix_write,
    };

    fn unique_sock(label: &str) -> PathBuf {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock after epoch")
            .as_nanos();
        std::env::temp_dir().join(format!("tina-{label}-{}-{unique}.sock", std::process::id()))
    }

    #[derive(Debug)]
    enum ServerMsg {
        Start,
        Bound(UnixBindReply),
        Accepted(UnixAcceptReply),
        Read(UnixReadReply),
        Wrote(UnixWriteReply),
        Done,
    }

    struct EchoServer {
        path: PathBuf,
        stream: Option<UnixStreamId>,
        pending: Vec<u8>,
        bound: Arc<Mutex<bool>>,
    }

    #[tina_runtime::isolate(message = ServerMsg, shard = AppShard)]
    impl EchoServer {
        fn handle(
            &mut self,
            msg: ServerMsg,
            _ctx: &mut Context<'_, AppShard, Self::Reply>,
        ) -> Effect<Self> {
            match msg {
                ServerMsg::Start => unix_bind(self.path.clone()).then(ServerMsg::Bound),
                ServerMsg::Bound(Ok((listener, _))) => {
                    *self.bound.lock().expect("bound lock") = true;
                    unix_accept(listener).then(ServerMsg::Accepted)
                }
                ServerMsg::Bound(Err(_)) => Effect::Stop,
                ServerMsg::Accepted(Ok(stream)) => {
                    self.stream = Some(stream);
                    unix_read(stream, 64).then(ServerMsg::Read)
                }
                ServerMsg::Accepted(Err(_)) => Effect::Stop,
                ServerMsg::Read(Ok(bytes)) => {
                    if bytes.is_empty() {
                        match self.stream.take() {
                            Some(stream) => unix_close_stream(stream).then(|_| ServerMsg::Done),
                            None => Effect::Stop,
                        }
                    } else {
                        self.pending = bytes;
                        let stream = self.stream.expect("stream");
                        unix_write(stream, self.pending.clone()).then(ServerMsg::Wrote)
                    }
                }
                ServerMsg::Read(Err(_)) => Effect::Stop,
                ServerMsg::Wrote(Ok(count)) => {
                    let drained = count.min(self.pending.len());
                    self.pending.drain(..drained);
                    let stream = self.stream.expect("stream");
                    if self.pending.is_empty() {
                        unix_read(stream, 64).then(ServerMsg::Read)
                    } else {
                        unix_write(stream, self.pending.clone()).then(ServerMsg::Wrote)
                    }
                }
                ServerMsg::Wrote(Err(_)) => Effect::Stop,
                ServerMsg::Done => Effect::Stop,
            }
        }
    }

    #[derive(Debug)]
    enum ClientMsg {
        Start,
        Connected(UnixConnectReply),
        Wrote(UnixWriteReply),
        Read(UnixReadReply),
        Done,
    }

    struct EchoClient {
        path: PathBuf,
        stream: Option<UnixStreamId>,
        pending: Vec<u8>,
        received: Arc<Mutex<Vec<u8>>>,
    }

    #[tina_runtime::isolate(message = ClientMsg, shard = AppShard)]
    impl EchoClient {
        fn handle(
            &mut self,
            msg: ClientMsg,
            _ctx: &mut Context<'_, AppShard, Self::Reply>,
        ) -> Effect<Self> {
            match msg {
                ClientMsg::Start => unix_connect(self.path.clone()).then(ClientMsg::Connected),
                ClientMsg::Connected(Ok(stream)) => {
                    self.stream = Some(stream);
                    self.pending = b"ping".to_vec();
                    unix_write(stream, self.pending.clone()).then(ClientMsg::Wrote)
                }
                ClientMsg::Connected(Err(_)) => Effect::Stop,
                ClientMsg::Wrote(Ok(count)) => {
                    let drained = count.min(self.pending.len());
                    self.pending.drain(..drained);
                    let stream = self.stream.expect("stream");
                    if self.pending.is_empty() {
                        unix_read(stream, 64).then(ClientMsg::Read)
                    } else {
                        unix_write(stream, self.pending.clone()).then(ClientMsg::Wrote)
                    }
                }
                ClientMsg::Wrote(Err(_)) => Effect::Stop,
                ClientMsg::Read(Ok(bytes)) => {
                    if !bytes.is_empty() {
                        self.received
                            .lock()
                            .expect("received lock")
                            .extend_from_slice(&bytes);
                    }
                    match self.stream.take() {
                        Some(stream) => unix_close_stream(stream).then(|_| ClientMsg::Done),
                        None => Effect::Stop,
                    }
                }
                ClientMsg::Read(Err(_)) => Effect::Stop,
                ClientMsg::Done => Effect::Stop,
            }
        }
    }

    #[test]
    fn local_system_unix_live_echo_round_trip() {
        let path = unique_sock("unix-echo");
        let _ = std::fs::remove_file(&path);
        let bound = Arc::new(Mutex::new(false));
        let received = Arc::new(Mutex::new(Vec::new()));

        let app = LocalSystem::single_shard(AppShard(44), AppMailboxFactory).build();
        let server = app
            .register_root::<EchoServer, Infallible>(
                EchoServer {
                    path: path.clone(),
                    stream: None,
                    pending: Vec::new(),
                    bound: Arc::clone(&bound),
                },
                8,
            )
            .expect("register echo server");
        app.try_send(server, ServerMsg::Start)
            .expect("start server");

        // Wait for the listener to be bound before connecting so the
        // client does not race ahead of bind (live connect to an unbound
        // path is ENOENT).
        wait_until(|| *bound.lock().expect("bound lock"));

        let client = app
            .register_root::<EchoClient, Infallible>(
                EchoClient {
                    path: path.clone(),
                    stream: None,
                    pending: Vec::new(),
                    received: Arc::clone(&received),
                },
                8,
            )
            .expect("register echo client");
        app.try_send(client, ClientMsg::Start)
            .expect("start client");

        wait_until(|| received.lock().expect("received lock").as_slice() == b"ping");

        let _ = app.shutdown().drain().join().expect("shutdown");
        let _ = std::fs::remove_file(&path);
        assert_eq!(received.lock().expect("received lock").as_slice(), b"ping");
    }

    #[derive(Debug)]
    enum BindProbeMsg {
        Start,
        Bound(UnixBindReply),
    }

    struct BindProbe {
        path: PathBuf,
        observed: Arc<Mutex<Option<UnixBindReply>>>,
    }

    #[tina_runtime::isolate(message = BindProbeMsg, shard = AppShard)]
    impl BindProbe {
        fn handle(
            &mut self,
            msg: BindProbeMsg,
            _ctx: &mut Context<'_, AppShard, Self::Reply>,
        ) -> Effect<Self> {
            match msg {
                BindProbeMsg::Start => unix_bind(self.path.clone()).then(BindProbeMsg::Bound),
                BindProbeMsg::Bound(result) => {
                    *self.observed.lock().expect("observed lock") = Some(result);
                    Effect::Stop
                }
            }
        }
    }

    #[test]
    fn local_system_unix_bind_does_not_remove_regular_file() {
        let path = unique_sock("ur");
        let _ = std::fs::remove_file(&path);
        std::fs::write(&path, b"keep me").expect("write regular file");
        let observed = Arc::new(Mutex::new(None));

        let app = LocalSystem::single_shard(AppShard(45), AppMailboxFactory).build();
        let address = app
            .register_root::<BindProbe, Infallible>(
                BindProbe {
                    path: path.clone(),
                    observed: Arc::clone(&observed),
                },
                8,
            )
            .expect("register bind probe");
        app.try_send(address, BindProbeMsg::Start)
            .expect("start bind probe");

        wait_until(|| observed.lock().expect("observed lock").is_some());
        let result = observed.lock().expect("observed lock").take().unwrap();
        assert!(matches!(result, Err(CallError::Io)), "{result:?}");
        assert_eq!(
            std::fs::read(&path).expect("regular file still exists"),
            b"keep me"
        );
        let _ = app.shutdown().drain().join().expect("shutdown");
        let _ = std::fs::remove_file(&path);
    }

    #[derive(Debug)]
    enum CloseProbeMsg {
        Start,
        Bound(UnixBindReply),
        Close,
        Closed,
    }

    struct CloseProbe {
        path: PathBuf,
        listener: Option<tina_runtime::UnixListenerId>,
        bound: Arc<Mutex<bool>>,
        closed: Arc<Mutex<bool>>,
    }

    #[tina_runtime::isolate(message = CloseProbeMsg, shard = AppShard)]
    impl CloseProbe {
        fn handle(
            &mut self,
            msg: CloseProbeMsg,
            _ctx: &mut Context<'_, AppShard, Self::Reply>,
        ) -> Effect<Self> {
            match msg {
                CloseProbeMsg::Start => unix_bind(self.path.clone()).then(CloseProbeMsg::Bound),
                CloseProbeMsg::Bound(Ok((listener, _))) => {
                    self.listener = Some(listener);
                    *self.bound.lock().expect("bound lock") = true;
                    noop()
                }
                CloseProbeMsg::Bound(Err(_)) => Effect::Stop,
                CloseProbeMsg::Close => match self.listener.take() {
                    Some(listener) => unix_close_listener(listener).then(|_| CloseProbeMsg::Closed),
                    None => Effect::Stop,
                },
                CloseProbeMsg::Closed => {
                    *self.closed.lock().expect("closed lock") = true;
                    Effect::Stop
                }
            }
        }
    }

    #[test]
    fn local_system_unix_close_does_not_remove_replaced_regular_file() {
        let path = unique_sock("ucr");
        let _ = std::fs::remove_file(&path);
        let bound = Arc::new(Mutex::new(false));
        let closed = Arc::new(Mutex::new(false));

        let app = LocalSystem::single_shard(AppShard(46), AppMailboxFactory).build();
        let address = app
            .register_root::<CloseProbe, Infallible>(
                CloseProbe {
                    path: path.clone(),
                    listener: None,
                    bound: Arc::clone(&bound),
                    closed: Arc::clone(&closed),
                },
                8,
            )
            .expect("register close probe");
        app.try_send(address, CloseProbeMsg::Start)
            .expect("start close probe");
        wait_until(|| *bound.lock().expect("bound lock"));

        std::fs::remove_file(&path).expect("remove socket path");
        std::fs::write(&path, b"replacement").expect("write replacement file");
        app.try_send(address, CloseProbeMsg::Close)
            .expect("close listener");
        wait_until(|| *closed.lock().expect("closed lock"));

        assert_eq!(
            std::fs::read(&path).expect("replacement file still exists"),
            b"replacement"
        );
        let _ = app.shutdown().drain().join().expect("shutdown");
        let _ = std::fs::remove_file(&path);
    }
}

#[test]
fn local_system_shutdown_signal_rail_delivers_message_before_stop() {
    let observed = Arc::new(Mutex::new(Vec::new()));
    let app = LocalSystem::single_shard(AppShard(41), AppMailboxFactory)
        .trace_retention(TraceRetention::Bounded(64))
        .build();
    let address = app
        .register_root::<SignalUnsupportedService, Infallible>(
            SignalUnsupportedService {
                observed: Arc::clone(&observed),
            },
            8,
        )
        .expect("register signal service");

    app.try_send(address, SignalUnsupportedMsg::Start)
        .expect("start signal service");
    wait_until(|| {
        let trace = app.trace();
        assert!(trace.is_complete());
        trace.events().iter().any(|event| {
            matches!(
                event.kind(),
                RuntimeEventKind::CallDispatchAttempted {
                    call_kind: CallKind::SignalWait,
                    ..
                }
            )
        })
    });

    let terminal = app
        .shutdown()
        .drain()
        .join()
        .expect("local system shutdown");
    assert!(
        observed
            .lock()
            .expect("signal observed lock")
            .iter()
            .any(|entry| entry == "signal:shutdown")
    );
    assert!(terminal.trace().iter().any(|event| {
        matches!(
            event.kind(),
            RuntimeEventKind::CallCompleted {
                call_kind: CallKind::SignalWait,
                ..
            }
        )
    }));
}

#[cfg(unix)]
#[test]
fn local_system_process_run_captures_truncates_and_times_out() {
    let observed = Arc::new(Mutex::new(Vec::new()));
    let app = LocalSystem::single_shard(AppShard(40), AppMailboxFactory)
        .trace_retention(TraceRetention::Bounded(128))
        .build();
    let address = app
        .register_root::<ProcessService, Infallible>(
            ProcessService {
                observed: Arc::clone(&observed),
            },
            16,
        )
        .expect("register process service");

    app.try_send(address, ProcessMsg::Echo)
        .expect("start process echo");
    wait_until_debug(
        || {
            observed
                .lock()
                .expect("process observed lock")
                .iter()
                .any(|entry| entry == "exit:Some(0):llam:true")
        },
        || {
            format!(
                "observed={:?}",
                observed.lock().expect("process observed lock")
            )
        },
    );

    app.try_send(address, ProcessMsg::Sleep)
        .expect("start process sleep");
    wait_until_debug(
        || {
            observed
                .lock()
                .expect("process observed lock")
                .iter()
                .any(|entry| entry == "err:Timeout")
        },
        || {
            format!(
                "observed={:?}",
                observed.lock().expect("process observed lock")
            )
        },
    );

    app.try_send(address, ProcessMsg::GrandchildStdout)
        .expect("start process grandchild stdout inheritance");
    wait_until_debug(
        || {
            observed
                .lock()
                .expect("process observed lock")
                .iter()
                .any(|entry| entry == "exit:Some(0)::true")
        },
        || {
            format!(
                "observed={:?}",
                observed.lock().expect("process observed lock")
            )
        },
    );

    let terminal = app
        .shutdown()
        .drain()
        .join()
        .expect("local system shutdown");
    assert!(terminal.trace().iter().any(|event| {
        matches!(
            event.kind(),
            RuntimeEventKind::CallCompleted {
                call_kind: CallKind::ProcessRun,
                ..
            }
        )
    }));
    assert!(terminal.trace().iter().any(|event| {
        matches!(
            event.kind(),
            RuntimeEventKind::CallFailed {
                call_kind: CallKind::ProcessRun,
                reason: CallError::Timeout,
                ..
            }
        )
    }));
}

#[cfg(unix)]
#[test]
fn local_system_composed_io_service_udp_process_and_journal_commit() {
    let observed = Arc::new(Mutex::new(Vec::new()));
    let app = LocalSystem::single_shard(AppShard(41), AppMailboxFactory)
        .storage_lane_capacity(4)
        .trace_retention(TraceRetention::Bounded(256))
        .build();
    let journal_path = unique_dir("composed-io").with_extension("journal");
    let _ = fs::remove_file(&journal_path);
    let address = app
        .register_root::<ComposedIoService, Infallible>(
            ComposedIoService {
                journal: None,
                receiver: None,
                sender: None,
                receiver_addr: None,
                udp_payload: None,
                closed: 0,
                observed: Arc::clone(&observed),
            },
            32,
        )
        .expect("register composed io service");

    app.try_send(address, ComposedIoMsg::Start(journal_path.clone()))
        .expect("start composed io service");
    wait_until(|| {
        observed
            .lock()
            .expect("composed observed lock")
            .iter()
            .any(|entry| entry == "committed")
    });

    let terminal = app
        .shutdown()
        .drain()
        .join()
        .expect("local system shutdown");
    for expected in [
        CallKind::UdpBind,
        CallKind::UdpSendTo,
        CallKind::UdpRecvFrom,
        CallKind::ProcessRun,
        CallKind::JournalAppend,
    ] {
        assert!(terminal.trace().iter().any(|event| {
            matches!(
                event.kind(),
                RuntimeEventKind::CallCompleted { call_kind, .. } if call_kind == expected
            )
        }));
    }

    let replay =
        tina_runtime::persistence::replay_journal(&journal_path).expect("composed journal replays");
    assert_eq!(replay.records.len(), 1);
    assert_eq!(replay.records[0].index, 1);
    assert_eq!(replay.records[0].bytes, b"llamafed\n".to_vec());
}

#[test]
fn local_system_udp_loopback_surfaces_send_recv_truncation_and_close() {
    let observed = Arc::new(Mutex::new(Vec::new()));
    let app = LocalSystem::single_shard(AppShard(38), AppMailboxFactory)
        .trace_retention(TraceRetention::Bounded(128))
        .build();
    let address = app
        .register_root::<UdpLoopbackService, Infallible>(
            UdpLoopbackService {
                receiver: None,
                sender: None,
                receiver_addr: None,
                observed: Arc::clone(&observed),
            },
            16,
        )
        .expect("register udp service");

    app.try_send(address, UdpLoopbackMsg::Start)
        .expect("start udp service");
    wait_until(|| {
        let observed = observed.lock().expect("udp observed lock");
        observed.iter().any(|entry| entry == "sent:6")
            && observed.iter().any(|entry| entry == "recv:llam:true")
            && observed
                .iter()
                .filter(|entry| entry.as_str() == "closed")
                .count()
                == 2
    });

    let terminal = app
        .shutdown()
        .drain()
        .join()
        .expect("local system shutdown");
    for expected in [
        CallKind::UdpBind,
        CallKind::UdpSendTo,
        CallKind::UdpRecvFrom,
        CallKind::UdpSocketClose,
    ] {
        assert!(terminal.trace().iter().any(|event| {
            matches!(
                event.kind(),
                RuntimeEventKind::CallCompleted { call_kind, .. } if call_kind == expected
            )
        }));
    }
}

#[test]
fn live_ingress_pressure_reports_capacity_and_full_counter() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let entered = Arc::new((Mutex::new(false), Condvar::new()));
    let release = Arc::new((Mutex::new(false), Condvar::new()));
    let app = LocalSystem::single_shard(AppShard(36), AppMailboxFactory)
        .ingress_capacity(1)
        .trace_retention(TraceRetention::Bounded(64))
        .build();
    let address = app
        .register_root::<BlockingService, Infallible>(
            BlockingService {
                seen: Arc::clone(&seen),
            },
            8,
        )
        .expect("register blocking root");

    app.try_send(
        address,
        BlockingMsg::Hold(Arc::clone(&entered), Arc::clone(&release)),
    )
    .expect("hold handoff");
    wait_for_flag(&entered);
    app.try_send(address, BlockingMsg::Note(1))
        .expect("first queued handoff fills ingress");
    assert_eq!(
        app.try_send(address, BlockingMsg::Note(2)),
        Err(ThreadedTrySendError::IngressFull)
    );

    let pressure = app.topology();
    let shard = pressure
        .shard(ShardId::new(36))
        .expect("pressure shard report");
    assert_eq!(shard.ingress().capacity(), 1);
    assert_eq!(shard.ingress().depth(), None);
    assert_eq!(shard.ingress().rejected_full(), Some(1));
    assert_eq!(shard.ingress().rejected_closed(), Some(0));
    assert!(shard.ingress().accepted().expect("accepted measured") >= 2);

    release_flag(&release);
    wait_until(|| seen.lock().expect("blocking seen lock").as_slice() == [1]);
    let terminal = app.shutdown().drain().join().expect("shutdown app");
    assert_eq!(terminal.state(), LocalSystemState::Closed);
}

#[test]
fn local_system_multi_shard_uses_same_entry_name_for_topology() {
    let left_seen = Arc::new(Mutex::new(Vec::new()));
    let right_seen = Arc::new(Mutex::new(Vec::new()));
    let app = LocalSystem::<AppShard, AppMailboxFactory>::multi_shard(AppMailboxFactory)
        .shard(AppShard(41))
        .shard(AppShard(42))
        .ingress_capacity(8)
        .shard_pair_capacity(8)
        .trace_retention(TraceRetention::Bounded(128))
        .build();

    let left = app
        .register_root_on::<LlamaService, Infallible>(
            ShardId::new(41),
            LlamaService {
                seen: Arc::clone(&left_seen),
            },
            8,
        )
        .expect("register left root");
    let right = app
        .register_root_on::<LlamaService, Infallible>(
            ShardId::new(42),
            LlamaService {
                seen: Arc::clone(&right_seen),
            },
            8,
        )
        .expect("register right root");

    app.try_send(left, LlamaMsg::Feed(1))
        .expect("left bounded handoff");
    app.try_send(right, LlamaMsg::Feed(2))
        .expect("right bounded handoff");

    wait_until(|| left_seen.lock().expect("left lock").as_slice() == [1]);
    wait_until(|| right_seen.lock().expect("right lock").as_slice() == [2]);

    let terminal = app
        .shutdown()
        .drain()
        .join()
        .expect("multi-shard local system shutdown");
    assert_eq!(terminal.state(), LocalSystemState::Closed);
    assert!(terminal.trace().len() >= 2);
}

#[test]
fn local_system_config_manifest_rejects_zero_capacities_before_start() {
    for (config, expected) in [
        (
            LocalSystemConfig {
                ingress_capacity: 0,
                ..LocalSystemConfig::default()
            },
            LocalSystemConfigError::ZeroIngressCapacity,
        ),
        (
            LocalSystemConfig {
                shard_pair_capacity: 0,
                ..LocalSystemConfig::default()
            },
            LocalSystemConfigError::ZeroShardPairCapacity,
        ),
        (
            LocalSystemConfig {
                remote_inbound_drain_budget: 0,
                ..LocalSystemConfig::default()
            },
            LocalSystemConfigError::ZeroRemoteInboundDrainBudget,
        ),
        (
            LocalSystemConfig {
                storage_lane_capacity: 0,
                ..LocalSystemConfig::default()
            },
            LocalSystemConfigError::ZeroStorageLaneCapacity,
        ),
        (
            LocalSystemConfig {
                dns_lane_capacity: 0,
                ..LocalSystemConfig::default()
            },
            LocalSystemConfigError::ZeroDnsLaneCapacity,
        ),
        (
            LocalSystemConfig {
                tls_lane_capacity: 0,
                ..LocalSystemConfig::default()
            },
            LocalSystemConfigError::ZeroTlsLaneCapacity,
        ),
        (
            LocalSystemConfig {
                process_lane_capacity: 0,
                ..LocalSystemConfig::default()
            },
            LocalSystemConfigError::ZeroProcessLaneCapacity,
        ),
        (
            LocalSystemConfig {
                signal_capacity: 0,
                ..LocalSystemConfig::default()
            },
            LocalSystemConfigError::ZeroSignalCapacity,
        ),
    ] {
        assert_eq!(config.validate(), Err(expected));
    }
}

#[test]
fn live_cross_shard_isolate_call_round_trips_reply_toer_shard() {
    let outcomes = Arc::new(Mutex::new(Vec::new()));
    let app = LocalSystem::<AppShard, AppMailboxFactory>::multi_shard(AppMailboxFactory)
        .shard(AppShard(45))
        .shard(AppShard(46))
        .ingress_capacity(8)
        .shard_pair_capacity(8)
        .trace_retention(TraceRetention::Bounded(256))
        .build();

    let worker = app
        .register_root_on::<CrossShardCallWorker, Infallible>(
            ShardId::new(46),
            CrossShardCallWorker::default(),
            8,
        )
        .expect("register cross shard worker");
    let client = app
        .register_root_on::<CrossShardCallClient, Infallible>(
            ShardId::new(45),
            CrossShardCallClient {
                outcomes: Arc::clone(&outcomes),
            },
            8,
        )
        .expect("register cross shard client");

    app.try_send(client, CrossShardCallClientMsg::Start(worker))
        .expect("start cross shard call");

    wait_until(|| {
        outcomes
            .lock()
            .expect("outcomes lock")
            .iter()
            .any(|outcome| {
                matches!(
                    outcome,
                    CallOutcome::Replied(CrossShardCallReply(bytes)) if bytes == b"llama"
                )
            })
    });

    let terminal = app
        .shutdown()
        .drain()
        .join()
        .expect("shutdown cross shard call app");
    assert_eq!(terminal.state(), LocalSystemState::Closed);
    assert!(terminal.trace().iter().any(|event| {
        event.shard() == ShardId::new(45)
            && matches!(
                event.kind(),
                RuntimeEventKind::CallCompleted {
                    call_kind: CallKind::IsolateCall,
                    ..
                }
            )
    }));
    assert!(!terminal.trace().iter().any(|event| {
        matches!(
            event.kind(),
            RuntimeEventKind::CallReplyRejected { .. }
                | RuntimeEventKind::CallFailed {
                    call_kind: CallKind::IsolateCall,
                    ..
                }
        )
    }));
}

#[test]
fn live_cross_shard_isolate_call_timeout_settles_on_requester_shard() {
    let outcomes = Arc::new(Mutex::new(Vec::new()));
    let app = LocalSystem::<AppShard, AppMailboxFactory>::multi_shard(AppMailboxFactory)
        .shard(AppShard(47))
        .shard(AppShard(48))
        .ingress_capacity(8)
        .shard_pair_capacity(8)
        .trace_retention(TraceRetention::Bounded(256))
        .build();

    let worker = app
        .register_root_on::<CrossShardCallWorker, Infallible>(
            ShardId::new(48),
            CrossShardCallWorker::default(),
            8,
        )
        .expect("register timeout worker");
    let client = app
        .register_root_on::<CrossShardCallClient, Infallible>(
            ShardId::new(47),
            CrossShardCallClient {
                outcomes: Arc::clone(&outcomes),
            },
            8,
        )
        .expect("register timeout client");

    app.try_send(client, CrossShardCallClientMsg::StartTimeout(worker))
        .expect("start cross shard timeout call");

    wait_until(|| {
        outcomes
            .lock()
            .expect("timeout outcomes lock")
            .iter()
            .any(|outcome| matches!(outcome, CallOutcome::Timeout))
    });

    let terminal = app.shutdown().drain().join().expect("shutdown timeout app");
    assert_eq!(terminal.state(), LocalSystemState::Closed);
    assert!(terminal.trace().iter().any(|event| {
        event.shard() == ShardId::new(47)
            && matches!(
                event.kind(),
                RuntimeEventKind::CallFailed {
                    call_kind: CallKind::IsolateCall,
                    reason: CallError::Timeout,
                    ..
                }
            )
    }));
}

#[test]
fn live_cross_shard_isolate_call_destination_full_returns_typed_full() {
    let outcomes = Arc::new(Mutex::new(Vec::new()));
    let app = LocalSystem::<AppShard, AppMailboxFactory>::multi_shard(AppMailboxFactory)
        .shard(AppShard(49))
        .shard(AppShard(50))
        .ingress_capacity(8)
        .shard_pair_capacity(8)
        .trace_retention(TraceRetention::Bounded(256))
        .build();

    let worker = app
        .register_root_on::<CrossShardCallWorker, Infallible>(
            ShardId::new(50),
            CrossShardCallWorker::default(),
            0,
        )
        .expect("register full worker");
    let client = app
        .register_root_on::<CrossShardCallClient, Infallible>(
            ShardId::new(49),
            CrossShardCallClient {
                outcomes: Arc::clone(&outcomes),
            },
            8,
        )
        .expect("register full client");

    app.try_send(client, CrossShardCallClientMsg::Start(worker))
        .expect("start full cross shard call");

    wait_until(|| {
        outcomes
            .lock()
            .expect("full outcomes lock")
            .iter()
            .any(|outcome| matches!(outcome, CallOutcome::Full))
    });

    let terminal = app.shutdown().drain().join().expect("shutdown full app");
    assert!(terminal.trace().iter().any(|event| {
        event.shard() == ShardId::new(49)
            && matches!(
                event.kind(),
                RuntimeEventKind::CallFailed {
                    call_kind: CallKind::IsolateCall,
                    reason: CallError::TargetFull,
                    ..
                }
            )
    }));
}

#[test]
fn live_cross_shard_isolate_call_destination_closed_returns_typed_closed() {
    let outcomes = Arc::new(Mutex::new(Vec::new()));
    let app = LocalSystem::<AppShard, AppMailboxFactory>::multi_shard(AppMailboxFactory)
        .shard(AppShard(51))
        .shard(AppShard(52))
        .ingress_capacity(8)
        .shard_pair_capacity(8)
        .trace_retention(TraceRetention::Bounded(256))
        .build();

    let worker = app
        .register_root_on::<CrossShardCallWorker, Infallible>(
            ShardId::new(52),
            CrossShardCallWorker::default(),
            8,
        )
        .expect("register closable worker");
    let client = app
        .register_root_on::<CrossShardCallClient, Infallible>(
            ShardId::new(51),
            CrossShardCallClient {
                outcomes: Arc::clone(&outcomes),
            },
            8,
        )
        .expect("register closed client");

    app.try_send(worker, CrossShardCallWorkerMsg::Stop)
        .expect("stop cross shard worker");
    wait_until(|| {
        let trace = app.trace();
        assert!(trace.is_complete());
        trace.events().iter().any(|event| {
            event.shard() == ShardId::new(52)
                && matches!(event.kind(), RuntimeEventKind::IsolateStopped)
        })
    });
    app.try_send(client, CrossShardCallClientMsg::Start(worker))
        .expect("start closed cross shard call");

    wait_until(|| {
        outcomes
            .lock()
            .expect("closed outcomes lock")
            .iter()
            .any(|outcome| matches!(outcome, CallOutcome::Closed))
    });

    let terminal = app.shutdown().drain().join().expect("shutdown closed app");
    assert!(terminal.trace().iter().any(|event| {
        event.shard() == ShardId::new(51)
            && matches!(
                event.kind(),
                RuntimeEventKind::CallFailed {
                    call_kind: CallKind::IsolateCall,
                    reason: CallError::TargetClosed,
                    ..
                }
            )
    }));
}

#[test]
fn remote_queue_pressure_reports_capacity_and_full_counter() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let entered = Arc::new((Mutex::new(false), Condvar::new()));
    let release = Arc::new((Mutex::new(false), Condvar::new()));
    let app = LocalSystem::<AppShard, AppMailboxFactory>::multi_shard(AppMailboxFactory)
        .shard(AppShard(43))
        .shard(AppShard(44))
        .ingress_capacity(8)
        .shard_pair_capacity(1)
        .trace_retention(TraceRetention::Bounded(128))
        .build();

    let target = app
        .register_root_on::<BlockingService, Infallible>(
            ShardId::new(44),
            BlockingService {
                seen: Arc::clone(&seen),
            },
            8,
        )
        .expect("register blocking target");
    let source = app
        .register_root_on::<BurstService, BlockingMsg>(ShardId::new(43), BurstService { target }, 8)
        .expect("register burst source");

    app.try_send(
        target,
        BlockingMsg::Hold(Arc::clone(&entered), Arc::clone(&release)),
    )
    .expect("hold target");
    wait_for_flag(&entered);
    app.try_send(source, BurstMsg::SendTwo)
        .expect("trigger burst");
    wait_until(|| {
        let topology = app.topology();
        topology.remote_queues().iter().any(|report| {
            report.source() == ShardId::new(43)
                && report.target() == ShardId::new(44)
                && report.queue().rejected_full() == Some(1)
        })
    });

    let topology = app.topology();
    let remote = topology
        .remote_queues()
        .iter()
        .find(|report| report.source() == ShardId::new(43) && report.target() == ShardId::new(44))
        .expect("remote queue report");
    assert_eq!(remote.queue().capacity(), 1);
    assert_eq!(remote.queue().depth(), None);
    assert_eq!(remote.queue().accepted(), Some(1));
    assert_eq!(remote.queue().rejected_full(), Some(1));
    let target_shard = topology
        .shard(ShardId::new(44))
        .expect("target shard topology");
    assert_eq!(target_shard.ingress().capacity(), 8);
    assert_eq!(target_shard.ingress().rejected_full(), Some(0));

    release_flag(&release);
    wait_until(|| seen.lock().expect("blocking seen lock").as_slice() == [1]);
    let terminal = app.shutdown().drain().join().expect("shutdown app");
    assert_eq!(terminal.state(), LocalSystemState::Closed);
    assert!(terminal.trace().iter().any(|event| {
        matches!(
            event.kind(),
            RuntimeEventKind::SendRejected {
                reason: tina_runtime::SendRejectedReason::Full,
                target_shard,
                ..
            } if target_shard == ShardId::new(44)
        )
    }));
}

// A multi-shard system whose worker thread fails must
// reject subsequent ingress to that shard's isolates, while healthy
// shards keep accepting work.
#[test]
fn failed_shard_rejects_later_ingress_to_existing_isolate() {
    let app = LocalSystem::<AppShard, CapacityPanicMailboxFactory>::multi_shard(
        CapacityPanicMailboxFactory { panic_capacity: 13 },
    )
    .shard(AppShard(81))
    .shard(AppShard(82))
    .ingress_capacity(4)
    .trace_retention(TraceRetention::Bounded(64))
    .build();

    // Register a healthy isolate on the doomed shard before the panic,
    // so we have a real address to send to once the worker dies.
    let doomed = app
        .register_root_on::<LlamaService, Infallible>(
            ShardId::new(81),
            LlamaService {
                seen: Arc::new(Mutex::new(Vec::new())),
            },
            8,
        )
        .expect("register isolate on shard 81 succeeds before panic");

    // Healthy reference shard for the post-failure check.
    let healthy_seen = Arc::new(Mutex::new(Vec::new()));
    let healthy = app
        .register_root_on::<LlamaService, Infallible>(
            ShardId::new(82),
            LlamaService {
                seen: Arc::clone(&healthy_seen),
            },
            8,
        )
        .expect("register healthy isolate");

    // Trigger a worker panic on shard 81 by registering with the
    // poisoned capacity — the mailbox factory panics inside the worker
    // thread, taking shard 81 to LiveShardState::Failed.
    assert_eq!(
        app.register_root_on::<LlamaService, Infallible>(
            ShardId::new(81),
            LlamaService {
                seen: Arc::new(Mutex::new(Vec::new())),
            },
            13,
        ),
        Err(ThreadedRuntimeError::WorkerStopped),
    );

    // Shard 81's prior isolate is now unreachable: subsequent ingress
    // must reject with WorkerStopped, not silently accept.
    let send_outcome = app.try_send(doomed, LlamaMsg::Feed(1));
    assert_eq!(send_outcome, Err(ThreadedTrySendError::WorkerStopped));

    // The healthy shard keeps accepting work.
    app.try_send(healthy, LlamaMsg::Feed(99))
        .expect("healthy shard still accepts ingress");
    wait_until(|| healthy_seen.lock().expect("healthy seen lock").as_slice() == [99]);

    let terminal = app.shutdown().drain().join_report();
    assert_eq!(terminal.state(), LocalSystemState::Failed);
    let terminal_topology = terminal.topology().expect("terminal topology");
    assert_eq!(
        terminal_topology
            .shard(ShardId::new(81))
            .expect("failed shard terminal")
            .state(),
        LiveShardState::Failed,
    );
    assert_eq!(
        terminal.shutdown_report().failed_shards(),
        &[ShardId::new(81)]
    );
    assert_eq!(
        terminal.shutdown_report().unclean_reason(),
        Some(ShutdownUncleanReason::RuntimeError(
            ThreadedRuntimeError::WorkerStopped
        )),
    );
}

#[test]
fn failed_shard_cross_shard_call_returns_one_closed_outcome() {
    let outcomes = Arc::new(Mutex::new(Vec::new()));
    let app = LocalSystem::<AppShard, CapacityPanicMailboxFactory>::multi_shard(
        CapacityPanicMailboxFactory { panic_capacity: 13 },
    )
    .shard(AppShard(83))
    .shard(AppShard(84))
    .ingress_capacity(8)
    .trace_retention(TraceRetention::Bounded(256))
    .build();

    let worker = app
        .register_root_on::<CrossShardCallWorker, Infallible>(
            ShardId::new(84),
            CrossShardCallWorker::default(),
            8,
        )
        .expect("register worker before shard failure");
    let client = app
        .register_root_on::<CrossShardCallClient, Infallible>(
            ShardId::new(83),
            CrossShardCallClient {
                outcomes: Arc::clone(&outcomes),
            },
            8,
        )
        .expect("register healthy client");

    assert_eq!(
        app.register_root_on::<LlamaService, Infallible>(
            ShardId::new(84),
            LlamaService {
                seen: Arc::new(Mutex::new(Vec::new())),
            },
            13,
        ),
        Err(ThreadedRuntimeError::WorkerStopped)
    );
    app.try_send(client, CrossShardCallClientMsg::Start(worker))
        .expect("healthy client accepts call command");

    wait_until(|| !outcomes.lock().expect("failed call outcomes").is_empty());
    let outcomes = outcomes.lock().expect("failed call outcomes").clone();
    assert_eq!(
        outcomes.len(),
        1,
        "call to failed shard must produce exactly one terminal outcome"
    );
    assert!(matches!(outcomes[0], CallOutcome::Closed));
    let terminal = app.shutdown().drain().join_report();
    assert_eq!(terminal.state(), LocalSystemState::Failed);
}

#[test]
fn timeout_before_later_shard_failure_keeps_timeout_outcome() {
    let outcomes = Arc::new(Mutex::new(Vec::new()));
    let app = LocalSystem::<AppShard, CapacityPanicMailboxFactory>::multi_shard(
        CapacityPanicMailboxFactory { panic_capacity: 13 },
    )
    .shard(AppShard(85))
    .shard(AppShard(86))
    .ingress_capacity(8)
    .trace_retention(TraceRetention::Bounded(256))
    .build();

    let worker = app
        .register_root_on::<CrossShardCallWorker, Infallible>(
            ShardId::new(86),
            CrossShardCallWorker::default(),
            8,
        )
        .expect("register no-reply worker");
    let client = app
        .register_root_on::<CrossShardCallClient, Infallible>(
            ShardId::new(85),
            CrossShardCallClient {
                outcomes: Arc::clone(&outcomes),
            },
            8,
        )
        .expect("register timeout client");

    app.try_send(client, CrossShardCallClientMsg::StartTimeout(worker))
        .expect("start timeout call");
    wait_until(|| {
        outcomes
            .lock()
            .expect("timeout outcomes")
            .iter()
            .any(|outcome| matches!(outcome, CallOutcome::Timeout))
    });

    assert_eq!(
        app.register_root_on::<LlamaService, Infallible>(
            ShardId::new(86),
            LlamaService {
                seen: Arc::new(Mutex::new(Vec::new())),
            },
            13,
        ),
        Err(ThreadedRuntimeError::WorkerStopped)
    );

    let outcomes = outcomes.lock().expect("timeout outcomes").clone();
    assert_eq!(
        outcomes.len(),
        1,
        "later shard failure must not add a second terminal outcome"
    );
    assert!(matches!(outcomes[0], CallOutcome::Timeout));
    let terminal = app.shutdown().drain().join_report();
    assert_eq!(terminal.state(), LocalSystemState::Failed);
}

#[test]
fn failed_worker_marks_one_shard_failed_and_rejects_later_work() {
    let right_seen = Arc::new(Mutex::new(Vec::new()));
    let app = LocalSystem::<AppShard, CapacityPanicMailboxFactory>::multi_shard(
        CapacityPanicMailboxFactory { panic_capacity: 13 },
    )
    .shard(AppShard(45))
    .shard(AppShard(46))
    .ingress_capacity(4)
    .trace_retention(TraceRetention::Bounded(128))
    .build();

    let right = app
        .register_root_on::<LlamaService, Infallible>(
            ShardId::new(46),
            LlamaService {
                seen: Arc::clone(&right_seen),
            },
            8,
        )
        .expect("register healthy shard");
    assert_eq!(
        app.register_root_on::<LlamaService, Infallible>(
            ShardId::new(45),
            LlamaService {
                seen: Arc::new(Mutex::new(Vec::new())),
            },
            13,
        ),
        Err(ThreadedRuntimeError::WorkerStopped)
    );

    let topology = app.topology();
    assert_eq!(
        topology
            .shard(ShardId::new(45))
            .expect("failed shard report")
            .state(),
        LiveShardState::Failed
    );
    assert_eq!(
        topology
            .shard(ShardId::new(46))
            .expect("healthy shard report")
            .state(),
        LiveShardState::Running
    );

    app.try_send(right, LlamaMsg::Feed(99))
        .expect("healthy shard still accepts ingress");
    wait_until(|| right_seen.lock().expect("right seen lock").as_slice() == [99]);
    assert_eq!(
        app.complete_trace(),
        Err(ThreadedRuntimeError::WorkerStopped)
    );
    let trace = app.trace();
    assert!(trace.is_partial());
    assert!(
        trace
            .events()
            .iter()
            .any(|event| event.shard() == ShardId::new(46)),
        "best-effort live trace should preserve events from healthy shards after one shard fails"
    );
    assert_eq!(trace.missing_shards(), &[ShardId::new(45)]);

    let terminal = app.shutdown().drain().join_report();
    assert_eq!(terminal.state(), LocalSystemState::Failed);
    assert_eq!(terminal.error(), Some(ThreadedRuntimeError::WorkerStopped));
    assert!(
        terminal
            .trace()
            .iter()
            .any(|event| event.shard() == ShardId::new(46)),
        "failed terminal report should retain partial trace from shards that shut down cleanly"
    );
    let terminal_topology = terminal.topology().expect("terminal topology");
    assert_eq!(
        terminal_topology
            .shard(ShardId::new(45))
            .expect("failed shard terminal report")
            .state(),
        LiveShardState::Failed
    );
    assert_eq!(
        terminal_topology
            .shard(ShardId::new(46))
            .expect("healthy shard terminal report")
            .state(),
        LiveShardState::Stopped
    );
}

#[test]
fn local_system_end_to_end_service_routes_cross_shard_persists_and_recovers() {
    let dir = unique_dir("local-app-e2e");
    fs::create_dir_all(&dir).expect("create e2e dir");
    let snapshot_path = dir.join("state.snapshot");
    let journal_path = dir.join("state.journal");
    let observed = Arc::new(Mutex::new(Vec::new()));

    let app = LocalSystem::<AppShard, AppMailboxFactory>::multi_shard(AppMailboxFactory)
        .shard(AppShard(71))
        .shard(AppShard(72))
        .ingress_capacity(8)
        .shard_pair_capacity(8)
        .storage_lane_capacity(4)
        .trace_retention(TraceRetention::Bounded(512))
        .build();

    let worker = app
        .register_root_on::<DurableWorker, Infallible>(
            ShardId::new(72),
            DurableWorker::new(
                snapshot_path.clone(),
                journal_path.clone(),
                Arc::clone(&observed),
            ),
            8,
        )
        .expect("register durable worker");
    let frontend = app
        .register_root_on::<Frontend, DurableWorkerMsg>(ShardId::new(71), Frontend { worker }, 8)
        .expect("register frontend");

    app.try_send(frontend, FrontendMsg::Feed("llama snacks".to_owned()))
        .expect("frontend ingress handoff");
    wait_until(|| {
        observed
            .lock()
            .expect("durable observed lock")
            .contains(&"llama snacks".to_owned())
    });

    let terminal = app.shutdown().drain().join().expect("shutdown e2e app");
    assert_eq!(terminal.state(), LocalSystemState::Closed);
    let summary = terminal.summary();
    assert_eq!(summary.journal_appended, 1);
    assert_eq!(summary.persistence_failed, 0);
    assert_eq!(summary.call_completion_rejected, 0);
    assert!(terminal.trace().iter().any(|event| {
        matches!(
            event.kind(),
            RuntimeEventKind::SendAccepted {
                target_shard,
                ..
            } if target_shard == ShardId::new(72)
        )
    }));
    assert!(terminal.trace().iter().any(|event| {
        matches!(
            event.kind(),
            RuntimeEventKind::JournalAppended { record_index: 1 }
        )
    }));

    let recovered = Arc::new(Mutex::new(Vec::new()));
    let fresh = LocalSystem::<AppShard, AppMailboxFactory>::multi_shard(AppMailboxFactory)
        .shard(AppShard(71))
        .shard(AppShard(72))
        .ingress_capacity(8)
        .shard_pair_capacity(8)
        .storage_lane_capacity(4)
        .trace_retention(TraceRetention::Bounded(512))
        .build();
    let fresh_worker = fresh
        .register_root_on::<DurableWorker, Infallible>(
            ShardId::new(72),
            DurableWorker::new(snapshot_path, journal_path, Arc::clone(&recovered)),
            8,
        )
        .expect("register fresh durable worker");
    fresh
        .try_send(fresh_worker, DurableWorkerMsg::Recover)
        .expect("recover handoff");
    wait_until(|| {
        recovered
            .lock()
            .expect("recovered observed lock")
            .contains(&"llama snacks".to_owned())
    });

    let fresh_terminal = fresh
        .shutdown()
        .drain()
        .join()
        .expect("shutdown recovered app");
    assert_eq!(fresh_terminal.state(), LocalSystemState::Closed);
    let fresh_summary = fresh_terminal.summary();
    assert_eq!(fresh_summary.recovery_finished, 2);
    assert_eq!(fresh_summary.recovery_failed, 0);
    assert!(
        fresh_terminal
            .trace()
            .iter()
            .any(|event| { matches!(event.kind(), RuntimeEventKind::RecoveryFinished) })
    );
    let _ = fs::remove_dir_all(dir);
}

#[test]
fn local_system_recovers_torn_journal_then_repairs_on_next_append() {
    // User perspective over the live Betelgeuse rail: a journal with a torn
    // tail recovers its complete prefix, and the next append repairs the tail
    // and lands — byte-for-byte the inline path, now on the reactor.
    let dir = unique_dir("local-app-torn-recover");
    fs::create_dir_all(&dir).expect("create torn recover dir");
    let snapshot_path = dir.join("state.snapshot");
    let journal_path = dir.join("state.journal");
    let good = tina_runtime::persistence::encode_journal_record(&tina_runtime::JournalRecord {
        index: 1,
        bytes: b"hay".to_vec(),
    });
    fs::write(&journal_path, [good, vec![9, 9, 9]].concat()).expect("write torn journal");

    let observed = Arc::new(Mutex::new(Vec::new()));
    let app = LocalSystem::single_shard(AppShard(77), AppMailboxFactory)
        .ingress_capacity(8)
        .storage_lane_capacity(4)
        .trace_retention(TraceRetention::Bounded(512))
        .build();
    let worker = app
        .register_root::<DurableWorker, Infallible>(
            DurableWorker::new(snapshot_path, journal_path.clone(), Arc::clone(&observed)),
            8,
        )
        .expect("register torn-recover worker");

    app.try_send(worker, DurableWorkerMsg::Recover)
        .expect("recover handoff");
    wait_until(|| {
        observed
            .lock()
            .expect("observed lock")
            .iter()
            .any(|state| state == "hay")
    });
    app.try_send(worker, DurableWorkerMsg::Feed("oats".to_owned()))
        .expect("feed handoff");
    wait_until(|| {
        observed
            .lock()
            .expect("observed lock")
            .iter()
            .any(|state| state == "hay,oats")
    });

    app.shutdown().drain().join().expect("shutdown torn app");

    let repaired =
        tina_runtime::persistence::replay_journal(&journal_path).expect("replay repaired journal");
    assert_eq!(
        repaired.warning, None,
        "the torn tail must be repaired by the next append"
    );
    assert_eq!(
        repaired
            .records
            .iter()
            .map(|record| String::from_utf8(record.bytes.clone()).expect("utf8"))
            .collect::<Vec<_>>(),
        vec!["hay", "oats"]
    );
    let _ = fs::remove_dir_all(dir);
}

#[test]
fn local_system_tcp_service_journals_before_replying_to_client() {
    let dir = unique_dir("local-app-tcp-journal");
    fs::create_dir_all(&dir).expect("create tcp journal dir");
    let journal_path = dir.join("tcp.journal");
    let published = Arc::new(Mutex::new(None));
    let observed = Arc::new(Mutex::new(Vec::new()));

    let app = LocalSystem::single_shard(AppShard(73), AppMailboxFactory)
        .ingress_capacity(8)
        .storage_lane_capacity(4)
        .trace_retention(TraceRetention::Bounded(512))
        .build();
    let service = app
        .register_root::<TcpJournalService, Infallible>(
            TcpJournalService {
                published: Arc::clone(&published),
                observed: Arc::clone(&observed),
                listener: None,
                journal_path: None,
                last_journal_index: 0,
            },
            8,
        )
        .expect("register tcp journal service");

    app.try_send(service, TcpJournalMsg::Start(journal_path.clone()))
        .expect("start tcp journal service");
    let addr = wait_for_addr(&published);
    let client = thread::spawn(move || {
        let mut stream = TcpStream::connect(addr).expect("connect tcp journal service");
        stream.write_all(b"grain").expect("write client payload");
        stream
            .shutdown(Shutdown::Write)
            .expect("finish client write");
        let mut reply = String::new();
        stream.read_to_string(&mut reply).expect("read reply");
        reply
    });

    wait_until(|| {
        observed
            .lock()
            .expect("observed lock")
            .contains(&"grain".to_string())
    });
    assert_eq!(client.join().expect("client thread joins"), "journaled\n");
    wait_until(|| {
        observed
            .lock()
            .expect("observed lock")
            .contains(&"closed".to_string())
    });

    let terminal = app.shutdown().drain().join().expect("shutdown tcp app");
    assert_eq!(terminal.state(), LocalSystemState::Closed);
    let summary = terminal.summary();
    assert_eq!(summary.call_completed, 7);
    assert_eq!(summary.call_failed, 0);
    assert_eq!(summary.call_completion_rejected, 0);
    assert_eq!(summary.send_rejected, 0);
    assert_eq!(summary.journal_appended, 1);
    assert_eq!(summary.persistence_failed, 0);
    for expected in [
        tina_runtime::CallKind::TcpBind,
        tina_runtime::CallKind::TcpAccept,
        tina_runtime::CallKind::TcpRead,
        tina_runtime::CallKind::JournalAppend,
        tina_runtime::CallKind::TcpWrite,
        tina_runtime::CallKind::TcpStreamClose,
        tina_runtime::CallKind::TcpListenerClose,
    ] {
        assert!(
            terminal.trace().iter().any(|event| {
                matches!(
                    event.kind(),
                    RuntimeEventKind::CallCompleted { call_kind, .. } if call_kind == expected
                )
            }),
            "expected {expected:?} completion in tcp journal service trace"
        );
    }

    let replay = tina_runtime::persistence::replay_journal(&journal_path)
        .expect("tcp journal replays after app shutdown");
    assert_eq!(replay.records.len(), 1);
    assert_eq!(replay.records[0].index, 1);
    assert_eq!(replay.records[0].bytes, b"grain");
    let _ = fs::remove_dir_all(dir);
}

#[test]
fn local_system_storage_lane_full_is_user_visible_without_sleep_as_proof() {
    let dir = unique_dir("local-app-storage-full");
    fs::create_dir_all(&dir).expect("create storage full dir");
    let journal_path = dir.join("pressure.journal");
    let observed = Arc::new(Mutex::new(Vec::new()));

    let app = LocalSystem::single_shard(AppShard(74), AppMailboxFactory)
        .ingress_capacity(8)
        .storage_lane_capacity(1)
        .trace_retention(TraceRetention::Bounded(512))
        .build();
    let service = app
        .register_root::<StoragePressureService, Infallible>(
            StoragePressureService {
                observed: Arc::clone(&observed),
            },
            8,
        )
        .expect("register storage pressure service");

    app.try_send(service, StoragePressureMsg::Start(journal_path.clone()))
        .expect("start storage pressure service");
    wait_until(|| {
        let observed = observed.lock().expect("storage pressure observed lock");
        observed.len() == 2
            && observed.iter().any(|entry| entry == "first:Ok(())")
            && observed
                .iter()
                .any(|entry| entry == "second:Err(StorageFull)")
    });

    let terminal = app
        .shutdown()
        .drain()
        .join()
        .expect("shutdown pressure app");
    assert_eq!(terminal.state(), LocalSystemState::Closed);
    let summary = terminal.summary();
    assert_eq!(summary.journal_appended, 1);
    assert_eq!(summary.persistence_failed, 1);
    assert_eq!(summary.call_failed, 1);
    assert!(terminal.trace().iter().any(|event| {
        matches!(
            event.kind(),
            RuntimeEventKind::CallFailed {
                call_kind: tina_runtime::CallKind::JournalAppend,
                reason: CallError::StorageFull,
                ..
            }
        )
    }));
    assert!(terminal.trace().iter().any(|event| {
        matches!(
            event.kind(),
            RuntimeEventKind::JournalAppendFailed {
                record_index: 2,
                reason: CallError::StorageFull,
            }
        )
    }));

    let replay = tina_runtime::persistence::replay_journal(&journal_path)
        .expect("storage pressure journal replays");
    assert_eq!(replay.records.len(), 1);
    assert_eq!(replay.records[0].index, 1);
    assert_eq!(replay.records[0].bytes, b"first");
    let _ = fs::remove_dir_all(dir);
}

#[test]
fn local_system_cross_shard_tcp_request_persists_before_client_reply() {
    let dir = unique_dir("local-app-cross-shard-tcp-persist");
    fs::create_dir_all(&dir).expect("create cross shard tcp dir");
    let journal_path = dir.join("cross-shard-tcp.journal");
    let published = Arc::new(Mutex::new(None));
    let observed = Arc::new(Mutex::new(Vec::new()));

    let app = LocalSystem::<AppShard, AppMailboxFactory>::multi_shard(AppMailboxFactory)
        .shard(AppShard(81))
        .shard(AppShard(82))
        .ingress_capacity(8)
        .shard_pair_capacity(8)
        .storage_lane_capacity(2)
        .trace_retention(TraceRetention::Bounded(1024))
        .build();
    let worker = app
        .register_root_on::<CrossShardPersistWorker, CrossShardTcpMsg>(
            ShardId::new(82),
            CrossShardPersistWorker {
                journal_path: journal_path.clone(),
                last_journal_index: 0,
            },
            8,
        )
        .expect("register cross shard persist worker");
    let frontend = app
        .register_root_on::<CrossShardTcpFrontend, CrossShardPersistMsg>(
            ShardId::new(81),
            CrossShardTcpFrontend {
                worker,
                published: Arc::clone(&published),
                observed: Arc::clone(&observed),
                listener: None,
                stream: None,
            },
            8,
        )
        .expect("register cross shard tcp frontend");

    app.try_send(frontend, CrossShardTcpMsg::Start)
        .expect("start cross shard tcp frontend");
    let addr = wait_for_addr(&published);
    let client = thread::spawn(move || {
        let mut stream = TcpStream::connect(addr).expect("connect cross shard tcp frontend");
        stream
            .write_all(b"grain")
            .expect("write cross shard payload");
        stream
            .shutdown(Shutdown::Write)
            .expect("finish cross shard write");
        let mut reply = String::new();
        stream.read_to_string(&mut reply).expect("read reply");
        reply
    });

    wait_until(|| {
        observed
            .lock()
            .expect("cross shard observed lock")
            .contains(&"grain".to_string())
    });
    assert_eq!(
        client.join().expect("client thread joins"),
        "persisted:grain\n"
    );
    wait_until(|| {
        observed
            .lock()
            .expect("cross shard observed lock")
            .contains(&"closed".to_string())
    });

    let terminal = app
        .shutdown()
        .drain()
        .join()
        .expect("shutdown cross shard tcp app");
    assert_eq!(terminal.state(), LocalSystemState::Closed);
    let summary = terminal.summary();
    assert_eq!(summary.journal_appended, 1);
    assert_eq!(summary.persistence_failed, 0);
    assert_eq!(summary.call_failed, 0);
    assert_eq!(summary.send_rejected, 0);
    assert!(terminal.trace().iter().any(|event| {
        event.shard() == ShardId::new(81)
            && matches!(
                event.kind(),
                RuntimeEventKind::SendAccepted {
                    target_shard,
                    ..
                } if target_shard == ShardId::new(82)
            )
    }));
    assert!(terminal.trace().iter().any(|event| {
        event.shard() == ShardId::new(82)
            && matches!(
                event.kind(),
                RuntimeEventKind::SendAccepted {
                    target_shard,
                    ..
                } if target_shard == ShardId::new(81)
            )
    }));
    for expected in [
        tina_runtime::CallKind::TcpBind,
        tina_runtime::CallKind::TcpAccept,
        tina_runtime::CallKind::TcpRead,
        tina_runtime::CallKind::JournalAppend,
        tina_runtime::CallKind::TcpWrite,
        tina_runtime::CallKind::TcpStreamClose,
        tina_runtime::CallKind::TcpListenerClose,
    ] {
        assert!(
            terminal.trace().iter().any(|event| {
                matches!(
                    event.kind(),
                    RuntimeEventKind::CallCompleted { call_kind, .. } if call_kind == expected
                )
            }),
            "expected {expected:?} completion in cross-shard tcp service trace"
        );
    }

    let replay = tina_runtime::persistence::replay_journal(&journal_path)
        .expect("cross shard tcp journal replays");
    assert_eq!(replay.records.len(), 1);
    assert_eq!(replay.records[0].index, 1);
    assert_eq!(replay.records[0].bytes, b"grain");
    let _ = fs::remove_dir_all(dir);
}

#[test]
fn llama_tcp_timer_service_uses_local_system_runtime_owned_time() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let app = LocalSystem::single_shard(AppShard(50), AppMailboxFactory)
        .ingress_capacity(8)
        .trace_retention(TraceRetention::Bounded(128))
        .build();
    let address = app
        .register_root::<TimerService, Infallible>(
            TimerService {
                seen: Arc::clone(&seen),
            },
            8,
        )
        .expect("register timer root");

    app.try_send(address, TimerMsg::Start)
        .expect("timer start handoff");
    wait_until(|| seen.lock().expect("timer seen lock").as_slice() == ["finished"]);

    let terminal = app.shutdown().drain().join().expect("timer app shutdown");
    assert_eq!(terminal.state(), LocalSystemState::Closed);
    assert!(terminal.trace().iter().any(|event| {
        matches!(
            event.kind(),
            RuntimeEventKind::CallCompleted {
                call_kind: tina_runtime::CallKind::Sleep,
                ..
            }
        )
    }));
}

#[test]
fn local_system_service_uses_runtime_owned_file_io() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock after epoch")
        .as_nanos();
    let path = std::env::temp_dir().join(format!(
        "tina-local-app-file-{}-{unique}.txt",
        std::process::id()
    ));
    let app = LocalSystem::single_shard(AppShard(51), AppMailboxFactory)
        .ingress_capacity(8)
        .trace_retention(TraceRetention::Bounded(256))
        .build();
    let address = app
        .register_root::<FileService, Infallible>(
            FileService {
                seen: Arc::clone(&seen),
            },
            8,
        )
        .expect("register file root");

    app.try_send(address, FileMsg::Start(path.clone()))
        .expect("file start handoff");
    wait_until(|| seen.lock().expect("file seen lock").as_slice() == ["local system file"]);

    let terminal = app.shutdown().drain().join().expect("file app shutdown");
    let _ = fs::remove_file(path);
    assert_eq!(terminal.state(), LocalSystemState::Closed);
    for expected in [
        tina_runtime::CallKind::FileOpen,
        tina_runtime::CallKind::FileWriteAt,
        tina_runtime::CallKind::FileFsync,
        tina_runtime::CallKind::FileSize,
        tina_runtime::CallKind::FileReadAt,
        tina_runtime::CallKind::FileClose,
        tina_runtime::CallKind::PathMetadata,
        tina_runtime::CallKind::RenameReplace,
        tina_runtime::CallKind::ReadDir,
        tina_runtime::CallKind::SyncParent,
        tina_runtime::CallKind::RemoveFile,
    ] {
        assert!(
            terminal.trace().iter().any(|event| {
                matches!(
                    event.kind(),
                    RuntimeEventKind::CallCompleted { call_kind, .. } if call_kind == expected
                )
            }),
            "expected {expected:?} completion in local system trace"
        );
    }
}

#[test]
fn local_system_reports_live_owned_resources_and_shutdown_cleanup() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let path = unique_dir("local-app-held-file").with_extension("txt");
    let app = LocalSystem::single_shard(AppShard(52), AppMailboxFactory)
        .ingress_capacity(8)
        .trace_retention(TraceRetention::Bounded(128))
        .build();
    let address = app
        .register_root::<HeldFileService, Infallible>(
            HeldFileService {
                seen: Arc::clone(&seen),
            },
            8,
        )
        .expect("register held file root");

    app.try_send(address, HeldFileMsg::Start(path.clone()))
        .expect("held file start handoff");
    wait_until(|| seen.lock().expect("held file seen lock").as_slice() == ["opened"]);
    wait_until_debug(
        || {
            app.topology()
                .shard(ShardId::new(52))
                .expect("held file shard")
                .owned_resource_count()
                == 1
        },
        || {
            format!(
                "topology={:?}",
                app.topology()
                    .shard(ShardId::new(52))
                    .expect("held file shard")
            )
        },
    );

    let live_topology = app.topology();
    assert_eq!(
        live_topology
            .shard(ShardId::new(52))
            .expect("held file shard")
            .owned_resource_count(),
        1
    );

    let terminal = app.shutdown().drain().join().expect("held file shutdown");
    let _ = fs::remove_file(path);
    assert_eq!(terminal.state(), LocalSystemState::Closed);
    assert_eq!(
        terminal.shutdown_report().remaining_owned_resource_count(),
        0
    );
    assert_eq!(
        terminal
            .topology()
            .expect("terminal topology")
            .shard(ShardId::new(52))
            .expect("terminal held file shard")
            .owned_resource_count(),
        0
    );
}

#[test]
fn local_system_try_build_reports_worker_startup_failure() {
    let error = LocalSystem::single_shard(AppShard(60), PanickingMailboxFactory)
        .ingress_capacity(8)
        .trace_retention(TraceRetention::Bounded(16))
        .try_build()
        .err()
        .expect("panicking factory must fail startup");

    assert!(matches!(
        error,
        tina_runtime::StartupError::WorkerStartupPanicked { shard, ref message }
            if shard == ShardId::new(60) && message.contains("test mailbox factory panic")
    ));
}
