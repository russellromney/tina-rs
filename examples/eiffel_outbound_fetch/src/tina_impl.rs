//! Tina: a `Fetcher` isolate that walks `tcp_connect → tcp_write
//! (loop on partial writes) → tcp_read (loop until EOF) →
//! tcp_close_stream` for each iteration. The host watches with
//! `observe_isolate_complete(fetcher)`; per-fetch results land in a
//! shared `Outcome` slot (app-specific data the runtime can't know
//! about; FINDINGS.md tracks it).

use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering};
use std::time::Duration;

use tina::prelude::*;
use tina_runtime::{
    CallError, DefaultThreadedMailboxFactory, StreamId, ThreadedRuntime, tcp_close_stream,
    tcp_connect, tcp_read, tcp_write,
};

use crate::{FETCH_COUNT, RESPONSE, Report, TestServer};

/// Per-iteration tally. App-specific data the runtime can't know;
/// shared via an `Arc<Outcome>` side channel, same shape as
/// `eiffel_mux_client::ArrivalLog` and
/// `eiffel_persistent_counter::Observation`.
#[derive(Debug, Default)]
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

/// Per-fetch working state. `Default` so the spawn site doesn't
/// hand-zero buffers / clear `stream`.
#[derive(Debug, Default)]
struct FetchState {
    stream: Option<StreamId>,
    pending_write: Vec<u8>,
    response_buf: Vec<u8>,
}

struct Fetcher {
    target: SocketAddr,
    remaining: u32,
    outcome: Arc<Outcome>,
    state: FetchState,
}

#[tina_runtime::isolate(message = FetchMsg)]
impl Fetcher {
    fn handle(&mut self, msg: FetchMsg, _ctx: &mut Context<'_, SingleShard>) -> Effect<Self> {
        match msg {
            FetchMsg::Begin => {
                if self.remaining == 0 {
                    return stop();
                }
                tcp_connect(self.target).reply(FetchMsg::Connected)
            }
            FetchMsg::Connected(Ok((stream, _local, _peer))) => {
                self.state.stream = Some(stream);
                self.state.pending_write = b"GET\n".to_vec();
                self.state.response_buf.clear();
                tcp_write(stream, self.state.pending_write.clone()).reply(FetchMsg::Wrote)
            }
            FetchMsg::Wrote(Ok(count)) => {
                let stream = self.state.stream.expect("stream after connect");
                if count >= self.state.pending_write.len() {
                    self.state.pending_write.clear();
                    tcp_read(stream, 64).reply(FetchMsg::Read)
                } else {
                    self.state.pending_write.drain(..count);
                    tcp_write(stream, self.state.pending_write.clone()).reply(FetchMsg::Wrote)
                }
            }
            FetchMsg::Read(Ok(bytes)) => {
                let stream = self.state.stream.expect("stream after read");
                if bytes.is_empty() {
                    if self.state.response_buf == RESPONSE {
                        self.outcome.successful.fetch_add(1, Ordering::Relaxed);
                        self.outcome
                            .bytes
                            .fetch_add(self.state.response_buf.len(), Ordering::Relaxed);
                    } else {
                        self.outcome.failed.fetch_add(1, Ordering::Relaxed);
                    }
                    tcp_close_stream(stream).reply(FetchMsg::Closed)
                } else {
                    self.state.response_buf.extend_from_slice(&bytes);
                    tcp_read(stream, 64).reply(FetchMsg::Read)
                }
            }
            FetchMsg::Closed(Ok(())) | FetchMsg::Closed(Err(_)) => {
                self.state.stream = None;
                self.next_iteration()
            }
            FetchMsg::Connected(Err(_)) => {
                self.outcome.failed.fetch_add(1, Ordering::Relaxed);
                self.next_iteration()
            }
            FetchMsg::Wrote(Err(_)) | FetchMsg::Read(Err(_)) => {
                self.outcome.failed.fetch_add(1, Ordering::Relaxed);
                self.close_or_iterate()
            }
        }
    }
}

impl Fetcher {
    fn close_or_iterate(&mut self) -> Effect<Self> {
        if let Some(stream) = self.state.stream.take() {
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

pub fn run() -> anyhow::Result<Report> {
    let server = TestServer::start(FETCH_COUNT)?;
    let addr = server.addr;

    let runtime = ThreadedRuntime::new(SingleShard, DefaultThreadedMailboxFactory);
    let outcome = Arc::new(Outcome::default());
    let fetcher = runtime
        .register_with_capacity::<_, Infallible>(
            Fetcher {
                target: addr,
                remaining: FETCH_COUNT,
                outcome: Arc::clone(&outcome),
                state: FetchState::default(),
            },
            16,
        )
        .map_err(|e| anyhow::anyhow!("register fetcher: {e:?}"))?;

    let fetcher_done = runtime.observe_isolate_complete(fetcher);
    runtime
        .try_send(fetcher, FetchMsg::Begin)
        .map_err(|e| anyhow::anyhow!("kick fetcher: {e:?}"))?;
    fetcher_done
        .wait(Duration::from_secs(5))
        .map_err(|e| anyhow::anyhow!("fetcher finishes: {e:?}"))?;

    let _ = runtime.shutdown();
    drop(server);

    Ok(Report {
        successful_fetches: outcome.successful.load(Ordering::Relaxed),
        failed_fetches: outcome.failed.load(Ordering::Relaxed),
        bytes_received: outcome.bytes.load(Ordering::Relaxed),
        exit_clean: true,
    })
}
