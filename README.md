# tina-rs

![tina-rs hero](tina.png)

Tina is a bounded Rust concurrency framework for predictable services:
isolated state machines, visible overload, runtime-owned effects, and
deterministic replay.

Use Tina when the hard parts of the service are not just "do I/O" but "know
where work is queued, what can overload, what was cancelled, what shut down, and
how to replay the failure."

In Tokio, the main unit of concurrency is a `Future`: a function-shaped state
machine that yields at `.await` points. That works well for many I/O-heavy
programs, but it makes several production concerns indirect: where work is
queued, which state may move between threads, which operations are bounded,
what happens after timeout, and how to replay an interleaving.

Tina makes those concerns part of the program model.

The main unit is an isolate: a shard-local state machine with private state.
An isolate handles one message synchronously and returns an `Effect`. Effects
are data: send a message, reply to a caller, sleep, read from a socket, write
to a journal, spawn a child, stop. The runtime interprets effects, owns I/O
and time, and resumes isolates by delivering continuation messages.

The project includes its own runtimes: `tina-runtime` for live execution and
`tina-sim` for deterministic simulation. Tina is a runtime, but not an
async/await runtime. It is a bounded service framework with shard-local
execution, optional thread-per-core shape, runtime-owned I/O, and deterministic
simulation as first-class constraints.

`tina-rs` is an independent Rust implementation inspired by
[Peter Mbanugo's Tina](https://github.com/pmbanugo/tina), a thread-per-core
concurrency framework in Odin, and by his article
[The Tokio/Rayon Trap and Why Async/Await Fails Concurrency](https://pmbanugo.me/blog/why-async-await-complect-concurrency).
The design also rhymes with Erlang/OTP supervision, Seastar-style shard-local
execution, and TigerBeetle-style deterministic testing: keep state owned,
queues bounded, and execution replayable.

> Tina is experimental and in active development. The model is strong enough to
> write real specimen services against; the public API is still moving.

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

Tina is aimed at services where these properties matter more than linear
`async fn` syntax:

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
* **Runtime-owned I/O.** TCP, UDP, DNS, TLS, file I/O, snapshot/journal
  persistence, signals, and process execution flow through typed runtime
  calls. Continuations come back as ordinary messages.
* **Visible cancellation, supervision, and shutdown.** Parent isolates can
  restart children under bounded policy. Calls time out. Late replies,
  cancellations, and terminal shutdown facts are recorded in the runtime trace.
* **Deterministic simulation.** The same isolate code runs under the live
  `ThreadedRuntime` and under `tina-sim` with virtual time, seeded faults,
  saved replay cases, and shrinking. Same seed, same config, same failure.

## What Tina Replaces

Tina is not a drop-in replacement for Tokio's async runtime. It is a different
service shape. In a Tina service:

| Tokio-shaped code often reaches for | Tina's default shape |
|---|---|
| `tokio::spawn` task graphs | named isolates with typed messages |
| `Arc<Mutex<AppState>>` | shard-local state owned by one isolate |
| channels whose backlog is hidden in plumbing | bounded mailboxes with typed `Full` |
| future cancellation by drop/abort | visible timeout, cancel, and late-reply facts |
| logs as the main debugging artifact | runtime trace, capacity reports, and replay cases |

The cost is real: more message variants and more explicit state transitions.
The reward is that overload, shutdown, cancellation, and replay are ordinary
program facts instead of conventions you hope every task follows.

## Where It Hurts

Tina is not optimized for the shortest happy-path handler.

* Linear request code is often more verbose than `async fn`.
* Multi-turn workflows add message variants; the suspension points are named,
  not hidden.
* Capacity must be chosen and reported. There is no secret unbounded escape
  hatch.
* Async ecosystem crates usually enter through explicit bridges, not arbitrary
  futures smuggled into an isolate.
* Some protocol/client surfaces are still first-real-form rather than hardened
  production replacements.

Those costs are deliberate only when they buy something visible: bounded
pressure, typed terminal outcomes, trace facts, or replayable state.

## A TCP echo connection

The connection isolate is a state machine over runtime-owned TCP. The
handler is synchronous; each runtime call returns its result as a normal
message variant.

```rust
use tina::prelude::*;
use tina_runtime::{CallError, StreamId, tcp_close_stream, tcp_read, tcp_write};

#[derive(Debug, Clone)]
enum ConnMsg {
    Begin,
    Read(Result<Vec<u8>, CallError>),
    Wrote(Result<usize, CallError>),
    Closed(Result<(), CallError>),
}

struct Connection {
    stream: StreamId,
}

#[tina_runtime::isolate(message = ConnMsg, shard = AppShard)]
impl Connection {
    fn handle(&mut self, msg: ConnMsg, _ctx: &mut Context<'_, AppShard>) -> Effect<Self> {
        match msg {
            ConnMsg::Begin => tcp_read(self.stream, 4096).then(ConnMsg::Read),
            ConnMsg::Read(Ok(bytes)) if bytes.is_empty() => {
                tcp_close_stream(self.stream).then(ConnMsg::Closed)
            }
            ConnMsg::Read(Ok(bytes)) => tcp_write(self.stream, bytes).then(ConnMsg::Wrote),
            ConnMsg::Wrote(Ok(_)) => tcp_read(self.stream, 4096).then(ConnMsg::Read),
            ConnMsg::Read(Err(_)) | ConnMsg::Wrote(Err(_)) => {
                tcp_close_stream(self.stream).then(ConnMsg::Closed)
            }
            ConnMsg::Closed(_) => stop(),
        }
    }
}
```

Tina exits the handler after each message, runs the I/O on its own driver
rail, and re-enters with the result. The handler stays synchronous; the
runtime owns suspension, cancellation, and shutdown.

For comparison, the same echo loop in Tokio keeps control flow inside an
async task:

```rust
tokio::spawn(async move {
    let mut buf = [0; 4096];
    loop {
        let n = stream.read(&mut buf).await?;
        if n == 0 { break; }
        stream.write_all(&buf[..n]).await?;
    }
});
```

Tokio hides the state machine behind a future. Tina asks you to write the
state machine directly.

That is more verbose for linear request code. It is easier to inspect when
the service has fanout, backpressure, retries, shutdown, supervision, or
state that must stay on one shard.

The full echo server, including the listener that spawns one connection
isolate per accepted socket, lives in
[`tina-runtime/examples/tcp_echo.rs`](tina-runtime/examples/tcp_echo.rs).

## Architecture

```
┌──────────────────────────────────────┐  ┌──────────────────────────────────────┐
│           SHARD 0 (one core)         │  │           SHARD 1 (one core)         │
│                                      │  │                                      │
│  Isolate ── Effect ─→ Runtime        │  │  Isolate ── Effect ─→ Runtime        │
│     ↑                    │           │  │     ↑                    │           │
│     └────── Message ─────┘           │  │     └────── Message ─────┘           │
│                                      │  │                                      │
│  Bounded mailboxes                   │  │  Bounded mailboxes                   │
│  Runtime-owned I/O · Time · Signals  │  │  Runtime-owned I/O · Time · Signals  │
│  Supervision · Persistence · Trace   │  │  Supervision · Persistence · Trace   │
│                                      │  │                                      │
└──────────────────────┬───────────────┘  └───────────────┬──────────────────────┘
                       │                                  │
                       └─── Bounded shard-pair queues ────┘
```

A **shard** owns one lane of service work: its isolates, their mailboxes, the
runtime's driver rails for time and I/O, supervision records, and the
shard-pair queues that connect it to other shards. The design is
thread-per-core-shaped and shared-nothing; hard OS pinning and the remaining
substrate-alignment work are active roadmap items, so the README does not claim
that every helper lane is already pinned to a core.

An **isolate** is a typed struct with a synchronous `handle` method that
returns an `Effect`. Isolates are referenced through typed `Address<M, R>`
values, never raw pointers. Their state is private.

An **effect** is a closed enum the runtime executes. The user surface is
`send`, `reply`, `stop`, `spawn`, `batch`, and the `tina-runtime` call
helpers (`sleep`, `tcp_*`, `udp_*`, `dns_*`, `tls_*`, file/path helpers,
`snapshot_*` / `journal_*`, `process_run`, `signal_wait`).

The repository is a Cargo workspace:

| Crate | Purpose |
|---|---|
| [`tina`](tina/) | Traits, effects, typed addresses, supervision policy types, and the `#[tina::isolate]` / `#[tina_runtime::isolate]` macros. No implementations. |
| [`tina-mailbox-spsc`](tina-mailbox-spsc/) | Bounded single-producer/single-consumer ring-buffer mailbox. |
| [`tina-supervisor`](tina-supervisor/) | Supervisor configuration: `RestartPolicy`, `RestartBudget`, `SupervisorConfig`. |
| [`tina-runtime`](tina-runtime/) | Explicit-step runtime, multi-shard runner, `ThreadedRuntime` over the [Betelgeuse](https://github.com/penberg/betelgeuse) backend, runtime-owned I/O, isolate calls, local snapshot/journal persistence. |
| [`tina-sim`](tina-sim/) | Deterministic simulator with virtual time, seeded faults, scripted I/O, durable images, and replay. |
| [`tina-http`](tina-http/) | Native HTTP/1.1, HTTPS, HTTP/2, gRPC, WebSocket server/client sessions, keepalive pools, body streaming/chunking, protocol facts, and typed pressure/lifecycle reports. |
| [`tina-codec`](tina-codec/) | Open synchronous codec trait for bounded parser/framer adapters. |
| [`tina-rpc`](tina-rpc/) | Framed request/reply, service registry, typed service helpers, and bounded RPC semantics. |
| [`tina-rpc-tokio`](tina-rpc-tokio/) | Tokio async facade over native Tina RPC for ecosystem-edge callers. |
| [`tina-tokio-bridge`](tina-tokio-bridge/) | Bounded ingress from a host Tokio runtime into a Tina service, for axum/Tower/Hyper integration. |
| [`tina-tower-bridge`](tina-tower-bridge/) | Bounded Tower service bridge. |
| [`tina-reqwest-bridge`](tina-reqwest-bridge/) | Bounded reqwest bridge with caller-owned retry/classification and bridge pressure truth. |
| [`tina-sqlite-bridge`](tina-sqlite-bridge/) | First-form SQLite worker around `rusqlite`. One connection, one blocking thread, autocommit only, named caps for mailbox / in-flight / pool / pending replies. |
| [`tina-sqlx-bridge`](tina-sqlx-bridge/) | Bounded SQLx/Postgres bridge with typed values, transactions, fetch-many, cancellation truth, metrics, and pressure reports. |
| [`tina-aws-bridge`](tina-aws-bridge/) | AWS SDK bridge for S3, SQS, SNS, DynamoDB, and Secrets Manager. Service-shaped requests, explicit config, bounded admission/in-flight work, body/message caps, typed errors, metrics, and honest late-result/close-drain semantics. |
| [`tina-tracing`](tina-tracing/) | Runtime trace to `tracing` events and offline Chrome Trace JSON timeline export. |
| [`tina-proof-harness`](tina-proof-harness/) | Load/fairness assertions, bad-peer scenarios, and live-run capture helpers for system specimens. |

End consumers depend on `tina` plus one runtime or simulator crate.

## The rule

If something can overload, Tina makes it visible.

If something can fail, Tina makes it traceable.

If something can race, Tina makes it replayable.

## Explicit by design

Tina keeps the important parts visible.

A handler matches a message and returns one effect. Runtime calls come back as
named continuation messages. Timeouts, `Full`, `Closed`, and mailbox capacity
are in the code you read.

This is not accidental verbosity. Tina accepts helpers that remove boring
bookkeeping. Tina rejects helpers that hide who owns state, what can overload,
where timeout lives, or what message comes next.

That matters for humans, and it matters for LLMs. Copyable local patterns are
safer than clever APIs whose important rules live somewhere else.

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

Same seed. Same config. Same failure.

See [`tina-sim/tests/`](tina-sim/tests/) and
[`docs/tina-user-guide/08-simulation-and-dst.md`](docs/tina-user-guide/08-simulation-and-dst.md)
for replay, DST, and fault-injection details.

## Quickstart

```bash
git clone https://github.com/russellromney/tina-rs.git
cd tina-rs
make verify
```

Run a canonical example:

```bash
cargo run --manifest-path examples/systems/system_copied_service_path/Cargo.toml
```

Run the paired Tokio-vs-Tina comparisons:

```bash
cargo run --manifest-path examples/specimen_mini_keyspace/Cargo.toml -- compare
cargo run --manifest-path examples/specimen_supervised_worker/Cargo.toml -- compare
cargo run --manifest-path examples/specimen_replay_dst/Cargo.toml -- compare
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
specimens for feel and behavior, not a shared harness prison. Cross-cutting
findings live in
[`examples/FINDINGS.md`](examples/FINDINGS.md).

Recent systems are also the best place to see the newer app shapes: gateway
limits, rate-limit policy, realtime WebSocket rooms, job queues, live replay
bugboxes, metrics shipping, and soak-shaped HTTP/DB services. They are supposed
to find rough edges; the findings file is part of the project loop, not a
marketing page.

## Documentation

| Section | What it covers |
|---|---|
| [`docs/tina-user-guide/README.md`](docs/tina-user-guide/README.md) | Full reading order |
| [`docs/tina-user-guide/00-agent-quickstart.md`](docs/tina-user-guide/00-agent-quickstart.md) | Short checklist for humans and coding agents |
| [`docs/tina-user-guide/01-mental-model.md`](docs/tina-user-guide/01-mental-model.md) | The model in one page |
| [`docs/tina-user-guide/02-first-isolate.md`](docs/tina-user-guide/02-first-isolate.md) | Writing your first isolate |
| [`docs/tina-user-guide/03-effects-and-runtime-calls.md`](docs/tina-user-guide/03-effects-and-runtime-calls.md) | Effects and runtime calls |
| [`docs/tina-user-guide/04-request-reply.md`](docs/tina-user-guide/04-request-reply.md) | Request/reply between isolates |
| [`docs/tina-user-guide/05-tcp-services.md`](docs/tina-user-guide/05-tcp-services.md) | TCP services |
| [`docs/tina-user-guide/06-boundedness-and-overload.md`](docs/tina-user-guide/06-boundedness-and-overload.md) | Boundedness and overload |
| [`docs/tina-user-guide/07-supervision.md`](docs/tina-user-guide/07-supervision.md) | Supervision and restart budgets |
| [`docs/tina-user-guide/08-simulation-and-dst.md`](docs/tina-user-guide/08-simulation-and-dst.md) | Simulation and DST |
| [`docs/tina-user-guide/09-tokio-to-tina-porting.md`](docs/tina-user-guide/09-tokio-to-tina-porting.md) | Porting Tokio-shaped code |
| [`docs/tina-user-guide/10-service-patterns.md`](docs/tina-user-guide/10-service-patterns.md) | Service patterns |
| [`docs/tina-user-guide/11-ergonomics-checklist.md`](docs/tina-user-guide/11-ergonomics-checklist.md) | Current ergonomics checklist |
| [`docs/tina-user-guide/12-io-model.md`](docs/tina-user-guide/12-io-model.md) | I/O model |
| [`docs/tina-user-guide/13-outcome-glossary.md`](docs/tina-user-guide/13-outcome-glossary.md) | Outcome glossary |
| [`docs/tina-user-guide/14-lifecycle-and-shutdown.md`](docs/tina-user-guide/14-lifecycle-and-shutdown.md) | Lifecycle and shutdown |
| [`docs/tina-user-guide/15-service-client-worked-example.md`](docs/tina-user-guide/15-service-client-worked-example.md) | Service-client worked example |
| [`docs/tina-user-guide/18-bridge-crates.md`](docs/tina-user-guide/18-bridge-crates.md) | Native-vs-bridge choice |
| [`docs/tina-user-guide/22-http-http2-grpc.md`](docs/tina-user-guide/22-http-http2-grpc.md) | HTTP/2 and gRPC protocol facts |
| [`docs/tina-user-guide/23-core-and-batteries.md`](docs/tina-user-guide/23-core-and-batteries.md) | Core and battery boundaries |
| [`docs/tina-user-guide/25-extension-hooks.md`](docs/tina-user-guide/25-extension-hooks.md) | Extension hooks |
| [`docs/tina-user-guide/26-async-boundary.md`](docs/tina-user-guide/26-async-boundary.md) | Async ecosystem boundary |
| [`docs/tina-user-guide/30-bridge-author-kit.md`](docs/tina-user-guide/30-bridge-author-kit.md) | Bridge author copied path |
| [`ROADMAP.md`](ROADMAP.md) | Phases, near-term work, and explicit non-goals |
| [`CHANGELOG.md`](CHANGELOG.md) | Completed phases |

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
* full thread-per-core substrate alignment: shard-local execution is the design, but optional hard pinning and moving remaining TLS/storage/Unix bypass lanes fully onto the substrate are active work;
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
