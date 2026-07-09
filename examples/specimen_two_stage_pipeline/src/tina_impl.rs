use std::convert::Infallible;
use std::time::Duration;

use tina::prelude::*;
use tina_runtime::{CallOutcome, DefaultThreadedMailboxFactory, ThreadedRuntime, call};

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

enum PipelineMsg {
    Submit(usize),
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
tina::flow! {
    flow PipelineFlow for Pipeline {
        reply PipelineReply;

        step Parsed() -> ParseReply {
            match outcome {
                CallOutcome::Replied(ParseReply::Ok(v)) => {
                    call(self.validate, ValidateInput(v), STAGE_TIMEOUT)
                        .then_with_request(req, move |req, outcome| {
                            PipelineMsg::Stage(PipelineFlow::Validated(req, outcome))
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
                            PipelineMsg::Stage(PipelineFlow::Executed(req, outcome))
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

#[tina_runtime::isolate(message = PipelineMsg, reply = PipelineReply)]
impl Pipeline {
    fn handle(
        &mut self,
        msg: PipelineMsg,
        _ctx: &mut Context<'_, SingleShard, Self::Reply>,
    ) -> Effect<Self> {
        match msg {
            PipelineMsg::Submit(_) => noop(),
            PipelineMsg::Stage(flow) => self.handle_pipeline_flow(flow),
        }
    }

    fn handle_call(&mut self, msg: PipelineMsg, call_ctx: CallContext<'_, Self>) -> Effect<Self> {
        match msg {
            PipelineMsg::Submit(input) => call_ctx
                .defer(call(self.parse, ParseInput(input), STAGE_TIMEOUT))
                .reply(move |req, outcome| {
                    PipelineMsg::Stage(PipelineFlow::Parsed(req, outcome))
                }),
            PipelineMsg::Stage(_) => call_ctx.reject(tina::CallRejectedReason::UnsupportedMessage),
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
    pipeline: Address<PipelineMsg, PipelineReply>,
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
                        call(pipeline, PipelineMsg::Submit(i), STAGE_TIMEOUT)
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
        .register_with_capacity::<_, Infallible>(
            Pipeline {
                parse,
                validate,
                execute,
            },
            64,
        )
        .map_err(|e| anyhow::anyhow!("register pipeline: {e:?}"))?;
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
