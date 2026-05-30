use std::convert::Infallible;
use std::sync::{Arc, Barrier};
use std::thread;
use std::time::{Duration, Instant};

use tina::prelude::*;
use tina::{RequestContext, reply_to_request};
use tina_runtime::{
    CallOutcome, DefaultThreadedMailboxFactory, RuntimeCall, ThreadedRuntime,
    ThreadedRuntimeConfig, ThreadedRuntimeError,
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

#[tina_runtime::isolate(message = EchoMsg, reply = u32, shard = TestShard)]
impl Echo {
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

#[tina_runtime::isolate(message = SilentMsg, reply = u32, shard = TestShard)]
impl Silent {
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
                tina_runtime::sleep(Duration::from_millis(100)).then(move |_| SilentMsg::Done(req))
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

#[tina_runtime::isolate(message = StopperMsg, reply = u32, shard = TestShard)]
impl Stopper {
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

#[derive(Debug)]
enum AsyncBridgeMsg {
    Send(u32),
    Poll(RequestContext<u32>, u32),
}

struct AsyncBridge;

#[tina_runtime::isolate(
    message = AsyncBridgeMsg,
    reply = u32,
    call = RuntimeCall<AsyncBridgeMsg>,
    shard = TestShard
)]
impl AsyncBridge {
    fn handle(
        &mut self,
        msg: AsyncBridgeMsg,
        _ctx: &mut Context<'_, TestShard, u32>,
    ) -> Effect<Self> {
        match msg {
            AsyncBridgeMsg::Send(_) => noop(),
            AsyncBridgeMsg::Poll(req, value) => reply_to_request(req, value + 1),
        }
    }

    fn handle_call(&mut self, msg: AsyncBridgeMsg, call: CallContext<'_, Self>) -> Effect<Self> {
        match msg {
            AsyncBridgeMsg::Send(value) => {
                let req = call.into_request_context();
                tina_runtime::sleep(Duration::from_millis(5))
                    .then(move |_| AsyncBridgeMsg::Poll(req, value))
            }
            AsyncBridgeMsg::Poll(_, _) => call.reject(tina::CallRejectedReason::UnsupportedMessage),
        }
    }
}

#[derive(Debug)]
enum RelayMsg {
    Start(u32),
    BridgeDone(RequestContext<u32>, CallOutcome<u32>),
}

struct Relay {
    bridge: Address<AsyncBridgeMsg, u32>,
}

#[tina_runtime::isolate(
    message = RelayMsg,
    reply = u32,
    call = RuntimeCall<RelayMsg>,
    shard = TestShard
)]
impl Relay {
    fn handle(&mut self, msg: RelayMsg, _ctx: &mut Context<'_, TestShard, u32>) -> Effect<Self> {
        match msg {
            RelayMsg::Start(_) => noop(),
            RelayMsg::BridgeDone(req, CallOutcome::Replied(value)) => {
                reply_to_request(req, value + 1)
            }
            RelayMsg::BridgeDone(req, _) => reply_to_request(req, 0),
        }
    }

    fn handle_call(&mut self, msg: RelayMsg, call_ctx: CallContext<'_, Self>) -> Effect<Self> {
        match msg {
            RelayMsg::Start(value) => call_ctx
                .defer(tina_runtime::call(
                    self.bridge,
                    AsyncBridgeMsg::Send(value),
                    Duration::from_secs(1),
                ))
                .reply(RelayMsg::BridgeDone),
            RelayMsg::BridgeDone(_, _) => {
                call_ctx.reject(tina::CallRejectedReason::UnsupportedMessage)
            }
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

#[test]
fn call_blocking_defer_through_multi_turn_callee_preserves_request_context() {
    let runtime = runtime();
    let bridge = runtime
        .register_with_capacity::<AsyncBridge, Infallible>(AsyncBridge, 8)
        .expect("register async bridge");
    let relay = runtime
        .register_with_capacity::<Relay, Infallible>(Relay { bridge }, 8)
        .expect("register relay");

    let outcome = runtime
        .call_blocking(relay, RelayMsg::Start(40), Duration::from_secs(1))
        .expect("call blocking");

    assert_eq!(outcome, CallOutcome::Replied(42));
    runtime.shutdown().expect("shutdown");
}

#[test]
fn call_blocking_with_host_timeout_surfaces_host_wait_timeout() {
    let runtime = runtime();
    let silent = runtime
        .register_with_capacity::<Silent, Infallible>(Silent, 4)
        .expect("register silent");

    // Silent::Start parks on a 100ms timer before replying. A generous target
    // deadline keeps the call alive while a tiny host budget expires first, so
    // the host wait surfaces distinctly as HostWaitTimeout — not a target
    // Timeout, and not a hang.
    let outcome = runtime.call_blocking_with_host_timeout(
        silent,
        SilentMsg::Start,
        Duration::from_secs(2),
        Duration::from_millis(20),
    );

    assert_eq!(outcome, Err(ThreadedRuntimeError::HostWaitTimeout));
    let _ = runtime.shutdown();
}

#[test]
fn call_blocking_dispatcher_pressure_never_reports_worker_stopped() {
    let runtime = Arc::new(ThreadedRuntime::with_config(
        TestShard,
        DefaultThreadedMailboxFactory,
        ThreadedRuntimeConfig {
            command_capacity: 1,
            idle_wait: Duration::from_millis(2),
            ..Default::default()
        },
    ));
    let silent = runtime
        .register_with_capacity::<Silent, Infallible>(Silent, 1)
        .expect("register silent");

    // Tiny command and target mailboxes force a messy burst through the
    // persistent dispatcher pool. Any outcome may be legitimate pressure,
    // but `WorkerStopped` would be a lie caused by dropping the dispatcher
    // task without answering the pooled host reply.
    let workers = 256;
    let barrier = Arc::new(Barrier::new(workers));
    let mut handles = Vec::with_capacity(workers);
    for _ in 0..workers {
        let runtime = Arc::clone(&runtime);
        let barrier = Arc::clone(&barrier);
        handles.push(thread::spawn(move || {
            barrier.wait();
            runtime.call_blocking_with_host_timeout(
                silent,
                SilentMsg::Start,
                Duration::from_secs(1),
                Duration::from_secs(2),
            )
        }));
    }

    let mut counts = std::collections::BTreeMap::new();
    for handle in handles {
        match handle.join().expect("caller thread joined") {
            Ok(CallOutcome::Full) => *counts.entry("full").or_insert(0usize) += 1,
            Err(ThreadedRuntimeError::WorkerStopped) => {
                *counts.entry("worker_stopped").or_insert(0usize) += 1
            }
            Ok(CallOutcome::Replied(_)) => *counts.entry("replied").or_insert(0usize) += 1,
            Ok(CallOutcome::Timeout) => *counts.entry("timeout").or_insert(0usize) += 1,
            Ok(CallOutcome::Closed) => *counts.entry("closed").or_insert(0usize) += 1,
            Ok(CallOutcome::Rejected(_)) => *counts.entry("rejected").or_insert(0usize) += 1,
            Err(ThreadedRuntimeError::CommandFull) => {
                *counts.entry("command_full").or_insert(0usize) += 1
            }
            Err(ThreadedRuntimeError::HostWaitTimeout) => {
                *counts.entry("host_wait_timeout").or_insert(0usize) += 1
            }
            Err(_) => *counts.entry("other_error").or_insert(0usize) += 1,
        }
    }

    assert_eq!(
        counts.get("worker_stopped").copied().unwrap_or(0),
        0,
        "{counts:?}"
    );
    let runtime = match Arc::try_unwrap(runtime) {
        Ok(runtime) => runtime,
        Err(_) => panic!("no extra runtime refs"),
    };
    let _ = runtime.shutdown();
}

#[test]
fn shutdown_while_call_pending_unblocks_caller_promptly() {
    let runtime = Arc::new(runtime());
    let silent = runtime
        .register_with_capacity::<Silent, Infallible>(Silent, 4)
        .expect("register silent");
    let shutdown = runtime.shutdown_handle();

    let rt = Arc::clone(&runtime);
    let caller =
        thread::spawn(move || rt.call_blocking(silent, SilentMsg::Start, Duration::from_secs(30)));

    // Let the call reach the worker and park on its 100ms timer.
    thread::sleep(Duration::from_millis(20));
    let requested = Instant::now();
    let _ = shutdown.request_shutdown();
    let outcome = caller.join().expect("caller thread");
    let elapsed = requested.elapsed();

    assert!(
        elapsed < Duration::from_secs(2),
        "pending caller was not unblocked promptly after shutdown: {elapsed:?}"
    );
    // No hang: the caller always sees a visible result. The worker stopped out
    // from under the wait (WorkerStopped), or a terminal call outcome landed.
    assert!(
        matches!(
            outcome,
            Err(ThreadedRuntimeError::WorkerStopped)
                | Ok(CallOutcome::Timeout)
                | Ok(CallOutcome::Closed)
                | Ok(CallOutcome::Replied(_))
        ),
        "unexpected pending-shutdown outcome: {outcome:?}"
    );
    drop(runtime);
}
