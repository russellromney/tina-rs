use std::time::Duration;

use tina::{
    Address, CallContext, Context, Effect, Isolate, RequestContext, noop, reply_to_request,
};
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
    type Call = RuntimeCall<ProbeMsg>;
    type Shard = tina::SingleShard;

    fn handle(
        &mut self,
        msg: Self::Message,
        _ctx: &mut Context<'_, Self::Shard, Self::Reply>,
    ) -> Effect<Self> {
        match msg {
            ProbeMsg::Request => {
                noop()
            }
            ProbeMsg::SleepDone(req) => reply_to_request(req, ProbeReply),
        }
    }

    fn handle_call(&mut self, msg: Self::Message, call: CallContext<'_, Self>) -> Effect<Self> {
        match msg {
            ProbeMsg::Request => {
                let req = call.into_request_context();
                sleep(Duration::from_millis(self.delay_ms)).reply(move |_| ProbeMsg::SleepDone(req))
            }
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
    type Call = RuntimeCall<DbMsg>;
    type Shard = tina::SingleShard;

    fn handle(
        &mut self,
        msg: Self::Message,
        _ctx: &mut Context<'_, Self::Shard, Self::Reply>,
    ) -> Effect<Self> {
        match msg {
            DbMsg::Request => {
                noop()
            }
            DbMsg::SleepDone(req) => reply_to_request(req, DbReply),
        }
    }

    fn handle_call(&mut self, msg: Self::Message, call: CallContext<'_, Self>) -> Effect<Self> {
        match msg {
            DbMsg::Request => {
                let req = call.into_request_context();
                sleep(Duration::from_millis(self.delay_ms)).reply(move |_| DbMsg::SleepDone(req))
            }
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
    ProbeResult(RequestContext<ServiceReply>, CallOutcome<ProbeReply>),
    DbResult(RequestContext<ServiceReply>, CallOutcome<DbReply>),
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
    type Call = RuntimeCall<ServiceMsg>;
    type Shard = tina::SingleShard;

    fn handle(
        &mut self,
        msg: Self::Message,
        _ctx: &mut Context<'_, Self::Shard, Self::Reply>,
    ) -> Effect<Self> {
        match msg {
            ServiceMsg::Start => noop(),
            ServiceMsg::ProbeResult(req, CallOutcome::Replied(_)) => {
                call(self.db, DbMsg::Request, Duration::from_millis(50))
                    .reply_with_request(req, ServiceMsg::DbResult)
            }
            ServiceMsg::ProbeResult(req, _) => reply_to_request(req, ServiceReply::NotReady),
            ServiceMsg::DbResult(req, CallOutcome::Replied(_)) => {
                reply_to_request(req, ServiceReply::Ready)
            }
            ServiceMsg::DbResult(req, _) => reply_to_request(req, ServiceReply::NotReady),
        }
    }

    fn handle_call(&mut self, msg: Self::Message, call_ctx: CallContext<'_, Self>) -> Effect<Self> {
        match msg {
            ServiceMsg::Start => {
                let req = call_ctx.into_request_context();
                call(self.probe, ProbeMsg::Request, Duration::from_millis(50))
                    .reply_with_request(req, ServiceMsg::ProbeResult)
            }
            ServiceMsg::ProbeResult(_, _) | ServiceMsg::DbResult(_, _) => {
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
    replies: Vec<String>,
}

impl Isolate for Client {
    type Message = ClientMsg;
    type Reply = ClientReply;
    type Send = tina::Outbound<std::convert::Infallible>;
    type Spawn = std::convert::Infallible;
    type Call = RuntimeCall<ClientMsg>;
    type Shard = tina::SingleShard;

    fn handle(
        &mut self,
        msg: Self::Message,
        _ctx: &mut Context<'_, Self::Shard, Self::Reply>,
    ) -> Effect<Self> {
        match msg {
            ClientMsg::Start(svc) => {
                call(svc, ServiceMsg::Start, Duration::from_millis(100))
                    .reply(ClientMsg::Returned)
            }
            ClientMsg::Returned(CallOutcome::Replied(ServiceReply::Ready)) => {
                self.replies.push(String::from("ready"));
                noop()
            }
            ClientMsg::Returned(CallOutcome::Replied(ServiceReply::NotReady)) => {
                self.replies.push(String::from("not_ready"));
                noop()
            }
            ClientMsg::Returned(CallOutcome::Timeout) => {
                self.replies.push(String::from("timeout"));
                noop()
            }
            ClientMsg::Returned(CallOutcome::Full) => {
                self.replies.push(String::from("full"));
                noop()
            }
            ClientMsg::Returned(CallOutcome::Closed) => {
                self.replies.push(String::from("closed"));
                noop()
            }
            ClientMsg::Returned(CallOutcome::Rejected(reason)) => {
                self.replies.push(format!("rejected:{reason:?}"));
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
    let client = sim.register(Client { replies: Vec::new() });

    sim.try_send(client, ClientMsg::Start(service))
        .map_err(|e| anyhow::anyhow!("client send failed: {:?}", e))?;
    sim.run_until_quiescent();

    // In a real runtime, events would live in the isolate state.
    // In simulation, extract via the trace.
    let replies: Vec<String> = sim
        .trace()
        .iter()
        .filter_map(|event| {
            if matches!(
                event.kind(),
                tina_runtime::RuntimeEventKind::DeferredReplySent { .. }
            ) {
                Some(String::from("ready"))
            } else {
                None
            }
        })
        .collect();

    Ok(RunReport { replies })
}
