//! Native Tina performance rows and bounded Tokio comparison rows.
//!
//! These are basic framework workloads, not app specimens. No SQLite,
//! no bridge, no business workflow. Each comparison uses the same op count,
//! same worker count, and same bounded capacity on both sides.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::convert::Infallible;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use axum::Router;
use axum::routing::get;
use tina::CallRejectedReason;
use tina::prelude::*;
use tina_http::{
    HttpListener, HttpListenerMsg, HttpRequest, HttpResponse, HttpServerConfig, StatusCode,
};
use tina_proof_harness::{
    LoadObservation, LoadReport, LoadRun, LoadStop, OpOutcome, PerfAllocationReport,
    PerfComparisonReport, PerfReport, SemanticMatch,
};
use tina_runtime::{
    CallOutcome, DefaultThreadedMailboxFactory, ThreadedRuntime, ThreadedRuntimeConfig,
    ThreadedSendObservedError, call,
};
use tokio::net::TcpListener;
use tokio::runtime::Builder;
use tokio::sync::{mpsc, oneshot};

#[global_allocator]
static GLOBAL_ALLOCATOR: CountingAllocator = CountingAllocator;

thread_local! {
    static COUNT_ALLOCATIONS: Cell<bool> = const { Cell::new(false) };
}

static ALLOCATIONS: AtomicU64 = AtomicU64::new(0);
static ALLOCATED_BYTES: AtomicU64 = AtomicU64::new(0);

// Process-wide allocation counter. Counts every allocation in the whole
// process, regardless of which thread or whether thread-local gating is on.
// Probes that want to see worker-thread allocations (driver mailbox, isolate
// entry, etc.) read this via [`count_process_allocations`].
static PROCESS_ALLOCATIONS: AtomicU64 = AtomicU64::new(0);
static PROCESS_ALLOCATED_BYTES: AtomicU64 = AtomicU64::new(0);

struct CountingAllocator;

unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        // SAFETY: Delegates to the process global allocator with the same layout.
        let ptr = unsafe { System.alloc(layout) };
        if !ptr.is_null() {
            PROCESS_ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
            PROCESS_ALLOCATED_BYTES.fetch_add(layout.size() as u64, Ordering::Relaxed);
            if COUNT_ALLOCATIONS
                .try_with(|enabled| enabled.get())
                .unwrap_or(false)
            {
                ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
                ALLOCATED_BYTES.fetch_add(layout.size() as u64, Ordering::Relaxed);
            }
        }
        ptr
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: The pointer/layout pair came from the same global allocator.
        unsafe { System.dealloc(ptr, layout) };
    }
}

const OPS: u64 = 120;
const HTTP_OPS: u64 = 32;
const WORKERS: usize = 4;
const SAMPLES: usize = 5;
const CAPACITY: usize = OPS as usize + 64;
const CALL_TIMEOUT: Duration = Duration::from_secs(2);
const KEEPALIVE_REQUESTS_PER_CONN: usize = 4;
const FIXED_BODY_BYTES: usize = 4096;

pub fn run_all() -> anyhow::Result<Vec<PerfComparisonReport>> {
    Ok(vec![
        host_enqueue_compare()?,
        observed_admission_compare()?,
        host_call_compare()?,
        service_call_chain_compare()?,
        http1_close_compare()?,
        http1_keepalive_compare()?,
        http1_fixed_body_compare()?,
    ])
}

pub fn host_enqueue_compare() -> anyhow::Result<PerfComparisonReport> {
    compare_samples(
        "host_enqueue",
        tina_host_enqueue_row,
        tokio_host_enqueue_row,
        SemanticMatch::Exact,
        "bounded first-queue handoff only; Tina does not wait for target mailbox truth",
    )
}

pub fn observed_admission_compare() -> anyhow::Result<PerfComparisonReport> {
    compare_samples(
        "observed_admission",
        tina_observed_admission_row,
        tokio_observed_admission_row,
        SemanticMatch::Exact,
        "both sides wait for actor-side receive/admission truth",
    )
}

pub fn host_call_compare() -> anyhow::Result<PerfComparisonReport> {
    compare_samples(
        "host_request_reply",
        tina_host_call_row,
        tokio_host_call_row,
        SemanticMatch::Exact,
        "host thread asks one actor and waits for one reply",
    )
}

pub fn service_call_chain_compare() -> anyhow::Result<PerfComparisonReport> {
    compare_samples(
        "service_request_reply_chain",
        tina_service_call_chain_row,
        tokio_service_call_chain_row,
        SemanticMatch::Exact,
        "host asks service A; service A asks service B before replying",
    )
}

pub fn http1_close_compare() -> anyhow::Result<PerfComparisonReport> {
    compare_samples(
        "http1_close_request",
        tina_http1_close_row,
        tokio_http1_close_row,
        SemanticMatch::Partial,
        "same close-per-request client and bounded load; native tina-http vs axum/hyper",
    )
}

pub fn http1_keepalive_compare() -> anyhow::Result<PerfComparisonReport> {
    compare_samples(
        "http1_keepalive_sequential",
        tina_http1_keepalive_row,
        tokio_http1_keepalive_row,
        SemanticMatch::Partial,
        "same client reuses one connection for four sequential requests; native tina-http vs axum/hyper",
    )
}

pub fn http1_fixed_body_compare() -> anyhow::Result<PerfComparisonReport> {
    compare_samples(
        "http1_fixed_body_close",
        tina_http1_fixed_body_row,
        tokio_http1_fixed_body_row,
        SemanticMatch::Partial,
        "same close-per-request client reads a fixed 4096-byte response body; native tina-http vs axum/hyper",
    )
}

fn compare_samples(
    label: &'static str,
    tina_fn: fn() -> anyhow::Result<PerfReport>,
    baseline_fn: fn() -> anyhow::Result<PerfReport>,
    semantic_match: SemanticMatch,
    mismatch_reason: &'static str,
) -> anyhow::Result<PerfComparisonReport> {
    // Warmup is deliberately ignored. It lets both sides pay one-time runtime,
    // code path, and allocator setup before the reported samples.
    let _ = tina_fn()?;
    let _ = baseline_fn()?;

    let mut tina = Vec::with_capacity(SAMPLES);
    let mut baseline = Vec::with_capacity(SAMPLES);
    for _ in 0..SAMPLES {
        tina.push(tina_fn()?);
        baseline.push(baseline_fn()?);
    }

    Ok(PerfComparisonReport::new(
        label,
        median_report(tina),
        median_report(baseline),
        semantic_match,
        mismatch_reason,
    )
    .with_samples(SAMPLES, "median_p50_after_warmup"))
}

fn median_report(mut reports: Vec<PerfReport>) -> PerfReport {
    reports.sort_by_key(|report| report.load.latency_p50_ns);
    reports.swap_remove(reports.len() / 2)
}

#[derive(Debug, Clone, Copy)]
enum CounterMsg {
    Hit,
}

#[derive(Debug)]
struct Counter {
    count: Arc<AtomicU64>,
}

#[tina_runtime::isolate(message = CounterMsg)]
impl Counter {
    fn handle(
        &mut self,
        msg: CounterMsg,
        _ctx: &mut Context<'_, SingleShard, Self::Reply>,
    ) -> Effect<Self> {
        match msg {
            CounterMsg::Hit => {
                self.count.fetch_add(1, Ordering::Relaxed);
                noop()
            }
        }
    }
}

#[derive(Debug, Clone, Copy)]
enum PingMsg {
    Ping,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PingReply {
    Pong,
}

#[derive(Debug)]
struct Ping;

#[tina_runtime::isolate(message = PingMsg, reply = PingReply)]
impl Ping {
    fn handle(
        &mut self,
        _msg: PingMsg,
        _ctx: &mut Context<'_, SingleShard, Self::Reply>,
    ) -> Effect<Self> {
        noop()
    }

    fn handle_call(&mut self, msg: PingMsg, call: CallContext<'_, Self>) -> Effect<Self> {
        match msg {
            PingMsg::Ping => call.reply(PingReply::Pong),
        }
    }
}

#[derive(Debug)]
enum ChainMsg {
    Run,
    PingReturned(RequestContext<ChainReply>, CallOutcome<PingReply>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ChainReply {
    Done,
}

#[derive(Debug)]
struct ChainService {
    ping: Address<PingMsg, PingReply>,
}

#[tina_runtime::isolate(message = ChainMsg, reply = ChainReply)]
impl ChainService {
    fn handle(
        &mut self,
        msg: ChainMsg,
        _ctx: &mut Context<'_, SingleShard, Self::Reply>,
    ) -> Effect<Self> {
        match msg {
            ChainMsg::Run => noop(),
            ChainMsg::PingReturned(request, CallOutcome::Replied(PingReply::Pong)) => {
                reply_to_request(request, ChainReply::Done)
            }
            ChainMsg::PingReturned(request, _) => reply_to_request(request, ChainReply::Done),
        }
    }

    fn handle_call(&mut self, msg: ChainMsg, call_ctx: CallContext<'_, Self>) -> Effect<Self> {
        match msg {
            ChainMsg::Run => call_ctx
                .defer(call(self.ping, PingMsg::Ping, CALL_TIMEOUT))
                .reply(ChainMsg::PingReturned),
            ChainMsg::PingReturned(_, _) => call_ctx.reject(CallRejectedReason::UnsupportedMessage),
        }
    }
}

#[derive(Debug)]
struct BodyService {
    body: Arc<Vec<u8>>,
}

#[tina_runtime::isolate(message = HttpRequest, reply = HttpResponse)]
impl BodyService {
    fn handle(
        &mut self,
        _request: HttpRequest,
        _ctx: &mut Context<'_, SingleShard, Self::Reply>,
    ) -> Effect<Self> {
        noop()
    }

    fn handle_call(&mut self, _request: HttpRequest, call: CallContext<'_, Self>) -> Effect<Self> {
        call.reply(HttpResponse::with_body(
            StatusCode::OK,
            self.body.as_ref().clone(),
        ))
    }
}

fn tina_host_enqueue_row() -> anyhow::Result<PerfReport> {
    let runtime = new_runtime();
    let count = Arc::new(AtomicU64::new(0));
    let addr = register_counter(&runtime, &count)?;

    runtime
        .send_and_observe(addr, CounterMsg::Hit)
        .map_err(|e| anyhow::anyhow!("warm tina enqueue: {e:?}"))?;
    wait_count(&count, 1);

    let rt = Arc::clone(&runtime);
    let (load, allocations) = run_counted(
        LoadRun {
            workers: WORKERS,
            stop: LoadStop::ops(OPS),
            label: "tina_host_enqueue",
        },
        move |_| match rt.try_send(addr, CounterMsg::Hit) {
            Ok(()) => OpOutcome::Ok,
            Err(_) => OpOutcome::Err { kind: "full" },
        },
        Some({
            let count = Arc::clone(&count);
            move || {
                wait_count(&count, OPS + 1);
                LoadObservation::default()
            }
        }),
    );
    shutdown_runtime(runtime)?;
    Ok(PerfReport::from_load_with_allocations(
        "tina_host_enqueue",
        "host_enqueue",
        load,
        allocations,
    ))
}

fn tokio_host_enqueue_row() -> anyhow::Result<PerfReport> {
    let (tx, handle, stop_tx, count) = start_tokio_counter();
    tx.try_send(()).expect("warm tokio enqueue");
    wait_count(&count, 1);

    let (load, allocations) = run_counted(
        LoadRun {
            workers: WORKERS,
            stop: LoadStop::ops(OPS),
            label: "tokio_host_enqueue",
        },
        move |_| match tx.try_send(()) {
            Ok(()) => OpOutcome::Ok,
            Err(_) => OpOutcome::Err { kind: "full" },
        },
        Some({
            let count = Arc::clone(&count);
            move || {
                wait_count(&count, OPS + 1);
                LoadObservation::default()
            }
        }),
    );
    stop_tokio_counter(stop_tx, handle)?;
    Ok(PerfReport::from_load_with_allocations(
        "tokio_host_enqueue",
        "host_enqueue",
        load,
        allocations,
    ))
}

fn tina_observed_admission_row() -> anyhow::Result<PerfReport> {
    let runtime = new_runtime();
    let count = Arc::new(AtomicU64::new(0));
    let addr = register_counter(&runtime, &count)?;

    runtime
        .send_and_observe(addr, CounterMsg::Hit)
        .map_err(|e| anyhow::anyhow!("warm tina observed send: {e:?}"))?;
    wait_count(&count, 1);

    let rt = Arc::clone(&runtime);
    let (load, allocations) = run_counted(
        LoadRun {
            workers: WORKERS,
            stop: LoadStop::ops(OPS),
            label: "tina_observed_admission",
        },
        move |_| match rt.send_and_observe(addr, CounterMsg::Hit) {
            Ok(()) => OpOutcome::Ok,
            Err(
                ThreadedSendObservedError::IngressFull | ThreadedSendObservedError::MailboxFull,
            ) => OpOutcome::Err { kind: "full" },
            Err(ThreadedSendObservedError::MailboxClosed) => OpOutcome::Err { kind: "closed" },
            Err(ThreadedSendObservedError::WorkerStopped) => OpOutcome::Err { kind: "stopped" },
        },
        Some({
            let count = Arc::clone(&count);
            move || {
                wait_count(&count, OPS + 1);
                LoadObservation::default()
            }
        }),
    );
    shutdown_runtime(runtime)?;
    Ok(PerfReport::from_load_with_allocations(
        "tina_observed_admission",
        "observed_admission",
        load,
        allocations,
    ))
}

fn tokio_observed_admission_row() -> anyhow::Result<PerfReport> {
    let (tx, mut rx) = mpsc::channel::<oneshot::Sender<()>>(CAPACITY);
    let (stop_tx, stop_rx) = oneshot::channel::<()>();
    let handle = thread::spawn(move || {
        let runtime = Builder::new_current_thread().enable_all().build().unwrap();
        runtime.block_on(async move {
            tokio::pin!(stop_rx);
            loop {
                tokio::select! {
                    msg = rx.recv() => {
                        let Some(ack) = msg else { break };
                        let _ = ack.send(());
                    }
                    _ = &mut stop_rx => break,
                }
            }
        });
    });

    let (warm_tx, warm_rx) = oneshot::channel();
    tx.blocking_send(warm_tx).expect("warm tokio observed send");
    warm_rx.blocking_recv().expect("warm tokio observed ack");

    let (load, allocations) = run_counted(
        LoadRun {
            workers: WORKERS,
            stop: LoadStop::ops(OPS),
            label: "tokio_observed_admission",
        },
        move |_| {
            let (ack_tx, ack_rx) = oneshot::channel();
            if tx.blocking_send(ack_tx).is_err() {
                return OpOutcome::Err { kind: "full" };
            }
            match ack_rx.blocking_recv() {
                Ok(()) => OpOutcome::Ok,
                Err(_) => OpOutcome::Err { kind: "closed" },
            }
        },
        None::<fn() -> LoadObservation>,
    );
    let _ = stop_tx.send(());
    handle
        .join()
        .map_err(|_| anyhow::anyhow!("tokio observed worker panicked"))?;
    Ok(PerfReport::from_load_with_allocations(
        "tokio_observed_admission",
        "observed_admission",
        load,
        allocations,
    ))
}

fn tina_host_call_row() -> anyhow::Result<PerfReport> {
    let runtime = new_runtime();
    let addr = runtime
        .register_with_capacity::<_, Infallible>(Ping, CAPACITY)
        .map_err(|e| anyhow::anyhow!("register tina ping: {e:?}"))?;
    assert_eq!(
        runtime.call_blocking(addr, PingMsg::Ping, CALL_TIMEOUT)?,
        CallOutcome::Replied(PingReply::Pong),
    );

    let rt = Arc::clone(&runtime);
    let (load, allocations) = run_counted(
        LoadRun {
            workers: WORKERS,
            stop: LoadStop::ops(OPS),
            label: "tina_host_call",
        },
        move |_| match rt.call_blocking(addr, PingMsg::Ping, CALL_TIMEOUT) {
            Ok(CallOutcome::Replied(PingReply::Pong)) => OpOutcome::Ok,
            Ok(CallOutcome::Full) => OpOutcome::Err { kind: "full" },
            Ok(CallOutcome::Timeout) => OpOutcome::Timeout,
            _ => OpOutcome::Err { kind: "call_error" },
        },
        None::<fn() -> LoadObservation>,
    );
    shutdown_runtime(runtime)?;
    Ok(PerfReport::from_load_with_allocations(
        "tina_host_call",
        "host_call",
        load,
        allocations,
    ))
}

fn tokio_host_call_row() -> anyhow::Result<PerfReport> {
    let (tx, handle, stop_tx) = start_tokio_ping();
    let (warm_tx, warm_rx) = oneshot::channel();
    tx.blocking_send(warm_tx).expect("warm tokio call send");
    warm_rx.blocking_recv().expect("warm tokio call reply");

    let (load, allocations) = run_counted(
        LoadRun {
            workers: WORKERS,
            stop: LoadStop::ops(OPS),
            label: "tokio_host_call",
        },
        move |_| tokio_call_op(&tx),
        None::<fn() -> LoadObservation>,
    );
    let _ = stop_tx.send(());
    handle
        .join()
        .map_err(|_| anyhow::anyhow!("tokio call worker panicked"))?;
    Ok(PerfReport::from_load_with_allocations(
        "tokio_host_call",
        "host_call",
        load,
        allocations,
    ))
}

fn tina_service_call_chain_row() -> anyhow::Result<PerfReport> {
    let runtime = new_runtime();
    let ping = runtime
        .register_with_capacity::<_, Infallible>(Ping, CAPACITY)
        .map_err(|e| anyhow::anyhow!("register tina chain ping: {e:?}"))?;
    let chain = runtime
        .register_with_capacity::<_, Infallible>(ChainService { ping }, CAPACITY)
        .map_err(|e| anyhow::anyhow!("register tina chain service: {e:?}"))?;
    assert_eq!(
        runtime.call_blocking(chain, ChainMsg::Run, CALL_TIMEOUT)?,
        CallOutcome::Replied(ChainReply::Done),
    );

    let rt = Arc::clone(&runtime);
    let (load, allocations) = run_counted(
        LoadRun {
            workers: WORKERS,
            stop: LoadStop::ops(OPS),
            label: "tina_service_call_chain",
        },
        move |_| match rt.call_blocking(chain, ChainMsg::Run, CALL_TIMEOUT) {
            Ok(CallOutcome::Replied(ChainReply::Done)) => OpOutcome::Ok,
            Ok(CallOutcome::Full) => OpOutcome::Err { kind: "full" },
            Ok(CallOutcome::Timeout) => OpOutcome::Timeout,
            _ => OpOutcome::Err { kind: "call_error" },
        },
        None::<fn() -> LoadObservation>,
    );
    shutdown_runtime(runtime)?;
    Ok(PerfReport::from_load_with_allocations(
        "tina_service_call_chain",
        "service_call_chain",
        load,
        allocations,
    ))
}

fn tokio_service_call_chain_row() -> anyhow::Result<PerfReport> {
    let (service_tx, ping_stop, service_stop, ping_handle, service_handle) =
        start_tokio_chain_service();
    let (warm_tx, warm_rx) = oneshot::channel();
    service_tx
        .blocking_send(warm_tx)
        .expect("warm tokio service chain send");
    warm_rx
        .blocking_recv()
        .expect("warm tokio service chain reply");

    let (load, allocations) = run_counted(
        LoadRun {
            workers: WORKERS,
            stop: LoadStop::ops(OPS),
            label: "tokio_service_call_chain",
        },
        move |_| tokio_call_op(&service_tx),
        None::<fn() -> LoadObservation>,
    );
    let _ = service_stop.send(());
    let _ = ping_stop.send(());
    service_handle
        .join()
        .map_err(|_| anyhow::anyhow!("tokio chain service panicked"))?;
    ping_handle
        .join()
        .map_err(|_| anyhow::anyhow!("tokio chain ping panicked"))?;
    Ok(PerfReport::from_load_with_allocations(
        "tokio_service_call_chain",
        "service_call_chain",
        load,
        allocations,
    ))
}

fn tina_http1_close_row() -> anyhow::Result<PerfReport> {
    tina_http_row(
        "tina_http1_close",
        "http1_close",
        small_body(),
        false,
        |addr| http_get(addr, false, 1, small_body().len()),
    )
}

fn tokio_http1_close_row() -> anyhow::Result<PerfReport> {
    tokio_http_row("axum_http1_close", "http1_close", small_body(), |addr| {
        http_get(addr, false, 1, small_body().len())
    })
}

fn tina_http1_keepalive_row() -> anyhow::Result<PerfReport> {
    tina_http_row(
        "tina_http1_keepalive",
        "http1_keepalive",
        small_body(),
        true,
        |addr| http_get(addr, true, KEEPALIVE_REQUESTS_PER_CONN, small_body().len()),
    )
}

fn tokio_http1_keepalive_row() -> anyhow::Result<PerfReport> {
    tokio_http_row(
        "axum_http1_keepalive",
        "http1_keepalive",
        small_body(),
        |addr| http_get(addr, true, KEEPALIVE_REQUESTS_PER_CONN, small_body().len()),
    )
}

fn tina_http1_fixed_body_row() -> anyhow::Result<PerfReport> {
    tina_http_row(
        "tina_http1_fixed_body",
        "http1_fixed_body",
        fixed_body(),
        false,
        |addr| http_get(addr, false, 1, FIXED_BODY_BYTES),
    )
}

fn tokio_http1_fixed_body_row() -> anyhow::Result<PerfReport> {
    tokio_http_row(
        "axum_http1_fixed_body",
        "http1_fixed_body",
        fixed_body(),
        |addr| http_get(addr, false, 1, FIXED_BODY_BYTES),
    )
}

fn tina_http_row(
    label: &'static str,
    kind: &'static str,
    body: Vec<u8>,
    keepalive: bool,
    request: fn(SocketAddr) -> anyhow::Result<()>,
) -> anyhow::Result<PerfReport> {
    let runtime = new_runtime();
    let expected_len = body.len();
    let service = runtime
        .register_with_capacity::<_, Infallible>(
            BodyService {
                body: Arc::new(body),
            },
            CAPACITY,
        )
        .map_err(|e| anyhow::anyhow!("register tina http service: {e:?}"))?;
    let mut config = HttpServerConfig::dev();
    if keepalive {
        config.limits.keepalive_idle_timeout = Some(Duration::from_secs(2));
    }
    let listener = runtime
        .register_with_capacity::<_, Infallible>(
            HttpListener::<SingleShard>::with_config("127.0.0.1:0".parse()?, service, config),
            config.listener_mailbox_capacity,
        )
        .map_err(|e| anyhow::anyhow!("register tina http listener: {e:?}"))?;
    let bound = runtime.observe_next_bound();
    runtime
        .try_send(listener, HttpListenerMsg::Start)
        .map_err(|e| anyhow::anyhow!("start tina http listener: {e:?}"))?;
    let addr = bound
        .wait(Duration::from_secs(2))
        .map_err(|e| anyhow::anyhow!("observe tina http bind: {e:?}"))?;
    http_get(addr, keepalive, if keepalive { 2 } else { 1 }, expected_len)?;

    let (load, allocations) = run_counted(
        LoadRun {
            workers: WORKERS,
            stop: LoadStop::ops(HTTP_OPS),
            label,
        },
        move |_| match request(addr) {
            Ok(()) => OpOutcome::Ok,
            Err(_) => OpOutcome::Err { kind: "http_error" },
        },
        None::<fn() -> LoadObservation>,
    );
    let _ = runtime.try_send(listener, HttpListenerMsg::Stop);
    shutdown_runtime(runtime)?;
    Ok(PerfReport::from_load_with_allocations(
        label,
        kind,
        load,
        allocations,
    ))
}

fn tokio_http_row(
    label: &'static str,
    kind: &'static str,
    body: Vec<u8>,
    request: fn(SocketAddr) -> anyhow::Result<()>,
) -> anyhow::Result<PerfReport> {
    let body = Arc::new(body);
    let (addr_tx, addr_rx) = std::sync::mpsc::channel::<SocketAddr>();
    let (done_tx, done_rx) = std::sync::mpsc::channel::<()>();
    let (stop_tx, stop_rx) = oneshot::channel::<()>();
    let handle = thread::spawn(move || {
        let runtime = Builder::new_current_thread().enable_all().build().unwrap();
        runtime.block_on(async move {
            let app_body = Arc::clone(&body);
            let app = Router::new().route(
                "/",
                get(move || {
                    let body = Arc::clone(&app_body);
                    async move { body.as_ref().clone() }
                }),
            );
            let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind axum");
            addr_tx
                .send(listener.local_addr().expect("axum local addr"))
                .expect("publish axum addr");
            axum::serve(listener, app)
                .with_graceful_shutdown(async move {
                    let _ = stop_rx.await;
                })
                .await
                .expect("serve axum");
            let _ = done_tx.send(());
        });
    });
    let addr = addr_rx.recv_timeout(Duration::from_secs(2))?;
    request(addr)?;
    let (load, allocations) = run_counted(
        LoadRun {
            workers: WORKERS,
            stop: LoadStop::ops(HTTP_OPS),
            label,
        },
        move |_| match request(addr) {
            Ok(()) => OpOutcome::Ok,
            Err(_) => OpOutcome::Err { kind: "http_error" },
        },
        None::<fn() -> LoadObservation>,
    );
    let _ = stop_tx.send(());
    done_rx.recv_timeout(Duration::from_secs(2))?;
    handle
        .join()
        .map_err(|_| anyhow::anyhow!("tokio http worker panicked"))?;
    Ok(PerfReport::from_load_with_allocations(
        label,
        kind,
        load,
        allocations,
    ))
}

fn register_counter(
    runtime: &ThreadedRuntime<SingleShard, DefaultThreadedMailboxFactory>,
    count: &Arc<AtomicU64>,
) -> anyhow::Result<Address<CounterMsg>> {
    runtime
        .register_with_capacity::<_, Infallible>(
            Counter {
                count: Arc::clone(count),
            },
            CAPACITY,
        )
        .map_err(|e| anyhow::anyhow!("register tina counter: {e:?}"))
}

type TokioCounterHandle = (
    mpsc::Sender<()>,
    thread::JoinHandle<()>,
    oneshot::Sender<()>,
    Arc<AtomicU64>,
);

fn start_tokio_counter() -> TokioCounterHandle {
    let (tx, mut rx) = mpsc::channel::<()>(CAPACITY);
    let count = Arc::new(AtomicU64::new(0));
    let worker_count = Arc::clone(&count);
    let (stop_tx, stop_rx) = oneshot::channel::<()>();
    let handle = thread::spawn(move || {
        let runtime = Builder::new_current_thread().enable_all().build().unwrap();
        runtime.block_on(async move {
            tokio::pin!(stop_rx);
            loop {
                tokio::select! {
                    msg = rx.recv() => {
                        if msg.is_some() {
                            worker_count.fetch_add(1, Ordering::Relaxed);
                        } else {
                            break;
                        }
                    }
                    _ = &mut stop_rx => break,
                }
            }
        });
    });
    (tx, handle, stop_tx, count)
}

fn stop_tokio_counter(
    stop_tx: oneshot::Sender<()>,
    handle: thread::JoinHandle<()>,
) -> anyhow::Result<()> {
    let _ = stop_tx.send(());
    handle
        .join()
        .map_err(|_| anyhow::anyhow!("tokio counter worker panicked"))
}

type TokioPingHandle = (
    mpsc::Sender<oneshot::Sender<()>>,
    thread::JoinHandle<()>,
    oneshot::Sender<()>,
);

fn start_tokio_ping() -> TokioPingHandle {
    let (tx, mut rx) = mpsc::channel::<oneshot::Sender<()>>(CAPACITY);
    let (stop_tx, stop_rx) = oneshot::channel::<()>();
    let handle = thread::spawn(move || {
        let runtime = Builder::new_current_thread().enable_all().build().unwrap();
        runtime.block_on(async move {
            tokio::pin!(stop_rx);
            loop {
                tokio::select! {
                    msg = rx.recv() => {
                        let Some(reply) = msg else { break };
                        let _ = reply.send(());
                    }
                    _ = &mut stop_rx => break,
                }
            }
        });
    });
    (tx, handle, stop_tx)
}

type TokioChainHandle = (
    mpsc::Sender<oneshot::Sender<()>>,
    oneshot::Sender<()>,
    oneshot::Sender<()>,
    thread::JoinHandle<()>,
    thread::JoinHandle<()>,
);

fn start_tokio_chain_service() -> TokioChainHandle {
    let (ping_tx, mut ping_rx) = mpsc::channel::<oneshot::Sender<()>>(CAPACITY);
    let (ping_stop_tx, ping_stop_rx) = oneshot::channel::<()>();
    let ping_handle = thread::spawn(move || {
        let runtime = Builder::new_current_thread().enable_all().build().unwrap();
        runtime.block_on(async move {
            tokio::pin!(ping_stop_rx);
            loop {
                tokio::select! {
                    msg = ping_rx.recv() => {
                        let Some(reply) = msg else { break };
                        let _ = reply.send(());
                    }
                    _ = &mut ping_stop_rx => break,
                }
            }
        });
    });

    let (service_tx, mut service_rx) = mpsc::channel::<oneshot::Sender<()>>(CAPACITY);
    let (service_stop_tx, service_stop_rx) = oneshot::channel::<()>();
    let service_handle = thread::spawn(move || {
        let runtime = Builder::new_current_thread().enable_all().build().unwrap();
        runtime.block_on(async move {
            tokio::pin!(service_stop_rx);
            loop {
                tokio::select! {
                    msg = service_rx.recv() => {
                        let Some(reply) = msg else { break };
                        let (inner_tx, inner_rx) = oneshot::channel();
                        if ping_tx.send(inner_tx).await.is_err() {
                            let _ = reply.send(());
                            continue;
                        }
                        let _ = inner_rx.await;
                        let _ = reply.send(());
                    }
                    _ = &mut service_stop_rx => break,
                }
            }
        });
    });

    (
        service_tx,
        ping_stop_tx,
        service_stop_tx,
        ping_handle,
        service_handle,
    )
}

fn tokio_call_op(tx: &mpsc::Sender<oneshot::Sender<()>>) -> OpOutcome {
    let (reply_tx, reply_rx) = oneshot::channel();
    if tx.blocking_send(reply_tx).is_err() {
        return OpOutcome::Err { kind: "full" };
    }
    match reply_rx.blocking_recv() {
        Ok(()) => OpOutcome::Ok,
        Err(_) => OpOutcome::Err { kind: "closed" },
    }
}

fn run_counted<F, O>(
    run: LoadRun,
    op: F,
    observation: Option<O>,
) -> (LoadReport, PerfAllocationReport)
where
    F: Fn(usize) -> OpOutcome + Send + Sync + 'static,
    O: FnOnce() -> LoadObservation,
{
    reset_allocations();
    let load = tina_proof_harness::load::run_with_observation(
        run,
        move |worker_id| counted_allocations(|| op(worker_id)),
        observation,
    );
    (load, finish_allocations("load_worker_op"))
}

/// Runs `f` on the calling thread and returns its result plus the number of
/// allocations the global counting allocator saw during the call.
///
/// Host-thread scope only: work the runtime does on its own worker thread is
/// not counted here. That is the honest scope for the host-side per-op cost a
/// caller pays (channel + boxed command per `call_blocking` / observed send).
pub fn count_host_allocations<T>(f: impl FnOnce() -> T) -> (T, u64) {
    reset_allocations();
    let result = counted_allocations(f);
    (result, ALLOCATIONS.load(Ordering::Relaxed))
}

/// Runs `f` and returns its result plus the number of allocations the whole
/// process saw during the call — every thread, no gate.
///
/// Use this to surface allocations the runtime's worker thread makes on a
/// caller's behalf (driver mailbox, isolate entry, etc.) which the host-only
/// counter misses. Run probes one at a time; concurrent measurements will
/// share the counter and contaminate each other.
pub fn count_process_allocations<T>(f: impl FnOnce() -> T) -> (T, u64) {
    let before = PROCESS_ALLOCATIONS.load(Ordering::Relaxed);
    let result = f();
    let after = PROCESS_ALLOCATIONS.load(Ordering::Relaxed);
    (result, after.saturating_sub(before))
}

/// Runs `f` once and returns its result plus both allocation counts for that
/// same call: `(result, host_allocations, process_allocations)`. Run probes
/// one at a time; the process counter is shared across threads.
pub fn count_all_allocations<T>(f: impl FnOnce() -> T) -> (T, u64, u64) {
    reset_allocations();
    let process_before = PROCESS_ALLOCATIONS.load(Ordering::Relaxed);
    let result = counted_allocations(f);
    let process_after = PROCESS_ALLOCATIONS.load(Ordering::Relaxed);
    let host = ALLOCATIONS.load(Ordering::Relaxed);
    (result, host, process_after.saturating_sub(process_before))
}

fn counted_allocations<T>(f: impl FnOnce() -> T) -> T {
    COUNT_ALLOCATIONS.with(|enabled| {
        let previous = enabled.replace(true);
        let result = f();
        enabled.set(previous);
        result
    })
}

fn reset_allocations() {
    ALLOCATIONS.store(0, Ordering::Relaxed);
    ALLOCATED_BYTES.store(0, Ordering::Relaxed);
}

fn finish_allocations(scope: &'static str) -> PerfAllocationReport {
    PerfAllocationReport {
        scope,
        allocations: ALLOCATIONS.load(Ordering::Relaxed),
        allocated_bytes: ALLOCATED_BYTES.load(Ordering::Relaxed),
    }
}

fn small_body() -> Vec<u8> {
    b"ok\n".to_vec()
}

fn fixed_body() -> Vec<u8> {
    vec![b'x'; FIXED_BODY_BYTES]
}

fn http_get(
    addr: SocketAddr,
    keepalive: bool,
    requests: usize,
    expected_body_len: usize,
) -> anyhow::Result<()> {
    let mut stream = TcpStream::connect_timeout(&addr, Duration::from_secs(2))?;
    stream.set_read_timeout(Some(Duration::from_secs(2)))?;
    for i in 0..requests {
        let close = !keepalive || i + 1 == requests;
        let connection = if close { "close" } else { "keep-alive" };
        stream.write_all(
            format!("GET / HTTP/1.1\r\nHost: x\r\nConnection: {connection}\r\n\r\n").as_bytes(),
        )?;
        stream.flush()?;
        read_one_response(&mut stream, expected_body_len)?;
    }
    Ok(())
}

fn read_one_response(stream: &mut TcpStream, expected_body_len: usize) -> anyhow::Result<()> {
    let mut response = Vec::with_capacity(256 + expected_body_len);
    let head_end = loop {
        let mut byte = [0; 1];
        let n = stream.read(&mut byte)?;
        if n == 0 {
            anyhow::bail!("peer closed before response head");
        }
        response.push(byte[0]);
        if let Some(pos) = response.windows(4).position(|w| w == b"\r\n\r\n") {
            break pos + 4;
        }
        if response.len() > 64 * 1024 {
            anyhow::bail!("response head too large");
        }
    };
    let head = std::str::from_utf8(&response[..head_end])?;
    if !head.starts_with("HTTP/1.1 200") {
        anyhow::bail!("unexpected response head: {head:?}");
    }
    let content_length = parse_content_length(head)?;
    if content_length != expected_body_len {
        anyhow::bail!("unexpected content length {content_length}, expected {expected_body_len}");
    }
    while response.len() - head_end < content_length {
        let mut buf = [0; 4096];
        let n = stream.read(&mut buf)?;
        if n == 0 {
            anyhow::bail!("peer closed before full body");
        }
        response.extend_from_slice(&buf[..n]);
    }
    Ok(())
}

fn parse_content_length(head: &str) -> anyhow::Result<usize> {
    for line in head.lines() {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        if name.eq_ignore_ascii_case("content-length") {
            return value
                .trim()
                .parse::<usize>()
                .map_err(|e| anyhow::anyhow!("bad content-length {value:?}: {e}"));
        }
    }
    anyhow::bail!("missing content-length")
}

fn wait_count(count: &AtomicU64, expected: u64) {
    let deadline = Instant::now() + Duration::from_secs(2);
    while count.load(Ordering::Relaxed) < expected {
        assert!(
            Instant::now() <= deadline,
            "perf row timed out current={} expected={expected}",
            count.load(Ordering::Relaxed)
        );
        thread::yield_now();
    }
}

fn new_runtime() -> Arc<ThreadedRuntime<SingleShard, DefaultThreadedMailboxFactory>> {
    Arc::new(ThreadedRuntime::with_config(
        SingleShard,
        DefaultThreadedMailboxFactory,
        ThreadedRuntimeConfig {
            command_capacity: CAPACITY,
            ..ThreadedRuntimeConfig::default()
        },
    ))
}

fn shutdown_runtime(
    runtime: Arc<ThreadedRuntime<SingleShard, DefaultThreadedMailboxFactory>>,
) -> anyhow::Result<()> {
    match Arc::try_unwrap(runtime) {
        Ok(runtime) => {
            runtime
                .shutdown()
                .map_err(|e| anyhow::anyhow!("shutdown tina runtime: {e:?}"))?;
            Ok(())
        }
        Err(_) => anyhow::bail!("runtime still shared after perf row"),
    }
}
