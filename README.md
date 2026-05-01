# tina-rs

![tina-rs hero](tina.png)

**A shared-nothing, thread-per-core concurrency toolkit for Rust.**

> Write simple, synchronous-looking state machines. Get bounded mailboxes,
> restartable children, runtime-owned I/O, and deterministic simulation.

`tina-rs` is an independent Rust port inspired by
[Peter Mbanugo's Tina](https://github.com/pmbanugo/tina) and the article
[Why async/await complect concurrency](https://pmbanugo.me/blog/why-async-await-complect-concurrency).
You write typed isolates that handle one message at a time and return
descriptions of work. The runtime owns delivery, time, and I/O. Because of
that split, failures can be reproduced in simulation instead of hunted with
logs and hope.

This is not a Tokio replacement and not an official Tina port. It is the Rust
version of the discipline: isolate-per-entity state machines, bounded queues,
thread-per-core execution, supervision, and replayable tests.

## A Small Example

In `tina-rs`, handlers are ordinary synchronous functions. They do not perform
I/O directly and they do not `await`. They update local state and return an
`Effect`.

```rust
use std::convert::Infallible;
use tina::{Address, Context, Effect, Isolate, SendMessage, Shard, ShardId};

#[derive(Clone, Copy)]
enum CounterMsg {
    Add(u64),
    Read,
}

#[derive(Clone, Copy)]
enum AuditMsg {
    Total(u64),
}

struct AppShard;

impl Shard for AppShard {
    fn id(&self) -> ShardId {
        ShardId::new(0)
    }
}

struct Counter {
    total: u64,
    audit: Address<AuditMsg>,
}

impl Isolate for Counter {
    type Message = CounterMsg;
    type Reply = u64;
    type Send = SendMessage<AuditMsg>;
    type Spawn = Infallible;
    type Shard = AppShard;

    fn handle(
        &mut self,
        msg: Self::Message,
        _ctx: &mut Context<'_, Self::Shard>,
    ) -> Effect<Self> {
        match msg {
            CounterMsg::Add(n) => {
                self.total += n;
                Effect::Send(SendMessage::new(self.audit, AuditMsg::Total(self.total)))
            }
            CounterMsg::Read => Effect::Reply(self.total),
        }
    }
}
```

That is the core model: local state, one message at a time, explicit effects.

## Features via Constraints

`tina-rs` gets its shape by saying "no" to a few things:

- **No hidden unbounded queues.** Mailboxes are bounded. `Full` and `Closed`
  are real outcomes, not edge cases swept under the heap.
- **No handler-owned I/O.** Handlers return call effects. The runtime performs
  bind, accept, read, write, close, sleep, and completion delivery.
- **No shared mutable hot-path state.** The model is isolate-per-entity, not
  `Arc<Mutex<...>>` everywhere.
- **No scheduler opacity.** The current runtime and simulator both expose a
  deterministic event trace.
- **No "works on my laptop" testing story.** `tina-sim` can replay seeded
  runs over timers, local-send faults, supervision, and single-shard TCP I/O.

## What Works Today

The project is already past the vocabulary-only stage.

- `tina` defines the abstraction boundary:
  `Isolate`, `Effect`, `Address`, `SendMessage`, `SpawnSpec`,
  `RestartableSpawnSpec`, `CurrentCall`, and supervision policy types.
- `tina-mailbox-spsc` provides a bounded SPSC mailbox with FIFO, drop/accounting
  tests, focused Miri checks, and Loom coverage.
- `tina-runtime-current` proves the single-shard runtime story:
  local send, spawn, stop, panic capture, stale-address rejection,
  parent-child lineage, restartable child records, `RestartChildren`,
  supervised panic restart, runtime-owned TCP calls, runtime-owned timers, and
  a trace-backed TCP echo proof.
- `tina-sim` proves the deterministic simulator story:
  virtual time, replay artifacts, seeded faults, checkers, single-shard
  spawn/supervision replay, and scripted single-shard TCP simulation with echo
  workloads and replayable failures.

## Workspace Crates

- `tina` — trait crate and policy vocabulary
- `tina-mailbox-spsc` — bounded single-producer/single-consumer mailbox
- `tina-supervisor` — supervisor configuration vocabulary
- `tina-runtime-current` — explicit-step single-shard runtime
- `tina-sim` — deterministic single-shard simulator

## Status

**Status: early, real, and still moving.**

The single-shard proof surface is now real:

- runtime-owned TCP bind/accept/read/write/close
- runtime-owned sleep/timer wake
- supervision and restart policy coverage
- deterministic replay artifacts
- seeded simulator faults and checker replay
- single-shard TCP echo simulation

What is still missing:

- multi-shard runtime
- monoio-backed production runtime
- Tokio bridge / adoption adapter
- broader simulation fault model and later hardening work

If you want the detailed plan, read [ROADMAP.md](ROADMAP.md). If you want the
evidence trail, read [CHANGELOG.md](CHANGELOG.md) and the phase reviews under
`.intent/`.

## Non-goals

- Replacing Tokio outright
- Claiming feature parity with Tina-Odin
- Building a magical general-purpose actor framework
- Pretending the current runtime is already the final production story

## Development

```bash
make verify   # fmt + check + test + loom + doc + clippy
make miri     # focused unsafe-memory checks for tina-mailbox-spsc
```

Useful individual targets:

- `make fmt`
- `make check`
- `make test`
- `make loom`
- `make doc`
- `make clippy`

## Further Reading

- [Tina](https://github.com/pmbanugo/tina) — the Odin reference implementation
- [Why async/await complect concurrency](https://pmbanugo.me/blog/why-async-await-complect-concurrency)
- [Seastar](https://seastar.io/) — thread-per-core systems inspiration
- [monoio](https://github.com/bytedance/monoio) — likely future runtime substrate
- [loom](https://github.com/tokio-rs/loom) and [Miri](https://github.com/rust-lang/miri) — proof tools used in this repo

## License

Dual-licensed under MIT or Apache-2.0, at your option.
