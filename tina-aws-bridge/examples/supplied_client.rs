//! Wrap a caller-built S3 client.
//!
//! This is the path to use when credentials, endpoint policy, HTTP
//! connector, TLS, proxies, or SDK retry configuration should stay
//! fully owned by the application.
//!
//! ```text
//! cargo run -p tina-aws-bridge --example supplied_client -- http://127.0.0.1:4566 my-bucket
//! ```

use std::convert::Infallible;
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use aws_sdk_s3::Client;
use aws_sdk_s3::config::retry::RetryConfig;
use aws_sdk_s3::config::{BehaviorVersion, Credentials, Region};
use tina::prelude::*;
use tina_aws_bridge::{
    S3Address, S3CallOutcome, S3Config, S3HeadObject, S3Request, S3Response, S3Worker, send_s3,
};
use tina_runtime::{
    CallOutcome, DefaultThreadedMailboxFactory, RuntimeCall, ThreadedRuntime, ThreadedRuntimeConfig,
};

#[derive(Debug)]
enum AppMsg {
    Start(String),
    HeadDone(S3CallOutcome),
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
        let deadline = Instant::now() + timeout;
        let mut guard = self.done.lock().expect("done lock");
        while !*guard {
            let now = Instant::now();
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
    aws: S3Address,
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

    fn handle(
        &mut self,
        msg: AppMsg,
        _ctx: &mut Context<'_, SingleShard, Self::Reply>,
    ) -> Effect<Self> {
        match msg {
            AppMsg::Start(bucket) => send_s3(
                self.aws,
                S3Request::HeadObject(S3HeadObject {
                    bucket,
                    key: "healthcheck".into(),
                }),
                Duration::from_secs(5),
            )
            .then(AppMsg::HeadDone),
            AppMsg::HeadDone(outcome) => {
                match outcome {
                    CallOutcome::Replied(Ok(S3Response::HeadObject(_))) => {
                        println!("object exists");
                    }
                    CallOutcome::Replied(Ok(other)) => println!("unexpected response: {other:?}"),
                    CallOutcome::Replied(Err(err)) => println!("worker error: {err}"),
                    CallOutcome::Full => println!("bridge call full"),
                    CallOutcome::Closed => println!("bridge call closed"),
                    CallOutcome::Timeout => println!("bridge call timeout"),
                    CallOutcome::Rejected(reason) => println!("bridge call rejected: {reason:?}"),
                }
                self.done.signal();
                stop()
            }
        }
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let endpoint = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "http://127.0.0.1:4566".into());
    let bucket = std::env::args()
        .nth(2)
        .unwrap_or_else(|| "example-bucket".into());

    let client_runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .worker_threads(2)
        .build()?;
    let client = Client::from_conf(
        aws_sdk_s3::Config::builder()
            .behavior_version(BehaviorVersion::v2026_01_12())
            .region(Region::new("us-east-1"))
            .endpoint_url(endpoint)
            .force_path_style(true)
            .credentials_provider(Credentials::new(
                "local-access-key",
                "local-secret-key",
                None,
                None,
                "example",
            ))
            .retry_config(RetryConfig::standard().with_max_attempts(2))
            .build(),
    );

    let runtime = Arc::new(ThreadedRuntime::with_config(
        SingleShard,
        DefaultThreadedMailboxFactory,
        ThreadedRuntimeConfig {
            command_capacity: 8,
            idle_wait: Duration::from_millis(1),
            ..Default::default()
        },
    ));
    let (worker, metrics) = S3Worker::<SingleShard>::with_supplied_client(
        S3Config::new().with_max_in_flight(2),
        client,
        client_runtime.handle().clone(),
    )?;
    let aws = runtime
        .register_with_capacity::<_, Infallible>(worker, 8)
        .map_err(|e| format!("register bridge: {e:?}"))?;

    let done = Arc::new(DoneSignal::default());
    let app = App {
        aws,
        done: Arc::clone(&done),
    };
    let app_addr = runtime
        .register_with_capacity::<_, Infallible>(app, 4)
        .map_err(|e| format!("register app: {e:?}"))?;
    runtime
        .try_send(app_addr, AppMsg::Start(bucket))
        .map_err(|e| format!("try_send: {e:?}"))?;
    done.wait(Duration::from_secs(15));

    let m = metrics.snapshot();
    println!(
        "metrics: admitted={} responses={} sdk_max_attempts={} late={}",
        m.admitted, m.responses, m.sdk_max_attempts, m.late_results
    );
    if let Ok(rt) = Arc::try_unwrap(runtime) {
        let _ = rt.shutdown();
    }
    client_runtime.shutdown_background();
    Ok(())
}
