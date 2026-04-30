use std::cell::RefCell;
use std::convert::Infallible;
use std::rc::Rc;
use std::time::Duration;

use tina::{Context, Effect, Isolate, SendMessage, Shard, ShardId};
use tina_runtime_current::{
    CallCompletionRejectedReason, CallKind, CallRequest, CallResult, CurrentCall, RuntimeEvent,
    RuntimeEventKind,
};
use tina_sim::{Simulator, SimulatorConfig};

#[derive(Debug, Default)]
struct TestShard;

impl Shard for TestShard {
    fn id(&self) -> ShardId {
        ShardId::new(41)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TimerMsg {
    Start,
    StartAndStop,
    Fired,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TimerObservation {
    Fired,
}

#[derive(Debug)]
struct Sleeper {
    delay: Duration,
    observations: Rc<RefCell<Vec<TimerObservation>>>,
}

impl Isolate for Sleeper {
    type Message = TimerMsg;
    type Reply = ();
    type Send = SendMessage<TimerMsg>;
    type Spawn = Infallible;
    type Call = CurrentCall<TimerMsg>;
    type Shard = TestShard;

    fn handle(&mut self, msg: Self::Message, _ctx: &mut Context<'_, Self::Shard>) -> Effect<Self> {
        match msg {
            TimerMsg::Start => Effect::Call(CurrentCall::new(
                CallRequest::Sleep { after: self.delay },
                |result| match result {
                    CallResult::TimerFired => TimerMsg::Fired,
                    other => panic!("expected TimerFired, got {other:?}"),
                },
            )),
            TimerMsg::StartAndStop => Effect::Batch(vec![
                Effect::Call(CurrentCall::new(
                    CallRequest::Sleep { after: self.delay },
                    |result| match result {
                        CallResult::TimerFired => TimerMsg::Fired,
                        other => panic!("expected TimerFired, got {other:?}"),
                    },
                )),
                Effect::Stop,
            ]),
            TimerMsg::Fired => {
                self.observations.borrow_mut().push(TimerObservation::Fired);
                Effect::Noop
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OrderingMsg {
    Start,
    Fired(&'static str),
}

#[derive(Debug)]
struct OrderingSleeper {
    label: &'static str,
    delay: Duration,
    log: Rc<RefCell<Vec<&'static str>>>,
}

impl Isolate for OrderingSleeper {
    type Message = OrderingMsg;
    type Reply = ();
    type Send = SendMessage<OrderingMsg>;
    type Spawn = Infallible;
    type Call = CurrentCall<OrderingMsg>;
    type Shard = TestShard;

    fn handle(&mut self, msg: Self::Message, _ctx: &mut Context<'_, Self::Shard>) -> Effect<Self> {
        match msg {
            OrderingMsg::Start => {
                let label = self.label;
                Effect::Call(CurrentCall::new(
                    CallRequest::Sleep { after: self.delay },
                    move |_| OrderingMsg::Fired(label),
                ))
            }
            OrderingMsg::Fired(label) => {
                self.log.borrow_mut().push(label);
                Effect::Noop
            }
        }
    }
}

fn count_call_completed(trace: &[RuntimeEvent], kind: CallKind) -> usize {
    trace
        .iter()
        .filter(|event| {
            matches!(
                event.kind(),
                RuntimeEventKind::CallCompleted { call_kind, .. } if call_kind == kind
            )
        })
        .count()
}

#[test]
fn timer_does_not_fire_early() {
    let observations = Rc::new(RefCell::new(Vec::new()));
    let mut sim = Simulator::new(TestShard, SimulatorConfig { seed: 7 });
    let sleeper = sim.register(Sleeper {
        delay: Duration::from_millis(10),
        observations: Rc::clone(&observations),
    });
    sim.try_send(sleeper, TimerMsg::Start).unwrap();

    assert_eq!(sim.step(), 1);
    assert!(observations.borrow().is_empty());

    sim.advance_time(Duration::from_millis(9));
    assert_eq!(sim.step(), 0);
    assert!(observations.borrow().is_empty());
}

#[test]
fn timer_fires_once_after_due_time() {
    let observations = Rc::new(RefCell::new(Vec::new()));
    let mut sim = Simulator::new(TestShard, SimulatorConfig { seed: 11 });
    let sleeper = sim.register(Sleeper {
        delay: Duration::from_millis(10),
        observations: Rc::clone(&observations),
    });
    sim.try_send(sleeper, TimerMsg::Start).unwrap();

    assert_eq!(sim.step(), 1);
    sim.advance_time(Duration::from_millis(10));
    assert_eq!(sim.step(), 1);
    assert_eq!(observations.borrow().as_slice(), [TimerObservation::Fired]);
    assert_eq!(count_call_completed(sim.trace(), CallKind::Sleep), 1);

    assert_eq!(sim.step(), 0);
    assert_eq!(observations.borrow().as_slice(), [TimerObservation::Fired]);
}

#[test]
fn timers_wake_in_due_time_order() {
    let log = Rc::new(RefCell::new(Vec::new()));
    let mut sim = Simulator::new(TestShard, SimulatorConfig { seed: 3 });
    let slow = sim.register(OrderingSleeper {
        label: "slow",
        delay: Duration::from_millis(15),
        log: Rc::clone(&log),
    });
    let fast = sim.register(OrderingSleeper {
        label: "fast",
        delay: Duration::from_millis(5),
        log: Rc::clone(&log),
    });
    sim.try_send(slow, OrderingMsg::Start).unwrap();
    sim.try_send(fast, OrderingMsg::Start).unwrap();
    assert_eq!(sim.step(), 2);

    sim.advance_time(Duration::from_millis(5));
    assert_eq!(sim.step(), 1);
    assert_eq!(log.borrow().as_slice(), ["fast"]);

    sim.advance_time(Duration::from_millis(10));
    assert_eq!(sim.step(), 1);
    assert_eq!(log.borrow().as_slice(), ["fast", "slow"]);
}

#[test]
fn equal_deadline_timers_wake_in_request_order() {
    let log = Rc::new(RefCell::new(Vec::new()));
    let mut sim = Simulator::new(TestShard, SimulatorConfig { seed: 5 });
    let first = sim.register(OrderingSleeper {
        label: "first",
        delay: Duration::from_millis(10),
        log: Rc::clone(&log),
    });
    let second = sim.register(OrderingSleeper {
        label: "second",
        delay: Duration::from_millis(10),
        log: Rc::clone(&log),
    });
    sim.try_send(first, OrderingMsg::Start).unwrap();
    sim.try_send(second, OrderingMsg::Start).unwrap();
    assert_eq!(sim.step(), 2);

    sim.advance_time(Duration::from_millis(10));
    assert_eq!(sim.step(), 2);
    assert_eq!(log.borrow().as_slice(), ["first", "second"]);
}

#[test]
fn stopped_requester_rejects_timer_completion() {
    let observations = Rc::new(RefCell::new(Vec::new()));
    let mut sim = Simulator::new(TestShard, SimulatorConfig { seed: 9 });
    let sleeper = sim.register(Sleeper {
        delay: Duration::from_millis(10),
        observations: Rc::clone(&observations),
    });
    sim.try_send(sleeper, TimerMsg::StartAndStop).unwrap();
    assert_eq!(sim.step(), 1);

    sim.advance_time(Duration::from_millis(10));
    assert_eq!(sim.step(), 0);
    assert!(observations.borrow().is_empty());
    assert!(sim.trace().iter().any(|event| {
        matches!(
            event.kind(),
            RuntimeEventKind::CallCompletionRejected {
                call_kind: CallKind::Sleep,
                reason: CallCompletionRejectedReason::RequesterClosed,
                ..
            }
        )
    }));
}

#[test]
fn same_config_reproduces_same_event_record() {
    fn run(seed: u64) -> Vec<RuntimeEvent> {
        let observations = Rc::new(RefCell::new(Vec::new()));
        let mut sim = Simulator::new(TestShard, SimulatorConfig { seed });
        let sleeper = sim.register(Sleeper {
            delay: Duration::from_millis(4),
            observations,
        });
        sim.try_send(sleeper, TimerMsg::Start).unwrap();
        sim.run_until_quiescent();
        sim.trace().to_vec()
    }

    assert_eq!(run(1234), run(1234));
}
