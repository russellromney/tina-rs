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

use tina::prelude::*;
use tina_runtime::{RuntimeEventKind, SendRejectedReason};
use tina_sim::dst::{
    ReplayCase, ReplayConfig, ReplayReport, assert_replay_case, check_replay_case,
};
use tina_sim::{FaultConfig, LocalSendFaultMode, Simulator};

const SOURCE_ROLE: &str = "source";
const SINK_ROLE: &str = "sink";

/// Operation alphabet for the burst-overflow case.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Op {
    /// Source fans out `size` sends into the sink in one handler.
    Burst { size: u32 },
    /// Drain the simulator one step.
    Step,
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
        "burst overflow under local-send delay",
        7,
        config,
        "two bursts of 4 sends into a capacity-2 sink under seeded local-send delay",
        vec![Op::Burst { size: 4 }, Op::Step, Op::Burst { size: 4 }],
        "mailbox full produces SendRejected{ reason: Full } that the trace records",
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
    assert!(rendered.contains("burst overflow"));
    assert!(rendered.contains("next step"));
}
