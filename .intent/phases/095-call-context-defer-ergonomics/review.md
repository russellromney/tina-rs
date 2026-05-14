# 095 Plan Review

## Hostile Review 1

### Finding 1 — Caller-rooted API may force a trait into `tina`

`CallContext::defer(work)` is the best user spelling, but it requires `tina` to
know a trait implemented by runtime crates. That is possible, but it may create
associated-type error messages that are worse than the ceremony it replaces.
The plan now includes an explicit fallback: `work.defer_reply(call_ctx).reply`
if the caller-rooted trait shape becomes trait soup. It also requires the
implementation to document why fallback was chosen instead of silently shipping
the weaker shape.

### Finding 2 — Renaming `.reply` could become a churn PR

The right vocabulary is `then` for ordinary continuations and `reply` for
caller replies. But deprecating every existing `work.reply(...)` in one phase
could turn a small ergonomic fix into a workspace-wide rename. The plan now
requires `then` / `then_with_request` aliases but treats deprecating old
`reply` names as optional and conditional on churn. Documentation and selected
specimens must move first.

### Finding 3 — The deferred builder could hide the final reply

`call_ctx.defer(work).reply(Msg::Done)` sounds like it replies to the caller,
but it only builds a continuation carrying `RequestContext`. The final
`reply_to_request` must still happen in a later handler turn. The plan now
spells this out in the goal, docs, tests, and review checklist.

### Finding 4 — Testing only happy paths would miss the original footgun

The dangerous behavior is not "helper compiles"; it is "ordinary continuation
inside `handle_call` abandons caller authority while still running later
effects." The plan now requires a direct test of that misuse boundary, plus
child full/closed/timeout paths, observed-send full, typed runtime call errors,
and trace assertions for captured versus ordinary continuations.

### Finding 5 — Specimen migration can overfit to toy examples

`specimen_multi_turn_request_context` proves teaching shape, but it is too
small to prove production ergonomics. The plan now requires a real-ish
`mini_saas_api` migration where the helper removes ceremony without hiding
authority, and keeps broad example churn explicitly out of scope.

### Finding 6 — `CancelCallBuilder` is a trap for first form

Cancel completions do not carry a service reply context in the same way as
ordinary child work; forcing them into this first form could confuse the
request/reply story. The plan now names cancel support as intentional-only,
rather than required, so implementation can defer it unless a specimen proves
the shape.

## Decision

Proceed with 095 as a plan-first phase. The implementation should prefer
`call_ctx.defer(work).reply(...)`, but must choose readable compiler errors over
perfect method chaining if Rust's trait shape fights back.
