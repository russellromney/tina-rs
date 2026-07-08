use std::cell::RefCell;
use std::rc::Rc;
use std::time::Duration;

use tina::{Address, CallContext, Context, Effect, Isolate, RequestContext, noop, reply_to};
use tina_runtime::{CallOutcome, RuntimeCall, call, sleep};
use tina_sim::{Simulator, SimulatorConfig};

#[derive(Debug, Clone)]
pub struct RunConfig {
    pub probe_delay_ms: u64,
    pub db_delay_ms: u64,
}

pub struct RunReport {
    pub replies: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ProbeReply;

#[derive(Debug)]
enum ProbeMsg {
    Request,
    SleepDone(RequestContext<ProbeReply>),
}

#[derive(Debug)]
struct Probe {
    delay_ms: u64,
}

impl Isolate for Probe {
    type Message = ProbeMsg;
    type Reply = ProbeReply;
    type Send = tina::Outbound<std::convert::Infallible>;
    type Spawn = std::convert::Infallible;
    type SpawnObserved = std::convert::Infallible;
    type Io = RuntimeCall<ProbeMsg>;
    type Fact = std::convert::Infallible;
    type Shard = tina::SingleShard;

    fn handle(
        &mut self,
        msg: Self::Message,
        _ctx: &mut Context<'_, Self::Shard, Self::Reply>,
    ) -> Effect<Self> {
        match msg {
            ProbeMsg::Request => noop(),
            ProbeMsg::SleepDone(req) => reply_to(req, ProbeReply),
        }
    }

    fn handle_call(&mut self, msg: Self::Message, call: CallContext<'_, Self>) -> Effect<Self> {
        match msg {
            ProbeMsg::Request => call
                .defer(sleep(Duration::from_millis(self.delay_ms)))
                .reply(|req, _| ProbeMsg::SleepDone(req)),
            ProbeMsg::SleepDone(_) => call.reject(tina::CallRejectedReason::UnsupportedMessage),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct DbReply;

#[derive(Debug)]
enum DbMsg {
    Request,
    SleepDone(RequestContext<DbReply>),
}

#[derive(Debug)]
struct Db {
    delay_ms: u64,
}

impl Isolate for Db {
    type Message = DbMsg;
    type Reply = DbReply;
    type Send = tina::Outbound<std::convert::Infallible>;
    type Spawn = std::convert::Infallible;
    type SpawnObserved = std::convert::Infallible;
    type Io = RuntimeCall<DbMsg>;
    type Fact = std::convert::Infallible;
    type Shard = tina::SingleShard;

    fn handle(
        &mut self,
        msg: Self::Message,
        _ctx: &mut Context<'_, Self::Shard, Self::Reply>,
    ) -> Effect<Self> {
        match msg {
            DbMsg::Request => noop(),
            DbMsg::SleepDone(req) => reply_to(req, DbReply),
        }
    }

    fn handle_call(&mut self, msg: Self::Message, call: CallContext<'_, Self>) -> Effect<Self> {
        match msg {
            DbMsg::Request => call
                .defer(sleep(Duration::from_millis(self.delay_ms)))
                .reply(|req, _| DbMsg::SleepDone(req)),
            DbMsg::SleepDone(_) => call.reject(tina::CallRejectedReason::UnsupportedMessage),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum ServiceReply {
    Ready,
    NotReady,
}

#[derive(Debug)]
enum ServiceMsg {
    Start,
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
                CallOutcome::Replied(_) => call(self.db, DbMsg::Request, Duration::from_millis(50))
                    .then_with_request(req, |req, outcome| {
                        ServiceMsg::Readiness(ReadinessFlow::Dbed(req, outcome))
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

// `flow!` does not derive `Debug`; `ServiceMsg` needs it because a peer holds
// `Address<ServiceMsg, _>` in a `Debug` enum. Print the outcome, skip `req`.
impl std::fmt::Debug for ReadinessFlow {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ReadinessFlow::Probed(_, outcome) => f.debug_tuple("Probed").field(outcome).finish(),
            ReadinessFlow::Dbed(_, outcome) => f.debug_tuple("Dbed").field(outcome).finish(),
        }
    }
}

#[derive(Debug)]
struct Service {
    probe: Address<ProbeMsg, ProbeReply>,
    db: Address<DbMsg, DbReply>,
}

impl Isolate for Service {
    type Message = ServiceMsg;
    type Reply = ServiceReply;
    type Send = tina::Outbound<std::convert::Infallible>;
    type Spawn = std::convert::Infallible;
    type SpawnObserved = std::convert::Infallible;
    type Io = RuntimeCall<ServiceMsg>;
    type Fact = std::convert::Infallible;
    type Shard = tina::SingleShard;

    fn handle(
        &mut self,
        msg: Self::Message,
        _ctx: &mut Context<'_, Self::Shard, Self::Reply>,
    ) -> Effect<Self> {
        match msg {
            ServiceMsg::Start => noop(),
            ServiceMsg::Readiness(flow) => self.handle_readiness_flow(flow),
        }
    }

    fn handle_call(&mut self, msg: Self::Message, call_ctx: CallContext<'_, Self>) -> Effect<Self> {
        match msg {
            ServiceMsg::Start => call_ctx
                .defer(call(
                    self.probe,
                    ProbeMsg::Request,
                    Duration::from_millis(50),
                ))
                .reply(|req, outcome| ServiceMsg::Readiness(ReadinessFlow::Probed(req, outcome))),
            ServiceMsg::Readiness(_) => {
                call_ctx.reject(tina::CallRejectedReason::UnsupportedMessage)
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum ClientReply {}

#[derive(Debug)]
enum ClientMsg {
    Start(Address<ServiceMsg, ServiceReply>),
    Returned(CallOutcome<ServiceReply>),
}

#[derive(Debug)]
struct Client {
    replies: Rc<RefCell<Vec<String>>>,
}

impl Isolate for Client {
    type Message = ClientMsg;
    type Reply = ClientReply;
    type Send = tina::Outbound<std::convert::Infallible>;
    type Spawn = std::convert::Infallible;
    type SpawnObserved = std::convert::Infallible;
    type Io = RuntimeCall<ClientMsg>;
    type Fact = std::convert::Infallible;
    type Shard = tina::SingleShard;

    fn handle(
        &mut self,
        msg: Self::Message,
        _ctx: &mut Context<'_, Self::Shard, Self::Reply>,
    ) -> Effect<Self> {
        match msg {
            ClientMsg::Start(svc) => {
                call(svc, ServiceMsg::Start, Duration::from_millis(100)).then(ClientMsg::Returned)
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
    let probe = sim.register(Probe {
        delay_ms: config.probe_delay_ms,
    });
    let db = sim.register(Db {
        delay_ms: config.db_delay_ms,
    });
    let service = sim.register(Service { probe, db });
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
