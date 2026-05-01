# 020 Galileo Multi-Shard Semantics and Simulation Plan

Session:

- A

## What We Are Building

Build the next major implementation phase after Voyager: make `tina-rs`
honestly multi-shard in both runtime and simulator terms.

This is intentionally a large Galileo package. The point is not to land "some
cross-shard helper" or "a monoio experiment." The point is that
multi-shard ownership, delivery, routing, and replay become first-class,
directly proved, and reviewable.

Concretely, this package adds:

- a multi-shard explicit-step runtime model
- shard ownership and registration semantics
- cross-shard message delivery
- routing / placement rules honest enough for real workloads
- deterministic multi-shard trace behavior
- multi-shard `tina-sim` parity for the same semantics
- replayable multi-shard proof workloads
- at least one user-shaped multi-shard workload that proves the model end to
  end

The story Galileo must tell is:

> `tina-rs` is no longer only a single-shard discipline. The runtime and the
> simulator now agree on what it means for work to cross shards.

This is intentionally a large semantic phase. The review bar is correspondingly
high: Galileo should not close on "cross-shard send exists" alone, or on a
runtime implementation without simulator parity, or on simulator parity without
written ordering rules. If implementation discovers that the existing `tina`
boundary is too weak to express honest multi-shard semantics, or that the
semantic/runtime/simulator package is getting mixed up with monoio substrate
engineering, pause and split there. Otherwise keep the package together so
"multi-shard" means a real contract, not a teaser.

## What Will Not Change

- `tina` should not gain a broad new public vocabulary just to make multi-shard
  testing convenient. If honest multi-shard semantics require a real boundary
  change, pause and design it explicitly.
- This phase is about semantics and simulation, not production substrate work.
- This phase does **not** include `tina-runtime-monoio`.
- This phase does **not** include io_uring / reactor integration work.
- This phase does **not** include benchmark claims, NUMA tuning, or hardening
  stories.
- This phase does **not** include live shard migration, rebalancing, or
  consistent-hashing sophistication.
- This phase does **not** include cross-shard spawn placement or cross-shard
  supervision semantics unless the public boundary is explicitly extended and
  reviewed. Parent/child execution remains shard-local in this phase.
- This phase does **not** invent a second observable model for multi-shard
  behavior. Runtime and simulator must share the same event vocabulary.
- This phase stays explicit-step, synchronous, and single-threaded at the
  coordinator level. Concurrent shard execution belongs to the later substrate
  phase, not to Galileo.
- This phase does **not** require `Shard` trait expansion. If honest
  implementation needs new `Shard` trait methods, pause and design that as a
  real `tina` boundary change.
- Existing Mariner and Voyager single-shard behavior must stay green.

## How We Will Build It

- Start from the explicit-step runtime model. Galileo should make the semantics
  visible and directly testable before any substrate-specific backend enters
  the picture.
- Pin the coordinator shape up front:
  - Galileo uses one global explicit-step coordinator API
  - one global `step()` advances all shards in ascending shard-id order
  - for each destination shard, inbound cross-shard harvest happens before that
    shard's handler snapshot for that same global step
  - harvest for global step `N` sees only cross-shard messages enqueued before
    step `N` began; a send produced during step `N` becomes first visible to a
    remote shard in step `N + 1`
  - there is no per-shard public stepping API in this phase
- Reuse the existing address discipline honestly:
  - addresses remain typed
  - addresses remain shard-qualified
  - shard ownership is semantically real, not decorative
- Keep the ingress shape additive and global:
  - harness/runtime ingress remains a global `try_send(addr, msg)`-style
    operation
  - ingress routes by `addr.shard()`; callers do not manually acquire a
    per-shard handle
  - workloads obtain remote `Address<M>` values through the existing public
    `Address` construction/generation surface; Galileo does not imply a
    Locator or Registry API
  - existing single-shard harnesses compile unchanged; Galileo adds sibling
    multi-shard coordinator surfaces rather than breaking current `Simulator<S>`
    workloads
- Keep current same-shard behavior intact:
  - same-shard `SendMessage` keeps the existing deterministic local-delivery
    story
  - shard-local spawn, call, timer, stop, panic, and supervision behavior
    preserve their current meanings
- Add cross-shard delivery through explicit shard-to-shard channels rather than
  a hidden global queue.
- Pin shard-pair channel boundedness honestly:
  - shard-pair cross-shard channels are explicitly bounded by config
  - overflow rejects at source-time through the live `SendRejected` /
    `SendRejectedReason::Full` vocabulary
  - Galileo must not hide unbounded buffering between shards
- Pin the multi-shard ordering contract:
  - within one mailbox, delivery remains FIFO by mailbox acceptance order
  - messages sent from one source isolate to one target isolate preserve send
    order even across shards
  - cross-shard delivery becomes visible only on a later destination-step after
    the source handler turn that produced it
  - when multiple source shards target the same destination in the same
    destination step, interleaving is deterministic by a fixed inbound harvest
    order, not "whatever the host scheduler happened to do"
  - request/reply causality must remain visible across shard boundaries:
    replies cannot become visible before the request reached the remote shard
- Pin a concrete harvest rule for the explicit-step runtime and simulator:
  - each destination shard drains inbound cross-shard channels in ascending
    source-shard order
  - each shard-pair channel is FIFO
  - each shard-pair channel is drained FIFO-to-empty before harvest moves to
    the next source shard; Galileo does not use round-robin one-message
    interleaving between channels
  - messages harvested in one destination step are accepted into destination
    mailboxes in that fixed order before the shard's handler snapshot for that
    step
- Pin the trace observability that makes the contract reviewable:
  - source-side cross-shard sends still emit `SendDispatchAttempted` followed
    by `SendAccepted` or `SendRejected` in the source shard's event record
  - accepted cross-shard sends become destination-side `MailboxAccepted` events
    on harvest in the destination shard's event record
  - cross-shard request/reply causality is proved against that source/destination
    event shape rather than only against final workload output
- Keep event and cause identity global:
  - `EventId` stays globally monotonic across all shards in one run
  - cause ids remain globally monotonic and link events across shards without a
    per-shard namespace
  - existing structural monotonicity/replay checkers should remain meaningful
- Keep placement/routing explicit and honest:
  - root registration may choose an owning shard explicitly
  - runtime and simulator route strictly by `Address<M>.shard()`
  - user workloads may route by a stable owner function (for example
    `hash(key) % shard_count`) rather than by migration magic
  - Galileo does not add a `Router` API; routing policy stays in workload code
  - once an isolate is placed, its shard ownership remains stable for its
    lifetime in this phase
- Extend `tina-sim` to model the same multi-shard rules rather than inventing a
  simulator-only delivery discipline.
- Keep the first user-shaped multi-shard proof workload simple and load-bearing:
  a routed dispatcher/worker or tenant/session workload is preferable to a
  benchmark-shaped toy.
- Defer cross-shard perturbation to a later slice. 020's replay/determinism
  claims are about the baseline multi-shard ordering model itself, not about a
  new cross-shard fault DSL.
- Keep composition with prior perturbation surfaces honest:
  - 017's local-send perturbation remains same-shard only
  - 017 timer perturbation remains shard-local
  - 019 TCP perturbation remains shard-local
  - one shared global seed still drives all surfaces, with each surface tagging
    it by its own constant and scope
  - when a seeded surface needs an ordinal under the global coordinator, that
    ordinal is per-shard/per-surface rather than one cross-shard global ordinal
- Keep runtime-owned calls shard-local:
  - sleep timers and TCP listeners/streams remain owned by the issuing shard
  - completion messages still return to the requesting isolate on that shard
  - any later cross-shard communication triggered by those completion handlers
    flows through ordinary `SendMessage`
- Pin programmer-error misuse up front:
  - attempting cross-shard spawn placement or cross-shard `RestartChildren`
    semantics in 020 is a programmer error and may panic
  - registering a root isolate on a non-existent shard id is a programmer error
    and may panic

## Build Steps

1. Add sibling multi-shard coordinator surfaces while keeping existing
   single-shard runtime/simulator entrypoints intact.
2. Extend the runtime model from one shard to a fixed set of explicit-step
   shards driven by one global `step()`.
3. Add explicit shard ownership at registration time for root isolates.
4. Add bounded shard-pair cross-shard queues for `SendMessage` delivery.
5. Implement deterministic inbound harvest on each destination shard:
   - ascending source-shard order
   - FIFO within each shard-pair channel
   - drain each channel to empty before moving to the next source shard
   - acceptance before the destination shard's handler snapshot for that step
6. Preserve existing same-shard semantics unchanged.
7. Preserve existing shard-local runtime-owned call, timer, spawn, stop, panic,
   and supervision behavior for isolates once placed on a shard.
8. Extend cross-shard rejection behavior honestly:
   - shard-pair queue overflow rejects at source-time through
     `SendRejected { reason: Full, .. }`
   - stale, stopped, or unknown remote targets discovered on destination
     harvest reject through the live `SendRejected { reason: Closed, .. }`
     vocabulary in the destination shard's event record
   - runtime ingress continues to distinguish programmer error from ordinary
     delivery rejection the same way the current runtime does
9. Add a simple explicit routing / placement story suitable for proof
   workloads:
   - deterministic owner function
   - no migration
   - no rebalancing
   - runtime routes by `Address<M>.shard()` only
10. Keep global event/cause identity deterministic across shards.
11. Define multi-shard quiescence honestly:
    - all shards locally quiescent
    - all shard-pair channels empty
12. Extend trace-based replay and deterministic repeated-run proof to the
   multi-shard runtime.
   - replay artifacts continue using one whole-run `Vec<RuntimeEvent>` rather
     than splitting into per-shard trace artifacts
13. Extend `tina-sim` to multiple shards using the same delivery/ordering
    rules.
   - existing 016-019 single-shard simulator workloads must compile unchanged
     and stay green without caller rewrites
14. Add direct proofs for:
    - root registration and ownership on multiple shards
    - same-shard behavior unchanged
    - `try_send(addr, msg)` ingress routing by `addr.shard()`
    - cross-shard send delivery
    - bounded shard-pair queue rejection at source-time
    - per-source -> per-target FIFO across shards
    - three-message FIFO across one shard-pair channel
    - deterministic multi-source interleaving on one destination shard
    - N>=3 shard harvest ordering
    - cross-shard stale/closed rejection
    - cross-shard unknown-isolate rejection
    - request/reply causality across shards
    - deterministic repeated runs in runtime and simulator
    - replay from saved config in the simulator
15. Add at least one user-shaped multi-shard workload:
    - preferred: two-shard dispatcher/worker proof with routed jobs and
      cross-shard completions
    - acceptable alternative: routed tenant/session workload with stable owner
      placement
    - the workload must also exercise a visible cross-shard `SendRejected`
      path, not only the green path
    - the workload must name whether that rejected path proves `Full` or
      `Closed`
16. Close out docs/evidence:
    - `.intent/SYSTEM.md`
    - `ROADMAP.md`
    - `CHANGELOG.md`
    - phase `commits.txt`
    - review receipt

## How We Will Prove It

Direct proof for the changed behavior:

- at least one runtime-path workload must execute on more than one shard
- at least one simulator-path workload must execute on more than one shard
- focused runtime and simulator tests for:
  - explicit root placement on shard 0 vs shard 1
  - typed addresses retaining correct shard ownership
  - single-shard 016-019 simulator workloads compiling and running unchanged
  - same-shard sends still behaving exactly as before
  - global `step()` / global `try_send(addr, msg)` coordinator semantics
  - in-step cross-shard visibility follows the next-step-only rule
  - cross-shard sends becoming visible only on later destination steps
  - source-side `SendDispatchAttempted` / `SendAccepted` or `SendRejected`
    trace shape
  - destination-side `MailboxAccepted` trace shape on harvest
  - shard-pair queue boundedness and `Full` rejection at source-time
  - per-source -> per-target FIFO across shards
  - three-message FIFO on one shard-pair channel
  - deterministic multi-source interleaving to one destination
  - N>=3 shard inbound harvest ordering
  - request/reply causality across shards
  - stale-address rejection across shards
  - unknown-isolate rejection across shards
  - stopped-target rejection across shards
  - global event-id monotonicity across all shards
  - multi-shard quiescence only when all shards and all shard-pair channels are
    empty
  - deterministic repeated runs with the same event record and causal shape
  - simulator replay reproducing the same multi-shard run
- user-shaped e2e proof for:
  - a routed multi-shard dispatcher/worker or tenant/session workload
  - cross-shard requests and replies
  - stable placement over the workload lifetime
  - cross-shard `SendRejected` visible on a real workload path
  - deterministic completion/output record

Proof modes expected in this package:

- unit proof:
  shard-pair queue bookkeeping and deterministic harvest ordering
- integration proof:
  multi-shard runtime delivery semantics
- e2e proof:
  user-shaped multi-shard workload
- replay proof:
  saved simulator config reproduces the same multi-shard run
- no adversarial proof:
  cross-shard perturbation/checker work is explicitly deferred to 021
- blast-radius proof:
  all prior Mariner and Voyager suites still pass

## Done Means

Galileo is done when:

- `tina-rs` is honestly multi-shard in both runtime and simulator terms
- the cross-shard contract is written down, not inferred from tests
- cross-shard delivery, ordering, and rejection behavior are directly proved
- a user-shaped workload exercises the model end to end
- repeated seeded runs are deterministic and replayable
- replay remains one whole-run event record rather than a per-shard artifact
  family
- multi-shard semantics land in the current explicit-step runtime line and
  simulator line rather than in a new substrate crate
- no part of the closeout story depends on benchmark theater or monoio
  substrate work
