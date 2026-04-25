# tina-rs

A Rust port of the discipline behind [Tina](https://github.com/pmbanugo/tina) — thread-per-core, shared-nothing, synchronous handlers returning effects, bounded mailboxes everywhere.

**Status:** design phase. No code yet. See [ROADMAP.md](ROADMAP.md).

## What this is — and isn't

This is **not** a new Rust runtime competing with Tokio or monoio.

This is a **discipline-and-types layer** that rides on top of an existing thread-per-core runtime, so a codebase already invested in the Rust async ecosystem can adopt Tina's structural ideas incrementally:

- **Isolates** — typed state-machine structs, one per entity (tenant, connection, request stream). Each lives in a pre-allocated arena. No `Arc<Mutex<...>>` spaghetti.
- **Effects** — handlers return values like `Effect::Send(mailbox, msg)` or `Effect::Spawn(builder)`. The runtime executes them. Handlers themselves are pure-ish, which makes deterministic simulation natural.
- **Bounded mailboxes** — single-producer/single-consumer ring buffers. `try_send` with shed-load policy. No `mpsc::unbounded_channel` ever.
- **Shards** — one OS thread per core, pinned. No work-stealing. Tenants/isolates are placed on shards by hash; cross-shard messages cross a separate SPSC.

## Why

The Rust async ecosystem has known structural problems for sharded workloads:

- Work-stealing destroys cache locality
- Async coloring spreads through the codebase
- `mpsc::unbounded_channel` and bare `tokio::spawn` defaults turn traffic spikes into OOMs
- Mixing CPU work and I/O on the same scheduler stalls multiplexing

[Banugo's article](https://pmbanugo.me/blog/why-async-await-complect-concurrency) frames the case; [Tina](https://github.com/pmbanugo/tina) is his Odin reference implementation. This repo brings the *patterns* (not the runtime) to Rust.

## Why not just write a fresh runtime

Two reasons:

1. **The runtimes already exist.** [monoio](https://github.com/bytedance/monoio) is actively maintained, io_uring-based, thread-per-core. [glommio](https://github.com/DataDog/glommio) is the Datadog version. `tokio::runtime::Builder::new_current_thread` gives you the same shape with the existing ecosystem.
2. **The hard part isn't the scheduler.** It's the discipline — isolate-per-entity, effects-pattern, bounded mailboxes, supervision trees, deterministic simulation. That layer is portable across runtimes.

So tina-rs is opinionated glue: the trait crate names the capability; runtime impl crates plug into monoio, current-thread Tokio, or whatever else later. Choose your scheduler; keep the discipline.

## References

- [Tina](https://github.com/pmbanugo/tina) — Banugo's original Odin implementation
- ["Why async/await complect concurrency"](https://pmbanugo.me/blog/why-async-await-complect-concurrency)
- [monoio](https://github.com/bytedance/monoio) — likely runtime backend
- [shuttle](https://github.com/awslabs/shuttle) — concurrency model checking, useful for the simulator phase

## License

Dual-licensed under MIT or Apache-2.0, at your option.
