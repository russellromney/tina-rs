use std::cell::RefCell;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::rc::Rc;
use std::time::Duration;

use tina::prelude::*;
use tina_runtime::sleep;
use tina_sim::{FaultConfig, FaultMode, LocalSendFaultMode, Simulator, SimulatorConfig};

use super::TinaReport;

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct ReplayShard;

impl Shard for ReplayShard {
    fn id(&self) -> ShardId {
        ShardId::new(73)
    }
}

#[derive(Debug, Clone, Copy)]
enum ProducerMsg {
    Tick(u32),
}

#[derive(Debug)]
struct Producer {
    sink: Address<SinkMsg>,
    remaining: u32,
}

#[tina_runtime::isolate(
    message = ProducerMsg,
    send = Outbound<SinkMsg>,
    shard = ReplayShard
)]
impl Producer {
    fn handle(&mut self, msg: ProducerMsg, _ctx: &mut Context<'_, ReplayShard>) -> Effect<Self> {
        match msg {
            ProducerMsg::Tick(count) => {
                let next = count + 1;
                let mut effects: Vec<Effect<Self>> = vec![send(self.sink, SinkMsg::Got(count))];
                if self.remaining > 0 {
                    self.remaining -= 1;
                    let delay = Duration::from_millis(1 + u64::from(next % 3));
                    effects.push(sleep(delay).reply(move |_| ProducerMsg::Tick(next)));
                }
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

#[tina_runtime::isolate(message = SinkMsg, shard = ReplayShard)]
impl Sink {
    fn handle(&mut self, msg: SinkMsg, _ctx: &mut Context<'_, ReplayShard>) -> Effect<Self> {
        match msg {
            SinkMsg::Got(value) => {
                self.received.borrow_mut().push(value);
                noop()
            }
        }
    }
}

fn run_once(seed: u64) -> (usize, u64, usize) {
    let config = SimulatorConfig {
        seed,
        faults: FaultConfig {
            // Make the seed do real work: 1-in-3 timer wakes get pushed by an
            // extra tick of virtual time, deterministically chosen by seed.
            timer_wake: FaultMode::DelayBy {
                one_in: 3,
                by: Duration::from_millis(1),
            },
            // Same shape on local sends.
            local_send: LocalSendFaultMode::DelayByRounds {
                one_in: 4,
                rounds: 1,
            },
            ..Default::default()
        },
        ..Default::default()
    };
    let mut sim = Simulator::new(ReplayShard, config);

    let received = Rc::new(RefCell::new(Vec::new()));
    let sink = sim.register_with_mailbox_capacity(
        Sink {
            received: Rc::clone(&received),
        },
        16,
    );

    let producer = sim.register_with_mailbox_capacity(Producer { sink, remaining: 5 }, 16);

    sim.try_send(producer, ProducerMsg::Tick(0)).expect("kick");
    sim.run_until_quiescent();

    let trace = sim.trace();
    let fingerprint = fingerprint_trace(trace);
    let received_count = received.borrow().len();
    (trace.len(), fingerprint, received_count)
}

fn fingerprint_trace(trace: &[tina_runtime::RuntimeEvent]) -> u64 {
    // Trace events are deterministic across replays under the same config.
    // We hash the Debug projection because it captures the full event shape
    // in stable form — enough for a fingerprint without re-implementing a
    // canonical encoding.
    let mut hasher = DefaultHasher::new();
    for event in trace {
        format!("{event:?}").hash(&mut hasher);
    }
    hasher.finish()
}

pub(crate) fn run() -> TinaReport {
    let seed_a = 42;
    let seed_b = 99;

    let (run_a1_event_count, run_a1_fingerprint, messages_received) = run_once(seed_a);
    let (run_a2_event_count, run_a2_fingerprint, _) = run_once(seed_a);
    let (run_b1_event_count, run_b1_fingerprint, _) = run_once(seed_b);

    TinaReport {
        seed_a,
        run_a1_event_count,
        run_a2_event_count,
        run_a1_fingerprint,
        run_a2_fingerprint,
        seed_b,
        run_b1_event_count,
        run_b1_fingerprint,
        messages_received,
    }
}
