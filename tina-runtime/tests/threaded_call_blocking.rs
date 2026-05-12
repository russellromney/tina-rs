use std::convert::Infallible;
use std::time::Duration;

use tina::prelude::*;
use tina::{CallContext, RequestContext, reply_to_request};
use tina_runtime::{
    CallOutcome, DefaultThreadedMailboxFactory, RuntimeCall, ThreadedRuntime, ThreadedRuntimeConfig,
};

#[derive(Debug, Clone, Copy)]
struct TestShard;

impl Shard for TestShard {
    fn id(&self) -> ShardId {
        ShardId::new(0)
    }
}

#[derive(Debug)]
enum EchoMsg {
    AddOne(u32),
}

struct Echo;

impl Isolate for Echo {
    type Message = EchoMsg;
    type Reply = u32;
    type Send = Outbound<Infallible>;
    type Spawn = Infallible;
    type SpawnObserved = Infallible;
    type Call = Infallible;
    type Shard = TestShard;

    fn handle(&mut self, msg: EchoMsg, _ctx: &mut Context<'_, TestShard, u32>) -> Effect<Self> {
        match msg {
            EchoMsg::AddOne(value) => reply(value + 1),
        }
    }

    fn handle_call(&mut self, msg: EchoMsg, call: CallContext<'_, Self>) -> Effect<Self> {
        match msg {
            EchoMsg::AddOne(value) => call.reply(value + 1),
        }
    }
}

#[derive(Debug)]
enum SilentMsg {
    Start,
    Done(RequestContext<u32>),
}

struct Silent;

impl Isolate for Silent {
    type Message = SilentMsg;
    type Reply = u32;
    type Send = Outbound<Infallible>;
    type Spawn = Infallible;
    type SpawnObserved = Infallible;
    type Call = RuntimeCall<SilentMsg>;
    type Shard = TestShard;

    fn handle(&mut self, msg: SilentMsg, _ctx: &mut Context<'_, TestShard, u32>) -> Effect<Self> {
        match msg {
            SilentMsg::Start => noop(),
            SilentMsg::Done(req) => reply_to_request(req, 1),
        }
    }

    fn handle_call(&mut self, msg: SilentMsg, call: CallContext<'_, Self>) -> Effect<Self> {
        match msg {
            SilentMsg::Start => {
                let req = call.into_request_context();
                tina_runtime::sleep(Duration::from_millis(100)).reply(move |_| SilentMsg::Done(req))
            }
            SilentMsg::Done(_) => call.reject(tina::CallRejectedReason::UnsupportedMessage),
        }
    }
}

#[derive(Debug)]
enum StopperMsg {
    Stop,
    Ping,
}

struct Stopper;

impl Isolate for Stopper {
    type Message = StopperMsg;
    type Reply = u32;
    type Send = Outbound<Infallible>;
    type Spawn = Infallible;
    type SpawnObserved = Infallible;
    type Call = Infallible;
    type Shard = TestShard;

    fn handle(&mut self, msg: StopperMsg, _ctx: &mut Context<'_, TestShard, u32>) -> Effect<Self> {
        match msg {
            StopperMsg::Stop => stop(),
            StopperMsg::Ping => reply(1),
        }
    }

    fn handle_call(&mut self, msg: StopperMsg, call: CallContext<'_, Self>) -> Effect<Self> {
        match msg {
            StopperMsg::Ping => call.reply(1),
            StopperMsg::Stop => call.reject(tina::CallRejectedReason::UnsupportedMessage),
        }
    }
}

fn runtime() -> ThreadedRuntime<TestShard, DefaultThreadedMailboxFactory> {
    ThreadedRuntime::with_config(
        TestShard,
        DefaultThreadedMailboxFactory,
        ThreadedRuntimeConfig {
            command_capacity: 128,
            idle_wait: Duration::from_millis(1),
            ..Default::default()
        },
    )
}

#[test]
fn call_blocking_returns_typed_reply() {
    let runtime = runtime();
    let echo = runtime
        .register_with_capacity::<Echo, Infallible>(Echo, 4)
        .expect("register echo");

    let outcome = runtime
        .call_blocking(echo, EchoMsg::AddOne(41), Duration::from_secs(1))
        .expect("call blocking");

    assert_eq!(outcome, CallOutcome::Replied(42));
    runtime.shutdown().expect("shutdown");
}

#[test]
fn call_blocking_preserves_timeout() {
    let runtime = runtime();
    let silent = runtime
        .register_with_capacity::<Silent, Infallible>(Silent, 4)
        .expect("register silent");

    let outcome = runtime
        .call_blocking(silent, SilentMsg::Start, Duration::from_millis(25))
        .expect("call blocking");

    assert_eq!(outcome, CallOutcome::Timeout);
    runtime.shutdown().expect("shutdown");
}

#[test]
fn call_blocking_preserves_closed() {
    let runtime = runtime();
    let stopper = runtime
        .register_with_capacity::<Stopper, Infallible>(Stopper, 4)
        .expect("register stopper");
    let stopped = runtime.observe_isolate_complete(stopper);
    runtime
        .try_send(stopper, StopperMsg::Stop)
        .expect("send stop");
    stopped
        .wait(Duration::from_secs(1))
        .expect("stopper completes");

    let outcome = runtime
        .call_blocking(stopper, StopperMsg::Ping, Duration::from_secs(1))
        .expect("call blocking");

    assert_eq!(outcome, CallOutcome::Closed);
    runtime.shutdown().expect("shutdown");
}
