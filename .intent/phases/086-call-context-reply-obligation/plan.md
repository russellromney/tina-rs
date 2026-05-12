# 086 Call Context Reply Obligation

## Status

- Done: plan.
- Open: implement typed call entry, migrate runtime/sim/tests/docs/specimens.
- Target: one PR if small enough, two PRs max.

## Goal

Fix the real bug behind `CallReplyAbandoned`.

Today a call-shaped message can return without replying and without
capturing the caller. Tina emits a warning and the caller waits until
timeout. That is a bandaid.

The Tina rule should be simpler:

```text
send message: no reply authority exists.
call message: reply authority exists.
reply authority must be consumed.
```

A called handler must reply, reject, or carry the request forward.

No hidden async context. No timeout purgatory.

## Non-Goals

- No magic caller context through `call(...).reply(...)`.
- No Go-style context bag.
- No app storage or dependency injection in `Context`.
- No fake compile-time linear type claim.
- No broad workflow helper.
- No pipeline sugar.

## Core Design

Add a typed call-entry context:

```rust
pub struct CallContext<'a, I: Isolate> {
    // runtime facts, same turn as Context
    // one private call reply authority for I::Reply
}
```

It exposes only runtime facts and one-shot reply authority:

```rust
impl<'a, I: Isolate> CallContext<'a, I> {
    pub fn reply(self, value: I::Reply) -> Effect<I>;
    pub fn reject(self, reason: CallRejectedReason) -> Effect<I>;
    pub fn into_request_context(self) -> RequestContext<I::Reply>;

    pub fn shard_id(&self) -> I::Shard;
    pub fn me(&self) -> Address<I::Message, I::Reply>;
    pub fn send_self(&self, msg: I::Message) -> Effect<I>;
    // only boring runtime-fact helpers copied from Context
}
```

The handler shape becomes visibly split:

```rust
impl Store {
    fn handle(
        &mut self,
        msg: StoreMsg,
        ctx: &mut Context<'_, AppShard, StoreReply>,
    ) -> Effect<Self> {
        // send-shaped messages only
    }

    fn handle_call(
        &mut self,
        msg: StoreMsg,
        call: CallContext<'_, Store>,
    ) -> Effect<Self> {
        match msg {
            StoreMsg::Get(k) => call.reply(StoreReply::Found(self.get(k))),

            StoreMsg::Put(req) => {
                let request = call.into_request_context();
                journal_append(self.journal, req.bytes)
                    .reply_with_request(request, StoreMsg::Journaled)
            }

            _ => call.reject(CallRejectedReason::UnsupportedMessage),
        }
    }
}
```

Final continuation stays explicit:

```rust
StoreMsg::Journaled(request, outcome) => {
    reply_to_request(request, StoreReply::Stored)
}
```

## Authority Storage

`CallContext` must not allocate a deferred slot just by existing.

First form:

```text
call is admitted
pending_isolate_calls owns caller capacity
CallContext borrows/moves the pending call authority for this handler turn
```

Then:

- `call.reply(value)` completes the pending call;
- `call.reject(reason)` completes the pending call with
  `CallOutcome::Rejected(reason)`;
- `call.into_request_context()` promotes the authority into the existing
  `RequestContext` / deferred-reply slot path and marks the call context
  consumed;
- after promotion, deferred/pending-reply capacity rules apply exactly as
  they do today;
- unused call authority completes the pending call with
  `ReplyAbandoned`.

No double-counting. No hidden extra pending table. No bypass around deferred
reply caps.

## Unused Fallback

Rust will not force linear consumption. Dropping values is legal.

So the runtime still needs a fallback, but it must be terminal.
Do not rely on Rust `Drop` doing runtime work. The implementation should
track whether `CallContext` was consumed, then check after the handler
returns and the effect is classified.

```text
CallContext returned unused
=> caller immediately receives CallOutcome::Rejected(ReplyAbandoned)
=> capacity reclaimed now
=> trace records the bug
```

This replaces "warn and wait until timeout".

The returned effect still runs.

Reason:

```text
handler returned this effect.
Tina should not hide or erase side effects.
caller truth is rejected immediately.
effect truth remains visible in trace.
```

So a bad multi-turn handler can still perform its nested runtime call, but
the original caller is already rejected and capacity is already reclaimed.
Any later continuation `reply(...)` has no caller and is a no-op/diagnostic.

`CallReplyAbandoned` should be removed or renamed. If it remains as trace
vocabulary, it must describe a terminal rejected call. It must not mean
"caller still waits".

## Rock 0 — Audit Current Paths

Read before code:

- `tina::Context`, `RequestContext`, `DeferredReply`;
- `tina-runtime/src/lib.rs` call dispatch and abandoned guard;
- `tina-sim/src/lib.rs` matching path;
- `tina-runtime/src/call.rs` builders;
- request-context tests in runtime and sim;
- docs pages 04, 10, 16;
- `specimen_multi_turn_request_context`.

Write down every path that can deliver a call-shaped message:

- local call;
- cross-shard call;
- sim local/remote call;
- host `call_blocking`;
- bridge/observed call if applicable.

## Rock 1 — Public API Shape

Add `CallContext` to `tina`.

Use this trait/macro shape unless code proves it impossible:

Current `Context` may keep its existing generics. `CallContext` is
isolate-typed because it owns reply authority for `I::Reply`.

```rust
fn handle(
    &mut self,
    msg,
    ctx: &mut Context<'_, I::Shard, I::Reply>,
) -> Effect<Self>;

fn handle_call(
    &mut self,
    msg,
    call: CallContext<'_, Self>,
) -> Effect<Self> {
    call.reject(CallRejectedReason::UnsupportedMessage)
}
```

Macro rules:

- if user writes `handle_call`, call-shaped messages use it;
- send-shaped messages still use `handle`;
- callable isolates should implement `handle_call`;
- if `reply = ...` is non-unit and `handle_call` is missing, prefer a
  macro/compile error that tells the user to add `handle_call` or mark
  the isolate send-only;
- if the macro cannot prove that, the runtime fallback rejects with
  `UnsupportedMessage`, never timeout;
- migration may temporarily allow delegation behind a named compat path,
  but final docs should teach split entry.

Send-only spelling should be explicit if needed:

```rust
#[tina_runtime::isolate(message = Msg, send_only, shard = AppShard)]
```

Exact attribute name may differ. The important rule: users should not
accidentally publish a callable address whose calls all reject because
`handle_call` was forgotten.

Keep `Context` narrow:

```text
Context is runtime facts and one-shot capabilities.
Context is not app state.
Context is not dependency injection.
Context is not arbitrary key/value storage.
```

## Rock 2 — Runtime Semantics

Live runtime:

- when an envelope has caller context, dispatch to `handle_call`;
- construct `CallContext` with exactly one request authority;
- `CallContext::reply` and `reply_to_request` complete the caller;
- `CallContext::into_request_context` promotes/captures authority;
- unused `CallContext` rejects the caller immediately;
- rejection reason is typed and traced;
- capacity is reclaimed immediately;
- returned effects still execute after unused-context rejection;
- late continuation `reply(...)` without caller remains no-op/diagnostic;
- panic before consuming call authority rejects caller with
  `CallOutcome::Rejected(CallRejectedReason::HandlerPanicked)`;
- panic after `into_request_context()` follows the existing deferred slot
  panic cleanup path and must not leak the promoted authority.

Remote/cross-shard calls must preserve the same cause/rejection truth.
Add a concrete remote outcome envelope for rejected call completion, not
just replied/full/closed:

```rust
RemoteCallOutcome::Rejected(CallRejectedReason)
```

If the existing remote path uses a different enum name, add the same
variant there. Test local->remote and remote->local when both paths exist.

## Rock 3 — Outcome And Trace Vocabulary

Use one visible caller outcome:

```rust
CallOutcome::Rejected(CallRejectedReason::ReplyAbandoned)
```

First-form reasons:

```rust
pub enum CallRejectedReason {
    ReplyAbandoned,
    HandlerPanicked,
    UnsupportedMessage,
}
```

Do not add broad user-defined rejection yet. If a service wants a domain
rejection, put it in the service reply type.

`CallError` projection maps this to a typed error:

```rust
CallError::Rejected(CallRejectedReason::ReplyAbandoned)
```

Do the broad migration. Do not collapse this into `Timeout`, `Closed`, or
a stringly internal error.

Trace must say the same fact:

```text
call replied? no.
caller captured? no.
caller rejected now.
reason: ReplyAbandoned.
```

No docs may say "caller keeps waiting".

## Rock 4 — Simulator Parity

Mirror semantics in `tina-sim`.

Same seed, same call path:

- reply;
- reject;
- carry request forward;
- drop unused context;
- panic before consume;
- panic after promote;
- remote call drop.

Trace vocabulary must match live names.

## Rock 5 — Migrate Internal Tests And Specimens

Update tests first. Then specimens.

Expected changes:

- single-turn called services use `call.reply(...)`;
- multi-turn services use `call.into_request_context()`;
- send-only messages stay in `handle`;
- call-only request messages move to `handle_call`;
- old abandoned-timeout tests become immediate rejection tests.

Do not "fix" examples by using hidden context.

## Rock 6 — Docs

User guide must teach:

```text
send handler has no caller.
call handler has caller obligation.
reply / reject / carry.
```

Keep the expanded multi-turn example.

Show the common helper only after the explicit truth:

```rust
call(worker, msg, timeout)
    .reply_with_current_request(call, ServiceMsg::WorkerReturned)
```

Only ship that helper if it consumes `CallContext` or a clearly named
request capability. Do not accept a generic `reply_with_context`.

## Rock 7 — Compatibility Decision

This is a project-wide API change.

Because Tina is still pre-public, prefer the clean break.

If breakage is too wide, use at most one temporary compat path:

```rust
fn handle_call_compat(...) {
    // delegates to old handle and runtime fallback rejects if uncaptured
}
```

The compat path must be documented as temporary and must not be the user
guide shape.

## Required Proof

Live runtime:

- call handler `call.reply(...)` replies immediately;
- call handler `call.reject(...)` returns typed rejection;
- call handler `into_request_context` plus later `reply_to_request`
  replies to original caller;
- call handler that does nothing rejects immediately, no timeout wait;
- returned effect still runs after unused-context rejection;
- capacity is reclaimed after abandoned rejection;
- panic before consuming call authority rejects with `HandlerPanicked`
  and reclaims;
- panic after request promotion does not leak deferred capacity;
- cross-shard abandoned call preserves rejection cause.

Simulator:

- same cases as live;
- trace names and event ordering stable enough for DST.

Compile/doc:

- `RequestContext` remains move-only;
- examples compile;
- old broken multi-turn shape is absent from docs;
- docs say abandoned call is terminal rejection, not warning-only.

## Ergonomic Impact

Good:

- new readers see call obligation at the function boundary;
- simple replies are shorter and clearer: `call.reply(value)`;
- multi-turn reply authority is explicit: `call.into_request_context()`;
- forgotten replies fail now, not 30 seconds later;
- `CallReplyAbandoned` stops feeling like a runtime bandaid.

Cost:

- many handlers/tests/specimens need migration;
- message protocols may split send/call cases more honestly;
- macro surface gets larger;
- some old free `reply(...)` examples become `call.reply(...)`.

This is worth it. It is core Tina semantics, not polish.

## Hostile Review Checklist

- Does any call-shaped path still wait for timeout after handler drops
  caller authority?
- Does any helper hide caller authority across turns?
- Can app code store arbitrary values in `Context`?
- Are live and sim semantics identical?
- Are rejection reasons typed and visible?
- Does capacity reclaim immediately after dropped obligation?
- Does default `handle_call` reject instead of silently doing old magic?
