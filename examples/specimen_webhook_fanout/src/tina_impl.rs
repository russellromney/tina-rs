use std::convert::Infallible;
use std::sync::Arc;
use std::time::Duration;

use tina::prelude::*;
use tina_reqwest_bridge::{
    ReqwestAddress, ReqwestCallOutcome, ReqwestConfig, ReqwestFatalReason, ReqwestOutcomeClass,
    ReqwestOutcomeExt, ReqwestRequest, ReqwestTransientReason, ReqwestWorker, send_request,
};
use tina_runtime::{DefaultThreadedMailboxFactory, ThreadedRuntime};

use crate::Report;
use crate::upstream::{self, Upstream};

const PER_CALL_TIMEOUT: Duration = Duration::from_millis(150);

#[derive(Debug)]
enum DispatcherMsg {
    Begin,
    HookReturned(ReqwestCallOutcome),
}

#[derive(Debug, Default, Clone, Copy)]
struct Counts {
    delivered: u32,
    server_unavailable: u32,
    timed_out: u32,
    other: u32,
}

struct Dispatcher {
    http: ReqwestAddress,
    urls: Vec<String>,
    pending: u32,
    counts: Counts,
}

#[tina_runtime::isolate(message = DispatcherMsg)]
impl Dispatcher {
    fn handle(
        &mut self,
        msg: DispatcherMsg,
        _ctx: &mut Context<'_, SingleShard, Self::Reply>,
    ) -> Effect<Self> {
        match msg {
            DispatcherMsg::Begin => {
                let calls: Vec<_> = self
                    .urls
                    .iter()
                    .map(|url| {
                        send_request(self.http, ReqwestRequest::get(url), PER_CALL_TIMEOUT)
                            .then(DispatcherMsg::HookReturned)
                    })
                    .collect();
                Effect::Batch(calls)
            }
            DispatcherMsg::HookReturned(outcome) => {
                match outcome.classify() {
                    ReqwestOutcomeClass::Succeeded(_) => self.counts.delivered += 1,
                    ReqwestOutcomeClass::Transient(ReqwestTransientReason::UpstreamServer {
                        status,
                    }) if status.as_u16() == 503 => self.counts.server_unavailable += 1,
                    ReqwestOutcomeClass::Transient(
                        ReqwestTransientReason::BridgeTimeout
                        | ReqwestTransientReason::WorkerTimeout,
                    ) => self.counts.timed_out += 1,
                    ReqwestOutcomeClass::Transient(_) => self.counts.other += 1,
                    ReqwestOutcomeClass::Fatal(ReqwestFatalReason::UpstreamClient { .. }) => {
                        self.counts.other += 1
                    }
                    ReqwestOutcomeClass::Fatal(_) => self.counts.other += 1,
                }
                self.pending -= 1;
                if self.pending == 0 {
                    stop_with(self.counts)
                } else {
                    noop()
                }
            }
        }
    }
}

pub fn run() -> anyhow::Result<Report> {
    let upstream = upstream::spawn(&upstream::workload())?;
    let result = run_inner(&upstream);
    upstream.stop();
    result
}

fn run_inner(upstream: &Upstream) -> anyhow::Result<Report> {
    let runtime = Arc::new(ThreadedRuntime::new(
        SingleShard,
        DefaultThreadedMailboxFactory,
    ));

    let bridge = ReqwestWorker::<SingleShard>::install(&runtime, ReqwestConfig::default())
        .map_err(|e| anyhow::anyhow!("install reqwest bridge: {e}"))?;

    let urls: Vec<String> = upstream
        .addrs
        .iter()
        .map(|a| format!("http://{a}/hook"))
        .collect();
    let pending = urls.len() as u32;

    let dispatcher = runtime
        .register_with_capacity::<_, Infallible>(
            Dispatcher {
                http: bridge.address,
                urls,
                pending,
                counts: Counts::default(),
            },
            32,
        )
        .map_err(|e| anyhow::anyhow!("register dispatcher: {e:?}"))?;

    let result = runtime
        .observe_result::<Counts, _, _>(dispatcher)
        .map_err(|e| anyhow::anyhow!("observe_result: {e:?}"))?;
    runtime
        .try_send(dispatcher, DispatcherMsg::Begin)
        .map_err(|e| anyhow::anyhow!("send Begin: {e:?}"))?;

    let counts = result
        .wait(Duration::from_secs(10))
        .map_err(|e| anyhow::anyhow!("dispatcher did not finish: {e:?}"))?;

    bridge.closer.close();
    if let Ok(rt) = Arc::try_unwrap(runtime) {
        let _ = rt.shutdown();
    }
    Ok(Report {
        delivered: counts.delivered,
        server_unavailable: counts.server_unavailable,
        timed_out: counts.timed_out,
        other: counts.other,
        exit_clean: true,
    })
}
