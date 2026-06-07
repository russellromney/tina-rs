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
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use axum::Router;
use axum::routing::get;
use tina::CallRejectedReason;
use tina::capacity::{CapacityMode, CapacitySurfaceReport};
use tina::prelude::*;
use tina_http::{
    BodyMetrics, GrpcLimits, GrpcRequest, GrpcResponse, GrpcRouter, GrpcRouterMsg, Http2Listener,
    Http2ListenerMsg, Http2ServerConfig, HttpLimits, HttpListener, HttpListenerAddress,
    HttpListenerMsg, HttpRequest, HttpResponse, HttpServerConfig, StatusCode, WebSocketLimits,
    WebSocketSessionMsg, WebSocketSessionOutcome, grpc_unary_call_h2c_blocking, websocket_upgrade,
};
use tina_proof_harness::{
    LoadObservation, LoadReport, LoadRun, LoadStop, OpOutcome, PerfAllocationReport,
    PerfComparisonReport, PerfReport, SemanticMatch, SurfacePlateau, UnavailableSurface,
};
use tina_runtime::{
    CallOutcome, DefaultThreadedMailboxFactory, ServicePressureReport, ThreadedRuntime,
    ThreadedRuntimeConfig, ThreadedSendObservedError, call,
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
        http1_keepalive_steady_state_small_compare()?,
        http1_keepalive_steady_state_fixed_compare()?,
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

pub fn http1_keepalive_steady_state_small_compare() -> anyhow::Result<PerfComparisonReport> {
    compare_samples(
        "http1_keepalive_steady_state_small",
        tina_http1_keepalive_steady_state_small_row,
        tokio_http1_keepalive_steady_state_small_row,
        SemanticMatch::Partial,
        "same warmed keepalive stream per load worker; native tina-http vs axum/hyper",
    )
}

pub fn http1_keepalive_steady_state_fixed_compare() -> anyhow::Result<PerfComparisonReport> {
    compare_samples(
        "http1_keepalive_steady_state_fixed",
        tina_http1_keepalive_steady_state_fixed_row,
        tokio_http1_keepalive_steady_state_fixed_row,
        SemanticMatch::Partial,
        "same warmed keepalive stream per load worker with 4096-byte body; native tina-http vs axum/hyper",
    )
}

pub fn http_body_pressure_probe() -> anyhow::Result<LoadReport> {
    const MAX_BODY_BYTES: usize = 16;
    const TOO_LARGE_BODY_BYTES: usize = 999;
    const PRESSURE_OPS: u64 = 8;

    let runtime = new_runtime();
    let metrics =
        BodyMetrics::with_body_capacity("perf.http.bodies", MAX_BODY_BYTES, MAX_BODY_BYTES);
    let service = runtime
        .register_with_capacity::<_, Infallible>(
            BodyService {
                body: Arc::new(small_body()),
            },
            CAPACITY,
        )
        .map_err(|e| anyhow::anyhow!("register tina http pressure service: {e:?}"))?;
    let config = HttpServerConfig {
        limits: HttpLimits {
            max_body_bytes: MAX_BODY_BYTES,
            ..HttpLimits::default()
        },
        ..HttpServerConfig::dev()
    };
    let listener = runtime
        .register_with_capacity::<_, Infallible>(
            HttpListener::<SingleShard>::with_config("127.0.0.1:0".parse()?, service, config)
                .with_metrics(metrics.clone()),
            config.listener_mailbox_capacity,
        )
        .map_err(|e| anyhow::anyhow!("register tina http pressure listener: {e:?}"))?;
    let bound = runtime.observe_next_bound();
    runtime
        .try_send(listener, HttpListenerMsg::Start)
        .map_err(|e| anyhow::anyhow!("start tina http pressure listener: {e:?}"))?;
    let addr = bound
        .wait(Duration::from_secs(2))
        .map_err(|e| anyhow::anyhow!("observe tina http pressure bind: {e:?}"))?;

    let (load, _allocations) = run_counted(
        LoadRun {
            workers: 1,
            stop: LoadStop::ops(PRESSURE_OPS),
            label: "tina_http_body_pressure",
        },
        move |_| match http_post_declared_too_large(addr, TOO_LARGE_BODY_BYTES) {
            Ok(()) => OpOutcome::Err { kind: "full" },
            Err(_) => OpOutcome::Err { kind: "http_error" },
        },
        Some({
            let metrics = metrics.clone();
            move || body_pressure_observation(&metrics, MAX_BODY_BYTES)
        }),
    );

    let _ = runtime.try_send(listener, HttpListenerMsg::Stop);
    shutdown_runtime(runtime)?;
    Ok(load)
}

fn body_pressure_observation(metrics: &BodyMetrics, max_body_bytes: usize) -> LoadObservation {
    let snapshot = metrics.snapshot();
    let mut report = ServicePressureReport::new("perf.http.body_pressure");
    if let Some(request) =
        snapshot.request_capacity_report("perf.http.request_body", CapacityMode::Fixed)
    {
        let high_water = request.high_water_weight.unwrap_or(0);
        report.add_measured("body", request);
        report.add_measured(
            "body",
            CapacitySurfaceReport::weighted(
                "perf.http.max_body_bytes",
                CapacityMode::Fixed,
                max_body_bytes,
                0,
                high_water,
                snapshot.body_full_count,
                "bytes",
            ),
        );
    } else {
        report.add_unavailable(
            "perf.http.request_body",
            "body",
            "body metrics were not configured",
        );
    }
    LoadObservation {
        leak_clean: snapshot.drained(),
        surface_plateaus: SurfacePlateau::from_service_pressure(&report),
        unavailable_surfaces: UnavailableSurface::from_service_pressure(&report),
        ..LoadObservation::default()
    }
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

fn tina_http1_keepalive_steady_state_small_row() -> anyhow::Result<PerfReport> {
    tina_http_steady_state_row(
        "tina_http1_keepalive_steady_state_small",
        "http1_keepalive_steady_state_small",
        small_body(),
    )
}

fn tokio_http1_keepalive_steady_state_small_row() -> anyhow::Result<PerfReport> {
    tokio_http_steady_state_row(
        "axum_http1_keepalive_steady_state_small",
        "http1_keepalive_steady_state_small",
        small_body(),
    )
}

fn tina_http1_keepalive_steady_state_fixed_row() -> anyhow::Result<PerfReport> {
    tina_http_steady_state_row(
        "tina_http1_keepalive_steady_state_fixed",
        "http1_keepalive_steady_state_fixed",
        fixed_body(),
    )
}

fn tokio_http1_keepalive_steady_state_fixed_row() -> anyhow::Result<PerfReport> {
    tokio_http_steady_state_row(
        "axum_http1_keepalive_steady_state_fixed",
        "http1_keepalive_steady_state_fixed",
        fixed_body(),
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

    let process_before = ProcessSnapshot::now();
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
    let (allocs_delta, allocated_bytes_delta, rss_delta) =
        ProcessSnapshot::now().delta_from(process_before);
    print_process_metrics(label, allocs_delta, allocated_bytes_delta, rss_delta);
    let _ = runtime.try_send(listener, HttpListenerMsg::Stop);
    shutdown_runtime(runtime)?;
    Ok(PerfReport::from_load_with_allocations(
        label,
        kind,
        load,
        allocations,
    ))
}

fn tina_http_steady_state_row(
    label: &'static str,
    kind: &'static str,
    body: Vec<u8>,
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
        .map_err(|e| anyhow::anyhow!("register tina steady http service: {e:?}"))?;
    let mut config = HttpServerConfig::dev();
    config.limits.keepalive_idle_timeout = Some(Duration::from_secs(2));
    let listener = runtime
        .register_with_capacity::<_, Infallible>(
            HttpListener::<SingleShard>::with_config("127.0.0.1:0".parse()?, service, config),
            config.listener_mailbox_capacity,
        )
        .map_err(|e| anyhow::anyhow!("register tina steady http listener: {e:?}"))?;
    let bound = runtime.observe_next_bound();
    runtime
        .try_send(listener, HttpListenerMsg::Start)
        .map_err(|e| anyhow::anyhow!("start tina steady http listener: {e:?}"))?;
    let addr = bound
        .wait(Duration::from_secs(2))
        .map_err(|e| anyhow::anyhow!("observe tina steady http bind: {e:?}"))?;
    let report = http_steady_state_load(label, kind, addr, expected_len)?;
    let _ = runtime.try_send(listener, HttpListenerMsg::Stop);
    shutdown_runtime(runtime)?;
    Ok(report)
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
    let process_before = ProcessSnapshot::now();
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
    let (allocs_delta, allocated_bytes_delta, rss_delta) =
        ProcessSnapshot::now().delta_from(process_before);
    print_process_metrics(label, allocs_delta, allocated_bytes_delta, rss_delta);
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

fn tokio_http_steady_state_row(
    label: &'static str,
    kind: &'static str,
    body: Vec<u8>,
) -> anyhow::Result<PerfReport> {
    let expected_len = body.len();
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
    let report = http_steady_state_load(label, kind, addr, expected_len)?;
    let _ = stop_tx.send(());
    done_rx.recv_timeout(Duration::from_secs(2))?;
    handle
        .join()
        .map_err(|_| anyhow::anyhow!("tokio steady http worker panicked"))?;
    Ok(report)
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

/// Resident set size in kilobytes for the calling process, from `getrusage`.
/// Macros into 0 if the call fails (it shouldn't; the syscall is a hard
/// requirement of POSIX). On macOS `ru_maxrss` is reported in bytes; on
/// Linux it is kilobytes — we normalise both to kilobytes here.
pub fn rss_kb_now() -> u64 {
    // SAFETY: `getrusage` writes into a caller-owned `rusage`; zero-init is
    // valid.
    let mut usage: libc::rusage = unsafe { std::mem::zeroed() };
    let rc = unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut usage) };
    if rc != 0 {
        return 0;
    }
    #[cfg(target_os = "macos")]
    {
        // macOS reports bytes — convert to kilobytes.
        (usage.ru_maxrss as u64) / 1024
    }
    #[cfg(not(target_os = "macos"))]
    {
        // Linux + most other unices already report kilobytes.
        usage.ru_maxrss as u64
    }
}

/// Snapshot of "what the whole process has done so far" — total
/// allocations and current RSS. Take one before a measured region and one
/// after; the deltas are the region's true process cost.
#[derive(Debug, Clone, Copy)]
pub struct ProcessSnapshot {
    pub allocations: u64,
    pub allocated_bytes: u64,
    pub rss_kb: u64,
}

impl ProcessSnapshot {
    pub fn now() -> Self {
        Self {
            allocations: PROCESS_ALLOCATIONS.load(Ordering::Relaxed),
            allocated_bytes: PROCESS_ALLOCATED_BYTES.load(Ordering::Relaxed),
            rss_kb: rss_kb_now(),
        }
    }

    /// Returns `(allocations_delta, allocated_bytes_delta, rss_delta_kb)`. RSS
    /// can go down, so the delta is signed; allocation counters are monotone.
    pub fn delta_from(&self, before: ProcessSnapshot) -> (u64, u64, i64) {
        (
            self.allocations.saturating_sub(before.allocations),
            self.allocated_bytes.saturating_sub(before.allocated_bytes),
            (self.rss_kb as i64) - (before.rss_kb as i64),
        )
    }
}

/// Prints one extra `perf-process` line for a row, capturing the
/// whole-process cost the existing thread-local-gated `tina_allocations`
/// number misses (server-thread allocations, RSS growth). Stable
/// grep-friendly key=value shape so historical tracking can scrape it.
pub fn print_process_metrics(
    label: &str,
    allocations_delta: u64,
    allocated_bytes_delta: u64,
    rss_delta_kb: i64,
) {
    println!(
        "perf-process label={label} process_allocations={allocations_delta} process_allocated_bytes={allocated_bytes_delta} rss_delta_kb={rss_delta_kb}"
    );
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
    let close_request = b"GET / HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n";
    let keepalive_request = b"GET / HTTP/1.1\r\nHost: x\r\nConnection: keep-alive\r\n\r\n";
    for i in 0..requests {
        let close = !keepalive || i + 1 == requests;
        let request = if close {
            close_request.as_slice()
        } else {
            keepalive_request.as_slice()
        };
        stream.write_all(request)?;
        stream.flush()?;
        read_one_response(&mut stream, expected_body_len)?;
    }
    Ok(())
}

fn http_steady_state_load(
    label: &'static str,
    kind: &'static str,
    addr: SocketAddr,
    expected_body_len: usize,
) -> anyhow::Result<PerfReport> {
    let mut streams = Vec::with_capacity(WORKERS);
    for _ in 0..WORKERS {
        let mut stream = TcpStream::connect_timeout(&addr, Duration::from_secs(2))?;
        stream.set_read_timeout(Some(Duration::from_secs(2)))?;
        http_get_on_stream(&mut stream, false, expected_body_len)?;
        streams.push(Mutex::new(Some(stream)));
    }
    let streams = Arc::new(streams);
    let process_before = ProcessSnapshot::now();
    let (load, allocations) = {
        let streams = Arc::clone(&streams);
        run_counted(
            LoadRun {
                workers: WORKERS,
                stop: LoadStop::ops(HTTP_OPS),
                label,
            },
            move |worker_id| {
                let Some(slot) = streams.get(worker_id) else {
                    return OpOutcome::Err {
                        kind: "missing_worker_stream",
                    };
                };
                let mut guard = slot.lock().expect("steady stream lock");
                let Some(stream) = guard.as_mut() else {
                    return OpOutcome::Err {
                        kind: "closed_worker_stream",
                    };
                };
                match http_get_on_stream(stream, false, expected_body_len) {
                    Ok(()) => OpOutcome::Ok,
                    Err(_) => OpOutcome::Err { kind: "http_error" },
                }
            },
            None::<fn() -> LoadObservation>,
        )
    };
    let (allocs_delta, allocated_bytes_delta, rss_delta) =
        ProcessSnapshot::now().delta_from(process_before);
    print_process_metrics(label, allocs_delta, allocated_bytes_delta, rss_delta);
    for slot in streams.iter() {
        if let Some(mut stream) = slot.lock().expect("steady stream lock").take() {
            let _ = http_get_on_stream(&mut stream, true, expected_body_len);
        }
    }
    Ok(PerfReport::from_load_with_allocations(
        label,
        kind,
        load,
        allocations,
    ))
}

fn http_get_on_stream(
    stream: &mut TcpStream,
    close: bool,
    expected_body_len: usize,
) -> anyhow::Result<()> {
    let request = if close {
        b"GET / HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n".as_slice()
    } else {
        b"GET / HTTP/1.1\r\nHost: x\r\nConnection: keep-alive\r\n\r\n".as_slice()
    };
    stream.write_all(request)?;
    stream.flush()?;
    read_one_response(stream, expected_body_len)
}

fn http_post_declared_too_large(addr: SocketAddr, declared_len: usize) -> anyhow::Result<()> {
    let mut stream = TcpStream::connect_timeout(&addr, Duration::from_secs(2))?;
    stream.set_read_timeout(Some(Duration::from_secs(2)))?;
    let request = format!(
        "POST / HTTP/1.1\r\nHost: x\r\nConnection: close\r\nContent-Length: {declared_len}\r\n\r\n"
    );
    stream.write_all(request.as_bytes())?;
    stream.flush()?;
    let mut response = Vec::new();
    stream.read_to_end(&mut response)?;
    let head = std::str::from_utf8(&response)?;
    if !head.starts_with("HTTP/1.1 413") {
        anyhow::bail!("expected 413 for declared-too-large body, got {head:?}");
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

// ---------------------------------------------------------------------------
// Native protocol rows (HTTP/2 h2c, WebSocket).
//
// These are Tina-only rows (`comparison_baseline=none`): a fair hyper/tonic or
// tungstenite baseline would dwarf the row and make semantic equality a lie, so
// the plan keeps the first form Tina-only. Each row drives the *real* Tina
// server isolate (Http2Listener / HttpListener+WebSocket gateway) over a raw
// socket client, exactly like the HTTP/1 rows drive the real server over a raw
// `TcpStream`. Allocation counts include the raw client work inside the row
// op; process rows include both client and server work. Treat them as
// whole-operation evidence, not server-only allocation proof.
//
// `kind` carries the setup-vs-reuse class so connection setup cost is never
// silently mixed with steady-state service cost:
//   - `connection_setup`           one fresh connection per op
//   - `connection_setup_amortized` one fresh connection, several requests
//   - `steady_state_reuse`         warmed connection reused across ops
// ---------------------------------------------------------------------------

const PROTOCOL_OPS: u64 = HTTP_OPS;
const H2_KEEPALIVE_REQUESTS_PER_CONN: usize = KEEPALIVE_REQUESTS_PER_CONN;
// Generous client-side socket timeout for the native protocol rows. These rows
// run four raw clients against one single-shard worker, so tails are wide; the
// timeout only exists to fail a genuine hang, not to clip a slow-but-real op on
// a contended machine. Kept well above the worst observed tail.
const PROTOCOL_CLIENT_TIMEOUT: Duration = Duration::from_secs(5);

type PerfRuntime = Arc<ThreadedRuntime<SingleShard, DefaultThreadedMailboxFactory>>;

/// All Tina-only native protocol rows. Returned as `PerfReport`s (not
/// comparisons): each prints a `perf ...` line with `comparison_baseline=none`.
///
/// Each row is sampled the same way the comparison rows are — one warmup run
/// discarded, then the median-of-`SAMPLES` by p50 — so the reported row matches
/// the suite's documented methodology instead of being a single noisy run.
pub fn run_native_rows() -> anyhow::Result<Vec<PerfReport>> {
    Ok(vec![
        native_sampled(http2_h2c_close_row)?,
        native_sampled(http2_h2c_keepalive_row)?,
        native_sampled(http2_h2c_steady_state_small_row)?,
        native_sampled(grpc_h2c_unary_close_row)?,
        native_sampled(websocket_open_close_row)?,
        native_sampled(websocket_text_round_trip_row)?,
        native_sampled(websocket_steady_state_small_row)?,
    ])
}

/// Warmup-then-median-of-five for a native row, mirroring `compare_samples`.
/// The warmup run lets one-time runtime/allocator setup happen before the
/// measured samples; `median_report` then picks the median-p50 sample.
fn native_sampled(row: fn() -> anyhow::Result<PerfReport>) -> anyhow::Result<PerfReport> {
    let _ = row()?;
    let mut reports = Vec::with_capacity(SAMPLES);
    for _ in 0..SAMPLES {
        reports.push(row()?);
    }
    Ok(median_report(reports).with_samples(SAMPLES, "median_p50_after_warmup"))
}

// ---- HTTP/2 (h2c) ----------------------------------------------------------

const H2_PREFACE: &[u8] = b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n";
const H2_FRAME_DATA: u8 = 0x0;
const H2_FRAME_HEADERS: u8 = 0x1;
const H2_FRAME_RST_STREAM: u8 = 0x3;
const H2_FRAME_SETTINGS: u8 = 0x4;
const H2_FLAG_ACK: u8 = 0x1;
const H2_FLAG_END_STREAM: u8 = 0x1;
const H2_FLAG_END_HEADERS: u8 = 0x4;

struct H2Frame {
    ty: u8,
    flags: u8,
    stream_id: u32,
    payload: Vec<u8>,
}

fn start_h2_server(
    body: Vec<u8>,
) -> anyhow::Result<(PerfRuntime, Address<Http2ListenerMsg>, SocketAddr)> {
    let runtime = new_runtime();
    let service = runtime
        .register_with_capacity::<_, Infallible>(
            BodyService {
                body: Arc::new(body),
            },
            CAPACITY,
        )
        .map_err(|e| anyhow::anyhow!("register tina http2 service: {e:?}"))?;
    let config = Http2ServerConfig::dev();
    let listener = runtime
        .register_with_capacity::<Http2Listener<SingleShard>, _>(
            Http2Listener::<SingleShard>::new("127.0.0.1:0".parse()?, service, config),
            config.listener_mailbox_capacity,
        )
        .map_err(|e| anyhow::anyhow!("register tina http2 listener: {e:?}"))?;
    let bound = runtime.observe_next_bound();
    runtime
        .try_send(listener, Http2ListenerMsg::Start)
        .map_err(|e| anyhow::anyhow!("start tina http2 listener: {e:?}"))?;
    let addr = bound
        .wait(Duration::from_secs(2))
        .map_err(|e| anyhow::anyhow!("observe tina http2 bind: {e:?}"))?;
    Ok((runtime, listener, addr))
}

fn h2c_connect(addr: SocketAddr) -> anyhow::Result<TcpStream> {
    let mut stream = TcpStream::connect_timeout(&addr, PROTOCOL_CLIENT_TIMEOUT)?;
    stream.set_read_timeout(Some(PROTOCOL_CLIENT_TIMEOUT))?;
    stream.set_write_timeout(Some(PROTOCOL_CLIENT_TIMEOUT))?;
    stream.write_all(H2_PREFACE)?;
    h2_write_frame(&mut stream, H2_FRAME_SETTINGS, 0, 0, &[])?;
    let mut saw_settings = false;
    let mut saw_ack = false;
    for _ in 0..6 {
        let frame = h2_read_frame(&mut stream)?;
        if frame.ty == H2_FRAME_SETTINGS && frame.flags & H2_FLAG_ACK == 0 {
            saw_settings = true;
            h2_write_frame(&mut stream, H2_FRAME_SETTINGS, H2_FLAG_ACK, 0, &[])?;
        } else if frame.ty == H2_FRAME_SETTINGS && frame.flags & H2_FLAG_ACK != 0 {
            saw_ack = true;
        }
        if saw_settings && saw_ack {
            return Ok(stream);
        }
    }
    anyhow::bail!("h2c settings handshake did not complete")
}

fn h2_write_frame(
    stream: &mut TcpStream,
    ty: u8,
    flags: u8,
    stream_id: u32,
    payload: &[u8],
) -> anyhow::Result<()> {
    let len = payload.len();
    let mut out = Vec::with_capacity(9 + len);
    out.push(((len >> 16) & 0xff) as u8);
    out.push(((len >> 8) & 0xff) as u8);
    out.push((len & 0xff) as u8);
    out.push(ty);
    out.push(flags);
    out.extend_from_slice(&(stream_id & 0x7fff_ffff).to_be_bytes());
    out.extend_from_slice(payload);
    stream.write_all(&out)?;
    stream.flush()?;
    Ok(())
}

fn h2_read_frame(stream: &mut TcpStream) -> anyhow::Result<H2Frame> {
    let mut head = [0u8; 9];
    stream.read_exact(&mut head)?;
    let len = ((head[0] as usize) << 16) | ((head[1] as usize) << 8) | head[2] as usize;
    let mut payload = vec![0u8; len];
    stream.read_exact(&mut payload)?;
    let mut sid = [0u8; 4];
    sid.copy_from_slice(&head[5..9]);
    Ok(H2Frame {
        ty: head[3],
        flags: head[4],
        stream_id: u32::from_be_bytes(sid) & 0x7fff_ffff,
        payload,
    })
}

fn h2_request_block(path: &str) -> Vec<u8> {
    // Minimal literal-never-indexed HPACK block, the same shape the live h2c
    // test client uses. Enough for the server's HPACK decoder; no dynamic table.
    let mut block = Vec::new();
    h2_literal(":method", "GET", &mut block);
    h2_literal(":scheme", "http", &mut block);
    h2_literal(":path", path, &mut block);
    h2_literal(":authority", "localhost", &mut block);
    block
}

fn h2_literal(name: &str, value: &str, out: &mut Vec<u8>) {
    out.push(0);
    h2_hpack_string(name, out);
    h2_hpack_string(value, out);
}

fn h2_hpack_string(value: &str, out: &mut Vec<u8>) {
    assert!(value.len() < 127);
    out.push(value.len() as u8);
    out.extend_from_slice(value.as_bytes());
}

/// One GET on an existing h2c connection: send HEADERS(END_STREAM), read frames
/// for `stream_id` until END_STREAM, assert the reassembled body length matches
/// and no RST_STREAM landed.
fn h2c_get(
    stream: &mut TcpStream,
    stream_id: u32,
    path: &str,
    expected_len: usize,
) -> anyhow::Result<()> {
    let block = h2_request_block(path);
    h2_write_frame(
        stream,
        H2_FRAME_HEADERS,
        H2_FLAG_END_HEADERS | H2_FLAG_END_STREAM,
        stream_id,
        &block,
    )?;
    let mut body = 0usize;
    let mut saw_headers = false;
    let mut ended = false;
    for _ in 0..32 {
        let frame = h2_read_frame(stream)?;
        if frame.stream_id != stream_id {
            continue;
        }
        match frame.ty {
            H2_FRAME_HEADERS => {
                saw_headers = true;
                if frame.flags & H2_FLAG_END_STREAM != 0 {
                    ended = true;
                    break;
                }
            }
            H2_FRAME_DATA => {
                body += frame.payload.len();
                if frame.flags & H2_FLAG_END_STREAM != 0 {
                    ended = true;
                    break;
                }
            }
            H2_FRAME_RST_STREAM => anyhow::bail!("h2 stream {stream_id} reset"),
            _ => {}
        }
    }
    // A valid response must carry a HEADERS frame (the status block) and reach
    // END_STREAM. Length-only checks would let a malformed/headerless or
    // truncated response pass; require both.
    if !saw_headers {
        anyhow::bail!("h2 stream {stream_id} produced no HEADERS frame");
    }
    if !ended {
        anyhow::bail!("h2 stream {stream_id} did not reach END_STREAM");
    }
    if body != expected_len {
        anyhow::bail!("unexpected h2 body len {body}, expected {expected_len}");
    }
    Ok(())
}

fn http2_h2c_close_row() -> anyhow::Result<PerfReport> {
    let (runtime, listener, addr) = start_h2_server(small_body())?;
    let expected = small_body().len();
    h2c_one_request(addr, expected)?; // warm one full connection + request

    let process_before = ProcessSnapshot::now();
    let (load, allocations) = run_counted(
        LoadRun {
            workers: WORKERS,
            stop: LoadStop::ops(PROTOCOL_OPS),
            label: "tina_http2_h2c_close_request",
        },
        move |_| match h2c_one_request(addr, expected) {
            Ok(()) => OpOutcome::Ok,
            Err(_) => OpOutcome::Err {
                kind: "http2_error",
            },
        },
        None::<fn() -> LoadObservation>,
    );
    let (allocs, bytes, rss) = ProcessSnapshot::now().delta_from(process_before);
    print_process_metrics("tina_http2_h2c_close_request", allocs, bytes, rss);
    let _ = runtime.try_send(listener, Http2ListenerMsg::Stop);
    shutdown_runtime(runtime)?;
    Ok(PerfReport::from_load_with_allocations(
        "http2_h2c_close_request",
        "connection_setup",
        load,
        allocations,
    ))
}

fn h2c_one_request(addr: SocketAddr, expected: usize) -> anyhow::Result<()> {
    let mut stream = h2c_connect(addr)?;
    h2c_get(&mut stream, 1, "/", expected)
}

fn http2_h2c_keepalive_row() -> anyhow::Result<PerfReport> {
    let (runtime, listener, addr) = start_h2_server(small_body())?;
    let expected = small_body().len();
    h2c_keepalive_op(addr, expected)?; // warm

    let process_before = ProcessSnapshot::now();
    let (load, allocations) = run_counted(
        LoadRun {
            workers: WORKERS,
            stop: LoadStop::ops(PROTOCOL_OPS),
            label: "tina_http2_h2c_keepalive_sequential",
        },
        move |_| match h2c_keepalive_op(addr, expected) {
            Ok(()) => OpOutcome::Ok,
            Err(_) => OpOutcome::Err {
                kind: "http2_error",
            },
        },
        None::<fn() -> LoadObservation>,
    );
    let (allocs, bytes, rss) = ProcessSnapshot::now().delta_from(process_before);
    print_process_metrics("tina_http2_h2c_keepalive_sequential", allocs, bytes, rss);
    let _ = runtime.try_send(listener, Http2ListenerMsg::Stop);
    shutdown_runtime(runtime)?;
    Ok(PerfReport::from_load_with_allocations(
        "http2_h2c_keepalive_sequential",
        "connection_setup_amortized",
        load,
        allocations,
    ))
}

fn h2c_keepalive_op(addr: SocketAddr, expected: usize) -> anyhow::Result<()> {
    let mut stream = h2c_connect(addr)?;
    for i in 0..H2_KEEPALIVE_REQUESTS_PER_CONN {
        // HTTP/2 client streams use odd, strictly-increasing ids.
        h2c_get(&mut stream, (1 + 2 * i) as u32, "/", expected)?;
    }
    Ok(())
}

fn http2_h2c_steady_state_small_row() -> anyhow::Result<PerfReport> {
    let (runtime, listener, addr) = start_h2_server(small_body())?;
    let expected = small_body().len();

    // One warmed connection per load worker. Each carries its own next odd
    // stream id so reuse across ops stays protocol-legal.
    let mut conns = Vec::with_capacity(WORKERS);
    for _ in 0..WORKERS {
        let mut stream = h2c_connect(addr)?;
        h2c_get(&mut stream, 1, "/", expected)?; // warm
        conns.push(Mutex::new(Some((stream, 3u32))));
    }
    let conns = Arc::new(conns);

    let process_before = ProcessSnapshot::now();
    let (load, allocations) = {
        let conns = Arc::clone(&conns);
        run_counted(
            LoadRun {
                workers: WORKERS,
                stop: LoadStop::ops(PROTOCOL_OPS),
                label: "tina_http2_h2c_steady_state_small",
            },
            move |worker_id| {
                let Some(slot) = conns.get(worker_id) else {
                    return OpOutcome::Err {
                        kind: "missing_worker_stream",
                    };
                };
                let mut guard = slot.lock().expect("h2 steady lock");
                let Some((stream, next_id)) = guard.as_mut() else {
                    return OpOutcome::Err {
                        kind: "closed_worker_stream",
                    };
                };
                let id = *next_id;
                *next_id += 2;
                match h2c_get(stream, id, "/", expected) {
                    Ok(()) => OpOutcome::Ok,
                    Err(_) => OpOutcome::Err {
                        kind: "http2_error",
                    },
                }
            },
            None::<fn() -> LoadObservation>,
        )
    };
    let (allocs, bytes, rss) = ProcessSnapshot::now().delta_from(process_before);
    print_process_metrics("tina_http2_h2c_steady_state_small", allocs, bytes, rss);
    for slot in conns.iter() {
        let _ = slot.lock().expect("h2 steady lock").take();
    }
    let _ = runtime.try_send(listener, Http2ListenerMsg::Stop);
    shutdown_runtime(runtime)?;
    Ok(PerfReport::from_load_with_allocations(
        "http2_h2c_steady_state_small",
        "steady_state_reuse",
        load,
        allocations,
    ))
}

// ---- gRPC (h2c unary) ------------------------------------------------------
//
// The smallest public unary gRPC row: a `GrpcRouter` service behind the real
// `Http2Listener`, driven by the public `grpc_unary_call_h2c_blocking` client.
// It exercises gRPC request framing, the HTTP/2 server response path (the
// by-value buffered-response writer this phase introduced), and gRPC trailer
// status. `connection_setup` kind: one fresh h2c connection per op (the
// blocking client is one-shot), so this is a setup row, not a steady-state one.

#[derive(Clone, PartialEq, prost::Message)]
struct GrpcPerfRequest {
    #[prost(uint64, tag = "1")]
    delta: u64,
}

#[derive(Clone, PartialEq, prost::Message)]
struct GrpcPerfReply {
    #[prost(uint64, tag = "1")]
    value: u64,
}

const GRPC_UNARY_PATH: &str = "/perf.Counter/Increment";

fn start_grpc_server() -> anyhow::Result<(PerfRuntime, Address<Http2ListenerMsg>, SocketAddr)> {
    let runtime = new_runtime();
    let limits = GrpcLimits::default();
    let router = GrpcRouter::<SingleShard>::new(limits).unary(
        GRPC_UNARY_PATH,
        |request: GrpcRequest<GrpcPerfRequest>| {
            Ok(GrpcResponse::new(GrpcPerfReply {
                value: request.message.delta + 1,
            }))
        },
    );
    let service = runtime
        .register_with_capacity::<GrpcRouter<SingleShard>, _>(router, CAPACITY)
        .map_err(|e| anyhow::anyhow!("register tina grpc router: {e:?}"))?;
    let config = Http2ServerConfig::dev();
    // The listener is generic over the service message type; the gRPC router
    // accepts `GrpcRouterMsg: From<HttpRequest>`.
    let listener = runtime
        .register_with_capacity::<Http2Listener<SingleShard, GrpcRouterMsg>, _>(
            Http2Listener::<SingleShard, GrpcRouterMsg>::new(
                "127.0.0.1:0".parse()?,
                service,
                config,
            ),
            config.listener_mailbox_capacity,
        )
        .map_err(|e| anyhow::anyhow!("register tina grpc listener: {e:?}"))?;
    let bound = runtime.observe_next_bound();
    runtime
        .try_send(listener, Http2ListenerMsg::Start)
        .map_err(|e| anyhow::anyhow!("start tina grpc listener: {e:?}"))?;
    let addr = bound
        .wait(Duration::from_secs(2))
        .map_err(|e| anyhow::anyhow!("observe tina grpc bind: {e:?}"))?;
    Ok((runtime, listener, addr))
}

fn grpc_unary_op(addr: SocketAddr) -> anyhow::Result<()> {
    let reply: GrpcPerfReply = grpc_unary_call_h2c_blocking(
        addr,
        GRPC_UNARY_PATH,
        &GrpcPerfRequest { delta: 41 },
        PROTOCOL_CLIENT_TIMEOUT,
        GrpcLimits::default(),
    )
    .map_err(|e| anyhow::anyhow!("grpc unary call: {e:?}"))?;
    // The status must be OK (a non-OK status surfaces as Err above) AND the
    // decoded message must be the expected value — never a silently dropped
    // status folded into a zero reply.
    if reply.value != 42 {
        anyhow::bail!("unexpected grpc reply value {}", reply.value);
    }
    Ok(())
}

fn grpc_h2c_unary_close_row() -> anyhow::Result<PerfReport> {
    let (runtime, listener, addr) = start_grpc_server()?;
    grpc_unary_op(addr)?; // warm one full connection + unary call

    let process_before = ProcessSnapshot::now();
    let (load, allocations) = run_counted(
        LoadRun {
            workers: WORKERS,
            stop: LoadStop::ops(PROTOCOL_OPS),
            label: "tina_grpc_h2c_unary_close",
        },
        move |_| match grpc_unary_op(addr) {
            Ok(()) => OpOutcome::Ok,
            Err(_) => OpOutcome::Err { kind: "grpc_error" },
        },
        None::<fn() -> LoadObservation>,
    );
    let (allocs, bytes, rss) = ProcessSnapshot::now().delta_from(process_before);
    print_process_metrics("tina_grpc_h2c_unary_close", allocs, bytes, rss);
    let _ = runtime.try_send(listener, Http2ListenerMsg::Stop);
    shutdown_runtime(runtime)?;
    Ok(PerfReport::from_load_with_allocations(
        "grpc_h2c_unary_close",
        "connection_setup",
        load,
        allocations,
    ))
}

// ---- WebSocket -------------------------------------------------------------

const WS_OPCODE_TEXT: u8 = 0x1;
const WS_OPCODE_CLOSE: u8 = 0x8;
const WS_CLOSE_NORMAL: [u8; 2] = [0x03, 0xe8]; // code 1000, big-endian

#[derive(Debug)]
struct WsGateway {
    app: Address<WebSocketSessionMsg, WebSocketSessionOutcome>,
    limits: WebSocketLimits,
}

#[tina_runtime::isolate(message = HttpRequest, reply = HttpResponse)]
impl WsGateway {
    fn handle(
        &mut self,
        _request: HttpRequest,
        _ctx: &mut Context<'_, SingleShard, Self::Reply>,
    ) -> Effect<Self> {
        noop()
    }

    fn handle_call(&mut self, request: HttpRequest, call: CallContext<'_, Self>) -> Effect<Self> {
        call.reply(self.response_for(request))
    }
}

impl WsGateway {
    fn response_for(&self, request: HttpRequest) -> HttpResponse {
        match websocket_upgrade(&request, self.limits) {
            Ok(upgrade) => HttpResponse::websocket(upgrade.accept(self.app, self.limits)),
            Err(_) => HttpResponse::bad_request(),
        }
    }
}

#[derive(Debug)]
struct WsApp {
    overfill_bytes: usize,
    pressure_count: Arc<AtomicU64>,
    // Counts every app-handler delivery (one increment per message the
    // connection owner delivers). Used by the Rock 5 turn-count probe to
    // prove one app turn per wire event after the duplicate-delivery removal.
    app_turns: Arc<AtomicU64>,
}

#[tina_runtime::isolate(message = WebSocketSessionMsg, reply = WebSocketSessionOutcome)]
impl WsApp {
    fn handle(
        &mut self,
        msg: WebSocketSessionMsg,
        _ctx: &mut Context<'_, SingleShard, Self::Reply>,
    ) -> Effect<Self> {
        self.app_turns.fetch_add(1, Ordering::Relaxed);
        reply(self.outcome_for(msg))
    }

    fn handle_call(
        &mut self,
        msg: WebSocketSessionMsg,
        call: CallContext<'_, Self>,
    ) -> Effect<Self> {
        self.app_turns.fetch_add(1, Ordering::Relaxed);
        call.reply(self.outcome_for(msg))
    }
}

impl WsApp {
    fn outcome_for(&self, msg: WebSocketSessionMsg) -> WebSocketSessionOutcome {
        // The protocol owner delivers one session-rich event per wire frame:
        // wire text/binary/close arrive as `Session*`, echoed back here.
        match msg {
            WebSocketSessionMsg::SessionText { text, .. }
                if self.overfill_bytes > 0 && text == "overfill" =>
            {
                // Reply larger than the bounded outbound capacity to force a
                // typed pressure event on the connection.
                WebSocketSessionOutcome::Text("x".repeat(self.overfill_bytes))
            }
            WebSocketSessionMsg::SessionText { text, .. } => WebSocketSessionOutcome::Text(text),
            WebSocketSessionMsg::SessionBinary { bytes, .. } => {
                WebSocketSessionOutcome::Binary(bytes)
            }
            WebSocketSessionMsg::SessionClose { code, reason, .. } => {
                WebSocketSessionOutcome::Close(code, reason)
            }
            // Count ONLY the typed `SessionPressure` surface, not the legacy
            // `Pressure(_)` spelling: the probe must prove the typed event
            // fires exactly once per op, and counting both would let a regression
            // that doubled or renamed the event slip past an exact-count check.
            WebSocketSessionMsg::SessionPressure { .. } => {
                self.pressure_count.fetch_add(1, Ordering::Relaxed);
                WebSocketSessionOutcome::None
            }
            _ => WebSocketSessionOutcome::None,
        }
    }
}

fn start_ws_server(
    limits: WebSocketLimits,
    overfill_bytes: usize,
    pressure_count: Arc<AtomicU64>,
    app_turns: Arc<AtomicU64>,
) -> anyhow::Result<(PerfRuntime, HttpListenerAddress, SocketAddr)> {
    let runtime = new_runtime();
    let app = runtime
        .register_with_capacity::<_, Infallible>(
            WsApp {
                overfill_bytes,
                pressure_count,
                app_turns,
            },
            CAPACITY,
        )
        .map_err(|e| anyhow::anyhow!("register tina ws app: {e:?}"))?;
    let gateway = runtime
        .register_with_capacity::<_, Infallible>(WsGateway { app, limits }, CAPACITY)
        .map_err(|e| anyhow::anyhow!("register tina ws gateway: {e:?}"))?;
    let config = HttpServerConfig::dev();
    let listener = runtime
        .register_with_capacity::<_, Infallible>(
            HttpListener::<SingleShard>::with_config("127.0.0.1:0".parse()?, gateway, config),
            config.listener_mailbox_capacity,
        )
        .map_err(|e| anyhow::anyhow!("register tina ws listener: {e:?}"))?;
    let bound = runtime.observe_next_bound();
    runtime
        .try_send(listener, HttpListenerMsg::Start)
        .map_err(|e| anyhow::anyhow!("start tina ws listener: {e:?}"))?;
    let addr = bound
        .wait(Duration::from_secs(2))
        .map_err(|e| anyhow::anyhow!("observe tina ws bind: {e:?}"))?;
    Ok((runtime, listener, addr))
}

fn ws_connect(addr: SocketAddr) -> anyhow::Result<TcpStream> {
    let mut stream = TcpStream::connect_timeout(&addr, PROTOCOL_CLIENT_TIMEOUT)?;
    stream.set_read_timeout(Some(PROTOCOL_CLIENT_TIMEOUT))?;
    stream.set_write_timeout(Some(PROTOCOL_CLIENT_TIMEOUT))?;
    stream.write_all(
        b"GET /ws HTTP/1.1\r\n\
          Host: x\r\n\
          Upgrade: websocket\r\n\
          Connection: Upgrade\r\n\
          Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n\
          Sec-WebSocket-Version: 13\r\n\r\n",
    )?;
    stream.flush()?;
    let mut head = Vec::with_capacity(256);
    let mut byte = [0u8; 1];
    while !head.ends_with(b"\r\n\r\n") {
        let n = stream.read(&mut byte)?;
        if n == 0 {
            anyhow::bail!("peer closed before websocket upgrade response");
        }
        head.push(byte[0]);
        if head.len() > 64 * 1024 {
            anyhow::bail!("websocket upgrade response head too large");
        }
    }
    if !head.starts_with(b"HTTP/1.1 101") {
        anyhow::bail!("unexpected websocket upgrade response");
    }
    Ok(stream)
}

fn ws_masked_frame(opcode: u8, payload: &[u8]) -> Vec<u8> {
    let mask = [1u8, 2, 3, 4];
    let mut out = vec![0x80 | opcode];
    if payload.len() < 126 {
        out.push(0x80 | payload.len() as u8);
    } else {
        out.push(0x80 | 126);
        out.extend_from_slice(&(payload.len() as u16).to_be_bytes());
    }
    out.extend_from_slice(&mask);
    for (i, b) in payload.iter().enumerate() {
        out.push(*b ^ mask[i % 4]);
    }
    out
}

// Returns `io::Result` so callers can tell a clean peer EOF
// (`UnexpectedEof` -> pressure close) apart from a read timeout or other I/O
// error (a hang or transport failure, which must NOT be reported as pressure).
fn ws_read_frame(stream: &mut TcpStream) -> std::io::Result<(u8, Vec<u8>)> {
    let mut head = [0u8; 2];
    stream.read_exact(&mut head)?;
    let opcode = head[0] & 0x0f;
    let mut len = usize::from(head[1] & 0x7f);
    if len == 126 {
        let mut wide = [0u8; 2];
        stream.read_exact(&mut wide)?;
        len = usize::from(u16::from_be_bytes(wide));
    } else if len == 127 {
        let mut wide = [0u8; 8];
        stream.read_exact(&mut wide)?;
        len = u64::from_be_bytes(wide) as usize;
    }
    let mut payload = vec![0u8; len];
    stream.read_exact(&mut payload)?;
    Ok((opcode, payload))
}

fn ws_send_text_recv(stream: &mut TcpStream, text: &[u8]) -> anyhow::Result<()> {
    stream.write_all(&ws_masked_frame(WS_OPCODE_TEXT, text))?;
    stream.flush()?;
    let (opcode, payload) = ws_read_frame(stream)?;
    if opcode != WS_OPCODE_TEXT || payload != text {
        anyhow::bail!("unexpected websocket echo opcode={opcode}");
    }
    Ok(())
}

fn ws_send_close(stream: &mut TcpStream) -> anyhow::Result<()> {
    stream.write_all(&ws_masked_frame(WS_OPCODE_CLOSE, &WS_CLOSE_NORMAL))?;
    stream.flush()?;
    // Drain until a close frame or EOF; both are a clean close handshake end.
    loop {
        match ws_read_frame(stream) {
            Ok((WS_OPCODE_CLOSE, _)) => return Ok(()),
            Ok(_) => continue,
            Err(err) if err.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(()),
            Err(err) => return Err(err.into()),
        }
    }
}

fn ws_open_close_op(addr: SocketAddr) -> anyhow::Result<()> {
    let mut stream = ws_connect(addr)?;
    ws_send_close(&mut stream)
}

fn ws_text_round_trip_op(addr: SocketAddr) -> anyhow::Result<()> {
    let mut stream = ws_connect(addr)?;
    ws_send_text_recv(&mut stream, b"hello")?;
    ws_send_close(&mut stream)
}

fn websocket_open_close_row() -> anyhow::Result<PerfReport> {
    ws_setup_row(
        "websocket_open_close",
        "connection_setup",
        "tina_websocket_open_close",
        ws_open_close_op,
    )
}

fn websocket_text_round_trip_row() -> anyhow::Result<PerfReport> {
    ws_setup_row(
        "websocket_text_round_trip",
        "connection_setup",
        "tina_websocket_text_round_trip",
        ws_text_round_trip_op,
    )
}

fn ws_setup_row(
    label: &'static str,
    kind: &'static str,
    run_label: &'static str,
    op: fn(SocketAddr) -> anyhow::Result<()>,
) -> anyhow::Result<PerfReport> {
    let (runtime, listener, addr) = start_ws_server(
        WebSocketLimits::default(),
        0,
        Arc::new(AtomicU64::new(0)),
        Arc::new(AtomicU64::new(0)),
    )?;
    op(addr)?; // warm

    let process_before = ProcessSnapshot::now();
    let (load, allocations) = run_counted(
        LoadRun {
            workers: WORKERS,
            stop: LoadStop::ops(PROTOCOL_OPS),
            label: run_label,
        },
        move |_| match op(addr) {
            Ok(()) => OpOutcome::Ok,
            Err(_) => OpOutcome::Err { kind: "ws_error" },
        },
        None::<fn() -> LoadObservation>,
    );
    let (allocs, bytes, rss) = ProcessSnapshot::now().delta_from(process_before);
    print_process_metrics(run_label, allocs, bytes, rss);
    let _ = runtime.try_send(listener, HttpListenerMsg::Stop);
    shutdown_runtime(runtime)?;
    Ok(PerfReport::from_load_with_allocations(
        label,
        kind,
        load,
        allocations,
    ))
}

fn websocket_steady_state_small_row() -> anyhow::Result<PerfReport> {
    let (runtime, listener, addr) = start_ws_server(
        WebSocketLimits::default(),
        0,
        Arc::new(AtomicU64::new(0)),
        Arc::new(AtomicU64::new(0)),
    )?;

    let mut streams = Vec::with_capacity(WORKERS);
    for _ in 0..WORKERS {
        let mut stream = ws_connect(addr)?;
        ws_send_text_recv(&mut stream, b"ok")?; // warm
        streams.push(Mutex::new(Some(stream)));
    }
    let streams = Arc::new(streams);

    let process_before = ProcessSnapshot::now();
    let (load, allocations) = {
        let streams = Arc::clone(&streams);
        run_counted(
            LoadRun {
                workers: WORKERS,
                stop: LoadStop::ops(PROTOCOL_OPS),
                label: "tina_websocket_steady_state_small",
            },
            move |worker_id| {
                let Some(slot) = streams.get(worker_id) else {
                    return OpOutcome::Err {
                        kind: "missing_worker_stream",
                    };
                };
                let mut guard = slot.lock().expect("ws steady lock");
                let Some(stream) = guard.as_mut() else {
                    return OpOutcome::Err {
                        kind: "closed_worker_stream",
                    };
                };
                match ws_send_text_recv(stream, b"ok") {
                    Ok(()) => OpOutcome::Ok,
                    Err(_) => OpOutcome::Err { kind: "ws_error" },
                }
            },
            None::<fn() -> LoadObservation>,
        )
    };
    let (allocs, bytes, rss) = ProcessSnapshot::now().delta_from(process_before);
    print_process_metrics("tina_websocket_steady_state_small", allocs, bytes, rss);
    for slot in streams.iter() {
        if let Some(mut stream) = slot.lock().expect("ws steady lock").take() {
            let _ = ws_send_close(&mut stream);
        }
    }
    let _ = runtime.try_send(listener, HttpListenerMsg::Stop);
    shutdown_runtime(runtime)?;
    Ok(PerfReport::from_load_with_allocations(
        "websocket_steady_state_small",
        "steady_state_reuse",
        load,
        allocations,
    ))
}

/// Rock 5 turn-count probe: drive `messages` text round trips over one warmed
/// WebSocket connection and return the total app-handler delivery count for the
/// whole session (open handshake + each text + close).
///
/// The connection owner now delivers exactly one session-rich app event per
/// wire event, so the total is one app turn per text plus a few handshake/close
/// turns — strictly fewer than the old duplicate path, which delivered a
/// session-rich *and* a legacy event for every wire frame (≈ `2 * messages`
/// text turns alone). The hotpath probe asserts `turns < 2 * messages`, which
/// the pre-dedup path could not satisfy.
pub fn websocket_text_round_trip_app_turns(messages: usize) -> anyhow::Result<u64> {
    let app_turns = Arc::new(AtomicU64::new(0));
    let (runtime, listener, addr) = start_ws_server(
        WebSocketLimits::default(),
        0,
        Arc::new(AtomicU64::new(0)),
        Arc::clone(&app_turns),
    )?;
    let mut stream = ws_connect(addr)?;
    for _ in 0..messages {
        ws_send_text_recv(&mut stream, b"turns")?;
    }
    ws_send_close(&mut stream)?;
    let _ = runtime.try_send(listener, HttpListenerMsg::Stop);
    shutdown_runtime(runtime)?;
    Ok(app_turns.load(Ordering::Relaxed))
}

/// Deterministic WebSocket capacity-fill pressure probe.
///
/// The plan allows replacing a timing-sensitive `slow_peer_pressure` row with a
/// deterministic capacity-fill that uses the public send path and proves *typed*
/// pressure without sleeping on a slow client. Each op opens a fresh session and
/// sends one `overfill` text; the echo reply is larger than the session's
/// bounded `max_queued_outbound_bytes`, so the connection raises a typed
/// `SessionPressure` to the app and closes without writing the over-cap frame.
///
/// Two independent facts are asserted: the client sees the closed/no-echo signal
/// (counted as `full` pressure in the load report), and the app's
/// `SessionPressure` counter reaches one per op — the server-side *typed*
/// pressure surface, proving the pressure was real and not just a dropped frame.
pub fn websocket_capacity_fill_probe() -> anyhow::Result<LoadReport> {
    const MAX_OUTBOUND_BYTES: usize = 8;
    const OVERFILL_BYTES: usize = 64;
    const PRESSURE_OPS: u64 = 8;

    let pressure_count = Arc::new(AtomicU64::new(0));
    let limits = WebSocketLimits {
        max_queued_outbound_bytes: MAX_OUTBOUND_BYTES,
        ..WebSocketLimits::default()
    };
    let (runtime, listener, addr) = start_ws_server(
        limits,
        OVERFILL_BYTES,
        Arc::clone(&pressure_count),
        Arc::new(AtomicU64::new(0)),
    )?;

    let (load, _allocations) = run_counted(
        LoadRun {
            workers: 1,
            stop: LoadStop::ops(PRESSURE_OPS),
            label: "tina_websocket_capacity_fill",
        },
        move |_| match ws_overfill_op(addr) {
            Ok(true) => OpOutcome::Err { kind: "full" },
            Ok(false) => OpOutcome::Ok,
            Err(_) => OpOutcome::Err { kind: "ws_error" },
        },
        Some({
            let pressure_count = Arc::clone(&pressure_count);
            move || {
                // Wait (bounded) for the worker to deliver every typed pressure
                // event to the app. leak_clean is true only if EXACTLY one typed
                // SessionPressure arrived per op: too few means a pressure event
                // was lost or a session leaked; more than one per op means the
                // typed event double-fired. Either way the proof must fail.
                let deadline = Instant::now() + Duration::from_secs(2);
                while pressure_count.load(Ordering::Relaxed) < PRESSURE_OPS
                    && Instant::now() < deadline
                {
                    thread::yield_now();
                }
                LoadObservation {
                    leak_clean: pressure_count.load(Ordering::Relaxed) == PRESSURE_OPS,
                    ..LoadObservation::default()
                }
            }
        }),
    );

    println!(
        "perf-ws-pressure label=tina_websocket_capacity_fill typed_session_pressure={} ops={}",
        pressure_count.load(Ordering::Relaxed),
        PRESSURE_OPS,
    );
    let _ = runtime.try_send(listener, HttpListenerMsg::Stop);
    shutdown_runtime(runtime)?;
    Ok(load)
}

/// Whole-process allocations for `requests` warmed h2c responses on one reused
/// connection, measured with the counting global allocator.
///
/// The connection and HPACK state are warmed first, so the measured window is
/// steady-state request/response work only — no listener startup, no first-time
/// allocator growth. This isolates the server's per-response framing cost (plus
/// the constant raw-client cost), which is what the Phase 152 buffered-response
/// rewrite reduces: one fewer body-sized allocation per DATA frame.
pub fn http2_steady_state_response_process_allocations(requests: usize) -> anyhow::Result<u64> {
    let (runtime, listener, addr) = start_h2_server(small_body())?;
    let expected = small_body().len();
    let mut stream = h2c_connect(addr)?;
    let mut next_id = 1u32;
    for _ in 0..16 {
        h2c_get(&mut stream, next_id, "/", expected)?;
        next_id += 2;
    }
    let mut error: Option<anyhow::Error> = None;
    let (_unit, process) = count_process_allocations(|| {
        for _ in 0..requests {
            if let Err(err) = h2c_get(&mut stream, next_id, "/", expected) {
                error = Some(err);
                break;
            }
            next_id += 2;
        }
    });
    if let Some(err) = error {
        return Err(err);
    }
    drop(stream);
    let _ = runtime.try_send(listener, Http2ListenerMsg::Stop);
    shutdown_runtime(runtime)?;
    Ok(process)
}

/// Returns `Ok(true)` when the server signalled pressure (a CLOSE frame, or a
/// clean peer EOF — the connection drops the over-cap session without writing
/// the frame), `Ok(false)` if the server actually echoed the over-cap text (no
/// pressure), and `Err` for any real transport failure (connect/write error, a
/// read timeout/hang, or a malformed frame). A hang must NOT masquerade as
/// pressure, so only `UnexpectedEof` is treated as the pressure close.
fn ws_overfill_op(addr: SocketAddr) -> anyhow::Result<bool> {
    let mut stream = ws_connect(addr)?;
    stream.write_all(&ws_masked_frame(WS_OPCODE_TEXT, b"overfill"))?;
    stream.flush()?;
    loop {
        match ws_read_frame(&mut stream) {
            Ok((WS_OPCODE_TEXT, _)) => return Ok(false), // got echo, no pressure
            Ok((WS_OPCODE_CLOSE, _)) => return Ok(true), // server closed on pressure
            Ok(_) => continue,
            Err(err) if err.kind() == std::io::ErrorKind::UnexpectedEof => {
                return Ok(true); // clean EOF after pressure-triggered close
            }
            Err(err) => return Err(err.into()), // timeout/hang/other: a real failure
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;
    use std::sync::mpsc;

    #[test]
    fn websocket_close_drain_does_not_treat_timeout_as_clean_close() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind test listener");
        let addr = listener.local_addr().expect("listener addr");
        let (done_tx, done_rx) = mpsc::channel();

        thread::spawn(move || {
            let (mut peer, _) = listener.accept().expect("accept close test peer");
            let mut buf = [0u8; 8];
            let _ = peer.read(&mut buf);
            // Keep the connection open without sending a close frame. The
            // client helper must surface its read timeout as failure, not count
            // it as a clean drain.
            let _ = done_rx.recv_timeout(Duration::from_millis(250));
        });

        let mut stream = TcpStream::connect(addr).expect("connect close test peer");
        stream
            .set_read_timeout(Some(Duration::from_millis(25)))
            .expect("set read timeout");
        stream
            .set_write_timeout(Some(Duration::from_millis(25)))
            .expect("set write timeout");

        let error = ws_send_close(&mut stream).expect_err("timeout is not clean close");
        let io_error = error
            .downcast_ref::<std::io::Error>()
            .expect("close timeout remains an io error");
        assert!(
            matches!(
                io_error.kind(),
                std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
            ),
            "unexpected timeout error kind: {:?}",
            io_error.kind(),
        );
        let _ = done_tx.send(());
    }
}
