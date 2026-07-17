//! Tina side. Same flaky upstream, but the client is
//! [`tina_reqwest_bridge::ReqwestWorker`] with no built-in retry, plus
//! a `Caller` isolate that drives retry by hand using
//! `call(...).then(...)` and `sleep(...).then(...)`.
//!
//! The point: every attempt is one `IsolateCall` and one continuation
//! message. The retry budget, backoff, and "what counts as transient"
//! all live in user code where they are testable and visible in the
//! trace. Nothing is hidden.

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::thread;
use std::time::Duration;

use axum::Router;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::get;
use tina::prelude::*;
use tina_reqwest_bridge::{
    ReqwestAddress, ReqwestCallOutcome, ReqwestConfig, ReqwestOutcomeClass, ReqwestOutcomeExt,
    ReqwestRequest, ReqwestWorker, send_request,
};
use tina_runtime::{DefaultThreadedMailboxFactory, LocalSystem, SleepReply, sleep};
use tokio::net::TcpListener;
use tokio::runtime::Builder;
use tokio::sync::oneshot;

use crate::{FLAKY_503_RUNS, MAX_ATTEMPTS, Report};

const PER_ATTEMPT_TIMEOUT: Duration = Duration::from_secs(2);
const RETRY_BACKOFF: Duration = Duration::from_millis(20);

// ---------- Flaky upstream (Tokio + axum, side thread) ----------

#[derive(Debug, Default)]
struct FlakyState {
    seen: AtomicU32,
}

async fn flaky_handler(State(state): State<Arc<FlakyState>>) -> impl IntoResponse {
    let n = state.seen.fetch_add(1, Ordering::Relaxed) + 1;
    if n <= FLAKY_503_RUNS {
        StatusCode::SERVICE_UNAVAILABLE
    } else {
        StatusCode::OK
    }
}

struct UpstreamHandle {
    addr: SocketAddr,
    shutdown: Option<oneshot::Sender<()>>,
    thread: Option<thread::JoinHandle<()>>,
}

impl UpstreamHandle {
    fn stop(mut self) -> anyhow::Result<()> {
        let send_failed = self.shutdown.take().is_some_and(|tx| tx.send(()).is_err());
        let join_failed = self
            .thread
            .take()
            .is_some_and(|handle| handle.join().is_err());
        match (send_failed, join_failed) {
            (false, false) => Ok(()),
            (true, false) => anyhow::bail!("upstream shutdown receiver closed"),
            (false, true) => anyhow::bail!("upstream thread panicked during shutdown"),
            (true, true) => {
                anyhow::bail!("upstream shutdown receiver closed and upstream thread panicked")
            }
        }
    }
}

fn spawn_flaky_upstream() -> anyhow::Result<UpstreamHandle> {
    let runtime = Builder::new_current_thread().enable_all().build()?;
    let (addr_tx, addr_rx) = std::sync::mpsc::channel::<SocketAddr>();
    let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();

    let handle = thread::spawn(move || {
        runtime.block_on(async move {
            let state = Arc::new(FlakyState::default());
            let app = Router::new()
                .route("/flaky", get(flaky_handler))
                .with_state(state);
            let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
            let local = listener.local_addr().expect("local addr");
            addr_tx.send(local).expect("publish addr");
            axum::serve(listener, app)
                .with_graceful_shutdown(async move {
                    let _ = shutdown_rx.await;
                })
                .await
                .expect("serve");
        });
    });

    let addr = addr_rx.recv_timeout(Duration::from_secs(2))?;
    Ok(UpstreamHandle {
        addr,
        shutdown: Some(shutdown_tx),
        thread: Some(handle),
    })
}

// ---------- Caller isolate: drives the retry loop ----------

#[derive(Debug)]
enum CallerMsg {
    /// Kick off the first attempt.
    Begin,
    /// Continuation for one bridged HTTP call.
    HttpReturned(ReqwestCallOutcome),
    /// Wakeup after a retry backoff sleep.
    BackoffElapsed(SleepReply),
}

struct Caller {
    http: ReqwestAddress,
    url: String,
    report: Report,
}

#[tina_runtime::isolate(message = CallerMsg)]
impl Caller {
    fn handle(
        &mut self,
        msg: CallerMsg,
        _ctx: &mut Context<'_, SingleShard, Self::Reply>,
    ) -> Effect<Self> {
        match msg {
            CallerMsg::Begin => self.send_attempt(),
            CallerMsg::HttpReturned(outcome) => self.absorb(outcome),
            // The backoff is plain time. If the sleep was cancelled
            // (e.g., runtime shutdown), bail out instead of issuing
            // another attempt — finishes the trace cleanly.
            CallerMsg::BackoffElapsed(Ok(())) => self.send_attempt(),
            CallerMsg::BackoffElapsed(Err(_)) => self.finish(false),
        }
    }
}

impl Caller {
    fn send_attempt(&mut self) -> Effect<Self> {
        self.report.attempts_made += 1;
        send_request(
            self.http,
            ReqwestRequest::get(&self.url),
            PER_ATTEMPT_TIMEOUT,
        )
        .then(CallerMsg::HttpReturned)
    }

    fn absorb(&mut self, outcome: ReqwestCallOutcome) -> Effect<Self> {
        // the retry classifier: the classifier collapses the six-arm
        // pattern over the layered ReqwestCallOutcome into three
        // typed buckets without losing the layered shape — the raw
        // `outcome` is consumed only after classify() decides which
        // bucket the call belongs in. Bridge-vs-worker timeout, 5xx
        // upstream, and worker transport errors all stay distinct
        // through their typed reason payloads.
        match outcome.classify() {
            ReqwestOutcomeClass::Succeeded(_) => self.finish(true),
            ReqwestOutcomeClass::Transient(_) => {
                self.report.transient_failures += 1;
                if self.report.attempts_made >= MAX_ATTEMPTS {
                    self.finish(false)
                } else {
                    sleep(RETRY_BACKOFF).then(CallerMsg::BackoffElapsed)
                }
            }
            ReqwestOutcomeClass::Fatal(_) => self.finish(false),
        }
    }

    fn finish(&mut self, ok: bool) -> Effect<Self> {
        self.report.final_ok = ok;
        self.report.exit_clean = true;
        stop_with(self.report)
    }
}

// ---------- Run ----------

pub fn run() -> anyhow::Result<Report> {
    let upstream = spawn_flaky_upstream()?;

    let app = LocalSystem::single_shard(SingleShard, DefaultThreadedMailboxFactory)
        .ingress_capacity(8)
        .idle_wait(Duration::from_millis(1))
        .try_build()?;
    let runtime_result = app.run_to_shutdown_reported(Duration::from_secs(5), move |app| {
        run_application(app, upstream.addr)
    });
    let upstream_shutdown = upstream.stop();
    match (runtime_result, upstream_shutdown) {
        (Ok(report), Ok(())) => Ok(report),
        (Err(runtime), Ok(())) => Err(runtime.into()),
        (Ok(_), Err(upstream)) => Err(upstream),
        (Err(runtime), Err(upstream)) => Err(anyhow::anyhow!(
            "Tina runtime shutdown failed: {runtime:#}; upstream shutdown also failed: {upstream:#}"
        )),
    }
}

fn run_application(
    app: &LocalSystem<SingleShard, DefaultThreadedMailboxFactory>,
    upstream_addr: SocketAddr,
) -> anyhow::Result<Report> {
    let bridge = ReqwestWorker::<SingleShard>::install_local(app, ReqwestConfig::default())
        .map_err(|e| anyhow::anyhow!("install reqwest bridge: {e}"))?;

    let caller = Caller {
        http: bridge.address,
        url: format!("http://{upstream_addr}/flaky"),
        report: Report::default(),
    };
    let caller_addr = app
        .register_root::<_, std::convert::Infallible>(caller, 8)
        .map_err(|e| anyhow::anyhow!("register caller: {e:?}"))?;

    let waiter = app
        .observe_result::<Report, _, _>(caller_addr)
        .map_err(|e| anyhow::anyhow!("observe_result: {e:?}"))?;

    app.try_send(caller_addr, CallerMsg::Begin)
        .map_err(|e| anyhow::anyhow!("send Begin: {e:?}"))?;

    let report = waiter
        .wait(Duration::from_secs(10))
        .map_err(|e| anyhow::anyhow!("caller did not finish: {e:?}"))?;

    bridge.closer.close();
    Ok(report)
}
