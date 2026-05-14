# 095 Call Context Defer Ergonomics

## Status

- Ready to review.
- This replaces the smaller `reply_with_current_request(call_ctx, ...)`
  helper idea. Do not ship that helper as the main user path unless this plan
  fails in implementation.
- Run after 086 and the current production-service specimen work are merged.
- One PR if the trait shape stays small; split if ordinary continuation
  vocabulary migration grows beyond docs and a few specimens.
- Hostile review pass incorporated; see `review.md`.

## Grug Truth

Call handler has caller.

Caller authority is precious.

Most call handlers either answer now or answer after visible work.

Default path should start from caller authority.

Ordinary continuation is not caller reply.

Words must not lie.

No hidden async stack.

No hidden caller carry.

## Goal

Make the obvious multi-turn request/reply code also be the correct code:

```rust
fn handle_call(&mut self, msg: ServiceMsg, call_ctx: CallContext<'_, Self>) -> Effect<Self> {
    match msg {
        ServiceMsg::Start(job) => call_ctx
            .defer(call(self.worker, WorkerMsg::Run(job), timeout))
            .reply(ServiceMsg::WorkerDone),

        ServiceMsg::WorkerDone(_, _) => {
            call_ctx.reject(CallRejectedReason::UnsupportedMessage)
        }
    }
}
```

The continuation message still carries the request context:

```rust
enum ServiceMsg {
    Start(Job),
    WorkerDone(RequestContext<ServiceReply>, CallOutcome<WorkerReply>),
}
```

The final turn stays explicit:

```rust
ServiceMsg::WorkerDone(req, CallOutcome::Replied(reply)) => {
    reply_to_request(req, ServiceReply::Done(reply))
}
ServiceMsg::WorkerDone(req, _) => {
    reply_to_request(req, ServiceReply::Failed)
}
```

This phase should reduce ceremony without making caller authority ambient.

## Non-Goals

- no generic `Context` access to caller authority;
- no hidden caller context on ordinary continuations;
- no fake `async fn` service handlers;
- no automatic final reply after the child call returns;
- no background task or side table;
- no macro-first design;
- no broad flow DSL;
- no domain rejection vocabulary;
- no removal of the explicit `into_request_context()` form;
- no silent compatibility break for existing user code unless explicitly
  chosen after a deprecation pass.

## Vocabulary Decision

Reserve `reply` for caller-facing reply authority.

Blessed request paths:

```rust
call_ctx.reply(value)            // answer caller now
call_ctx.reject(reason)          // reject caller now
call_ctx.defer(work).reply(msg)  // answer caller after visible work
```

Ordinary continuation path:

```rust
work.then(msg)                   // later message, no caller authority
work.then_with_request(req, msg) // expanded request-context form
```

Compatibility:

- keep existing `work.reply(msg)` and `work.reply_with_request(req, msg)` in
  first form unless removing them is proven small;
- add `then` / `then_with_request` aliases and move docs/specimens toward them;
- consider deprecating ordinary `work.reply(...)` only if the workspace can be
  migrated without drowning the real change in churn;
- never document `work.reply(...)` inside `handle_call` as the blessed shape.

## Rock 0: Read First

Read:

- `.intent/phases/086-call-context-reply-obligation/plan.md`;
- docs request/reply sections:
  - `docs/tina-user-guide/03-effects-and-runtime-calls.md`;
  - `docs/tina-user-guide/04-request-reply.md`;
  - `docs/tina-user-guide/10-service-patterns.md`;
  - `docs/tina-user-guide/16-continuation-and-pipeline-patterns.md`;
- `tina/src/lib.rs` `CallContext`, `RequestContext`, and `Effect`;
- `tina-runtime/src/call.rs` call builders;
- `tina-runtime/src/tests/request_context.rs`;
- specimens:
  - `examples/specimen_multi_turn_request_context`;
  - `examples/specimen_cancellation_chain`;
  - `examples/systems/mini_saas_api`;
  - one ordinary runtime-call-heavy specimen as a warning for `then` churn.

Before coding, add a status note here naming:

- chosen trait home;
- whether ordinary `.reply(...)` is deprecated now or later;
- specimens selected for migration.

## Rock 1: Public API Shape

Preferred first form:

```rust
impl<'a, I> CallContext<'a, I>
where
    I: Isolate,
{
    pub fn defer<W>(self, work: W) -> W::Deferred
    where
        W: DeferThrough<I>;
}

pub trait DeferThrough<I: Isolate> {
    type Deferred;
    fn defer_through(self, call: CallContext<'_, I>) -> Self::Deferred;
}
```

The trait can live in `tina` so `CallContext::defer(...)` is available without
`tina` depending on `tina-runtime`. Runtime crates implement it for their own
builder types.

The implementation must check compiler errors from a wrong continuation shape.
If users see associated-type soup instead of "your continuation must accept
`RequestContext<Reply>` and the work outcome", prefer the fallback spelling or
add targeted diagnostics before shipping.

If this trait shape gets ugly because of lifetimes, associated types, or
inference, stop and choose the simpler runtime-side spelling:

```rust
call(self.worker, WorkerMsg::Run(job), timeout)
    .defer_reply(call_ctx)
    .reply(ServiceMsg::WorkerDone)
```

Do not fall back to `reply_with_current_request(...)` without documenting why
the caller-rooted shape failed.

## Rock 2: Deferred Builder Shape

Add one small deferred builder per existing work builder, or one shared generic
wrapper if it is genuinely simpler.

Required builder families:

- isolate call: `call(addr, msg, timeout)`;
- isolate call with handle: `call_with_handle(addr, msg, timeout)`;
- observed send: `send_observed(addr, msg)`;
- typed runtime call: `sleep`, TCP/TLS/file/process/DNS/etc. typed calls.

`cancel_call(handle)` is optional in first form. Include it only if a real
specimen shows that deferring caller reply through a cancel acknowledgement is
clearer than the explicit form.

Required methods:

```rust
call_ctx.defer(work).reply(Msg::Done)
```

Where `Msg::Done` receives:

- `RequestContext<I::Reply>`;
- the work outcome (`CallOutcome<R>`, `SendOutcome`, `Result<T, CallError>`,
  or `CancelOutcome` only if cancel-call support is intentionally included).

For `call_with_handle`, preserve the existing handle shape:

```rust
let (effect, handle) = call_ctx
    .defer(call_with_handle(worker, WorkerMsg::Run, timeout))
    .reply(ServiceMsg::WorkerDone);
```

Do not allocate a second request context. The builder must be sugar for:

```rust
let req = call_ctx.into_request_context();
work.then_with_request(req, Msg::Done)
```

## Rock 3: Ordinary Continuation Vocabulary

Add ordinary continuation aliases:

```rust
work.then(Msg::Done)
work.then_with_request(req, Msg::Done)
```

Rules:

- `then` is exactly today's ordinary `reply` continuation behavior;
- `then_with_request` is exactly today's `reply_with_request`;
- no hidden caller authority;
- no automatic final reply;
- docs should call `then` "ordinary continuation";
- docs should call `reply` "caller reply" or "deferred caller reply".

Migration rule:

- update docs that teach concepts to prefer `then`;
- update specimens only where it clarifies the request/reply distinction;
- leave broad mechanical `.reply` sweeps for a later cleanup unless the phase
  chooses deprecation.

## Rock 4: Docs

Update the request/reply guide so the teaching ladder is:

1. caller asks service;
2. service replies now with `call_ctx.reply(value)`;
3. service replies later with `call_ctx.defer(work).reply(Msg::Done)`;
4. expanded form:

```rust
let req = call_ctx.into_request_context();
work.then_with_request(req, Msg::Done)
```

Keep the expanded form once, prominently enough that readers know where caller
authority lives.

Add a warning box:

```text
work.then(...) is an ordinary continuation. It never replies to the caller by
itself. If used in handle_call without consuming CallContext, the caller gets
ReplyAbandoned.
```

Remove or rewrite any docs that say ordinary runtime-call continuations
preserve caller context.

## Rock 5: Tests

Add focused runtime tests from a user's point of view:

- `call_ctx.defer(call(...)).reply(...)` replies to the original caller;
- child call `Full`/`Closed`/`Timeout` still reaches the continuation with
  request context, and the service can reply to the original caller;
- unsupported call message rejects immediately, not after timeout;
- plain `work.then(...)` inside `handle_call` still produces
  `ReplyAbandoned` if `CallContext` is not consumed, while the ordinary
  continuation still runs;
- `call_with_handle` returns the handle and preserves request context;
- `send_observed` accepted and full paths preserve request context;
- typed runtime-call success and typed `CallError` paths preserve request
  context;
- no `DeferredReplyCaptured` occurs for ordinary `then`;
- exactly one `DeferredReplyCaptured` and one terminal deferred reply/rejection
  occur for each deferred request path.

Add compile-fail or doc tests where practical:

- `Context` has no `defer` method;
- continuation message with missing `RequestContext` does not type-check for
  `call_ctx.defer(work).reply(...)`;
- wrong continuation outcome type produces a readable error or is covered by a
  documented fallback shape;
- double reply through one `RequestContext` remains impossible.

Run at least:

- `cargo test -p tina-runtime request_context --lib`;
- `cargo clippy -p tina-runtime --lib --tests -- -D warnings`;
- selected specimen tests;
- docs tests if the new snippets are doctested.

## Rock 6: Specimens

Migrate one small and one real-ish specimen:

- `specimen_multi_turn_request_context` for the compact teaching path;
- `examples/systems/mini_saas_api` where the helper removes ceremony without
  obscuring authority.

Optional:

- `specimen_cancellation_chain` if `call_with_handle` or typed sleep
  coverage benefits from a real cancellation example.

Do not migrate every `.reply(...)` in examples. The goal is vocabulary proof,
not a churn trophy.

## Rock 7: Review Checkpoints

Before implementation is called done, answer:

- Is the default multi-turn call-handler path rooted at `CallContext`?
- Is ordinary continuation vocabulary clearly not caller reply vocabulary?
- Does every helper still move `RequestContext` through the message enum?
- Is the expanded `into_request_context()` form still documented?
- Is `ReplyAbandoned` still present as runtime truth, but no longer the easy
  mistake in docs/specimens?
- Did any helper create hidden state, hidden retries, hidden cancellation, or
  hidden final replies?
- Are tests proving user outcomes, not just helper type-checking?
- Are compiler errors acceptable for the blessed path when the continuation
  constructor has the wrong shape?

## Rollback Plan

If `CallContext::defer(work)` cannot be made ergonomic without trait soup:

1. keep `then` / `then_with_request` vocabulary;
2. ship runtime-side `work.defer_reply(call_ctx).reply(...)`;
3. document why the caller-rooted shape failed;
4. do not ship `reply_with_current_request(...)` as the teaching path.

If ordinary `.reply(...)` deprecation causes too much churn:

1. leave it as compatibility alias;
2. stop teaching it for ordinary continuations;
3. add a later cleanup phase for deprecation/migration.
