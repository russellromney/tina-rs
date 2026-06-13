# tina-rs

![tina-rs hero](tina.png)

Tina is a bounded Rust concurrency framework for services built from isolated
state machines. Each isolate owns its state, handles one message synchronously,
and returns an `Effect`: send, reply, sleep, read from a socket, write to a
journal, spawn a child, stop. The runtime interprets effects, owns scheduling,
I/O, time, supervision, and replay.

Use Tina when the service must choose how much work can be in flight. Queue
capacity, overload, cancellation, shutdown, and replay are typed program facts,
not conventions around async tasks.

The repository includes `tina-runtime` for live execution and `tina-sim` for
deterministic simulation.

`tina-rs` is an independent Rust implementation inspired by
[Peter Mbanugo's Tina](https://github.com/pmbanugo/tina), a thread-per-core
concurrency framework in Odin, and by his article
[The Tokio/Rayon Trap and Why Async/Await Fails Concurrency](https://pmbanugo.me/blog/why-async-await-complect-concurrency).
The design uses ideas from Erlang/OTP supervision, Seastar-style shard-local
execution, and TigerBeetle-style deterministic testing: keep state owned,
queues bounded, and execution replayable.

> Tina is experimental and in active development. The model is strong enough to
> write real specimen services against; the public API is still moving.

## A Bounded Service Step

A Tina handler is a synchronous state-machine step. It consumes one message and
returns one `Effect`. Runtime calls come back later as named messages.

```rust
use std::time::Duration;

use tina::prelude::*;
use tina_runtime::{LocalPermitGate, LocalPermitName, Permit, sleep};

#[derive(Debug)]
enum ApiEvent {
    Finished { id: u64, permit: Permit },
}

#[derive(Debug, Clone)]
enum ApiRequest {
    Request { id: u64 },
}

#[derive(Debug, Clone)]
enum ApiReply {
    Accepted,
    Busy { in_flight: usize, cap: usize },
}

struct Api {
    in_flight: LocalPermitGate,
}

#[tina_runtime::isolate(event = ApiEvent, request = ApiRequest, reply = ApiReply)]
impl Api {
    fn new() -> Self {
        Self {
            in_flight: LocalPermitGate::with_capacity(128)
                .named(LocalPermitName("api.in_flight")),
        }
    }

    fn handle_event(
        &mut self,
        event: ApiEvent,
        _ctx: &mut Context<'_, SingleShard, Self::Reply>,
    ) -> Effect<Self> {
        match event {
            ApiEvent::Finished { id: _, permit } => {
                self.in_flight
                    .release(permit)
                    .expect("permit is released exactly once");
                noop()
            }
        }
    }

    fn handle_request(
        &mut self,
        request: ApiRequest,
        caller: RequestCall<'_, Self>,
    ) -> RequestEffect<Self> {
        match request {
            ApiRequest::Request { id } => match self.in_flight.try_admit() {
                Ok(permit) => caller.reply_and(
                    ApiReply::Accepted,
                    vec![sleep(Duration::from_millis(10)).then(move |_| {
                        tina::ServiceMessage::Event(ApiEvent::Finished { id, permit })
                    })],
                ),
                Err(full) => {
                    let report = full.report();
                    caller.reply(ApiReply::Busy {
                        in_flight: report.current,
                        cap: report.capacity,
                    })
                }
            }
        }
    }
}
```

What is not in the handler:

* no `.await`;
* no unbounded `spawn` loop over the request;
* no `Arc<Mutex<ApiState>>`;
* no application-owned hidden queue;
* no implicit cancellation by dropping a future.

The service, not the request, owns the concurrency bound. When the gate is full,
the caller gets `Busy` with the observed capacity facts.

The same mistake in Tokio-shaped code is easy to write:

```rust
async fn handle(ids: Vec<u64>) {
    for id in ids {
        tokio::spawn(async move {
            process(id).await;
        });
    }
}
```

If `ids` came from a client, the client chose the concurrency. Tina pushes that
choice into service state and makes the refusal path explicit.

## Features Through Constraints

Tina is aimed at services where these constraints are useful:

* **Shard-local ownership.** Connections, sessions, workers, or protocol
  roles each get a typed state machine that owns its data. Shared state is
  possible, but it is not the default shape.
* **Synchronous handlers returning `Effect`.** Handlers do not `await`. They
  return one runtime-interpreted effect: `send`, `reply`, `stop`, `spawn`,
  `batch`, or a runtime call like `sleep`, `tcp_read`, `tcp_write`,
  `snapshot_commit`, or `journal_append`.
* **Bounded mailboxes and visible overload.** Every important queue has a
  capacity. `Full`, `Closed`, and `Timeout` are normal outcomes, not
  exceptions.
* **Runtime-owned explicit-step I/O.** TCP, UDP, DNS, TLS, file I/O,
  snapshot/journal persistence, signals, and process execution flow through
  typed runtime calls. Substrate progress is advanced by the Tina runner,
  continuations come back as ordinary messages, and there is no hidden wake
  side-channel outside the completion/event model.
* **Visible cancellation, supervision, and shutdown.** Parent isolates can
  restart children under bounded policy. Calls time out. Late replies,
  cancellations, and terminal shutdown facts are recorded in the runtime trace.
* **Deterministic simulation.** The same isolate code runs under the live
  `ThreadedRuntime` and under `tina-sim` with virtual time, seeded faults,
  saved replay cases, and shrinking. Same seed, same config, same failure.

## Copyable Service Skeleton

The current copied path starts with
[`examples/systems/system_copied_service_path`](examples/systems/system_copied_service_path/).
It is intentionally smaller than a product: one readable service shape that
shows request entry, admission, durable state, session control, fairness/load
assertions, live capture, and replay.

Run it from the repo root:

```sh
cargo run --manifest-path examples/systems/system_copied_service_path/Cargo.toml
cargo test --manifest-path examples/systems/system_copied_service_path/Cargo.toml
```

For a larger service-shaped system, see
[`examples/systems/mini_saas_api`](examples/systems/mini_saas_api/): native
`tina-http`, a controller isolate, `tina-sqlite-bridge`, an outbound keepalive
pool, health/readiness, graceful shutdown, capacity reporting, and a
live-replay fact.

## Programming Model Compared With Tokio

Tina is not a drop-in replacement for Tokio's async runtime. It is a different
service architecture. In a Tina service:

| Tokio-shaped code often reaches for | Tina's default shape |
|---|---|
| `tokio::spawn` task graphs | named isolates with typed messages |
| `Arc<Mutex<AppState>>` | shard-local state owned by one isolate |
| channels whose backlog is hidden in plumbing | bounded mailboxes with typed `Full` |
| future cancellation by drop/abort | visible timeout, cancel, and late-reply facts |
| logs as the main debugging artifact | runtime trace, capacity reports, and replay cases |

The cost is more message variants and more explicit state transitions. The
result is that overload, shutdown, cancellation, and replay are represented as
typed program facts instead of out-of-band conventions.

## Tradeoffs

Tina is not optimized for the shortest happy-path handler.

* Linear request code is often more verbose than `async fn`.
* Multi-turn workflows add message variants; the suspension points are named,
  not hidden.
* Capacity must be chosen and reported. There is no secret unbounded escape
  hatch.
* Async ecosystem crates usually enter through explicit bridges, not arbitrary
  futures smuggled into an isolate.
* Some protocol/client surfaces are initial implementations rather than hardened
  production replacements.

Those costs are justified only when they surface a concrete fact: bounded
pressure, typed terminal outcomes, trace events, or replayable state.

## Architecture

```
┌──────────────────────────────────────┐  ┌──────────────────────────────────────┐
│           SHARD 0 WORKER             │  │           SHARD 1 WORKER             │
│                                      │  │                                      │
│  Mailbox ─→ Isolate ─→ Effect        │  │  Mailbox ─→ Isolate ─→ Effect        │
│     ↑           │          │         │  │     ↑           │          │         │
│     └─ Message ─┘          │         │  │     └─ Message ─┘          │         │
│                            ↓         │  │                            ↓         │
│  Runtime scheduler + driver rails    │  │  Runtime scheduler + driver rails    │
│                                      │  │                                      │
│  Timers · Betelgeuse I/O · Signals   │  │  Timers · Betelgeuse I/O · Signals   │
│  Persistence · Trace · Capacity      │  │  Persistence · Trace · Capacity      │
│  Supervision · Lifecycle reports     │  │  Supervision · Lifecycle reports     │
│                                      │  │                                      │
└──────────────────────┬───────────────┘  └───────────────┬──────────────────────┘
                       │                                  │
                       └─── Bounded shard-pair queues ────┘
```

A **shard** owns one lane of service work: its isolates, their mailboxes, the
runtime's driver rails for time and I/O, supervision records, and the
shard-pair queues that connect it to other shards. The design is
thread-per-core-inspired and shared-nothing, but worker/core affinity is an
explicit capability rather than a blanket guarantee; hard OS pinning and the
remaining substrate-alignment work are active roadmap items.

An **isolate** is a typed struct with a synchronous `handle` method that
returns an `Effect`. Isolates are referenced through typed `Address<M, R>`
values, never raw pointers. Their state is private.

An **effect** is a closed enum the runtime executes. The user surface is
`send`, `reply`, `stop`, `spawn`, `batch`, and the `tina-runtime` call
helpers (`sleep`, `tcp_*`, `udp_*`, `dns_*`, `tls_*`, file/path helpers,
`snapshot_*` / `journal_*`, `process_run`, `signal_wait`).

## Design Invariants

Tina's APIs are organized around three invariants:

* queues and resource lanes are bounded or explicitly reported as unsupported;
* terminal outcomes (`Full`, `Closed`, `Timeout`, cancellation, late reply,
  rejection) are typed values;
* runtime behavior is traceable, and simulator-supported behavior is replayable
  from explicit seed/config/history.

The replay is of *logical* interleavings — message, timer, and completion
order — which is what the single-threaded simulator proves and reproduces
byte-for-byte. Physical memory-ordering races are a separate, tiny surface:
shared-nothing isolates keep it small, and the custom lock-free structures on
it (the SPSC mailbox and `SharedCapacityScope`) are loom-checked, not assumed
safe. The live parallel runtime is fully introspectable but not
byte-reproducible. See [`.intent/SYSTEM.md`](.intent/SYSTEM.md) for the verified
race surface.

## Explicit by design

Tina keeps the important parts visible.

A handler matches a message and returns one effect. Runtime calls come back as
named continuation messages. Timeouts, `Full`, `Closed`, and mailbox capacity
are in the code you read.

This is not accidental verbosity. Helpers may remove repeated bookkeeping, but
they must keep state ownership, capacity, timeout, and continuation messages
visible.

That matters for humans, and it matters for LLMs. Copyable local patterns are
safer than APIs whose important rules live outside the call site.

### Cancelable deferred calls

`CallContext::defer(work).reply(...)` is the blessed helper for ordinary
multi-turn replies. Cancelable multi-turn replies use a bounded admission helper
so caller authority is stored before child work can be dispatched:

```rust
match call_ctx
    .defer_cancelable(call_cancelable(worker, WorkerMsg::Run(job), timeout))
    .try_admit(&mut self.pending, job_id, Msg::WorkerReturned)
{
    Ok(effect) => effect,
    Err(PendingCancelableInsertError::Full { token }) => {
        reply_to_request(token.into_request_context(), Reply::Busy)
    }
    Err(PendingCancelableInsertError::DuplicateKey { token }) => {
        reply_to_request(token.into_request_context(), Reply::Duplicate)
    }
}
```

The key is user vocabulary (`job_id`, `worker_slot`, `request_id`). The ticket
carried into `Msg::WorkerReturned` is the exact admitted instance, so stale
completions cannot remove a newer call under the same key.

## Deterministic simulation testing

`tina-sim` runs the same isolate code as `tina-runtime` under a deterministic
driver. Time is virtual, I/O can be scripted or seeded, and the trace of every
run is reproducible from a `(seed, config)` pair.

```rust
use tina_sim::{Simulator, SimulatorConfig};

let mut sim = Simulator::new(AppShard, SimulatorConfig { seed: 42, ..Default::default() });
let counter = sim.register_with_mailbox_capacity(Counter::default(), 16);
sim.try_send(counter, CounterMsg::Increment).unwrap();
sim.run_until_quiescent();

assert_eq!(sim.trace().len(), expected_event_count);
```

A run with the same seed and config produces a byte-identical trace. A
different seed exercises different timer-wake ordering, send delivery
order, and TCP completion order under bounded fault models. Live and
simulated runs share the same handler code; the difference is the driver
underneath.

The determinism boundary is the Tina I/O model: messages, timers, typed runtime
calls, completions, pressure, cancellation, and seeded substrate faults. Live
kernel drivers are tested separately, but they must present progress to Tina as
explicit-step completion/event work, not as executor tasks or wake callbacks
that bypass the model.

See [`tina-sim/tests/`](tina-sim/tests/) and
[`docs/tina-user-guide/08-simulation-and-dst.md`](docs/tina-user-guide/08-simulation-and-dst.md)
for replay, DST, and fault-injection details.

## Quickstart

```bash
git clone https://github.com/russellromney/tina-rs.git
cd tina-rs
make verify
```

Run the smallest service-shaped example:

```bash
cargo run -p tina-runtime --example task_dispatcher
```

Run runtime-owned TCP:

```bash
cargo run -p tina-runtime --example tcp_echo
```

Run the current copied service skeleton:

```bash
cargo run --manifest-path examples/systems/system_copied_service_path/Cargo.toml
```

Run a saved replay/DST comparison:

```bash
cargo run --manifest-path examples/specimen_replay_dst/Cargo.toml -- compare
```

Run the paired Tokio-vs-Tina comparisons:

```bash
cargo run --manifest-path examples/specimen_mini_keyspace/Cargo.toml -- compare
cargo run --manifest-path examples/specimen_supervised_worker/Cargo.toml -- compare
```

`make` targets:

| Command | Purpose |
|---|---|
| `make verify` | Full project gate: fmt, clippy, tests, miri, simulator, cost smoke. |
| `make portable-runtime-cost` | Optional local cost-smoke rows. Not a benchmark. |
| `make miri` | Focused unsafe-memory checks for `tina-mailbox-spsc`. |
| `make fmt` / `make check` / `make test` / `make clippy` / `make doc` | Individual targets. |

## Examples

| Example | What it shows |
|---|---|
| [`examples/systems/system_copied_service_path`](examples/systems/system_copied_service_path/) | Current copied service skeleton: request entry, limits, durable state, session control, fairness/load proof, live capture, replay, join/select helpers. |
| [`examples/systems/mini_saas_api`](examples/systems/mini_saas_api/) | Larger service-shaped system with native HTTP, SQLite bridge, outbound keepalive pool, readiness, shutdown, capacity reporting, and live-replay fact. |
| [`tina-runtime/examples/task_dispatcher.rs`](tina-runtime/examples/task_dispatcher.rs) | Smallest complete service: dispatcher isolate, worker isolates, supervision. Recommended starting point. |
| [`tina-runtime/examples/tcp_echo.rs`](tina-runtime/examples/tcp_echo.rs) | Runtime-owned TCP from listener through connection close, including bounded multi-client overlap. |
| [`tina-tokio-bridge/examples/llama_bridge.rs`](tina-tokio-bridge/examples/llama_bridge.rs) | Bridging an existing Tokio/axum app into a Tina-supervised core. |

The [`examples/`](examples/) directory at the repo root contains **specimens**:
paired Tokio-vs-Tina implementations of common service shapes (chat fanout,
key/value store, axum counter, supervised worker, persistent counter,
deterministic replay, outbound fetch, graceful shutdown, and more). They are
specimens for comparing shape and behavior, not a shared framework. Cross-cutting
findings live in
[`examples/FINDINGS.md`](examples/FINDINGS.md).

Recent systems show newer app shapes: gateway limits, rate-limit policy,
realtime WebSocket rooms, job queues, live replay bug capture, metrics shipping,
and soak-shaped HTTP/DB services. These examples are used to find rough edges;
the findings file is part of the development loop, not a release document.

## Documentation

The user guide starts at [`docs/tina-user-guide/README.md`](docs/tina-user-guide/README.md).
Project direction lives in [`ROADMAP.md`](ROADMAP.md), and completed work lives
in [`CHANGELOG.md`](CHANGELOG.md).

## Status and limits

Implemented today: explicit-step single-shard runtime, multi-shard runner with
bounded shard-pair queues, `ThreadedRuntime` over Betelgeuse, runtime-owned
TCP/UDP/DNS/TLS/file/path/process/signal/persistence rails, isolate calls with
mandatory timeout, native HTTP/1.1, native HTTP/2 server/client, native gRPC
server/client streaming modes, native WebSocket server/client sessions, framed
RPC with typed service helpers, supervision with `OneForOne`/`OneForAll`/
`RestForOne` and lifetime/windowed restart budgets, cross-shard child ownership
inside the local multi-shard runtime, terminal shutdown reports with topology
and trace, Chrome Trace JSON timeline export, `tina-sim` with virtual time /
seeded faults / scripted I/O / replay / DST shrinking, live-run capture helpers,
capacity/fairness/load proof harnesses, and bounded bridge crates for Tokio,
Tower, reqwest, SQLite, SQLx/Postgres, and AWS SDK work.

Not yet:

* pooled/reconnecting native WebSocket client managers, HTTP/2 mTLS, gRPC reflection/interceptors/load balancing, and pooled production gRPC clients;
* native database wire clients (PG wire / SQLite-native runtime rail); current paths are `tina-sqlite-bridge` over `rusqlite` and `tina-sqlx-bridge` over SQLx/Postgres;
* full thread-per-core substrate alignment: shard-local execution is the design, but hard pinning remains an explicit capability and moving remaining TLS/storage/Unix bypass lanes fully onto the substrate is active work;
* broad Linux performance claim; Linux already uses Betelgeuse's native backend;
* remoting or clustering;
* production performance claim;
* a stable public API.

The repository ships an honest capability report at runtime
(`RuntimeCapabilities`) and explicit non-claims for resources the current
backend cannot prove.

## Prior art

Tina synthesises ideas that are not original to it. The list below names the
sources, not order of discovery.

| Idea | Source |
|---|---|
| Supervision trees, "let it crash", error kernel | Joe Armstrong — Erlang/OTP |
| Thread-per-core, shared-nothing reactor | [Seastar](https://seastar.io/) by ScyllaDB |
| Deterministic simulation testing | [TigerBeetle](https://tigerbeetle.com/) and [FoundationDB](https://www.youtube.com/watch?v=4fFDFbi3toc) |
| Synchronous-handler + Effect programming model | [Peter Mbanugo's Tina](https://github.com/pmbanugo/tina) |
| Bounded SPSC ring buffers | LMAX Disruptor lineage |
| Rust thread-per-core substrates | [monoio](https://github.com/bytedance/monoio), [glommio](https://github.com/DataDog/glommio), [Betelgeuse](https://github.com/penberg/betelgeuse) |
| Concurrency model checking in Rust | [loom](https://github.com/tokio-rs/loom), [shuttle](https://github.com/awslabs/shuttle) |
| Deterministic-simulation libraries in Rust | [madsim](https://github.com/madsim-rs/madsim), [turmoil](https://github.com/tokio-rs/turmoil) |
| OTP-style frameworks in Rust | [ambitious](https://github.com/scrogson/ambitious), [joerl](https://github.com/am-kantox/joerl), [lunatic](https://github.com/lunatic-solutions/lunatic) |
| Memory-safety stress | [Miri](https://github.com/rust-lang/miri) |

## License

Dual-licensed under MIT or Apache-2.0, at your option.
