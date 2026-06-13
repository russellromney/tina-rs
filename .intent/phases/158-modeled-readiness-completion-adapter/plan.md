# Phase 158: Modeled Readiness Completion Adapter

Status: planned.

## Goal

Recover kernel-efficient idle waiting without reintroducing the Phase 151 wake
side-channel.

Phase 157 restored the clean boundary: Tina advances I/O by explicit `step()`
calls and observes substrate progress as completion/event work. The cost is
known: idle CPU is bounded but nonzero, and HTTP latency now tracks the bounded
re-poll interval. This phase may improve that cost only by making readiness a
modeled Tina event, not by adding a live-only waker, doorbell, executor task, or
blocking `step`.

Success means:

- the live runtime can sleep efficiently when it has no modeled work ready;
- Tina still observes I/O progress as ordinary completion/event work;
- `tina-sim` can model the same readiness ordering, delay, cancellation, and
  fault cases deterministically;
- the current bounded re-poll path remains a simple baseline and fallback;
- evidence includes correctness tests and measured idle CPU / HTTP latency.

## Non-Negotiables

- No `step_blocking`, `IOWaker`, doorbell, hidden wake callback, or executor.
- No live-only path that changes when `step()` is called without a simulator
  model of the observable readiness/completion.
- No readiness event may let a socket/file operation complete outside the
  existing completion drain and trace rules.
- Cancellation, close, shutdown, EOF, reset, partial read/write, and late
  completion tombstones must keep their existing typed truth.
- Ordering must be stated before implementation: readiness events, timers,
  mailbox ingress, remote inbound, and backend completions need an explicit
  same-turn/next-turn policy.
- No performance claim may pass without the current explicit-step bounded
  re-poll baseline measured beside it.

## Design Shape

The likely shape is a runtime-owned readiness/completion adapter:

- Register interest through Tina-owned call state, not through a free callback.
- Deliver readiness as a bounded driver event or completion-like notification
  owned by the shard.
- Treat readiness as advisory. A readiness event permits an explicit `step()`;
  it does not itself perform user-visible I/O.
- Coalesce readiness only through modeled state with visible loss rules. If
  "already ready" and "ready during park" collapse into one event, the simulator
  must do the same.
- Keep backend-specific waiting below the adapter. Linux may use io_uring or
  poll-shaped facilities, macOS may use kqueue, but Tina sees the same modeled
  event vocabulary.
- Keep the adapter optional. Platforms or tests may use bounded re-poll without
  changing observable semantics.

Bad shapes:

- a cloneable cross-thread `wake()` handle;
- a condvar/eventfd hidden behind `step()`;
- relying on "wake only changes latency" reasoning without a modeled event;
- readiness that can reorder a completion ahead of a timer/mailbox event
  without a deterministic rule.

## Build

### 1. Write the model first

Add a short design doc or module comment that defines:

- the event vocabulary (`Readable`, `Writable`, `Hangup`, `Error`, or a more
  Tina-shaped equivalent);
- which resource owns each interest;
- whether events are edge-triggered, level-triggered, or coalesced;
- how readiness relates to an outstanding read/write completion;
- ordering against timers, mailbox ingress, remote inbound, and existing driver
  completions;
- what happens on cancel, close, shutdown, EOF, reset, and backend error.

The model must be precise enough that the simulator can implement it without
peeking at live backend code.

### 2. Extend the simulator before the live optimization

Teach `tina-sim` to script readiness/advisory progress under seeded delay and
fault policy.

Required proof:

- ready before park;
- ready during park;
- coalesced repeated readiness;
- readiness followed by EOF/reset/error;
- readiness after cancel/close becomes ignored or tombstoned by written rule;
- partial writes and reads still complete through the existing completion path;
- deterministic fingerprint for a fixed seed/config.

### 3. Add the live adapter behind the model

Implement the platform-specific waiting path only after the model exists.

Required proof:

- no public `step_blocking`, `IOWaker`, doorbell, or callback handle;
- the worker parks only while waiting for modeled runtime-owned events or a
  bounded timeout fallback;
- a readiness observation causes an explicit driver step/completion drain;
- shutdown cannot strand registered interests or completion storage;
- Linux and macOS behavior share the same Tina-level ordering contract.

### 4. Keep bounded re-poll as fallback and control

Do not delete the Phase 157 simple path. It is the reference implementation for
correctness and the fallback for unsupported backends.

Required knobs/reporting:

- configured bounded re-poll interval;
- whether modeled readiness waiting is enabled;
- observed readiness notifications;
- timeout fallback count;
- driver step count and completion drain count.

### 5. Measure the tradeoff honestly

Compare:

- Phase 157 bounded re-poll baseline;
- modeled readiness adapter enabled;
- idle single-shard CPU;
- warmed HTTP/1 keepalive p50/p90/p99;
- relevant HTTP/2/gRPC rows if the implementation touches shared transport;
- macOS/aarch64 and Linux/x86 where possible.

Acceptance is not "faster once." Acceptance is lower idle CPU and materially
better idle-to-request latency without weakening DST/model truth.

## Acceptance

- `rg "step_blocking|IOWaker|Doorbell|doorbell|wake side|readiness-driven park"`
  has no live positive hit except historical docs/tests that explicitly say the
  Phase 151 path was removed.
- `tina-sim` models readiness enough to replay the same observable event order
  under a fixed seed.
- Existing DST fingerprints remain stable unless the phase intentionally adds
  new modeled events and updates the evidence.
- Platform integration tests cover real Linux and macOS waiting.
- Loom is used for any new shared-memory handoff in the adapter. If the design
  avoids cross-thread shared state, record that as the proof instead of adding a
  fake loom test.
- The README/ROADMAP/VENDOR language still says explicit-step I/O, bounded
  re-poll fallback, and no wake side-channel.

