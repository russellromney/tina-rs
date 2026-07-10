use std::convert::Infallible;
use std::time::Duration;

use tina::prelude::*;
use tina_runtime::{
    CallOutcome, DefaultThreadedMailboxFactory, ThreadedRuntime, call, call_request,
};

use crate::{REQUESTS, Report, Stage, classify};

const STAGE_TIMEOUT: Duration = Duration::from_secs(2);

// --- Parse stage ----------------------------------------------------

#[derive(Debug, Clone, Copy)]
struct ParseInput(usize);

#[derive(Debug, Clone, Copy)]
enum ParseReply {
    Ok(usize),
    Failed,
}

struct ParseStage;

#[tina::isolate(message = ParseInput, reply = ParseReply)]
impl ParseStage {
    fn handle(
        &mut self,
        msg: ParseInput,
        _ctx: &mut Context<'_, SingleShard, Self::Reply>,
    ) -> Effect<Self> {
        reply(self.apply(msg))
    }

    fn handle_call(&mut self, msg: ParseInput, call: CallContext<'_, Self>) -> Effect<Self> {
        call.reply(self.apply(msg))
    }
}

impl ParseStage {
    fn apply(&self, msg: ParseInput) -> ParseReply {
        match classify(msg.0) {
            Stage::ParseFailure => ParseReply::Failed,
            _ => ParseReply::Ok(msg.0),
        }
    }
}

// --- Validate stage -------------------------------------------------

#[derive(Debug, Clone, Copy)]
struct ValidateInput(usize);

#[derive(Debug, Clone, Copy)]
enum ValidateReply {
    Ok(usize),
    Failed,
}

struct ValidateStage;

#[tina::isolate(message = ValidateInput, reply = ValidateReply)]
impl ValidateStage {
    fn handle(
        &mut self,
        msg: ValidateInput,
        _ctx: &mut Context<'_, SingleShard, Self::Reply>,
    ) -> Effect<Self> {
        reply(self.apply(msg))
    }

    fn handle_call(&mut self, msg: ValidateInput, call: CallContext<'_, Self>) -> Effect<Self> {
        call.reply(self.apply(msg))
    }
}

impl ValidateStage {
    fn apply(&self, msg: ValidateInput) -> ValidateReply {
        match classify(msg.0) {
            Stage::ValidateFailure => ValidateReply::Failed,
            _ => ValidateReply::Ok(msg.0),
        }
    }
}

// --- Execute stage --------------------------------------------------

#[derive(Debug, Clone, Copy)]
struct ExecuteInput(usize);

#[derive(Debug, Clone, Copy)]
struct ExecuteReply;

struct ExecuteStage;

#[tina::isolate(message = ExecuteInput, reply = ExecuteReply)]
impl ExecuteStage {
    fn handle(
        &mut self,
        msg: ExecuteInput,
        _ctx: &mut Context<'_, SingleShard, Self::Reply>,
    ) -> Effect<Self> {
        reply(self.apply(msg))
    }

    fn handle_call(&mut self, msg: ExecuteInput, call: CallContext<'_, Self>) -> Effect<Self> {
        call.reply(self.apply(msg))
    }
}

impl ExecuteStage {
    fn apply(&self, msg: ExecuteInput) -> ExecuteReply {
        // Pretend to do work with the parsed value; bind it so the
        // payload stays a real lesson rather than a unit hole.
        let ExecuteInput(_v) = msg;
        ExecuteReply
    }
}

// --- Pipeline frontend ---------------------------------------------

#[derive(Debug, Clone, Copy)]
pub enum PipelineReply {
    Completed,
    ParseFailed,
    ValidateFailed,
    Failed,
}

/// Caller-authority request: the only thing an outside caller can ask.
#[derive(Debug)]
enum PipelineRequest {
    Submit(usize),
}

/// Internal event: one stage's continuation, never caller authority.
enum PipelineEvent {
    Stage(PipelineFlow),
}

struct Pipeline {
    parse: Address<ParseInput, ParseReply>,
    validate: Address<ValidateInput, ValidateReply>,
    execute: Address<ExecuteInput, ExecuteReply>,
}

// One step per pipeline stage. `tina::flow!` writes the continuation enum
// (`PipelineFlow`) and its dispatcher (`handle_pipeline_flow`) that this
// crate used to hand-roll as `PipelineMsg::{Parsed,Validated,Executed}` plus
// a `qid`-keyed `PendingReplies` table. The caller's `RequestContext` now
// threads through `req` directly, so the qid indirection is gone too.
// Continuations land as `ServiceMessage::Event(PipelineEvent::Stage(...))`
// so the split-service form can drop the hand-written reject arm.
tina::flow! {
    flow PipelineFlow for Pipeline {
        reply PipelineReply;

        step Parsed() -> ParseReply {
            match outcome {
                CallOutcome::Replied(ParseReply::Ok(v)) => {
                    call(self.validate, ValidateInput(v), STAGE_TIMEOUT)
                        .then_with_request(req, move |req, outcome| {
                            tina::ServiceMessage::Event(PipelineEvent::Stage(
                                PipelineFlow::Validated(req, outcome),
                            ))
                        })
                }
                CallOutcome::Replied(ParseReply::Failed) => {
                    reply_to(req, PipelineReply::ParseFailed)
                }
                _ => reply_to(req, PipelineReply::Failed),
            }
        }

        step Validated() -> ValidateReply {
            match outcome {
                CallOutcome::Replied(ValidateReply::Ok(v)) => {
                    call(self.execute, ExecuteInput(v), STAGE_TIMEOUT)
                        .then_with_request(req, move |req, outcome| {
                            tina::ServiceMessage::Event(PipelineEvent::Stage(
                                PipelineFlow::Executed(req, outcome),
                            ))
                        })
                }
                CallOutcome::Replied(ValidateReply::Failed) => {
                    reply_to(req, PipelineReply::ValidateFailed)
                }
                _ => reply_to(req, PipelineReply::Failed),
            }
        }

        step Executed() -> ExecuteReply {
            match outcome {
                CallOutcome::Replied(ExecuteReply) => reply_to(req, PipelineReply::Completed),
                _ => reply_to(req, PipelineReply::Failed),
            }
        }
    }
}

// `flow!` does not derive `Debug`; print the outcome, skip `req`.
impl std::fmt::Debug for PipelineFlow {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PipelineFlow::Parsed(_, outcome) => f.debug_tuple("Parsed").field(outcome).finish(),
            PipelineFlow::Validated(_, outcome) => {
                f.debug_tuple("Validated").field(outcome).finish()
            }
            PipelineFlow::Executed(_, outcome) => f.debug_tuple("Executed").field(outcome).finish(),
        }
    }
}

impl std::fmt::Debug for PipelineEvent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PipelineEvent::Stage(flow) => f.debug_tuple("Stage").field(flow).finish(),
        }
    }
}

#[tina_runtime::isolate(event = PipelineEvent, request = PipelineRequest, reply = PipelineReply)]
impl Pipeline {
    fn handle_event(
        &mut self,
        event: PipelineEvent,
        _ctx: &mut Context<'_, SingleShard, Self::Reply>,
    ) -> Effect<Self> {
        match event {
            PipelineEvent::Stage(flow) => self.handle_pipeline_flow(flow),
        }
    }

    fn handle_request(
        &mut self,
        request: PipelineRequest,
        req_call: RequestCall<'_, Self>,
    ) -> RequestEffect<Self> {
        match request {
            PipelineRequest::Submit(input) => req_call
                .defer(call(self.parse, ParseInput(input), STAGE_TIMEOUT))
                .reply(move |req, outcome| {
                    tina::ServiceMessage::Event(PipelineEvent::Stage(PipelineFlow::Parsed(
                        req, outcome,
                    )))
                }),
        }
    }
}

// --- Driver ---------------------------------------------------------

#[derive(Debug, Default, Clone, Copy)]
struct DriverOutcome {
    completed: usize,
    parse_failed: usize,
    validate_failed: usize,
    other: usize,
}

#[derive(Debug)]
enum DriverMsg {
    Begin,
    Returned(CallOutcome<PipelineReply>),
}

struct Driver {
    pipeline: tina::ServiceRequestAddress<PipelineEvent, PipelineRequest, PipelineReply>,
    remaining: usize,
    outcome: DriverOutcome,
}

#[tina_runtime::isolate(message = DriverMsg)]
impl Driver {
    fn handle(
        &mut self,
        msg: DriverMsg,
        _ctx: &mut Context<'_, SingleShard, Self::Reply>,
    ) -> Effect<Self> {
        match msg {
            DriverMsg::Begin => {
                let pipeline = self.pipeline;
                let calls: Vec<_> = (0..REQUESTS)
                    .map(|i| {
                        call_request(pipeline, PipelineRequest::Submit(i), STAGE_TIMEOUT)
                            .then(DriverMsg::Returned)
                    })
                    .collect();
                Effect::Batch(calls)
            }
            DriverMsg::Returned(outcome) => {
                match outcome {
                    CallOutcome::Replied(PipelineReply::Completed) => self.outcome.completed += 1,
                    CallOutcome::Replied(PipelineReply::ParseFailed) => {
                        self.outcome.parse_failed += 1
                    }
                    CallOutcome::Replied(PipelineReply::ValidateFailed) => {
                        self.outcome.validate_failed += 1
                    }
                    _ => self.outcome.other += 1,
                }
                self.remaining -= 1;
                if self.remaining == 0 {
                    stop_with(self.outcome)
                } else {
                    noop()
                }
            }
        }
    }
}

pub fn run() -> anyhow::Result<Report> {
    let runtime = ThreadedRuntime::new(SingleShard, DefaultThreadedMailboxFactory);

    let parse = runtime
        .register_with_capacity::<_, Infallible>(ParseStage, 32)
        .map_err(|e| anyhow::anyhow!("register parse: {e:?}"))?;
    let validate = runtime
        .register_with_capacity::<_, Infallible>(ValidateStage, 32)
        .map_err(|e| anyhow::anyhow!("register validate: {e:?}"))?;
    let execute = runtime
        .register_with_capacity::<_, Infallible>(ExecuteStage, 32)
        .map_err(|e| anyhow::anyhow!("register execute: {e:?}"))?;
    let pipeline = runtime
        .register_split_service::<Pipeline, PipelineEvent, PipelineRequest, Infallible>(
            Pipeline {
                parse,
                validate,
                execute,
            },
            64,
        )
        .map_err(|e| anyhow::anyhow!("register pipeline: {e:?}"))?
        .requests;
    let driver = runtime
        .register_with_capacity::<_, Infallible>(
            Driver {
                pipeline,
                remaining: REQUESTS,
                outcome: DriverOutcome::default(),
            },
            64,
        )
        .map_err(|e| anyhow::anyhow!("register driver: {e:?}"))?;

    let result = runtime
        .observe_result::<DriverOutcome, _, _>(driver)
        .map_err(|e| anyhow::anyhow!("observe_result: {e:?}"))?;
    runtime
        .try_send(driver, DriverMsg::Begin)
        .map_err(|e| anyhow::anyhow!("kick driver: {e:?}"))?;
    let outcome = result
        .wait(Duration::from_secs(10))
        .map_err(|e| anyhow::anyhow!("driver finishes: {e:?}"))?;

    let _ = runtime.shutdown();

    Ok(Report {
        requests: REQUESTS,
        completed: outcome.completed,
        parse_failed: outcome.parse_failed,
        validate_failed: outcome.validate_failed,
        exit_clean: true,
    })
}
