//! Tina side. The counter is a registered Tina isolate behind
//! `TinaTowerService`. Each request schedules `sleep(SLOW_HANDLER_MS)`
//! before responding so the slow work shows up as a real Tina runtime
//! call (one trace event per call). The Tower stack is identical to
//! the Tokio side: `ServiceBuilder::new().timeout(...).concurrency_limit(...)`.

use std::collections::VecDeque;
use std::convert::Infallible;
use std::time::Duration;

use tina::prelude::*;
use tina_runtime::{DefaultThreadedMailboxFactory, SleepReply, sleep};
use tina_tokio_bridge::{BridgeError, BridgeHost, BridgeMessage, BridgeRequest, BridgeResponder};
use tina_tower_bridge::TinaTowerService;
use tokio::runtime::Builder;
use tokio::task::JoinSet;
use tower::limit::ConcurrencyLimitLayer;
use tower::timeout::TimeoutLayer;
use tower::timeout::error::Elapsed;
use tower::{BoxError, ServiceBuilder, ServiceExt};

use crate::{BURST, CONCURRENCY, Report, SLOW_HANDLER_MS, TIMEOUT_MS};

#[derive(Debug, Clone, Copy)]
pub struct BrushRequest;

#[derive(Debug, Clone, Copy)]
pub struct BrushReply {
    #[allow(dead_code)]
    pub value: u64,
}

#[derive(Debug)]
enum CounterMsg {
    /// Bridge request that arrived from the Tower stack.
    Request(BridgeRequest<BrushRequest, BrushReply>),
    /// Sleep continuation: the slow work for the in-flight request
    /// has elapsed; respond and become idle again.
    Done(SleepReply),
}

impl From<BridgeRequest<BrushRequest, BrushReply>> for CounterMsg {
    fn from(req: BridgeRequest<BrushRequest, BrushReply>) -> Self {
        CounterMsg::Request(req)
    }
}

impl BridgeMessage for CounterMsg {
    fn bridge_cancelled(&self) -> bool {
        match self {
            CounterMsg::Request(req) => req.is_cancelled(),
            CounterMsg::Done(_) => false,
        }
    }
}

struct Counter {
    value: u64,
    /// Work queue of in-flight responders. Each `Done` tick pops the
    /// front, increments the counter, and responds. The queue makes
    /// the serialization explicit instead of clobbering a single slot
    /// when concurrent BridgeRequests arrive in the mailbox before
    /// the previous sleep completes.
    pending: VecDeque<BridgeResponder<BrushReply>>,
    slow: Duration,
    busy: bool,
}

#[tina_runtime::isolate(message = CounterMsg)]
impl Counter {
    fn handle(&mut self, msg: CounterMsg, _ctx: &mut Context<'_, SingleShard>) -> Effect<Self> {
        match msg {
            CounterMsg::Request(req) => {
                if req.is_cancelled() {
                    // Tower already gave up on this request (timeout
                    // layer fired or future was dropped). Skip it.
                    return noop();
                }
                let (_, responder) = req.into_parts();
                self.pending.push_back(responder);
                if self.busy {
                    noop()
                } else {
                    self.busy = true;
                    sleep(self.slow).reply(CounterMsg::Done)
                }
            }
            CounterMsg::Done(reply) => {
                if reply.is_err() {
                    // Sleep was cancelled (runtime shutdown). Drop the
                    // queue; the bridge will surface this as
                    // `BridgeError::Closed` to the Tower stack.
                    self.pending.clear();
                    self.busy = false;
                    return noop();
                }
                self.value += 1;
                if let Some(responder) = self.pending.pop_front() {
                    let _ = responder.respond(BrushReply { value: self.value });
                }
                if self.pending.is_empty() {
                    self.busy = false;
                    noop()
                } else {
                    sleep(self.slow).reply(CounterMsg::Done)
                }
            }
        }
    }
}

pub fn run() -> anyhow::Result<Report> {
    let rt = Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()?;
    rt.block_on(async {
        let mut host = BridgeHost::new(
            SingleShard,
            DefaultThreadedMailboxFactory,
            Default::default(),
        );
        let bridge = host
            .register_bridge::<Counter, BrushRequest, BrushReply, Infallible>(
                Counter {
                    value: 0,
                    pending: VecDeque::new(),
                    slow: Duration::from_millis(SLOW_HANDLER_MS),
                    busy: false,
                },
                32,
                Duration::from_secs(2),
            )
            .map_err(|e| anyhow::anyhow!("register bridge: {e:?}"))?;

        let svc = TinaTowerService::new(bridge);
        let stack = ServiceBuilder::new()
            .layer(TimeoutLayer::new(Duration::from_millis(TIMEOUT_MS)))
            .layer(ConcurrencyLimitLayer::new(CONCURRENCY))
            .service(svc);

        let mut report = Report::default();
        let mut joins: JoinSet<Outcome> = JoinSet::new();
        for _ in 0..BURST {
            let svc = stack.clone();
            joins.spawn(async move { call_one(svc).await });
        }
        while let Some(joined) = joins.join_next().await {
            match joined.expect("join") {
                Outcome::Ok => report.successful_count += 1,
                Outcome::ServiceUnavailable => report.service_unavailable_count += 1,
                Outcome::GatewayTimeout => report.gateway_timeout_count += 1,
            }
        }
        report.exit_clean = true;

        // Bridge lifecycle: the helper waits for in-flight clones to
        // drop, then shuts the runtime down. With JoinSet drained, the
        // only remaining handle is the one inside `stack`; dropping
        // `stack` releases it.
        drop(stack);
        let _ = host.drain_and_shutdown(Duration::from_secs(2));

        Ok(report)
    })
}

enum Outcome {
    Ok,
    ServiceUnavailable,
    GatewayTimeout,
}

async fn call_one<S>(svc: S) -> Outcome
where
    S: tower::Service<BrushRequest, Response = BrushReply, Error = BoxError>,
{
    match svc.oneshot(BrushRequest).await {
        Ok(_) => Outcome::Ok,
        Err(e) => classify(&e),
    }
}

fn classify(e: &BoxError) -> Outcome {
    if e.is::<Elapsed>() {
        return Outcome::GatewayTimeout;
    }
    if let Some(b) = e.downcast_ref::<BridgeError>() {
        return match b {
            BridgeError::Timeout => Outcome::GatewayTimeout,
            BridgeError::Full | BridgeError::Closed => Outcome::ServiceUnavailable,
        };
    }
    Outcome::ServiceUnavailable
}
