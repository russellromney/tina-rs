//! Put one S3 object through the Tina AWS bridge.
//!
//! Run with real AWS static credentials:
//!
//! ```text
//! AWS_ACCESS_KEY_ID=... AWS_SECRET_ACCESS_KEY=... \
//! cargo run -p tina-aws-bridge --example put_object -- my-bucket path/key.txt hello
//! ```

use std::convert::Infallible;
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use tina::prelude::*;
use tina_aws_bridge::{
    S3Address, S3CallOutcome, S3Config, S3Credentials, S3PutObject, S3Request, S3Response,
    install_s3, send_s3,
};
use tina_runtime::{
    CallOutcome, DefaultThreadedMailboxFactory, RuntimeCall, ThreadedRuntime, ThreadedRuntimeConfig,
};

#[derive(Debug)]
enum AppMsg {
    Start {
        bucket: String,
        key: String,
        body: Vec<u8>,
    },
    PutDone(S3CallOutcome),
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
            AppMsg::Start { bucket, key, body } => send_s3(
                self.aws,
                S3Request::PutObject(S3PutObject {
                    bucket,
                    key,
                    body,
                    content_type: Some("text/plain".into()),
                }),
                Duration::from_secs(10),
            )
            .then(AppMsg::PutDone),
            AppMsg::PutDone(outcome) => {
                match outcome {
                    CallOutcome::Replied(Ok(S3Response::PutObject(ok))) => {
                        println!("put ok etag={:?} version={:?}", ok.e_tag, ok.version_id);
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
    let mut args = std::env::args().skip(1);
    let bucket = args.next().ok_or("usage: put_object BUCKET KEY BODY")?;
    let key = args.next().ok_or("usage: put_object BUCKET KEY BODY")?;
    let body = args
        .next()
        .ok_or("usage: put_object BUCKET KEY BODY")?
        .into_bytes();

    let mut credentials = S3Credentials::new(
        std::env::var("AWS_ACCESS_KEY_ID")?,
        std::env::var("AWS_SECRET_ACCESS_KEY")?,
    );
    if let Ok(token) = std::env::var("AWS_SESSION_TOKEN") {
        credentials = credentials.with_session_token(token);
    }

    let runtime = Arc::new(ThreadedRuntime::with_config(
        SingleShard,
        DefaultThreadedMailboxFactory,
        ThreadedRuntimeConfig {
            command_capacity: 8,
            idle_wait: Duration::from_millis(1),
            ..Default::default()
        },
    ));
    let bridge = install_s3(
        &runtime,
        S3Config::new()
            .with_credentials(credentials)
            .with_max_in_flight(4)
            .with_default_timeout(Duration::from_secs(10)),
    )
    .map_err(|e| format!("install bridge: {e}"))?;

    let done = Arc::new(DoneSignal::default());
    let app = App {
        aws: bridge.address,
        done: Arc::clone(&done),
    };
    let app_addr = runtime
        .register_with_capacity::<_, Infallible>(app, 4)
        .map_err(|e| format!("register app: {e:?}"))?;
    runtime
        .try_send(app_addr, AppMsg::Start { bucket, key, body })
        .map_err(|e| format!("try_send: {e:?}"))?;
    done.wait(Duration::from_secs(30));

    let m = bridge.metrics.snapshot();
    println!(
        "metrics: admitted={} responses={} full={} timeout={} late={}",
        m.admitted, m.responses, m.full, m.timeouts, m.late_results
    );

    if let Ok(rt) = Arc::try_unwrap(runtime) {
        let _ = rt.shutdown();
    }
    Ok(())
}
