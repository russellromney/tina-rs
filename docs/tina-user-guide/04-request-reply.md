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
    .reply(ClientMsg::WorkerReturned)
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
            .reply(ClientMsg::WorkerReturned),

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

## Deferred Reply

A service can reply after more than one handler turn.

Common shape:

```rust
use tina::{RequestContext, reply_to_request};

#[derive(Debug, Clone)]
enum ServiceMsg {
    Store(StoreRequest),
    Journaled(RequestContext<StoreReply>, Result<(), CallError>, StoreRequest),
}

#[tina_runtime::isolate(message = ServiceMsg, reply = StoreReply, shard = AppShard)]
impl StoreService {
    fn handle(&mut self, msg: ServiceMsg, ctx: &mut Context<'_, AppShard>) -> Effect<Self> {
        match msg {
            ServiceMsg::Store(req) => {
                let request = ctx.take_request_context().expect("Store is call-shaped");
                journal_append(self.journal.clone(), req.bytes.clone())
                    .reply_with_request(request, |request, result| {
                        ServiceMsg::Journaled(request, result, req)
                    })
            }

            ServiceMsg::Journaled(request, Ok(()), req) => {
                self.apply(req);
                reply_to_request(request, StoreReply::Stored)
            }

            ServiceMsg::Journaled(request, Err(_), _req) => {
                reply_to_request(request, StoreReply::Failed)
            }
        }
    }
}
```

The caller wrote one call:

```rust
call(store, ServiceMsg::Store(req), Duration::from_millis(50))
    .reply(ClientMsg::Stored)
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
use tina::{Context, Effect, Isolate, noop, reply_to_request};
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
#   fn handle(&mut self, msg: SvcMsg, ctx: &mut Context) -> Effect<Self> {
#     match msg {
#       SvcMsg::Start => {
#         let req = ctx.take_request_context().unwrap();
#         call(self.probe, ProbeMsg, Duration::from_millis(50))
#             .reply_with_request(req, SvcMsg::ProbeResult)
#       }
#       SvcMsg::ProbeResult(req, outcome) => {
#         match outcome {
#           CallOutcome::Replied(ProbeReply(v)) if v >= 10 => reply_to_request(req, SvcReply::Ready),
#           _ => reply_to_request(req, SvcReply::NotReady),
#         }
#       }
#     }
#   }
# }
```

The caller sees the same timeout and `Full`/`Closed`/`Timeout` outcomes as a
single-turn call. The service just answered later.

`reply_with_request` is a convenience on any call builder. It boxes a
translator that carries the `RequestContext` into the continuation message.
It does not hide any timeout or reply path.

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

## Grug Rule

No request/reply without timeout.

If a port has a Tokio `.await` waiting for another task, ask:

- what is the timeout?
- what happens on timeout?
- what happens if the destination is full?
- what happens if the destination is closed?

If the old code has no answer, Tina has found useful missing design.
