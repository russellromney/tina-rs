//! Tina: a `Fetcher` isolate that walks `tcp_connect → write_all →
//! read_to_eof → tcp_close_stream` for each iteration. Phase 059
//! Rocks 1+3:
//!
//! - the partial-write loop and the read-until-EOF loop are owned by
//!   client-side helpers (`TcpWriteAll`, `TcpReadToEof`) so the
//!   handler arms collapse to "advance the helper, dispatch the next
//!   effect or move on";
//! - the host receives the per-fetch tally through
//!   `observe_result::<FetchOutcome>(fetcher)` when the isolate
//!   finishes via `stop_with(self.outcome)`.

use std::convert::Infallible;
use std::net::SocketAddr;
use std::time::Duration;

use tina::prelude::*;
use tina_runtime::{
    DefaultThreadedMailboxFactory, LoopStep, StreamId, TcpConnectReply, TcpReadReply,
    TcpReadToEof, TcpStreamCloseReply, TcpWriteAll, TcpWriteReply, ThreadedRuntime,
    tcp_close_stream, tcp_connect,
};

use crate::{FETCH_COUNT, READ_CHUNK, RESPONSE, RESPONSE_MAX, Report, TestServer};

/// Per-iteration tally. Owned by the isolate; published to the host via
/// `stop_with` when the isolate finishes.
#[derive(Debug, Default, Clone)]
struct FetchOutcome {
    successful: u32,
    failed: u32,
    bytes: usize,
}

#[derive(Debug)]
enum FetchMsg {
    Begin,
    Connected(TcpConnectReply),
    Wrote(TcpWriteReply),
    Read(TcpReadReply),
    /// Payload is kept for trace shape but not inspected at the
    /// handler — close is fire-and-forget here.
    Closed(#[allow(dead_code)] TcpStreamCloseReply),
}

/// Per-fetch working state. The two loop helpers are `Option`s so a
/// fresh iteration starts from `None` without inventing a sentinel.
#[derive(Debug, Default)]
struct FetchState {
    stream: Option<StreamId>,
    write_all: Option<TcpWriteAll>,
    read_to_eof: Option<TcpReadToEof>,
}

struct Fetcher {
    target: SocketAddr,
    remaining: u32,
    outcome: FetchOutcome,
    state: FetchState,
}

#[tina_runtime::isolate(message = FetchMsg)]
impl Fetcher {
    fn handle(&mut self, msg: FetchMsg, _ctx: &mut Context<'_, SingleShard, Self::Reply>) -> Effect<Self> {
        match msg {
            FetchMsg::Begin => {
                if self.remaining == 0 {
                    return stop_with(self.outcome.clone());
                }
                tcp_connect(self.target).then(FetchMsg::Connected)
            }
            FetchMsg::Connected(Ok((stream, _local, _peer))) => {
                self.state.stream = Some(stream);
                let writer = TcpWriteAll::new(stream, b"GET\n".to_vec());
                let effect = writer
                    .next_effect(FetchMsg::Wrote)
                    .expect("write helper has bytes to ship");
                self.state.write_all = Some(writer);
                effect
            }
            FetchMsg::Wrote(reply) => {
                let mut writer = self
                    .state
                    .write_all
                    .take()
                    .expect("write helper present after Connected");
                match writer.advance(reply, FetchMsg::Wrote) {
                    LoopStep::Pending(effect) => {
                        self.state.write_all = Some(writer);
                        effect
                    }
                    LoopStep::Done(_) => self.start_read(),
                    LoopStep::Failed(_) => {
                        self.outcome.failed += 1;
                        self.close_or_iterate()
                    }
                }
            }
            FetchMsg::Read(reply) => {
                let mut reader = self
                    .state
                    .read_to_eof
                    .take()
                    .expect("read helper present after Wrote");
                match reader.advance(reply, FetchMsg::Read) {
                    LoopStep::Pending(effect) => {
                        self.state.read_to_eof = Some(reader);
                        effect
                    }
                    LoopStep::Done(buffer) => {
                        if buffer == RESPONSE {
                            self.outcome.successful += 1;
                            self.outcome.bytes += buffer.len();
                        } else {
                            self.outcome.failed += 1;
                        }
                        self.close_or_iterate()
                    }
                    LoopStep::Failed(_) => {
                        self.outcome.failed += 1;
                        self.close_or_iterate()
                    }
                }
            }
            FetchMsg::Closed(_) => {
                self.state.stream = None;
                self.next_iteration()
            }
            FetchMsg::Connected(Err(_)) => {
                self.outcome.failed += 1;
                self.next_iteration()
            }
        }
    }
}

impl Fetcher {
    fn start_read(&mut self) -> Effect<Self> {
        let stream = self.state.stream.expect("stream after Connected");
        let reader = TcpReadToEof::new(stream, RESPONSE_MAX, READ_CHUNK);
        let effect = reader
            .next_effect(FetchMsg::Read)
            .expect("read helper has budget for at least one read");
        self.state.read_to_eof = Some(reader);
        effect
    }

    fn close_or_iterate(&mut self) -> Effect<Self> {
        if let Some(stream) = self.state.stream.take() {
            tcp_close_stream(stream).then(FetchMsg::Closed)
        } else {
            self.next_iteration()
        }
    }

    fn next_iteration(&mut self) -> Effect<Self> {
        self.remaining -= 1;
        if self.remaining == 0 {
            stop_with(self.outcome.clone())
        } else {
            tcp_connect(self.target).then(FetchMsg::Connected)
        }
    }
}

pub fn run() -> anyhow::Result<Report> {
    let server = TestServer::start(FETCH_COUNT)?;
    let addr = server.addr;

    let runtime = ThreadedRuntime::new(SingleShard, DefaultThreadedMailboxFactory);
    let fetcher = runtime
        .register_with_capacity::<_, Infallible>(
            Fetcher {
                target: addr,
                remaining: FETCH_COUNT,
                outcome: FetchOutcome::default(),
                state: FetchState::default(),
            },
            16,
        )
        .map_err(|e| anyhow::anyhow!("register fetcher: {e:?}"))?;

    let result = runtime
        .observe_result::<FetchOutcome, _, _>(fetcher)
        .map_err(|e| anyhow::anyhow!("register result waiter: {e:?}"))?;
    runtime
        .try_send(fetcher, FetchMsg::Begin)
        .map_err(|e| anyhow::anyhow!("kick fetcher: {e:?}"))?;
    let outcome = result
        .wait(Duration::from_secs(5))
        .map_err(|e| anyhow::anyhow!("fetcher finishes with result: {e:?}"))?;

    let _ = runtime.shutdown();
    drop(server);

    Ok(Report {
        successful_fetches: outcome.successful,
        failed_fetches: outcome.failed,
        bytes_received: outcome.bytes,
        exit_clean: true,
    })
}
