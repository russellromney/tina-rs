use std::cell::RefCell;
use std::convert::Infallible;
use std::rc::Rc;
use std::time::Duration;

use tina::prelude::*;
use tina_runtime::{CallOutcome, call_request, sleep};
use tina_sim::{Simulator, SimulatorConfig};

#[derive(Debug, Clone)]
pub struct RunConfig {
    pub probe_delay_ms: u64,
    pub db_delay_ms: u64,
}

pub struct RunReport {
    pub replies: Vec<String>,
}

// --- Probe ---------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ProbeReply;

/// Caller-authority request: the only thing an outside caller can ask.
#[derive(Debug)]
enum ProbeRequest {
    Request,
}

/// Internal event: sleep finished for a call in flight.
#[derive(Debug)]
enum ProbeEvent {
    SleepDone(RequestContext<ProbeReply>),
}

struct Probe {
    delay_ms: u64,
}

#[tina_runtime::isolate(event = ProbeEvent, request = ProbeRequest, reply = ProbeReply)]
impl Probe {
    fn handle_event(
        &mut self,
        event: ProbeEvent,
        _ctx: &mut Context<'_, SingleShard, Self::Reply>,
    ) -> Effect<Self> {
        match event {
            ProbeEvent::SleepDone(req) => reply_to(req, ProbeReply),
        }
    }

    fn handle_request(
        &mut self,
        request: ProbeRequest,
        call: RequestCall<'_, Self>,
    ) -> RequestEffect<Self> {
        match request {
            ProbeRequest::Request => call
                .defer(sleep(Duration::from_millis(self.delay_ms)))
                .reply_service_event(|req, _| ProbeEvent::SleepDone(req)),
        }
    }
}

// --- Db ------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct DbReply;

/// Caller-authority request: the only thing an outside caller can ask.
#[derive(Debug)]
enum DbRequest {
    Request,
}

/// Internal event: sleep finished for a call in flight.
#[derive(Debug)]
enum DbEvent {
    SleepDone(RequestContext<DbReply>),
}

struct Db {
    delay_ms: u64,
}

#[tina_runtime::isolate(event = DbEvent, request = DbRequest, reply = DbReply)]
impl Db {
    fn handle_event(
        &mut self,
        event: DbEvent,
        _ctx: &mut Context<'_, SingleShard, Self::Reply>,
    ) -> Effect<Self> {
        match event {
            DbEvent::SleepDone(req) => reply_to(req, DbReply),
        }
    }

    fn handle_request(
        &mut self,
        request: DbRequest,
        call: RequestCall<'_, Self>,
    ) -> RequestEffect<Self> {
        match request {
            DbRequest::Request => call
                .defer(sleep(Duration::from_millis(self.delay_ms)))
                .reply_service_event(|req, _| DbEvent::SleepDone(req)),
        }
    }
}

// --- Service -------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
enum ServiceReply {
    Ready,
    NotReady,
}

/// Caller-authority request: the only thing an outside caller can ask.
#[derive(Debug)]
enum ServiceRequest {
    Start,
}

/// Internal event: readiness-flow continuation, never caller authority.
#[derive(Debug)]
enum ServiceEvent {
    Readiness(ReadinessFlow),
}

// Linear two-step readiness check: probe, then db. `tina::flow!` writes the
// continuation enum + dispatcher a hand-written state machine would spell out;
// each step still receives the full `CallOutcome` and threads the caller's
// `RequestContext` explicitly.
tina::flow! {
    flow ReadinessFlow for Service {
        reply ServiceReply;

        step Probed() -> ProbeReply {
            match outcome {
                CallOutcome::Replied(_) => call_request(
                    self.db,
                    DbRequest::Request,
                    Duration::from_millis(50),
                )
                .then_service_event_with_request(req, |req, outcome| {
                    ServiceEvent::Readiness(ReadinessFlow::Dbed(req, outcome))
                }),
                _ => reply_to(req, ServiceReply::NotReady),
            }
        }

        step Dbed() -> DbReply {
            match outcome {
                CallOutcome::Replied(_) => reply_to(req, ServiceReply::Ready),
                _ => reply_to(req, ServiceReply::NotReady),
            }
        }
    }
}

// `flow!` does not derive `Debug`; `ServiceEvent` needs it because a peer holds
// the service address in a `Debug` enum. Print the outcome, skip `req`.
impl std::fmt::Debug for ReadinessFlow {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ReadinessFlow::Probed(_, outcome) => f.debug_tuple("Probed").field(outcome).finish(),
            ReadinessFlow::Dbed(_, outcome) => f.debug_tuple("Dbed").field(outcome).finish(),
        }
    }
}

struct Service {
    probe: tina::ServiceRequestAddress<ProbeEvent, ProbeRequest, ProbeReply>,
    db: tina::ServiceRequestAddress<DbEvent, DbRequest, DbReply>,
}

#[tina_runtime::isolate(event = ServiceEvent, request = ServiceRequest, reply = ServiceReply)]
impl Service {
    fn handle_event(
        &mut self,
        event: ServiceEvent,
        _ctx: &mut Context<'_, SingleShard, Self::Reply>,
    ) -> Effect<Self> {
        match event {
            ServiceEvent::Readiness(flow) => self.handle_readiness_flow(flow),
        }
    }

    fn handle_request(
        &mut self,
        request: ServiceRequest,
        call: RequestCall<'_, Self>,
    ) -> RequestEffect<Self> {
        match request {
            ServiceRequest::Start => call
                .defer(call_request(
                    self.probe,
                    ProbeRequest::Request,
                    Duration::from_millis(50),
                ))
                .reply_service_event(|req, outcome| {
                    ServiceEvent::Readiness(ReadinessFlow::Probed(req, outcome))
                }),
        }
    }
}

// --- Client --------------------------------------------------------------

#[derive(Debug)]
enum ClientMsg {
    Start(tina::ServiceRequestAddress<ServiceEvent, ServiceRequest, ServiceReply>),
    Returned(CallOutcome<ServiceReply>),
}

struct Client {
    replies: Rc<RefCell<Vec<String>>>,
}

#[tina_runtime::isolate(message = ClientMsg)]
impl Client {
    fn handle(
        &mut self,
        msg: ClientMsg,
        _ctx: &mut Context<'_, SingleShard, Self::Reply>,
    ) -> Effect<Self> {
        match msg {
            ClientMsg::Start(svc) => {
                call_request(svc, ServiceRequest::Start, Duration::from_millis(100))
                    .then(ClientMsg::Returned)
            }
            ClientMsg::Returned(CallOutcome::Replied(ServiceReply::Ready)) => {
                self.replies.borrow_mut().push(String::from("ready"));
                noop()
            }
            ClientMsg::Returned(CallOutcome::Replied(ServiceReply::NotReady)) => {
                self.replies.borrow_mut().push(String::from("not_ready"));
                noop()
            }
            ClientMsg::Returned(CallOutcome::Timeout) => {
                self.replies.borrow_mut().push(String::from("timeout"));
                noop()
            }
            ClientMsg::Returned(CallOutcome::Full) => {
                self.replies.borrow_mut().push(String::from("full"));
                noop()
            }
            ClientMsg::Returned(CallOutcome::Closed) => {
                self.replies.borrow_mut().push(String::from("closed"));
                noop()
            }
            ClientMsg::Returned(CallOutcome::Rejected(reason)) => {
                self.replies
                    .borrow_mut()
                    .push(format!("rejected:{reason:?}"));
                noop()
            }
        }
    }
}

pub fn run(config: RunConfig) -> anyhow::Result<RunReport> {
    let mut sim = Simulator::new(tina::SingleShard, SimulatorConfig::default());
    let probe = sim
        .register_split_service::<Probe, ProbeEvent, ProbeRequest, Infallible>(
            Probe {
                delay_ms: config.probe_delay_ms,
            },
            usize::MAX,
        )
        .requests;
    let db = sim
        .register_split_service::<Db, DbEvent, DbRequest, Infallible>(
            Db {
                delay_ms: config.db_delay_ms,
            },
            usize::MAX,
        )
        .requests;
    let service = sim
        .register_split_service::<Service, ServiceEvent, ServiceRequest, Infallible>(
            Service { probe, db },
            usize::MAX,
        )
        .requests;
    let replies = Rc::new(RefCell::new(Vec::new()));
    let client = sim.register(Client {
        replies: Rc::clone(&replies),
    });

    sim.try_send(client, ClientMsg::Start(service))
        .map_err(|e| anyhow::anyhow!("client send failed: {:?}", e))?;
    sim.run_until_quiescent();

    Ok(RunReport {
        replies: replies.borrow().clone(),
    })
}
