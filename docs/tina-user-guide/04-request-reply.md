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

## Picking A Reply Shape

The caller always writes `call(...).then(...)`. The service picks how it
answers. Three shapes cover almost everything:

- **Same turn.** The service can answer from the handler that received the
  request. Return `reply(value)`.
- **More than one turn.** The service must do one runtime call before it can
  answer. Use `call_ctx.defer(work).reply(...)`.
- **A pipeline.** The service does several runtime calls in sequence. Use
  [`tina::flow!`](29-continuation-flows.md).

Reach for the first that fits. The rest of this page is those three shapes,
then the lower-level tools for when they do not fit.

## Same Turn: `reply`

The worker declares a reply type and returns `reply(...)` from the handler that
received the message.

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

This is the common case. If the answer is in hand, answer now.

## More Than One Turn: `defer(work).reply`

If a service must call something else before answering its caller, it cannot
reply in the same turn. `call(...).then(...)` creates a continuation message. It
does not create a hidden async stack, and it does not automatically carry the
original caller into later handler turns.

Consume the call authority by deferring through the visible work:

```rust
fn handle_call(&mut self, msg: ServiceMsg, call_ctx: CallContext<'_, Self>) -> Effect<Self> {
    call_ctx
        .defer(call(worker, WorkerMsg::Run(job), Duration::from_millis(50)))
        .reply(ServiceMsg::WorkerReturned)
}
```

Then consume that request context in the final turn:

```rust
ServiceMsg::WorkerReturned(request, CallOutcome::Replied(reply)) => {
    reply_to(request, ServiceReply::Done(reply))
}
```

If a call handler ignores its `CallContext`, Tina immediately completes the
caller with `CallOutcome::Rejected(CallRejectedReason::ReplyAbandoned)`.
The handler's returned effect still runs; it just no longer has caller
authority.

Worked shape. The service journals before replying:

```rust
use tina::{CallContext, CallRejectedReason, RequestContext, noop, reply_to};

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
                reply_to(request, StoreReply::Stored)
            }

            ServiceMsg::Journaled(request, Err(_), _req) => {
                reply_to(request, StoreReply::Failed)
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

`CallContext::defer(work).reply(...)` consumes caller authority, converts it to
`RequestContext`, and carries that context into the continuation message. It
does not hide any timeout or reply path, and it does not produce the final
application reply by itself.

The caller sees the same timeout and `Full`/`Closed`/`Timeout` outcomes as a
single-turn call. The service just answered later.

This is the service pattern for HTTP clients, RPC clients, database clients, and
other "takes several I/O turns before answering" code.

## A Pipeline: `tina::flow!`

When a request makes several runtime calls in a row — load a row, acquire a
connection, send it, answer — the continuation enum and dispatch method become
boilerplate. `tina::flow!` writes them for you. See
[Continuation Flows](29-continuation-flows.md).

Start the first step with the same `call_ctx.defer(work).reply(...)` spelling.
Each later step receives the full `CallOutcome<T>` and threads the
`RequestContext` forward until one step replies. It is the default for the
common "do one runtime call, inspect outcome, dispatch the next" request path.

## When You Need More Control

The three shapes above are the default. Reach below them only when the workflow
does not fit.

### Raw Request Context

A deferred reply can be carried as a typed `RequestContext<R>`.

This signals intent: "I will reply later through a multi-turn workflow."
It is the same primitive as `DeferredReply` but the type name tells readers
what to expect.

```rust
use tina::{CallContext, CallRejectedReason, Context, Effect, Isolate, noop, reply_to};
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
#           CallOutcome::Replied(ProbeReply(v)) if v >= 10 => reply_to(req, SvcReply::Ready),
#           _ => reply_to(req, SvcReply::NotReady),
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

The expanded form is available when it is clearer than `defer(work).reply(...)`:

```rust
let req = call_ctx.into_request_context();
call(self.probe, ProbeMsg, Duration::from_millis(50))
    .then_with_request(req, SvcMsg::ProbeResult)
```

Use `then(...)` for ordinary continuations that do not carry caller authority.
If a call handler returns `then(...)` without consuming its `CallContext`, Tina
rejects the caller with `ReplyAbandoned` immediately while the ordinary
continuation still runs.

`RequestContext` is a real newtype over `DeferredReply`; `reply_to`
consumes it and delegates to `reply_to`. There is no hidden caller context
preservation.

### Cancelable Deferred Calls: Admit Before Dispatch

`call_ctx.defer_cancelable(call_cancelable(...))` creates a pending token and
a child effect. Admit the token into bounded state before any child effect is
returned:

```rust
match call_ctx
    .defer_cancelable(call_cancelable(worker, WorkerMsg::Run(job), timeout))
    .try_admit(&mut self.pending, id, ServiceMsg::WorkerReturned)
{
    Ok(effect) => effect,
    Err(PendingCancelableInsertError::Full { token }) => {
        reply_to(token.into_request_context(), ServiceReply::Busy)
    }
    Err(PendingCancelableInsertError::DuplicateKey { token }) => {
        reply_to(token.into_request_context(), ServiceReply::Duplicate)
    }
}
```

`try_admit` does not dispatch work. It only returns the child effect after
storage succeeds. On `Full` or duplicate, recover the caller from the
rejected token and answer now.

The `id` is your domain key: useful for duplicate rejection and user-driven
cancel-by-id. `try_admit` also carries a `PendingCancelableTicket` into
`ServiceMsg::WorkerReturned`; that ticket is the exact admitted instance.
Remove completions with `(id, ticket)`, not `id` alone, so stale completions
cannot remove a newer call that reused the same key.

On owner stop or service shutdown, drain the set and settle every token.
If the child call may still be in flight, cancel the token and reply from
the cancel continuation; do not merely drop the token or the child handle.

### Split Events And Requests

For new services, prefer separate domain types for mailbox events and callable
requests:

```rust
#[derive(Debug)]
enum CacheEvent {
    FillDone { key: String },
}

#[derive(Debug)]
enum CacheRequest {
    Get { key: String },
}

#[tina_runtime::isolate(event = CacheEvent, request = CacheRequest, reply = CacheReply)]
impl Cache {
    fn handle_event(
        &mut self,
        event: CacheEvent,
        ctx: &mut Context<'_, SingleShard, Self::Reply>,
    ) -> Effect<Self> {
        // fire-and-forget continuations land here
    }

    fn handle_request(
        &mut self,
        request: CacheRequest,
        call: RequestCall<'_, Self>,
    ) -> RequestEffect<Self> {
        // caller-authority requests land here
    }
}
```

`handle_request` returns `RequestEffect<Self>`, not ordinary `Effect<Self>`.
That is intentional: `noop()` does not type-check on the copied request path.
Use `call.reply(...)`, `call.reject(...)`, `call.capture(...)`, or
`call.defer(...).reply(...)`.
Copied app code cannot manufacture a `RequestEffect` from `noop()`; the raw
constructor lives under the hidden runtime-internal escape hatch for adapter
crates that have already consumed caller authority.

Register it through `register_split_service`. The returned handle has two
lanes:

```rust
let cache = runtime.register_split_service::<Cache, CacheEvent, CacheRequest, Infallible>(
    Cache::new(),
    64,
);

tina::send_event(cache.events, CacheEvent::FillDone { key });
tina_runtime::call_request(cache.requests, CacheRequest::Get { key }, timeout);
```

From host-thread code:

```rust
runtime.try_send_event(cache.events, CacheEvent::FillDone { key })?;
runtime.send_event_and_observe(cache.events, CacheEvent::FillDone { key })?;
runtime.call_blocking_request(cache.requests, CacheRequest::Get { key }, timeout)?;
```

The compiler rejects the two common wrong shapes:

```text
send_event(cache.events, CacheRequest::Get { ... })   // expected CacheEvent
call_request(cache.requests, CacheEvent::FillDone { ... }, timeout) // expected CacheRequest
```

The raw `ServiceMessage<Event, Request>` envelope still exists for runtime and
interop code. Keep it out of copied service code unless you are deliberately
using the escape hatch. The escape hatch has boring runtime truth:

```text
raw Event sent on the call lane -> Rejected(UnsupportedMessage)
raw Request sent on the send/event lane -> visible Reject effect; request handler is not run
```

That second case is why copied app code should keep the event/request
capabilities instead of passing raw envelope addresses around. A send has no
caller to reject, so the compiler rail is the safety feature. The runtime trace
still records that the raw wrong-lane path returned `Reject`.

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
stop), call `scope.cancel_into_effect(cause, translator)`. It returns a
synchronous `ScopeCancelReport` and one batched `Effect::Io` cancel —
return it from the handler. Wrap that report, the post-removal
`RequestScopeSetCapacityReport`, and any late-result / ignored-timer
counts in a `ScopedRequestReport` so the request-level teardown is one
typed value.

Two honesty rules for rails that do not fit a plain `CallHandle` cancel:

- **Timers.** Plain `sleep` is not `CallHandle`-cancelable. Use a
  `ScopedTimerSet`: arm a `ScopedTimer` for the deadline, tombstone it on
  cancel, and when the physical sleep fires later the continuation reads
  `ScopedTimerFire::IgnoredLate` and skips the user work. The ignored
  count is visible, never a silent magic cancel.
- **HTTP body/session rails.** Use the adapters in `tina_http::scope`:
  `scoped_request_body_pull` registers a parked body pull,
  `scoped_websocket_send` / `_report` / `_close` register a single
  WebSocket operation a request owns (the session is not the scope), and
  `cancel_response_source` issues the protocol-honest
  `ResponseChunkMsg::Cancel`. A rail with no cancel handle (a buffered
  body already in hand) is recorded as an `UnsupportedScopeRow`, not
  pretended-cancelled.

See [lifecycle § Request-Scoped Cancellation](14-lifecycle-and-shutdown.md#request-scoped-cancellation)
for the full truth table and bridge honesty rules, and
[service patterns § One HTTP Request Is One Request Tree](10-service-patterns.md#one-http-request-is-one-request-tree)
for the copied shape.

## Grug Rule

No request/reply without timeout.

If a port has a Tokio `.await` waiting for another task, ask:

- what is the timeout?
- what happens on timeout?
- what happens if the destination is full?
- what happens if the destination is closed?

If the old code has no answer, Tina has found useful missing design.
