# Eiffel Tokio-vs-Tina Comparison Gauntlet

Eiffel is not one comparison and it is not a benchmark suite. It is a
discovery program made of paired implementations: one Tokio version, one Tina
version, the same protocol, the same load, and the same metrics shape.

The point is to find places where Tina:

- is wrong, broken, or half-formed;
- has awkward or repetitive ergonomics;
- works better than ordinary Tokio-shaped code because overload is visible;
- works worse because the model is too narrow;
- needs new helpers, a model change, or explicit non-claims before larger ports;
- should feed findings back into DST, docs, public API, or roadmap work.

## Shape

Each comparison should contain:

- a Tokio server process;
- a Tina server process;
- a shared protocol;
- a shared report format;
- a load-driver mode that starts one side at a time;
- the same pressure knobs for both sides;
- separate runnable process modes for each side, so later runs can wrap either
  side in memory, CPU, scheduler, or tracing constraints;
- real I/O when the comparison is about I/O;
- output that names accepted, rejected, delivered, buffered, and trace-visible
  outcomes;
- notes about what felt good, what felt bad, and what broke.

Future comparisons should live next to this one under `examples/`, not inside
the Tina crates. Tina is the subject under test, not the owner of Eiffel.

## Comparison Backlog

| Comparison | Pressure shape | What we are trying to learn |
|---|---|---|
| `eiffel-real-io-chat` | Slow fanout over real TCP | How visible bounded pressure feels compared with easy Tokio buffering. |
| mini-redis-style keyspace | Hot keys, many clients, slow responses | Whether isolate-per-key/session feels natural and where call ergonomics repeat. |
| Axum/Tower stateful service | Tokio edge with Tina core | Whether the bridge is pleasant and whether overload remains visible at HTTP edges. |
| WebSocket room | Bidirectional read/write, ping/pong, slow readers | Whether Tina's explicit sessions help or become too ceremonial. |
| Multiplexed client subset | Many in-flight requests on one connection | Whether Tina is usable for client libraries, not just servers. |
| CPU contention run | Same load plus CPU quota/burner | Whether Tina keeps shedding visibly under scheduler pressure. |
| Memory-tier run | 32/64/128 MB server wrappers | Whether Tina plateaus while Tokio accumulates hidden buffered work. |

## `eiffel-real-io-chat`

Runs a real Tokio TCP listener and a real Tina live-runtime TCP listener over
loopback. A real client sends a burst size to each side.

The Tokio side uses an unbounded channel to represent a common chat-style
fanout path. Under a slow consumer, every message is accepted and the excess
stays buffered.

The Tina side reads the same request through runtime-owned TCP and tries to
deliver the same burst into a capacity-1 slow-client mailbox. The first message
is accepted and later delivered; the rest are visible `Full` outcomes in the
response and runtime trace.

Run it with:

```bash
cargo run --manifest-path examples/eiffel_real_io_chat/Cargo.toml
```

The default mode spawns separate child processes for the Tokio and Tina sides.
You can pass a burst size:

```bash
cargo run --manifest-path examples/eiffel_real_io_chat/Cargo.toml -- compare 1024
```

You can also run each side directly, which is the shape to wrap with OS-level
constraints later:

```bash
cargo run --manifest-path examples/eiffel_real_io_chat/Cargo.toml -- tokio 1024
cargo run --manifest-path examples/eiffel_real_io_chat/Cargo.toml -- tina 1024
```

What this currently teaches:

- Both sides are doing real loopback I/O.
- Tokio's unbounded channel ergonomics are very easy, but buffered work grows
  with the burst.
- Tina's bounded mailbox pressure is visible at the edge and in the runtime
  trace.
- Tina has a second pressure point in this shape: every `send_observed` reply
  comes back through the connection isolate mailbox. The first draft used a
  tiny connection mailbox and the connection could not collect enough observed
  outcomes to write its response under a burst of 64. The current example
  sizes the connection mailbox separately, but this is an ergonomics/design
  smell for fanout workloads.
- Fanout with observed admission currently needs `batch` plus repeated
  `send_observed(...).reply(...)`, which is clear but a little ceremonial for
  chat-room style code.

## Next Harness Step

This comparison still needs a real load-driver mode. The intended next shape is:

```bash
cargo run --manifest-path examples/eiffel_real_io_chat/Cargo.toml -- load --side tokio --clients 1000 --messages 50000
cargo run --manifest-path examples/eiffel_real_io_chat/Cargo.toml -- load --side tina --clients 1000 --messages 50000
```

Then add wrapper support:

```bash
EIFFEL_WRAPPER='systemd-run --scope -p MemoryMax=64M -p CPUQuota=50%' \
cargo run --manifest-path examples/eiffel_real_io_chat/Cargo.toml -- load --side tina
```

The wrapper must be configurable because Linux has good cgroup/systemd/prlimit
options, while macOS memory limiting is much less direct.
