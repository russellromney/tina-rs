use std::cell::RefCell;
use std::convert::Infallible;
use std::rc::Rc;
use std::time::Duration;

use tina::{Context, Effect, Isolate, Outbound, Shard, ShardId, time::TimerInterval};
use tina_runtime::{
    CallCompletionRejectedReason, CallInput, CallKind, CallOutput, RuntimeCall,
    RuntimeCallCompletion, RuntimeEvent, RuntimeEventKind, SleepReply, StreamId,
    TerminalCompletionAction, sleep,
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
    TerminalStop,
    TerminalNoop,
    BadStreamNoop,
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
    type Send = Outbound<TimerMsg>;
    type Spawn = Infallible;
    type SpawnObserved = std::convert::Infallible;
    type Io = RuntimeCall<TimerMsg>;
    type Fact = ::std::convert::Infallible;
    type Shard = TestShard;

    fn handle(
        &mut self,
        msg: Self::Message,
        _ctx: &mut Context<'_, Self::Shard, Self::Reply>,
    ) -> Effect<Self> {
        match msg {
            TimerMsg::Start => Effect::Io(RuntimeCall::new(
                CallInput::Sleep { after: self.delay },
                |result| match result {
                    CallOutput::TimerFired => TimerMsg::Fired,
                    other => panic!("expected TimerFired, got {other:?}"),
                },
            )),
            TimerMsg::StartAndStop => Effect::Batch(vec![
                Effect::Io(RuntimeCall::new(
                    CallInput::Sleep { after: self.delay },
                    |result| match result {
                        CallOutput::TimerFired => TimerMsg::Fired,
                        other => panic!("expected TimerFired, got {other:?}"),
                    },
                )),
                Effect::Stop,
            ]),
            TimerMsg::TerminalStop => Effect::Io(RuntimeCall::new_with_completion(
                CallInput::Sleep { after: self.delay },
                |result| match result {
                    CallOutput::TimerFired => RuntimeCallCompletion::StopRequester,
                    other => RuntimeCallCompletion::Message(unexpected_timer_completion(other)),
                },
            )),
            TimerMsg::TerminalNoop => Effect::Io(RuntimeCall::new_with_completion(
                CallInput::Sleep { after: self.delay },
                |result| match result {
                    CallOutput::TimerFired => RuntimeCallCompletion::Noop,
                    other => RuntimeCallCompletion::Message(unexpected_timer_completion(other)),
                },
            )),
            TimerMsg::BadStreamNoop => Effect::Io(RuntimeCall::new_with_completion(
                CallInput::TcpStreamClose {
                    stream: StreamId::new(999_999),
                },
                |_| RuntimeCallCompletion::Noop,
            )),
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
    type Send = Outbound<OrderingMsg>;
    type Spawn = Infallible;
    type SpawnObserved = std::convert::Infallible;
    type Io = RuntimeCall<OrderingMsg>;
    type Fact = ::std::convert::Infallible;
    type Shard = TestShard;

    fn handle(
        &mut self,
        msg: Self::Message,
        _ctx: &mut Context<'_, Self::Shard, Self::Reply>,
    ) -> Effect<Self> {
        match msg {
            OrderingMsg::Start => {
                let label = self.label;
                Effect::Io(RuntimeCall::new(
                    CallInput::Sleep { after: self.delay },
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

fn unexpected_timer_completion(output: CallOutput) -> TimerMsg {
    panic!("expected TimerFired, got {output:?}")
}

fn drain(sim: &mut Simulator<TestShard>) {
    while sim.step() > 0 {}
}

#[test]
fn timer_does_not_fire_early() {
    let observations = Rc::new(RefCell::new(Vec::new()));
    let mut sim = Simulator::new(
        TestShard,
        SimulatorConfig {
            seed: 7,
            ..Default::default()
        },
    );
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
    let mut sim = Simulator::new(
        TestShard,
        SimulatorConfig {
            seed: 11,
            ..Default::default()
        },
    );
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
fn terminal_stop_completion_records_action_and_stops_in_sim() {
    let observations = Rc::new(RefCell::new(Vec::new()));
    let mut sim = Simulator::new(
        TestShard,
        SimulatorConfig {
            seed: 13,
            ..Default::default()
        },
    );
    let sleeper = sim.register(Sleeper {
        delay: Duration::from_millis(10),
        observations: Rc::clone(&observations),
    });
    sim.try_send(sleeper, TimerMsg::TerminalStop).unwrap();

    drain(&mut sim);
    sim.advance_time(Duration::from_millis(10));
    drain(&mut sim);

    assert!(observations.borrow().is_empty());
    assert!(sim.trace().iter().any(|event| {
        matches!(
            event.kind(),
            RuntimeEventKind::CallCompletionAction {
                call_kind: CallKind::Sleep,
                action: TerminalCompletionAction::StopRequester,
                ..
            }
        )
    }));
    assert!(
        sim.trace()
            .iter()
            .any(|event| { matches!(event.kind(), RuntimeEventKind::IsolateStopped) })
    );
}

#[test]
fn terminal_noop_completion_records_action_and_keeps_isolate_alive_in_sim() {
    let observations = Rc::new(RefCell::new(Vec::new()));
    let mut sim = Simulator::new(
        TestShard,
        SimulatorConfig {
            seed: 17,
            ..Default::default()
        },
    );
    let sleeper = sim.register(Sleeper {
        delay: Duration::from_millis(10),
        observations: Rc::clone(&observations),
    });
    sim.try_send(sleeper, TimerMsg::TerminalNoop).unwrap();

    drain(&mut sim);
    sim.advance_time(Duration::from_millis(10));
    drain(&mut sim);

    assert!(observations.borrow().is_empty());
    assert!(sim.trace().iter().any(|event| {
        matches!(
            event.kind(),
            RuntimeEventKind::CallCompletionAction {
                call_kind: CallKind::Sleep,
                action: TerminalCompletionAction::Noop,
                ..
            }
        )
    }));
    assert!(
        !sim.trace()
            .iter()
            .any(|event| { matches!(event.kind(), RuntimeEventKind::IsolateStopped) })
    );

    sim.try_send(sleeper, TimerMsg::Start).unwrap();
    drain(&mut sim);
    sim.advance_time(Duration::from_millis(10));
    drain(&mut sim);
    assert_eq!(observations.borrow().as_slice(), [TimerObservation::Fired]);
}

#[test]
fn terminal_noop_cannot_hide_backend_failure_in_sim() {
    let observations = Rc::new(RefCell::new(Vec::new()));
    let mut sim = Simulator::new(
        TestShard,
        SimulatorConfig {
            seed: 19,
            ..Default::default()
        },
    );
    let sleeper = sim.register(Sleeper {
        delay: Duration::from_millis(10),
        observations: Rc::clone(&observations),
    });
    sim.try_send(sleeper, TimerMsg::BadStreamNoop).unwrap();
    drain(&mut sim);

    assert!(observations.borrow().is_empty());
    assert!(sim.trace().iter().any(|event| {
        matches!(
            event.kind(),
            RuntimeEventKind::CallCompletionRejected {
                call_kind: CallKind::TcpStreamClose,
                reason: CallCompletionRejectedReason::TerminalActionOnFailure,
                ..
            }
        )
    }));
    assert!(
        !sim.trace()
            .iter()
            .any(|event| { matches!(event.kind(), RuntimeEventKind::CallCompletionAction { .. }) })
    );
}

#[test]
fn fallback_message_completion_reports_mailbox_full_in_sim() {
    let observations = Rc::new(RefCell::new(Vec::new()));
    let mut sim = Simulator::new(
        TestShard,
        SimulatorConfig {
            seed: 23,
            ..Default::default()
        },
    );
    let sleeper = sim.register_with_mailbox_capacity(
        Sleeper {
            delay: Duration::from_millis(10),
            observations: Rc::clone(&observations),
        },
        1,
    );

    sim.try_send(sleeper, TimerMsg::Start).unwrap();
    assert_eq!(sim.step(), 1, "start message issues the sleep call");
    sim.try_send(sleeper, TimerMsg::Fired)
        .expect("queued filler should occupy the only mailbox slot");
    sim.advance_time(Duration::from_millis(10));
    drain(&mut sim);

    assert_eq!(
        observations.borrow().as_slice(),
        [TimerObservation::Fired],
        "only the queued filler message should run"
    );
    assert!(sim.trace().iter().any(|event| {
        matches!(
            event.kind(),
            RuntimeEventKind::CallCompletionRejected {
                call_kind: CallKind::Sleep,
                reason: CallCompletionRejectedReason::MailboxFull,
                ..
            }
        )
    }));
}

#[test]
fn timers_wake_in_due_time_order() {
    let log = Rc::new(RefCell::new(Vec::new()));
    let mut sim = Simulator::new(
        TestShard,
        SimulatorConfig {
            seed: 3,
            ..Default::default()
        },
    );
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
    let mut sim = Simulator::new(
        TestShard,
        SimulatorConfig {
            seed: 5,
            ..Default::default()
        },
    );
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
fn equal_deadline_timers_preserve_request_order_when_registration_order_differs() {
    let log = Rc::new(RefCell::new(Vec::new()));
    let mut sim = Simulator::new(
        TestShard,
        SimulatorConfig {
            seed: 17,
            ..Default::default()
        },
    );
    let registered_first = sim.register(OrderingSleeper {
        label: "registered-first",
        delay: Duration::from_millis(10),
        log: Rc::clone(&log),
    });
    let registered_second = sim.register(OrderingSleeper {
        label: "registered-second",
        delay: Duration::from_millis(20),
        log: Rc::clone(&log),
    });
    sim.try_send(registered_second, OrderingMsg::Start).unwrap();
    assert_eq!(sim.step(), 1);

    sim.advance_time(Duration::from_millis(10));
    sim.try_send(registered_first, OrderingMsg::Start).unwrap();
    assert_eq!(sim.step(), 1);

    sim.advance_time(Duration::from_millis(10));
    assert_eq!(sim.step(), 1);
    assert_eq!(log.borrow().as_slice(), ["registered-second"]);

    assert_eq!(sim.step(), 1);
    assert_eq!(
        log.borrow().as_slice(),
        ["registered-second", "registered-first"]
    );
}

#[test]
fn stopped_requester_rejects_timer_completion() {
    let observations = Rc::new(RefCell::new(Vec::new()));
    let mut sim = Simulator::new(
        TestShard,
        SimulatorConfig {
            seed: 9,
            ..Default::default()
        },
    );
    let sleeper = sim.register(Sleeper {
        delay: Duration::from_millis(10),
        observations: Rc::clone(&observations),
    });
    sim.try_send(sleeper, TimerMsg::StartAndStop).unwrap();
    assert_eq!(sim.step(), 1);
    assert!(
        !sim.has_in_flight_calls(),
        "stopped requester should cancel its pending timer immediately"
    );
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
        let mut sim = Simulator::new(
            TestShard,
            SimulatorConfig {
                seed,
                ..Default::default()
            },
        );
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HelperIntervalMsg {
    Start,
    Tick(u64, SleepReply),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct HelperIntervalObservation {
    tick_number: u64,
    scheduled_after: Duration,
    fired: bool,
}

#[derive(Debug)]
struct HelperIntervalSleeper {
    interval: TimerInterval,
    observations: Rc<RefCell<Vec<HelperIntervalObservation>>>,
}

impl Isolate for HelperIntervalSleeper {
    type Message = HelperIntervalMsg;
    type Reply = ();
    type Send = Outbound<HelperIntervalMsg>;
    type Spawn = Infallible;
    type SpawnObserved = Infallible;
    type Io = RuntimeCall<HelperIntervalMsg>;
    type Fact = ::std::convert::Infallible;
    type Shard = TestShard;

    fn handle(
        &mut self,
        msg: Self::Message,
        ctx: &mut Context<'_, Self::Shard, Self::Reply>,
    ) -> Effect<Self> {
        match msg {
            HelperIntervalMsg::Start => {
                let now = ctx.now();
                let decision = self.interval.next_delay(now);
                let tick_number = decision.tick_number();
                self.observations
                    .borrow_mut()
                    .push(HelperIntervalObservation {
                        tick_number,
                        scheduled_after: decision
                            .scheduled_at()
                            .saturating_duration_since(decision.observed_at()),
                        fired: false,
                    });
                sleep(decision.delay())
                    .then(move |reply| HelperIntervalMsg::Tick(tick_number, reply))
            }
            HelperIntervalMsg::Tick(tick_number, reply) => {
                reply.expect("sim sleep should fire");
                self.observations
                    .borrow_mut()
                    .push(HelperIntervalObservation {
                        tick_number,
                        scheduled_after: Duration::ZERO,
                        fired: true,
                    });
                Effect::Noop
            }
        }
    }
}

#[test]
fn timer_interval_helper_runs_through_sim_sleep_path() {
    let observations = Rc::new(RefCell::new(Vec::new()));
    let mut sim = Simulator::new(
        TestShard,
        SimulatorConfig {
            seed: 91,
            ..Default::default()
        },
    );
    let sleeper = sim.register(HelperIntervalSleeper {
        interval: TimerInterval::every(Duration::from_millis(10)).unwrap(),
        observations: Rc::clone(&observations),
    });

    sim.try_send(sleeper, HelperIntervalMsg::Start).unwrap();
    assert_eq!(sim.step(), 1);
    assert_eq!(
        observations.borrow().as_slice(),
        [HelperIntervalObservation {
            tick_number: 1,
            scheduled_after: Duration::from_millis(10),
            fired: false,
        }]
    );

    sim.advance_time(Duration::from_millis(10));
    assert_eq!(sim.step(), 1);
    assert_eq!(count_call_completed(sim.trace(), CallKind::Sleep), 1);
    assert_eq!(
        observations.borrow()[1],
        HelperIntervalObservation {
            tick_number: 1,
            scheduled_after: Duration::ZERO,
            fired: true,
        }
    );
}
