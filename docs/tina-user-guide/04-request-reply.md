# Request Reply

Sometimes one isolate needs an answer from another isolate.

Use a call with a timeout.

Tokio shape:

```rust
let answer = worker.ask(job).await?;
```

Tina shape:

```rust
call(worker, WorkerMsg::Run(job), Duration::from_millis(50))
    .then(ClientMsg::WorkerReturned)
```

Full shape:

```rust
use std::time::Duration;
use tina::prelude::*;
use tina_runtime::{call, CallOutcome};

#[derive(Debug, Clone)]
enum WorkerMsg {
    Run(Job),
}

#[derive(Debug, Clone)]
enum ClientMsg {
    Start(Job),
    WorkerReturned(CallOutcome<WorkerReply>),
}

struct Client {
    worker: Address<WorkerMsg>,
}

#[tina_runtime::isolate(message = ClientMsg, shard = AppShard)]
impl Client {
    fn handle(&mut self, msg: ClientMsg, _ctx: &mut Context<'_, AppShard>) -> Effect<Self> {
        match msg {
            ClientMsg::Start(job) => call(
                self.worker,
                WorkerMsg::Run(job),
                Duration::from_millis(50),
            )
            .then(ClientMsg::WorkerReturned),

            ClientMsg::WorkerReturned(CallOutcome::Replied(reply)) => {
                self.use_reply(reply);
                noop()
            }
            ClientMsg::WorkerReturned(CallOutcome::Timeout) => {
                self.note_timeout();
                noop()
            }
            ClientMsg::WorkerReturned(CallOutcome::Closed) => stop(),
        }
    }
}
```

Names may move as the API settles. The rule should not move:

```text
request/reply always has timeout
timeout is normal outcome
caller handles timeout
```

> **Multi-turn reply rule**
>
> `call(...).then(...)` creates a continuation message. It does not create
> a hidden async stack, and it does not automatically carry the original caller
> into later handler turns.
>
> If a service must call something else before answering its caller, consume the
> call authority by deferring through that visible work:
>
> ```rust
> fn handle_call(&mut self, msg: ServiceMsg, call_ctx: CallContext<'_, Self>) -> Effect<Self> {
>     call_ctx
>         .defer(call(worker, WorkerMsg::Run(job), Duration::from_millis(50)))
>         .reply(ServiceMsg::WorkerReturned)
> }
> ```
>
> Then consume that request context in the final turn:
>
> ```rust
> ServiceMsg::WorkerReturned(request, CallOutcome::Replied(reply)) => {
>     reply_to_request(request, ServiceReply::Done(reply))
> }
> ```
>
> If a call handler ignores its `CallContext`, Tina immediately completes the
> caller with `CallOutcome::Rejected(CallRejectedReason::ReplyAbandoned)`.
> The handler's returned effect still runs; it just no longer has caller
> authority.

## Deferred Reply

A service can reply after more than one handler turn.

Common shape:

```rust
use tina::{CallContext, CallRejectedReason, RequestContext, noop, reply_to_request};

#[derive(Debug, Clone)]
enum ServiceMsg {
    Store(StoreRequest),
    Journaled(RequestContext<StoreReply>, Result<(), CallError>, StoreRequest),
}

#[tina_runtime::isolate(message = ServiceMsg, reply = StoreReply, shard = AppShard)]
impl StoreService {
    fn handle(&mut self, msg: ServiceMsg, _ctx: &mut Context<'_, AppShard>) -> Effect<Self> {
        match msg {
            ServiceMsg::Store(_) => noop(),

            ServiceMsg::Journaled(request, Ok(()), req) => {
                self.apply(req);
                reply_to_request(request, StoreReply::Stored)
            }

            ServiceMsg::Journaled(request, Err(_), _req) => {
                reply_to_request(request, StoreReply::Failed)
            }
        }
    }

    fn handle_call(&mut self, msg: ServiceMsg, call_ctx: CallContext<'_, Self>) -> Effect<Self> {
        match msg {
            ServiceMsg::Store(req) => call_ctx
                .defer(journal_append(self.journal.clone(), req.bytes.clone()))
                .reply(|request, result| ServiceMsg::Journaled(request, result, req)),
            ServiceMsg::Journaled(_, _, _) => {
                call_ctx.reject(CallRejectedReason::UnsupportedMessage)
            }
        }
    }
}
```

The caller wrote one call:

```rust
call(store, ServiceMsg::Store(req), Duration::from_millis(50))
    .then(ClientMsg::Stored)
```

The service did a runtime call before replying. Tina did not keep hidden
caller context. The handler captured a `RequestContext`, moved it through
the continuation message, and consumed it at the final reply.

This is the service pattern for HTTP clients, RPC clients, database clients, and
other "takes several I/O turns before answering" code.

## Request Context

A deferred reply can be carried as a typed `RequestContext<R>`.

This signals intent: "I will reply later through a multi-turn workflow."
It is the same primitive as `DeferredReply` but the type name tells readers
what to expect.

```rust
use tina::{CallContext, CallRejectedReason, Context, Effect, Isolate, noop, reply_to_request};
use tina_runtime::{call, CallOutcome};

#[derive(Debug, Clone)]
enum SvcMsg {
    Start,
    ProbeResult(RequestContext<SvcReply>, CallOutcome<ProbeReply>),
}

# struct Svc { probe: tina::Address<ProbeMsg, ProbeReply> }
# impl Isolate for Svc {
#   type Message = SvcMsg;
#   type Reply = SvcReply;
#   fn handle(&mut self, msg: SvcMsg, _ctx: &mut Context) -> Effect<Self> {
#     match msg {
#       SvcMsg::Start => noop(),
#       SvcMsg::ProbeResult(req, outcome) => {
#         match outcome {
#           CallOutcome::Replied(ProbeReply(v)) if v >= 10 => reply_to_request(req, SvcReply::Ready),
#           _ => reply_to_request(req, SvcReply::NotReady),
#         }
#       }
#     }
#   }
#   fn handle_call(&mut self, msg: SvcMsg, call_ctx: CallContext<'_, Self>) -> Effect<Self> {
#     match msg {
#       SvcMsg::Start => call_ctx
#         .defer(call(self.probe, ProbeMsg, Duration::from_millis(50)))
#         .reply(SvcMsg::ProbeResult),
#       SvcMsg::ProbeResult(_, _) => call_ctx.reject(CallRejectedReason::UnsupportedMessage),
#     }
#   }
# }
```

The caller sees the same timeout and `Full`/`Closed`/`Timeout` outcomes as a
single-turn call. The service just answered later.

`CallContext::defer(work).reply(...)` consumes caller authority, converts it to
`RequestContext`, and carries that context into the continuation message. It
does not hide any timeout or reply path, and it does not produce the final
application reply by itself.

> **Cancelable deferred calls: admit before dispatch**
>
> `call_ctx.defer_cancelable(call_cancelable(...))` creates a pending token and
> a child effect. Admit the token into bounded state before any child effect is
> returned:
>
> ```rust
> match call_ctx
>     .defer_cancelable(call_cancelable(worker, WorkerMsg::Run(job), timeout))
>     .try_admit(&mut self.pending, id, ServiceMsg::WorkerReturned)
> {
>     Ok(effect) => effect,
>     Err(PendingCancelableInsertError::Full { token }) => {
>         reply_to_request(token.into_request_context(), ServiceReply::Busy)
>     }
>     Err(PendingCancelableInsertError::DuplicateKey { token }) => {
>         reply_to_request(token.into_request_context(), ServiceReply::Duplicate)
>     }
> }
> ```
>
> `try_admit` does not dispatch work. It only returns the child effect after
> storage succeeds. On `Full` or duplicate, recover the caller from the
> rejected token and answer now.
>
> The `id` is your domain key: useful for duplicate rejection and user-driven
> cancel-by-id. `try_admit` also carries a `PendingCancelableTicket` into
> `ServiceMsg::WorkerReturned`; that ticket is the exact admitted instance.
> Remove completions with `(id, ticket)`, not `id` alone, so stale completions
> cannot remove a newer call that reused the same key.
>
> On owner stop or service shutdown, drain the set and settle every token.
> If the child call may still be in flight, cancel the token and reply from
> the cancel continuation; do not merely drop the token or the child handle.

The expanded form is still available when it is clearer:

```rust
let req = call_ctx.into_request_context();
call(self.probe, ProbeMsg, Duration::from_millis(50))
    .then_with_request(req, SvcMsg::ProbeResult)
```

Use `then(...)` for ordinary continuations that do not carry caller authority.
If a call handler returns `then(...)` without consuming its `CallContext`, Tina
rejects the caller with `ReplyAbandoned` immediately while the ordinary
continuation still runs.

`RequestContext` is a real newtype over `DeferredReply`; `reply_to_request`
consumes it and delegates to `reply_to`. There is no hidden caller context
preservation.

## Worker Side


Worker declares a reply type and returns `reply(...)`.

```rust
#[tina::isolate(message = WorkerMsg, reply = WorkerReply, shard = AppShard)]
impl Worker {
    fn handle(&mut self, msg: WorkerMsg, _ctx: &mut Context<'_, AppShard>) -> Effect<Self> {
        match msg {
            WorkerMsg::Run(job) => reply(self.run(job)),
        }
    }
}
```

## Request-Scoped Children

A multi-turn request that fans out to several child rails (DB call,
outbound HTTP, internal worker, …) wants one button: "the request
went away; stop the children." The runtime primitive is
`RequestScope` (in `tina-runtime::scope`). The blessed pattern:

```text
let scope = RequestScope::with_child_cap(RequestScopeId::alloc(), 3);
self.scopes.try_insert(request_id, scope.clone())?;

// One admission per child rail. The scope is registered first so a
// scope-wide cancel that races with admission still closes the wait.
let admit = call_ctx
    .defer_scoped(&scope, "db_lookup", call_cancelable(db, q, t))
    .try_admit(&mut self.pending, request_id, Msg::DbReturned)?;
return admit; // child effect runs only after admission succeeded
```

When the request dies (client disconnect, per-request deadline, owner
stop), call `scope.cancel_into_effects(cause, translator)`. It returns
a synchronous [`ScopeCancelReport`] and a list of `Effect::Call`
cancellations — return them from the handler. Any rail that does not
expose a `CallHandle` (sleep, raw TCP read/write, body sources) is not
yet scope-cancellable; wire an application `Cancel` message for those.

See [lifecycle § Request-Scoped Cancellation](14-lifecycle-and-shutdown.md#request-scoped-cancellation)
for the full truth table and bridge honesty rules.

## Grug Rule

No request/reply without timeout.

If a port has a Tokio `.await` waiting for another task, ask:

- what is the timeout?
- what happens on timeout?
- what happens if the destination is full?
- what happens if the destination is closed?

If the old code has no answer, Tina has found useful missing design.
