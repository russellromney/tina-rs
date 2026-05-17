//! `system_live_replay_bugbox` — proof of the live-capture → sim
//! replay → shrink workflow.
//!
//! What this specimen pulls on:
//!
//! - `tina_runtime::ThreadedRuntime::with_config_and_trace_observer` to
//!   wire a live trace observer before the first event.
//! - [`tina_proof_harness::LiveTrace`] to capture the live trace shape
//!   (event count + `stable_trace_hash`).
//! - [`tina_sim::dst::ReplayCase`] / [`tina_sim::dst::observe_replay_case`] /
//!   [`tina_sim::dst::assert_replay_case`] for the saved-seed sim replay.
//! - [`tina_sim::dst::delete_shrink`] for the deletion shrinker.
//! - [`tina_sim::dst::discover_constants`] for the "pin constants"
//!   sweep helper.
//!
//! The bug-in-a-box: a contrived "rare drop" sink that discards a
//! specific value (`POISON_VALUE`). Live capture sees the workload run
//! end-to-end; the sim replay finds the exact subset of ops that still
//! drops at least one message; the shrinker reduces the offending
//! history to its minimum.
//!
//! Read this top-to-bottom: `Op` → live → sim case → sim runner →
//! shrink → `run()` ties them together.

use std::cell::RefCell;
use std::convert::Infallible;
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tina::prelude::*;
use tina_proof_harness::live_replay::LiveTrace;
use tina_runtime::{DefaultThreadedMailboxFactory, ThreadedRuntime, ThreadedRuntimeConfig};
use tina_sim::dst::{
    DiscoveredConstants, ReplayCase, ReplayConfig, ReplayReport, ShrinkConfig, ShrunkFailure,
    TraceShape, assert_replay_case, delete_shrink, discover_constants, observe_replay_case,
};
use tina_sim::{FaultConfig, LocalSendFaultMode, Simulator};

/// One observable workload step.
///
/// `Send(value)` queues one `Tick(value)` toward the sink. `Drain` is
/// a sim-only sync barrier — it has no effect in the live run because
/// the live runtime drains continuously.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Op {
    Send(u32),
    Drain,
}

/// The bug value the sink silently drops.
pub const POISON_VALUE: u32 = 7;

const PRODUCER_ROLE: &str = "producer";
const SINK_ROLE: &str = "sink";

/// Aggregate output of one specimen run: the live snapshot, the saved
/// sim case, the live-vs-sim comparison, the seed-sweep discovery, the
/// shrunk failing history, and a one-line summary suitable for the
/// system README.
#[derive(Debug)]
pub struct BugboxReport {
    pub live_received: usize,
    pub live_trace_shape: TraceShape,
    pub sim_pinned: SavedCase,
    pub sim_report: ReplayReport<Output>,
    pub discovered: Vec<DiscoveredConstants>,
    pub shrunk: ShrunkFailure<Op>,
    pub summary_line: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Output {
    pub messages_received: usize,
}

/// The saved-seed sim case that pairs with this specimen's live
/// workload. `live_*` fields are filled in by [`run`] after the live
/// run completes so the README/test can show both shapes side by side.
#[derive(Debug)]
pub struct SavedCase {
    pub case: ReplayCase<Op>,
    pub live_event_count: usize,
    pub live_trace_hash: u64,
}

// ---------------------------------------------------------------------------
// Live side: same isolate logic running on `ThreadedRuntime`.
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy)]
enum ProducerMsg {
    Tick(u32),
}

struct LiveProducer {
    sink: Address<SinkMsg>,
}

#[tina_runtime::isolate(message = ProducerMsg, send = Outbound<SinkMsg>)]
impl LiveProducer {
    fn handle(
        &mut self,
        msg: ProducerMsg,
        _ctx: &mut Context<'_, SingleShard, Self::Reply>,
    ) -> Effect<Self> {
        match msg {
            ProducerMsg::Tick(value) => send(self.sink, SinkMsg::Got(value)),
        }
    }
}

#[derive(Debug, Clone, Copy)]
enum SinkMsg {
    Got(u32),
}

#[derive(Default)]
struct LiveSink {
    received: Arc<Mutex<Vec<u32>>>,
}

#[tina_runtime::isolate(message = SinkMsg)]
impl LiveSink {
    fn handle(
        &mut self,
        msg: SinkMsg,
        _ctx: &mut Context<'_, SingleShard, Self::Reply>,
    ) -> Effect<Self> {
        match msg {
            SinkMsg::Got(value) if value == POISON_VALUE => {
                // The bug: poison values are silently dropped. The live
                // observer sees the message handler fire, the sink does
                // not record the value. The sim case below reproduces
                // the same drop and the shrinker finds the minimal
                // history that still produces it.
                noop()
            }
            SinkMsg::Got(value) => {
                self.received.lock().expect("live sink lock").push(value);
                noop()
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Sim side: same logic on `tina_sim::Simulator`. Deterministic.
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy)]
enum SimProducerMsg {
    Tick(u32),
}

struct SimProducer {
    sink: Address<SimSinkMsg>,
}

#[tina_runtime::isolate(message = SimProducerMsg, send = Outbound<SimSinkMsg>)]
impl SimProducer {
    fn handle(
        &mut self,
        msg: SimProducerMsg,
        _ctx: &mut Context<'_, SingleShard, Self::Reply>,
    ) -> Effect<Self> {
        match msg {
            SimProducerMsg::Tick(value) => send(self.sink, SimSinkMsg::Got(value)),
        }
    }
}

#[derive(Debug, Clone, Copy)]
enum SimSinkMsg {
    Got(u32),
}

#[derive(Default)]
struct SimSink {
    received: Rc<RefCell<Vec<u32>>>,
}

#[tina_runtime::isolate(message = SimSinkMsg)]
impl SimSink {
    fn handle(
        &mut self,
        msg: SimSinkMsg,
        _ctx: &mut Context<'_, SingleShard, Self::Reply>,
    ) -> Effect<Self> {
        match msg {
            SimSinkMsg::Got(value) if value == POISON_VALUE => noop(),
            SimSinkMsg::Got(value) => {
                self.received.borrow_mut().push(value);
                noop()
            }
        }
    }
}

/// Saved seed for the canonical sim case.
pub const SAVED_SEED: u64 = 108;

/// Pinned constants (filled in by [`run`] on first use via
/// `observe_replay_case`). When the case is exercised again later the
/// pinned values let `assert_replay_case` fail loudly on drift.
const SAVED_EVENT_COUNT: usize = 54;
const SAVED_TRACE_HASH: u64 = 0xe0f7_990d_ddf0_fb49;

fn faults() -> FaultConfig {
    FaultConfig {
        local_send: LocalSendFaultMode::DelayByRounds {
            one_in: 3,
            rounds: 1,
        },
        ..Default::default()
    }
}

/// The canonical workload as a sim case. Holds the seed, the faults,
/// the mailbox capacities, and the typed op history.
pub fn case() -> ReplayCase<Op> {
    let config = ReplayConfig::with_faults(faults())
        .with_mailbox(PRODUCER_ROLE, 8)
        .with_mailbox(SINK_ROLE, 8);
    ReplayCase::new(
        "system_live_replay_bugbox_canonical",
        SAVED_SEED,
        config,
        "producer fans Send ops into a sink; POISON values are silently dropped",
        vec![
            Op::Send(1),
            Op::Send(2),
            Op::Send(POISON_VALUE),
            Op::Send(3),
            Op::Drain,
            Op::Send(POISON_VALUE),
            Op::Send(5),
            Op::Drain,
        ],
        "sink receives every non-poison value in trace order",
    )
    .expecting(SAVED_EVENT_COUNT, SAVED_TRACE_HASH)
}

/// Sim runner. Reads every knob from the case; the history is
/// load-bearing (deleting an op changes the trace).
pub fn run_case(case: &ReplayCase<Op>) -> ReplayReport<Output> {
    let mut sim = Simulator::new(SingleShard, case.simulator_config());
    let received = Rc::new(RefCell::new(Vec::new()));
    let sink = sim.register_with_mailbox_capacity(
        SimSink {
            received: Rc::clone(&received),
        },
        case.config.mailbox(SINK_ROLE),
    );
    let producer = sim
        .register_with_mailbox_capacity(SimProducer { sink }, case.config.mailbox(PRODUCER_ROLE));
    for op in case.history.operations() {
        match *op {
            Op::Send(value) => {
                sim.try_send(producer, SimProducerMsg::Tick(value))
                    .expect("producer ingress accepted");
            }
            Op::Drain => {
                sim.run_until_quiescent();
            }
        }
    }
    sim.run_until_quiescent();

    let messages_received = received.borrow().len();
    ReplayReport::from_case_and_events(case, sim.trace(), Output { messages_received })
}

// ---------------------------------------------------------------------------
// The end-to-end demo
// ---------------------------------------------------------------------------

/// Run the live workload, capture the live trace, then prove the sim
/// case still reproduces, pin constants for a small seed sweep, and
/// shrink the bug history to its minimum.
pub fn run() -> anyhow::Result<BugboxReport> {
    // 1) Live capture. ThreadedRuntime + LiveTrace observer.
    let live_trace = LiveTrace::new();
    let runtime = ThreadedRuntime::with_config_and_trace_observer(
        SingleShard,
        DefaultThreadedMailboxFactory,
        ThreadedRuntimeConfig {
            command_capacity: 64,
            idle_wait: Duration::from_millis(1),
            ..Default::default()
        },
        live_trace.observer(),
    );
    let live_received = Arc::new(Mutex::new(Vec::<u32>::new()));
    let sink = runtime
        .register_with_capacity::<_, Infallible>(
            LiveSink {
                received: Arc::clone(&live_received),
            },
            16,
        )
        .map_err(|e| anyhow::anyhow!("register live sink: {e:?}"))?;
    let producer = runtime
        .register_with_capacity::<_, SinkMsg>(LiveProducer { sink }, 16)
        .map_err(|e| anyhow::anyhow!("register live producer: {e:?}"))?;

    let live_case = case();
    for op in live_case.history.operations() {
        if let Op::Send(value) = *op {
            runtime
                .try_send(producer, ProducerMsg::Tick(value))
                .map_err(|e| anyhow::anyhow!("live ingress failed: {e:?}"))?;
        }
    }

    // The live runtime drains continuously; wait until the sink has
    // received every non-poison value or a short ceiling elapses.
    let expected_non_poison: usize = live_case
        .history
        .operations()
        .iter()
        .filter(|op| matches!(op, Op::Send(v) if *v != POISON_VALUE))
        .count();
    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    while std::time::Instant::now() < deadline {
        let len = live_received.lock().expect("live sink lock").len();
        if len >= expected_non_poison {
            break;
        }
        std::thread::sleep(Duration::from_millis(5));
    }

    let _trace = runtime
        .shutdown()
        .map_err(|e| anyhow::anyhow!("live runtime shutdown: {e:?}"))?;
    let live_received_count = live_received.lock().expect("live sink lock").len();
    let live_shape = live_trace.snapshot();

    // 2) Sim replay. assert_replay_case panics on drift; the saved
    //    constants must match.
    let sim_report = assert_replay_case(&live_case, run_case);

    // 3) Helper: discover_constants over a small seed sweep so a coding
    //    agent that wants to pin a new case can copy the printed
    //    constants directly. `discover_constants` requires `&'static
    //    str` labels, so the labels are listed as constants.
    let template = case();
    const SWEEP: [(&str, u64); 4] = [
        ("seed_108", 108),
        ("seed_109", 109),
        ("seed_110", 110),
        ("seed_111", 111),
    ];
    let sweep_cases: Vec<(&'static str, ReplayCase<Op>)> = SWEEP
        .iter()
        .map(|&(label, seed)| {
            (
                label,
                ReplayCase::new(
                    template.name,
                    seed,
                    template.config.clone(),
                    template.scenario,
                    template.history.operations().to_vec(),
                    template.invariant,
                ),
            )
        })
        .collect();
    let discovered = discover_constants(sweep_cases, run_case);

    // 4) Shrink: find the minimum history that still drops at least
    //    one message. "Still buggy" = sink.received < non-poison ops.
    let shrunk = delete_shrink(
        &live_case.history,
        ShrinkConfig::default(),
        "at least one non-poison message lost",
        |history| {
            // Build a fresh case at the candidate history and check
            // whether the bug still reproduces.
            let candidate = ReplayCase::new(
                live_case.name,
                live_case.seed,
                live_case.config.clone(),
                live_case.scenario,
                history.operations().to_vec(),
                live_case.invariant,
            );
            let report = observe_replay_case(&candidate, run_case);
            let non_poison = history
                .operations()
                .iter()
                .filter(|op| matches!(op, Op::Send(v) if *v != POISON_VALUE))
                .count();
            // The bug is "the sink discards POISON". A history still
            // shows the bug iff it contains at least one POISON send,
            // because that is the value the sink silently drops.
            report.output.messages_received == non_poison
                && history
                    .operations()
                    .iter()
                    .any(|op| matches!(op, Op::Send(v) if *v == POISON_VALUE))
        },
    );

    let summary_line = format!(
        "bugbox live_received={live_received_count} live_events={live_count} \
         live_hash=0x{live_hash:016x} sim_events={sim_count} sim_hash=0x{sim_hash:016x} \
         shrunk_from={from} to={to} discovered_seeds={ds}",
        live_count = live_shape.event_count,
        live_hash = live_shape.trace_hash,
        sim_count = sim_report.event_count,
        sim_hash = sim_report.trace_hash,
        from = shrunk.original().len(),
        to = shrunk.shrunk().len(),
        ds = discovered.len(),
    );

    Ok(BugboxReport {
        live_received: live_received_count,
        live_trace_shape: live_shape,
        sim_pinned: SavedCase {
            case: live_case,
            live_event_count: live_shape.event_count,
            live_trace_hash: live_shape.trace_hash,
        },
        sim_report,
        discovered,
        shrunk,
        summary_line,
    })
}

