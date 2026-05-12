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
    // one private RequestContext<I::Reply>
}
```

It exposes only runtime facts and one-shot reply authority:

```rust
impl<'a, I: Isolate> CallContext<'a, I> {
    pub fn reply(self, value: I::Reply) -> Effect<I>;
    pub fn reject(self, reason: CallRejectReason) -> Effect<I>;
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
    fn handle(&mut self, msg: StoreMsg, ctx: &mut Context<'_, Store>) -> Effect<Self> {
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

            _ => call.reject(CallRejectReason::UnsupportedMessage),
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

## Unused Fallback

Rust will not force linear consumption. Dropping values is legal.

So the runtime still needs a fallback, but it must be terminal.
Do not rely on Rust `Drop` doing runtime work. The implementation should
track whether `CallContext` was consumed, then check after the handler
returns and the effect is classified.

```text
CallContext returned unused
=> caller immediately receives ReplyAbandoned / Rejected
=> capacity reclaimed now
=> trace records the bug
```

This replaces "warn and wait until timeout".

`CallReplyAbandoned` may remain as trace vocabulary only if it names a
terminal rejected call. It must not mean "caller still waits".

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

Decide trait/macro shape:

Preferred:

```rust
fn handle(&mut self, msg, ctx: &mut Context<'_, Self>) -> Effect<Self>;

fn handle_call(
    &mut self,
    msg,
    call: CallContext<'_, Self>,
) -> Effect<Self> {
    call.reject(CallRejectReason::UnsupportedMessage)
}
```

Macro rules:

- if user writes `handle_call`, call-shaped messages use it;
- send-shaped messages still use `handle`;
- default `handle_call` rejects, not delegates silently forever;
- migration may temporarily allow delegation behind a named compat path,
  but final docs should teach split entry.

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
- late continuation `reply(...)` without caller remains no-op/diagnostic.

Remote/cross-shard calls must preserve the same cause/rejection truth.

## Rock 3 — Outcome And Trace Vocabulary

Pick one visible caller outcome.

Preferred:

```rust
CallOutcome::Rejected(CallRejectedReason::ReplyAbandoned)
```

If this is too much churn, use the existing error envelope only as a
temporary bridge, but do not collapse this into `Timeout`.

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
- capacity is reclaimed after abandoned rejection;
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
