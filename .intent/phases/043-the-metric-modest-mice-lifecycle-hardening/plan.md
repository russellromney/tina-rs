# Phase 043: The Metric Modest Mice Lifecycle Hardening

## Goal

Make Tina's live local system boring when the ugly things happen.

Victor gave Tina real local service shape: bounded cross-shard calls, inbound
TLS, resource counts, health reports, and shutdown accounting. This phase makes
that lifecycle harder to fool.

At closeout, Tina should be able to say:

> A live local Tina system can shut down, fail a shard, cancel worker-lane work,
> report resources, and handle OS signals without pretending hidden work is
> gone.

This is production-readiness work. It is not flow syntax, remoting, clustering,
release docs, or a broad benchmark claim.

## Core Rules

- No hidden fallback queues.
- No "clean" shutdown while worker-held resources remain.
- No resource count that only counts convenient tables.
- No signal story that depends on app code polling random globals.
- No failed shard silently accepting work later.
- No live behavior without a simulator/DST or direct e2e proof when the behavior
  is semantic.

## Build Order

1. Audit live lifecycle after Victor in `review.md`.
2. Harden worker-lane shutdown/resource accounting.
3. Add bounded drain/join rules for worker lanes.
4. Add raw OS signal capture where platform support is honest.
5. Harden failed-shard cleanup and post-failure rejection.
6. Expand topology/shutdown report proof.
7. Add DST and e2e lifecycle pressure.
8. Do positive, blast-radius, and hostile review. Fix findings.

## Rock 1: Audit

Write current facts in `review.md`. Cover:

- worker lanes: storage, DNS, TLS, process, signal, TCP/Betelgeuse;
- what can block after cancellation;
- what resources are table-owned vs worker-held;
- what shutdown can drain, cancel, tombstone, or only report;
- what shard failure currently cancels or leaves pending;
- which topology fields are exact and which are best-effort.

## Rock 2: Worker-Held Resource Accounting

Resource reports must include resources still held by worker-lane commands after
runtime tables stop accepting new work.

Required behavior:

- table-owned and worker-held resource counts are distinguishable or summed
  honestly;
- canceled-but-not-finished lane work remains visible until finished;
- terminal shutdown report is not clean if worker-held resources remain;
- no double-count after late completion drains;
- tests cover TLS, process, storage, DNS, TCP where each has a meaningful owned
  resource or pending-work shape.

## Rock 3: Bounded Drain / Join Rules

Each lane gets one explicit shutdown rule:

- finish quickly;
- cancel and drain completions;
- bounded wait;
- tombstone and report remaining work.

No lane may block runtime shutdown forever because user timeout was huge. If an
OS operation cannot be interrupted, report it as remaining worker-held work and
keep the lifecycle honest.

## Rock 4: Raw OS Signal Capture

Add live OS signal capture for the smallest useful set:

- Unix: `SIGINT` and `SIGTERM` if platform support is clean;
- non-Unix: explicit unsupported capability is acceptable.

Signals must enter Tina as runtime-owned signal completions. They must be
bounded, traceable, cancelable on requester stop, and simulator-compatible.

No broad signal framework. No daemon/service-manager integration.

## Rock 5: Failed-Shard Cleanup

A failed shard must become a hard boundary:

- ingress rejects;
- cross-shard sends/calls reject;
- pending local driver work is canceled/tombstoned;
- pending cross-shard request/reply work reaches one terminal outcome;
- healthy shards continue;
- topology and terminal reports name the failed shard and remaining resources.

Do not add automatic shard restart unless this phase proves ownership and
address-generation semantics. Quarantine first.

## Rock 6: Topology And Shutdown Proof

Topology and terminal reports must be useful to operators and tests:

- shard state;
- ingress/remote pressure;
- lane capacities;
- configured resource capacities;
- owned and worker-held resource counts;
- dropped trace count;
- failed shard ids;
- clean vs unclean shutdown reason.

Health is observation, not hidden correctness state.

## Rock 7: DST And E2E Pressure

Add tests that combine rocks:

- shutdown during TLS accept/handshake/read/write;
- shutdown during storage/process/DNS work;
- signal arrives while shutdown is draining;
- shard fails while cross-shard call is in flight;
- remote queue full plus requester timeout;
- late completions after canceled work;
- topology checked before, during, and after pressure.

DST should throw weird combinations. Normal tests still pin known negative
paths. DST does not replace boring direct tests.

## Done Means

- `make verify` passes.
- New lifecycle tests prove positive, negative, weird, and shutdown paths.
- `review.md` has audit plus positive/blast-radius/hostile review.
- `SYSTEM.md`, `ROADMAP.md`, and `CHANGELOG.md` tell only landed truth.

## Non-Goals

- No remoting, clustering, membership, or placement.
- No flow syntax.
- No Tower/Axum-inside-Tina story.
- No durable mailbox or exactly-once claim.
- No broad performance claim.
- No new async runtime.
