use std::convert::Infallible;
use std::time::Duration;

use tina::{Outbound, noop, reply_to_request};
use tina_runtime::{
    CallOutcome, DefaultThreadedMailboxFactory, RuntimeCall, ThreadedRuntime,
    ThreadedRuntimeConfig, call,
};

#[derive(Debug)]
enum WorkerMsg {
    AddOne(u32),
}

struct Worker;

#[tina_runtime::isolate(message = WorkerMsg, reply = u32)]
impl Worker {
    fn handle(
        &mut self,
        _msg: WorkerMsg,
        _ctx: &mut tina::Context<'_, tina::SingleShard, Self::Reply>,
    ) -> tina::Effect<Self> {
        noop()
    }

    fn handle_call(
        &mut self,
        msg: WorkerMsg,
        call: tina::CallContext<'_, Self>,
    ) -> tina::Effect<Self> {
        match msg {
            WorkerMsg::AddOne(value) => call.reply(value + 1),
        }
    }
}

enum DriverMsg {
    Start(u32),
    Flow(AddFlow),
}

struct Driver {
    worker: tina::Address<WorkerMsg, u32>,
}

tina::flow! {
    flow AddFlow for Driver {
        reply u32;

        step WorkerReturned(original: u32) -> u32 {
            match outcome {
                CallOutcome::Replied(value) => reply_to_request(req, original + value),
                CallOutcome::Full
                | CallOutcome::Closed
                | CallOutcome::Timeout
                | CallOutcome::Rejected(_) => reply_to_request(req, 0),
            }
        }
    }
}

#[tina_runtime::isolate(
    message = DriverMsg,
    reply = u32,
    send = Outbound<Infallible>,
    call = RuntimeCall<DriverMsg>
)]
impl Driver {
    fn handle(
        &mut self,
        msg: DriverMsg,
        _ctx: &mut tina::Context<'_, tina::SingleShard, Self::Reply>,
    ) -> tina::Effect<Self> {
        match msg {
            DriverMsg::Start(_) => noop(),
            DriverMsg::Flow(flow) => self.handle_add_flow(flow),
        }
    }

    fn handle_call(
        &mut self,
        msg: DriverMsg,
        call_ctx: tina::CallContext<'_, Self>,
    ) -> tina::Effect<Self> {
        match msg {
            DriverMsg::Start(value) => call_ctx
                .defer(call(
                    self.worker,
                    WorkerMsg::AddOne(value),
                    Duration::from_secs(1),
                ))
                .reply(move |req, outcome| {
                    DriverMsg::Flow(AddFlow::WorkerReturned(req, value, outcome))
                }),
            DriverMsg::Flow(_) => call_ctx.reject(tina::CallRejectedReason::UnsupportedMessage),
        }
    }
}

fn runtime() -> ThreadedRuntime<tina::SingleShard, DefaultThreadedMailboxFactory> {
    ThreadedRuntime::with_config(
        tina::SingleShard,
        DefaultThreadedMailboxFactory,
        ThreadedRuntimeConfig {
            command_capacity: 64,
            idle_wait: Duration::from_millis(1),
            ..Default::default()
        },
    )
}

#[test]
fn generated_flow_dispatches_through_runtime_call_and_replies() {
    let runtime = runtime();
    let worker = runtime
        .register_with_capacity::<Worker, Infallible>(Worker, 8)
        .expect("register worker");
    let driver = runtime
        .register_with_capacity::<Driver, Infallible>(Driver { worker }, 8)
        .expect("register driver");

    let outcome = runtime
        .call_blocking(driver, DriverMsg::Start(40), Duration::from_secs(1))
        .expect("host call");

    assert_eq!(outcome, CallOutcome::Replied(81));
    runtime.shutdown().expect("shutdown");
}
