# tina-rs

![tina-rs hero](tina.png)

`tina-rs` is a Rust library for building servers out of independent state machines. Each tenant, connection, room, or session is its own struct with its own message queue, and they never share memory. Handlers process one message at a time and return a value describing what to do next — `Send this`, `Spawn that`, `Reply with X`, `Stop`. The runtime is the only thing that actually does I/O. Because handlers are descriptions of work rather than the work itself, the whole system can be driven deterministically in tests: every failure becomes a seed you can replay.

It's an independent Rust port inspired by [Peter Mbanugo's Tina](https://github.com/pmbanugo/tina), and the motivation lives in his article [Why async/await complect concurrency](https://pmbanugo.me/blog/why-async-await-complect-concurrency) which you should read first, then check out the Odin reference impl, then come back here.

This repo is a Cargo workspace. Today it has four crates:

- **`tina`** — trait crate. `Isolate`, `Mailbox`, `Address`, `ChildDefinition`, `Outbound`, and the common helpers in `tina::prelude::*`.
- **`tina-mailbox-spsc`** — bounded single-producer/single-consumer mailbox implementation.
- **`tina-supervisor`** — supervisor policy/config vocabulary.
- **`tina-runtime`** — deterministic single-shard runtime with trace events, local send/spawn dispatch, runtime-owned calls, stop-and-abandon behavior, panic capture, parent-child lineage, and supervised panic restart.

The timer simulator, runtime-owned TCP/time path, multi-shard runtime, and
Tokio bridge all have their own place on the roadmap. See [ROADMAP.md](ROADMAP.md).

> tina-rs is **experimental**. The vocabulary, bounded SPSC mailbox,
> supervisor config, timer simulator, and first single-shard runtime are here.
> The broader simulator and production runtime story are still in flight. The
> API will change.

## Why this exists

The default async tools in Rust make it easy to write a server that works fine on a laptop and falls over under real load. Mbanugo's article walks through the reasons; the short version:

- **The Tokio scheduler moves tasks between cores ("work-stealing").** Every time a task moves, the cache lines it was using on the old core are useless. For servers where each connection or tenant has its own state, that movement is pure waste.
- **`mpsc::unbounded_channel` turns traffic spikes into out-of-memory crashes.** A producer that briefly outpaces a consumer fills memory until the process dies.
- **A blocking call on a Tokio worker stalls everything that worker was juggling.** One slow SQLite query or one cgo call can pause unrelated tasks for hundreds of milliseconds.
- **`Arc<Mutex<…>>` is a graveyard.** Once you reach for it, you've accepted that several units are sharing state, and the lock is going to be where every weird latency spike comes from.

`tina-rs` enforces a different set of ideas:

- **One state machine per unit.** Each tenant or connection is a typed struct (an `Isolate`) with one message type and one queue.
- **Handlers are synchronous and return descriptions of work.** `fn handle(msg) -> Effect`. The handler never does I/O; it returns a value like "send this message" or "spawn this child" or "stop me." The runtime executes the description.
- **Queues are bounded with explicit `Full` and `Closed` errors.** Backpressure is something the application sees and handles, not a leak that builds quietly.
- **One OS thread per core, pinned, no stealing.** Each shard owns a fixed set of isolates. Work doesn't move between cores. Cache stays warm.
- **The whole runtime is replayable.** Because handlers are descriptions and the runtime is the only thing that touches I/O or time, a test harness can drive the system from a seed and reproduce any failure.

None of this is new: Erlang, Akka, and [Seastar](https://seastar.io/) all do versions of it. `tina-rs` is these patterns expressed as Rust traits and a small set of impl crates.

## Why not write a new runtime?

Thread-per-core runtimes for Rust already exist. [monoio](https://github.com/bytedance/monoio) is io_uring-based and actively maintained. [glommio](https://github.com/DataDog/glommio) is the Datadog version. `tokio::runtime::Builder::new_current_thread` gives you the same single-threaded idea inside the existing async ecosystem. The challenge lies in the discipline above rather than the scheduler, and that layer is portable across runtimes. `tina-rs` is the discipline; the scheduler is whatever you pick.

If you want to contribute or find bugs, please open a PR or issue.

## What Tina code looks like

The important shape is simple: one struct owns one unit of state, one message
arrives, and the handler returns the next thing to do.

Use the prelude:

```rust
use tina::prelude::*;
```

First, a tiny local-state example. Imagine one session isolate. It keeps a set
of connected users. It can be told that a user connected. It can be asked for
a snapshot of its current users. And when a user connects, it also sends an
audit event somewhere else. That looks like this:

```rust
enum AuditEvent {
    UserJoined(UserId),
}

enum Message {
    UserConnected(UserId),
    SnapshotRequested,
}

match msg {
    Message::UserConnected(user_id) => {
        self.users.insert(user_id);
        send(self.audit, AuditEvent::UserJoined(user_id))
    }
    Message::SnapshotRequested => reply(self.users.clone()),
}
```

Second, a tiny runtime-owned time example. Imagine one worker isolate that
starts some work, needs a backoff delay, and then retries. The isolate does
not sleep by itself. It asks the runtime to sleep, and later the runtime sends
back one ordinary message saying whether that sleep finished or failed. That
looks like this:

```rust
use tina_runtime::{CallError, sleep};

enum Message {
    Start,
    RetryNow,
    BackoffFailed(CallError),
}

match msg {
    Message::Start => sleep(self.backoff).reply(|result| match result {
        Ok(()) => Message::RetryNow,
        Err(reason) => Message::BackoffFailed(reason),
    }),
    Message::RetryNow => send(self.worker, WorkerEvent::Retry),
    Message::BackoffFailed(_) => stop(),
}
```

Runtime-owned TCP uses the same shape too. The state machine is bigger, but
the idea stays the same: read some bytes, write them back, keep draining if
the write is partial, then re-arm the next read.

The full runnable example lives in
[`tcp_echo.rs`](/Users/russellromney/.codex/worktrees/3ac3/tina-rs/tina-runtime-current/examples/tcp_echo.rs).

That is the whole vibe. Handlers stay synchronous. Local state stays local.
`send`, `reply`, `spawn`, `stop`, and `batch` are plain returned values. Time
and I/O happen in the runtime. Completions come back later as ordinary
messages. The code stays honest about things Tokio usually hides, like retries
and partial writes.

Full runnable examples live here:

- [`task_dispatcher.rs`](/Users/russellromney/.codex/worktrees/3ac3/tina-rs/tina-runtime-current/examples/task_dispatcher.rs)
- [`tcp_echo.rs`](/Users/russellromney/.codex/worktrees/3ac3/tina-rs/tina-runtime-current/examples/tcp_echo.rs)

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

What works today: the trait crate, the bounded SPSC mailbox crate, supervisor
configuration, the single-shard runtime, and the timer simulator. You can
write isolates against the preferred public API, exercise real mailbox
semantics, run handlers through `tina-runtime`, use runtime-owned TCP and
runtime-owned time, and replay timer-driven behavior through `tina-sim`.

What's coming next, in order: broader simulator coverage beyond timers; a
multi-shard runtime; and then an adapter that lets a tina isolate run inside
an existing Tokio app, so codebases can adopt the discipline incrementally
without making Tokio the core model.

See [ROADMAP.md](ROADMAP.md) for what each step delivers and how it gets proven.

## Non-goals

- A new runtime competing with Tokio or monoio. Use what exists.
- Full feature parity with Tina-Odin. We port the *shape*, not every primitive.
- "Replacing Tokio." This is a discipline layer that rides on top of any thread-per-core runtime.
- FFI to Tina-Odin. Two runtimes fighting for cores would be the worst of both worlds.

## Development

```bash
make verify   # fmt + check + test + loom + doc + clippy
make miri     # focused unsafe-memory checks for tina-mailbox-spsc
```

Individual targets: `make fmt`, `make check`, `make test`, `make doc`, `make clippy`.
Concurrency model checking: `make loom`. Unsafe-memory checking for the SPSC
mailbox: `make miri`.

## Prior art and references

- [Tina](https://github.com/pmbanugo/tina) — Mbanugo's reference implementation in Odin
- [Why async/await complect concurrency](https://pmbanugo.me/blog/why-async-await-complect-concurrency) — the framing
- [Seastar](https://seastar.io/) — C++ thread-per-core framework, ScyllaDB's foundation
- [monoio](https://github.com/bytedance/monoio) — likely runtime backend
- [shuttle](https://github.com/awslabs/shuttle) — concurrency model checking, useful as the simulator grows beyond timer coverage
- [loom](https://github.com/tokio-rs/loom) — concurrency permutation testing for unsafe primitives
- [Miri](https://github.com/rust-lang/miri) — interpreter for catching undefined behavior in unsafe Rust

## License

Dual-licensed under MIT or Apache-2.0, at your option.
