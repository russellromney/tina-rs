//! Small Tina service that fetches one URL through the reqwest bridge
//! and prints the outcome. The shape mirrors `call(...).then(...)`
//! from the plan.
//!
//! Run with:
//!
//! ```text
//! cargo run --example fetch_one -p tina-reqwest-bridge -- https://example.com/
//! ```

use std::convert::Infallible;
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use tina::prelude::*;
use tina_reqwest_bridge::{
    ReqwestAddress, ReqwestCallOutcome, ReqwestConfig, ReqwestRequest, ReqwestWorker, send_request,
};
use tina_runtime::{
    CallOutcome, DefaultThreadedMailboxFactory, RuntimeCall, ThreadedRuntime, ThreadedRuntimeConfig,
};

#[derive(Debug)]
enum AppMsg {
    Start(String),
    HttpReturned(ReqwestCallOutcome),
}

#[derive(Default)]
struct DoneSignal {
    done: Mutex<bool>,
    cv: Condvar,
}

impl DoneSignal {
    fn signal(&self) {
        *self.done.lock().expect("done lock") = true;
        self.cv.notify_all();
    }

    fn wait(&self, timeout: Duration) {
        let deadline = std::time::Instant::now() + timeout;
        let mut guard = self.done.lock().expect("done lock");
        while !*guard {
            let now = std::time::Instant::now();
            if now >= deadline {
                return;
            }
            let (g, _) = self
                .cv
                .wait_timeout(guard, deadline - now)
                .expect("done wait");
            guard = g;
        }
    }
}

struct App {
    http: ReqwestAddress,
    done: Arc<DoneSignal>,
}

impl Isolate for App {
    tina::isolate_types! {
        message: AppMsg,
        reply: (),
        send: tina::Outbound<Infallible>,
        spawn: Infallible,
        io: RuntimeCall<AppMsg>,
        shard: SingleShard,
    }

    fn handle(
        &mut self,
        msg: AppMsg,
        _ctx: &mut Context<'_, SingleShard, Self::Reply>,
    ) -> Effect<Self> {
        match msg {
            AppMsg::Start(url) => {
                println!("fetching {url}");
                send_request(self.http, ReqwestRequest::get(&url), Duration::from_secs(5))
                    .then(AppMsg::HttpReturned)
            }
            AppMsg::HttpReturned(outcome) => {
                // Layered match — the recommended shape. Bridge-layer
                // and worker-layer failures are visibly distinct.
                match outcome {
                    CallOutcome::Replied(Ok(response)) => {
                        println!(
                            "ok status={} bytes={}",
                            response.status.as_u16(),
                            response.body.len()
                        );
                    }
                    CallOutcome::Replied(Err(err)) => {
                        println!("err {err}");
                    }
                    CallOutcome::Full => println!("err bridge full"),
                    CallOutcome::Closed => println!("err bridge closed"),
                    CallOutcome::Rejected(reason) => println!("err bridge rejected: {reason:?}"),
                    CallOutcome::Timeout => println!("err call timeout"),
                }
                // Or, with the opt-in flatten helper:
                //
                // ```ignore
                // use tina_reqwest_bridge::{flatten_outcome, ReqwestCallError};
                // match flatten_outcome(outcome) {
                //     Ok(response)                       => { ... }
                //     Err(ReqwestCallError::Bridge(b))   => { ... }
                //     Err(ReqwestCallError::Worker(e))   => { ... }
                // }
                // ```
                //
                // Use the layered form by default. Use `flatten_outcome`
                // only when the call site is genuinely an app edge that
                // does not need to distinguish the two layers.
                self.done.signal();
                stop()
            }
        }
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let url = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "https://example.com/".to_string());

    let runtime = Arc::new(ThreadedRuntime::with_config(
        SingleShard,
        DefaultThreadedMailboxFactory,
        ThreadedRuntimeConfig {
            command_capacity: 8,
            idle_wait: Duration::from_millis(1),
            ..Default::default()
        },
    ));

    let bridge = ReqwestWorker::<SingleShard>::install(&runtime, ReqwestConfig::default())
        .map_err(|e| format!("install bridge: {e}"))?;

    let done = Arc::new(DoneSignal::default());
    let app = App {
        http: bridge.address,
        done: Arc::clone(&done),
    };
    let app_addr = runtime
        .register_with_capacity::<_, Infallible>(app, 4)
        .map_err(|e| format!("register app: {e:?}"))?;

    runtime
        .try_send(app_addr, AppMsg::Start(url))
        .map_err(|e| format!("try_send: {e:?}"))?;
    done.wait(Duration::from_secs(15));

    let snapshot = bridge.metrics.snapshot();
    println!(
        "metrics: admitted={} responses={} timeout={} reqwest_err={} retries={}",
        snapshot.admitted,
        snapshot.responses,
        snapshot.timeout,
        snapshot.reqwest_error,
        snapshot.retries
    );

    if let Ok(rt) = Arc::try_unwrap(runtime) {
        let _ = rt.shutdown();
    }
    Ok(())
}
