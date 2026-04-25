# tina-rs

`tina-rs` is a Rust library for building servers out of independent state machines. Each tenant, connection, room, or session is its own struct with its own message queue, and they never share memory. Handlers process one message at a time and return a value describing what to do next — `Send this`, `Spawn that`, `Reply with X`, `Stop`. The runtime is the only thing that actually does I/O. Because handlers are descriptions of work rather than the work itself, the whole system can be driven deterministically in tests: every failure becomes a seed you can replay.

It's a port of [Peter Banugo's Tina](https://github.com/pmbanugo/tina), and the motivation lives in his article [Why async/await complect concurrency](https://pmbanugo.me/blog/why-async-await-complect-concurrency) — read that first. This README assumes you have.

This repo is a Cargo workspace. Today it has one crate:

- **`tina`** — Trait crate. `Isolate`, `Effect`, `Mailbox`, `Shard`, `Context`, `Address`, `SendMessage`, `SpawnSpec`. No impls.

The runtime, mailbox, supervisor, and simulator crates land in later phases. See [ROADMAP.md](ROADMAP.md).

> tina-rs is **experimental**. Phase Sputnik ships only the vocabulary; nothing runs yet. The API will change.

## Why this exists

The default async tools in Rust make it easy to write a server that works fine on a laptop and falls over under real load. Banugo's article walks through the reasons; the short version:

- **The Tokio scheduler moves tasks between cores ("work-stealing").** Every time a task moves, the cache lines it was using on the old core are useless. For servers where each connection or tenant has its own state, that movement is pure waste.
- **`mpsc::unbounded_channel` turns traffic spikes into out-of-memory crashes.** A producer that briefly outpaces a consumer fills memory until the process dies.
- **A blocking call on a Tokio worker stalls everything that worker was juggling.** One slow SQLite query or one cgo call can pause unrelated tasks for hundreds of milliseconds.
- **`Arc<Mutex<…>>` is a graveyard.** Once you reach for it, you've accepted that several units are sharing state, and the lock is going to be where every weird latency spike comes from.

`tina-rs` enforces a different shape:

- **One state machine per unit.** Each tenant or connection is a typed struct (an `Isolate`) with one message type and one queue.
- **Handlers are synchronous and return descriptions of work.** `fn handle(msg) -> Effect`. The handler never does I/O; it returns a value like "send this message" or "spawn this child" or "stop me." The runtime executes the description.
- **Queues are bounded with explicit `Full` and `Closed` errors.** Backpressure is something the application sees and handles, not a leak that builds quietly.
- **One OS thread per core, pinned, no stealing.** Each shard owns a fixed set of isolates. Work doesn't move between cores. Cache stays warm.
- **The whole runtime is replayable.** Because handlers are descriptions and the runtime is the only thing that touches I/O or time, a test harness can drive the system from a seed and reproduce any failure.

None of this is new — Erlang, Akka, and [Seastar](https://seastar.io/) all do versions of it. `tina-rs` is these patterns expressed as Rust traits and a small set of impl crates.

## Why not write a new runtime

Because thread-per-core runtimes for Rust already exist. [monoio](https://github.com/bytedance/monoio) is io_uring-based and actively maintained. [glommio](https://github.com/DataDog/glommio) is the Datadog version. `tokio::runtime::Builder::new_current_thread` gives you the same single-threaded shape inside the existing async ecosystem. The hard part isn't building a scheduler — it's the discipline above, and that layer is portable across runtimes. `tina-rs` is the discipline; the scheduler is whatever you pick.

If you want to contribute or find bugs, please open a PR or issue.

## At a glance

Phase Sputnik lets you define isolates against the API surface even though no runtime exists yet. Handlers describe what to do; they don't do it.

```rust
use std::convert::Infallible;
use tina::{Address, Context, Effect, Isolate, SendMessage, Shard, ShardId};

#[derive(Clone, Copy)]
enum CounterMsg { Add(u64), Read }

#[derive(Clone, Copy)]
enum AuditMsg { Total(u64) }

struct InlineShard;
impl Shard for InlineShard {
    fn id(&self) -> ShardId { ShardId::new(0) }
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
    type Shard = InlineShard;

    fn handle(&mut self, msg: Self::Message, _ctx: &mut Context<'_, Self::Shard>) -> Effect<Self> {
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

`Effect` is a closed enum (`Noop`, `Reply`, `Send`, `Spawn`, `Stop`, `RestartChildren`) with per-isolate associated payload types. The runtime — when it arrives in Phase Mariner — is the only place real I/O happens.

## Design

The discipline is small enough to list on one page:

| Idea | What it means | Why |
|------|---------------|-----|
| **Thread-per-core, no work-stealing** | One OS thread per core, pinned. Each shard owns a fixed set of isolates. | Work-stealing destroys cache locality. Pinning preserves it. |
| **Isolate-per-entity** | Tenants, connections, sessions each get a typed state machine in a pre-allocated arena. No `Arc<Mutex<…>>` spaghetti. | Local state is local. No lock contention, no false sharing. |
| **Effect-returning handlers** | Handlers are synchronous: `fn handle(&mut self, msg, ctx) -> Effect`. The runtime executes the effect. | Pure-ish handlers are deterministic. Determinism enables simulation. |
| **Bounded mailboxes** | SPSC ring buffers with `try_send`. `Full` and `Closed` are explicit errors. | Unbounded queues turn spikes into OOMs. |
| **Supervision trees** | Parent isolates watch children; restart policies (`one-for-one`, `one-for-all`, `rest-for-one`) with budgets. | Failures stay local. Restart budgets keep crash loops from cascading. |
| **Deterministic simulation** | Time, I/O, and message-arrival order are controlled by a seeded test harness. | Failures become reproducible artifacts, not phantoms. |

None of these ideas are new — Erlang, Akka, [Seastar](https://seastar.io/), and Tina-Odin all do versions of this. `tina-rs` collects them into Rust traits plus a small set of impl crates.

## Status

Phases land in order. Each one is proved out by tests against the abstraction, not just by a runnable example:

- **Sputnik** — trait crate (`tina`) ✅
- **Pioneer** — SPSC mailbox + supervision trees, loom-tested
- **Mariner** — single-shard runtime on `tokio::runtime::Builder::new_current_thread` + working TCP echo
- **Voyager** — deterministic simulator
- **Galileo** — multi-shard runtime on monoio (io_uring)
- **Cassini** — Tokio-bridge for incremental adoption + production hardening

See [ROADMAP.md](ROADMAP.md) for what each phase delivers and how it gets proven.

## Non-goals

- A new runtime competing with Tokio or monoio. Use what exists.
- Full feature parity with Tina-Odin. We port the *shape*, not every primitive.
- "Replacing Tokio." This is a discipline layer that rides on top of any thread-per-core runtime.
- FFI to Tina-Odin. Two runtimes fighting for cores would be the worst of both worlds.

## Development

```bash
make verify   # fmt + check + test + doc + clippy
```

Individual targets: `make fmt`, `make check`, `make test`, `make doc`, `make clippy`.

## Prior art and references

- [Tina](https://github.com/pmbanugo/tina) — Banugo's reference implementation in Odin
- [Why async/await complect concurrency](https://pmbanugo.me/blog/why-async-await-complect-concurrency) — the framing
- [Seastar](https://seastar.io/) — C++ thread-per-core framework, ScyllaDB's foundation
- [monoio](https://github.com/bytedance/monoio) — likely runtime backend
- [shuttle](https://github.com/awslabs/shuttle) — concurrency model checking, useful for the simulator phase
- [loom](https://github.com/tokio-rs/loom) — concurrency permutation testing for unsafe primitives

## License

Dual-licensed under MIT or Apache-2.0, at your option.
