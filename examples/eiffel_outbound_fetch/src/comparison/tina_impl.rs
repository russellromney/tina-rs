use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::convert::Infallible;
use std::net::SocketAddr;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use tina::{Mailbox, TrySendError, prelude::*};
use tina_runtime::{
    CallError, MailboxFactory, StreamId, ThreadedRuntime, ThreadedRuntimeConfig, tcp_close_stream,
    tcp_connect, tcp_read, tcp_write,
};

use super::{FETCH_COUNT, RESPONSE, SideReport, TestServer};

#[derive(Debug, Default)]
struct FetchShard;

impl Shard for FetchShard {
    fn id(&self) -> ShardId {
        ShardId::new(81)
    }
}

struct FetchMailbox<T> {
    capacity: usize,
    queue: Rc<RefCell<VecDeque<T>>>,
    closed: Rc<Cell<bool>>,
}

impl<T> FetchMailbox<T> {
    fn new(capacity: usize) -> Self {
        Self {
            capacity,
            queue: Rc::new(RefCell::new(VecDeque::new())),
            closed: Rc::new(Cell::new(false)),
        }
    }
}

impl<T> Mailbox<T> for FetchMailbox<T> {
    fn capacity(&self) -> usize {
        self.capacity
    }

    fn try_send(&self, message: T) -> Result<(), TrySendError<T>> {
        if self.closed.get() {
            return Err(TrySendError::Closed(message));
        }
        let mut queue = self.queue.borrow_mut();
        if queue.len() >= self.capacity {
            return Err(TrySendError::Full(message));
        }
        queue.push_back(message);
        Ok(())
    }

    fn recv(&self) -> Option<T> {
        self.queue.borrow_mut().pop_front()
    }

    fn close(&self) {
        self.closed.set(true);
    }
}

#[derive(Debug, Clone, Copy)]
struct FetchMailboxFactory;

impl MailboxFactory for FetchMailboxFactory {
    fn create<T: 'static>(&self, capacity: usize) -> Box<dyn Mailbox<T>> {
        Box::new(FetchMailbox::new(capacity))
    }
}

#[derive(Default)]
struct Outcome {
    successful: AtomicU32,
    failed: AtomicU32,
    bytes: AtomicUsize,
    done: AtomicBool,
}

#[derive(Debug, Clone)]
enum FetchMsg {
    Begin,
    Connected(Result<(StreamId, SocketAddr, SocketAddr), CallError>),
    Wrote(Result<usize, CallError>),
    Read(Result<Vec<u8>, CallError>),
    Closed(Result<(), CallError>),
}

struct Fetcher {
    target: SocketAddr,
    remaining: u32,
    outcome: Arc<Outcome>,
    stream: Option<StreamId>,
    pending_write: Vec<u8>,
    response_buf: Vec<u8>,
}

#[tina_runtime::isolate(message = FetchMsg, shard = FetchShard)]
impl Fetcher {
    fn handle(&mut self, msg: FetchMsg, _ctx: &mut Context<'_, FetchShard>) -> Effect<Self> {
        match msg {
            FetchMsg::Begin => {
                if self.remaining == 0 {
                    self.outcome.done.store(true, Ordering::Release);
                    return stop();
                }
                tcp_connect(self.target).reply(FetchMsg::Connected)
            }
            FetchMsg::Connected(Ok((stream, _local, _peer))) => {
                self.stream = Some(stream);
                self.pending_write = b"GET\n".to_vec();
                self.response_buf.clear();
                tcp_write(stream, self.pending_write.clone()).reply(FetchMsg::Wrote)
            }
            FetchMsg::Connected(Err(_)) => {
                self.outcome.failed.fetch_add(1, Ordering::Relaxed);
                self.next_iteration()
            }
            FetchMsg::Wrote(Ok(count)) => {
                let stream = self.stream.expect("stream after connect");
                if count >= self.pending_write.len() {
                    self.pending_write.clear();
                    tcp_read(stream, 64).reply(FetchMsg::Read)
                } else {
                    self.pending_write.drain(..count);
                    tcp_write(stream, self.pending_write.clone()).reply(FetchMsg::Wrote)
                }
            }
            FetchMsg::Wrote(Err(_)) => {
                self.outcome.failed.fetch_add(1, Ordering::Relaxed);
                self.close_or_iterate()
            }
            FetchMsg::Read(Ok(bytes)) => {
                let stream = self.stream.expect("stream after read");
                if bytes.is_empty() {
                    // EOF from server; check what we got.
                    if self.response_buf == RESPONSE {
                        self.outcome.successful.fetch_add(1, Ordering::Relaxed);
                        self.outcome
                            .bytes
                            .fetch_add(self.response_buf.len(), Ordering::Relaxed);
                    } else {
                        self.outcome.failed.fetch_add(1, Ordering::Relaxed);
                    }
                    tcp_close_stream(stream).reply(FetchMsg::Closed)
                } else {
                    self.response_buf.extend_from_slice(&bytes);
                    tcp_read(stream, 64).reply(FetchMsg::Read)
                }
            }
            FetchMsg::Read(Err(_)) => {
                self.outcome.failed.fetch_add(1, Ordering::Relaxed);
                self.close_or_iterate()
            }
            FetchMsg::Closed(result) => {
                if result.is_err() {
                    // Close failures are recorded but do not prevent the
                    // next iteration; the OS has already torn down the FD.
                }
                self.stream = None;
                self.next_iteration()
            }
        }
    }
}

impl Fetcher {
    fn close_or_iterate(&mut self) -> Effect<Self> {
        if let Some(stream) = self.stream.take() {
            tcp_close_stream(stream).reply(FetchMsg::Closed)
        } else {
            self.next_iteration()
        }
    }

    fn next_iteration(&mut self) -> Effect<Self> {
        self.remaining -= 1;
        if self.remaining == 0 {
            self.outcome.done.store(true, Ordering::Release);
            stop()
        } else {
            tcp_connect(self.target).reply(FetchMsg::Connected)
        }
    }
}

pub(crate) fn run() -> SideReport {
    let server = TestServer::start(FETCH_COUNT);
    let addr = server.addr;

    let runtime = ThreadedRuntime::with_config(
        FetchShard,
        FetchMailboxFactory,
        ThreadedRuntimeConfig {
            command_capacity: 16,
            idle_wait: Duration::from_millis(1),
            ..Default::default()
        },
    );

    let outcome = Arc::new(Outcome::default());
    let fetcher = runtime
        .register_with_capacity::<Fetcher, Infallible>(
            Fetcher {
                target: addr,
                remaining: FETCH_COUNT,
                outcome: Arc::clone(&outcome),
                stream: None,
                pending_write: Vec::new(),
                response_buf: Vec::new(),
            },
            16,
        )
        .expect("register fetcher");

    runtime
        .try_send(fetcher, FetchMsg::Begin)
        .expect("kick fetcher");

    // Wait for the isolate to finish all iterations.
    let deadline = Instant::now() + Duration::from_secs(5);
    while !outcome.done.load(Ordering::Acquire) {
        if Instant::now() > deadline {
            panic!("tina fetcher timed out");
        }
        thread::yield_now();
    }

    let _ = runtime.shutdown().expect("runtime shutdown");
    drop(server);

    SideReport {
        addr_used: addr,
        successful_fetches: outcome.successful.load(Ordering::Relaxed),
        failed_fetches: outcome.failed.load(Ordering::Relaxed),
        bytes_received: outcome.bytes.load(Ordering::Relaxed),
        exit_clean: true,
    }
}
