//! Tina under `tina-sim`: a saved [`ReplayCase`] with a pinned event
//! count and `stable_trace_hash`. Same seed, same hash. Saved seed,
//! saved bug.
//!
//! Read top-to-bottom:
//!
//! 1. `Op` — the explicit operation alphabet for the case history.
//! 2. `case()` — the bug in a box: name, seed, full simulator config,
//!    declared mailbox capacities, scenario, history, expected event
//!    count and hash, invariant.
//! 3. `run_case` — the runner. It reads every knob from the case and
//!    drives one ingress per `Op` so the history is load-bearing
//!    (deleting an op changes the trace).
//! 4. `run()` — asserts the case, then demos a tiny seed sweep and a
//!    deletion shrink so this file teaches the whole loop.

use std::cell::RefCell;
use std::rc::Rc;

use tina::prelude::*;
use tina_sim::dst::{
    ReplayCase, ReplayConfig, ReplayReport, ShrinkConfig, ShrinkReport, SweepFailure,
    SweepSuccess, assert_replay_case, shrink_replay_case, sweep_seeds,
};
use tina_sim::{FaultConfig, LocalSendFaultMode, Simulator};

/// The history alphabet. Each op causes an observable simulator
/// action — deleting one changes the trace hash.
///
/// `Tick(value)` queues one ingress message into the producer.
/// `Drain` runs the simulator to quiescence, so the seeded local-send
/// delay between concurrent queued ticks perturbs the trace shape.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Op {
    Tick(u32),
    Drain,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Output {
    pub messages_received: usize,
}

const PRODUCER_ROLE: &str = "producer";
const SINK_ROLE: &str = "sink";

#[derive(Debug, Clone, Copy)]
enum ProducerMsg {
    /// Forward `value` to the sink. Each `Tick` is one ingress message
    /// driven by the case `History`; the producer never schedules
    /// itself.
    Tick(u32),
}

struct Producer {
    sink: Address<SinkMsg>,
}

#[tina_runtime::isolate(message = ProducerMsg, send = Outbound<SinkMsg>)]
impl Producer {
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

/// Seeded faults for the canonical case. Visible Rust data.
fn faults() -> FaultConfig {
    FaultConfig {
        local_send: LocalSendFaultMode::DelayByRounds {
            one_in: 4,
            rounds: 1,
        },
        ..Default::default()
    }
}

/// The bug in a box: a saved `ReplayCase` with pinned event count and
/// trace hash. Copy this shape into your bug report.
pub fn case() -> ReplayCase<Op> {
    let config = ReplayConfig::with_faults(faults())
        .with_mailbox(PRODUCER_ROLE, 16)
        .with_mailbox(SINK_ROLE, 16);
    ReplayCase::new(
        "eiffel_replay_dst saved seed",
        42,
        config,
        "history-driven ticks fan out into a sink under seeded delivery delays",
        vec![
            Op::Tick(0),
            Op::Tick(1),
            Op::Tick(2),
            Op::Drain,
            Op::Tick(3),
            Op::Tick(4),
            Op::Tick(5),
            Op::Drain,
        ],
        "every Tick op produces one SinkMsg::Got(value) in trace order",
    )
    .expecting(SAVED_EVENT_COUNT, SAVED_TRACE_HASH)
}

// Saved constants. Refresh these only after a conscious trace-shape
// review (for example, when a new RuntimeEventKind variant gets added
// or when the producer/sink isolate logic intentionally changes).
const SAVED_EVENT_COUNT: usize = 54;
const SAVED_TRACE_HASH: u64 = 0xc878_d2a4_3912_9480;

/// Runs one [`ReplayCase`] and returns a [`ReplayReport`] whose event
/// count and trace hash come from `tina_runtime::stable_trace_hash`.
///
/// Every `Op::Tick` in the history becomes one ingress message. The
/// case's `ReplayConfig` supplies the simulator config (with `seed`
/// taken from the case) and both mailbox capacities.
pub fn run_case(case: &ReplayCase<Op>) -> ReplayReport<Output> {
    let mut sim = Simulator::new(SingleShard, case.simulator_config());

    let received = Rc::new(RefCell::new(Vec::new()));
    let sink = sim.register_with_mailbox_capacity(
        Sink {
            received: Rc::clone(&received),
        },
        case.config.mailbox(SINK_ROLE),
    );
    let producer =
        sim.register_with_mailbox_capacity(Producer { sink }, case.config.mailbox(PRODUCER_ROLE));

    for op in case.history.operations() {
        match *op {
            Op::Tick(value) => {
                sim.try_send(producer, ProducerMsg::Tick(value))
                    .expect("producer ingress accepted");
            }
            Op::Drain => {
                sim.run_until_quiescent();
            }
        }
    }
    // Final drain catches anything still queued after the last op.
    sim.run_until_quiescent();

    let messages_received = received.borrow().len();
    ReplayReport::from_case_and_events(
        case,
        sim.trace(),
        Output { messages_received },
    )
}

/// Demonstrates the full DST loop: assert the saved case, sweep a few
/// seeds, then shrink the saved history.
#[derive(Debug)]
pub struct Demo {
    pub saved: ReplayReport<Output>,
    pub sweep: Result<SweepSuccess, Box<SweepFailure<Op, Output>>>,
    pub shrink: ShrinkReport<Op, Output>,
}

pub fn run() -> anyhow::Result<Demo> {
    let saved = assert_replay_case(&case(), run_case);

    // Tiny local seed sweep over a handful of seeds. The check passes
    // for every seed; the demo is the shape, not finding a bug here.
    let sweep = sweep_seeds(
        "eiffel_replay_dst seed sweep",
        [42_u64, 99, 123, 256, 1024],
        |seed| {
            // Build a fresh case at this seed via ReplayCase::new so
            // case.name/seed and history.name/seed stay aligned.
            // No `.expecting(...)` — sweep is searching, so a found
            // failure refreshes the expected constants on its own.
            let template = case();
            ReplayCase::new(
                template.name,
                seed,
                template.config.clone(),
                template.scenario,
                template.history.operations().to_vec(),
                template.invariant,
            )
        },
        run_case,
        |report| {
            if report.output.messages_received == 6 {
                Ok(())
            } else {
                Err(format!(
                    "expected 6 sink messages, got {}",
                    report.output.messages_received
                ))
            }
        },
    );

    // Deletion shrink while the bug invariant ("at least one tick is
    // received") still holds. Starts from the saved case so the demo
    // stays small.
    let shrink = shrink_replay_case(
        &case(),
        ShrinkConfig::default(),
        "at least one tick reaches the sink",
        run_case,
        |report| report.output.messages_received >= 1,
    );

    Ok(Demo {
        saved,
        sweep,
        shrink,
    })
}
