use std::convert::Infallible;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tina::{Outbound, noop, reply_to_request};
use tina_runtime::{
    CallOutcome, DefaultThreadedMailboxFactory, RuntimeCall, ThreadedRuntime,
    ThreadedRuntimeConfig, call,
};

#[derive(Debug)]
enum WorkerMsg {
    AddOne(u32),
    Hold,
}

struct Worker {
    held: Option<tina::RequestContext<u32>>,
}

#[tina_runtime::isolate(message = WorkerMsg, reply = u32)]
impl Worker {
    fn handle(
        &mut self,
        msg: WorkerMsg,
        _ctx: &mut tina::Context<'_, tina::SingleShard, Self::Reply>,
    ) -> tina::Effect<Self> {
        match msg {
            WorkerMsg::AddOne(_) | WorkerMsg::Hold => noop(),
        }
    }

    fn handle_call(
        &mut self,
        msg: WorkerMsg,
        call: tina::CallContext<'_, Self>,
    ) -> tina::Effect<Self> {
        match msg {
            WorkerMsg::AddOne(value) => call.reply(value + 1),
            WorkerMsg::Hold => {
                self.held = Some(call.into_request_context());
                noop()
            }
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ErrorMode {
    Timeout,
    Full,
}

enum ErrorDriverMsg {
    Start(ErrorMode),
    Flow(ErrorFlow),
}

struct ErrorDriver {
    starter: tina::Address<WorkerMsg, u32>,
    timeout_target: tina::Address<WorkerMsg, u32>,
    full_target: tina::Address<WorkerMsg, u32>,
    errors: Arc<Mutex<Vec<ErrorMode>>>,
}

tina::flow! {
    flow ErrorFlow for ErrorDriver {
        reply u32;

        step First(mode: ErrorMode) -> u32 {
            match outcome {
                CallOutcome::Replied(_) => {
                    let target = match mode {
                        ErrorMode::Timeout => self.timeout_target,
                        ErrorMode::Full => self.full_target,
                    };
                    let msg = match mode {
                        ErrorMode::Timeout => WorkerMsg::Hold,
                        ErrorMode::Full => WorkerMsg::AddOne(0),
                    };
                    call(target, msg, Duration::from_millis(20))
                        .then_with_request(req, move |req, outcome| {
                            ErrorDriverMsg::Flow(ErrorFlow::Second(req, mode, outcome))
                        })
                }
                CallOutcome::Full
                | CallOutcome::Closed
                | CallOutcome::Timeout
                | CallOutcome::Rejected(_) => reply_to_request(req, 0),
            }
        }

        step Second(mode: ErrorMode) -> u32 {
            match outcome {
                CallOutcome::Timeout => {
                    self.errors.lock().expect("errors").push(mode);
                    reply_to_request(req, 10)
                }
                CallOutcome::Full => {
                    self.errors.lock().expect("errors").push(mode);
                    reply_to_request(req, 20)
                }
                CallOutcome::Replied(_)
                | CallOutcome::Closed
                | CallOutcome::Rejected(_) => reply_to_request(req, 0),
            }
        }
    }
}

#[tina_runtime::isolate(
    message = ErrorDriverMsg,
    reply = u32,
    send = Outbound<Infallible>,
    call = RuntimeCall<ErrorDriverMsg>
)]
impl ErrorDriver {
    fn handle(
        &mut self,
        msg: ErrorDriverMsg,
        _ctx: &mut tina::Context<'_, tina::SingleShard, Self::Reply>,
    ) -> tina::Effect<Self> {
        match msg {
            ErrorDriverMsg::Start(_) => noop(),
            ErrorDriverMsg::Flow(flow) => self.handle_error_flow(flow),
        }
    }

    fn handle_call(
        &mut self,
        msg: ErrorDriverMsg,
        call_ctx: tina::CallContext<'_, Self>,
    ) -> tina::Effect<Self> {
        match msg {
            ErrorDriverMsg::Start(mode) => call_ctx
                .defer(call(
                    self.starter,
                    WorkerMsg::AddOne(1),
                    Duration::from_secs(1),
                ))
                .reply(move |req, outcome| {
                    ErrorDriverMsg::Flow(ErrorFlow::First(req, mode, outcome))
                }),
            ErrorDriverMsg::Flow(_) => {
                call_ctx.reject(tina::CallRejectedReason::UnsupportedMessage)
            }
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
        .register_with_capacity::<Worker, Infallible>(Worker { held: None }, 8)
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

#[test]
fn generated_multi_step_flow_maps_timeout_and_full_outcomes() {
    let runtime = runtime();
    let starter = runtime
        .register_with_capacity::<Worker, Infallible>(Worker { held: None }, 8)
        .expect("register starter");
    let timeout_target = runtime
        .register_with_capacity::<Worker, Infallible>(Worker { held: None }, 8)
        .expect("register timeout target");
    let full_target = runtime
        .register_with_capacity::<Worker, Infallible>(Worker { held: None }, 0)
        .expect("register full target");
    let errors = Arc::new(Mutex::new(Vec::new()));
    let driver = runtime
        .register_with_capacity::<ErrorDriver, Infallible>(
            ErrorDriver {
                starter,
                timeout_target,
                full_target,
                errors: Arc::clone(&errors),
            },
            8,
        )
        .expect("register error driver");

    let timeout = runtime
        .call_blocking(
            driver,
            ErrorDriverMsg::Start(ErrorMode::Timeout),
            Duration::from_secs(1),
        )
        .expect("timeout host call");
    let full = runtime
        .call_blocking(
            driver,
            ErrorDriverMsg::Start(ErrorMode::Full),
            Duration::from_secs(1),
        )
        .expect("full host call");

    assert_eq!(timeout, CallOutcome::Replied(10));
    assert_eq!(full, CallOutcome::Replied(20));
    assert_eq!(
        errors.lock().expect("errors").as_slice(),
        [ErrorMode::Timeout, ErrorMode::Full]
    );
    runtime.shutdown().expect("shutdown");
}
