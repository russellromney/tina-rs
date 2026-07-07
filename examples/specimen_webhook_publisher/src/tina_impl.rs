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
use std::sync::Arc;
use std::time::Duration;

use tina::prelude::*;
use tina_reqwest_bridge::{
    ReqwestAddress, ReqwestCallError, ReqwestCallOutcome, ReqwestConfig, ReqwestMsg,
    ReqwestRequest, ReqwestResponse, ReqwestWorker, flatten_outcome, send_request,
};
use tina_runtime::{CallOutcome, DefaultThreadedMailboxFactory, RuntimeCall, ThreadedRuntime, call};

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
                send_request(self.http, request, self.timeout)
                    .then(DriverMsg::PostedViaSendRequest)
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

impl Isolate for Driver {
    tina::isolate_types! {
        message: DriverMsg,
        reply: (),
        send: tina::Outbound<Infallible>,
        spawn: Infallible,
        io: RuntimeCall<DriverMsg>,
        shard: SingleShard,
    }

    fn handle(&mut self, msg: DriverMsg, _ctx: &mut Context<'_, SingleShard, Self::Reply>) -> Effect<Self> {
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

pub fn run() -> Report {
    let runtime = Arc::new(ThreadedRuntime::new(
        SingleShard,
        DefaultThreadedMailboxFactory,
    ));

    let webhook = WebhookServer::spawn();
    let url = webhook.url();

    let bridge =
        ReqwestWorker::<SingleShard>::install(&runtime, ReqwestConfig::default()).expect("install");

    let driver = Driver {
        http: bridge.address,
        url: String::new(),
        counter: 0,
        timeout: Duration::from_secs(2),
    };
    let driver_addr = runtime
        .register_with_capacity::<_, Infallible>(driver, 8)
        .expect("register driver");

    let complete = runtime.observe_isolate_complete(driver_addr);

    runtime
        .try_send(driver_addr, DriverMsg::Run(url))
        .expect("kick driver");

    complete
        .wait(Duration::from_secs(10))
        .expect(
            "tina driver did not stop before timeout — the bridge or \
             the webhook never finished. The webhook bodies assertion \
             would otherwise blame the wrong layer.",
        );

    // Let the webhook server's writer task land before snapshotting.
    std::thread::sleep(Duration::from_millis(20));
    let bodies = webhook.snapshot();
    webhook.stop();

    if let Ok(rt) = Arc::try_unwrap(runtime) {
        let _ = rt.shutdown();
    }
    Report { bodies }
}
