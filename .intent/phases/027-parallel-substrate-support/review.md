# 027 Review

Session:

- A

## Scope Check

027 stayed in the support lane beside 026. The changes are documentation,
tests, comparison evidence, and Betelgeuse simulated-I/O polish. No Tina core
semantics, effect vocabulary, runtime-owned call meanings, public isolate
handler shape, bounded queue behavior, or driver contract were changed.

## Betelgeuse Simulated I/O Polish

The simulated backend now reads more clearly as Betelgeuse code rather than a
Tina-specific test shim:

- `io::simulated` has module-level documentation describing the in-memory TCP
  backend, explicit `IOLoop::step` progress, deterministic delay, and partial
  send pressure.
- Internal names now say `SimulatedState`, `SimulatedSocket`,
  `SimulatedSocketKind`, `SimulatedSocketState`, `SimulatedPendingOp`, and
  `SimulatedReadyResult`, which makes upstream review easier.
- Tests were tightened around TCP-specific behavior:
  - accept / recv / send roundtrip
  - partial send limit
  - delayed ready completion
  - peer-provided input followed by EOF
  - listener and stream address reporting
  - unsupported file operations remain explicitly unsupported

Before proposing upstream, the remaining questions are ownership/lifetime
policy for stored completion pointers, whether peer scripting should stay this
small or become a separate fixture layer, and whether unsupported file ops
should remain hard errors or be hidden behind a TCP-only feature.

## Cost Evidence

The new evidence is intentionally narrow:

- Existing allocation probes still pin selected runtime hot paths:
  multi-shard send handoff, isolate call, and Betelgeuse ingress handoff.
- Added operation-count probes pin the number of explicit runtime rounds for a
  single-shard send and a multi-shard send.

This is not a wall-clock benchmark and not a global allocation-free claim. It
only says the named hot paths still have the measured allocation / round-count
shape.

## Tokio-vs-Tina Expansion

The runnable comparison suite now includes 23 examples. The new constrained
cases add stronger Tokio variants rather than comparing only the easiest
channel shape:

- bounded `try_reserve` backpressure compared with Tina bounded ingress
- `send_timeout` under backpressure compared with Tina's immediate visible
  `Full`
- receiver shutdown that drains buffered Tokio work compared with Tina stop
  abandonment and trace visibility

The intentionally different cases are recorded as evidence, not marketing.
They show where Tokio can be hardened with bounded channels and timeouts, and
where Tina exposes a different rule at the isolate/runtime boundary.

## External Review Prompts

### Prompt for 025

Review `.intent/phases/025-betelgeuse-runtime-substrate-completion/plan.md`,
`.intent/phases/025-betelgeuse-runtime-substrate-completion/review.md`, and the
commits through `ed44f27`.

Focus on:

- whether `BetelgeuseRuntime` remains an interpreter of Tina semantics rather
  than a second semantic model
- whether bounded ingress and cross-shard queues stay honest under live worker
  handoff
- whether the Betelgeuse simulated TCP proof is scoped honestly
- whether allocation evidence is narrow enough for the claims made

### Prompt for 026

Review `.intent/phases/026-tina-driver-contract/plan.md` and the commits after
025 before implementation lands.

Focus on:

- whether the proposed driver boundary owns only time/TCP/completion readiness
- whether cancellation and shutdown remain visible Tina outcomes
- whether the refactor preserves synchronous isolate handlers and bounded
  queue semantics
- whether simulated and native Betelgeuse can share one driver path without
  pushing protocol semantics into the driver

No external review result has been folded back yet in this session.

## Adapter Research Notes

### Tokio current-thread

Gives a mature timer, TCP, and ecosystem surface with `LocalSet` and bounded
`mpsc` available. It weakens the completion-owned model because operations are
future-driven and wake themselves. The smallest adapter shape would be a
single-thread driver that owns a current-thread runtime, stores Tina call ids
beside spawned local futures, polls the runtime from `Driver::step`, and
translates future completion into Tina `DriverCompletion`s. It must not expose
`tokio::Handle` to isolates.

### Monoio

Gives an io_uring-oriented, thread-local async I/O substrate that is closer to
shard ownership than a work-stealing runtime. It weakens portability and still
uses futures as the operation representation. The smallest adapter would map
each Tina TCP/time op to one driver-owned local task on one monoio runtime per
shard, with bounded command admission outside the adapter.

### Glommio

Gives a thread-per-core local executor and Linux-focused I/O model with an
explicit affinity story. It weakens portability and may push more executor
policy into the substrate than Tina wants in core. The smallest adapter would
be a shard-local executor wrapper that submits TCP/time ops from driver state
and drains completion notifications into Tina's runtime loop.

### Compio

Gives a completion-oriented async I/O project with cross-platform ambition. It
is attractive for a backend-neutral driver experiment, but the API surface and
maturity need review before betting core semantics on it. The smallest adapter
would mirror the Tokio/Monoio shape: one driver-owned runtime per shard,
driver-owned futures/tasks for TCP/time, and Tina call ids preserved at the
submission boundary.

## Closeout Notes

README wording was kept brief and honest: Tina remains an experimental
concurrency primitive with selected live-substrate evidence, not a broad Tokio
replacement or a production-performance claim.
