# tina-rs

![tina-rs hero](tina.png)

Tina is a Rust framework for writing concurrent services as small synchronous
state machines. Each isolate owns its state, processes one message at a time,
and returns an `Effect`. The runtime owns scheduling, time, I/O, supervision,
and replay.

Tina is not an async I/O runtime like Tokio or monoio. It is the concurrency
model above the runtime substrate. Today `tina-runtime` runs on an explicit-step
oracle and a threaded runtime backed by
[Pekka Enberg's Betelgeuse](https://github.com/penberg/betelgeuse); future
backends (`io_uring`, monoio, glommio) can ride underneath if they preserve
the contract.

It is an independent Rust implementation inspired by
[Peter Mbanugo's Tina](https://github.com/pmbanugo/tina) and by thread-per-core
systems like [Seastar](https://seastar.io/). The motivation comes from
Mbanugo's article
[The Tokio/Rayon Trap and Why Async/Await Fails Concurrency](https://pmbanugo.me/blog/why-async-await-complect-concurrency).

> Tina is very experimental and in active development. The model is stable
> enough to write services against; the public API surface is not.

Tina provides:

* **Isolate-per-entity state machines.** Connections, sessions, workers, or
  protocol roles each get a typed state machine that owns its data. No
  `Arc<Mutex<_>>`.
* **Synchronous handlers returning `Effect`.** Handlers don't `await`. They
  return one of: `send`, `reply`, `stop`, `spawn`, `batch`, or a runtime call
  like `sleep`, `tcp_read`, `tcp_write`, `snapshot_commit`, or `journal_append`.
* **Bounded mailboxes.** Every queue has a capacity. `Full`, `Closed`, and
  `Timeout` are normal outcomes, not exceptions.
* **Runtime-owned I/O.** TCP, UDP, DNS, TLS, file I/O, snapshot/journal
  persistence, signals, and process execution flow through typed runtime
  calls. Continuations come back as ordinary messages.
* **Supervision with restart budgets.** Parent isolates restart children
  under `OneForOne`, `OneForAll`, or `RestForOne` policy with a finite
  budget. Restart events are typed entries in the runtime trace.
* **Deterministic simulation.** The same isolate code runs under the live
  `ThreadedRuntime` and under `tina-sim` with virtual time, seeded faults,
  and replay. Same seed, same config, same failure.

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
            ConnMsg::Begin => tcp_read(self.stream, 4096).reply(ConnMsg::Read),
            ConnMsg::Read(Ok(bytes)) if bytes.is_empty() => {
                tcp_close_stream(self.stream).reply(ConnMsg::Closed)
            }
            ConnMsg::Read(Ok(bytes)) => tcp_write(self.stream, bytes).reply(ConnMsg::Wrote),
            ConnMsg::Wrote(Ok(_)) => tcp_read(self.stream, 4096).reply(ConnMsg::Read),
            ConnMsg::Read(Err(_)) | ConnMsg::Wrote(Err(_)) => {
                tcp_close_stream(self.stream).reply(ConnMsg::Closed)
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

Tokio suspends the function across each `.await`. Tina returns to the
runtime between effects. Both are correct; the difference is whether
suspension points are syntactic (`.await`) or structural (one match arm
per resumption).

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

A **shard** owns one core's worth of work: its isolates, their mailboxes, the
runtime's driver rails for time and I/O, supervision records, and the
shard-pair queues that connect it to other shards. Shards share no memory.

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
| [`tina-http`](tina-http/) | First-form native HTTP/1.1 server/client/pool pieces built as Tina state machines. |
| [`tina-rpc`](tina-rpc/) | First-form framed request/reply, service registry, typed service helpers, and bounded RPC semantics. |
| [`tina-rpc-tokio`](tina-rpc-tokio/) | Tokio async facade over native Tina RPC for ecosystem-edge callers. |
| [`tina-tokio-bridge`](tina-tokio-bridge/) | Bounded ingress from a host Tokio runtime into a Tina service, for axum/Tower/Hyper integration. |

End consumers depend on `tina` plus one runtime or simulator crate.

## The rule

If something can overload, Tina makes it visible.

If something can fail, Tina makes it traceable.

If something can race, Tina makes it replayable.

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
cargo run --example tcp_echo -p tina-runtime
```

Run the paired Tokio-vs-Tina comparisons:

```bash
cargo run --manifest-path examples/eiffel_mini_keyspace/Cargo.toml -- compare
cargo run --manifest-path examples/eiffel_supervised_worker/Cargo.toml -- compare
cargo run --manifest-path examples/eiffel_replay_dst/Cargo.toml -- compare
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
| [`tina-runtime/examples/task_dispatcher.rs`](tina-runtime/examples/task_dispatcher.rs) | Smallest complete service: dispatcher isolate, worker isolates, supervision. Recommended starting point. |
| [`tina-runtime/examples/tcp_echo.rs`](tina-runtime/examples/tcp_echo.rs) | Runtime-owned TCP from listener through connection close, including bounded multi-client overlap. |
| [`tina-tokio-bridge/examples/llama_bridge.rs`](tina-tokio-bridge/examples/llama_bridge.rs) | Bridging an existing Tokio/axum app into a Tina-supervised core. |

The [`examples/`](examples/) directory at the repo root contains **Eiffel**:
paired Tokio-vs-Tina implementations of common service shapes (chat fanout,
key/value store, axum counter, supervised worker, persistent counter,
deterministic replay, outbound fetch, graceful shutdown, and more). They are
specimens for feel and behavior, not a shared harness prison. Cross-cutting
findings live in
[`examples/FINDINGS.md`](examples/FINDINGS.md).

## Documentation

| Section | What it covers |
|---|---|
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
| [`ROADMAP.md`](ROADMAP.md) | Phases, near-term work, and explicit non-goals |
| [`CHANGELOG.md`](CHANGELOG.md) | Completed phases |

## Status and limits

Implemented today: explicit-step single-shard runtime, multi-shard runner with
bounded shard-pair queues, `ThreadedRuntime` over Betelgeuse, runtime-owned
TCP/UDP/DNS/TLS/file/path/process/signal/persistence rails, isolate calls
with mandatory timeout, native first-form HTTP/1.1, framed RPC with typed
service helpers, supervision with `OneForOne`/`OneForAll`/`RestForOne` and
runtime-lifetime restart budgets, terminal shutdown reports with topology and
trace, `tina-sim` with virtual time / seeded faults / scripted I/O / replay /
DST shrinking, and narrow Tokio bridge crates with typed
`Full`/`Closed`/`Timeout` outcomes.

Not yet:

* native HTTP/2 / gRPC service stack (planned, see [ROADMAP.md](ROADMAP.md));
* native database client (PG wire / SQLite); current path is the bridge to `sqlx`/`tokio-postgres`;
* `io_uring` substrate (Linux); current backend is portable;
* remoting or clustering;
* time-windowed restart budgets (runtime-lifetime budgets only today);
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
