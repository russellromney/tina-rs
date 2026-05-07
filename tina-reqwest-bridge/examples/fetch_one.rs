//! Small Tina service that fetches one URL through the reqwest bridge
//! and prints the outcome. The shape mirrors `call(...).reply(...)`
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
    ReqwestConfig, ReqwestError, ReqwestMsg, ReqwestRequest, ReqwestResponse, ReqwestWorker,
};
use tina_runtime::{
    CallOutcome, DefaultThreadedMailboxFactory, RuntimeCall, ThreadedRuntime,
    ThreadedRuntimeConfig, call,
};

#[derive(Debug)]
enum AppMsg {
    Start(String),
    HttpReturned(CallOutcome<Result<ReqwestResponse, ReqwestError>>),
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
    http: Address<ReqwestMsg, Result<ReqwestResponse, ReqwestError>>,
    done: Arc<DoneSignal>,
}

impl Isolate for App {
    tina::isolate_types! {
        message: AppMsg,
        reply: (),
        send: tina::Outbound<Infallible>,
        spawn: Infallible,
        call: RuntimeCall<AppMsg>,
        shard: SingleShard,
    }

    fn handle(&mut self, msg: AppMsg, _ctx: &mut Context<'_, SingleShard>) -> Effect<Self> {
        match msg {
            AppMsg::Start(url) => {
                println!("fetching {url}");
                call(
                    self.http,
                    ReqwestMsg::Send(ReqwestRequest::get(&url)),
                    Duration::from_secs(5),
                )
                .reply(AppMsg::HttpReturned)
            }
            AppMsg::HttpReturned(outcome) => {
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
                    CallOutcome::Timeout => println!("err call timeout"),
                }
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
