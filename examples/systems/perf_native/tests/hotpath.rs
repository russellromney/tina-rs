//! Hot-path stage probes for the native Tina runtime.
//!
//! Three probes prove where host send/call time goes, in release mode:
//!
//! - `hotpath_try_send` — one bounded queue handoff. Proves the first handoff
//!   stays cheap regardless of the worker-loop policy.
//! - `hotpath_send_and_observe` — one observed admission. Shows where the wait
//!   sits: host submit, worker pickup, mailbox admission, host unblock.
//! - `hotpath_call_blocking` — one host call to an immediate-reply isolate.
//!   A live `TraceObserver` timestamps every worker turn, so the per-turn gaps
//!   (where the old 1ms progress sleep hid) are visible until the host receives
//!   `Replied`.
//!
//! The p50/min/max totals come from an uninstrumented iteration loop. The
//! per-stage breakdown comes from one extra instrumented run so the observer's
//! own cost does not pollute the headline number. Allocation counts are
//! host-thread scope: the channel + boxed command a caller pays per op.

use std::collections::HashMap;
use std::convert::Infallible;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use perf_native::{
    count_all_allocations, http2_steady_state_response_process_allocations,
    websocket_text_round_trip_app_turns,
};
use tina::IsolateId;
use tina::prelude::*;
use tina_http::{
    HttpListener, HttpListenerMsg, HttpRequest, HttpResponse, HttpServerConfig, StatusCode,
};
use tina_proof_harness::{HotPathCounters, HotPathReport, HotPathStage};
use tina_runtime::{
    CallKind, CallOutcome, DefaultThreadedMailboxFactory, RuntimeEvent, RuntimeEventKind,
    ThreadedRuntime, ThreadedRuntimeConfig, TraceObserver,
};

const CAP: usize = 512;
const ITERS: usize = 200;
const WARMUP: usize = 40;
const CALL_TIMEOUT: Duration = Duration::from_secs(2);

// Loose latency floor ceilings. They exist to catch a regression back to
// deterministic millisecond-scale local work (for example, the old worker-loop
// sleep tax), not to pin p50 on a shared machine after a heavier perf run.
// p50/p90/p99 are still printed and checked by perf history.
const HANDOFF_CEILING_NS: u64 = 100_000; // 100us
const OBSERVED_CEILING_NS: u64 = 500_000; // 500us
const CALL_CEILING_NS: u64 = 500_000; // 500us

// Pinned warmed allocation ceilings (one warmed op). Two flavors: host-thread
// scope (what the caller's thread pays per op) and process-wide (host + every
// allocation the runtime worker thread makes on the caller's behalf —
// dispatcher routing, in-flight-call entry, translator, etc.). The
// process-wide number is the *real* per-op cost.
//
// Observed steady-state today. The persistent dispatcher pool eliminated the
// per-call `HostCallDriver` registration, the reply pool eliminated the
// per-call `mpsc::channel()`, and the typed `HostCall` command eliminated the
// per-call command-closure box (so `call_blocking` host alloc dropped 2 -> 1,
// process 6 -> 5):
//   try_send         host=1  process=1
//   send_and_observe host=4  process=5
//   call_blocking    host=1  process=5  (4/17 -> 5/11 -> 2/6 -> 1/5)
const HANDOFF_HOST_ALLOCATIONS_CEILING: u64 = 2;
const OBSERVED_HOST_ALLOCATIONS_CEILING: u64 = 6;
// One per-call allocation: the type-erased begin task box for the shared
// dispatcher. Pinned at 2 so a regression back to the closure box is caught.
const CALL_HOST_ALLOCATIONS_CEILING: u64 = 2;
const HANDOFF_PROCESS_ALLOCATIONS_CEILING: u64 = 2;
const OBSERVED_PROCESS_ALLOCATIONS_CEILING: u64 = 8;
const CALL_PROCESS_ALLOCATIONS_CEILING: u64 = 10;
const HTTP_HOTPATH_ITERS: usize = 32;
const HTTP_HOTPATH_WARMUP: usize = 4;
const HTTP_FIXED_BODY_BYTES: usize = 4096;
// HTTP stage ceilings are intentionally loose. They catch a bad turn-count
// regression while leaving timing and trace noise to perf history.
const HTTP_CLOSE_STAGE_CEILING: usize = 48;
const HTTP_KEEPALIVE_STAGE_CEILING: usize = 120;
const HTTP_FIXED_BODY_STAGE_CEILING: usize = 48;
const HTTP_STEADY_STAGE_CEILING: usize = 36;

// Whole-process allocation ceiling for 64 warmed h2c buffered responses on one
// reused connection. Lives in the hotpath binary because process-wide
// allocation counting must run with no other test thread allocating; this
// binary has a single test, so the counter is clean.
//
// The count is dominated by the Rust code path (one `alloc` per `Vec`) and is
// stable across repeated runs on this toolchain. The arc, on macOS/aarch64:
//   Phase 152 (one fewer DATA copy):           3075 (48.05/request)
//   Phase 153 leaf copies (body clone gone):   3011 (47.05/request)
//   Phase 153 structural (this pass):          2626 (41.03/request)
// The structural pass removed the inbound HEADERS frame-decode `Vec` (the read
// loop now decodes from a borrowed buffer slice) and coalesced the response
// HEADERS + DATA into one queued write (so the per-frame encode `Vec`s and a
// write turn go away): ~7 fewer allocations per request, ~14.6% off the Phase
// 152 baseline. The ceiling sits just above the measured 2626 so a regression
// that re-adds a per-request structural allocation fails, with headroom for
// toolchain/std drift. Regression guard, not a cross-platform invariant;
// recalibrate with a recorded before/after if a platform's baseline differs.
const H2_BUFFERED_RESPONSE_ALLOC_CEILING: u64 = 2_680;
const H2_BUFFERED_RESPONSE_REQUESTS: usize = 64;
// Rock 5 (fewer turns): a WebSocket session of N text round trips. The
// duplicate-delivery removal means the connection owner emits one app event per
// wire event, so total app deliveries for the session are far below the old
// `2 * N` text turns alone. The ceiling sits below `2 * N` so the pre-dedup
// path (which delivered a session-rich AND a legacy event per frame) fails.
const WS_TURN_PROBE_MESSAGES: usize = 64;

type Runtime = ThreadedRuntime<SingleShard, DefaultThreadedMailboxFactory>;

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
struct BodyService {
    body: Arc<Vec<u8>>,
}

#[tina_runtime::isolate(message = HttpRequest, reply = HttpResponse)]
impl BodyService {
    fn handle(
        &mut self,
        _msg: HttpRequest,
        _ctx: &mut Context<'_, SingleShard, Self::Reply>,
    ) -> Effect<Self> {
        noop()
    }

    fn handle_call(&mut self, _msg: HttpRequest, call: CallContext<'_, Self>) -> Effect<Self> {
        call.reply(HttpResponse::with_body(
            StatusCode::OK,
            self.body.as_ref().clone(),
        ))
    }
}

/// Records the wall-clock instant of every worker-thread trace event so a
/// single instrumented op can be broken into per-turn stages. The lock is
/// only contended while the host clears or snapshots, both done when the
/// worker is idle.
#[derive(Default)]
struct StageTimer {
    events: Mutex<Vec<TraceSample>>,
}

impl StageTimer {
    fn clear(&self) {
        self.events.lock().expect("stage timer lock").clear();
    }

    fn snapshot(&self) -> Vec<TraceSample> {
        self.events.lock().expect("stage timer lock").clone()
    }
}

#[derive(Debug, Clone, Copy)]
struct TraceSample {
    label: &'static str,
    isolate: IsolateId,
    kind: RuntimeEventKind,
    at: Instant,
}

#[derive(Debug, Clone)]
struct HotPathBreakdown {
    stages: Vec<HotPathStage>,
    counters: HotPathCounters,
}

impl TraceObserver for StageTimer {
    fn on_event(&self, event: &RuntimeEvent) {
        let label = kind_label(event.kind());
        self.events
            .lock()
            .expect("stage timer lock")
            .push(TraceSample {
                label,
                isolate: event.isolate(),
                kind: event.kind(),
                at: Instant::now(),
            });
    }
}

fn kind_label(kind: RuntimeEventKind) -> &'static str {
    match kind {
        RuntimeEventKind::MailboxAccepted => "mbox_accepted",
        RuntimeEventKind::HandlerStarted => "handler_started",
        RuntimeEventKind::HandlerPanicked => "handler_panicked",
        RuntimeEventKind::HandlerReportedFailure => "handler_failed",
        RuntimeEventKind::HandlerFinished { .. } => "handler_finished",
        RuntimeEventKind::EffectObserved { .. } => "effect_observed",
        RuntimeEventKind::SendDispatchAttempted { .. } => "send_attempted",
        RuntimeEventKind::SendAccepted { .. } => "send_accepted",
        RuntimeEventKind::SendRejected { .. } => "send_rejected",
        RuntimeEventKind::CallDispatchAttempted { .. } => "call_attempted",
        RuntimeEventKind::CallCompleted { .. } => "call_completed",
        RuntimeEventKind::CallFailed { .. } => "call_failed",
        RuntimeEventKind::CallCompletionAction { .. } => "call_completion_action",
        RuntimeEventKind::CallCompletionRejected { .. } => "call_completion_rejected",
        RuntimeEventKind::CallReplyRejected { .. } => "call_reply_rejected",
        RuntimeEventKind::CallReplyAbandoned { .. } => "call_reply_abandoned",
        RuntimeEventKind::CallRejected { .. } => "call_rejected",
        RuntimeEventKind::CallCancelled { .. } => "call_cancelled",
        RuntimeEventKind::IsolateStopped => "isolate_stopped",
        RuntimeEventKind::MessageAbandoned => "message_abandoned",
        _ => "other",
    }
}

fn new_runtime(observer: Option<Arc<dyn TraceObserver>>) -> Runtime {
    let mut config = ThreadedRuntimeConfig {
        command_capacity: CAP,
        ..ThreadedRuntimeConfig::default()
    };
    // Opt-in sweep knob: `TINA_PERF_IDLE_REPOLL_US=N` sets the I/O re-poll park
    // so an A/B can quantify how the worker's socket-readiness latency (the
    // HTTP `host_submit -> mbox_accepted` gap) responds. Unset = default
    // (idle_repoll == idle_wait), so normal runs are unchanged.
    if let Ok(raw) = std::env::var("TINA_PERF_IDLE_REPOLL_US")
        && let Ok(us) = raw.trim().parse::<u64>()
    {
        config.idle_repoll_interval = Duration::from_micros(us.max(1));
    }
    match observer {
        Some(observer) => ThreadedRuntime::with_config_and_trace_observer(
            SingleShard,
            DefaultThreadedMailboxFactory,
            config,
            observer,
        ),
        None => ThreadedRuntime::with_config(SingleShard, DefaultThreadedMailboxFactory, config),
    }
}

fn shutdown(runtime: Runtime) {
    let _ = runtime.shutdown();
}

fn median(samples: &[u64]) -> u64 {
    let mut sorted = samples.to_vec();
    sorted.sort_unstable();
    sorted[sorted.len() / 2]
}

fn register_counter(runtime: &Runtime, count: &Arc<AtomicU64>) -> Address<CounterMsg> {
    runtime
        .register_with_capacity::<_, Infallible>(
            Counter {
                count: Arc::clone(count),
            },
            CAP,
        )
        .expect("register counter")
}

fn wait_count(count: &AtomicU64, target: u64) {
    let deadline = Instant::now() + Duration::from_secs(2);
    while count.load(Ordering::Relaxed) < target {
        assert!(
            Instant::now() < deadline,
            "counter stalled at {} (target {target})",
            count.load(Ordering::Relaxed)
        );
        std::thread::yield_now();
    }
}

/// Turns one instrumented timeline into named stage gaps: host submit -> first
/// worker event -> ... -> host unblock. Repeated boundaries get a numeric
/// suffix so two inter-turn gaps stay distinct in the report.
fn stages_from_timeline(t0: Instant, events: &[TraceSample], t_end: Instant) -> HotPathBreakdown {
    stages_from_filtered_timeline(t0, events, t_end, |_| true)
}

fn stages_from_filtered_timeline<F>(
    t0: Instant,
    events: &[TraceSample],
    t_end: Instant,
    mut include: F,
) -> HotPathBreakdown
where
    F: FnMut(TraceSample) -> bool,
{
    let mut stages = Vec::new();
    let mut counts: HashMap<String, u32> = HashMap::new();
    let mut prev_label = "host_submit";
    let mut prev_at = t0;
    let mut counters = HotPathCounters::default();
    for sample in events.iter().copied() {
        if !include(sample) {
            continue;
        }
        let label = sample.label;
        let at = sample.at;
        if at > t_end {
            continue;
        }
        count_event(sample.kind, &mut counters);
        let nanos = at.saturating_duration_since(prev_at).as_nanos() as u64;
        push_stage(
            &mut stages,
            &mut counts,
            format!("{prev_label}__to__{label}"),
            nanos,
        );
        prev_label = label;
        prev_at = at;
    }
    let tail = t_end.saturating_duration_since(prev_at).as_nanos() as u64;
    push_stage(
        &mut stages,
        &mut counts,
        format!("{prev_label}__to__host_unblocked"),
        tail,
    );
    counters.event_stage_count = stages.len();
    HotPathBreakdown { stages, counters }
}

fn dominant_connection_isolate(
    events: &[TraceSample],
    t0: Instant,
    t_end: Instant,
    service: IsolateId,
) -> Option<IsolateId> {
    let mut counts: HashMap<IsolateId, usize> = HashMap::new();
    for sample in events.iter().copied() {
        if sample.at < t0 || sample.at > t_end || sample.isolate == service {
            continue;
        }
        *counts.entry(sample.isolate).or_insert(0) += 1;
    }
    counts
        .into_iter()
        .max_by_key(|(_isolate, count)| *count)
        .map(|(isolate, _)| isolate)
}

fn stages_from_http_timeline(
    t0: Instant,
    events: &[TraceSample],
    t_end: Instant,
    service: IsolateId,
) -> HotPathBreakdown {
    let Some(connection) = dominant_connection_isolate(events, t0, t_end, service) else {
        return stages_from_timeline(t0, events, t_end);
    };
    stages_from_filtered_timeline(t0, events, t_end, |sample| {
        sample.isolate == connection || sample.isolate == service
    })
}

fn count_event(kind: RuntimeEventKind, counters: &mut HotPathCounters) {
    match kind {
        RuntimeEventKind::HandlerStarted => counters.handler_turn_count += 1,
        RuntimeEventKind::CallDispatchAttempted {
            call_kind: CallKind::IsolateCall,
            ..
        } => counters.service_call_count += 1,
        RuntimeEventKind::CallDispatchAttempted { .. } => counters.runtime_call_count += 1,
        RuntimeEventKind::CallCompleted { .. } => counters.completion_count += 1,
        RuntimeEventKind::CallCompletionRejected { .. } => {
            counters.rejected_completion_count += 1;
        }
        _ => {}
    }
}

fn push_stage(
    stages: &mut Vec<HotPathStage>,
    counts: &mut HashMap<String, u32>,
    base: String,
    nanos: u64,
) {
    let count = counts.entry(base.clone()).or_insert(0);
    *count += 1;
    let name = if *count == 1 {
        base
    } else {
        format!("{base}_{count}")
    };
    stages.push(HotPathStage::new(name, nanos));
}

fn probe_try_send() -> HotPathReport {
    let runtime = new_runtime(None);
    let count = Arc::new(AtomicU64::new(0));
    let addr = register_counter(&runtime, &count);
    for _ in 0..WARMUP {
        runtime
            .try_send(addr, CounterMsg::Hit)
            .expect("warm try_send");
    }

    let mut totals = Vec::with_capacity(ITERS);
    for _ in 0..ITERS {
        let t0 = Instant::now();
        runtime
            .try_send(addr, CounterMsg::Hit)
            .expect("try_send handoff");
        totals.push(t0.elapsed().as_nanos() as u64);
    }
    let (_outcome, host_allocations, process_allocations) =
        count_all_allocations(|| runtime.try_send(addr, CounterMsg::Hit));

    let p50 = median(&totals);
    shutdown(runtime);
    HotPathReport::from_samples_with_process_allocations(
        "hotpath_try_send",
        totals,
        vec![HotPathStage::new("host_submit_to_command_accepted", p50)],
        Some(host_allocations),
        Some(process_allocations),
    )
}

fn probe_send_and_observe() -> HotPathReport {
    let runtime = new_runtime(None);
    let count = Arc::new(AtomicU64::new(0));
    let addr = register_counter(&runtime, &count);
    for _ in 0..WARMUP {
        runtime
            .send_and_observe(addr, CounterMsg::Hit)
            .expect("warm observed admission");
    }
    let mut totals = Vec::with_capacity(ITERS);
    for _ in 0..ITERS {
        let t0 = Instant::now();
        runtime
            .send_and_observe(addr, CounterMsg::Hit)
            .expect("observed admission");
        totals.push(t0.elapsed().as_nanos() as u64);
    }
    let (_outcome, host_allocations, process_allocations) =
        count_all_allocations(|| runtime.send_and_observe(addr, CounterMsg::Hit));
    shutdown(runtime);

    let breakdown = instrumented_send_and_observe();
    HotPathReport::from_samples_with_counters(
        "hotpath_send_and_observe",
        totals,
        breakdown.stages,
        Some(host_allocations),
        Some(process_allocations),
        breakdown.counters,
    )
}

fn instrumented_send_and_observe() -> HotPathBreakdown {
    let timer = Arc::new(StageTimer::default());
    let runtime = new_runtime(Some(timer.clone()));
    let count = Arc::new(AtomicU64::new(0));
    let addr = register_counter(&runtime, &count);
    runtime
        .send_and_observe(addr, CounterMsg::Hit)
        .expect("warm observed admission");
    // Drain the warm message fully (delivered, not just admitted) before
    // clearing the timeline. `send_and_observe` returns at admission and the
    // worker delivers on a later turn, so without this the warm message's
    // delivery events race into the measured window and the breakdown lies.
    wait_count(&count, 1);
    // `wait_count` sees the increment inside `handle()`, but the runtime emits
    // `HandlerFinished` and `EffectObserved` *after* the handler returns. A
    // worker round-trip serializes the clear past those tail events so they do
    // not race into the next window. (Pure read, no extra trace events.)
    let _ = runtime.has_in_flight_calls();
    timer.clear();
    let t0 = Instant::now();
    runtime
        .send_and_observe(addr, CounterMsg::Hit)
        .expect("instrumented observed admission");
    let t_end = Instant::now();
    let events = timer.snapshot();
    shutdown(runtime);
    stages_from_timeline(t0, &events, t_end)
}

fn probe_call_blocking() -> HotPathReport {
    let runtime = new_runtime(None);
    let ping = runtime
        .register_with_capacity::<_, Infallible>(Ping, CAP)
        .expect("register ping");
    for _ in 0..WARMUP {
        assert_eq!(
            runtime
                .call_blocking(ping, PingMsg::Ping, CALL_TIMEOUT)
                .expect("warm call"),
            CallOutcome::Replied(PingReply::Pong)
        );
    }
    let mut totals = Vec::with_capacity(ITERS);
    for _ in 0..ITERS {
        let t0 = Instant::now();
        let outcome = runtime
            .call_blocking(ping, PingMsg::Ping, CALL_TIMEOUT)
            .expect("call");
        totals.push(t0.elapsed().as_nanos() as u64);
        assert_eq!(outcome, CallOutcome::Replied(PingReply::Pong));
    }
    let (_outcome, host_allocations, process_allocations) =
        count_all_allocations(|| runtime.call_blocking(ping, PingMsg::Ping, CALL_TIMEOUT));
    shutdown(runtime);

    let breakdown = instrumented_call_blocking();
    HotPathReport::from_samples_with_counters(
        "hotpath_call_blocking",
        totals,
        breakdown.stages,
        Some(host_allocations),
        Some(process_allocations),
        breakdown.counters,
    )
}

fn instrumented_call_blocking() -> HotPathBreakdown {
    let timer = Arc::new(StageTimer::default());
    let runtime = new_runtime(Some(timer.clone()));
    let ping = runtime
        .register_with_capacity::<_, Infallible>(Ping, CAP)
        .expect("register ping");
    runtime
        .call_blocking(ping, PingMsg::Ping, CALL_TIMEOUT)
        .expect("warm call");
    // `call_blocking` returns when the driver sends its outcome on the reply
    // channel, *before* the runtime processes the driver's `stop()` effect and
    // emits `IsolateStopped`. A worker round-trip serializes the clear past
    // that tail so it cannot race into the measured window.
    let _ = runtime.has_in_flight_calls();
    timer.clear();
    let t0 = Instant::now();
    let outcome = runtime
        .call_blocking(ping, PingMsg::Ping, CALL_TIMEOUT)
        .expect("instrumented call");
    let t_end = Instant::now();
    assert_eq!(outcome, CallOutcome::Replied(PingReply::Pong));
    let events = timer.snapshot();
    shutdown(runtime);
    stages_from_timeline(t0, &events, t_end)
}

// Scheduler-gap threshold for the tail rows: a gap above 100us between two
// instrumented trace events is a scheduling hiccup (a worker park between a
// ready completion and the next handler turn), not ordinary handler cost.
const GAP_THRESHOLD_NS: u64 = 100_000;

/// Counts inter-event gaps at or above `threshold` and the largest single gap.
/// The stages are exactly the inter-event gaps of one instrumented timeline,
/// so this is the honest scheduler-gap source.
fn gap_stats(stages: &[HotPathStage], threshold: u64) -> (usize, u64) {
    let mut count = 0;
    let mut max = 0;
    for stage in stages {
        if stage.nanos >= threshold {
            count += 1;
        }
        max = max.max(stage.nanos);
    }
    (count, max)
}

/// Runs the warmed `call_blocking` timing loop, optionally with a live trace
/// observer attached so the traced row's totals carry the observer's own cost.
/// Returns (per-iter totals ns, host allocs, process allocs).
fn call_blocking_totals(observer: Option<Arc<dyn TraceObserver>>) -> (Vec<u64>, u64, u64) {
    let runtime = new_runtime(observer);
    let ping = runtime
        .register_with_capacity::<_, Infallible>(Ping, CAP)
        .expect("register ping");
    for _ in 0..WARMUP {
        runtime
            .call_blocking(ping, PingMsg::Ping, CALL_TIMEOUT)
            .expect("warm call");
    }
    let mut totals = Vec::with_capacity(ITERS);
    for _ in 0..ITERS {
        let t0 = Instant::now();
        let outcome = runtime
            .call_blocking(ping, PingMsg::Ping, CALL_TIMEOUT)
            .expect("call");
        totals.push(t0.elapsed().as_nanos() as u64);
        assert_eq!(outcome, CallOutcome::Replied(PingReply::Pong));
    }
    let (_outcome, host, process) =
        count_all_allocations(|| runtime.call_blocking(ping, PingMsg::Ping, CALL_TIMEOUT));
    shutdown(runtime);
    (totals, host, process)
}

/// The `hotpath_call_blocking_tail` row in both flavors: untraced (the honest
/// distribution headline) and traced (same path with the observer attached so
/// its overhead is visible). Both carry scheduler-gap stats measured from one
/// clean instrumented run; the p50 delta between them is the observer cost.
fn probe_call_blocking_tail() -> (HotPathReport, HotPathReport) {
    let breakdown = instrumented_call_blocking();
    let (gap_count, gap_max) = gap_stats(&breakdown.stages, GAP_THRESHOLD_NS);

    let (untraced_totals, u_host, u_proc) = call_blocking_totals(None);
    let untraced = HotPathReport::from_samples_with_counters(
        "hotpath_call_blocking_tail",
        untraced_totals,
        breakdown.stages.clone(),
        Some(u_host),
        Some(u_proc),
        breakdown.counters,
    )
    .with_traced(false)
    .with_scheduler_gaps(GAP_THRESHOLD_NS, gap_count, gap_max);

    let timer = Arc::new(StageTimer::default());
    let (traced_totals, t_host, t_proc) =
        call_blocking_totals(Some(timer as Arc<dyn TraceObserver>));
    let traced = HotPathReport::from_samples_with_counters(
        "hotpath_call_blocking_tail_traced",
        traced_totals,
        breakdown.stages,
        Some(t_host),
        Some(t_proc),
        breakdown.counters,
    )
    .with_traced(true)
    .with_scheduler_gaps(GAP_THRESHOLD_NS, gap_count, gap_max);

    (untraced, traced)
}

/// Runs the warmed steady-state keepalive timing loop, optionally with a live
/// observer attached. Returns (totals ns, host allocs, process allocs).
fn steady_state_totals(
    body: &[u8],
    observer: Option<Arc<dyn TraceObserver>>,
) -> (Vec<u64>, u64, u64) {
    let (runtime, addr, _service) = start_http_runtime(observer, body.to_vec(), true);
    for _ in 0..HTTP_HOTPATH_WARMUP {
        http_get_reused(addr, 2, body.len()).expect("warm steady-state http request");
    }
    let mut totals = Vec::with_capacity(HTTP_HOTPATH_ITERS);
    for _ in 0..HTTP_HOTPATH_ITERS {
        let t0 = Instant::now();
        http_get_reused(addr, 1, body.len()).expect("steady-state http hotpath op");
        totals.push(t0.elapsed().as_nanos() as u64);
    }
    let (_outcome, host, process) = count_all_allocations(|| http_get_reused(addr, 1, body.len()));
    shutdown(runtime);
    (totals, host, process)
}

/// The `hotpath_http1_keepalive_steady_state_tail` row, untraced + traced.
fn probe_http1_keepalive_steady_state_tail() -> (HotPathReport, HotPathReport) {
    let body = small_body();
    let breakdown = instrumented_http1_steady_state(body.clone());
    let (gap_count, gap_max) = gap_stats(&breakdown.stages, GAP_THRESHOLD_NS);

    let (untraced_totals, u_host, u_proc) = steady_state_totals(&body, None);
    let untraced = HotPathReport::from_samples_with_counters(
        "hotpath_http1_keepalive_steady_state_tail",
        untraced_totals,
        breakdown.stages.clone(),
        Some(u_host),
        Some(u_proc),
        breakdown.counters,
    )
    .with_traced(false)
    .with_scheduler_gaps(GAP_THRESHOLD_NS, gap_count, gap_max);

    let timer = Arc::new(StageTimer::default());
    let (traced_totals, t_host, t_proc) =
        steady_state_totals(&body, Some(timer as Arc<dyn TraceObserver>));
    let traced = HotPathReport::from_samples_with_counters(
        "hotpath_http1_keepalive_steady_state_tail_traced",
        traced_totals,
        breakdown.stages,
        Some(t_host),
        Some(t_proc),
        breakdown.counters,
    )
    .with_traced(true)
    .with_scheduler_gaps(GAP_THRESHOLD_NS, gap_count, gap_max);

    (untraced, traced)
}

/// Runs the warmed close/keepalive HTTP timing loop, optionally with a live
/// observer attached. Returns (totals ns, host allocs, process allocs).
fn http1_totals(
    body: &[u8],
    keepalive: bool,
    requests_per_op: usize,
    expected_body_len: usize,
    observer: Option<Arc<dyn TraceObserver>>,
) -> (Vec<u64>, u64, u64) {
    let (runtime, addr, _service) = start_http_runtime(observer, body.to_vec(), keepalive);
    for _ in 0..HTTP_HOTPATH_WARMUP {
        http_get(addr, keepalive, requests_per_op, expected_body_len).expect("warm http request");
    }
    let mut totals = Vec::with_capacity(HTTP_HOTPATH_ITERS);
    for _ in 0..HTTP_HOTPATH_ITERS {
        let t0 = Instant::now();
        http_get(addr, keepalive, requests_per_op, expected_body_len).expect("http hotpath op");
        totals.push(t0.elapsed().as_nanos() as u64);
    }
    let (_outcome, host, process) =
        count_all_allocations(|| http_get(addr, keepalive, requests_per_op, expected_body_len));
    shutdown(runtime);
    (totals, host, process)
}

/// A close/fixed-body HTTP `*_tail` row pair (untraced + traced).
fn probe_http1_tail(
    label: &'static str,
    traced_label: &'static str,
    body: Vec<u8>,
    requests_per_op: usize,
    expected_body_len: usize,
) -> (HotPathReport, HotPathReport) {
    let breakdown = instrumented_http1(body.clone(), false, requests_per_op, expected_body_len);
    let (gap_count, gap_max) = gap_stats(&breakdown.stages, GAP_THRESHOLD_NS);

    let (untraced_totals, u_host, u_proc) =
        http1_totals(&body, false, requests_per_op, expected_body_len, None);
    let untraced = HotPathReport::from_samples_with_counters(
        label,
        untraced_totals,
        breakdown.stages.clone(),
        Some(u_host),
        Some(u_proc),
        breakdown.counters,
    )
    .with_traced(false)
    .with_scheduler_gaps(GAP_THRESHOLD_NS, gap_count, gap_max);

    let timer = Arc::new(StageTimer::default());
    let (traced_totals, t_host, t_proc) = http1_totals(
        &body,
        false,
        requests_per_op,
        expected_body_len,
        Some(timer as Arc<dyn TraceObserver>),
    );
    let traced = HotPathReport::from_samples_with_counters(
        traced_label,
        traced_totals,
        breakdown.stages,
        Some(t_host),
        Some(t_proc),
        breakdown.counters,
    )
    .with_traced(true)
    .with_scheduler_gaps(GAP_THRESHOLD_NS, gap_count, gap_max);

    (untraced, traced)
}

fn probe_http1_close_tail() -> (HotPathReport, HotPathReport) {
    probe_http1_tail(
        "hotpath_http1_close_request_tail",
        "hotpath_http1_close_request_tail_traced",
        small_body(),
        1,
        small_body().len(),
    )
}

fn probe_http1_fixed_body_close_tail() -> (HotPathReport, HotPathReport) {
    probe_http1_tail(
        "hotpath_http1_fixed_body_close_tail",
        "hotpath_http1_fixed_body_close_tail_traced",
        vec![b'x'; HTTP_FIXED_BODY_BYTES],
        1,
        HTTP_FIXED_BODY_BYTES,
    )
}

fn probe_http1_close() -> HotPathReport {
    probe_http1(
        "hotpath_http1_close_request",
        small_body(),
        false,
        1,
        small_body().len(),
    )
}

fn probe_http1_keepalive() -> HotPathReport {
    probe_http1(
        "hotpath_http1_keepalive_sequential",
        small_body(),
        true,
        4,
        small_body().len(),
    )
}

fn probe_http1_fixed_body() -> HotPathReport {
    probe_http1(
        "hotpath_http1_fixed_body_close",
        vec![b'x'; HTTP_FIXED_BODY_BYTES],
        false,
        1,
        HTTP_FIXED_BODY_BYTES,
    )
}

fn probe_http1_keepalive_steady_state() -> HotPathReport {
    let body = small_body();
    let (runtime, addr, _service) = start_http_runtime(None, body.clone(), true);
    for _ in 0..HTTP_HOTPATH_WARMUP {
        http_get_reused(addr, 2, body.len()).expect("warm steady-state http request");
    }
    let mut totals = Vec::with_capacity(HTTP_HOTPATH_ITERS);
    for _ in 0..HTTP_HOTPATH_ITERS {
        let t0 = Instant::now();
        http_get_reused(addr, 1, body.len()).expect("steady-state http hotpath op");
        totals.push(t0.elapsed().as_nanos() as u64);
    }
    let (_outcome, host_allocations, process_allocations) =
        count_all_allocations(|| http_get_reused(addr, 1, body.len()));
    shutdown(runtime);

    let breakdown = instrumented_http1_steady_state(body);
    HotPathReport::from_samples_with_counters(
        "hotpath_http1_keepalive_steady_state_small",
        totals,
        breakdown.stages,
        Some(host_allocations),
        Some(process_allocations),
        breakdown.counters,
    )
}

fn probe_http1(
    label: &'static str,
    body: Vec<u8>,
    keepalive: bool,
    requests_per_op: usize,
    expected_body_len: usize,
) -> HotPathReport {
    let (runtime, addr, _service) = start_http_runtime(None, body.clone(), keepalive);
    for _ in 0..HTTP_HOTPATH_WARMUP {
        http_get(addr, keepalive, requests_per_op, expected_body_len).expect("warm http request");
    }
    let mut totals = Vec::with_capacity(HTTP_HOTPATH_ITERS);
    for _ in 0..HTTP_HOTPATH_ITERS {
        let t0 = Instant::now();
        http_get(addr, keepalive, requests_per_op, expected_body_len).expect("http hotpath op");
        totals.push(t0.elapsed().as_nanos() as u64);
    }
    let (_outcome, host_allocations, process_allocations) =
        count_all_allocations(|| http_get(addr, keepalive, requests_per_op, expected_body_len));
    shutdown(runtime);

    let breakdown = instrumented_http1(body, keepalive, requests_per_op, expected_body_len);
    HotPathReport::from_samples_with_counters(
        label,
        totals,
        breakdown.stages,
        Some(host_allocations),
        Some(process_allocations),
        breakdown.counters,
    )
}

fn instrumented_http1(
    body: Vec<u8>,
    keepalive: bool,
    requests_per_op: usize,
    expected_body_len: usize,
) -> HotPathBreakdown {
    let timer = Arc::new(StageTimer::default());
    let (runtime, addr, service) = start_http_runtime(Some(timer.clone()), body, keepalive);
    http_get(addr, keepalive, requests_per_op, expected_body_len).expect("warm http request");
    // The warm request returns to the std client before the connection isolate
    // necessarily processes every close/tail trace event. Drain that tail so
    // the one instrumented request owns the timeline below.
    std::thread::sleep(Duration::from_millis(20));
    let _ = runtime.has_in_flight_calls();
    timer.clear();
    let t0 = Instant::now();
    http_get(addr, keepalive, requests_per_op, expected_body_len).expect("instrumented http");
    let t_end = Instant::now();
    let events = timer.snapshot();
    shutdown(runtime);
    stages_from_http_timeline(t0, &events, t_end, service)
}

fn instrumented_http1_steady_state(body: Vec<u8>) -> HotPathBreakdown {
    let expected_body_len = body.len();
    let timer = Arc::new(StageTimer::default());
    let (runtime, addr, service) = start_http_runtime(Some(timer.clone()), body, true);
    let mut stream = TcpStream::connect_timeout(&addr, Duration::from_secs(2))
        .expect("connect steady-state instrumented");
    stream
        .set_read_timeout(Some(Duration::from_secs(2)))
        .expect("set read timeout");
    http_get_on_stream(&mut stream, false, expected_body_len).expect("warm steady-state request");
    std::thread::sleep(Duration::from_millis(20));
    let _ = runtime.has_in_flight_calls();
    timer.clear();
    let t0 = Instant::now();
    http_get_on_stream(&mut stream, false, expected_body_len).expect("instrumented steady-state");
    let t_end = Instant::now();
    let events = timer.snapshot();
    let _ = http_get_on_stream(&mut stream, true, expected_body_len);
    drop(stream);
    shutdown(runtime);
    stages_from_http_timeline(t0, &events, t_end, service)
}

fn start_http_runtime(
    observer: Option<Arc<dyn TraceObserver>>,
    body: Vec<u8>,
    keepalive: bool,
) -> (Runtime, SocketAddr, IsolateId) {
    let runtime = new_runtime(observer);
    let service = runtime
        .register_with_capacity::<_, Infallible>(
            BodyService {
                body: Arc::new(body),
            },
            CAP,
        )
        .expect("register body service");
    let mut config = HttpServerConfig::dev();
    if keepalive {
        config.limits.keepalive_idle_timeout = Some(Duration::from_secs(2));
    }
    let listener = runtime
        .register_with_capacity::<_, Infallible>(
            HttpListener::<SingleShard>::with_config(
                "127.0.0.1:0".parse().unwrap(),
                service,
                config,
            ),
            config.listener_mailbox_capacity,
        )
        .expect("register http listener");
    let bound = runtime.observe_next_bound();
    runtime
        .try_send(listener, HttpListenerMsg::Start)
        .expect("start http listener");
    let addr = bound.wait(Duration::from_secs(2)).expect("observe bound");
    (runtime, addr, service.isolate())
}

fn small_body() -> Vec<u8> {
    b"ok\n".to_vec()
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

fn http_get_reused(
    addr: SocketAddr,
    requests: usize,
    expected_body_len: usize,
) -> anyhow::Result<()> {
    let mut stream = TcpStream::connect_timeout(&addr, Duration::from_secs(2))?;
    stream.set_read_timeout(Some(Duration::from_secs(2)))?;
    for index in 0..requests {
        http_get_on_stream(&mut stream, index + 1 == requests, expected_body_len)?;
    }
    Ok(())
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

fn assert_stage_count(report: &HotPathReport, ceiling: usize) {
    assert!(
        report.stages.len() <= ceiling,
        "{} stage_count={} ceiling={ceiling} stages={:?}",
        report.label,
        report.stages.len(),
        report.stages
    );
}

#[test]
fn hotpath_probes_report_and_stay_bounded() {
    let try_send = probe_try_send();
    let send_and_observe = probe_send_and_observe();
    let call_blocking = probe_call_blocking();
    let http_close = probe_http1_close();
    let http_keepalive = probe_http1_keepalive();
    let http_fixed_body = probe_http1_fixed_body();
    let http_steady = probe_http1_keepalive_steady_state();

    // Tail rows: each key path in untraced + traced flavor. The untraced row is
    // the honest distribution headline; the traced row carries the observer's
    // own cost so the delta is visible. Same path, distinct labels + `traced`
    // flag so the two can never be confused in history.
    let (call_blocking_tail, call_blocking_tail_traced) = probe_call_blocking_tail();
    let (http_close_tail, http_close_tail_traced) = probe_http1_close_tail();
    let (http_fixed_tail, http_fixed_tail_traced) = probe_http1_fixed_body_close_tail();
    let (http_steady_tail, http_steady_tail_traced) = probe_http1_keepalive_steady_state_tail();

    let tail_rows = [
        &call_blocking_tail,
        &call_blocking_tail_traced,
        &http_close_tail,
        &http_close_tail_traced,
        &http_fixed_tail,
        &http_fixed_tail_traced,
        &http_steady_tail,
        &http_steady_tail_traced,
    ];
    for report in tail_rows {
        println!("{}", report.summary_line());
        println!("{}", report.json_line());
        assert!(!report.stages.is_empty(), "tail stage breakdown present");
        assert!(
            report.scheduler_gap_threshold_ns == GAP_THRESHOLD_NS,
            "tail row carries the gap threshold: {}",
            report.label
        );
        assert!(
            report.p99_ns >= report.p50_ns,
            "p99 >= p50: {}",
            report.label
        );
        assert!(
            report.p90_ns >= report.p50_ns,
            "p90 >= p50: {}",
            report.label
        );
    }
    // Traced and untraced rows for the same path must be distinguishable: both
    // a distinct label and the `traced` flag.
    for (untraced, traced) in [
        (&call_blocking_tail, &call_blocking_tail_traced),
        (&http_close_tail, &http_close_tail_traced),
        (&http_fixed_tail, &http_fixed_tail_traced),
        (&http_steady_tail, &http_steady_tail_traced),
    ] {
        assert!(!untraced.traced, "untraced flag: {}", untraced.label);
        assert!(traced.traced, "traced flag: {}", traced.label);
        assert_ne!(
            untraced.label, traced.label,
            "traced/untraced labels must differ"
        );
    }

    for report in [
        &try_send,
        &send_and_observe,
        &call_blocking,
        &http_close,
        &http_keepalive,
        &http_fixed_body,
        &http_steady,
    ] {
        println!("{}", report.summary_line());
        println!("{}", report.json_line());
        if report.label.starts_with("hotpath_http1_") {
            assert_eq!(
                report.iterations as usize, HTTP_HOTPATH_ITERS,
                "HTTP iteration count"
            );
        } else {
            assert_eq!(report.iterations as usize, ITERS, "iteration count");
        }
        assert!(!report.stages.is_empty(), "stage breakdown present");
        assert!(
            report.allocations.is_some(),
            "host allocation evidence present"
        );
        assert!(
            report.process_allocations.is_some(),
            "process allocation evidence present for {}",
            report.label
        );
        assert_eq!(
            report.counters.event_stage_count,
            report.stages.len(),
            "event_stage_count mirrors stage_count for {}",
            report.label
        );
    }

    assert_stage_count(&http_close, HTTP_CLOSE_STAGE_CEILING);
    assert_stage_count(&http_keepalive, HTTP_KEEPALIVE_STAGE_CEILING);
    assert_stage_count(&http_fixed_body, HTTP_FIXED_BODY_STAGE_CEILING);
    assert_stage_count(&http_steady, HTTP_STEADY_STAGE_CEILING);
    assert!(
        http_steady.counters.handler_turn_count <= http_keepalive.counters.handler_turn_count,
        "steady-state should not have more handler turns than connect/accept keepalive: steady={} keepalive={}",
        http_steady.counters.handler_turn_count,
        http_keepalive.counters.handler_turn_count
    );
    assert!(
        http_steady.counters.runtime_call_count <= http_keepalive.counters.runtime_call_count,
        "steady-state should not have more backend calls than connect/accept keepalive: steady={} keepalive={}",
        http_steady.counters.runtime_call_count,
        http_keepalive.counters.runtime_call_count
    );

    assert!(
        try_send.min_ns < HANDOFF_CEILING_NS,
        "first queue handoff floor must stay cheap: min={} p50={} ns",
        try_send.min_ns,
        try_send.p50_ns
    );
    assert!(
        send_and_observe.min_ns < OBSERVED_CEILING_NS,
        "observed admission floor must not be millisecond-scale: min={} p50={} ns",
        send_and_observe.min_ns,
        send_and_observe.p50_ns
    );
    assert!(
        call_blocking.min_ns < CALL_CEILING_NS,
        "host call floor must not be millisecond-scale: min={} p50={} ns",
        call_blocking.min_ns,
        call_blocking.p50_ns
    );

    // Pin both host-thread and process-wide allocations. Host pins the
    // caller-side cost; process pins the true per-op cost including everything
    // the worker thread allocates on the caller's behalf.
    let try_send_host = try_send.allocations.expect("try_send host");
    let observed_host = send_and_observe.allocations.expect("send_and_observe host");
    let call_host = call_blocking.allocations.expect("call_blocking host");
    let try_send_process = try_send.process_allocations.expect("try_send process");
    let observed_process = send_and_observe
        .process_allocations
        .expect("send_and_observe process");
    let call_process = call_blocking
        .process_allocations
        .expect("call_blocking process");
    assert!(
        try_send_host <= HANDOFF_HOST_ALLOCATIONS_CEILING,
        "try_send host {} (ceiling {})",
        try_send_host,
        HANDOFF_HOST_ALLOCATIONS_CEILING
    );
    assert!(
        observed_host <= OBSERVED_HOST_ALLOCATIONS_CEILING,
        "send_and_observe host {} (ceiling {})",
        observed_host,
        OBSERVED_HOST_ALLOCATIONS_CEILING
    );
    assert!(
        call_host <= CALL_HOST_ALLOCATIONS_CEILING,
        "call_blocking host {} (ceiling {})",
        call_host,
        CALL_HOST_ALLOCATIONS_CEILING
    );
    assert!(
        try_send_process <= HANDOFF_PROCESS_ALLOCATIONS_CEILING,
        "try_send process {} (ceiling {})",
        try_send_process,
        HANDOFF_PROCESS_ALLOCATIONS_CEILING
    );
    assert!(
        observed_process <= OBSERVED_PROCESS_ALLOCATIONS_CEILING,
        "send_and_observe process {} (ceiling {})",
        observed_process,
        OBSERVED_PROCESS_ALLOCATIONS_CEILING
    );
    assert!(
        call_process <= CALL_PROCESS_ALLOCATIONS_CEILING,
        "call_blocking process {} (ceiling {})",
        call_process,
        CALL_PROCESS_ALLOCATIONS_CEILING
    );

    // Phase 152 buffered-response framing: whole-process allocations for warmed
    // h2c responses on a reused connection. Runs here (single-test binary) so
    // the process counter is not contaminated by a parallel test thread.
    let h2_buffered_allocs =
        http2_steady_state_response_process_allocations(H2_BUFFERED_RESPONSE_REQUESTS)
            .expect("measure h2c buffered response process allocations");
    let per_request = h2_buffered_allocs as f64 / H2_BUFFERED_RESPONSE_REQUESTS as f64;
    println!(
        "perf-h2-alloc requests={H2_BUFFERED_RESPONSE_REQUESTS} process_allocations={h2_buffered_allocs} per_request={per_request:.2}"
    );
    assert!(
        h2_buffered_allocs <= H2_BUFFERED_RESPONSE_ALLOC_CEILING,
        "h2c buffered response allocations {h2_buffered_allocs} exceeded ceiling {H2_BUFFERED_RESPONSE_ALLOC_CEILING} ({per_request:.2}/request)",
    );

    // Rock 5: app-handler turns for a WebSocket session of N text round trips.
    // With one session-rich event per wire event, the whole session (open +
    // N texts + close) costs ~N+a-few app turns; the pre-dedup path delivered a
    // session-rich AND a legacy event for every wire frame, so its text turns
    // alone were 2*N. The ceiling at 2*N is the structural turn-reduction proof.
    let ws_app_turns = websocket_text_round_trip_app_turns(WS_TURN_PROBE_MESSAGES)
        .expect("measure websocket app-handler turns");
    let turn_ceiling = (2 * WS_TURN_PROBE_MESSAGES) as u64;
    let per_message = ws_app_turns as f64 / WS_TURN_PROBE_MESSAGES as f64;
    println!(
        "perf-ws-turns messages={WS_TURN_PROBE_MESSAGES} app_turns={ws_app_turns} per_message={per_message:.2}"
    );
    assert!(
        ws_app_turns >= WS_TURN_PROBE_MESSAGES as u64,
        "each wire text must reach the app at least once: app_turns {ws_app_turns} < messages {WS_TURN_PROBE_MESSAGES}",
    );
    assert!(
        ws_app_turns < turn_ceiling,
        "WebSocket app turns {ws_app_turns} not below the pre-dedup 2*N={turn_ceiling}; duplicate delivery may have returned",
    );
}
