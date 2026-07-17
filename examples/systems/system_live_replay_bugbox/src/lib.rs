//! `system_live_replay_bugbox` — proof of the live-capture → sim
//! replay → shrink workflow.
//!
//! What this specimen pulls on:
//!
//! - `tina_runtime::ThreadedRuntime::try_with_config_and_trace_observer` to
//!   fallibly start the worker and wire a live trace observer before the first
//!   event.
//!   `LocalSystemBuilder::trace_observer(...).try_build()` already supports
//!   this shape; migrating this example to that facade remains follow-up work.
//! - [`tina_proof_harness::LiveTrace`] to capture the live trace shape
//!   (event count + `stable_trace_hash`).
//! - [`tina_sim::dst::capture_overload_run`] /
//!   [`tina_sim::dst::replay_overload_bug`] /
//!   [`tina_sim::dst::shrink_captured_replay`] for the live-derived saved case.
//! - [`tina_sim::dst::ReplayCase`] / [`tina_sim::dst::assert_replay_case`] for
//!   the saved-seed sim replay.
//! - [`tina_sim::dst::discover_constants`] for the "pin constants"
//!   sweep helper.
//!
//! The bug-in-a-box: a contrived "rare drop" sink that discards a
//! specific value (`POISON_VALUE`). Live capture sees the workload run
//! end-to-end; the sim replay finds the exact subset of ops that still
//! drops at least one message; the shrinker reduces the offending
//! history while preserving the fact set that made the live capture
//! interesting.
//!
//! Read this top-to-bottom: `Op` → live → sim case → sim runner →
//! shrink → `run()` ties them together.

use std::convert::Infallible;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use tina::capacity::{CapacityMode, CapacitySurfaceReport};
use tina::prelude::*;
use tina_proof_harness::live_replay::LiveTrace;
use tina_runtime::{DefaultThreadedMailboxFactory, ThreadedRuntime, ThreadedRuntimeConfig};
use tina_sim::dst::{
    CaptureSource, CaptureSummary, DiscoveredConstants, LiveReplayCapture, LiveReplayFact,
    LiveReplayReport, ReplayCase, ReplayConfig, ReplayReport, ShrinkCapturedReport, ShrinkConfig,
    TraceProjection, TraceShape, assert_captured_replay, assert_no_hidden_buffering,
    assert_replay_case, capture_overload_run, check_captured_replay, discover_constants,
    read_saved_replay_case, replay_overload_bug, save_overload_bug, shrink_captured_replay,
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
/// sim case, the live pressure snapshot, the seed-sweep discovery, the
/// shrunk failing history, and a one-line summary suitable for the
/// system README.
#[derive(Debug)]
pub struct BugboxReport {
    pub live_received: usize,
    pub live_trace_shape: TraceShape,
    /// Pressure summary from the live trace (Pressure facts
    /// visible"). A clean run has `non_zero() == false`.
    pub live_pressure: tina_runtime::PressureSummary,
    pub sim_pinned: SavedCase,
    pub sim_report: ReplayReport<Output>,
    pub capture: LiveReplayCapture<Op>,
    pub capture_summary: CaptureSummary,
    pub capture_replay: LiveReplayReport<Output>,
    pub discovered: Vec<DiscoveredConstants>,
    pub shrunk: ShrinkCapturedReport<Op, Output>,
    pub unsupported_mismatch_seen: bool,
    pub saved_bugbox_path: PathBuf,
    pub summary_line: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Output {
    pub messages_received: usize,
    pub poison_sent: bool,
}

fn encode_op(op: &Op) -> String {
    match op {
        Op::Send(value) => format!("send:{value}"),
        Op::Drain => "drain".to_owned(),
    }
}

fn decode_op(text: &str) -> Result<Op, String> {
    if text == "drain" {
        return Ok(Op::Drain);
    }
    let Some(value) = text.strip_prefix("send:") else {
        return Err(format!("unknown op {text:?}"));
    };
    Ok(Op::Send(
        value.parse::<u32>().map_err(|error| error.to_string())?,
    ))
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
    /// Host has finished the workload. Forwarded after every prior Tick so
    /// the sink can settle its private receive list and `stop_with` the count.
    Finish,
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
            ProducerMsg::Finish => send(self.sink, SinkMsg::Finish),
        }
    }
}

#[derive(Debug, Clone, Copy)]
enum SinkMsg {
    Got(u32),
    Finish,
}

struct LiveSink {
    received: Vec<u32>,
    /// Whether the workload history included a poison value. Set by the host
    /// via the final Finish path so the terminal report matches the sim fact.
    poison_sent: bool,
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
                self.received.push(value);
                noop()
            }
            SinkMsg::Finish => stop_with(Output {
                messages_received: self.received.len(),
                poison_sent: self.poison_sent,
            }),
        }
    }
}

// ---------------------------------------------------------------------------
// Sim side: same logic on `tina_sim::Simulator`. Deterministic.
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy)]
enum SimProducerMsg {
    Tick(u32),
    Finish,
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
            SimProducerMsg::Finish => send(self.sink, SimSinkMsg::Finish),
        }
    }
}

#[derive(Debug, Clone, Copy)]
enum SimSinkMsg {
    Got(u32),
    Finish,
}

struct SimSink {
    received: Vec<u32>,
    poison_sent: bool,
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
                self.received.push(value);
                noop()
            }
            SimSinkMsg::Finish => stop_with(Output {
                messages_received: self.received.len(),
                poison_sent: self.poison_sent,
            }),
        }
    }
}

/// Saved seed for the canonical sim case.
pub const SAVED_SEED: u64 = 108;

/// Pinned constants for the canonical sim case including the post-history
/// `Finish` settlement that mirrors the live `observe_result` path. When the
/// case is exercised again later the pinned values let `assert_replay_case`
/// fail loudly on drift.
const SAVED_EVENT_COUNT: usize = 63;
const SAVED_TRACE_HASH: u64 = 0xa3e8_b8cf_1de3_94fd;

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
        .with_mailbox(SINK_ROLE, 8)
        // Live capture uses `ThreadedRuntime`, which registers a host-call
        // dispatcher pool at worker startup. Reserve the same id range in
        // the sim so user-isolate ids match and the captured trace replays
        // exactly.
        .with_reserved_system_isolates(tina_runtime::HOST_CALL_DISPATCHER_POOL_SIZE);
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
    run_case_with_events(case).0
}

fn run_case_with_events(
    case: &ReplayCase<Op>,
) -> (ReplayReport<Output>, Vec<tina_runtime::RuntimeEvent>) {
    let mut sim = Simulator::new(SingleShard, case.simulator_config());
    let poison_sent = case
        .history
        .operations()
        .iter()
        .any(|op| matches!(op, Op::Send(v) if *v == POISON_VALUE));
    let sink = sim.register_with_mailbox_capacity(
        SimSink {
            received: Vec::new(),
            poison_sent,
        },
        case.config.mailbox(SINK_ROLE),
    );
    let waiter = sim
        .observe_result::<Output, _, _>(sink)
        .expect("claim sink result before workload");
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
    // Settlement is outside the named history ops but is part of the runner
    // contract: same Finish path the live host uses for observe_result.
    sim.try_send(producer, SimProducerMsg::Finish)
        .expect("producer finish accepted");
    sim.run_until_quiescent();

    let output = waiter
        .wait(Duration::ZERO)
        .expect("sink settled after Finish");
    let events = sim.trace().to_vec();
    (
        ReplayReport::from_case_and_events(case, &events, output),
        events,
    )
}

fn replay_fact(output: &Output) -> LiveReplayFact {
    LiveReplayFact::capacity_surface(&CapacitySurfaceReport::count(
        "bugbox.sink.receive-count",
        CapacityMode::Fixed,
        8,
        0,
        output.messages_received,
        0,
    ))
}

fn run_captured_case(
    case: &ReplayCase<Op>,
) -> Result<LiveReplayReport<Output>, tina_sim::dst::TraceProjectionError> {
    let report = run_case(case);
    let fact = replay_fact(&report.output);
    Ok(LiveReplayReport::exact(report).with_live_fact(fact))
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
    let runtime = Arc::new(ThreadedRuntime::try_with_config_and_trace_observer(
        SingleShard,
        DefaultThreadedMailboxFactory,
        ThreadedRuntimeConfig {
            command_capacity: 64,
            idle_wait: Duration::from_millis(1),
            ..Default::default()
        },
        live_trace.observer(),
    )?);
    let shutdown = runtime.shutdown_handle();

    let live_case = case();
    let poison_sent = live_case
        .history
        .operations()
        .iter()
        .any(|op| matches!(op, Op::Send(v) if *v == POISON_VALUE));

    let sink = runtime
        .register_with_capacity::<_, Infallible>(
            LiveSink {
                received: Vec::new(),
                poison_sent,
            },
            16,
        )
        .map_err(|e| anyhow::anyhow!("register live sink: {e:?}"))?;
    // Claim the terminal receive report before any workload message can stop
    // the sink.
    let waiter = runtime
        .observe_result::<Output, _, _>(sink)
        .map_err(|e| anyhow::anyhow!("observe_result: {e:?}"))?;
    let producer = runtime
        .register_with_capacity::<_, SinkMsg>(LiveProducer { sink }, 16)
        .map_err(|e| anyhow::anyhow!("register live producer: {e:?}"))?;

    for op in live_case.history.operations() {
        if let Op::Send(value) = *op {
            runtime
                .try_send(producer, ProducerMsg::Tick(value))
                .map_err(|e| anyhow::anyhow!("live ingress failed: {e:?}"))?;
        }
    }
    // Finish is ordered after every Tick in the producer mailbox, so every
    // Got reaches the sink before the sink settles its terminal report.
    runtime
        .try_send(producer, ProducerMsg::Finish)
        .map_err(|e| anyhow::anyhow!("live finish failed: {e:?}"))?;

    let live_output = waiter
        .wait(Duration::from_secs(2))
        .map_err(|e| anyhow::anyhow!("live sink did not settle: {e:?}"))?;
    let live_received_count = live_output.messages_received;

    let terminal = shutdown.request_and_wait_report(Duration::from_secs(5))?;
    drop(runtime);
    terminal.ensure_clean()?;
    let live_shape = live_trace.snapshot();
    let live_events = live_trace.events();
    let live_pressure = live_trace.pressure_summary();

    // 2) Sim replay. assert_replay_case panics on drift; the saved
    //    constants must match.
    let sim_report = assert_replay_case(&live_case, run_case);
    let fact = replay_fact(&sim_report.output);

    // 3) Live capture -> save -> read -> replay. The capture keeps the live
    //    trace shape, explicit replay config/history, typed facts, and source
    //    metadata together.
    let receive_count_surface = CapacitySurfaceReport::count(
        "bugbox.sink.receive-count",
        CapacityMode::Fixed,
        8,
        0,
        live_received_count,
        0,
    );
    assert_no_hidden_buffering(&receive_count_surface);
    let capture = capture_overload_run(live_case.name)
        .with_seed(live_case.seed)
        .with_config(live_case.config.clone())
        .with_scenario(live_case.scenario)
        .with_history(live_case.history.operations().to_vec())
        .with_invariant(live_case.invariant)
        .with_source("system_live_replay_bugbox live smoke")
        .with_source_metadata(
            CaptureSource::new("system_live_replay_bugbox live smoke")
                .runtime_kind("threaded")
                .backend("live"),
        )
        .with_projection(TraceProjection::Exact)
        .with_trace(&live_events)
        .with_capacity_summary(&receive_count_surface)
        .finish()?;
    let capture_summary = capture.summary();

    let path = std::env::temp_dir().join(format!(
        "system-live-replay-bugbox-{}-{}-{}.case",
        std::process::id(),
        capture.expected.trace_hash,
        // Parallel tests in one process share pid + hash; uniquify the path.
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0),
    ));
    let bugbox_save = save_overload_bug(&path, &capture, encode_op)?;
    let saved = read_saved_replay_case(&path, decode_op)?;
    let replay_case = saved.to_replay_case(
        live_case.name,
        live_case.config.clone(),
        live_case.scenario,
        live_case.invariant,
    )?;
    let capture_replay = replay_overload_bug(&capture, &replay_case, run_captured_case)
        .map_err(|error| anyhow::anyhow!("overload replay failed: {error}"))?;
    let asserted_capture_replay = assert_captured_replay(&capture, &replay_case, run_captured_case);
    assert_eq!(capture_replay, asserted_capture_replay);
    assert_eq!(capture_replay.live_facts, vec![fact.clone()]);

    let unsupported = capture.clone().with_unsupported_fact(
        "wall-clock drain timing",
        "live runtime drains continuously",
    );
    let unsupported_mismatch_seen = check_captured_replay(
        &unsupported,
        &unsupported.to_replay_case(),
        run_captured_case,
    )
    .is_err();

    // 4) Helper: discover_constants over a small seed sweep so a coding
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

    // 5) Shrink: find the minimum live-derived capture that still drops at
    //    least one poison message while preserving the replay fact set.
    let shrunk = shrink_captured_replay(
        &capture,
        ShrinkConfig::default(),
        "at least one poison message is silently dropped",
        run_captured_case,
        |report| report.replay.output.poison_sent,
    )?;

    let summary_line = format!(
        "bugbox live_received={live_received_count} live_events={live_count} \
         live_hash=0x{live_hash:016x} sim_events={sim_count} sim_hash=0x{sim_hash:016x} \
         shrunk_from={from} to={to} discovered_seeds={ds} \
         live_pressure_nonzero={pressure_nonzero} capture_blocked={capture_blocked} \
         unsupported_proof={unsupported_mismatch_seen} saved_bugbox={saved_path}",
        live_count = live_shape.event_count,
        live_hash = live_shape.trace_hash,
        sim_count = sim_report.event_count,
        sim_hash = sim_report.trace_hash,
        from = shrunk.original_len,
        to = shrunk.shrunk_len,
        ds = discovered.len(),
        pressure_nonzero = live_pressure.non_zero(),
        capture_blocked = capture_summary.replay_blocked,
        saved_path = bugbox_save.path.display(),
    );

    Ok(BugboxReport {
        live_received: live_received_count,
        live_trace_shape: live_shape,
        live_pressure,
        sim_pinned: SavedCase {
            case: live_case,
            live_event_count: live_shape.event_count,
            live_trace_hash: live_shape.trace_hash,
        },
        sim_report,
        capture,
        capture_summary,
        capture_replay,
        discovered,
        shrunk,
        unsupported_mismatch_seen,
        saved_bugbox_path: bugbox_save.path,
        summary_line,
    })
}
