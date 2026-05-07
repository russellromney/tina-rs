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
#[derive(Debug, Clone)]
enum ServiceMsg {
    Store(StoreRequest),
    Journaled(Result<(), CallError>, StoreRequest),
}

#[tina_runtime::isolate(message = ServiceMsg, reply = StoreReply, shard = AppShard)]
impl StoreService {
    fn handle(&mut self, msg: ServiceMsg, _ctx: &mut Context<'_, AppShard>) -> Effect<Self> {
        match msg {
            ServiceMsg::Store(req) => journal_append(self.journal.clone(), req.bytes.clone())
                .reply(|result| ServiceMsg::Journaled(result, req)),

            ServiceMsg::Journaled(Ok(()), req) => {
                self.apply(req);
                reply(StoreReply::Stored)
            }

            ServiceMsg::Journaled(Err(_), _req) => reply(StoreReply::Failed),
        }
    }
}
```

The caller wrote one call:

```rust
call(store, ServiceMsg::Store(req), Duration::from_millis(50))
    .reply(ClientMsg::Stored)
```

The service did a runtime call before replying. Tina kept the reply context.

This is the service pattern for HTTP clients, RPC clients, database clients, and
other "takes several I/O turns before answering" code.

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
