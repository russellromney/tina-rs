# 020 Review

Session:

- B (review)

## Plan Review 1

Artifact reviewed:

- `.intent/phases/020-galileo-multi-shard-semantics-and-simulation/plan.md`

Reviewed against `.intent/SYSTEM.md`, the closeout state of 016-019, and
the current cross-shard story in code (today `tina-sim` panics on
`target_shard != self.shard.id()` for both `try_send` and dispatch, so
"cross-shard" in 020 is real semantic work, not just additional
plumbing).

### What looks strong

- Galileo is correctly framed as a **semantics** phase, not a substrate
  phase. The plan explicitly carves out monoio, io_uring, NUMA,
  benchmarks, migration, rebalancing, and consistent-hashing
  sophistication. That matches SYSTEM.md's standing rule that
  "cross-shard rules must be written down before multi-shard runtime
  work starts," and refuses the easy mistake of mixing semantics with
  reactor engineering.
- Cross-shard supervision and cross-shard spawn placement are
  explicitly **out of scope**. Parent/child execution stays shard-local.
  This is the right fence: it keeps Galileo from quietly redesigning
  018's surface while also doing 020's work.
- The ordering contract is named, not gestured at:
  - per-mailbox FIFO
  - per-source-isolate / per-target-isolate FIFO across shards
  - cross-shard messages only visible on a later destination step
  - deterministic multi-source interleaving via ascending source-shard
    inbound harvest
  - request/reply causality preserved across shards
  Compared to 019's pre-amendment `and/or` looseness, this plan is
  unusually pinned for a semantics phase. Good.
- Both runtime **and** simulator must implement the same rules, and the
  plan refuses a simulator-only delivery discipline. That protects the
  SYSTEM.md rule that the simulator must use the runtime's meaning
  model.
- "Done Means" is sharp: contract written down, not inferred from tests;
  benchmark theater explicitly forbidden; closeout cannot lean on
  monoio substrate work. This is the right closeout bar for a phase
  this size.
- Pause gates name the right escape valves: pause if honest semantics
  require a public `tina` boundary change, or if the package starts
  bleeding into substrate engineering.

### What is weak or missing

1. **Multi-shard step coordinator semantics are unpinned.**
   Today `Simulator::step()` advances one shard. The plan says
   "explicit-step runtime model" extended to "a fixed set of
   explicit-step shards" but does not say *who calls step on what*. Two
   honest shapes:
   - one global `step()` that drives all shards in a fixed shard-id
     order plus the cross-shard harvest at well-defined points, OR
   - per-shard `step_shard(shard_id)` with an explicit harvest API the
     test driver/runtime must call between shard steps.
   The harvest rule ("ascending source-shard order before the
   destination shard's handler snapshot") implies a coordinator. Pin
   the API shape so the proof tests can talk about it without
   reverse-engineering it from the implementation.

2. **Cross-shard channel boundedness / backpressure are unpinned.**
   Same-shard mailboxes are bounded; `Full`/`Closed` are load-bearing
   tina rules. Shard-pair channels carry the same risk, with a sharper
   asymmetry: the source shard's handler runs *before* the destination
   shard's harvest, so a `Full` queue could either reject at source-time
   (`SendRejectedReason::Full`) or silently buffer. Pin:
   - shard-pair channels are bounded by simulator/runtime config
   - overflow surfaces through the live `SendRejected` /
     `SendRejectedReason::Full` shape at source-time, not through hidden
     unbounded buffering or a `panic!`
   This is the same honesty bar 019 raised for
   `pending_completion_capacity` on the TCP path.

3. **Causality observability across shards is not pinned to specific
   trace events.**
   "Request/reply causality across shards" is a semantic claim. The
   plan does not say *which* events the reader can use to verify it.
   Today same-shard sends emit `SendDispatchAttempted` →
   `SendAccepted`/`SendRejected` on the source side and a later
   `MailboxAccepted` on the destination side. Pin: cross-shard sends
   produce a source-side `SendDispatchAttempted` /
   `SendAccepted`/`SendRejected` event in the source-shard event
   record, and a corresponding destination-side `MailboxAccepted` in
   the destination-shard event record on the harvest step. Without
   pinning the trace shape, "causality is preserved" can degrade to
   "no test caught a violation."

4. **Event-id and cause-id namespacing across shards is unpinned.**
   Today `next_event_id` is per-simulator and `EventId` carries no
   shard tag. With N shards, are event ids:
   - globally monotonic across shards (one shared counter), or
   - per-shard monotonic with `(shard_id, event_id)` as the unique
     key, or
   - some other scheme?
   Cause-id links across shards depend on this. The 017 structural
   `event_id_monotonicity` checker also depends on it. Pick one and
   pin it; flag downstream impact on existing structural checkers.

5. **Multi-shard quiescence is not defined.**
   Single-shard `run_until_quiescent` checks: no pending messages, no
   pending timers, no in-flight calls. The 019 closeout already had to
   widen this when TCP calls landed. Multi-shard quiescence adds a new
   condition: no pending cross-shard messages on any shard-pair channel.
   Pin the rule: a multi-shard run is quiescent only when every shard
   is locally quiescent **and** every shard-pair channel is empty.
   Without this, `run_until_quiescent` could stop early while a
   cross-shard message is in flight, mirroring exactly the 019 driver
   bug.

6. **API additivity for `Simulator<S>` is unpinned.**
   Every existing 016-019 test uses `Simulator::new(TestShard, ...)`
   with one shard. The plan does not say whether multi-shard support
   lands as:
   - a widening of `Simulator<S>` (`Simulator::new` keeps working with
     one shard; add `Simulator::new_multi_shard(...)` or similar), or
   - a new `MultiShardSimulator<...>` type (existing single-shard tests
     untouched), or
   - a generic-parameter change that requires all callers to migrate.
   The blast-radius claim "existing Mariner and Voyager single-shard
   behavior must stay green" depends on additivity. State which path
   you intend; my read: option 1 or 2 only, and existing tests should
   compile unchanged.

7. **Cross-shard ingress (`try_send`) shape is unpinned.**
   Today `try_send` panics on cross-shard. With multi-shard, a test
   harness needs to inject messages to isolates on any shard. Pin:
   - does `MultiShardSimulator::try_send(addr, msg)` route by
     `addr.shard()`, or
   - must the harness call `sim.shard(shard_id).try_send(addr, msg)`?
   Either is fine. Pick one. Same question for the live runtime's
   typed ingress.

8. **"Routing" is user-side, not runtime-side — say so.**
   The plan names "deterministic owner function (for example
   `hash(key) % shard_count`)" which is *workload code*, not runtime
   code. The runtime just delivers based on `Address<M>.shard()`. A
   reader could read "routing/placement rules" as expecting a `Router`
   API. State plainly: the runtime/simulator route by
   `Address<M>.shard()` only; user workloads choose a stable owner
   function and bake it into the addresses they construct. No `Router`
   trait or method is added.

9. **The "narrow cross-shard perturbation surface" is conditional.**
   "If needed for replay value, add one narrow seeded cross-shard
   delivery perturbation." This is the same plan-shape weasel that
   019's `and/or` had. Decide now:
   - **Option A (recommended for a semantics phase):** no cross-shard
     perturbation in 020; the determinism/replay claims are about the
     baseline ordering rules. Defer perturbation to 021.
   - **Option B:** name the surface now (e.g.,
     `CrossShardFaultMode::DelayPairChannelBySteps`), require both
     "different seeds diverge" and "same config replays" direct proofs,
     and accept the larger review bar.
   Either is honest; "if needed" is not.

10. **Cross-shard send rejection — source-side or destination-side?**
    "Cross-shard stale/closed rejection" is named but not pinned to a
    code path. Two distinct cases:
    - Source shard knows the destination is stale (because the
      destination's known generation is older than source's last
      observation): rejection at source-time, observable in source's
      event record.
    - Source shard does not know; destination shard discovers staleness
      or stopped-target on harvest: rejection at destination-time,
      observable in destination's event record (probably
      `SendRejected` on harvest).
    Both can happen. Pin which event flows where, and pick the same
    event vocabulary the live runtime already uses (`SendRejected
    { reason: Closed }`).

11. **Multi-source interleaving precision.**
    "Ascending source-shard order" is the global tie-break, but the
    plan does not say what happens *within* one source shard's
    pair-channel when it has multiple messages waiting on harvest. Pin:
    drain each shard-pair channel FIFO until empty, then move to the
    next source shard. Otherwise the rule could be read as "round-robin
    one message per channel," which is a different ordering and a
    different observable invariant.

12. **Composition with 017/018/019 perturbation surfaces is undecided.**
    017 perturbs local-send and timer-wake; 019 perturbs TCP
    completions. With multi-shard:
    - is the seed shared globally across shards or per-shard?
    - do `LocalSendFaultMode` perturbations apply per-shard
      independently?
    - do they apply at all to cross-shard sends, or only same-shard
      sends?
    Plan should say plainly. My read, by analogy with 018's "stay
    scoped" honesty: 017's local-send perturbation applies to
    same-shard sends only; cross-shard perturbation (if it lands per
    finding 9) is its own surface; 019 TCP perturbation stays
    shard-local; the seed is shared globally and each surface tags it
    with its own constant.

13. **Runtime-owned `Call` effects stay shard-local — say so.**
    Sleep, TCP bind/accept/read/write/close are runtime-owned and
    shard-local in implementation. Plan implicitly keeps it ("preserve
    existing shard-local runtime-owned call ... behavior") but does
    not say what happens if a workload's translator on shard 0 sends a
    completion message to shard 1 (which is fine: that's just a
    user-level send across shards). One clarifying line: "runtime-
    owned call resources (sleep timers, TCP listeners/streams) remain
    owned by their issuing shard. The translator's resulting `Message`
    can be addressed to any shard via `Outbound`, which then flows
    through the new cross-shard delivery path."

14. **Unknown isolate id on a remote shard.**
    Today same-shard send to an unknown isolate id is a programmer
    error and rejects. With multi-shard, the source shard cannot
    locally verify the destination isolate id exists — only the
    destination shard can. Pin the rejection path: destination shard
    rejects on harvest with the same event vocabulary
    (`SendRejected { reason: Closed }` or a new `Unknown` variant —
    pick one and match the live runtime's existing distinction
    between programmer error and ordinary delivery rejection).

15. **`Shard` trait shape is unspoken.**
    Today the `Shard` trait requires `id()`. Plan should say
    explicitly: "no `Shard` trait expansion is required for 020." If
    additional methods are required (e.g., to cooperate with the
    runtime's coordinator), call that out as a `tina` boundary change
    and trigger the pause gate — that is exactly what the plan says
    should pause-and-design.

16. **Cross-shard misuse — panic or error variant?**
    What does the runtime/simulator do when a workload tries to:
    - `Effect::Spawn` with a target shard != current shard, or
    - `RestartChildren` referencing a child known to be on a remote
      shard, or
    - register a root isolate on a non-existent shard id?
    Plan says cross-shard spawn placement is out of scope, so the
    answer is probably panic ("programmer error"). Pin it explicitly,
    matching the live runtime's existing programmer-error vs.
    delivery-rejection distinction.

17. **"Concurrent shard execution is out of scope" is implied but not
    said.**
    "What Will Not Change" lists a lot of things, but does not say
    plainly that 020's multi-shard runtime is still synchronous /
    explicit-step / single-threaded. Add one line. Concurrent shard
    execution belongs to the substrate phase that brings monoio /
    io_uring; saying so prevents readers from over-reading the slice
    as concurrency.

18. **Single user-shaped workload — bar comparison.**
    019 had four echo workloads + one fault test. 020 lands the
    largest semantic surface so far and asks for one routed
    dispatcher/worker workload (or a tenant/session alternative). Two
    options:
    - keep the bar at one workload but require it to exercise
      everything: cross-shard request, cross-shard reply, replacement-
      child continuity (within a shard, since cross-shard supervision

## Session A Response To Plan Review 1

Plan amended against the review.

Most important tightenings folded in:

- pinned one global explicit-step coordinator API:
  - one global `step()`
  - ascending shard-id order
  - inbound harvest before each destination shard snapshot
  - no per-shard public stepping API in 020
- pinned bounded shard-pair channels with source-time
  `SendRejected { reason: Full }`
- pinned source/destination trace observability for cross-shard sends:
  `SendDispatchAttempted` / `SendAccepted` or `SendRejected` on source,
  `MailboxAccepted` on destination harvest
- pinned global monotonic `EventId` / cause-id story across shards
- pinned multi-shard quiescence: all shards locally quiescent and all
  shard-pair channels empty
- pinned additivity:
  - existing single-shard entrypoints remain intact
  - Galileo adds sibling multi-shard coordinator surfaces
- pinned global ingress shape: `try_send(addr, msg)` routes by `addr.shard()`
- pinned routing honesty: no `Router` API; routing policy stays in workload
  code
- decided cross-shard perturbation is **out** of 020 and deferred
- pinned rejection flow:
  - source-time `Full` on shard-pair overflow
  - destination-harvest `Closed` for stale/stopped/unknown remote targets
- pinned drain-to-empty per shard-pair channel, not round-robin
- pinned composition with 017/019 surfaces as shard-local only
- pinned runtime-owned calls as shard-local resources
- pinned no `Shard` trait expansion unless the phase pauses for a real boundary
  change
- pinned programmer-error cases:
  - non-existent shard registration may panic
  - cross-shard spawn/restart misuse may panic
- pinned explicit proof asks for:
  - N>=3 harvest ordering
  - three-message shard-pair FIFO
  - cross-shard `SendRejected` on a workload path

The phase is still intentionally large, but the contract is now much less
"reverse-engineer it from the implementation" and much more "read it here,
then prove it."
      is out of scope), stable placement, and at least one rejection
      path; or
    - require one preserved-success workload and one fault-shaped
      workload (which is finding 9 reframed: cross-shard perturbation
      buys a second workload).
    My read: combine option 1 with finding 9 option A. One workload,
    no cross-shard perturbation in 020, and the workload exercises
    cross-shard rejection paths directly.

### How this could still be broken while the listed tests pass

- "Cross-shard send delivery" passes because every test uses two
  shards with shard ids 0 and 1, and source-shard 0 always lands first
  by the ascending-harvest rule. A bug that ignores the harvest rule
  and accidentally drains in registration order silently passes
  every two-shard test. A three-shard test would catch it. Plan
  should require N >= 3 shards in at least one ordering proof.
- "Per-source -> per-target FIFO across shards" passes because all
  cross-shard tests send fewer than two messages from the same source
  to the same target. Pin: at least one test sequences three messages
  from one source isolate to one target isolate across shards and
  asserts arrival order.
- "Deterministic repeated runs" passes because both runs use exactly
  the same shard count, shard ids, and registration order. A bug that
  makes determinism depend on a `HashMap` iteration of shards would
  pass with two-shard tests and randomly fail under load. Pin:
  determinism is over a fixed shard-id list and ascending iteration,
  not iteration of a `HashMap<ShardId, _>`.
- "Stale-address rejection across shards" passes because the test
  marks the destination stopped via the destination shard's local
  state. A workload that sends from source shard 0 to a destination
  on shard 1 that was *never* registered hits the unknown-isolate
  path — finding 14 — which is silently uncovered if not pinned.
- "Replay reproduces multi-shard run" passes in-process under the same
  workload binary, but if the cross-shard channel scheduler depends on
  any non-deterministic input (system clock, atomic ordering, etc.)
  the determinism invariant could regress without notice. Pin: no
  per-shard wall clock; virtual time is a simulator-global property.
- The single routed workload exercises everything except explicit
  rejection. If it never observes a `SendRejected` cross-shard event,
  the rejection path is exercised only by the focused unit tests, and
  a regression where routing accidentally falls back to a
  permissive-default behavior under workload pressure passes the
  workload test.
- 017 local-send perturbation on a multi-shard workload silently
  applies to cross-shard channels too, not just same-shard mailboxes,
  because composition with prior perturbations is undecided (finding
  12). Tests pass; semantics quietly broaden.

### What old behavior is still at risk

- Every 016-019 single-shard test depends on `Simulator::new(shard,
  config)` and `try_send` with a same-shard target. The blast-radius
  claim "all prior Mariner and Voyager suites still pass" depends
  entirely on additivity (finding 6). Implementation review should
  treat any forced migration of existing tests as a regression.
- 017 structural checker over event-id monotonicity (
  `structural_checker_observes_monotonic_event_ids`) depends on
  finding 4 (event-id namespacing). If event ids stop being globally
  monotonic, that checker either has to be qualified per-shard or
  retired. Plan should call this out as a knock-on change.
- 019's `pending_completion_capacity` behavior on the TCP path is
  shard-local. Cross-shard channel boundedness (finding 2) is a new
  but parallel rule. They should not get conflated.
- Today `tina-sim`'s `dispatch_local_send` panics on
  `target_shard != self.shard.id()` (lib.rs around line 1530 in 019's
  source). The transition from "panic" to "route to cross-shard
  channel" is a semantic widening. Implementation review should
  verify there is no path left where the simulator panics on a
  legitimate cross-shard send.

### What needs a human decision

- **Cross-shard perturbation in 020 or deferred to 021?** (finding 9)
  My recommendation: defer. 020 is already large and a semantics phase
  is honest without a perturbation surface. The replay claim becomes
  "same config reproduces the same event record under the same
  ordering rules," which is meaningful without seeded faults.
- **API shape for multi-shard simulator: widen `Simulator<S>` or add
  a sibling type?** (finding 6) My recommendation: add a sibling
  `MultiShardSimulator<S, ...>` (or similar) so existing
  `Simulator<S>` workloads compile completely unchanged. The
  single-shard simulator is then a special case of the multi-shard
  one, and a future refactor can collapse them when there is honest
  reason to.
- **Whether monoio / substrate engineering can leak into 020 closeout
  via a "small adapter."** Plan says no, pause gates say no. Implementation
  review should treat any monoio-shaped engineering work as
  out-of-scope by default and require an explicit pause-and-amend
  cycle if it appears.
- **N >= 3 shards in at least one ordering test.** (cross-cuts
  findings 1, 11, "How this could still be broken") My recommendation:
  yes; two-shard tests can hide ordering bugs by symmetry.

### Recommendation

The plan is on-shape for a semantics phase: it refuses substrate work,
keeps cross-shard supervision/spawn out, names ordering rules, and
demands runtime/simulator parity. The structural framing is sound. Not
yet ready to hand off to implementation — too many semantic decisions
are still unpinned for a phase that is explicitly about pinning
semantics.

Amend the plan before implementation begins to:

1. Pin the multi-shard step coordinator API (one `step()` over all
   shards in fixed order, vs per-shard `step_shard(shard_id)` plus an
   explicit harvest call).
2. Pin shard-pair channel boundedness and the source-time `Full`
   rejection through the live `SendRejected
   { reason: Full }` event shape.
3. Pin the cross-shard trace-event vocabulary for both the sending
   shard's `SendDispatchAttempted`/`SendAccepted`/`SendRejected` and
   the destination shard's `MailboxAccepted` on harvest, so causality
   is observable in the trace rather than asserted from helper state.
4. Pin event-id and cause-id namespacing across shards, and call out
   downstream impact on the 017 structural monotonicity checker.
5. Pin the multi-shard quiescence rule: every shard locally quiescent
   AND every shard-pair channel empty.
6. State explicitly that existing single-shard `Simulator<S>`
   workloads compile and run unchanged; either widen the type
   additively or introduce a sibling multi-shard simulator type.
7. Pin the cross-shard ingress shape — does `try_send` route by
   `addr.shard()` or do callers reach a per-shard handle.
8. State explicitly that "routing/placement" is user-side address
   construction, not a `Router` API in the runtime.
9. Decide cross-shard perturbation: in scope and named, or out of
   scope and deferred to 021.
10. Pin source-side vs destination-side cross-shard send rejection,
    using the same event vocabulary the live runtime already uses.
11. Pin multi-source interleaving precision: drain each shard-pair
    channel FIFO to empty before moving to the next source shard.
12. Pin composition with 017/019 perturbations: per-shard
    independent, same-shard scope, shared seed with per-surface tag.
13. State that runtime-owned `Call` effects stay shard-local; their
    translated messages flow through the new cross-shard path the
    same way any user send does.
14. Pin unknown-isolate-id-on-remote-shard rejection vocabulary
    (probably the same `Closed` / programmer-error distinction the
    live runtime already makes).
15. State explicitly that the `Shard` trait shape does not grow.
16. Pin cross-shard misuse response (spawn on remote shard, register
    on unknown shard id) as panic / programmer error, matching the
    live runtime's existing distinction.
17. Add one line to "What Will Not Change": 020's multi-shard runtime
    is still synchronous / explicit-step. Concurrent shard execution
    belongs to a later substrate phase.
18. Require N >= 3 shards in at least one ordering test; require at
    least three messages from one source isolate to one target
    isolate to honestly prove per-source/per-target FIFO across
    shards; require the routed workload to observe at least one
    `SendRejected` cross-shard event so the rejection path is
    exercised by the user-shaped proof and not only by focused unit
    tests.

None of these require a `tina` boundary change as currently scoped.
They are tightening the semantic surface against the bar the plan
already names.

## Plan Review 2

Reviewed the amended plan against the 18 Plan Review 1 findings.

### Plan Review 1 findings: status against the amended plan

All 18 closed in the plan text:

1. Multi-shard step coordinator API — **closed**.
   "one global `step()` advances all shards in ascending shard-id
   order"; "there is no per-shard public stepping API in this phase."
2. Cross-shard channel boundedness / source-time `Full` rejection —
   **closed**. "shard-pair cross-shard channels are explicitly
   bounded by config"; "overflow rejects at source-time through the
   live `SendRejected` / `SendRejectedReason::Full` vocabulary";
   "Galileo must not hide unbounded buffering between shards."
3. Cross-shard causality observability tied to trace events —
   **closed**. Source-side `SendDispatchAttempted` /
   `SendAccepted`/`SendRejected` and destination-side
   `MailboxAccepted` on harvest are pinned; "cross-shard request/reply
   causality is proved against that source/destination event shape
   rather than only against final workload output."
4. Event-id and cause-id namespacing across shards — **closed**.
   "`EventId` stays globally monotonic across all shards in one run";
   "cause ids remain globally monotonic and link events across shards
   without a per-shard namespace"; "existing structural
   monotonicity/replay checkers should remain meaningful."
5. Multi-shard quiescence — **closed**. Build step 11: "all shards
   locally quiescent / all shard-pair channels empty."
6. API additivity — **closed**. "existing single-shard harnesses
   compile unchanged; Galileo adds sibling multi-shard coordinator
   surfaces rather than breaking current `Simulator<S>` workloads"
   plus build step 1.
7. Cross-shard ingress shape — **closed**. Global
   `try_send(addr, msg)` routing by `addr.shard()`; no per-shard
   handle.
8. Routing is user-side, not a `Router` API — **closed**. "Galileo
   does not add a `Router` API; routing policy stays in workload
   code."
9. Cross-shard perturbation conditional — **closed (deferred)**.
   "Defer cross-shard perturbation to a later slice. 020's
   replay/determinism claims are about the baseline multi-shard
   ordering model itself."
10. Source-side vs destination-side rejection — **closed**.
    `Full` at source-time, `Closed` at destination-harvest-time, both
    using the live `SendRejected` vocabulary.
11. Multi-source interleaving precision — **closed**.
    "drained FIFO-to-empty before harvest moves to the next source
    shard; Galileo does not use round-robin one-message interleaving
    between channels."
12. Composition with 017/019 perturbations — **closed**. "017's
    local-send perturbation remains same-shard only"; "017 timer
    perturbation remains shard-local"; "019 TCP perturbation remains
    shard-local"; shared global seed with per-surface tagging.
13. Runtime-owned `Call` effects stay shard-local — **closed**.
    "sleep timers and TCP listeners/streams remain owned by the
    issuing shard"; cross-shard cascade flows through ordinary
    `SendMessage`.
14. Unknown-isolate rejection vocabulary — **closed**. Build step 8
    pins it; build step 14 lists "cross-shard unknown-isolate
    rejection" as a separate direct proof from stale/stopped.
15. `Shard` trait stability — **closed**. "This phase does **not**
    require `Shard` trait expansion."
16. Cross-shard misuse panic — **closed**. Programmer-error block:
    "attempting cross-shard spawn placement or cross-shard
    `RestartChildren` semantics in 020 is a programmer error and may
    panic"; "registering a root isolate on a non-existent shard id is
    a programmer error and may panic."
17. "Still explicit-step / synchronous" honesty line — **closed**.
    "This phase stays explicit-step, synchronous, and single-threaded
    at the coordinator level. Concurrent shard execution belongs to
    the later substrate phase, not to Galileo."
18. N≥3 ordering test, three-message FIFO, workload exercises a
    `SendRejected` path — **closed**. Build step 14 has N>=3 and
    three-message FIFO as direct proofs; build step 15 requires
    "the workload must also exercise a visible cross-shard
    `SendRejected` path, not only the green path."

### Small remaining gaps

These are tighter than Plan Review 1's load-bearing items. Most are
one-sentence amendments. Not blocking on their own, but worth pinning
before implementation:

1. **In-step cross-shard visibility rule.**
   The plan says "cross-shard delivery becomes visible only on a later
   destination-step after the source handler turn that produced it,"
   and that global `step()` advances all shards in ascending shard-id
   order with harvest before each shard's handler snapshot. This
   leaves one ambiguity: if shard 0's handler turn produces a send to
   shard 2 *during* the same global step, does shard 2's harvest
   *later in the same global step* pick it up, or only the next global
   step's harvest? "Later destination-step" reads as the latter, but
   the harvest-in-same-step-after-source-shard ordering reads as the
   former. Pin one. The cleanest invariant: harvest at the start of a
   global step picks up messages enqueued *before* this global step;
   sends produced during this global step become visible on the next
   step's harvest, regardless of source/destination shard-id order.
2. **Per-shard or global ordinal for seeded perturbation surfaces.**
   Composition is pinned to "shared global seed with per-surface
   tagging" (good), but the *ordinal* per surface is not. With one
   global seed and per-shard fault surfaces, do `LocalSendFaultMode`
   selectors on shard 0 and shard 1 use a shared monotonic ordinal or
   a per-shard ordinal? Either is fine; pick one. (Same-shard scope
   tilts toward per-shard ordinal so that adding a new shard does
   not perturb other shards' fault firing patterns.)
3. **Adversarial proof mode is silently dropped.**
   Plan Review 1 listed adversarial proof; the amended proof-modes
   list omits it (consistent with the perturbation deferral, finding
   9). State plainly: "020 has no adversarial proof mode; it is
   deferred along with cross-shard perturbation."
4. **"Existing single-shard 016-019 workloads compile and run
   unchanged" — verified how?**
   Build step / direct-proof list calls this out (line 245). Pin
   that this is verified by running the existing `tina-sim` test
   surface untouched, not by re-asserting per test. The closeout
   bar should be `cargo +nightly test -p tina-sim` green with all
   prior tests untouched, plus `make verify` exit 0.
5. **Replay artifact shape across shards.**
   `EventId` stays globally monotonic and `RuntimeEvent` already
   carries its shard, so the existing
   `event_record: Vec<RuntimeEvent>` artifact shape is adequate
   without modification. State this so a future reader does not
   imagine a per-shard event record split.
6. **Where does the multi-shard runtime live?**
   Plan extends "the runtime model" but does not say whether the
   multi-shard surface lands in `tina-runtime` or in a new
   crate. Plumbing, not semantics, but worth one line. Default reading:
   stays in `tina-runtime` unless Galileo intentionally chooses a new
   crate boundary.
7. **How does a workload on shard 0 obtain an `Address<M>` for a
   remote isolate on shard 1?**
   Implicitly via the existing public
   `Address::new_with_generation(shard, isolate, generation)` plus
   any registry/handshake the workload chooses. No privileged
   "lookup" API is added. Worth one line so a reader does not
   imagine a `Locator` / `Registry` is implied.
8. **Which `SendRejected` reason does the user workload exercise?**
   Build step 15 mandates a visible cross-shard `SendRejected` path
   but does not pin `Full` (queue overflow) vs `Closed` (stale/
   stopped/unknown). Either is fine for the workload as long as the
   focused proofs cover both. Acceptable. Worth a one-liner saying
   the workload picks one and the focused tests cover the other.

### Recommendation

The plan is now load-bearing and reviewable for a semantics phase.
All Plan Review 1 findings are closed in the plan text; the remaining
gaps are tightening, not structural. My read:

- The seven small refinements above are worth pinning before
  implementation, especially #1 (in-step cross-shard visibility) and
  #2 (per-shard ordinal) since both have observable consequences on
  the trace and on test stability.
- After those amendments, the plan is ready to hand off to
  implementation. The slice is large but bounded; the closeout bar is
  sharp; substrate work is fenced off.
- If amending now would cost more than it buys, items 3-8 can also
  be addressed in a quick pre-implementation amendment or absorbed
  into the implementation review against a documented assumption,
  since none of them shift semantics.

No `tina` boundary change is implied by these remaining gaps.

## Session A Response To Plan Review 2

Folded in the remaining tightenings.

Added/pinned:

- next-step-only cross-shard visibility within one global `step()`
- per-shard/per-surface ordinals under one shared global seed
- explicit statement that 020 has no adversarial proof mode
- existing 016-019 single-shard simulator surface stays untouched and is
  re-verified as a whole, not rephrased per test
- replay artifact remains the existing one-record `Vec<RuntimeEvent>` shape
- multi-shard semantics land in the current explicit-step runtime/simulator
  line, not a new substrate crate
- workloads obtain remote `Address<M>` through the existing public address
  surface; no Locator/Registry API is implied
- the user-shaped workload must name whether its `SendRejected` path proves
  `Full` or `Closed`

With those amendments, the plan is now intentionally large but cleanly pinned.
At this point the remaining work is implementation, not semantics cleanup.

## Plan Review 3

Quick final pass against the 8 small Plan Review 2 refinements.

### Plan Review 2 small gaps: status

All 8 closed in the plan text:

1. **In-step cross-shard visibility** — closed.
   "harvest for global step `N` sees only cross-shard messages
   enqueued before step `N` began; a send produced during step `N`
   becomes first visible to a remote shard in step `N + 1`." Direct
   proof item: "in-step cross-shard visibility follows the next-step-
   only rule."
2. **Per-shard or global ordinal for seeded surfaces** — closed.
   "when a seeded surface needs an ordinal under the global
   coordinator, that ordinal is per-shard/per-surface rather than one
   cross-shard global ordinal." This is the per-shard tilt I
   recommended; adding a new shard does not perturb other shards'
   fault firing.
3. **Adversarial proof mode silently dropped** — closed.
   Proof modes now lists "no adversarial proof: cross-shard
   perturbation/checker work is explicitly deferred to 021."
4. **"Existing 016-019 workloads compile unchanged" verification
   bar** — closed. Build step 13 sub-bullet pins it; direct proof
   list lists "single-shard 016-019 simulator workloads compiling
   and running unchanged."
5. **Replay artifact shape across shards** — closed. Build step 12
   sub-bullet: "replay artifacts continue using one whole-run
   `Vec<RuntimeEvent>` rather than splitting into per-shard trace
   artifacts." Reinforced in "Done Means."
6. **Where the multi-shard runtime lives** — closed. "Done Means"
   pins it to the existing runtime/simulator line, not a new
   substrate crate.
7. **Remote `Address<M>` obtaining** — closed. "workloads obtain
   remote `Address<M>` values through the existing public `Address`
   construction/generation surface; Galileo does not imply a Locator
   or Registry API."
8. **Which `SendRejected` reason the workload proves** — closed.
   Build step 15 sub-bullet: "the workload must name whether that
   rejected path proves `Full` or `Closed`."

### Recommendation

Plan is ready to hand off to implementation. All 18 Plan Review 1
findings and all 8 Plan Review 2 small refinements are closed in the
plan text. The semantics are pinned, the proof list is sharp, the
substrate work is fenced off, and additivity over the 016-019 single-
shard surface is guaranteed.

No further plan-review rounds expected. Implementation review is the
next bar.

## Upstream Alignment Note

After the first implementation slice, I re-checked the upstream Tina/Odin
material and code to sanity-check the Rust multi-shard story against the
original model.

The strongest confirmations:

- cross-shard channels are strictly bounded
- overflow is visible immediately at source-time
- next-step-only cross-shard visibility is right
- drain order by source shard is right

The important nuance:

- once a message has already been admitted into the cross-shard transport,
  upstream Tina treats later destination-side mailbox overflow / stale-target
  outcomes as destination-local drop facts, not as retroactive synchronous
  send-result changes for the source

That means `tina-rs` should be explicit about the distinction between:

- **source-time transport admission** (`SendAccepted` / `SendRejected` on the
  source shard), and
- **destination-time harvest outcome** (`MailboxAccepted`, or an explicitly
  traced destination-local drop if we choose to keep that richer trace shape)

Keeping the richer destination-side trace in `tina-rs` is defensible because
replay/explainability is one of the Rust port's strengths, but it should be
described as an observability extension rather than presented as "what the
source send really returned."

## Implementation Closeout Note

Galileo's implemented surface now covers the promised first honest multi-shard
story:

- additive `MultiShardRuntime` and `MultiShardSimulator`
- one global `step()` and one global `try_send(addr, msg)`
- root placement by shard
- shared global event-id / call-id identity
- bounded shard-pair transport with immediate source-time `Full`
- next-step-only remote visibility
- deterministic destination harvest in ascending source-shard order
- direct proofs for destination-time `Closed`, unknown-target, and
  destination-mailbox-`Full` outcomes
- one user-shaped two-shard dispatcher/worker workload on the preferred 021
  surface
- direct cross-shard request/reply causality proof
- deterministic repeated-run proof in the live runtime
- first-class multi-shard replay artifact in the simulator

What is still intentionally *not* part of this Galileo closeout:

- full upstream-style peer quarantine / shard-restarted broadcast semantics
- cross-shard spawn placement or cross-shard supervision
- adversarial cross-shard perturbation/checker work
- substrate concurrency / monoio / io_uring work

That is a good stopping boundary. The current implementation is already an
honest, reviewable multi-shard semantic runtime/simulator pair without
pretending the later shard-liveness story has landed too.

## Implementation Review 1 — Session B

Reviewed the landed implementation against the amended plan, Plan Review
1/2/3 closeouts, and the upstream-alignment / closeout notes by Session
A. Inspected `tina-runtime-current/src/lib.rs` (`MultiShardRuntime` shell
at line 1410), `tina-sim/src/lib.rs` (`MultiShardSimulator` shell at line
2708), the new `tests/multishard_dispatcher.rs` files in both crates,
the inline cross-shard tests in `tina-runtime-current/src/tests.rs`, and
the SYSTEM/CHANGELOG updates. Reran the multi-shard test surface and
`make verify` end-to-end.

Note: 020 was implemented on top of 021's rename pass. The crate
`tina-runtime-current` is now `tina-runtime`, and the new vocabulary
(`Runtime`, `RuntimeCall`, `CallInput`, `CallOutput`, `CallError`,
`ChildDefinition`, `RestartableChildDefinition`, `with_initial_message`,
`register_with_capacity`, `tina::prelude`) is in place. The 020 review
artifacts and code references in this section reflect the post-021
naming.

### What I verified directly

- `cargo +nightly test -p tina-sim -p tina-runtime` passes the full
  multi-shard test surface: 8 inline runtime tests + 3
  `multishard_dispatcher` workload tests on the runtime side; 8 inline
  simulator tests + 3 `multishard_dispatcher` workload tests on the
  sim side. 22 multi-shard tests, 22 green.
- `make verify` exits 0. clippy `-D warnings`, doc, loom, full
  workspace test all clean.
- Existing 016-019 simulator and runtime tests still green without
  changes — additivity holds across the rename + multi-shard work.
- `tina/src/lib.rs` boundary unchanged for 020; the new public surface
  lives in `tina_runtime::MultiShardRuntime` and
  `tina_sim::MultiShardSimulator` plus their `Config` siblings, none
  of which require new `tina` vocabulary.
- 020 lands in the existing runtime/simulator crates rather than a new
  substrate crate (matches Plan Review 3 finding 6).

### Plan direct-proof items: status against the implementation

All plan build-step-14 direct proofs are exercised by named tests:

| Plan item | Test |
| --- | --- |
| root registration / placement on multiple shards | `multishard_runtime_routes_ingress_and_steps_in_ascending_shard_order`, sim variant |
| same-shard behavior unchanged | blast-radius via `make verify` |
| `try_send(addr, msg)` ingress routes by `addr.shard()` | same as row 1 |
| cross-shard send delivery | `cross_shard_send_becomes_visible_on_the_next_global_step` |
| in-step cross-shard visibility follows next-step-only rule | same |
| bounded shard-pair queue rejection at source-time | `cross_shard_queue_overflow_rejects_at_source_time` |
| per-source -> per-target FIFO | `cross_shard_harvest_preserves_fifo_from_one_source` (3 messages from one source isolate via `KickThrice`) |
| three-message FIFO across one shard-pair | same |
| deterministic multi-source interleaving | `cross_shard_harvest_drains_sources_in_ascending_shard_order` (4 shards: 33, 22, 11, 44 — sources 11, 22, 33 to sink on 44) |
| N >= 3 shard harvest ordering | same — 3 source shards + 1 sink shard |
| cross-shard stale/closed rejection | `cross_shard_closed_target_rejects_on_destination_harvest` |
| cross-shard unknown-isolate rejection | `cross_shard_unknown_isolate_rejects_on_destination_harvest` |
| destination mailbox full on harvest | `cross_shard_destination_mailbox_full_rejects_on_harvest` |
| request/reply causality across shards | `dispatcher_worker_workload_preserves_cross_shard_request_reply_causality` (asserts strict event-id ordering: source attempt < source accept < destination harvest < reply attempt < reply harvest) |
| deterministic repeated runs | `dispatcher_worker_workload_is_deterministic_across_identical_runs` |
| simulator replay from saved config | `multishard_dispatcher_workload_replays_from_saved_config` (rebuilds simulator from artifact's `simulator_config()` + `multishard_config()`) |
| user-shaped two-shard workload exercises a `SendRejected` path | `dispatcher_worker_workload_surfaces_source_time_full_rejection` (`Full` named per build step 15) |

### Specific implementation invariants I verified

- **Global event-id monotonicity across shards.** `IdSource` is
  `Rc<Cell<u64>>` cloned into every per-shard simulator
  (`tina-sim/src/lib.rs:783, 2783`). Each shard increments the same
  counter, so EventIds are globally monotonic. Same pattern in
  `tina-runtime` (`IdSource::clone()` passed via
  `Runtime::with_clock_and_ids`). 017's structural
  event-id-monotonicity checker remains meaningful by construction.
- **Next-step-only cross-shard visibility.** `step()` does
  `let mut ready = std::mem::take(&mut self.remote_queues);` at the
  start, so harvest reads only sends enqueued *before* this step;
  sends produced during this step go to a freshly cleared
  `self.remote_queues` and become harvestable next step. The test
  `cross_shard_send_becomes_visible_on_the_next_global_step`
  directly asserts that the destination shard's `HandlerStarted`
  count is 0 after step 1 (sender ran but reached only the queue)
  and 1 after step 2 (harvest produced `MailboxAccepted`).
- **Ascending source-shard harvest, drain-to-empty per channel.**
  `harvest_for_destination` iterates `shard_ids` in ascending order
  (the `shard_ids` vec is built from `BTreeMap` keys, sorted by
  `Shard::id` at construction in `with_config`). For each source it
  drains its queue with `while let Some(queued) =
  queue.pop_front()`. Drain-to-empty per source matches the plan's
  "drain each shard-pair channel FIFO-to-empty before harvest moves
  to the next source shard."
- **Source-time vs destination-time semantic stages.** Source-side
  `SendDispatchAttempted` / `SendAccepted` / `SendRejected` are
  emitted on the source shard during the source handler's effect
  execution. Destination-side `MailboxAccepted` and destination-
  local rejection events are emitted on the destination shard
  during harvest. Each test asserts both shards' events explicitly,
  honoring the SYSTEM.md committed constraint that the two stages
  are different and should stay described that way.
- **Bounded shard-pair channels with source-time `Full`.**
  `MultiShardRuntimeConfig.shard_pair_capacity` (default 64) is
  enforced by `if queue.len() >= config.shard_pair_capacity {
  return Err(SendRejectedReason::Full); }` in the closure passed to
  `step_with_remote`. The runtime's `KickTwice` test sets capacity
  to 1 and asserts a single source-shard `SendRejected{Full}`. The
  dispatcher full-rejection workload uses `shard_pair_capacity: 1`
  to make `SubmitPair` overflow on the second request.
- **Multi-shard quiescence.**
  `MultiShardSimulator::run_until_quiescent` (sim-side at line
  2943) checks all four conditions before declaring quiescence:
  - delivered count == 0
  - no due timers across any shard
  - no in-flight calls across any shard
  - `!self.remote_queues.is_empty()` is false (no pending cross-
    shard messages)
  - no shard has pending mailbox messages
  Mirrors the 019 lesson where ignoring an in-flight category
  produced a stop-too-early bug. Honored here.
- **Replay artifact stays one whole-run record.**
  `MultiShardReplayArtifact.event_record: Vec<RuntimeEvent>` (sim
  line 128) plus the doc comment "this records one whole-run event
  record rather than splitting replay state into per-shard
  artifacts" matches Plan Review 3 finding 5.
- **Programmer-error misuse panics.**
  `MultiShardRuntime::with_config` panics on empty shard set,
  duplicate shard ids, or zero pair capacity (lines 1460-1475).
  `register_on` and `try_send` panic via `checked_shard_index` if
  the target shard isn't owned. Same in the simulator. Matches the
  plan's programmer-error rule.

### Plan Review 1/2/3 small gaps: status against the implementation

All 26 plan-review findings (18 + 8) closed in code:

- Coordinator API: one global `step()` ascending shard-id, no
  per-shard public stepping API. ✓
- Shard-pair channel boundedness with source-time `Full`. ✓
- Trace observability with source-side and destination-side events. ✓
- Globally-monotonic `EventId` via shared `IdSource`. ✓
- Multi-shard quiescence: all-shards-local + all-pair-channels-empty.
  ✓
- Additivity: existing 016-019 tests untouched. ✓
- Global ingress routing by `addr.shard()`. ✓
- No `Router` API: routing is `Address::shard()` only. ✓
- No cross-shard perturbation surface (deferred to 021/later). ✓
- Source-side `Full` vs destination-side `Closed` distinction. ✓
- Drain-to-empty per channel before next source. ✓
- 017/019 perturbation surfaces stay shard-local (they only fire on
  `Simulator`-local code paths, which the multi-shard simulator
  composes per shard with shared seed and per-surface tagging). ✓
- Runtime-owned calls stay shard-local. ✓
- Unknown-isolate rejection on destination harvest. ✓
- `Shard` trait unchanged. ✓
- Cross-shard misuse panics. ✓
- Still explicit-step / synchronous: verified by inspection — no
  threading or async substrate. ✓
- N >= 3 ordering test, three-message FIFO, workload exercises
  `SendRejected`. ✓
- In-step cross-shard visibility next-step-only. ✓
- Per-shard/per-surface ordinals (017/019 surfaces are shard-local
  by virtue of running inside per-shard `Simulator` instances; the
  shared seed has per-surface constants from 017 and 019). ✓
- No adversarial proof mode. ✓
- Existing 016-019 surface stays untouched (verified by `make
  verify`). ✓
- Replay artifact one whole-run `Vec<RuntimeEvent>`. ✓
- Multi-shard semantics in existing runtime/simulator crates. ✓
- Workloads obtain remote `Address<M>` via existing public
  `Address::new_with_generation` (the unknown-isolate test
  constructs an address with `IsolateId::new(999)` directly). ✓
- Workload's `SendRejected` reason named (`Full` in the dispatcher
  full-rejection test). ✓

### Specific implementation observations beyond the plan

- The Session-A "Upstream Alignment Note" (review.md lines 707-735)
  is honest: upstream Tina treats post-admission destination drops as
  destination-local facts. The implementation reflects this — source-
  side `SendAccepted` is emitted *only* if the shard-pair queue
  accepts the message (`Ok(()) => SendAccepted`), and any later
  destination-side rejection (mailbox-full on harvest, stale-target,
  unknown-isolate) is emitted as a separate event on the destination
  shard, not as a retroactive change to the source-side outcome.
  This is the right read of the upstream model.
- The 4-shard ordering test (`cross_shard_harvest_drains_sources_in_
  ascending_shard_order`) is constructed deliberately to defeat
  symmetric-shard-id bias: shards are passed in `[33, 22, 11, 44]`
  order, then sorted internally to `[11, 22, 33, 44]` for stepping;
  senders are `try_send`'d in `[33, 22, 11]` order; assertion is
  that destination causes match the *send-order event-id sequence*
  (which is itself ascending because senders run in step-order
  ascending). A bug that drained in registration order or input
  order would fail this test.
- The dispatcher request/reply causality test asserts strict
  event-id ordering across five distinct trace events on two shards
  (request-attempt, request-accept, worker-mailbox, reply-attempt,
  coordinator-mailbox). This is the strongest single-workload proof
  of cross-shard causality preservation in the suite.

### How this could still be broken while the listed tests pass

- **Per-source-isolate / per-target-isolate FIFO is technically a
  consequence of per-shard-pair FIFO, not an independent
  invariant.** All messages from any isolate on shard A to any
  isolate on shard B share the same `(A, B)` queue. Two source
  isolates on the same shard sending to two different target
  isolates on the same shard all go through one queue and harvest
  in interleave-by-send-order. The plan's "messages sent from one
  source isolate to one target isolate preserve send order" holds
  by construction — the per-isolate-pair claim is subsumed by the
  per-shard-pair claim, but no test actually exercises the harder
  case (two sources on shard A, two targets on shard B, prove that
  per-isolate-pair order holds even when interleaved). Today every
  test uses one source and one target per shard pair. Surrogate-
  supported, not directly proved.
- **The 4-shard test still uses one sink isolate.** A regression
  that broke harvest ordering for the case of multiple destination
  isolates on the same shard (different mailboxes, same harvest
  step) would not be caught by the existing 4-shard test. Surrogate
  via the dispatcher workload (which has 1 coordinator + 1 worker)
  but the multi-isolate-per-shard ordering case is not directly
  pinned.
- **`run_until_quiescent` correctness depends on `mem::take` clearing
  `remote_queues` at start of step.** A future refactor that
  changed this to "mutate in place" would make the
  `!self.remote_queues.is_empty()` quiescence check always true (or
  always false) depending on the change. No invariant test pins
  this. Mitigation is plumbing-discipline rather than test surface.
- **The dispatcher's `Full` workload uses `shard_pair_capacity: 1`
  which makes the second admission fail.** With pair capacity 2 or
  greater, the `SubmitPair` workload would not exercise `Full`. The
  workload demonstrates the rejection path under a pinned capacity,
  which is fine, but a test with the default capacity would not
  exercise `Full` on this workload. Acceptable — the test names
  the capacity and the `SendRejected` shape, matching build step
  15's "name whether it proves `Full` or `Closed`" requirement.
- **Cross-shard composition with 017 local-send / timer-wake faults
  and 019 TCP perturbation is implicit, not directly tested.**
  Each per-shard `Simulator` carries the same `SimulatorConfig`
  cloned, so faults fire shard-locally. No test exercises a
  multi-shard run with non-default 017/019 perturbations. Plan
  Review 1 finding 12 was closed as "stay scoped to existing
  surfaces"; that holds by construction but is not directly
  proved. Acceptable for closeout, deferred to a later
  cross-shard-perturbation slice (021's note about deferring
  cross-shard perturbation).
- **The simulator replay test uses `SimulatorConfig::default()`**.
  Same-config replay with default seeds is the easy case. Replay
  under a non-default seed across shards is not directly tested.
  The simulator's seeded paths are all shard-local, so this is
  surrogate-supported by the existing 017 same-seed-replay tests
  + the 020 multi-shard replay test together.

### What old behavior is still at risk

- 016-019 tests pass under `make verify`, but the test surface for
  each prior phase remains identical. No prior tests were rewritten.
  Implementation review treats this as the right blast-radius
  posture.
- 017 structural event-id monotonicity checker continues to be
  meaningful because EventIds are globally monotonic.
- 018 spawn/restart and 019 TCP semantics are untouched: the
  multi-shard runtime/simulator delegate per-shard logic to the
  existing single-shard `Runtime` / `Simulator`. Spawn lineage and
  TCP resources stay shard-local by routing through the existing
  per-shard implementations.

### What needs a human decision

- **Whether to add a focused per-shard-pair test with multiple
  sources/targets on each side.** Not required for closeout; the
  per-shard-pair FIFO is honored by construction and the existing
  3-message FIFO test plus 4-shard ordering test cover the load-
  bearing cases. Worth a closeout note that per-isolate-pair
  ordering is surrogate-supported through per-shard-pair FIFO.
- **Whether to add a multi-shard run under non-default
  017/019 fault config.** Not required; cross-shard perturbation
  is explicitly deferred. Worth a closeout note acknowledging
  surrogate proof through composition rather than direct test.

### Recommendation

020 closes on its own terms.

- All 18 Plan Review 1 findings, all 8 Plan Review 2 small
  refinements, and all 16 build-step-14 direct proofs are
  exercised by green tests.
- The two user-shaped workloads (one runtime, one simulator)
  exercise the hardest invariant the plan named — request/reply
  causality across shards — directly through cross-shard event-id
  ordering assertions, not through helper-state probes.
- The implementation flushed out the right discipline (next-step-
  only visibility via `mem::take`, ascending source-shard harvest
  via `BTreeMap` shard indexing, drain-to-empty per channel,
  shared `IdSource` for global event-id monotonicity).
- SYSTEM.md gained the right two committed constraints
  (source-time vs destination-time stages; full peer-quarantine /
  shard-restarted is later work).
- CHANGELOG documents the slice honestly.
- Boundary promise held: no `tina` API additions; multi-shard
  surface lives in `tina_runtime` and `tina_sim` as additive
  shells.
- `make verify` exit 0, including all 016-019 suites.

Suggested non-blocking closeout notes for `commits.txt` /
closeout review:

1. Per-isolate-pair FIFO across shards is surrogate-supported by
   per-shard-pair FIFO; no test directly exercises two sources
   plus two targets sharing one shard pair.
2. Multi-shard composition with 017/019 fault surfaces is
   surrogate-supported by per-shard scoping; no test runs a
   multi-shard workload under non-default seed/fault config.
3. Multi-shard simulator replay is proved under
   `SimulatorConfig::default()`; non-default-seed multi-shard
   replay is surrogate-supported through 017's per-shard replay
   plus the multi-shard ordering tests.

After those notes, 020 closes single-shard semantics' first honest
multi-shard chapter: deterministic ordering, source vs destination
stages, replayability, additivity, and explicit future-work
boundary against shard-quarantine and substrate concurrency.

## Closeout Update

The three recommended closeout proof notes were promoted from surrogate
evidence to direct evidence before merge.

- Per-isolate-pair FIFO across shards now has a direct runtime and simulator
  proof with two source isolates and two target isolates sharing one shard
  pair.
- Multi-shard simulator replay now has a direct non-default seeded fault proof.
- Multi-shard composition with existing seeded fault surfaces now has direct
  proofs for timer/local-send faults, scripted TCP completion faults, and
  supervised restart under seeded local-send delay.

020 is closed. Remaining work belongs to Kepler: peer/shard liveness,
shard-restart or quarantine rules, cross-shard supervision boundaries, and
runtime-level buffering/allocation claims.
