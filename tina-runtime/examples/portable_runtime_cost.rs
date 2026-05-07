use std::collections::VecDeque;
use std::convert::Infallible;
use std::fs;
use std::hint::black_box;
use std::io::{Read, Write};
use std::net::{Shutdown, SocketAddr, TcpStream};
use std::path::PathBuf;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicU64, Ordering},
};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use tina::{Address, Mailbox, TrySendError, prelude::*};
use tina_runtime::{
    CallError, CallOutcome, ListenerId, LocalSystem, MailboxFactory, StreamId, call, tcp_accept,
    tcp_bind, tcp_close_listener, tcp_close_stream, tcp_read, tcp_write,
};

fn main() {
    let profile = option_env!("PROFILE").unwrap_or("unknown");
    println!("tina portable runtime cost smoke / local machine / not benchmark");
    println!(
        "backend=betelgeuse-shaped-local profile={profile} platform={}",
        std::env::consts::OS
    );
    println!("capacities=default preallocation=default");
    println!("operation,iterations,timing_ns,status");

    print_measured("mailbox local push/pop", 10_000, || {
        let mut queue = VecDeque::with_capacity(1);
        queue.push_back(black_box(1_u64));
        black_box(queue.pop_front());
    });
    print_measured_once("local send", 200, measure_local_send);
    print_measured_once("live ingress", 200, measure_live_ingress);
    print_measured_once("cross-shard send", 200, measure_cross_shard_send);
    print_measured_once("isolate call", 200, measure_isolate_call);
    print_measured("timer", 10_000, || {
        black_box(Instant::now() + Duration::from_micros(1));
    });
    print_measured_once("Tina TCP loopback", 20, measure_tina_tcp_loopback);
    print_unmeasured("TLS loopback", "covered by e2e; no portable cost row yet");
    print_measured("file read/write", 20, || {
        let path = temp_path("file-cost");
        fs::write(&path, b"baobab").expect("write cost file");
        black_box(fs::read(&path).expect("read cost file"));
        let _ = fs::remove_file(path);
    });
    print_measured("journal append", 20, || {
        let path = temp_path("journal-cost");
        tina_runtime::persistence::append_journal_record(&path, 1, b"baobab".to_vec())
            .expect("append journal row");
        black_box(tina_runtime::persistence::replay_journal(&path).expect("replay journal row"));
        let _ = fs::remove_file(path);
    });
    print_unmeasured(
        "bridge call",
        "covered by bridge e2e; no portable cost row yet",
    );
}

fn print_measured(label: &str, iterations: u64, mut run: impl FnMut()) {
    let started = Instant::now();
    for _ in 0..iterations {
        run();
    }
    let total_ns = started.elapsed().as_nanos();
    println!("{label},{iterations},{total_ns},measured-local-smoke");
}

fn print_measured_once(label: &str, iterations: u64, run: impl FnOnce(u64)) {
    let started = Instant::now();
    run(iterations);
    let total_ns = started.elapsed().as_nanos();
    println!("{label},{iterations},{total_ns},measured-local-smoke");
}

fn print_unmeasured(label: &str, reason: &str) {
    println!("{label},0,n/a,not-measured:{reason}");
}

fn temp_path(prefix: &str) -> PathBuf {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock after epoch")
        .as_nanos();
    std::env::temp_dir().join(format!("tina-{prefix}-{}-{now}", std::process::id()))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct CostShard(u32);

impl Shard for CostShard {
    fn id(&self) -> ShardId {
        ShardId::new(self.0)
    }
}

struct CostMailbox<T> {
    capacity: usize,
    queue: Mutex<VecDeque<T>>,
    closed: Mutex<bool>,
}

impl<T> CostMailbox<T> {
    fn new(capacity: usize) -> Self {
        Self {
            capacity,
            queue: Mutex::new(VecDeque::new()),
            closed: Mutex::new(false),
        }
    }
}

impl<T> Mailbox<T> for CostMailbox<T> {
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

    fn close(&self) {
        *self.closed.lock().expect("closed lock") = true;
    }
}

#[derive(Debug, Clone, Copy)]
struct CostMailboxFactory;

impl MailboxFactory for CostMailboxFactory {
    fn create<T: 'static>(&self, capacity: usize) -> Box<dyn Mailbox<T>> {
        Box::new(CostMailbox::new(capacity))
    }
}

#[derive(Debug, Clone, Copy)]
enum CountMsg {
    Hit,
}

#[derive(Debug)]
struct Counter {
    count: Arc<AtomicU64>,
}

#[tina_runtime::isolate(message = CountMsg, shard = CostShard)]
impl Counter {
    fn handle(
        &mut self,
        msg: CountMsg,
        _ctx: &mut Context<'_, CostShard, Self::Reply>,
    ) -> Effect<Self> {
        match msg {
            CountMsg::Hit => {
                self.count.fetch_add(1, Ordering::Relaxed);
                noop()
            }
        }
    }
}

#[derive(Debug, Clone, Copy)]
enum RelayMsg {
    Forward,
}

#[derive(Debug)]
struct Relay {
    target: Address<CountMsg>,
}

#[tina_runtime::isolate(message = RelayMsg, send = Outbound<CountMsg>, shard = CostShard)]
impl Relay {
    fn handle(
        &mut self,
        msg: RelayMsg,
        _ctx: &mut Context<'_, CostShard, Self::Reply>,
    ) -> Effect<Self> {
        match msg {
            RelayMsg::Forward => send(self.target, CountMsg::Hit),
        }
    }
}

#[derive(Debug, Clone, Copy)]
enum PingMsg {
    Ping,
}

#[derive(Debug, Clone, Copy)]
enum PingReply {
    Pong,
}

#[derive(Debug)]
struct PingWorker;

#[tina_runtime::isolate(message = PingMsg, reply = PingReply, shard = CostShard)]
impl PingWorker {
    fn handle(
        &mut self,
        msg: PingMsg,
        _ctx: &mut Context<'_, CostShard, Self::Reply>,
    ) -> Effect<Self> {
        match msg {
            PingMsg::Ping => reply(PingReply::Pong),
        }
    }
}

#[derive(Debug, Clone)]
enum ClientMsg {
    Start,
    Done(CallOutcome<PingReply>),
}

#[derive(Debug)]
struct PingClient {
    worker: Address<PingMsg, PingReply>,
    count: Arc<AtomicU64>,
}

#[tina_runtime::isolate(
    message = ClientMsg,
    call = tina_runtime::RuntimeCall<ClientMsg>,
    shard = CostShard
)]
impl PingClient {
    fn handle(
        &mut self,
        msg: ClientMsg,
        _ctx: &mut Context<'_, CostShard, Self::Reply>,
    ) -> Effect<Self> {
        match msg {
            ClientMsg::Start => {
                call(self.worker, PingMsg::Ping, Duration::from_secs(1)).reply(ClientMsg::Done)
            }
            ClientMsg::Done(CallOutcome::Replied(PingReply::Pong)) => {
                self.count.fetch_add(1, Ordering::Relaxed);
                noop()
            }
            ClientMsg::Done(_) => noop(),
        }
    }
}

#[derive(Debug, Clone)]
enum TcpCostMsg {
    Start,
    Bound(Result<(ListenerId, SocketAddr), CallError>),
    Accepted(Result<(StreamId, SocketAddr), CallError>),
    Read(Result<Vec<u8>, CallError>, StreamId),
    Wrote(Result<usize, CallError>, StreamId),
    StreamClosed(Result<(), CallError>),
    ListenerClosed(Result<(), CallError>),
}

#[derive(Debug)]
struct TcpCostService {
    published: Arc<Mutex<Option<SocketAddr>>>,
    completed: Arc<AtomicU64>,
    failed: Arc<Mutex<Option<String>>>,
    listener: Option<ListenerId>,
    buffer: Vec<u8>,
}

#[tina_runtime::isolate(
    message = TcpCostMsg,
    call = tina_runtime::RuntimeCall<TcpCostMsg>,
    shard = CostShard
)]
impl TcpCostService {
    fn handle(
        &mut self,
        msg: TcpCostMsg,
        _ctx: &mut Context<'_, CostShard, Self::Reply>,
    ) -> Effect<Self> {
        match msg {
            TcpCostMsg::Start => tcp_bind("127.0.0.1:0".parse().expect("tcp cost bind addr"))
                .reply(TcpCostMsg::Bound),
            TcpCostMsg::Bound(Ok((listener, addr))) => {
                self.listener = Some(listener);
                *self.published.lock().expect("tcp cost published lock") = Some(addr);
                tcp_accept(listener).reply(TcpCostMsg::Accepted)
            }
            TcpCostMsg::Bound(Err(error)) => self.fail(format!("tcp-bind:{error:?}")),
            TcpCostMsg::Accepted(Ok((stream, _peer))) => {
                self.buffer.clear();
                tcp_read(stream, 2).reply(move |result| TcpCostMsg::Read(result, stream))
            }
            TcpCostMsg::Accepted(Err(error)) => self.fail(format!("tcp-accept:{error:?}")),
            TcpCostMsg::Read(Ok(bytes), stream) if bytes.is_empty() => {
                self.record_failure("tcp-eof-before-ping");
                tcp_close_stream(stream).reply(TcpCostMsg::StreamClosed)
            }
            TcpCostMsg::Read(Ok(bytes), stream) => {
                self.buffer.extend_from_slice(&bytes);
                match self.buffer.len().cmp(&4) {
                    std::cmp::Ordering::Less => {
                        tcp_read(stream, 2).reply(move |result| TcpCostMsg::Read(result, stream))
                    }
                    std::cmp::Ordering::Equal if self.buffer == b"ping" => {
                        tcp_write(stream, self.buffer.clone())
                            .reply(move |result| TcpCostMsg::Wrote(result, stream))
                    }
                    _ => {
                        self.record_failure(format!("tcp-read-shape:{:?}", self.buffer));
                        tcp_close_stream(stream).reply(TcpCostMsg::StreamClosed)
                    }
                }
            }
            TcpCostMsg::Read(Err(error), stream) => {
                self.record_failure(format!("tcp-read:{error:?}"));
                tcp_close_stream(stream).reply(TcpCostMsg::StreamClosed)
            }
            TcpCostMsg::Wrote(Ok(4), stream) => {
                tcp_close_stream(stream).reply(TcpCostMsg::StreamClosed)
            }
            TcpCostMsg::Wrote(Ok(written), stream) => {
                self.record_failure(format!("tcp-short-write:{written}"));
                tcp_close_stream(stream).reply(TcpCostMsg::StreamClosed)
            }
            TcpCostMsg::Wrote(Err(error), stream) => {
                self.record_failure(format!("tcp-write:{error:?}"));
                tcp_close_stream(stream).reply(TcpCostMsg::StreamClosed)
            }
            TcpCostMsg::StreamClosed(result) => {
                if let Err(error) = result {
                    self.record_failure(format!("tcp-close-stream:{error:?}"));
                }
                match self.listener.take() {
                    Some(listener) => {
                        tcp_close_listener(listener).reply(TcpCostMsg::ListenerClosed)
                    }
                    None => self.fail("tcp-listener-missing"),
                }
            }
            TcpCostMsg::ListenerClosed(result) => {
                if let Err(error) = result {
                    self.record_failure(format!("tcp-close-listener:{error:?}"));
                }
                self.completed.fetch_add(1, Ordering::Relaxed);
                noop()
            }
        }
    }
}

impl TcpCostService {
    fn fail(&self, reason: impl Into<String>) -> Effect<Self> {
        self.record_failure(reason);
        self.completed.fetch_add(1, Ordering::Relaxed);
        noop()
    }

    fn record_failure(&self, reason: impl Into<String>) {
        *self.failed.lock().expect("tcp cost failure lock") = Some(reason.into());
    }
}

fn measure_live_ingress(iterations: u64) {
    let count = Arc::new(AtomicU64::new(0));
    let app = LocalSystem::single_shard(CostShard(1), CostMailboxFactory)
        .ingress_capacity(iterations as usize + 8)
        .build();
    let counter = app
        .register_root::<Counter, Infallible>(
            Counter {
                count: Arc::clone(&count),
            },
            iterations as usize + 8,
        )
        .expect("register cost counter");
    for _ in 0..iterations {
        app.try_send(counter, CountMsg::Hit)
            .expect("cost ingress send");
    }
    wait_count(&count, iterations);
    app.shutdown()
        .drain()
        .join()
        .expect("cost ingress shutdown");
}

fn measure_local_send(iterations: u64) {
    let count = Arc::new(AtomicU64::new(0));
    let app = LocalSystem::single_shard(CostShard(6), CostMailboxFactory)
        .ingress_capacity(iterations as usize + 8)
        .build();
    let counter = app
        .register_root::<Counter, Infallible>(
            Counter {
                count: Arc::clone(&count),
            },
            iterations as usize + 8,
        )
        .expect("register cost local counter");
    let relay = app
        .register_root::<Relay, CountMsg>(Relay { target: counter }, iterations as usize + 8)
        .expect("register cost local relay");
    for _ in 0..iterations {
        app.try_send(relay, RelayMsg::Forward)
            .expect("cost local relay send");
    }
    wait_count(&count, iterations);
    app.shutdown()
        .drain()
        .join()
        .expect("cost local send shutdown");
}

fn measure_cross_shard_send(iterations: u64) {
    let count = Arc::new(AtomicU64::new(0));
    let app = LocalSystem::<CostShard, CostMailboxFactory>::multi_shard(CostMailboxFactory)
        .shard(CostShard(2))
        .shard(CostShard(3))
        .ingress_capacity(iterations as usize + 8)
        .shard_pair_capacity(iterations as usize + 8)
        .build();
    let counter = app
        .register_root_on::<Counter, Infallible>(
            ShardId::new(3),
            Counter {
                count: Arc::clone(&count),
            },
            iterations as usize + 8,
        )
        .expect("register cost remote counter");
    let relay = app
        .register_root_on::<Relay, CountMsg>(
            ShardId::new(2),
            Relay { target: counter },
            iterations as usize + 8,
        )
        .expect("register cost relay");
    for _ in 0..iterations {
        app.try_send(relay, RelayMsg::Forward)
            .expect("cost relay send");
    }
    wait_count(&count, iterations);
    app.shutdown()
        .drain()
        .join()
        .expect("cost cross-shard shutdown");
}

fn measure_isolate_call(iterations: u64) {
    let count = Arc::new(AtomicU64::new(0));
    let app = LocalSystem::<CostShard, CostMailboxFactory>::multi_shard(CostMailboxFactory)
        .shard(CostShard(4))
        .shard(CostShard(5))
        .ingress_capacity(iterations as usize + 8)
        .shard_pair_capacity(iterations as usize + 8)
        .build();
    let worker = app
        .register_root_on::<PingWorker, Infallible>(
            ShardId::new(5),
            PingWorker,
            iterations as usize + 8,
        )
        .expect("register cost ping worker");
    let client = app
        .register_root_on::<PingClient, Infallible>(
            ShardId::new(4),
            PingClient {
                worker,
                count: Arc::clone(&count),
            },
            iterations as usize + 8,
        )
        .expect("register cost ping client");
    for _ in 0..iterations {
        app.try_send(client, ClientMsg::Start)
            .expect("cost call start");
    }
    wait_count(&count, iterations);
    app.shutdown().drain().join().expect("cost call shutdown");
}

fn measure_tina_tcp_loopback(iterations: u64) {
    for _ in 0..iterations {
        let published = Arc::new(Mutex::new(None));
        let completed = Arc::new(AtomicU64::new(0));
        let failed = Arc::new(Mutex::new(None));
        let app = LocalSystem::single_shard(CostShard(7), CostMailboxFactory)
            .ingress_capacity(8)
            .storage_lane_capacity(2)
            .build();
        let service = app
            .register_root::<TcpCostService, Infallible>(
                TcpCostService {
                    published: Arc::clone(&published),
                    completed: Arc::clone(&completed),
                    failed: Arc::clone(&failed),
                    listener: None,
                    buffer: Vec::new(),
                },
                8,
            )
            .expect("register cost TCP service");
        app.try_send(service, TcpCostMsg::Start)
            .expect("start cost TCP service");
        let addr = wait_socket_addr(&published);
        let mut client = TcpStream::connect(addr).expect("connect cost tcp listener");
        client.write_all(b"ping").expect("write cost tcp request");
        client
            .shutdown(Shutdown::Write)
            .expect("shutdown cost tcp write");
        let mut reply = [0_u8; 4];
        client.read_exact(&mut reply).expect("read cost tcp reply");
        assert_eq!(reply, *b"ping");
        black_box(reply);
        wait_count(&completed, 1);
        app.shutdown().drain().join().expect("cost tcp shutdown");
        if let Some(reason) = failed.lock().expect("tcp cost failure lock").clone() {
            panic!("Tina TCP cost path failed: {reason}");
        }
    }
}

fn wait_socket_addr(published: &Mutex<Option<SocketAddr>>) -> SocketAddr {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if let Some(addr) = *published.lock().expect("published lock") {
            return addr;
        }
        assert!(Instant::now() <= deadline, "cost TCP publish timed out");
        std::thread::yield_now();
    }
}

fn wait_count(count: &AtomicU64, expected: u64) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while count.load(Ordering::Relaxed) < expected {
        assert!(Instant::now() <= deadline, "cost row timed out");
        std::thread::yield_now();
    }
}
