use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering};
use std::time::Duration;

use tina::prelude::*;
use tina_runtime::{
    CallError, DefaultThreadedMailboxFactory, StreamId, ThreadedRuntime, ThreadedRuntimeConfig,
    tcp_close_stream, tcp_connect, tcp_read, tcp_write,
};

use super::{FETCH_COUNT, RESPONSE, SideReport, TestServer};

#[derive(Debug, Default)]
struct FetchShard;

impl Shard for FetchShard {
    fn id(&self) -> ShardId {
        ShardId::new(81)
    }
}

#[derive(Default)]
struct Outcome {
    successful: AtomicU32,
    failed: AtomicU32,
    bytes: AtomicUsize,
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
        DefaultThreadedMailboxFactory,
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

    // Phase 047 Rock 4: typed isolate-complete waiter replaces the
    // `Arc<AtomicBool> done` flag + spin loop. Register before kicking the
    // fetcher so the host is already observing when the fetcher finishes.
    let fetcher_done = runtime.observe_isolate_complete(fetcher);
    runtime
        .try_send(fetcher, FetchMsg::Begin)
        .expect("kick fetcher");

    fetcher_done
        .wait(Duration::from_secs(5))
        .expect("tina fetcher finishes");

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
