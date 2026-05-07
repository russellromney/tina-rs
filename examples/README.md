# Eiffel Examples

`examples/` is the home for Eiffel: paired Tokio-vs-Tina implementation
comparisons for common use cases. These are not Tina crate tests and not
benchmarks. They are ergonomics and functionality probes that should help us
discover where Tina is safer, awkward, incomplete, broken, or pointing at a
better model.

Cross-cutting ergonomic findings — patterns that show up in more than one
comparison and the API/runtime suggestions they imply — live in
[`FINDINGS.md`](FINDINGS.md). The longer field journal and resolved
archaeology live in [`FINDINGS_HISTORY.md`](FINDINGS_HISTORY.md).
Per-comparison ergonomic notes stay in each comparison's own `README.md`.

Before writing or rewriting an example, check the
[ergonomics checklist](../docs/tina-user-guide/11-ergonomics-checklist.md)
for the primitives the runtime now ships (mailbox factory, single-shard
default, observation handles, etc.). It's the "use this, not that"
shortlist — saves re-discovering each retired hand-rolled pattern.

## Examples are specimens

> Examples are specimens. Tests are proof. README is discussion.

The point of these examples is *feel*. You read the Tokio side, you read
the Tina side, and you form an opinion about which one you'd rather
write and maintain. Anything that gets in the way of that — shared
harnesses, mechanical wire-byte parity, "what this does not prove"
disclaimers — is bloat.

### Rules

- **Each side is a self-contained file.** `tokio_impl.rs` and
  `tina_impl.rs` are readable top-to-bottom. No shared `drive_client`
  and no shared harness that constrains both sides.
- **Tiny local shared types are fine.** A small crate-local
  `RunConfig` / `RunReport` is okay when it makes `main.rs` and smoke
  tests boring. Do not let those types grow into a shared protocol
  driver.
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
(or whatever local shape is useful). The shape should make the side
easy to read, not force parity for its own sake.

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
| `eiffel_rpc` | Specimen | Same framed request burst, two implementations: Tina framed RPC with bounded in-flight (overload becomes `Error(Full)` on the wire) vs Tokio with an unbounded queue (silently buffers). |
| `eiffel_real_io_chat` | Specimen | Slow-consumer chat/fanout over real TCP. |
| `eiffel_mini_keyspace` | Specimen | Tiny Redis-shaped key/value service over real TCP; tests request/reply continuations and store-isolate ergonomics. |
| `eiffel_mux_client` | Specimen | Tina as a multiplexed *client* against a Tokio TCP responder; proves out-of-order arrival and tests correlation/parsing in an isolate. |
| `eiffel_supervised_worker` | Specimen | Worker that panics on poison messages; compares Tokio's hand-rolled `catch_unwind`/respawn loop against Tina's supervisor + restart budget. |
| `eiffel_persistent_counter` | Specimen | Counter that survives restart via runtime-owned snapshot + journal; compares against a Tokio file-write story. |
| `eiffel_replay_dst` | Specimen | Same workload run twice under `tina-sim` with one seed; demonstrates deterministic replay versus the Tokio shape that cannot answer the question. |
| `eiffel_outbound_fetch` | Specimen | "Go fetch these endpoints and aggregate" — Tina as a TCP/DNS *client*, compared to `reqwest`/`hyper`. |
| `eiffel_outbound_http` | Specimen | Same scripted HTTP endpoint sequence, two clients: `tina_http::HttpClient` (with a `Driver` isolate bridging the host thread) vs `reqwest::Client`. |
| `eiffel_native_http` | Specimen | Native HTTP/1.1 counter server: `tina_http::HttpListener` + Counter isolate vs `axum`. **First example where the Tina side is shorter than the Tokio side.** |
| `eiffel_graceful_shutdown` | Specimen | Long-lived service with in-flight work receives SIGINT; compares `tokio::signal` + manual drain against Tina's signal capture and bounded shutdown story. |
| `eiffel_axum_counter` | Bridge (deferred) | Stateful HTTP counter over axum; tests `tina-tokio-bridge` ergonomics and HTTP-shaped pushback. *Awaiting bridge ergonomics work; not rewritten under the specimens rule yet.* |
| `eiffel_ws_room` | Bridge (deferred) | WebSocket broadcast room with two clients; tests bridge-hosted bidirectional sessions and subscriber pruning. *Awaiting bridge ergonomics work.* |
| `eiffel_cpu_run` | Wrapper | Wrapper runner that re-executes any built comparison under N CPU-busy spinner threads; reports baseline vs contended wall-clock and exit status. *Pure subprocess driver; no Tina/Tokio code, no specimens rule applies.* |
| `eiffel_mem_run` | Wrapper | Wrapper runner that re-executes any built comparison under a series of `RLIMIT_AS` caps (Linux real, macOS best-effort no-op); reports per-tier duration and exit status. *Pure subprocess driver.* |

**Status legend:**

- **Specimen** — rewritten under the [examples-as-specimens rule](#examples-are-specimens)
  with the [ergonomics checklist](../docs/tina-user-guide/11-ergonomics-checklist.md)
  applied: self-contained `tokio_impl.rs` / `tina_impl.rs`, `main.rs`
  dispatcher, smoke tests, prose README. Reads top-to-bottom on each
  side.
- **Bridge (deferred)** — uses `tina-tokio-bridge`; rewritten only after
  the bridge's ergonomics work lands so the rewrite reflects the shape
  callers should actually use.
- **Wrapper** — `std::process::Command`-driven runner that re-executes
  another comparison under OS-level pressure. Not a paired
  Tokio-vs-Tina specimen; rule and checklist do not apply.
