use std::convert::Infallible;
use std::time::Duration;

use tina::prelude::*;
use tina_runtime::{
    BoundedItems, CallOutcome, DefaultThreadedMailboxFactory, LocalSystem, bounded_batch, call,
    call_request,
};

use crate::{PipelineStage, PipelineTerminal, REQUESTS, Report, Stage, classify};

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
    Terminal(PipelineTerminal),
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
// Continuations land as domain events via `then_service_event*` helpers
// so the split-service form never names the envelope.
tina::flow! {
    flow PipelineFlow for Pipeline {
        reply PipelineReply;

        step Parsed() -> ParseReply {
            match outcome {
                CallOutcome::Replied(ParseReply::Ok(v)) => {
                    call(self.validate, ValidateInput(v), STAGE_TIMEOUT)
                        .then_service_event_with_request(req, move |req, outcome| {
                            PipelineEvent::Stage(PipelineFlow::Validated(req, outcome))
                        })
                }
                CallOutcome::Replied(ParseReply::Failed) => {
                    reply_to(req, PipelineReply::ParseFailed)
                }
                CallOutcome::Full => reply_to(
                    req,
                    PipelineReply::Terminal(PipelineTerminal::StageFull(PipelineStage::Parse)),
                ),
                CallOutcome::Closed => reply_to(
                    req,
                    PipelineReply::Terminal(PipelineTerminal::StageClosed(PipelineStage::Parse)),
                ),
                CallOutcome::Timeout => reply_to(
                    req,
                    PipelineReply::Terminal(PipelineTerminal::StageTimeout(PipelineStage::Parse)),
                ),
                CallOutcome::Rejected(reason) => reply_to(
                    req,
                    PipelineReply::Terminal(PipelineTerminal::StageRejected {
                        stage: PipelineStage::Parse,
                        reason,
                    }),
                ),
            }
        }

        step Validated() -> ValidateReply {
            match outcome {
                CallOutcome::Replied(ValidateReply::Ok(v)) => {
                    call(self.execute, ExecuteInput(v), STAGE_TIMEOUT)
                        .then_service_event_with_request(req, move |req, outcome| {
                            PipelineEvent::Stage(PipelineFlow::Executed(req, outcome))
                        })
                }
                CallOutcome::Replied(ValidateReply::Failed) => {
                    reply_to(req, PipelineReply::ValidateFailed)
                }
                CallOutcome::Full => reply_to(
                    req,
                    PipelineReply::Terminal(PipelineTerminal::StageFull(PipelineStage::Validate)),
                ),
                CallOutcome::Closed => reply_to(
                    req,
                    PipelineReply::Terminal(PipelineTerminal::StageClosed(PipelineStage::Validate)),
                ),
                CallOutcome::Timeout => reply_to(
                    req,
                    PipelineReply::Terminal(PipelineTerminal::StageTimeout(PipelineStage::Validate)),
                ),
                CallOutcome::Rejected(reason) => reply_to(
                    req,
                    PipelineReply::Terminal(PipelineTerminal::StageRejected {
                        stage: PipelineStage::Validate,
                        reason,
                    }),
                ),
            }
        }

        step Executed() -> ExecuteReply {
            match outcome {
                CallOutcome::Replied(ExecuteReply) => reply_to(req, PipelineReply::Completed),
                CallOutcome::Full => reply_to(
                    req,
                    PipelineReply::Terminal(PipelineTerminal::StageFull(PipelineStage::Execute)),
                ),
                CallOutcome::Closed => reply_to(
                    req,
                    PipelineReply::Terminal(PipelineTerminal::StageClosed(PipelineStage::Execute)),
                ),
                CallOutcome::Timeout => reply_to(
                    req,
                    PipelineReply::Terminal(PipelineTerminal::StageTimeout(PipelineStage::Execute)),
                ),
                CallOutcome::Rejected(reason) => reply_to(
                    req,
                    PipelineReply::Terminal(PipelineTerminal::StageRejected {
                        stage: PipelineStage::Execute,
                        reason,
                    }),
                ),
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
                .reply_service_event(move |req, outcome| {
                    PipelineEvent::Stage(PipelineFlow::Parsed(req, outcome))
                }),
        }
    }
}

// --- Driver ---------------------------------------------------------

#[derive(Debug, Default, Clone)]
struct DriverOutcome {
    completed: usize,
    parse_failed: usize,
    validate_failed: usize,
    terminals: Vec<PipelineTerminal>,
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
                let inputs = BoundedItems::try_from_iter(REQUESTS, 0..REQUESTS)
                    .expect("REQUESTS is the driver-owned producer bound");
                bounded_batch(inputs.map_effects(|i| {
                    call_request(pipeline, PipelineRequest::Submit(i), STAGE_TIMEOUT)
                        .then(DriverMsg::Returned)
                }))
            }
            DriverMsg::Returned(outcome) => {
                record_driver_outcome(&mut self.outcome, outcome);
                self.remaining -= 1;
                if self.remaining == 0 {
                    stop_with(std::mem::take(&mut self.outcome))
                } else {
                    noop()
                }
            }
        }
    }
}

fn record_driver_outcome(outcome: &mut DriverOutcome, returned: CallOutcome<PipelineReply>) {
    match returned {
        CallOutcome::Replied(PipelineReply::Completed) => outcome.completed += 1,
        CallOutcome::Replied(PipelineReply::ParseFailed) => outcome.parse_failed += 1,
        CallOutcome::Replied(PipelineReply::ValidateFailed) => outcome.validate_failed += 1,
        CallOutcome::Replied(PipelineReply::Terminal(terminal)) => outcome.terminals.push(terminal),
        CallOutcome::Full => outcome.terminals.push(PipelineTerminal::OuterFull),
        CallOutcome::Closed => outcome.terminals.push(PipelineTerminal::OuterClosed),
        CallOutcome::Timeout => outcome.terminals.push(PipelineTerminal::OuterTimeout),
        CallOutcome::Rejected(reason) => outcome
            .terminals
            .push(PipelineTerminal::OuterRejected(reason)),
    }
}

pub fn run() -> anyhow::Result<Report> {
    let app = LocalSystem::single_shard(SingleShard, DefaultThreadedMailboxFactory).try_build()?;
    Ok(app.run_to_shutdown_reported(Duration::from_secs(5), run_application)?)
}

fn run_application(
    app: &LocalSystem<SingleShard, DefaultThreadedMailboxFactory>,
) -> anyhow::Result<Report> {
    let parse = app
        .register_root::<_, Infallible>(ParseStage, 32)
        .map_err(|e| anyhow::anyhow!("register parse: {e:?}"))?;
    let validate = app
        .register_root::<_, Infallible>(ValidateStage, 32)
        .map_err(|e| anyhow::anyhow!("register validate: {e:?}"))?;
    let execute = app
        .register_root::<_, Infallible>(ExecuteStage, 32)
        .map_err(|e| anyhow::anyhow!("register execute: {e:?}"))?;
    let pipeline = app
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
    let driver = app
        .register_root::<_, Infallible>(
            Driver {
                pipeline,
                remaining: REQUESTS,
                outcome: DriverOutcome::default(),
            },
            64,
        )
        .map_err(|e| anyhow::anyhow!("register driver: {e:?}"))?;

    let result = app
        .observe_result::<DriverOutcome, _, _>(driver)
        .map_err(|e| anyhow::anyhow!("observe_result: {e:?}"))?;
    app.try_send(driver, DriverMsg::Begin)
        .map_err(|e| anyhow::anyhow!("kick driver: {e:?}"))?;
    let outcome = result
        .wait(Duration::from_secs(10))
        .map_err(|e| anyhow::anyhow!("driver finishes: {e:?}"))?;

    Ok(Report {
        requests: REQUESTS,
        completed: outcome.completed,
        parse_failed: outcome.parse_failed,
        validate_failed: outcome.validate_failed,
        exit_clean: true,
        tina_terminals: outcome.terminals,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use tina::CallRejectedReason;

    #[test]
    fn driver_preserves_domain_and_every_outer_terminal() {
        let reason = CallRejectedReason::UnsupportedMessage;
        let mut outcome = DriverOutcome::default();
        for returned in [
            CallOutcome::Replied(PipelineReply::Completed),
            CallOutcome::Replied(PipelineReply::ParseFailed),
            CallOutcome::Replied(PipelineReply::ValidateFailed),
            CallOutcome::Replied(PipelineReply::Terminal(PipelineTerminal::StageTimeout(
                PipelineStage::Validate,
            ))),
            CallOutcome::Full,
            CallOutcome::Closed,
            CallOutcome::Timeout,
            CallOutcome::Rejected(reason),
        ] {
            record_driver_outcome(&mut outcome, returned);
        }
        assert_eq!(outcome.completed, 1);
        assert_eq!(outcome.parse_failed, 1);
        assert_eq!(outcome.validate_failed, 1);
        assert_eq!(
            outcome.terminals,
            [
                PipelineTerminal::StageTimeout(PipelineStage::Validate),
                PipelineTerminal::OuterFull,
                PipelineTerminal::OuterClosed,
                PipelineTerminal::OuterTimeout,
                PipelineTerminal::OuterRejected(reason),
            ]
        );
    }
}
