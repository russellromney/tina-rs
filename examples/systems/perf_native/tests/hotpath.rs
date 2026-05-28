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
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use perf_native::count_host_allocations;
use tina::prelude::*;
use tina_proof_harness::{HotPathReport, HotPathStage};
use tina_runtime::{
    CallOutcome, DefaultThreadedMailboxFactory, RuntimeEvent, RuntimeEventKind, ThreadedRuntime,
    ThreadedRuntimeConfig, TraceObserver,
};

const CAP: usize = 512;
const ITERS: usize = 200;
const WARMUP: usize = 40;
const CALL_TIMEOUT: Duration = Duration::from_secs(2);

// Loose latency ceilings. They exist to catch a regression back to
// millisecond-scale local work, not to pin an exact number on a shared
// machine. Tiny same-shard work should sit far below these after the
// worker-loop fix.
const HANDOFF_CEILING_NS: u64 = 100_000; // 100us
const OBSERVED_CEILING_NS: u64 = 500_000; // 500us
const CALL_CEILING_NS: u64 = 500_000; // 500us

// Pinned warmed allocation ceilings (one host-thread op past warmup). These
// catch a host-side allocation regression — e.g., a future change that boxes
// or channel-allocates extra per call — without locking down a fragile exact
// count. Observed steady-state today: try_send=1, send_and_observe=4,
// call_blocking=4. Headroom is small on purpose.
const HANDOFF_ALLOCATIONS_CEILING: u64 = 2;
const OBSERVED_ALLOCATIONS_CEILING: u64 = 6;
const CALL_ALLOCATIONS_CEILING: u64 = 8;

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

/// Records the wall-clock instant of every worker-thread trace event so a
/// single instrumented op can be broken into per-turn stages. The lock is
/// only contended while the host clears or snapshots, both done when the
/// worker is idle.
#[derive(Default)]
struct StageTimer {
    events: Mutex<Vec<(&'static str, Instant)>>,
}

impl StageTimer {
    fn clear(&self) {
        self.events.lock().expect("stage timer lock").clear();
    }

    fn snapshot(&self) -> Vec<(&'static str, Instant)> {
        self.events.lock().expect("stage timer lock").clone()
    }
}

impl TraceObserver for StageTimer {
    fn on_event(&self, event: &RuntimeEvent) {
        let label = kind_label(event.kind());
        self.events
            .lock()
            .expect("stage timer lock")
            .push((label, Instant::now()));
    }
}

fn kind_label(kind: RuntimeEventKind) -> &'static str {
    match kind {
        RuntimeEventKind::MailboxAccepted => "mbox_accepted",
        RuntimeEventKind::HandlerStarted => "handler_started",
        RuntimeEventKind::HandlerFinished { .. } => "handler_finished",
        RuntimeEventKind::EffectObserved { .. } => "effect_observed",
        RuntimeEventKind::SendDispatchAttempted { .. } => "send_attempted",
        RuntimeEventKind::SendAccepted { .. } => "send_accepted",
        RuntimeEventKind::SendRejected { .. } => "send_rejected",
        _ => "other",
    }
}

fn new_runtime(observer: Option<Arc<dyn TraceObserver>>) -> Runtime {
    let config = ThreadedRuntimeConfig {
        command_capacity: CAP,
        ..ThreadedRuntimeConfig::default()
    };
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
fn stages_from_timeline(
    t0: Instant,
    events: &[(&'static str, Instant)],
    t_end: Instant,
) -> Vec<HotPathStage> {
    let mut stages = Vec::new();
    let mut counts: HashMap<String, u32> = HashMap::new();
    let mut prev_label = "host_submit";
    let mut prev_at = t0;
    for &(label, at) in events {
        if at > t_end {
            continue;
        }
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
    stages
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
    let (_outcome, allocations) =
        count_host_allocations(|| runtime.try_send(addr, CounterMsg::Hit));

    let p50 = median(&totals);
    shutdown(runtime);
    HotPathReport::from_samples(
        "hotpath_try_send",
        totals,
        vec![HotPathStage::new("host_submit_to_command_accepted", p50)],
        Some(allocations),
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
    let (_outcome, allocations) =
        count_host_allocations(|| runtime.send_and_observe(addr, CounterMsg::Hit));
    shutdown(runtime);

    let stages = instrumented_send_and_observe();
    HotPathReport::from_samples(
        "hotpath_send_and_observe",
        totals,
        stages,
        Some(allocations),
    )
}

fn instrumented_send_and_observe() -> Vec<HotPathStage> {
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
    let (_outcome, allocations) =
        count_host_allocations(|| runtime.call_blocking(ping, PingMsg::Ping, CALL_TIMEOUT));
    shutdown(runtime);

    let stages = instrumented_call_blocking();
    HotPathReport::from_samples("hotpath_call_blocking", totals, stages, Some(allocations))
}

fn instrumented_call_blocking() -> Vec<HotPathStage> {
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

#[test]
fn hotpath_probes_report_and_stay_bounded() {
    let try_send = probe_try_send();
    let send_and_observe = probe_send_and_observe();
    let call_blocking = probe_call_blocking();

    for report in [&try_send, &send_and_observe, &call_blocking] {
        println!("{}", report.summary_line());
        println!("{}", report.json_line());
        assert!(report.iterations as usize == ITERS, "iteration count");
        assert!(!report.stages.is_empty(), "stage breakdown present");
        assert!(
            report.allocations.is_some(),
            "host allocation evidence present"
        );
    }

    assert!(
        try_send.p50_ns < HANDOFF_CEILING_NS,
        "first queue handoff must stay cheap: {} ns",
        try_send.p50_ns
    );
    assert!(
        send_and_observe.p50_ns < OBSERVED_CEILING_NS,
        "observed admission must not be millisecond-scale: {} ns",
        send_and_observe.p50_ns
    );
    assert!(
        call_blocking.p50_ns < CALL_CEILING_NS,
        "host call must not be millisecond-scale: {} ns",
        call_blocking.p50_ns
    );

    // Pin warmed host-thread allocations so a future regression that boxes or
    // channel-allocates extra per op fails loudly rather than hiding in the
    // reported number. Steady-state today: 1 / 4 / 4.
    let try_send_allocations = try_send.allocations.expect("try_send allocations");
    let observed_allocations = send_and_observe
        .allocations
        .expect("send_and_observe allocations");
    let call_allocations = call_blocking
        .allocations
        .expect("call_blocking allocations");
    assert!(
        try_send_allocations <= HANDOFF_ALLOCATIONS_CEILING,
        "try_send allocates {} per op (ceiling {})",
        try_send_allocations,
        HANDOFF_ALLOCATIONS_CEILING
    );
    assert!(
        observed_allocations <= OBSERVED_ALLOCATIONS_CEILING,
        "send_and_observe allocates {} per op (ceiling {})",
        observed_allocations,
        OBSERVED_ALLOCATIONS_CEILING
    );
    assert!(
        call_allocations <= CALL_ALLOCATIONS_CEILING,
        "call_blocking allocates {} per op (ceiling {})",
        call_allocations,
        CALL_ALLOCATIONS_CEILING
    );
}
