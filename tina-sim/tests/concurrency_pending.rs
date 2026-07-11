//! Simulator parity for concurrency-charged parked callers.

use std::cell::RefCell;
use std::convert::Infallible;
use std::rc::Rc;
use std::time::Duration;

use tina::prelude::*;
use tina_runtime::{
    AdmissionFailure, CallOutcome, ConcurrencyParkError, ConcurrencyPendingReplies,
    ConcurrencyPendingReport, call_request, request_effect_after_concurrency_park, sleep,
};
use tina_sim::{Simulator, SimulatorConfig};

#[derive(Debug, Default)]
struct TestShard;

impl Shard for TestShard {
    fn id(&self) -> ShardId {
        ShardId::new(77)
    }
}

#[derive(Debug)]
enum Event {
    Complete(u64),
}

#[derive(Debug)]
enum Request {
    Hold { key: u64, duration: Duration },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Reply {
    Done(u64),
    Full,
    Duplicate,
    PendingMismatch,
}

struct Service {
    pending: ConcurrencyPendingReplies<u64, Reply>,
    reports: Rc<RefCell<Vec<ConcurrencyPendingReport>>>,
}

#[tina_runtime::isolate(event = Event, request = Request, reply = Reply, shard = TestShard)]
impl Service {
    fn handle_event(
        &mut self,
        event: Event,
        _ctx: &mut Context<'_, TestShard, Self::Reply>,
    ) -> Effect<Self> {
        match event {
            Event::Complete(key) => {
                let effect = self
                    .pending
                    .reply_by_key::<Self>(&key, Reply::Done(key))
                    .unwrap_or_else(noop);
                self.reports.borrow_mut().push(self.pending.report());
                effect
            }
        }
    }

    fn handle_request(
        &mut self,
        request: Request,
        call: RequestCall<'_, Self>,
    ) -> RequestEffect<Self> {
        match request {
            Request::Hold { key, duration } => match self.pending.park_request(key, call) {
                Ok((_ticket, permit)) => request_effect_after_concurrency_park(
                    permit,
                    sleep(duration)
                        .then(move |_| tina::ServiceMessage::Event(Event::Complete(key))),
                ),
                Err(ConcurrencyParkError::Admission {
                    call,
                    failure: AdmissionFailure::Full(_),
                    ..
                }) => call.reply(Reply::Full),
                Err(ConcurrencyParkError::Admission { call, .. }) => call.reply(Reply::Full),
                Err(ConcurrencyParkError::DuplicateKey { call, .. }) => {
                    call.reply(Reply::Duplicate)
                }
                Err(ConcurrencyParkError::PendingFull { call, .. }) => {
                    call.reply(Reply::PendingMismatch)
                }
            },
        }
    }
}

#[derive(Debug)]
enum ClientMsg {
    Start {
        target: tina::ServiceRequestAddress<Event, Request, Reply>,
        key: u64,
        hold_for: Duration,
        timeout: Duration,
    },
    Returned(CallOutcome<Reply>),
}

struct Client {
    outcomes: Rc<RefCell<Vec<CallOutcome<Reply>>>>,
}

#[tina_runtime::isolate(message = ClientMsg, shard = TestShard)]
impl Client {
    fn handle(
        &mut self,
        message: ClientMsg,
        _ctx: &mut Context<'_, TestShard, Self::Reply>,
    ) -> Effect<Self> {
        match message {
            ClientMsg::Start {
                target,
                key,
                hold_for,
                timeout,
            } => call_request(
                target,
                Request::Hold {
                    key,
                    duration: hold_for,
                },
                timeout,
            )
            .then(ClientMsg::Returned),
            ClientMsg::Returned(outcome) => {
                self.outcomes.borrow_mut().push(outcome);
                noop()
            }
        }
    }
}

#[test]
fn sim_full_duplicate_completion_and_refill_match_live_ownership() {
    let mut sim = Simulator::new(TestShard, SimulatorConfig::default());
    let reports = Rc::new(RefCell::new(Vec::new()));
    let service = sim.register_split_service::<Service, Event, Request, Infallible>(
        Service {
            pending: ConcurrencyPendingReplies::with_capacity("sim.parked", 2),
            reports: Rc::clone(&reports),
        },
        16,
    );
    let outcomes = Rc::new(RefCell::new(Vec::new()));
    let client = sim.register(Client {
        outcomes: Rc::clone(&outcomes),
    });

    for key in [1, 1, 2, 3] {
        sim.try_send(
            client,
            ClientMsg::Start {
                target: service.requests,
                key,
                hold_for: Duration::from_millis(10),
                timeout: Duration::from_secs(1),
            },
        )
        .unwrap();
    }
    sim.run_until_quiescent();

    sim.try_send(
        client,
        ClientMsg::Start {
            target: service.requests,
            key: 4,
            hold_for: Duration::from_millis(10),
            timeout: Duration::from_secs(1),
        },
    )
    .unwrap();
    sim.run_until_quiescent();

    let outcomes = outcomes.borrow();
    assert_eq!(outcomes.len(), 5);
    assert_eq!(
        outcomes
            .iter()
            .filter(|outcome| matches!(outcome, CallOutcome::Replied(Reply::Done(_))))
            .count(),
        3
    );
    assert!(outcomes.contains(&CallOutcome::Replied(Reply::Duplicate)));
    assert!(outcomes.contains(&CallOutcome::Replied(Reply::Full)));
    let final_report = reports.borrow().last().cloned().unwrap();
    assert!(final_report.counts_agree());
    assert_eq!(final_report.parked, 0);
    assert_eq!(final_report.completed_count, 3);
    assert_eq!(final_report.retired_count, 0);
    assert_eq!(final_report.duplicate_key_count, 1);
    assert_eq!(final_report.admission.full_count, 1);
}

#[test]
fn sim_late_completion_retires_caller_gone_then_refills() {
    let mut sim = Simulator::new(TestShard, SimulatorConfig::default());
    let reports = Rc::new(RefCell::new(Vec::new()));
    let service = sim.register_split_service::<Service, Event, Request, Infallible>(
        Service {
            pending: ConcurrencyPendingReplies::with_capacity("sim.caller_gone", 1),
            reports: Rc::clone(&reports),
        },
        16,
    );
    let outcomes = Rc::new(RefCell::new(Vec::new()));
    let client = sim.register(Client {
        outcomes: Rc::clone(&outcomes),
    });

    sim.try_send(
        client,
        ClientMsg::Start {
            target: service.requests,
            key: 1,
            hold_for: Duration::from_millis(100),
            timeout: Duration::from_millis(1),
        },
    )
    .unwrap();
    sim.run_until_quiescent();
    sim.try_send(
        client,
        ClientMsg::Start {
            target: service.requests,
            key: 2,
            hold_for: Duration::from_millis(1),
            timeout: Duration::from_secs(1),
        },
    )
    .unwrap();
    sim.run_until_quiescent();

    let outcomes = outcomes.borrow();
    assert_eq!(outcomes.len(), 2);
    assert!(matches!(outcomes[0], CallOutcome::Timeout));
    assert_eq!(outcomes[1], CallOutcome::Replied(Reply::Done(2)));
    let final_report = reports.borrow().last().cloned().unwrap();
    assert!(final_report.counts_agree());
    assert_eq!(final_report.parked, 0);
    assert_eq!(final_report.caller_gone_count, 1);
    assert_eq!(final_report.retired_count, 1);
    assert_eq!(final_report.completed_count, 1);
}
