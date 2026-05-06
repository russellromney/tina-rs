# Phase 046: Baobab Production-Readiness Rails

## Goal

Judge the portable local runtime from 045 hard.

Baobab answers:

> Can a small Tokio/Glommio-shaped local service move to Tina and become more
> bounded, visible, and testable without awful ergonomics?

This is not a demo, porting guide, release story, `io_uring` phase, or
marketing phase. It is the readiness wall over the thing that now exists.

## Baseline

Already built:

- `LocalSystem` / `LocalMultiShardSystem`;
- runtime-owned TCP, UDP, DNS, TLS, file/path, process, signal, timers,
  persistence, shutdown notification;
- bounded Tokio/Tower/Axum edge bridge;
- `tina-runtime/tests/portable_service.rs`;
- `tina-sim/tests/portable_service_dst.rs`;
- `make verify`;
- `make portable-runtime-cost` smoke rows.

Baobab extends these. It does not rebuild them.

## Non-Goals

- No remoting, clustering, distributed placement, durable mailbox, or
  exactly-once claim.
- No broad "faster than Tokio/Glommio" or production-ready claim.
- No Tower/Axum middleware living inside Tina.
- No `io_uring`.
- No hidden fallback queues.
- No broad macro/flow-syntax work.

## Rules

- Overload must be visible as `Full` or pressure.
- Failure must be typed and traced.
- Races must replay through DST or deterministic e2e.
- Capability gaps must be explicit.
- Cost commands print numbers and environment; they do not make speed claims.
- Glommio is optional/platform-gated and must not break macOS/default verify.

## Rocks

1. **Executable Capability Matrix**

   Add a Rust-owned matrix, likely `tina-runtime/tests/readiness_matrix.rs`.
   Docs only summarize it.

   Rows: TCP, UDP, DNS, TLS, file/path, process, signal, timers, isolate calls,
   cross-shard sends, persistence, shutdown, bridge ingress, cancellation,
   backpressure, replay/DST, affinity, cost reporting.

   Status: `Supported`, `Partial`, `Unsupported`, `NotClaimed`,
   `PlatformGated`.

   It must compare Tina against public `RuntimeCapabilities`; add Tokio/Glommio
   notes only where meaningful.

2. **User-Service Gauntlet**

   Extend `portable_service.rs`.

   Keep one composed happy service using multiple rails: TCP or TLS, DNS,
   file/path, timer, process, persistence, cross-shard call, graceful shutdown.

   Add focused scary-edge tests for:

   - listener/session loop;
   - outbound client call;
   - timeout and retry;
   - config read;
   - process helper;
   - signal/shutdown;
   - durable checkpoint;
   - clean shutdown;
   - driver shutdown failure after useful trace;
   - failed shard with sibling trace retained;
   - pending owned work reported;
   - incomplete trace marked incomplete.

3. **Backpressure Wall**

   Add no-sleep-as-proof overload tests for:

   - mailbox full;
   - live ingress full;
   - shard-pair full;
   - bridge ingress full;
   - storage, DNS, TLS, process, and signal lane full;
   - persistence append rejection;
   - requester completion mailbox full.

4. **Bridge Readiness**

   Exercise the bridge as an app edge:

   - bounded ingress;
   - caller timeout;
   - cancellation before admission;
   - cancellation after admission;
   - retry policy naming;
   - total deadline;
   - retryable shutdown after shared handles drop;
   - metrics and health.

   Pin the contract: caller timeout is not rollback unless cancellation wins
   before Tina admission.

5. **DST Gauntlet**

   Add service-shaped histories with saved seeds, replay assertions, and clear
   invariants:

   - cancellation + timeout + late completion;
   - pressure + shard failure + topology truth;
   - persistence + restart + corrupt/truncated data;
   - bridge ingress + service shutdown + retry;
   - observed send + persistence + requester stop.

   At least one new family must exercise deletion shrinking. The others need
   saved seeds and replay.

6. **Live Multi-Shard Pressure**

   Run live services on real worker threads with bounded cross-shard sends and
   isolate calls under pressure.

   Prove healthy shards keep running when one shard fails, terminal report keeps
   failed-shard truth and sibling trace, advisory core/thread ownership is
   visible, and hard OS pinning is not claimed.

7. **Cost And Comparison Rows**

   Upgrade `make portable-runtime-cost`.

   It must print environment, backend, build profile, row status, and timing
   and/or allocation numbers where available.

   Rows: local send, live ingress, cross-shard send, isolate call, timer, TCP
   loopback, TLS loopback, file read/write, journal append, bridge call.

   Add runnable behavior comparisons for bounded channel pressure, many
   connections, slow peer, cancelled request, shutdown with work in flight, and
   local disk/persistence pressure. Include hardened Tokio where small. Glommio
   rows are platform-gated and may skip visibly.

8. **CI And Readiness Report**

   Extend current CI, do not replace it.

   Default gate: `make verify`. It must include the readiness matrix, selected
   Baobab DST seeds, and cost command smoke. Do not make users remember a
   second verification command.

   Host-specific or slow comparison jobs must be optional and named.

   Update `review.md`, `CHANGELOG.md`, `ROADMAP.md`, and `SYSTEM.md` with
   landed truth only: what Tina can run, what the gate proves, what remains a
   non-claim, and what blocks prime-time porting.

## Required Proof

- `make verify` passes.
- Readiness matrix tests pass.
- Service gauntlet has positive, negative, overload, shutdown, and failure
  tests.
- New DST families replay from saved seeds.
- At least one new DST family exercises deletion shrinking.
- Bridge timeout/cancel semantics are directly tested.
- Cost command prints local numbers and makes no speed claim.
- CI names platform-specific exclusions honestly.

## Done Means

- A future porting session has a real gate to run before claiming Tina is ready
  for that port.
- The matrix tells users what works, what rejects, what is platform-gated, and
  what Tina does not claim.
- No closeout note can honestly say "this was only happy-path tested" for the
  rocks above.
- Tina can claim a serious local-service readiness gate, not production
  readiness and not general Tokio replacement.

## Closeout Notes

Implemented:

- `tina-runtime/tests/readiness_matrix.rs` is the executable capability matrix.
  It now includes explicit cancellation, backpressure, and cost-reporting rows.
- `portable_service.rs` now includes a Baobab user-service gauntlet over a TCP
  listener/session, Tina-owned timer, DNS, bounded process execution,
  runtime-owned file I/O, journal append, cross-shard isolate call, and terminal
  shutdown truth.
- `portable_service.rs` also includes a live multi-shard failure gauntlet: one
  worker shard fails, sibling persisted service work still completes, and
  calls into the failed shard return typed closed/failure truth.
- `portable_service_dst.rs` now includes saved-seed Baobab histories for
  requester stop after admitted work, pressure plus shard failure, and deletion
  shrinking.
- `persistence_simulation.rs` now includes saved-seed Baobab persistence
  histories for clean restart, truncated-tail recovery, and corrupt recovery.
- `bridge_model_dst.rs` now includes a saved-seed Baobab timeout, retry, and
  shutdown contract history.
- `make portable-runtime-cost` now runs local smoke rows for local send, live
  ingress, cross-shard send, isolate call, and TCP loopback; it still prints
  explicit `not-measured` rows where not yet measured.
- `make verify` and CI run the readiness matrix, portable service, LocalSystem
  rail/backpressure e2e, simulator lane-full DST for DNS, process, TLS, and
  signal, service DST, bridge model/e2e, and cost smoke.

Remaining non-claims:

- The cost command is still smoke/report evidence, not a benchmark.
- TLS/bridge cost rows are not measured yet.
- Glommio remains platform-gated and optional.
- Tina is not production-ready and not a general Tokio replacement.
