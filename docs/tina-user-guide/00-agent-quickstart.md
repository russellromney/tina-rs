# Agent Quickstart

Read this before writing Tina code.

Tina is not async Rust with different names. Tina is state machines plus
runtime-owned effects.

## Core Rules

- Handler is sync.
- Handler owns state.
- Handler returns one `Effect`.
- Runtime performs side effects.
- Runtime later sends continuation messages.
- Every request/reply has a timeout.
- Every important queue has a capacity.
- Overload is a normal result: `Full`, `Closed`, or `Timeout`.

No `.await` in handler. No direct socket reads in handler. No hidden unbounded
channel unless the boundary is explicitly an adapter.

## Common Imports

```rust
use std::time::Duration;
use tina::prelude::*;
use tina_runtime::{call, sleep, tcp_read, tcp_write, CallError, CallOutcome};
```

The effect constructors (`noop`, `send`, `stop`, `batch`, `spawn`) come with
`tina::prelude::*`; the runtime calls (`call`, `sleep`, `tcp_*`) come from
`tina_runtime`. Replies consume caller authority through `CallContext`,
`RequestCall`, or a captured `RequestContext`.

## First Shape To Reach For

```rust
#[derive(Debug, Clone)]
enum Msg {
    Start,
    Done(Result<(), CallError>),
}

struct Worker;

#[tina_runtime::isolate(message = Msg, shard = AppShard)]
impl Worker {
    fn handle(&mut self, msg: Msg, _ctx: &mut Context<'_, AppShard>) -> Effect<Self> {
        match msg {
            Msg::Start => sleep(Duration::from_millis(10)).then(Msg::Done),
            Msg::Done(Ok(())) => noop(),
            Msg::Done(Err(_)) => stop(),
        }
    }
}
```

## First Program To Run

Start with the complete program used by the root README:

```text
tina-runtime/examples/hello_world.rs
```

Run it from the repo root:

```sh
cargo run --locked -p tina-runtime --example hello_world
```

The repository-root `examples/` tree is an R&D specimen corpus. Consult it when
investigating a specific service shape, but do not treat every specimen as a
blessed user template.

## Request Reply Shape

Caller:

```rust
call(worker, WorkerMsg::Run(job), Duration::from_millis(50))
    .then(ClientMsg::WorkerReturned)
```

Worker call handler:

```rust
fn handle_call(
    &mut self,
    msg: WorkerMsg,
    call: CallContext<'_, Self>,
) -> Effect<Self> {
    match msg {
        WorkerMsg::Run(job) => call.reply(self.run(job)),
    }
}
```

If the worker needs I/O first, it can still reply later through continuation
messages. Do not spawn a one-shot child only to route a reply back.

## When Porting Tokio

Map:

| Tokio | Tina |
| --- | --- |
| task state across `.await` | isolate fields |
| `.await` point | next message variant |
| `tokio::spawn` | child isolate |
| `mpsc` | bounded mailbox |
| socket read/write | runtime call effect |
| `sleep().await` | `sleep(...).then(...)` |
| request then await answer | `call(..., timeout).then(...)` |
| `tokio::time::timeout` budget through a chain | `Deadline` value, `ctx.deadline_after(d)` |
| `JoinSet::abort_all` | `PendingCallSet` + drain + `cancel_call` per handle |

## What To Check Before Done

- What is the mailbox capacity?
- What is the call timeout?
- What happens when destination is full?
- What happens when destination is closed?
- What happens when caller times out but callee later replies?
- What resource is owned by which isolate?
- Can the same logic run in `tina-sim`?

If you cannot answer those, the code is not done.

## When You Find A Bug In Sim

Save it as a `ReplayCase` in `tina_sim::dst`. The case is plain
Rust data: name, seed, full `SimulatorConfig`, declared mailbox
capacities, history, pinned event count, pinned `stable_trace_hash`.

Then write one `#[test]` calling `assert_replay_case(&case(), run_case)`.
Replay identity includes the simulator version, complete config, initial state,
and materialized history. The seed affects only perturbation axes enabled in
the config; changing a seed with every axis disabled normally changes nothing.

Sweep seeds with `sweep_seeds`. Shrink the failing history with
`shrink_replay_case`. Both helpers return pasteable output.

Do not use an event count or trace hash as the only business assertion. See the
[Simulation And DST](08-simulation-and-dst.md) chapter for the full workflow.
