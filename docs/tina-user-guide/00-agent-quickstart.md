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

The effect constructors (`noop`, `reply`, `send`, `stop`, `batch`, `spawn`)
come with `tina::prelude::*`; the runtime calls (`call`, `sleep`, `tcp_*`)
come from `tina_runtime`. Names move sometimes. The shape matters more than
the exact path.

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

## First Service To Copy

For an HTTP service with state, DB work, outbound HTTP, readiness, shutdown,
capacity, and a live-replay fact, copy:

```text
examples/systems/mini_saas_api
```

Run it from the repo root:

```sh
cargo test --manifest-path examples/systems/mini_saas_api/Cargo.toml
cargo run --manifest-path examples/systems/mini_saas_api/Cargo.toml -- smoke
cargo run --manifest-path examples/systems/mini_saas_api/Cargo.toml -- pressure
```

The route to study first is `POST /items/{id}/notify`: it starts from
`call_ctx.defer(...).reply(...)`, does SQLite work, acquires a native keepalive
pool lease, calls an upstream HTTP service, releases the lease, and only then
replies to the original HTTP caller.

## Request Reply Shape

Caller:

```rust
call(worker, WorkerMsg::Run(job), Duration::from_millis(50))
    .then(ClientMsg::WorkerReturned)
```

Worker:

```rust
match msg {
    WorkerMsg::Run(job) => reply(self.run(job)),
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

Then write one `#[test]` calling `assert_replay_case(&case(),
run_case)`. Same seed, same story. Saved seed, saved bug.

Sweep seeds with `sweep_seeds`. Shrink the failing history with
`shrink_replay_case`. Both helpers return pasteable output.

Do not roll a per-test `Report` struct or hand-rolled fingerprint
comparison. See the [Simulation And DST](08-simulation-and-dst.md)
chapter and `examples/specimen_replay_dst` for the copyable shape.
