# tina-rs

![tina-rs hero](tina.png)

Tina is a bounded Rust concurrency framework for services built from isolated
state machines. Each isolate owns its state, handles one message synchronously,
and returns an `Effect`. The runtime owns scheduling, I/O, time, supervision,
shutdown, and trace collection.

Tina is useful when overload and lifecycle behavior must be part of the
program's contract. Mailbox capacity, timeouts, cancellation, shutdown, and
replay are explicit values rather than conventions around detached tasks.

The repository contains:

- `tina`, the isolate, address, context, and effect model;
- `tina-runtime`, the live single- and multi-shard runtimes;
- `tina-sim`, the deterministic simulator and replay tooling;
- protocol batteries and bounded bridges for HTTP, RPC, Tokio, Tower,
  databases, and AWS SDK clients.

> Tina is experimental and its public API is still changing before 0.1.0. The
> current release target is 64-bit Linux and macOS. The live runtime requires
> the pinned nightly toolchain in `rust-toolchain.toml`; 32-bit targets and
> Windows are not currently tested or supported.

## Run The Smallest Program

Clone the repository and run:

```sh
cargo run --locked -p tina-runtime --example hello_world
```

It prints:

```text
counter total = 5
```

The complete program is below. Its checked-in source is
[`tina-runtime/examples/hello_world.rs`](tina-runtime/examples/hello_world.rs),
which normal all-target workspace checks compile.

```rust
use std::convert::Infallible;
use std::time::Duration;

use tina::prelude::*;
use tina_runtime::{CallOutcome, DefaultThreadedMailboxFactory, ThreadedRuntime};

#[derive(Debug, Default)]
struct Counter {
    value: u64,
}

#[derive(Debug, Clone, Copy)]
enum CounterMsg {
    Add(u64),
    Read,
}

#[tina::isolate(message = CounterMsg, reply = u64)]
impl Counter {
    fn handle(
        &mut self,
        msg: CounterMsg,
        _ctx: &mut Context<'_, SingleShard, Self::Reply>,
    ) -> Effect<Self> {
        match msg {
            CounterMsg::Add(n) => {
                self.value += n;
                noop()
            }
            CounterMsg::Read => noop(),
        }
    }

    fn handle_call(&mut self, msg: CounterMsg, call: CallContext<'_, Self>) -> Effect<Self> {
        match msg {
            CounterMsg::Read => call.reply(self.value),
            CounterMsg::Add(n) => {
                self.value += n;
                call.reply(self.value)
            }
        }
    }
}

fn main() {
    let runtime = ThreadedRuntime::new(SingleShard, DefaultThreadedMailboxFactory);

    let counter = runtime
        .register_with_capacity::<Counter, Infallible>(Counter::default(), 16)
        .expect("register counter");

    runtime
        .try_send(counter, CounterMsg::Add(2))
        .expect("send add");
    runtime
        .try_send(counter, CounterMsg::Add(3))
        .expect("send add");

    match runtime.call_blocking(counter, CounterMsg::Read, Duration::from_secs(1)) {
        Ok(CallOutcome::Replied(total)) => println!("counter total = {total}"),
        other => println!("unexpected outcome: {other:?}"),
    }

    runtime.shutdown().expect("clean shutdown");
}
```

The important shape is small:

1. An isolate owns ordinary Rust state.
2. `handle` processes fire-and-forget messages.
3. `handle_call` consumes caller authority by replying, rejecting, or
   deferring the request.
4. Every handler returns one effect; it does not `.await`.
5. Registration chooses a mailbox capacity.
6. The runtime is shut down explicitly.

Continue with the [First Isolate](docs/tina-user-guide/02-first-isolate.md)
chapter and the [user-guide index](docs/tina-user-guide/README.md).

## Programming Model

Tina is not a drop-in replacement for Tokio. It is a service architecture
built around explicit ownership and state transitions.

| Tokio-shaped service code | Tina's default shape |
|---|---|
| task state across `.await` | isolate fields |
| task graphs | named isolates and typed messages |
| shared application state behind locks | state owned by one isolate |
| channel backlog hidden in plumbing | bounded mailboxes with typed pressure |
| cancellation by dropping a future | explicit timeout, cancel, and late-reply outcomes |
| logs as the only postmortem artifact | runtime trace, capacity reports, and replay cases |

The tradeoff is more visible state-machine vocabulary. Multi-step operations
have named continuation messages, capacities must be chosen, and external
async libraries enter through bounded bridges. Those costs are useful only
when they expose real pressure, lifecycle, or replay facts.

Tina does have explicitly named development escape hatches for capacity
discovery. Production capacity policy rejects unbounded modes; unboundedness
is never the default or an invisible queue.

## Live Runtime And I/O

`ThreadedRuntime` runs a live single-shard service. `LocalSystem` and
`ThreadedMultiShardRuntime` own multi-shard execution with bounded shard-pair
queues. Runtime-owned TCP, TLS, Unix-domain sockets, local file I/O, and
persistence use the vendored Betelgeuse completion substrate. UDP and signal
flags are polled without blocking on the shard thread. Blocking DNS and process
work, plus storage metadata operations the substrate does not expose, use named
bounded fallback lanes.

Application isolates see opaque resource IDs and typed completions. They do
not retain raw file descriptors or invoke Betelgeuse directly. Current rail
support and cancellation/shutdown behavior are available through
`RuntimeCapabilityReport` rather than inferred from marketing claims.

See the [I/O model](docs/tina-user-guide/12-io-model.md) and
[runtime-call guide](docs/tina-user-guide/03-effects-and-runtime-calls.md).

## Simulation And Replay

`tina-sim` runs the same isolate logic with virtual time and scripted resource
completions. A deterministic run is identified by the simulator version,
complete configuration, initial state, and operation history.

The seed selects choices only inside perturbation axes that the configuration
enables. With the default `FaultConfig`, changing the seed alone normally
changes nothing. Seeded axes currently cover ready-isolate scheduling,
local-send delay, timer-wake delay, and TCP-completion delay or reordering.

The practical workflow is:

1. run a deterministic baseline;
2. enable the perturbation axes relevant to the suspected bug;
3. sweep seeds while checking a semantic invariant;
4. save the failing seed, full config, and materialized history;
5. shrink the history and commit the resulting `ReplayCase`.

The full trace hash is a regression fingerprint, not a substitute for an
assertion about the service outcome. Same version, config, initial state, and
history reproduces the same trace. Different seeds *may* explore different
choices when an enabled axis and the workload provide a choice.

See [Simulation And DST](docs/tina-user-guide/08-simulation-and-dst.md).

## Documentation

- [User guide](docs/tina-user-guide/README.md)
- [Mental model](docs/tina-user-guide/01-mental-model.md)
- [Request and reply](docs/tina-user-guide/04-request-reply.md)
- [Boundedness and overload](docs/tina-user-guide/06-boundedness-and-overload.md)
- [Lifecycle and shutdown](docs/tina-user-guide/14-lifecycle-and-shutdown.md)
- [Outcome glossary](docs/tina-user-guide/13-outcome-glossary.md)
- [Roadmap](ROADMAP.md)
- [Changelog](CHANGELOG.md)

The repository-root [`examples/`](examples/) tree is an R&D specimen corpus.
It compares Tokio and Tina shapes and exercises difficult combinations to
discover API problems. It is useful implementation evidence, but it is not yet
a curated user tutorial or a stable API catalog.

## Development

The repository pins its Rust toolchain. Common checks are:

| Command | Purpose |
|---|---|
| `cargo run --locked -p tina-runtime --example hello_world` | Run the smallest live program. |
| `make check` | Type-check the workspace. |
| `make test` | Run nextest plus workspace doctests. |
| `make clippy` | Lint all workspace targets with warnings denied. |
| `make verify` | Run the sequential workspace gate: static checks, Clippy, tests, loom, guards, and cost smoke. |
| `make verify-examples` | Sweep the separate R&D specimen workspaces; intentionally not part of the normal workspace gate. |

CI runs the workspace gate as parallel Linux and macOS jobs and separately
promotes a small set of system and I/O specimens. Miri, fuzzing, long soaks,
and the full specimen sweep remain separate targeted checks.

## Status

The repository has extensive unit, integration, end-to-end, loom, replay, and
protocol tests, but it does not yet claim a stable API or production
performance. Important pre-0.1 work includes service ergonomics, the Tinio
rename, a deliberate public API contract, complete packaging of the crate
graph, and final release documentation.

## Lineage

`tina-rs` is an independent Rust implementation inspired by
[Peter Mbanugo's Tina](https://github.com/pmbanugo/tina) and
[The Tokio/Rayon Trap and Why Async/Await Fails Concurrency](https://pmbanugo.me/blog/why-async-await-complect-concurrency).
It also draws from Erlang/OTP supervision, Seastar-style shard-local
execution, TigerBeetle and FoundationDB deterministic testing, and Pekka
Enberg's [Betelgeuse](https://github.com/penberg/betelgeuse) I/O substrate.

## License

Dual-licensed under MIT or Apache-2.0, at your option.
