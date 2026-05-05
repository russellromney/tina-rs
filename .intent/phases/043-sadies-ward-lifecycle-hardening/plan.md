# Phase 043: Sadie's Ward Lifecycle Hardening

## Goal

Make Tina's live local system boring under shutdown, failed shards, worker-lane
work, and OS signals.

At closeout:

> Tina can stop or fail a live local system without pretending hidden work or
> resources disappeared.

No flow syntax, remoting, clustering, release docs, metrics backend, or broad
performance claim.

## Handoff Rules

- Execute the rocks in order.
- After each rock, run the narrow tests for that rock and review the diff for
  bugs before moving on.
- Ask before changing public Tina vocabulary beyond fields named here.
- Ask before adding a dependency other than `signal-hook`.
- Ask before adding unsafe code.
- Ask before weakening a test because live timing is inconvenient.
- Do not skip a rock and call the phase done.

## Vocabulary

- **Table-owned resource:** resource id stored in runtime/driver tables:
  TCP listener/stream, TLS listener/stream, UDP socket, file.
- **Worker-held resource:** OS/resource handle cloned into active lane work but
  not necessarily represented by a live table id.
- **Pending call:** runtime-owned operation waiting for completion.
- **Tombstoned work:** canceled work whose late completion must be swallowed and
  traced/reported.
- **Clean shutdown:** all shards closed and table-owned count, worker-held
  count, and pending-call count are zero.

## Rules

- No hidden fallback queues.
- No clean shutdown while worker-held resources or pending calls remain.
- Resource reports must not count only convenient tables.
- Failed shards reject later ingress/sends/calls.
- Health is observation, not hidden correctness state.
- Direct tests pin known bad paths; DST combines weird paths.

## Build Order

1. Audit current lifecycle in `review.md`.
2. Add table-owned / worker-held / pending-call accounting.
3. Add bounded lane shutdown rules.
4. Add raw OS signal capture.
5. Harden failed-shard cleanup.
6. Expand topology/shutdown reports.
7. Add e2e and DST pressure.
8. Write positive, blast-radius, hostile review; fix findings.

## Rock 1: Audit

Write facts in `review.md`. Cover storage, DNS, TLS, process, signal, and
TCP/Betelgeuse. For each: what can block, what can cancel, what can only be
tombstoned, what is table-owned, what is worker-held, and what shutdown reports.

## Rock 2: Resource Accounting

Add or tighten these report fields:

- `owned_resource_count`
- `worker_held_resource_count`
- `pending_driver_call_count`
- `shutdown_unclean_reason`

Expected public shape:

- `LiveShardReport::{owned_resource_count, worker_held_resource_count,
  pending_driver_call_count}`
- `LocalSystemShutdownReport::{remaining_owned_resource_count,
  remaining_worker_held_resource_count, remaining_pending_driver_call_count,
  unclean_reason}`
- `ShutdownUncleanReason` or similarly named enum, not a stringly value.

Count rules:

- TCP/file/UDP Betelgeuse ids count as table-owned.
- TLS listener/stream table ids count as table-owned.
- TLS accept/read/write/close holding cloned listener/stream counts as
  worker-held until completion drains.
- Process child counts as worker-held while running.
- DNS/storage blocked worker jobs count as pending calls; count worker-held only
  if they own a real handle.
- Signal waits count as pending calls, not resources.

No double-count after late completion drains. Remaining worker-held or pending
work makes shutdown unclean.

## Rock 3: Bounded Shutdown

Add one shutdown budget in `LocalSystemConfig`:

- `shutdown_lane_drain_timeout`
- default: `Duration::from_millis(100)`
- applies per shard to lane-worker drain after cancellation

Each lane must do:

1. stop new work;
2. cancel queued/pending work;
3. drain completions until budget;
4. join if finished;
5. report remaining worker-held/pending work if not finished.

No lane may block shutdown forever because user operation timeout was huge.
If a lane cannot stop inside the budget, shutdown still returns with an
unclean report.

## Rock 4: OS Signals

Expected implementation: use `signal-hook` or similarly small sync-safe crate.

Rules:

- Unix: support `SIGINT` and `SIGTERM`.
- Non-Unix: explicit unsupported capability is okay.
- No Tokio dependency.
- No async signal task.
- No custom unsafe handler unless reviewed.
- Signals become runtime-owned signal completions.
- Signal waits are bounded, traceable, cancelable on requester stop, and
  simulator-compatible.

## Rock 5: Failed Shards

A failed shard is quarantined.

Priority for races:

1. requester already stopped/full wins for requester-local completion;
2. shard failed wins over later success;
3. timeout wins if requester deadline fired before failure observed;
4. full transport/mailbox wins only when no terminal state already exists.

Required behavior:

- ingress to failed shard rejects;
- sends/calls to failed shard reject;
- pending local driver work is canceled/tombstoned;
- in-flight cross-shard request/reply gets exactly one terminal outcome;
- healthy shards continue;
- topology and terminal report name failed shard and remaining work.

No automatic shard restart.

Do not invent peer liveness, remoting, membership, or network failure
semantics. This is local worker-thread failure only.

## Rock 6: Reports

Topology/shutdown reports must expose:

- shard state;
- ingress/remote pressure;
- lane capacities;
- configured resource capacities;
- table-owned resources;
- worker-held resources;
- pending driver calls;
- dropped trace count;
- failed shard ids;
- clean/unclean reason.

No Prometheus, tracing subscriber, dashboard, metrics sink, or observability
framework.

## Rock 7: Proof

Proof mode table:

| Behavior | Proof |
|---|---|
| Count rules | unit + live e2e |
| Bounded lane drain | unit + live e2e |
| OS signal delivery | live e2e on supported platform; unsupported test elsewhere |
| Failed-shard race priority | live e2e + simulator/DST where modeled |
| Topology/shutdown fields | live e2e |
| Cross-rock weirdness | DST |

Must test:

- shutdown during TLS accept/handshake/read/write;
- shutdown during storage/process/DNS work;
- signal during drain;
- shard failure during cross-shard call;
- remote full plus requester timeout;
- late completion after cancellation;
- topology before/during/after pressure.

Minimum named tests to add or update:

- one unit test per lane count rule where practical;
- one live LocalSystem test for in-flight TLS shutdown;
- one live LocalSystem test for in-flight process shutdown;
- one live LocalSystem test for DNS/storage pending-call accounting using
  crate-private hooks if needed;
- one live signal test on Unix, one unsupported/capability test on non-Unix;
- one live failed-shard cross-shard call test for each priority class that can
  be forced deterministically;
- one DST history that combines shutdown, late completion, and topology;
- one DST history that combines shard failure, remote full, and timeout.

Test hooks may be crate-private/test-only. Do not turn them into user API.

## Done Means

- `make verify` passes.
- New tests cover positive, negative, weird, and shutdown paths.
- `review.md` has audit plus positive/blast-radius/hostile review.
- `ROADMAP.md` and `CHANGELOG.md` tell only landed truth.

## Non-Goals

- No remoting, clustering, membership, or placement.
- No flow syntax.
- No Tower/Axum-inside-Tina story.
- No durable mailbox or exactly-once claim.
- No broad performance claim.
- No new async runtime.
