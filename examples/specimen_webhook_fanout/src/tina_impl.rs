use std::convert::Infallible;
use std::time::Duration;

use tina::prelude::*;
use tina_reqwest_bridge::{
    ReqwestAddress, ReqwestCallOutcome, ReqwestConfig, ReqwestOutcomeClass, ReqwestOutcomeExt,
    ReqwestRequest, ReqwestTransientReason, ReqwestWorker, send_request,
};
use tina_runtime::{BoundedItems, DefaultThreadedMailboxFactory, LocalSystem, bounded_batch};

use crate::upstream::{self, Upstream};
use crate::{MAX_ENDPOINTS, Report, WebhookTerminal};

const PER_CALL_TIMEOUT: Duration = Duration::from_millis(150);

#[derive(Debug)]
enum DispatcherMsg {
    Begin,
    HookReturned(ReqwestCallOutcome),
}

#[derive(Debug, Default, Clone)]
struct Counts {
    delivered: u32,
    server_unavailable: u32,
    timed_out: u32,
    other: u32,
    terminals: Vec<WebhookTerminal>,
}

struct Dispatcher {
    http: ReqwestAddress,
    urls: BoundedItems<String>,
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
                if self.pending == 0 {
                    return stop_with(std::mem::take(&mut self.counts));
                }
                let urls = self.urls.clone();
                bounded_batch(urls.map_effects(|url| {
                    send_request(self.http, ReqwestRequest::get(&url), PER_CALL_TIMEOUT)
                        .then(DispatcherMsg::HookReturned)
                }))
            }
            DispatcherMsg::HookReturned(outcome) => {
                match outcome.classify() {
                    ReqwestOutcomeClass::Succeeded(_) => self.counts.delivered += 1,
                    ReqwestOutcomeClass::Transient(reason) => {
                        match &reason {
                            ReqwestTransientReason::UpstreamServer { status }
                                if status.as_u16() == 503 =>
                            {
                                self.counts.server_unavailable += 1
                            }
                            ReqwestTransientReason::BridgeTimeout
                            | ReqwestTransientReason::WorkerTimeout => self.counts.timed_out += 1,
                            ReqwestTransientReason::UpstreamServer { .. }
                            | ReqwestTransientReason::WorkerTransport(_) => self.counts.other += 1,
                        }
                        self.counts
                            .terminals
                            .push(WebhookTerminal::Transient(reason));
                    }
                    ReqwestOutcomeClass::Fatal(reason) => {
                        self.counts.other += 1;
                        self.counts.terminals.push(WebhookTerminal::Fatal(reason));
                    }
                }
                self.pending -= 1;
                if self.pending == 0 {
                    stop_with(self.counts.clone())
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
    let shutdown = upstream.stop();
    match (result, shutdown) {
        (Ok(report), Ok(())) => Ok(report),
        (Err(run), Ok(())) => Err(run),
        (Ok(_), Err(shutdown)) => Err(shutdown),
        (Err(run), Err(shutdown)) => Err(anyhow::anyhow!(
            "run failed: {run:#}; shutdown also failed: {shutdown:#}"
        )),
    }
}

fn run_inner(upstream: &Upstream) -> anyhow::Result<Report> {
    let app = LocalSystem::single_shard(SingleShard, DefaultThreadedMailboxFactory).try_build()?;
    Ok(app.run_to_shutdown_reported(Duration::from_secs(5), |app| run_application(app, upstream))?)
}

fn run_application(
    app: &LocalSystem<SingleShard, DefaultThreadedMailboxFactory>,
    upstream: &Upstream,
) -> anyhow::Result<Report> {
    let bridge = ReqwestWorker::<SingleShard>::install_local(app, ReqwestConfig::default())
        .map_err(|e| anyhow::anyhow!("install reqwest bridge: {e}"))?;

    let addrs = BoundedItems::try_from_iter(MAX_ENDPOINTS, upstream.addrs.iter().copied())
        .map_err(|error| anyhow::anyhow!("bound webhook endpoints: {error}"))?;
    let urls = BoundedItems::try_from_iter(
        MAX_ENDPOINTS,
        addrs
            .into_vec()
            .into_iter()
            .map(|addr| format!("http://{addr}/hook")),
    )
    .map_err(|error| anyhow::anyhow!("bound webhook URLs: {error}"))?;
    let pending = urls.len() as u32;

    let dispatcher = app
        .register_root::<_, Infallible>(
            Dispatcher {
                http: bridge.address,
                urls,
                pending,
                counts: Counts::default(),
            },
            32,
        )
        .map_err(|e| anyhow::anyhow!("register dispatcher: {e:?}"))?;

    let result = app
        .observe_result::<Counts, _, _>(dispatcher)
        .map_err(|e| anyhow::anyhow!("observe_result: {e:?}"))?;
    app.try_send(dispatcher, DispatcherMsg::Begin)
        .map_err(|e| anyhow::anyhow!("send Begin: {e:?}"))?;

    let counts = result
        .wait(Duration::from_secs(10))
        .map_err(|e| anyhow::anyhow!("dispatcher did not finish: {e:?}"))?;

    bridge.closer.close();
    Ok(Report {
        delivered: counts.delivered,
        server_unavailable: counts.server_unavailable,
        timed_out: counts.timed_out,
        other: counts.other,
        exit_clean: true,
        tina_terminals: counts.terminals,
    })
}
