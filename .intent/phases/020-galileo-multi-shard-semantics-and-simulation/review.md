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
    can be addressed to any shard via `SendMessage`, which then flows
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
   multi-shard surface lands in `tina-runtime-current` or in a new
   crate. Plumbing, not semantics, but worth one line. Default reading:
   stays in `tina-runtime-current` since "current" names the
   substrate, not the shard count.
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
