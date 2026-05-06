# tina-rs

![tina-rs hero](tina.png)

`tina-rs` is a bounded, shared-nothing concurrency framework for Rust.

It is an independent Rust implementation inspired by
[Peter Mbanugo's Tina](https://github.com/pmbanugo/tina) and by
thread-per-core systems like [Seastar](https://seastar.io/).

You write small synchronous state machines called isolates. Each isolate owns
its state, receives one message at a time, and returns an `Effect`: send this,
sleep this long, read from this socket, spawn this child, reply with this value,
or stop now.

The runtime does the dangerous parts. It owns scheduling, time, I/O,
cross-shard messages, supervision, and replay.

No async handlers. No shared state by default. No hidden unbounded queues.

Tina looks actor-shaped, but the goal is not another actor crate. The point is
the whole contract: bounded mailboxes, shard-owned execution, explicit
runtime-owned I/O, supervision, and deterministic simulation.

Tina is not an I/O runtime like Tokio or monoio. It is the concurrency model
above the runtime substrate. Today `tina-runtime` uses an explicit-step oracle,
deterministic simulation, and a threaded runtime backed by
[Pekka Enberg's Betelgeuse](https://github.com/penberg/betelgeuse). Future
bridges can ride Tokio, monoio, or another shard-local substrate if they
preserve the contract.

> Tina is very experimental and in active development.

The motivation comes from Mbanugo's article
[The Tokio/Rayon Trap and Why Async/Await Fails Concurrency](https://pmbanugo.me/blog/why-async-await-complect-concurrency).

## Why

Rust async is powerful. It also gives you sharp knives:

- unbounded channels that become memory leaks under load;
- task migration that fights shard-local state;
- `Arc<Mutex<_>>` because sharing state was easy;
- forgotten timeouts;
- failures that only reproduce when the moon is mean.

Tina chooses a smaller machine.

Each unit of work has one owner. Each queue has a capacity. Every overload case
is visible: `Full`, `Closed`, `Timeout`. Every side effect goes through the
runtime. The same isolate code can run live or inside a seeded simulator.

Same seed. Same config. Same failure.

## What Code Looks Like

Use the prelude:

```rust
use tina::prelude::*;
```

Tina is a llama. So first, one isolate owns one llama.

`LlamaMsg::Fed` means someone gave Tina a snack. The isolate updates only its
own state. It does not send anything, reply to anyone, or touch the runtime.
That is the smallest shape:

```rust
enum LlamaMsg {
    Fed(Snack),
}

#[tina::isolate(message = LlamaMsg, shard = AppShard)]
impl Tina {
    fn handle(&mut self, msg: LlamaMsg, _ctx: &mut Context<'_, AppShard>) -> Effect<Self> {
        match msg {
            LlamaMsg::Fed(snack) => {
                self.snacks.push(snack);
                noop()
            }
        }
    }
}
```

If an isolate sends or replies, it names those effect types in the macro.
Here Tina can be brushed, can tell the barn log that brushing happened, and
can reply with her snack list. The message names are the whole protocol:

- `Fed`: add a snack to local state.
- `Brushed`: write one barn log event.
- `SnackReport`: reply to whoever asked.

```rust
enum LlamaMsg {
    Fed(Snack),
    Brushed,
    SnackReport,
}

enum BarnLogMsg {
    TinaGotBrushed,
}

#[tina::isolate(
    message = LlamaMsg,
    reply = Vec<Snack>,
    send = Outbound<BarnLogMsg>,
    shard = AppShard
)]
impl Tina {
    fn handle(&mut self, msg: LlamaMsg, _ctx: &mut Context<'_, AppShard>) -> Effect<Self> {
        match msg {
            LlamaMsg::Fed(snack) => {
                self.snacks.push(snack);
                noop()
            }
            LlamaMsg::Brushed => send(self.barn_log, BarnLogMsg::TinaGotBrushed),
            LlamaMsg::SnackReport => reply(self.snacks.clone()),
        }
    }
}
```

Runtime work comes back as normal messages too. Tina does not sleep inside the
handler. She asks the runtime for a nap timer. Later the runtime sends a normal
message saying the nap is over or failed.

```rust
use tina_runtime::{CallError, sleep};

enum NapMsg {
    StartNap,
    WakeUp,
    NapFailed(CallError),
}

#[tina_runtime::isolate(
    message = NapMsg,
    send = Outbound<CaretakerMsg>,
    shard = AppShard
)]
impl Tina {
    fn handle(&mut self, msg: NapMsg, _ctx: &mut Context<'_, AppShard>) -> Effect<Self> {
        match msg {
            NapMsg::StartNap => sleep(self.nap_time).reply(|result| match result {
                Ok(()) => NapMsg::WakeUp,
                Err(reason) => NapMsg::NapFailed(reason),
            }),
            NapMsg::WakeUp => send(self.caretaker, CaretakerMsg::LlamaAwake),
            NapMsg::NapFailed(_) => stop(),
        }
    }
}
```

Tokio keeps that control flow inside an async task:

```rust
tokio::spawn(async move {
    tokio::time::sleep(nap_time).await;
    caretaker.send(CaretakerMsg::LlamaAwake).await?;
});
```

Tina splits it into an effect and a later message:

```rust
NapMsg::StartNap => sleep(self.nap_time).reply(|_| NapMsg::WakeUp),
NapMsg::WakeUp => send(self.caretaker, CaretakerMsg::LlamaAwake),
```

Tokio suspends the function. Tina returns to the runtime. The runtime owns the
timer and sends a normal message when it completes.

No `.await` in the handler. No socket read in user code. The handler describes
work. The runtime performs it and sends the result back later.

Full runnable examples:

- [`task_dispatcher.rs`](tina-runtime/examples/task_dispatcher.rs)
- [`tcp_echo.rs`](tina-runtime/examples/tcp_echo.rs)
- [`llama_bridge.rs`](tina-tokio-bridge/examples/llama_bridge.rs)

## What Works Today

This repo is a Cargo workspace with six crates:

- **`tina`**: traits, effects, typed addresses, helper functions, isolate
  macros, supervision policy types.
- **`tina-mailbox-spsc`**: bounded single-producer/single-consumer mailbox.
- **`tina-supervisor`**: supervisor config.
- **`tina-runtime`**: explicit-step runtime, multi-shard runner,
  `ThreadedRuntime` over the Betelgeuse backend, runtime-owned TCP/time, observed
  backpressure, isolate calls with mandatory timeout, local snapshot/journal
  persistence helpers, preferred `LocalSystem`/`LocalMultiShardSystem` app
  owners, and a named `TINA_DRIVER_RUNTIME_CONTRACT`.
- **`tina-sim`**: deterministic simulator with virtual time, seeded faults,
  checkers, scripted TCP, durable images, and replay.
- **`tina-tokio-bridge`**: narrow bounded ingress from Tokio/Tower/Axum into a
  Tina service, with explicit health, metrics, timeout, cancellation, and
  overload policies. Bridge-hosted services can use ordinary Tina message enums,
  so runtime calls still fit. Tokio owns the edge. Tina owns the isolate state.

You can write isolates against the modern surface: `Outbound`,
`ChildDefinition`, `RestartableChildDefinition`, `RuntimeCall`, `CallInput`,
`CallOutput`, `CallError`, `#[tina::isolate(...)]`,
`#[tina_runtime::isolate(...)]`, `send(...)`, `reply(...)`, `stop()`,
`batch(...)`, `sleep(...)`, `tcp_read(...)`, `tcp_write(...)`,
`snapshot_commit(...)`, `snapshot_load(...)`, `journal_append(...)`, and
`journal_replay(...)`.

Local persistence has an explicit support table:
`LOCAL_PERSISTENCE_SUPPORT` names temp-write, rename, file fsync,
parent-directory fsync, truncated-tail warning, and checksum validation
support for the current build. No quiet durability claim.
If snapshot rename succeeds but the final durability step cannot be proven,
Tina reports `CallError::CommitUncertain`, because disk state may already have
changed. Live persistence runs through a bounded storage lane; already-started
local filesystem work still cannot be preempted. These are correct-first
helpers, not a high-throughput storage reactor.

The canonical local-service proof is
[`portable_service.rs`](tina-runtime/tests/portable_service.rs). It wires one
copyable app shape: configure budgets, register router and shard-owned workers,
route by key, apply visible backpressure, perform runtime-owned persistence
before reply, shut down, inspect terminal truth, and replay the durable journal.
The simulator companion
[`portable_service_dst.rs`](tina-sim/tests/portable_service_dst.rs) runs the
same service idea through saved-seed DST with replay and shrinking.

The old `SendMessage`, `SpawnSpec`, `CurrentCall`, `CallRequest`,
`CallResult`, `CallFailureReason`, and `tina-runtime-current` names are not the
public teaching surface.

## What Does Not Yet

- not production-ready;
- Tokio/Tower/Axum bridge is narrow first form only;
- the driver-runtime contract is named, but Tina is not a general Rust async
  runtime;
- local persistence is snapshot/journal only, not a database or durable
  mailbox;
- no remoting or clustering;
- no broad zero-allocation claim;
- no general async ecosystem integration;
- no promise that every Tokio-shaped server should be ported today.

## The Rule

If something can overload, Tina should make it visible.

If something can fail, Tina should make it traceable.

If something can race, Tina should make it replayable.

## Design

| Idea | What it means |
|---|---|
| **Isolate-per-entity** | Tenants, connections, sessions, workers, or protocol roles each get a typed state machine. |
| **Effect-returning handlers** | Handlers are synchronous. The runtime executes the returned effect. |
| **Bounded queues** | Mailboxes and cross-shard queues have capacity. `Full` and `Closed` are normal outcomes. |
| **Shard-owned execution** | A shard owns its isolates, timers, runtime resources, and cross-shard queues. |
| **Supervision** | Parent isolates can restart children with policy and budget. |
| **Deterministic simulation** | Time, I/O, faults, and message order can be driven from a seed. |

None of this is new. Erlang, Akka, Seastar, TigerBeetle, FoundationDB, and
Tina-Odin all matter here. `tina-rs` is these ideas expressed as Rust traits
and small implementation crates.

## Development

```bash
make verify   # full project gate, including service/DST/bridge/cost smoke
make portable-runtime-cost    # optional local smoke rows, not a benchmark
make miri     # focused unsafe-memory checks for tina-mailbox-spsc
```

Individual targets: `make fmt`, `make check`, `make test`, `make doc`, and
`make clippy`.

## Prior Art

- [Tina](https://github.com/pmbanugo/tina) by Peter Mbanugo
- [The Tokio/Rayon Trap and Why Async/Await Fails Concurrency](https://pmbanugo.me/blog/why-async-await-complect-concurrency)
- [Seastar](https://seastar.io/)
- [Betelgeuse](https://github.com/penberg/betelgeuse) by Pekka Enberg
- [monoio](https://github.com/bytedance/monoio)
- [TigerBeetle](https://tigerbeetle.com/)
- [FoundationDB simulation testing](https://www.youtube.com/watch?v=4fFDFbi3toc)
- [loom](https://github.com/tokio-rs/loom)
- [Miri](https://github.com/rust-lang/miri)

## License

Dual-licensed under MIT or Apache-2.0, at your option.
