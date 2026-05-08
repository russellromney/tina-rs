# Rock 9 Design Note — Pipeline Ergonomics

## Status

Design only. **No helper.** The raw match-state-machine form
is the blessed shape.

## The Pain

`eiffel_two_stage_pipeline` (three stages: parse, validate,
execute) reads as four `PipelineMsg` variants:

```rust
enum PipelineMsg {
    Submit(usize),
    Parsed(u64, CallOutcome<ParseReply>),
    Validated(u64, CallOutcome<ValidateReply>),
    Executed(u64, CallOutcome<ExecuteReply>),
}
```

One variant per stage transition. Variant count grows linearly
with stage count. Tokio reads as
`parse(i).await?; validate(p).await?; execute(v).await?` —
three lines.

## Why No Helper

A pipeline helper that shrinks variant count must hide one of:

- **Named stages.** A combined `StageDone(StageId, ...)` arm
  pays down variant count but loses the typed `ParseReply` vs
  `ValidateReply` vs `ExecuteReply` shape. Reader can no
  longer tell which stage's `Full` they are looking at.
- **Suspension points.** Each `call(...).reply(...)` is a
  trace-visible suspension. Chaining stages inside one effect
  erases those events.
- **Per-stage `Full` / `Closed` / `Timeout`.** A "submit, await
  result" facade hides which stage saturated.
- **Partial progress.** When stage 2 of 3 fails, the typed
  state in the example says exactly that. A helper that
  returns a single `PipelineResult` either reports the failed
  stage as data (no shorter than today) or hides it.

Plan rule: "raw match-state-machine form stays the semantic
truth". Every shape that delivers materially fewer LOC fails
one of the four checks.

## What Would Be Honest But Marginal

A helper that owns *only*:

- the per-call-site reply continuation closure factory;
- the per-stage timeout constant;
- a `next_stage_call(...)` that wraps
  `call(addr, msg, timeout).reply(continuation)`.

Shaves a few characters per stage without hiding anything.
Adds reader load: a reader has to look up the helper. Win is
small enough that the explicit form remains preferable.

## Decision

No helper. No example migration. Finding 11 stays open with
this note attached: the raw form is the answer.

`eiffel_two_stage_pipeline` keeps its four-variant
`PipelineMsg` and per-stage match. README documents the rule.
