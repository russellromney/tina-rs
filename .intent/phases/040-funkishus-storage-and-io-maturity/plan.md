# Phase 040: Funkishus Storage And I/O Maturity

## Goal

Make Tina's local runtime I/O story feel real enough for serious local
server-shaped programs.

Funkishus is the phase where runtime-owned I/O stops being "TCP plus a few
local files" and becomes a coherent driver contract for the kinds of resources
Tokio-shaped applications actually touch: storage, DNS, UDP, TLS rails,
processes, signals, cancellation, shutdown, and adapter policy.

This is not a demo phase. This is not a docs phase. This is production-core
work.

At closeout, Tina should be able to say:

> A local Tina app can run bounded shared-nothing shards, submit runtime-owned
> network/storage/process/signal work through named driver rails, observe
> overload and cancellation as typed outcomes, shut down without hidden zombie
> operations, and pressure the same semantics through direct tests and DST.

## Why Now

Timmerhus made live topology first-class:

- per-shard lifecycle reports;
- ingress and remote queue pressure reports;
- terminal topology snapshots;
- worker failure visibility;
- live-vs-simulator topology/failure DST.

That makes the next weakness obvious. Tina can explain which local shards are
alive, but its I/O/storage substrate is still not broad or mature enough to
support a full Tokio-ish local service without escape hatches.

The core rule from the README stays load-bearing:

> If something can overload, Tina should make it visible.
> If something can fail, Tina should make it traceable.
> If something can race, Tina should make it replayable.

Funkishus applies that rule to the rest of the local I/O story.

## Current Baseline

Already landed before Funkishus:

- synchronous Tina handlers returning `Effect`;
- typed addresses, bounded mailboxes, visible `Full`/`Closed`;
- single-shard and multi-shard explicit-step runtimes;
- deterministic simulator and first-class DST harness;
- live `ThreadedRuntime` worker runtime over the Betelgeuse backend;
- live fixed multi-shard local runtime;
- canonical `LocalSystem`;
- narrow Tokio/Tower/Axum bridge;
- runtime-owned timers;
- runtime-owned TCP accept/read/write/connect/close;
- runtime-owned local file read/write;
- local snapshot/journal persistence;
- bounded live storage lane for file/persistence admission;
- cancellation and shutdown proofs for existing TCP/storage paths;
- topology reports for live shard and remote queue pressure.

Known gaps:

- storage work is bounded and lane-backed on live paths, but the lane-backed
  blocking contract is not yet fully named, reported, and stressed as the
  production shape;
- platform durability support is honest but still thin;
- DNS, UDP, TLS, process, and signal rails do not exist;
- unsupported I/O capabilities are not yet all expressed as typed Tina
  outcomes;
- adapter policy beyond the current ThreadedRuntime-over-Betelgeuse shape is not sharp
  enough;
- cancellation semantics are strong for current paths but not generalized to
  new resource families;
- DST coverage is good, but new I/O rails must be born DST-native rather than
  patched later;
- no single user-shaped e2e workload exercises many I/O rails together.

## Non-Goals

These are important, but not Funkishus:

- no remoting;
- no clustering;
- no durable mailbox;
- no database;
- no exactly-once promise;
- no public release/Gemini story;
- no broad performance claim;
- no new general-purpose async runtime;
- no flow macro ergonomics;
- no hidden Tokio fallback;
- no API that pretends unsupported I/O works.

## Design Principles

1. **Tina owns semantics; drivers own completions.**
   A substrate may provide readiness/completion mechanics. It may not smuggle in
   unbounded queues, invisible retries, hidden background tasks, or vague
   cancellation.

   Named bounded driver lanes are allowed. A storage, DNS, process, or signal
   lane is not a hidden task if it has configured capacity, reported capacity,
   bounded accepted running work, runtime-owned lifecycle, tested shutdown, and
   no recursive unbounded internal queue.

2. **Unsupported is a typed outcome.**
   If DNS, UDP, TLS, process, signal, directory fsync, rename replacement, or
   any driver capability is unavailable on a platform/backend, the user sees a
   typed `Unsupported`/capability result and trace event. No silent fallback.

3. **Blocking work must be named.**
   If a first slice still performs synchronous filesystem work, the public
   capability report and docs/tests must say so. The normal live app path should
   not accidentally stall a shard worker on slow disk.

4. **Cancellation is ownership.**
   A requester that stops must cancel or tombstone its owned pending work. A
   cancellation for one call must not close a shared resource unless that
   resource ownership rule is explicit and tested.

5. **Every queue has a bound.**
   Driver ingress, storage lanes, DNS lanes, process lanes, UDP receive/send
   queues, TLS handshakes, and bridge-adapter queues must have visible capacity
   or explicit unsupported status. No `mpsc fallback` that hides pressure.

6. **DST starts with the feature.**
   New rails need hand-authored positive/negative tests and generated weird
   histories from the start. Happy path first, weird path same phase.

7. **One app story, not many mini demos.**
   Funkishus should end with at least one composed local service that uses
   several rails together under overload, cancellation, shutdown, and recovery.

8. **Implementation must move real rocks.**
   Capability reports that honestly say `Unsupported` are good. A phase that
   mostly adds `Unsupported` labels is not good enough. Funkishus must land
   concrete storage maturity, UDP, process, and composed-service proof.

9. **Resource helpers stay out of `tina`.**
   `tina` owns the isolate/effect/address words. Resource-specific helpers and
   runtime call constructors live in `tina-runtime` and its prelude. The
   simulator mirrors the runtime vocabulary instead of inventing a second model.

## Minimum Resource Commitments

Funkishus is allowed to be honest about unsupported capabilities, but the
minimum expected closeout is pinned:

| Resource family | 040 direction |
|---|---|
| Storage execution | Implement and prove `LaneBackedBlockingStorage` as the preferred live shape: bounded admission, no shard-worker disk blocking on canonical live paths, visible pressure, tombstoned/canceled queued work, and honest already-started non-preemption. Do **not** build a full storage reactor in 040 unless the lane-backed model proves impossible. |
| Platform durability | Harden current support table and recovery semantics. Directory fsync, rename replacement, commit-uncertain, relative paths, and monotonic append rejection must be tested. |
| DNS | Either implement a bounded lane-backed resolver with queued-cancel/started-tombstone semantics, or close live DNS as typed unsupported with a direct reason. Simulator DNS semantics still land either way. |
| UDP | Implement runtime-owned UDP live loopback plus simulator packet scripting. This is a real 040 deliverable, not optional unsupported. |
| TLS | Do **not** implement native TLS in 040. Ship a typed TLS adapter/unsupported rail that makes Tina's non-claim explicit and testable. Native TLS is later work. |
| Process | Implement a narrow bounded process rail: command plus args, run/spawn-wait, bounded captured output, timeout/cancel kill-and-reap policy, simulator scripts. No shell-by-default helper and no interactive process streaming in 040. |
| Signal | Implement simulator signal injection. Live signal support lands only where deterministic and safe to test; otherwise live signal capability is typed unsupported. No global handler may bypass Tina traces. |

## Expected User Shape

The exact names may change during implementation, but the user-level shape
should read like ordinary Tina:

```rust
use tina::prelude::*;

#[tina_runtime::isolate(message = GatewayMsg, reply = GatewayReply, shard = AppShard)]
impl Gateway {
    fn handle(&mut self, msg: GatewayMsg, ctx: &mut Context<Self>) -> Effect<Self> {
        match msg {
            GatewayMsg::Resolve(name) => {
                dns_lookup(name, Duration::from_millis(200))
                    .reply(GatewayMsg::Resolved)
            }
            GatewayMsg::Resolved(Ok(addrs)) => {
                udp_send(self.socket, addrs[0], b"llama?".to_vec())
                    .reply(GatewayMsg::ProbeSent)
            }
            GatewayMsg::Persist(bytes) => {
                journal_append(self.journal.clone(), self.next_index, bytes)
                    .reply(GatewayMsg::Persisted)
            }
            GatewayMsg::Shutdown => stop(),
            GatewayMsg::Resolved(Err(err)) => reply(GatewayReply::Rejected(err.into())),
            GatewayMsg::ProbeSent(outcome) => reply(GatewayReply::Udp(outcome)),
            GatewayMsg::Persisted(outcome) => reply(GatewayReply::Stored(outcome)),
        }
    }
}
```

Important: this is still effect-returning Tina. No `await` cosplay. Runtime
owned work is visible in the message names and traces.

## Capability Surface

Add or tighten a capability report that can answer, from the canonical app path:

- timer support;
- TCP support;
- local file support;
- local persistence support;
- storage execution shape;
- DNS support;
- UDP support;
- TLS support or explicit adapter rail status;
- process support;
- signal support;
- directory fsync support;
- rename replacement support;
- cancellation support per resource family;
- whether a resource family is simulated, native, blocking, lane-backed, or
  unsupported.

Required access points:

- `LocalSystem::capabilities()` or equivalent;
- `LocalMultiShardSystem::capabilities()` or equivalent;
- terminal reports preserve final topology/capability context where relevant;
- the Tokio bridge exposes a bridge capability projection only, not a second
  full runtime capability model.

Expected rough names:

- `RuntimeCapabilities`;
- `DriverCapabilityReport`;
- `ResourceSupport`;
- `ResourceExecutionShape`;
- `CancellationSupport`.

The vocabulary is structured by axis, not one giant enum:

- support: `Supported`, `Unsupported`, `SimulatedOnly`, `AdapterOnly`;
- execution: `Inline`, `LaneBackedBlocking`, `CompletionBacked`,
  `ExternalAdapter`;
- cancellation: `CancelableBeforeStart`, `TombstonedAfterStart`,
  `ResourceCloseOnly`, `NotCancelable`;
- shutdown: `Drained`, `Canceled`, `Tombstoned`, `Unsupported`;
- durability: platform support fields for file fsync, parent-directory fsync,
  rename replacement, and commit-uncertain possibility.

Avoid too many names if existing capability structs can grow cleanly, but do
not flatten these axes into prose or ambiguous booleans.

## Build Steps

### 1. Audit Current Driver And Storage Shape

Inventory current driver-owned resources:

- timers;
- TCP listener/stream calls;
- outbound TCP connect;
- local file reads/writes;
- snapshot/journal persistence;
- storage lane admission;
- shutdown/cancel/tombstone paths;
- simulator durable image and storage faults;
- live topology reports;
- bridge/app capability reports.

Write findings in `review.md`, not a separate audit file.

Done means Funkishus starts from exact current semantics:

- what blocks;
- what is lane-backed;
- what is cancellable;
- what is tombstoned;
- what is unsupported;
- what is simulated;
- what is live-native.

### 2. Pin Driver Capability Vocabulary

Create or tighten the structured resource capability report:

- support axis;
- execution axis;
- cancellation axis;
- shutdown axis;
- durability axis.

Do not expose one mush enum. Do not expose a pile of booleans when a small enum
per axis makes the contract clearer.

Required proof:

- capability report has direct unit/integration tests;
- unsupported capabilities are visible through the same surface as supported
  ones;
- no "fallback" word appears in the public contract unless it means a typed
  explicit user choice.

### 3. Mature Storage Execution Shape

Implement the 040 storage target: **lane-backed blocking storage**.

This is the production-shaped storage answer for now, not a fallback and not a
failed nonblocking reactor. The promise is bounded live storage execution that
does not block shard workers.

- keep storage work off the shard worker on live paths;
- make storage lane capacity visible in topology/capabilities;
- keep accepted pending storage bounded;
- cancel/tombstone requester-owned work;
- preserve existing append-before-apply and snapshot/journal semantics.
- explicitly report `LaneBackedBlockingStorage`;
- direct tests show a slow storage op does not stop unrelated shard message
  handling;
- roadmap note for actual platform async storage only if evidence says the
  lane-backed model is insufficient.

Named lane contract for storage and any other lane-backed resource:

- configured capacity is public and reportable;
- accepted running work counts against capacity until completion is harvested
  or tombstoned;
- queued work can be canceled before start;
- started work may be tombstoned if the platform operation cannot be preempted;
- lane shutdown cancels queued work and drains or tombstones started work
  according to the resource cancellation matrix;
- no lane may hide pressure in an unbounded internal queue.

Do not build a full storage reactor in 040 unless the lane-backed target proves
impossible. If that happens, stop and ask.

### 4. Harden Platform Durability Support

Tighten local persistence support:

- directory fsync support;
- rename replacement semantics;
- commit-uncertain behavior;
- current-directory relative paths;
- temp file cleanup behavior;
- recovery after partial/truncated/corrupt journal;
- append monotonicity at append time;
- platform-specific support table.

Required proof:

- Unix and non-Unix support tables are tested with cfg-specific assertions;
- commit-uncertain is directly tested;
- rejected journal indexes do not mutate app state in e2e tests;
- relative paths work or reject visibly;
- recovery accepts the latest safe snapshot plus valid journal suffix.

### 5. Add DNS Rail

Add runtime-owned DNS lookup shape.

Expected semantics:

- bounded driver admission;
- timeout required or inherited from runtime call deadline;
- cancellation on requester stop;
- typed outcomes: resolved, failed, timed out, canceled/requester closed,
  unsupported;
- simulator can script success/failure/timeout/reorder;
- live implementation may use a bounded lane-backed system resolver only with
  explicit queued-cancel and already-started tombstone semantics;
- if the live resolver cannot be bounded honestly, live DNS ships as typed
  unsupported with tests and a direct reason.

No hidden unbounded resolver thread pool.

### 6. Add UDP Rail

Add runtime-owned UDP socket operations.

Expected operations:

- bind/open;
- send_to;
- recv_from;
- close;
- maybe local address query if needed by tests.

Expected semantics:

- bounded pending receive/send work;
- visible datagram truncation if buffer too small;
- cancellation on requester stop;
- close rejects or cancels pending operations;
- simulator can script packet delivery, loss, reorder, and error;
- live path has direct local loopback e2e proof.

### 7. Add TLS Rail Without Lying

TLS can be large. Funkishus should still make the rail explicit.

040 must not implement native TLS.

Acceptable closeout:

- **adapter rail only:** typed capability says TLS is supported only through an
  explicit user-supplied adapter/service isolate; or
- **typed unsupported:** expose `Unsupported` with tests and a roadmap home.

The phase must not pretend raw TCP equals TLS.

### 8. Add Process Rail

Add runtime-owned process operation shape for local services that shell out.

Expected first slice:

- narrow `run_command` / `spawn_wait` shape with bounded command admission;
- command plus args is the safe path; no shell-by-default helper;
- capture exit status;
- bounded stdout/stderr capture with explicit output caps;
- no interactive stdin streaming in 040;
- output cap produces a typed truncation/full outcome;
- timeout/cancel for a started child attempts kill and wait/reap according to a
  bounded named policy;
- if kill/reap cannot be proven, surface a typed `KillUncertain` or equivalent
  outcome and trace event;
- capture either drains and truncates stdout/stderr, or rejects the capture mode
  before spawn;
- no unbounded output buffering;
- simulator scripts exit/signal/output/truncation.

Full process I/O is not the goal. Features outside this narrow rail must be
typed unsupported.

### 9. Add Signal Rail

Add runtime-owned signal subscription/notification shape.

Expected first slice:

- simulator signal injection is mandatory;
- subscribe to shutdown-ish live signals only where platform-supported and safe
  to test;
- deliver signal events as Tina messages;
- bounded subscription/event queue;
- cancellation/unsubscribe on isolate stop;
- unsupported platforms report typed unsupported.

Signal discipline:

- one runtime-owned signal registry per app/runtime;
- no raw global handler calls isolate code;
- signal events enter Tina through bounded runtime-owned delivery;
- tests must not send process-wide signals that can kill or perturb the test
  runner;
- if live support uses an explicit test injection hook, name it as test
  injection rather than OS-signal proof.

### 10. Resource Cancellation Matrix

Build one explicit matrix covering:

- timer;
- TCP accept/read/write/connect;
- file read/write;
- snapshot/journal;
- DNS;
- UDP send/recv;
- TLS rail;
- process;
- signal.

For each resource family, pin:

- owner identity;
- what cancel means;
- what close means;
- whether late completions are swallowed, traced, or delivered as failure;
- whether shared-resource cancellation is legal;
- how requester stop behaves;
- how runtime shutdown behaves;
- what simulator does.

This matrix belongs in `review.md` and/or committed docs under the phase. It
must be reflected by tests, not just prose.

### 11. Adapter Policy

Write and enforce a driver-adapter policy:

- `tina` trait crate does not grow resource-specific helpers;
- resource-specific helpers live in `tina-runtime` and its prelude;
- existing runtime-owned call shape remains the extension point;
- simulator mirrors runtime event/outcome vocabulary;
- no hidden unbounded queues;
- bounded command ingress;
- explicit capability report;
- explicit cancellation story;
- no blocking shard worker unless capability says so;
- no implicit fallback to Tokio/mpsc/threadpool;
- adapter-specific differences are typed and tested;
- simulator backend remains the oracle for replayable semantics.

This is where Betelgeuse/native/Tokio-current-thread/monoio-shaped future
drivers get their rules without implementing all of them now.

### 12. Composed User-Shaped E2E Service

Build one app-shaped test workload, not a toy:

- TCP ingress accepts requests;
- service does UDP work;
- service runs one bounded process call;
- state shard applies bounded updates;
- storage shard journals/snapshots state;
- timer drives retry/timeout;
- cancellation path stops requester while work is pending;
- overload path fills a queue/lane and gets visible `Full`;
- shutdown path happens while resource work is pending;
- topology/terminal report explains what happened.

The test must assert behavior, not print logs:

- accepted request mutates state only after durable append succeeds;
- rejected, timed-out, canceled, or unsupported request does not mutate state;
- DNS/UDP failure is visible in reply and trace;
- overload returns typed `Full`, `StorageFull`, or another named rejection;
- shutdown leaves no hidden pending work;
- restart/recovery rebuilds expected state from snapshot/journal;
- terminal topology explains queue pressure and shard lifecycle.

DNS and signal may have separate direct/DST tests when live support is
unsupported or platform-specific. The composed service must still exercise UDP
and process, because those are concrete 040 rails.

### 13. DST Expansion For I/O

Add generated histories around new and matured rails:

- storage slow/failed/uncertain/recovery;
- DNS success/failure/timeout/unsupported;
- UDP packet delivery/loss/reorder/truncation;
- process exit/timeout/output cap/kill;
- signal delivery/unsubscribe;
- requester stop while any resource is pending;
- shutdown while many resources are pending;
- restart while pending resource work exists;
- cross-shard pressure plus resource completions.

Every generated failure must name seed/history and be deletion-shrinkable for
at least one new model.

Minimum DST bar:

- at least one new resource model has deletion shrinking;
- at least one composed I/O history has fixed regression seeds plus randomized
  seed sweep;
- generated histories force at least one success, one timeout/cancel, one
  full/closed, and one unsupported/failure path;
- every new rail has direct negative tests and either a DST model or an
  explicit reason why DST does not apply.

### 14. Review And Tighten Hot Paths

Run a hostile performance/allocation pass:

- no accidental per-step allocation in warmed simple paths;
- no topology/capability reporting locks on hot delivery paths unless proven
  cheap and necessary;
- no unbounded Vec growth in driver queues;
- no clone-heavy large buffers in UDP/file/persistence paths when avoidable;
- no trace record explosion in long generated histories without retention
  policy.

Do not make broad benchmark claims. Fix obvious waste.

## Required Tests

Direct tests:

- capability report names every supported and unsupported I/O family;
- storage lane slow operation does not block unrelated live shard handling;
- storage lane full is surfaced without sleeps-as-proof;
- requester stop cancels/tombstones pending storage work;
- commit-uncertain recovery shape remains visible;
- DNS success/failure/timeout/unsupported;
- UDP loopback send/recv and truncation;
- UDP pending recv canceled by requester stop;
- process success, timeout, output cap, and cancel/kill policy;
- signal injection in simulator and live unsupported/platform-supported shape;
- TLS rail reports adapter/native/unsupported honestly;
- shutdown with pending timer/TCP/file/persistence/DNS/UDP/process/signal work;
- topology/capability report after failure/shutdown includes resource pressure
  where supported;
- composed TCP plus DNS/UDP plus persistence service asserts user-visible
  outcomes.
- accepted request mutates state only after durable append succeeds;
- rejected/timeout/canceled/unsupported request does not mutate state;
- process output cap produces typed truncation/full result;
- signal registry unsubscribes on isolate stop without delivering to stopped
  isolate.

DST tests:

- DNS/UDP/process/signal histories replay byte-for-byte or compare semantic
  projections where raw trace differs;
- resource cancellation histories shrink;
- shutdown-with-pending-resource histories shrink;
- resource completions after requester stop never mutate stopped requester
  state;
- overload outcomes always settle as typed `Full`, `Closed`, `Unsupported`,
  `Timeout`, `Canceled`, or named failure;
- live-vs-sim projection for the composed service covers accepted work,
  rejected work, terminal state, durable output, and topology outcome.
- at least one new resource model deletion-shrinks a failing predicate;
- at least one composed I/O history runs fixed regression seeds and randomized
  seed sweep.

Verification:

- `cargo +nightly test -p tina-runtime`;
- `cargo +nightly test -p tina-sim`;
- targeted new integration tests;
- targeted DST seed sweep;
- `make verify`;
- `git diff --check`.

## Done Means

- Tina has structured capability axes for runtime-owned resources.
- Storage execution is honestly `LaneBackedBlockingStorage` and nonblocking
  from the shard perspective on canonical live paths.
- Platform durability support is explicit and tested.
- UDP and narrow process rails have live and simulator proof.
- DNS has simulator semantics and either bounded lane-backed live semantics or
  typed unsupported with a direct reason.
- Signal has simulator injection and platform-aware live support or typed
  unsupported.
- TLS is adapter-only or typed unsupported, not native.
- New resource families have bounded admission, visible failures, cancellation
  semantics, simulator support, and direct tests.
- A composed user-shaped service exercises multiple resource families together.
- DST can generate, replay, and shrink weird I/O histories.
- No hidden fallback path exists.
- Roadmap/changelog are updated honestly.

## Pause Gates

Stop and ask before continuing if:

- adding a resource requires a public `tina` trait change larger than extending
  runtime-owned call vocabulary;
- a platform API cannot provide honest cancellation without changing the
  resource ownership model;
- implementing native TLS is proposed;
- process or signal support grows beyond the pinned narrow slice;
- a proposed adapter needs an unbounded queue or hidden background task;
- storage maturation requires a full new runtime;
- a generated DST failure reveals a semantic ambiguity rather than a bug;
- the composed e2e service needs remoting/clustering to make sense.

## Non-Claims After Closeout

Even if Funkishus succeeds:

- Tina is still not a general Tokio replacement.
- Tina still does not claim remoting or clustering.
- Tina still does not claim durable mailboxes or exactly-once processing.
- Tina still does not claim broad performance superiority.
- Tina still does not claim every Tokio ecosystem library can run inside an
  isolate.
- Tina still does not claim all platforms support all I/O rails equally.

What Tina should claim is stronger:

> For local shared-nothing services, Tina has a bounded, traceable,
> replay-pressure-tested I/O substrate story that covers the common resource
> families honestly.
