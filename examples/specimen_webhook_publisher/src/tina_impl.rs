//! Tina: a `Driver` isolate runs three scripted increments against
//! the reqwest bridge. Each increment exercises a different
//! call-site shape so the file reads as a side-by-side comparison:
//!
//! 1. Increment #1 uses `send_request(...).then(...)` — the
//!    polished helper. This is the recommended default shape.
//! 2. Increment #2 uses literal
//!    `call(addr, ReqwestMsg::Send(req), timeout)` — the raw layered
//!    form. Same outcome type, more boilerplate. Kept here so the
//!    underlying contract stays exercised in real call sites, not
//!    only in unit tests.
//! 3. Increment #3 uses `send_request(...)` AND collapses the
//!    outcome with `flatten_outcome(...)` inside the reply
//!    translator — the opt-in flat-error edge.
//!
//! All three send the same kind of POST; the webhook records bodies
//! `["1", "2", "3"]` regardless of which shape produced them.

use std::convert::Infallible;
use std::time::Duration;

use tina::prelude::*;
use tina_reqwest_bridge::{
    ReqwestAddress, ReqwestCallError, ReqwestCallOutcome, ReqwestConfig, ReqwestMsg,
    ReqwestRequest, ReqwestResponse, ReqwestWorker, flatten_outcome, send_request,
};
use tina_runtime::{CallOutcome, DefaultThreadedMailboxFactory, LocalSystem, call};

use crate::{Report, WebhookServer};

const REQUESTS: usize = 3;

#[derive(Debug)]
enum DriverMsg {
    /// Kicks the run. Carries the webhook URL.
    Run(String),
    /// Continuation from the polished `send_request` helper. The
    /// translator passes the outcome through unchanged so this arm
    /// can show the layered match.
    PostedViaSendRequest(ReqwestCallOutcome),
    /// Continuation from the raw `call(addr, ReqwestMsg::Send(...))`
    /// path. Identical outcome type to PostedViaSendRequest.
    PostedViaRawCall(ReqwestCallOutcome),
    /// Continuation from the polished helper with `flatten_outcome`
    /// inside the reply translator. The driver receives a single
    /// `Result` instead of the layered outcome.
    PostedFlattened(Result<ReqwestResponse, ReqwestCallError>),
}

struct Driver {
    http: ReqwestAddress,
    url: String,
    counter: u64,
    timeout: Duration,
}

impl Driver {
    fn next_post(&mut self) -> Effect<Self> {
        self.counter += 1;
        let body = self.counter.to_string().into_bytes();
        let request = ReqwestRequest::post(self.url.clone(), body);
        match self.counter {
            1 => {
                // Shape 1: polished send_request helper. Layered match
                // in PostedViaSendRequest. This is the default a user
                // should reach for.
                send_request(self.http, request, self.timeout).then(DriverMsg::PostedViaSendRequest)
            }
            2 => {
                // Shape 2: raw layered call. Functionally identical
                // to send_request; spelled out so the underlying
                // bridge contract stays exercised in real call sites.
                // If you find yourself writing this every time, use
                // shape 1 instead.
                call(self.http, ReqwestMsg::Send(request), self.timeout)
                    .then(DriverMsg::PostedViaRawCall)
            }
            3 => {
                // Shape 3: polished helper + flatten_outcome at the
                // reply translator boundary. The continuation
                // message variant carries Result<R, ReqwestCallError>
                // instead of CallOutcome<Result<...>>. Use this only
                // at app edges where you don't need to distinguish
                // bridge-delivery failures from worker-domain
                // failures — the flat error type still names which
                // layer failed via Bridge(...) vs Worker(...).
                send_request(self.http, request, self.timeout)
                    .then(|outcome| DriverMsg::PostedFlattened(flatten_outcome(outcome)))
            }
            _ => noop(),
        }
    }
}

#[tina_runtime::isolate(message = DriverMsg)]
impl Driver {
    fn handle(
        &mut self,
        msg: DriverMsg,
        _ctx: &mut Context<'_, SingleShard, Self::Reply>,
    ) -> Effect<Self> {
        match msg {
            DriverMsg::Run(url) => {
                self.url = url;
                self.next_post()
            }
            DriverMsg::PostedViaSendRequest(outcome) => {
                check_layered(&outcome, "send_request");
                self.next_post()
            }
            DriverMsg::PostedViaRawCall(outcome) => {
                check_layered(&outcome, "raw call");
                self.next_post()
            }
            DriverMsg::PostedFlattened(result) => {
                check_flat(&result);
                if self.counter >= REQUESTS as u64 {
                    stop()
                } else {
                    self.next_post()
                }
            }
        }
    }
}

fn check_layered(outcome: &ReqwestCallOutcome, label: &str) {
    match outcome {
        CallOutcome::Replied(Ok(response)) => {
            assert!(
                response.status.is_success(),
                "{label} got non-2xx: {}",
                response.status
            );
        }
        CallOutcome::Replied(Err(err)) => {
            panic!("{label} worker error: {err}");
        }
        CallOutcome::Full => panic!("{label} bridge full"),
        CallOutcome::Closed => panic!("{label} bridge closed"),
        CallOutcome::Timeout => panic!("{label} bridge call timed out"),
        CallOutcome::Rejected(reason) => panic!("{label} bridge rejected: {reason:?}"),
    }
}

fn check_flat(result: &Result<ReqwestResponse, ReqwestCallError>) {
    match result {
        Ok(response) => {
            assert!(
                response.status.is_success(),
                "flatten_outcome got non-2xx: {}",
                response.status
            );
        }
        Err(ReqwestCallError::Bridge(b)) => {
            panic!("flatten_outcome bridge layer failed: {b:?}");
        }
        Err(ReqwestCallError::Worker(e)) => {
            panic!("flatten_outcome worker layer failed: {e}");
        }
    }
}

pub fn run() -> anyhow::Result<Report> {
    let webhook = WebhookServer::spawn();
    let url = webhook.url();
    let app = LocalSystem::single_shard(SingleShard, DefaultThreadedMailboxFactory).try_build()?;
    let runtime_result =
        app.run_to_shutdown_reported(Duration::from_secs(5), move |app| run_application(app, url))
            .map_err(anyhow::Error::from);
    let webhook_result = webhook.stop_and_snapshot();
    finish_run(webhook_result, runtime_result)
}

fn finish_run(
    webhook_result: anyhow::Result<Vec<String>>,
    runtime_result: anyhow::Result<()>,
) -> anyhow::Result<Report> {
    // Preserve the specimen's established external-server-first error
    // precedence while still naming both failures when both sides fail.
    match (webhook_result, runtime_result) {
        (Ok(bodies), Ok(())) => Ok(Report { bodies }),
        (Err(webhook), Ok(())) => Err(webhook),
        (Ok(_), Err(runtime)) => Err(runtime),
        (Err(webhook), Err(runtime)) => Err(anyhow::anyhow!(
            "webhook shutdown failed: {webhook:#}; Tina runtime shutdown also failed: {runtime:#}"
        )),
    }
}

fn run_application(
    app: &LocalSystem<SingleShard, DefaultThreadedMailboxFactory>,
    url: String,
) -> anyhow::Result<()> {
    let bridge = ReqwestWorker::<SingleShard>::install_local(app, ReqwestConfig::default())
        .map_err(|e| anyhow::anyhow!("install reqwest bridge: {e}"))?;

    let driver = Driver {
        http: bridge.address,
        url: String::new(),
        counter: 0,
        timeout: Duration::from_secs(2),
    };
    let driver_addr = app
        .register_root::<_, Infallible>(driver, 8)
        .map_err(|e| anyhow::anyhow!("register driver: {e:?}"))?;

    let complete = app
        .observe_isolate_complete(driver_addr)
        .map_err(|e| anyhow::anyhow!("observe isolate complete: {e:?}"))?;

    app.try_send(driver_addr, DriverMsg::Run(url))
        .map_err(|e| anyhow::anyhow!("kick driver: {e:?}"))?;

    complete.wait(Duration::from_secs(10)).map_err(|_| {
        anyhow::anyhow!(
            "tina driver did not stop before timeout — the bridge or \
             the webhook never finished. The webhook bodies assertion \
             would otherwise blame the wrong layer."
        )
    })?;

    bridge.closer.close();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn webhook_error_precedence_is_stable_and_dual_failures_name_both() {
        let webhook_only = finish_run(Err(anyhow::anyhow!("webhook")), Ok(()))
            .expect_err("webhook failure propagates");
        assert_eq!(webhook_only.to_string(), "webhook");

        let runtime_only = finish_run(Ok(Vec::new()), Err(anyhow::anyhow!("runtime")))
            .expect_err("runtime failure propagates");
        assert_eq!(runtime_only.to_string(), "runtime");

        let both = finish_run(
            Err(anyhow::anyhow!("webhook")),
            Err(anyhow::anyhow!("runtime")),
        )
        .expect_err("dual failure propagates");
        assert_eq!(
            both.to_string(),
            "webhook shutdown failed: webhook; Tina runtime shutdown also failed: runtime"
        );
    }
}
