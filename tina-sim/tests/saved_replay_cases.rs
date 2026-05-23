//! Service-shaped saved [`ReplayCase`] proofs.
//!
//! Each case pins:
//!
//! - a real Tina pressure or lifecycle fact (mailbox `Full`,
//!   `Closed`, etc.);
//! - the seed and `ReplayConfig`;
//! - the explicit history;
//! - the observed event count and `stable_trace_hash`.
//!
//! These tests are the regression form of "saved seed, saved bug".

use std::cell::RefCell;
use std::rc::Rc;

use tina::capacity::{CapacityMode, CapacitySurfaceReport};
use tina::prelude::*;
use tina::{IsolateId, ShardId};
use tina_runtime::{EventId, RuntimeEvent, RuntimeEventKind, SendRejectedReason};
use tina_sim::dst::{
    CaptureLimits, CaptureSource, LiveReplayCapture, LiveReplayFact, LiveReplayReport, ReplayCase,
    ReplayConfig, ReplayReport, RuntimeEventKindName, ShrinkConfig, TraceCompleteness,
    TraceProjection, assert_replay_case, capture_live_run, check_captured_replay,
    check_replay_case, discover_constants, read_saved_replay_case, shrink_captured_replay,
    shrink_replay_case, write_saved_replay_case,
};
use tina_sim::{FaultConfig, LocalSendFaultMode, Simulator};

const SOURCE_ROLE: &str = "source";
const SINK_ROLE: &str = "sink";

fn fake_event(id: u64, kind: RuntimeEventKind) -> RuntimeEvent {
    RuntimeEvent::new(
        EventId::new(id),
        None,
        ShardId::new(0),
        IsolateId::new(1),
        kind,
    )
}

/// Operation alphabet for the burst-overflow case.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Op {
    /// Source fans out `size` sends into the sink in one handler.
    Burst { size: u32 },
    /// Drain the simulator one step.
    Step,
}

fn encode_op(op: &Op) -> String {
    match op {
        Op::Burst { size } => format!("burst:{size}"),
        Op::Step => "step".to_owned(),
    }
}

fn decode_op(text: &str) -> Result<Op, String> {
    if text == "step" {
        return Ok(Op::Step);
    }
    let Some(size) = text.strip_prefix("burst:") else {
        return Err(format!("unknown op {text:?}"));
    };
    Ok(Op::Burst {
        size: size.parse::<u32>().map_err(|error| error.to_string())?,
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Output {
    received: Vec<u32>,
    full_rejections: usize,
    accepted_sends: usize,
}

#[derive(Debug, Clone, Copy)]
enum SourceMsg {
    Burst { size: u32, base: u32 },
}

struct Source {
    sink: Address<SinkMsg>,
}

#[tina_runtime::isolate(message = SourceMsg, send = Outbound<SinkMsg>)]
impl Source {
    fn handle(
        &mut self,
        msg: SourceMsg,
        _ctx: &mut Context<'_, SingleShard, Self::Reply>,
    ) -> Effect<Self> {
        match msg {
            SourceMsg::Burst { size, base } => {
                let effects: Vec<Effect<Self>> = (0..size)
                    .map(|i| send(self.sink, SinkMsg::Got(base + i)))
                    .collect();
                batch(effects)
            }
        }
    }
}

#[derive(Debug, Clone, Copy)]
enum SinkMsg {
    Got(u32),
}

#[derive(Debug, Default)]
struct Sink {
    received: Rc<RefCell<Vec<u32>>>,
}

#[tina_runtime::isolate(message = SinkMsg)]
impl Sink {
    fn handle(
        &mut self,
        msg: SinkMsg,
        _ctx: &mut Context<'_, SingleShard, Self::Reply>,
    ) -> Effect<Self> {
        match msg {
            SinkMsg::Got(value) => {
                self.received.borrow_mut().push(value);
                noop()
            }
        }
    }
}

fn run_burst_overflow_case(case: &ReplayCase<Op>) -> ReplayReport<Output> {
    let mut sim = Simulator::new(SingleShard, case.simulator_config());

    let received = Rc::new(RefCell::new(Vec::new()));
    let sink = sim.register_with_mailbox_capacity(
        Sink {
            received: Rc::clone(&received),
        },
        case.config.mailbox(SINK_ROLE),
    );
    let source =
        sim.register_with_mailbox_capacity(Source { sink }, case.config.mailbox(SOURCE_ROLE));

    let mut next_base: u32 = 0;
    for op in case.history.operations() {
        match *op {
            Op::Burst { size } => {
                sim.try_send(
                    source,
                    SourceMsg::Burst {
                        size,
                        base: next_base,
                    },
                )
                .expect("source ingress accepted");
                next_base += size;
            }
            Op::Step => {
                sim.step();
            }
        }
    }
    sim.run_until_quiescent();

    let trace = sim.trace();
    let full_rejections = trace
        .iter()
        .filter(|event| {
            matches!(
                event.kind(),
                RuntimeEventKind::SendRejected {
                    reason: SendRejectedReason::Full,
                    ..
                }
            )
        })
        .count();
    let accepted_sends = trace
        .iter()
        .filter(|event| matches!(event.kind(), RuntimeEventKind::SendAccepted { .. }))
        .count();
    let output = Output {
        received: received.borrow().clone(),
        full_rejections,
        accepted_sends,
    };

    ReplayReport::from_case_and_events(case, trace, output)
}

fn run_burst_overflow_live_replay(
    case: &ReplayCase<Op>,
) -> Result<LiveReplayReport<Output>, tina_sim::dst::TraceProjectionError> {
    let report = run_burst_overflow_case(case);
    let fact = http1_keepalive_body_pressure_fact(&report.output);
    Ok(LiveReplayReport::exact(report).with_live_fact(fact))
}

fn http1_keepalive_body_pressure_fact(output: &Output) -> LiveReplayFact {
    LiveReplayFact::capacity_surface(&CapacitySurfaceReport::count(
        "http1.keepalive.body-pressure.sink-mailbox",
        CapacityMode::Fixed,
        2,
        0,
        2,
        output.full_rejections as u64,
    ))
}

/// Two bursts of 4 messages into a sink with capacity 2 produce
/// deterministic `SendRejected::Full` events. With 1-in-2 local-send
/// delay perturbing delivery rounds, the trace has a stable shape but
/// only because seed + config + history are all visible on the case.
fn burst_overflow_case() -> ReplayCase<Op> {
    let faults = FaultConfig {
        local_send: LocalSendFaultMode::DelayByRounds {
            one_in: 2,
            rounds: 1,
        },
        ..Default::default()
    };
    let config = ReplayConfig::with_faults(faults)
        .with_mailbox(SOURCE_ROLE, 8)
        .with_mailbox(SINK_ROLE, 2);
    ReplayCase::new(
        "http1 keepalive body pressure under local-send delay",
        7,
        config,
        "HTTP/1 keepalive/body-pressure shaped run: two explicit app bursts into a capacity-2 sink under seeded local-send delay",
        vec![Op::Burst { size: 4 }, Op::Step, Op::Burst { size: 4 }],
        "body-pressure capture records capacity high-water/full facts and SendRejected{ reason: Full }",
    )
    .expecting(BURST_OVERFLOW_EVENT_COUNT, BURST_OVERFLOW_TRACE_HASH)
}

// Pinned by `burst_overflow_case_replays_byte_for_byte`. Refresh only
// after a conscious trace-shape review (e.g. a new RuntimeEventKind
// variant or an intentional change to the burst-overflow semantics).
const BURST_OVERFLOW_EVENT_COUNT: usize = 34;
const BURST_OVERFLOW_TRACE_HASH: u64 = 0xe22d_12a5_1cd8_cf10;

// Real Tina pressure fact pinned exactly. If these counts drift the
// case still catches the regression through trace_hash + event_count,
// but pinning the structural numbers names the invariant the test is
// about (mailbox-full pressure, not just "the trace looks the same").
const BURST_OVERFLOW_FULL_REJECTIONS: usize = 5;
const BURST_OVERFLOW_ACCEPTED_SENDS: usize = 3;

#[test]
fn burst_overflow_case_replays_byte_for_byte() {
    let report = assert_replay_case(&burst_overflow_case(), run_burst_overflow_case);
    assert_eq!(
        report.output.full_rejections, BURST_OVERFLOW_FULL_REJECTIONS,
        "saved case pins the exact mailbox-full pressure shape",
    );
    assert_eq!(
        report.output.accepted_sends, BURST_OVERFLOW_ACCEPTED_SENDS,
        "saved case pins the exact accepted-sends shape",
    );
}

#[test]
fn burst_overflow_case_runs_twice_to_the_same_shape() {
    let case = burst_overflow_case();
    let first = run_burst_overflow_case(&case);
    let second = run_burst_overflow_case(&case);
    assert_eq!(first.event_count, second.event_count);
    assert_eq!(first.trace_hash, second.trace_hash);
    assert_eq!(first.output, second.output);
}

#[test]
fn changing_the_seed_changes_the_trace_hash() {
    let case = burst_overflow_case();
    let baseline = run_burst_overflow_case(&case);
    let mut perturbed = burst_overflow_case();
    perturbed.seed = case.seed.wrapping_add(11);
    let other = run_burst_overflow_case(&perturbed);
    assert_ne!(
        baseline.trace_hash, other.trace_hash,
        "seeded local-send delay must perturb the trace hash so the saved seed property is non-trivial",
    );
}

#[test]
fn check_replay_case_reports_drift_when_constants_are_stale() {
    let mut stale = burst_overflow_case();
    stale.expected_event_count = 0;
    stale.expected_trace_hash = 0;
    let mismatch = check_replay_case(&stale, run_burst_overflow_case)
        .expect_err("stale constants should mismatch");
    assert!(mismatch.count_diverged());
    assert!(mismatch.hash_diverged());
    let rendered = mismatch.to_string();
    assert!(rendered.contains("http1 keepalive body pressure"));
    assert!(rendered.contains("next step"));
}

fn capture_burst_overflow() -> LiveReplayCapture<Op> {
    let case = burst_overflow_case();
    let report = run_burst_overflow_case(&case);
    let fact = http1_keepalive_body_pressure_fact(&report.output);
    LiveReplayCapture::from_case_and_report(
        &case,
        "HTTP/1 keepalive/body pressure live-shaped capture",
        &report,
    )
    .with_source_metadata(
        CaptureSource::new("HTTP/1 keepalive/body pressure live-shaped capture")
            .runtime_kind("sim-smoke")
            .backend("sim"),
    )
    .with_live_fact(fact)
}

#[test]
fn captured_pressure_case_replays_in_simulator() {
    let capture = capture_burst_overflow();
    let case = capture.to_replay_case();
    let report = check_captured_replay(&capture, &case, run_burst_overflow_live_replay)
        .expect("captured pressure facts replay in sim");
    assert_eq!(
        report.replay.output.full_rejections,
        BURST_OVERFLOW_FULL_REJECTIONS
    );
    assert_eq!(report.replay.event_count, capture.expected.event_count);
    assert_eq!(report.replay.trace_hash, capture.expected.trace_hash);
}

#[test]
fn changed_config_invalidates_captured_pressure_replay() {
    let capture = capture_burst_overflow();
    let mut changed = capture.to_replay_case();
    changed.config = changed.config.clone().with_mailbox(SINK_ROLE, 3);

    let mismatch = check_captured_replay(&capture, &changed, run_burst_overflow_live_replay)
        .expect_err("changed mailbox capacity must invalidate captured replay");
    let rendered = mismatch.to_string();
    assert!(rendered.contains("config"));
    assert!(rendered.contains("events:"));
    assert!(rendered.contains("hash:"));
    assert!(rendered.contains("invariant:"));
}

#[test]
fn captured_http1_body_pressure_case_saves_and_converts() {
    let capture = capture_burst_overflow();
    let path = std::env::temp_dir().join(format!(
        "tina-http1-body-pressure-{}-{}.case",
        std::process::id(),
        capture.expected.trace_hash
    ));

    write_saved_replay_case(&path, &capture, encode_op).expect("write saved replay case");
    let saved = read_saved_replay_case(&path, decode_op).expect("read saved replay case");
    let _ = std::fs::remove_file(&path);

    assert_eq!(saved.name, capture.name);
    assert_eq!(saved.history, capture.history.operations());
    assert_eq!(saved.live_facts.len(), capture.live_facts.len());
    assert!(
        saved
            .live_facts
            .iter()
            .any(|fact| fact.contains("http1.keepalive.body-pressure"))
    );

    let case = saved
        .to_replay_case(
            capture.name,
            capture.config.clone(),
            capture.scenario,
            capture.invariant,
        )
        .expect("saved config hash matches typed config");
    check_captured_replay(&capture, &case, run_burst_overflow_live_replay)
        .expect("saved captured case converts back to replay");
}

#[test]
fn discover_constants_prints_multiple_pressure_cases() {
    let baseline = burst_overflow_case();
    let mut changed_seed = ReplayCase::new(
        baseline.name,
        baseline.seed + 1,
        baseline.config.clone(),
        baseline.scenario,
        baseline.history.operations().to_vec(),
        baseline.invariant,
    );
    changed_seed.expected_event_count = 0;
    changed_seed.expected_trace_hash = 0;

    let rows = discover_constants(
        [
            ("burst_overflow_seed_7", baseline),
            ("burst_overflow_seed_8", changed_seed),
        ],
        run_burst_overflow_case,
    );

    assert_eq!(rows.len(), 2);
    let printed = rows
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join("\n\n");
    assert!(printed.contains("// burst_overflow_seed_7"));
    assert!(printed.contains("// burst_overflow_seed_8"));
    assert!(printed.matches("expected_event_count:").count() == 2);
    assert!(printed.matches("expected_trace_hash:").count() == 2);
}

#[test]
fn live_replay_capture_includes_config_topology_and_mailboxes() {
    let capture = capture_burst_overflow();
    assert_eq!(capture.config.mailbox(SOURCE_ROLE), 8);
    assert_eq!(capture.config.mailbox(SINK_ROLE), 2);
    assert!(capture.topology_roles.contains(&SOURCE_ROLE));
    assert!(capture.topology_roles.contains(&SINK_ROLE));
    assert_eq!(
        capture.source_metadata.trace_completeness,
        TraceCompleteness::Complete
    );
    assert!(
        capture
            .summary()
            .to_string()
            .contains("replay_blocked=false")
    );
}

#[test]
fn live_replay_same_capture_converts_to_same_case_twice() {
    let capture = capture_burst_overflow();
    let first = capture.to_replay_case();
    let second = capture.to_replay_case();
    assert_eq!(first, second);
}

#[test]
fn live_replay_unsupported_fact_is_reported_fail_closed() {
    let capture = capture_burst_overflow().with_unsupported_fact(
        "reqwest completion body bytes",
        "bridge did not materialize the body as a replay op",
    );
    let case = capture.to_replay_case();
    let mismatch = check_captured_replay(&capture, &case, run_burst_overflow_live_replay)
        .expect_err("unsupported live facts must fail closed");
    let rendered = mismatch.to_string();
    assert!(rendered.contains("unsupported fact"));
    assert!(rendered.contains("reqwest completion body bytes"));
}

#[test]
fn capture_live_run_builder_records_metadata_and_facts() {
    let case = burst_overflow_case();
    let events = vec![fake_event(1, RuntimeEventKind::HandlerStarted)];
    let fact = LiveReplayFact::capacity_surface(&CapacitySurfaceReport::count(
        "builder.capacity",
        CapacityMode::Fixed,
        4,
        1,
        2,
        0,
    ));

    let capture = capture_live_run(case.name)
        .with_seed(case.seed)
        .with_config(case.config.clone())
        .with_scenario(case.scenario)
        .with_history(case.history.operations().to_vec())
        .with_invariant(case.invariant)
        .with_source_metadata(
            CaptureSource::new("builder live smoke")
                .runtime_kind("threaded")
                .backend("live"),
        )
        .with_trace(&events)
        .with_fact(fact)
        .finish()
        .expect("builder capture");

    assert_eq!(capture.expected.event_count, 1);
    assert_eq!(capture.source_metadata.runtime_kind, "threaded");
    assert_eq!(capture.source_metadata.label, "builder live smoke");
    assert_eq!(capture.live_facts.len(), 1);
    assert!(capture.summary().to_string().contains(case.name));
}

#[test]
fn bounded_capture_adds_explicit_truncation_truth_and_fails_closed() {
    let case = burst_overflow_case();
    let events = vec![
        fake_event(1, RuntimeEventKind::HandlerStarted),
        fake_event(2, RuntimeEventKind::MailboxAccepted),
    ];
    let capture = capture_live_run(case.name)
        .with_limits(CaptureLimits {
            max_events: 1,
            ..CaptureLimits::default()
        })
        .with_seed(case.seed)
        .with_config(case.config.clone())
        .with_scenario(case.scenario)
        .with_history(case.history.operations().to_vec())
        .with_invariant(case.invariant)
        .with_source("bounded capture")
        .with_trace(&events)
        .finish()
        .expect("truncated capture is saveable evidence");
    assert!(capture.truncated);
    assert!(
        capture
            .unsupported_facts
            .iter()
            .any(|fact| fact.fact == "TraceTruncated" || fact.fact == "CaptureFull")
    );

    let replay_case = capture.to_replay_case();
    let mismatch = check_captured_replay(&capture, &replay_case, |_| {
        LiveReplayReport::from_case_and_events(
            &replay_case,
            &events[..1],
            (),
            TraceProjection::Exact,
        )
    })
    .expect_err("default exact replay rejects truncated capture");
    assert!(mismatch.includes(tina_sim::dst::CapturedReplayChange::Capture));
    assert!(mismatch.to_string().contains("CaptureFull"));
}

#[test]
fn live_replay_unknown_event_kind_fails_closed_when_not_named_by_projection() {
    let case = burst_overflow_case();
    let events = vec![
        fake_event(1, RuntimeEventKind::HandlerStarted),
        fake_event(
            2,
            RuntimeEventKind::SendRejected {
                target_shard: ShardId::new(0),
                target_isolate: IsolateId::new(2),
                target_generation: tina::AddressGeneration::new(0),
                reason: SendRejectedReason::Full,
            },
        ),
    ];
    let projection = TraceProjection::Projected {
        included: vec![RuntimeEventKindName::SendRejected],
        ignored: Vec::new(),
        family_filter: None,
    };

    let err = LiveReplayCapture::from_events_with_options(
        case.name,
        case.seed,
        case.config.clone(),
        case.scenario,
        case.history.operations().to_vec(),
        case.invariant,
        "live trace with unnamed event",
        &events,
        projection,
        Vec::new(),
    )
    .expect_err("HandlerStarted was not named by projection");
    assert!(err.to_string().contains("not named as included or ignored"));
}

#[test]
fn live_replay_projected_comparison_names_ignored_event_kinds() {
    let case = burst_overflow_case();
    let events = vec![
        fake_event(1, RuntimeEventKind::HandlerStarted),
        fake_event(
            2,
            RuntimeEventKind::SendRejected {
                target_shard: ShardId::new(0),
                target_isolate: IsolateId::new(2),
                target_generation: tina::AddressGeneration::new(0),
                reason: SendRejectedReason::Full,
            },
        ),
    ];
    let projection = TraceProjection::Projected {
        included: vec![RuntimeEventKindName::SendRejected],
        ignored: vec![RuntimeEventKindName::HandlerStarted],
        family_filter: None,
    };
    let capture = LiveReplayCapture::from_events_with_options(
        case.name,
        case.seed,
        case.config.clone(),
        case.scenario,
        case.history.operations().to_vec(),
        case.invariant,
        "projected live trace",
        &events,
        projection.clone(),
        Vec::new(),
    )
    .expect("projection names every event kind");
    let replay_case = capture.to_replay_case();
    let report = check_captured_replay(&capture, &replay_case, |_| {
        LiveReplayReport::from_case_and_events(&replay_case, &events, (), projection.clone())
    })
    .expect("projected replay matches");
    assert_eq!(
        report.projection.ignored(),
        &[RuntimeEventKindName::HandlerStarted]
    );
}

#[test]
fn live_replay_shrink_refreshes_constants_for_live_derived_case() {
    let capture = capture_burst_overflow();
    let case = capture.to_replay_case();
    let report = shrink_replay_case(
        &case,
        ShrinkConfig::default(),
        "at least one full rejection remains",
        run_burst_overflow_case,
        |report| report.output.full_rejections >= 1,
    );
    assert_eq!(
        report.shrunk_case.expected_event_count,
        report.shrunk_report.event_count
    );
    assert_eq!(
        report.shrunk_case.expected_trace_hash,
        report.shrunk_report.trace_hash
    );
}

#[test]
fn shrink_captured_replay_preserves_fact_set_by_default() {
    let capture = capture_burst_overflow();
    let report = shrink_captured_replay(
        &capture,
        ShrinkConfig::default(),
        "full rejection fact remains exact",
        run_burst_overflow_live_replay,
        |report| report.replay.output.full_rejections >= 1,
    )
    .expect("shrink runs");

    assert_eq!(
        report.capture().expected.event_count,
        report.shrunk_report.replay.event_count
    );
    assert_eq!(
        report.capture().expected.trace_hash,
        report.shrunk_report.replay.trace_hash
    );
    assert_eq!(
        report.capture().live_facts,
        capture.live_facts,
        "live-derived shrink preserves proving facts by default",
    );
}
