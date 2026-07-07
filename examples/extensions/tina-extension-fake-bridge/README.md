# tina-extension-fake-bridge

A **fake bridge** — one bounded worker around a blocking function — built with
only public APIs and the public `tina_runtime::bridge` vocabulary.

## The hook

A bridge glues Tina to a messy outside system. Tina bounds admission and
observes worker-terminal truth; it cannot always stop the outside work. The
`tina_runtime::bridge` vocabulary names every part a bridge exposes:

- **install result** — `FakeBridgeInstall` implements `BridgeInstall` (owns the
  closer + metrics handle);
- **closer** — `FakeBridgeCloser` implements `BridgeCloser` (`close()` /
  `is_closed()`);
- **metrics / pressure** — `FakeBridgeMetrics::pressure()` renders a
  `BridgePressure` with installed capacity, in-flight, high-water, and the
  rejection/late counters;
- **shutdown** — `close_and_drain(deadline)` returns a `BridgeDrainReport`;
- **worker-terminal vs caller-observed** — `BridgeTerminal` records what the
  worker reached; `BridgeCallerWarning` records what the caller saw.

## What it proves

- **Bounded setup.** A submit past the queue capacity is
  `Retryable(BridgeFull)`; after close it is `Unavailable(BridgeClosed)`. No
  unbounded buffer.
- **Caller-timeout honesty.** When the caller's deadline fires first, the bridge
  replies `ExternalWorkMayContinue` — it does **not** pretend the external work
  stopped. When the work later lands it is counted as a late terminal.
- **Lifecycle.** `close()` is idempotent and visible; `close_and_drain` drains
  and joins the worker within a deadline.

## Feeding a Tina isolate

This crate captures completions through a result channel so the smoke test stays
deterministic. A bridge feeding a live Tina service delivers each completion as a
message to an isolate instead — `runtime.try_send(address, Msg::Completed { .. })`
with an address from `ThreadedRuntime::register_with_capacity`, then
`reply_to(..)` to the original caller. That path is entirely public; the
bounded admission, worker-terminal accounting, and caller warning shown here are
exactly what such a bridge surfaces.

## Run the smoke test

```sh
cargo test --manifest-path examples/extensions/tina-extension-fake-bridge/Cargo.toml
```
