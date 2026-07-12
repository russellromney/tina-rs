# Tokio + Tina Bridge Composition

Companion to [mailbox-capacity.md](mailbox-capacity.md).

The Tokio bridge (`tina-tokio-bridge`) is for small, gradual adoption inside
existing Tokio apps. Tokio owns the edge; Tina owns isolate state. The
shape is inherently two runtimes — one Tokio, one
`ThreadedRuntime`. This page names the seams that bite first-time
readers, plus the one-call shutdown path that replaces the
`Arc::try_unwrap` dance.

## The two-runtime shape

A bridge service is one Tina `ThreadedRuntime` (its own OS thread) plus a
Tokio runtime that hosts the HTTP/RPC stack and calls into the bridge:

```text
+-- Tokio runtime --+         +-- Tina ThreadedRuntime (OS thread) --+
|                   |         |                                      |
|   axum::serve     |  call() |   Counter isolate                    |
|   handler ──────────────────►   handle(BridgeRequest, ctx) ─►reply |
|                   |  reply  |                                      |
+-------------------+         +--------------------------------------+
        ^                                   ^
        |                                   |
   tokio::spawn                       sync handler turn
   (async tasks)                      (no .await possible)
```

`BridgeHandle` clones flow from Tokio code into axum extractors. Each
clone holds an `Arc<ThreadedRuntime>` — that's how requests reach the
worker thread.

## Footgun #1: sync `recv()` inside Tokio `block_on`

```rust,ignore
let tokio_runtime = tokio::runtime::Builder::new_current_thread().build()?;
tokio_runtime.block_on(async move {
    let _ = std::sync::mpsc::Receiver::recv(&shutdown_rx);  // DEADLOCK
});
```

Calling a sync `recv()` inside a `current_thread` runtime parks the OS
thread that the executor needs to drive other futures. The responder
task never runs, so nothing ever sends, so the recv never wakes. The
failure mode looks like *"my server didn't start"* but the cause is
*"my driver thread blocked the executor."*

**Fix:** use `tokio::sync::oneshot` (or any other Tokio-aware
synchronization primitive) inside `block_on`.

This is not a Tina bug — it's a property of `current_thread` Tokio
runtimes — but the bridge is the place most users hit it.

## Footgun #2: `Arc::try_unwrap` shutdown dance

Older examples ended with:

```rust,ignore
match Arc::try_unwrap(runtime) {
    Ok(runtime) => { let _ = runtime.shutdown(); }
    Err(still_shared) => drop(still_shared),
}
```

This works only when every `BridgeHandle` clone has been dropped — the
host has to know the strong-count is exactly 1 before unwrapping. In
practice, axum extractors keep clones alive at scope-exit time and the
unwrap silently fails.

**Fix:** use `BridgeHost::drain_and_shutdown(drain_timeout)`:

```rust,ignore
let app = LocalSystem::single_shard(SingleShard, DefaultThreadedMailboxFactory)
    .config(local_system_config)
    .try_build()?;
let mut host = BridgeHost::from_app(app);
let bridge = host.register_bridge::<MyIsolate, Req, Resp, Infallible>(
    isolate, mailbox_capacity, per_call_timeout,
)?;
// ... run axum, hand bridge clones around, ...
let report = host.drain_and_shutdown(Duration::from_secs(2))?;
assert!(report.drained_within_timeout);
```

The drain loop polls until every `BridgeHandle` clone has been dropped
or the timeout elapses. If clones remain, the runtime is left alive and
the host can retry after more handles are dropped.
`BridgeShutdownReport.outstanding_handles_at_shutdown` names how many
clones survived the drain (zero on success). `pending_handles()` is also
available between calls if the host wants to log progress.

## Footgun #3: Tokio + Tina signal handlers in the same process

Both `tokio::signal::ctrl_c()` and `tina_runtime::signal_wait("sigint", _)`
register process-global handlers via `signal-hook`. The chain coexists
*technically*, but when the Tokio runtime drops, its registration stays
in the chain. Subsequent SIGINTs fire the now-orphaned Tokio handler too.

**Mitigation:** in tests, run each side as a subprocess (this is what
`specimen_graceful_shutdown`'s `compare` mode does). In a real bridge app,
pick one side to own signal handling — usually Tina (`signal_wait`)
because it surfaces signals as ordinary later messages and integrates
with the runtime's shutdown report. Document the choice loudly.

There is no public API to query "which handlers are registered" or
"tear down my registrations."

## Capability table

The bridge's `BRIDGE_CAPABILITIES` constant publishes the truth as code:

| Capability               | Status     | Meaning                                   |
| ------------------------ | ---------- | ----------------------------------------- |
| `bounded_ingress`        | Preserved  | Bridge ingress is bounded.                |
| `synchronous_handlers`   | Preserved  | Handlers stay synchronous.                |
| `visible_failures`       | Preserved  | `Full` / `Closed` / `Timeout` typed.      |
| `deterministic_replay`   | Weakened   | Tokio side does not replay deterministically. |
| `tokio_scheduler_control`| NotClaimed | Bridge does not constrain Tokio scheduling. |

Read this from code as the bridge's contract; do not duplicate it in
prose.

## Related

- [`tina_tokio_bridge::BridgeHost::drain_and_shutdown`]
- [`tina_tokio_bridge::BridgeHost::pending_handles`]
- [`tina_tokio_bridge::BridgeShutdownReport`]
- [`tina_tokio_bridge::BridgeBackpressure`] — explicit retry / reject
  policy for the `Full` outcome.

[`tina_tokio_bridge::BridgeHost::drain_and_shutdown`]: https://docs.rs/tina-tokio-bridge
[`tina_tokio_bridge::BridgeHost::pending_handles`]: https://docs.rs/tina-tokio-bridge
[`tina_tokio_bridge::BridgeShutdownReport`]: https://docs.rs/tina-tokio-bridge
[`tina_tokio_bridge::BridgeBackpressure`]: https://docs.rs/tina-tokio-bridge
