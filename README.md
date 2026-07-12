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

## Bounded By Construction

Clone the repository and run:

```sh
cargo run --locked -p tina-runtime --example bounded_mailbox
```

This first example isolates the host admission boundary. A host producer fills
a worker's two-slot mailbox. The third `Runtime::try_send` returns typed `Full`
with the undelivered job still owned by the host. After one deterministic
runtime step frees capacity, the host retries that exact job. Once the worker
stops, another send returns typed `Closed` with its job too.

```text
send Run(3) -> Full(Run(3)); host retains the job
retry Run(3) after one step -> Accepted
send Run(4) after stop -> Closed(Run(4)); host retains the job
```

The complete program is below. Its checked-in source is
[`tina-runtime/examples/bounded_mailbox.rs`](tina-runtime/examples/bounded_mailbox.rs),
which the normal all-target workspace checks compile. A test also requires this
README block to remain byte-for-byte synchronized with that source.

<!-- bounded-mailbox-source -->
```rust
use std::convert::Infallible;
use std::fmt;

use tina::TrySendError;
use tina::prelude::*;
use tina_runtime::{DefaultMailboxFactory, Runtime};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Job {
    Run(u64),
    Stop,
}

#[derive(Debug)]
pub struct Worker;

#[tina_runtime::isolate(message = Job)]
impl Worker {
    fn handle(
        &mut self,
        job: Job,
        _ctx: &mut Context<'_, SingleShard, Self::Reply>,
    ) -> Effect<Self> {
        match job {
            Job::Run(_) => noop(),
            Job::Stop => stop(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScenarioReport {
    pub rejected: Job,
    pub retried: Job,
    pub closed: Job,
}

impl fmt::Display for ScenarioReport {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(
            formatter,
            "send {:?} -> Full({:?}); host retains the job",
            self.rejected, self.rejected
        )?;
        writeln!(
            formatter,
            "retry {:?} after one step -> Accepted",
            self.retried
        )?;
        write!(
            formatter,
            "send {:?} after stop -> Closed({:?}); host retains the job",
            self.closed, self.closed
        )
    }
}

pub fn run_scenario() -> ScenarioReport {
    let mut runtime = Runtime::new(SingleShard, DefaultMailboxFactory);
    let worker = runtime.register_with_capacity::<Worker, Infallible>(Worker, 2);

    runtime.try_send(worker, Job::Run(1)).expect("job 1 fits");
    runtime.try_send(worker, Job::Run(2)).expect("job 2 fits");

    let rejected = match runtime.try_send(worker, Job::Run(3)) {
        Err(TrySendError::Full(job)) => job,
        other => panic!("expected typed Full, got {other:?}"),
    };
    assert_eq!(rejected, Job::Run(3), "Full returns the attempted job");

    assert_eq!(runtime.step(), 1, "one worker handles one queued job");
    runtime
        .try_send(worker, rejected)
        .expect("retry fits after one step");

    while runtime.step() > 0 {}

    runtime.try_send(worker, Job::Stop).expect("stop fits");
    assert_eq!(runtime.step(), 1, "worker handles stop");

    let closed = match runtime.try_send(worker, Job::Run(4)) {
        Err(TrySendError::Closed(job)) => job,
        other => panic!("expected typed Closed, got {other:?}"),
    };
    assert_eq!(closed, Job::Run(4), "Closed returns the attempted job");

    ScenarioReport {
        rejected,
        retried: rejected,
        closed,
    }
}

fn main() {
    println!("{}", run_scenario());
}
```

The host-boundary facts are visible at the call site:

1. Capacity is chosen when the isolate is registered.
2. Admission never silently grows a queue.
3. `Full` and `Closed` are typed outcomes.
4. A refused host send returns ownership of the message, so retry policy
   remains with the host.
5. The explicit-step runtime makes the pressure transition deterministic.

Inside an isolate, `send(...)` is an effect and therefore cannot return a
synchronous result to the current handler. When the sending isolate needs the
outcome, it uses `send_observed(...).then(...)`; the continuation receives
typed `SendOutcome::Accepted`, `SendOutcome::Full`, or `SendOutcome::Closed`.
That distinction keeps host admission and isolate-to-isolate pressure honest.

For the smallest complete live program, run:

```sh
cargo run --locked -p tina-runtime --example hello_world
```

It demonstrates owned state, fire-and-forget messages, a typed blocking call,
and clean threaded-runtime shutdown. Continue with the
[First Isolate](docs/tina-user-guide/02-first-isolate.md) chapter and the
[user-guide index](docs/tina-user-guide/README.md).

## Real I/O: A TCP Echo Server

A TCP echo server is one listener isolate plus one isolate per connection. The
listener binds a loopback port, accepts in a bounded loop, and spawns a fresh
`EchoConnection` for each accepted stream. Each connection reads a chunk, writes
the identical bytes back, and repeats until the peer half-closes. One connection
is one isolate; nothing is shared between them.

Clone the repository and run:

```sh
cargo run --manifest-path examples/specimen_tcp_echo/Cargo.toml
```

```text
echo: sent 38 bytes, got them back unchanged
load shed: burst=32 cap=4 -> admitted=4 Full=28 (listener cap for reference: 8)
```

The exact admitted/shed split on that second line shifts run to run — it is a
race between the producer and the worker. What is guaranteed, and all the test
pins, is that every one of the 32 records is accounted for and at least one is
shed as a typed `Full`.

<!-- TODO: tina_echo.gif -->

The connection isolate is the whole story. Its checked-in source is
[`examples/specimen_tcp_echo/src/lib.rs`](examples/specimen_tcp_echo/src/lib.rs).
The specimen is a separate workspace, excluded from the main gate, so
`make verify-examples` (not `cargo check --workspace`) is what compiles and
clippy-checks it.

```rust
/// One connection's lifecycle, one message per I/O completion.
#[derive(Debug, Clone)]
pub enum EchoConnectionMsg {
    /// Kick off the first read.
    Begin,
    /// A read completed (bytes, or an I/O error).
    Read(TcpReadReply),
    /// A write completed (accepted byte count, or an I/O error).
    Wrote(TcpWriteReply),
    /// The stream close completed.
    Closed(TcpStreamCloseReply),
}

/// One accepted TCP stream, echoed back to its peer.
#[derive(Debug)]
pub struct EchoConnection {
    stream: StreamId,
    max_chunk: usize,
    /// Bytes read but not yet fully written back. A partial write
    /// leaves the tail here so the echo is never truncated.
    pending: Vec<u8>,
}

impl EchoConnection {
    fn new(stream: StreamId, max_chunk: usize) -> Self {
        Self {
            stream,
            max_chunk,
            pending: Vec::new(),
        }
    }
}

#[tina_runtime::isolate(message = EchoConnectionMsg)]
impl EchoConnection {
    fn handle(
        &mut self,
        msg: EchoConnectionMsg,
        _ctx: &mut Context<'_, SingleShard, Self::Reply>,
    ) -> Effect<Self> {
        match msg {
            EchoConnectionMsg::Begin => {
                tcp_read(self.stream, self.max_chunk).then(EchoConnectionMsg::Read)
            }
            EchoConnectionMsg::Read(Ok(bytes)) => {
                if bytes.is_empty() {
                    tcp_close_stream(self.stream).then(EchoConnectionMsg::Closed)
                } else {
                    self.pending = bytes;
                    tcp_write(self.stream, self.pending.clone()).then(EchoConnectionMsg::Wrote)
                }
            }
            EchoConnectionMsg::Wrote(Ok(count)) => {
                self.pending.drain(..count);
                if self.pending.is_empty() {
                    tcp_read(self.stream, self.max_chunk).then(EchoConnectionMsg::Read)
                } else {
                    tcp_write(self.stream, self.pending.clone()).then(EchoConnectionMsg::Wrote)
                }
            }
            EchoConnectionMsg::Closed(Ok(())) => stop(),
            EchoConnectionMsg::Read(Err(_))
            | EchoConnectionMsg::Wrote(Err(_))
            | EchoConnectionMsg::Closed(Err(_)) => stop(),
        }
    }
}
```

The honest part is what runs this code. The *same* `EchoConnection` source — not
a reimplementation — drives two runtimes unchanged:

- live, over a real loopback socket on `ThreadedRuntime`
  ([`tests/live_echo.rs`](examples/specimen_tcp_echo/tests/live_echo.rs));
- deterministically, inside `tina-sim`'s `Simulator` driven by a scripted peer
  and replayed byte-for-byte from a fixed seed to a saved trace hash
  ([`tests/sim_echo.rs`](examples/specimen_tcp_echo/tests/sim_echo.rs)).

`tcp_read` / `tcp_write` / `tcp_close_stream` produce the same `Effect::Io` in
both places, so the connection's read-echo-read state machine is the same
program whether a kernel socket or a seeded simulator answers the call.

Backpressure stays explicit but honest. A sequential echo self-paces one read at
a time, so the wire can never overflow a connection's mailbox. The bounded
contract still governs every isolate: when a producer outruns a bounded worker,
the runtime returns a typed `Full` instead of growing an unbounded queue. That
is the `load shed` line above; the assertion behind it lives in the live test.

Run the proofs directly:

```sh
cargo test --manifest-path examples/specimen_tcp_echo/Cargo.toml
```

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
| `cargo run --locked -p tina-runtime --example bounded_mailbox` | Run the boundedness example from this README. |
| `cargo run --locked -p tina-runtime --example hello_world` | Run the smallest threaded live program. |
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
