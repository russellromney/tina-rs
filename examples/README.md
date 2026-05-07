# Eiffel Examples

`examples/` is the home for Eiffel: paired Tokio-vs-Tina implementation
comparisons for common use cases. These are not Tina crate tests and not
benchmarks. They are ergonomics and functionality probes that should help us
discover where Tina is safer, awkward, incomplete, broken, or pointing at a
better model.

Cross-cutting ergonomic findings — patterns that show up in more than one
comparison and the API/runtime suggestions they imply — live in
[`FINDINGS.md`](FINDINGS.md). Per-comparison ergonomic notes stay in each
comparison's own `README.md`.

## Examples are specimens

> Examples are specimens. Tests are proof. README is discussion.

The point of these examples is *feel*. You read the Tokio side, you read
the Tina side, and you form an opinion about which one you'd rather
write and maintain. Anything that gets in the way of that — shared
harnesses, mechanical wire-byte parity, "what this does not prove"
disclaimers — is bloat.

### Rules

- **Each side is a self-contained file.** `tokio_impl.rs` and
  `tina_impl.rs` are readable top-to-bottom. No shared `drive_client`,
  no shared `SideReport`, no `mod.rs` that constrains both sides.
- **Local types stay local.** A small `RunConfig` / `RunReport` per
  side is fine if it helps. Don't pull them into a shared module.
- **No third side.** No `tina_typed_impl.rs` etc. If a feature needs
  its own demo, it gets its own example crate.
- **`main.rs` is a dispatcher.** Argument is `tokio` / `tina` / `both`.
  It calls each side's `run` and prints whatever comes back.
- **Smoke tests only.** Each side gets a tiny test that runs it and
  checks "every request accounted for" or similar coarse shape. No
  exact invariant pinning.
- **Library invariants live in library tests.** If you want to pin
  "the in-flight cap returns `Error(Full)` after N requests," that
  test belongs in `tina-rpc/tests/connection.rs`, not in the example.
- **README is discussion.** Compare feel and observed behavior in
  prose. No "What This Does Not Prove" sections — that reads like a
  court filing.
- **Do not preserve byte-for-byte parity** if it forces one side to
  look unidiomatic. If parity creates friction, drop the parity, not
  the idioms.

### Layout

```
examples/foo/
  README.md
  Cargo.toml
  src/
    main.rs
    tokio_impl.rs
    tina_impl.rs
  tests/
    smoke.rs
```

Each side exposes `pub fn run(config: RunConfig) -> anyhow::Result<RunReport>`
(or whatever local shape is useful — sides do not have to agree on
type).

### README template

```md
# Foo

This example shows ...

## Run

cargo run --manifest-path examples/foo/Cargo.toml -- tokio
cargo run --manifest-path examples/foo/Cargo.toml -- tina
cargo run --manifest-path examples/foo/Cargo.toml -- both

## Read

- `src/tokio_impl.rs`
- `src/tina_impl.rs`

## Tokio Shape

Tokio does ...

When you run it, notice ...

## Tina Shape

Tina does ...

When you run it, notice ...

## Discussion

What feels better:
- ...

What feels worse:
- ...

What this suggests:
- ...
```

### Smoke test shape

```rust
#[test]
fn tina_smoke() {
    let report = tina_impl::run(RunConfig { burst: 4 }).unwrap();
    assert_eq!(report.total(), 4);
}
```

That's the ceiling, not the floor — most examples don't need more
than that.

## Current comparisons

| Directory | Status | Purpose |
|---|---|---|
| `eiffel_real_io_chat` | First specimen | Slow-consumer chat/fanout over real TCP. |
| `eiffel_mini_keyspace` | Built | Tiny Redis-shaped key/value service over real TCP; tests request/reply continuations and store-isolate ergonomics. |
| `eiffel_axum_counter` | Built | Stateful HTTP counter over axum; tests `tina-tokio-bridge` ergonomics and HTTP-shaped pushback. |
| `eiffel_ws_room` | Built | WebSocket broadcast room with two clients; tests bridge-hosted bidirectional sessions and subscriber pruning. |
| `eiffel_mux_client` | Built | Tina as a multiplexed *client* against a Tokio TCP responder; proves out-of-order arrival and tests correlation/parsing in an isolate. |
| `eiffel_cpu_run` | Built | Wrapper runner that re-executes any built comparison under N CPU-busy spinner threads; reports baseline vs contended wall-clock and exit status. |
| `eiffel_mem_run` | Built | Wrapper runner that re-executes any built comparison under a series of `RLIMIT_AS` caps (Linux real, macOS best-effort no-op); reports per-tier duration and exit status. |
| `eiffel_supervised_worker` | Built | Worker that panics on poison messages; compares Tokio's hand-rolled `catch_unwind`/respawn loop against Tina's supervisor + restart budget. |
| `eiffel_persistent_counter` | Built | Counter that survives restart via runtime-owned snapshot + journal; compares against a Tokio file-write story. |
| `eiffel_replay_dst` | Built | Same workload run twice under `tina-sim` with one seed; demonstrates deterministic replay versus the Tokio shape that cannot answer the question. |
| `eiffel_outbound_fetch` | Built | "Go fetch these endpoints and aggregate" — Tina as a TCP/DNS *client*, compared to `reqwest`/`hyper`. |
| `eiffel_graceful_shutdown` | Built | Long-lived service with in-flight work receives SIGINT; compares `tokio::signal` + manual drain against Tina's signal capture and bounded shutdown story. |
| `eiffel_rpc` | Built | Same wire workload, two implementations: Tina framed RPC with bounded in-flight (overload becomes `Error(Full)` on the wire) vs Tokio with an unbounded queue (silently buffers). |

All seven items from the original ROADMAP Eiffel backlog are built, and
all five forward-backlog items added in this round are also built.
