//! Simulator parity for concurrency-charged parked callers.

use std::cell::RefCell;
use std::convert::Infallible;
use std::rc::Rc;
use std::time::Duration;

use tina::prelude::*;
use tina_runtime::{
    AdmissionFailure, CallOutcome, ConcurrencyParkError, ConcurrencyPendingReplies, call_request,
    request_effect_after_concurrency_park, sleep,
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
}

#[tina_runtime::isolate(event = Event, request = Request, reply = Reply, shard = TestShard)]
impl Service {
    fn handle_event(
        &mut self,
        event: Event,
        _ctx: &mut Context<'_, TestShard, Self::Reply>,
    ) -> Effect<Self> {
        match event {
            Event::Complete(key) => self
                .pending
                .reply_by_key::<Self>(&key, Reply::Done(key))
                .unwrap_or_else(noop),
        }
    }

    fn handle_request(
        &mut self,
        request: Request,
        call: RequestCall<'_, Self>,
    ) -> RequestEffect<Self> {
        match request {
            Request::Hold { key, duration } => match self.pending.park_request(key, call) {
                Ok(ticket) => request_effect_after_concurrency_park(
                    &ticket,
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
            ClientMsg::Start { target, key } => call_request(
                target,
                Request::Hold {
                    key,
                    duration: Duration::from_millis(10),
                },
                Duration::from_secs(1),
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
    let service = sim.register_split_service::<Service, Event, Request, Infallible>(
        Service {
            pending: ConcurrencyPendingReplies::with_capacity("sim.parked", 2),
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
            },
        )
        .unwrap();
    }
    sim.run_until_quiescent();

    let outcomes = outcomes.borrow();
    assert_eq!(outcomes.len(), 4);
    assert_eq!(
        outcomes
            .iter()
            .filter(|outcome| matches!(outcome, CallOutcome::Replied(Reply::Done(_))))
            .count(),
        2
    );
    assert!(outcomes.contains(&CallOutcome::Replied(Reply::Duplicate)));
    assert!(outcomes.contains(&CallOutcome::Replied(Reply::Full)));
}
