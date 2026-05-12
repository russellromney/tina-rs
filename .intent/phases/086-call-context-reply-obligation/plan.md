# 086 Call Context Reply Obligation

## Status

- Done: plan.
- Open: implement, migrate runtime/sim/tests/docs/specimens.
- Shape: one PR if sane, two PRs max.

## Goal

Kill the `CallReplyAbandoned` footgun.

Current bug:

```text
called handler returns no reply
called handler captures no caller
runtime warns
caller waits until timeout
```

That is wrong.

Tina rule:

```text
send message: no caller exists.
call message: caller authority exists.
caller authority must be replied, rejected, or carried forward.
```

No hidden async context. No timeout purgatory.

## Non-Goals

- No magic caller carry through `call(...).reply(...)`.
- No Go-style context bag.
- No app storage / DI in `Context`.
- No fake Rust linear-type claim.
- No workflow helper or pipeline sugar.

## Public Shape

Add `CallContext` to `tina`.

```rust
pub struct CallContext<'a, I: Isolate> {
    // runtime facts for this turn
    // one private reply authority for I::Reply
}
```

Methods:

```rust
impl<'a, I: Isolate> CallContext<'a, I> {
    pub fn reply(self, value: I::Reply) -> Effect<I>;
    pub fn reject(self, reason: CallRejectedReason) -> Effect<I>;
    pub fn into_request_context(self) -> RequestContext<I::Reply>;

    pub fn shard_id(&self) -> I::Shard;
    pub fn me(&self) -> Address<I::Message, I::Reply>;
    pub fn send_self(&self, msg: I::Message) -> Effect<I>;
}
```

Handler shape:

```rust
fn handle(
    &mut self,
    msg: Msg,
    ctx: &mut Context<'_, AppShard, Reply>,
) -> Effect<Self>;

fn handle_call(
    &mut self,
    msg: Msg,
    call: CallContext<'_, Self>,
) -> Effect<Self>;
```

Example:

```rust
match msg {
    StoreMsg::Get(k) => call.reply(StoreReply::Found(self.get(k))),

    StoreMsg::Put(req) => {
        let request = call.into_request_context();
        journal_append(self.journal, req.bytes)
            .reply_with_request(request, StoreMsg::Journaled)
    }

    _ => call.reject(CallRejectedReason::UnsupportedMessage),
}
```

Continuation stays explicit:

```rust
StoreMsg::Journaled(request, outcome) => {
    reply_to_request(request, StoreReply::Stored)
}
```

## Core Semantics

Storage:

```text
call admitted
pending_isolate_calls owns caller capacity
CallContext owns/borrows that authority for one handler turn
```

Outcomes:

- `call.reply(value)` completes the pending call.
- `call.reject(reason)` completes with `CallOutcome::Rejected(reason)`.
- `call.into_request_context()` promotes into the existing
  `RequestContext` / deferred-reply path.
- After promotion, existing deferred capacity rules apply.
- Unused authority completes with `ReplyAbandoned`.

Must not happen:

- no deferred slot just because `CallContext` exists;
- no double-counting;
- no hidden pending table;
- no bypass around deferred caps.

## Unused Authority

Rust allows dropping values, so the runtime must check.

Do not rely on `Drop` doing runtime work. Track consumed/not-consumed.
After handler returns and effect is classified:

```text
CallContext unused
=> caller gets CallOutcome::Rejected(ReplyAbandoned)
=> caller capacity reclaimed now
=> trace records rejected call
=> returned effect still runs
```

Why effect still runs:

```text
handler returned it.
Tina does not erase side effects.
caller truth is still immediate rejection.
effect truth is still visible.
```

Later continuation `reply(...)` has no caller and stays no-op/diagnostic.

`CallReplyAbandoned` should be removed or renamed. If kept, it must mean
terminal rejection, not "caller keeps waiting".

## Public Outcomes

Add:

```rust
pub enum CallOutcome<T> {
    Replied(T),
    Full,
    Closed,
    Timeout,
    Rejected(CallRejectedReason),
}

pub enum CallRejectedReason {
    ReplyAbandoned,
    HandlerPanicked,
    UnsupportedMessage,
}
```

`CallError` projection:

```rust
CallError::Rejected(CallRejectedReason)
```

No user-defined rejection yet. Domain rejection belongs in the service
reply type.

Never collapse these into `Timeout`, `Closed`, or string errors.

Trace must say:

```text
call replied? no
caller captured? no
caller rejected now
reason: ReplyAbandoned
```

## Macro Rules

- If user writes `handle_call`, call-shaped messages use it.
- Send-shaped messages still use `handle`.
- Callable isolates should implement `handle_call`.
- If `reply = ...` is non-unit and `handle_call` is missing, prefer a
  macro/compile error.
- If compile-time proof is not possible, runtime rejects with
  `UnsupportedMessage`, never timeout.
- If needed, add explicit send-only spelling:

```rust
#[tina_runtime::isolate(message = Msg, send_only, shard = AppShard)]
```

Exact attribute name can change. The rule cannot: users should not
accidentally publish callable addresses whose calls all reject.

Keep `Context` narrow:

```text
runtime facts.
one-shot capabilities.
not app state.
not dependency injection.
not key/value storage.
```

## Runtime Work

Audit every call-shaped entry:

- local call;
- cross-shard call;
- sim local / remote call;
- host `call_blocking`;
- bridge / observed call if applicable.

Live runtime must:

- dispatch caller-context envelopes to `handle_call`;
- construct exactly one `CallContext`;
- complete/reject/promote authority exactly once;
- reclaim capacity immediately on rejection;
- still execute returned effects after unused-authority rejection;
- keep late continuation `reply(...)` as no-op/diagnostic;
- reject panic-before-consume as `HandlerPanicked`;
- after `into_request_context()`, use existing deferred panic cleanup.

Cross-shard needs a concrete rejected envelope:

```rust
RemoteCallOutcome::Rejected(CallRejectedReason)
```

Use the actual enum name if different, but the fact must cross shards.

## Simulator Work

Mirror live semantics in `tina-sim`.

Must cover:

- reply;
- reject;
- carry as `RequestContext`;
- unused authority;
- panic before consume;
- panic after promote;
- remote rejected call.

Trace vocabulary must match live.

## Migration

Update tests first, then specimens/docs.

Expected edits:

- single-turn call service: `call.reply(value)`;
- multi-turn call service: `call.into_request_context()`;
- final continuation: `reply_to_request(request, value)`;
- send-only messages stay in `handle`;
- call request messages move to `handle_call`;
- old abandoned-timeout tests become immediate-rejection tests.

Do not hide caller authority to make examples shorter.

Docs must teach:

```text
send handler has no caller.
call handler has caller obligation.
reply / reject / carry.
```

Show expanded form first. Helper may follow only if it consumes
`CallContext` or a clearly named request authority:

```rust
call(worker, msg, timeout)
    .reply_with_current_request(call, ServiceMsg::WorkerReturned)
```

No `reply_with_context`.

## Compatibility

Tina is pre-public. Prefer clean break.

If migration is too wide, one temporary compat path is allowed, but:

- named as compat;
- documented temporary;
- not the user-guide shape;
- never waits until timeout.

## Required Proof

Live:

- `call.reply(...)` replies.
- `call.reject(...)` returns `CallOutcome::Rejected`.
- `into_request_context()` + later `reply_to_request(...)` replies.
- unused `CallContext` rejects immediately, no timeout wait.
- returned effect still runs after unused rejection.
- capacity is reclaimed after rejection.
- panic before consume rejects with `HandlerPanicked`.
- panic after promote does not leak deferred capacity.
- cross-shard rejected call preserves reason.

Sim:

- same cases as live;
- same trace vocabulary.

Compile/doc:

- `RequestContext` remains move-only.
- examples compile.
- old broken multi-turn shape is gone.
- docs say terminal rejection, not warning-only.

## Hostile Review Checklist

- Does any call path still wait for timeout after unused authority?
- Does any helper hide caller authority across turns?
- Can app code store arbitrary values in `Context`?
- Are live and sim identical?
- Are rejection reasons typed and visible?
- Is capacity reclaimed immediately?
- Is callable-without-`handle_call` loud enough?
