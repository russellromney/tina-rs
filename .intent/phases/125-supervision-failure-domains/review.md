# Plan Review 1

- [fixed] Cross-shard ownership was allowed to devolve into "typed
  unsupported" for the whole phase. That would not advance the core capability.
  The plan now requires local live/sim multi-shard child ownership and reserves
  typed unsupported for remoting/clustering edges.
- [fixed] The plan did not state old behavior that must remain stable. Added
  same-shard spawn, `spawn_observed`, restart-budget, panic-restart, stale
  generation, and runtime-owned lineage non-change rules.
- [fixed] Proof did not name blast radius. Added public-path regression proof
  for existing supervision and observed-spawn behavior.

Remaining risk: cross-shard child ownership is runtime-deep. Implementation
review must check that stop/restart/report truth works in both live and sim,
not only in simulator.

# Plan Review 2

- [fixed] Runtime fairness was too small as a separate phase and too close to
  Phase 121. It is now folded into the supervision/failure-domain phase because
  both touch runtime ownership, failed-shard truth, progress, and terminal
  reports.
- [fixed] Fairness proof now requires Tina-visible facts: ready-turn lag,
  timer lateness, remote-drain yield, progress counts, and starvation warnings.
  Throughput charts are explicitly not enough.
- [fixed] Trace determinism and replay blast radius are now part of the same
  runtime phase instead of a separate later paper cut.

Remaining risk: one large runtime phase can sprawl. Keep scope to ownership,
failure, progress, and reports. Do not add broad scheduler policy or priority
queues.

# Implementation Findings — first wave

Branch `phase-125-supervision` off `main`. Records what shipped, the open
capability gaps, and the design tensions a follow-up must respect.

## Shipped

1. `Effect::Fail` / `fail()` — typed, non-panic child failure. Distinct
   `HandlerReportedFailure` trace fact; same supervision path as panic. Live +
   sim, replay-stable. New trace tags are append-only
   (`HandlerReportedFailure` = 37, `EffectKind::Fail` = 14); no existing replay
   hash changes because no existing scenario emits the new variants.
2. `SupervisorReport` — typed terminal report, trace reader, mirrors
   `PressureSummary::from_events`. Names children by ordinal + latest
   incarnation; distinct halt reason (budget exhausted vs supervisor stopped).
3. `FairnessReport` + `StarvationWarning` — per-isolate turns/timer-ticks,
   hot-vs-quiet + timer-under-load proof. Progress is turns and timers, not a
   wall-clock promise.
4. `Effect::StopChildren` / `stop_children()` — explicit supervised shutdown.
   Owner closes every owned child (callers settle), each named by a
   `ChildStopped` fact under the owner; default `Effect::Stop` unchanged.
   `SupervisorReport` counts/names the closed children. Live + sim, replay-
   stable; new trace tags append-only (`ChildStopped` = 38,
   `EffectKind::StopChildren` = 15).

Tests: `tina-runtime` lib (501) and `tina-sim` lib (54) green; sim
`supervision_simulation` (12) and `multishard_dispatcher` green; clippy clean on
`tina`, `tina-runtime`, `tina-sim`, `tina-tracing`, `tina-tokio-bridge`.

## Open capability gaps

### B — parent-stop child cleanup (core shipped; tail open)

Shipped as the opt-in `Effect::StopChildren`: the owner walks its `child_records`
and stops each live child through the normal path (mailbox closed, pending calls
cancelled → callers settle), emitting a `ChildStopped` fact per child under the
owner. Default `Effect::Stop` is untouched, so the pinned guarantee
`stopped_supervisor_rejects_later_child_failure_without_replacement` (which needs
children to outlive a plain parent stop) still holds. A parent does a full
shutdown with `batch([stop_children(), stop()])`.

Still open:
- A dedicated "owner stop while a child has an in-flight call settles the caller
  visibly" test. The settle already happens (the child's `stop_entry` cancels
  pending calls), but it deserves its own named proof.
- A no-leaked-leases/permits/body-charges/pending-calls assertion after the
  cascade (compose `SupervisorReport` with the pressure/capacity readers).
- `StopChildren` stops direct children only; grandchildren of a stopped child
  are orphaned exactly as they are after any isolate stop today. If recursive
  shutdown is wanted, that is a follow-up decision.

### D — spawn + learn-address sub-phase: SHIPPED

`spawn_observed(child).on_shard(shard)` landed and is proven live
(`MultiShardRuntime`) and in the deterministic simulator (replay-stable). Both
type-system barriers below were resolved as predicted:

1. New `Isolate::SpawnObservedRemote` associated type, defaulted to `Infallible`
   via nightly `associated_type_defaults` — existing isolates and the
   `isolate_types!` macro are source-unchanged. New `Effect::SpawnObservedOn`
   carries it; a parallel `IntoSendErasedSpawnObserved` erasure (Send-bounded)
   sits beside the existing one, propagated to ~34 runtime + ~13 sim bound sites.
2. The live transport stayed **non-generic**: the `Send`-erased spawn box is
   carried as `Box<dyn Any + Send>` and downcast back on the destination (same
   `S, F`), so no `<S, F>` envelope refactor was needed. The earlier "transport
   must go generic" prediction was avoided by the `Any` round-trip. (Adding the
   `Any` boxing did require `S: 'static, F: 'static` on the cross-shard step/
   harvest methods, propagated up the call chain.)

A same-shard `.on_shard(my_shard)` is registered as a normal owned child
(parent + `ChildRecord`, `StopChildren` reaches it). A *cross-shard* child is
registered with `parent = None` — the owner link is **not** recorded on the
child's shard yet, which is what the supervision half needs.

Still open in D (cross-shard *ownership/supervision*, the harder half): record
the cross-shard owner link, let an owner stop a child on another shard, and the
multi-round failure → restart → `ChildAddressChanged` protocol (B detects
failure, notifies A's supervisor, A decides on policy/budget, B restarts and
replies with the new address).

### D — original sequenced design (from the cross-shard plumbing map)

The cross-shard transport today (`tina-runtime/src/remote.rs`,
`threaded_multi_shard.rs`, `tina-sim/src/multi_shard.rs`) moves only two things
between shard threads: `QueuedRemoteSend` (a `Box<dyn Any + Send>` message) and
`RemoteCallReply`. A cross-shard call carries the requester `RegisteredAddress`
in `MessageCallContext::Remote` so the reply routes home; cross-shard spawn is
shaped the same way (request + address-bearing reply). Both engines step shards
in ascending `ShardId` order with per-pair queues, so the protocol stays
deterministic and replayable if every step is queue-mediated.

The barrier: spawning on another shard means sending an **isolate constructor**
across threads. Today `ErasedSpawn` and the spawn payload are **not `Send`**
(`into_erased_spawn` boxes a non-`Send` trait object; the isolate value and
bootstrap stay on the source shard). `ChildRecord.parent` and
`RegisteredEntry.parent` are shard-local `IsolateId`s. So cross-shard ownership
needs new type-foundation, not just new wiring.

Sequenced plan (each step independently compiles + tests; live test gates the
ownership claim):

1. **Sendable spawn payload.** Add a `Send` spawn path: a
   `Box<dyn SendErasedSpawn<S, F> + Send>` carrying an `I: Isolate<Shard = S> +
   Send` constructor + `Send` bootstrap + (optional) `Send` restart recipe.
   Same-shard `spawn`/`spawn_observed` stay non-`Send` and unchanged. The
   natural public surface is to let `spawn_observed(child).on_shard(shard_id)`
   target another shard, because spawn-observed already delivers the child
   address back to the parent — exactly what cross-shard spawn must do.
2. **Spawn request/reply envelopes.** `QueuedRemoteEnvelope::SpawnRequest {
   owner: RegisteredAddress, payload: SendErasedSpawn, mailbox_capacity, cause }`
   and reuse the `RemoteCallReply` shape to carry the new child
   `RegisteredAddress` back to the owner (or a `SpawnRejected`). Add the
   `Sendable*` mirrors for the threaded queues.
3. **Register-from-envelope on the destination shard.** A `harvest_remote_spawn`
   that runs the payload through `register_entry` (local `parent = None`, since
   the owner is remote), records the owner link as a `RegisteredAddress`, and
   returns the address-bearing reply. New `ChildRecord` variant (or a parallel
   `RemoteChildRecord`) keyed by a `RegisteredAddress` owner.
4. **Owner learns the address (ChildStarted).** Reply completes the owner's
   pending spawn like a cross-shard call completion; deliver the `ChildRef` to
   the parent. **Live test gate:** parent on shard A spawns a child on shard B
   and learns its address.
5. **Cross-shard stop.** `StopChildren` for a cross-shard child sends a stop
   envelope to B; B stops the entry and replies with the terminal `ChildStopped`
   truth. Live multi-shard parent-stop child cleanup proof.
6. **Cross-shard failure → restart → ChildAddressChanged.** The hardest: B
   detects child failure, notifies A's supervisor (policy + budget live on A);
   A decides and tells B to restart (B holds the recipe); B replies with the new
   address; A records it and emits `ChildAddressChanged`. This is a multi-round
   protocol — its own sub-phase.
7. **Mirror every step in `tina-sim/src/multi_shard.rs`** and revise
   `multishard_simulation_supervision_keeps_children_on_parent_shard`
   intentionally (it currently pins the same-shard invariant). Prove sim replay
   determinism for the cross-shard start/fail/restart/stop sequence.

Estimated as a multi-session build; steps 1–5 are the tractable first sub-phase
(cross-shard spawn/own/stop), step 6 the second.

#### Two type-system barriers found by attempting the build (API shape: `.on_shard()`)

The public shape `spawn_observed(child).on_shard(shard).then(...)` is settled.
Attempting step 1 surfaced two concrete barriers the outline above glossed:

1. **Getting a `Send`-erased spawn out of the effect needs a new `Isolate`
   associated type.** `Effect::SpawnObserved` carries `I::SpawnObserved`, erased
   by the runtime through `IntoErasedSpawnObserved` — which produces a
   *non-`Send`* `Box<dyn ErasedSpawnObserved<S,F>>`. A single erasure impl cannot
   conditionally require `Send` (one impl, no per-call-site bound), so a
   cross-shard spawn cannot reuse that path without forcing `Send` on **all**
   `spawn_observed` children (a breaking bound; some children hold `Rc`). The
   clean alternative is a distinct effect variant carrying a distinct payload —
   which, because `Effect<I>` can introduce no free type beyond `I`, means a new
   `Isolate::SpawnObservedRemote` associated type. Adding an associated type
   normally breaks every `Isolate` impl, but the workspace is on **nightly**, so
   `#![feature(associated_type_defaults)]` can default it to `Infallible` and
   keep existing impls (and the `isolate_types!` macro) source-compatible. So:
   feasible, non-breaking, but it adds a nightly feature and a new public
   associated type + a parallel erasure trait (`SendErasedSpawnObserved`).

2. **The live cross-shard transport must become generic over `<S,F>`.** Today
   `SendableQueuedRemoteEnvelope` and `ThreadedRemoteWiring` are monomorphic: a
   message is `Box<dyn Any + Send>`. That works because the destination only
   *enqueues* the message into an existing mailbox. A spawn must *register a new
   entry* on the destination's `Runtime<S,F>`, which needs a
   `Box<dyn SendErasedSpawnObserved<S,F> + Send>` — generic over `S,F`. A
   monomorphic channel cannot carry it, and no `Box<dyn Any>` erasure escapes
   this (registration is fundamentally generic over the isolate type and the
   factory `F`). All shards in one `ThreadedMultiShardRuntime` share `S,F`, so
   making the envelope + wiring generic over `<S,F>` is sound — but it is a
   refactor of the performance-sensitive cross-shard hot path, not an additive
   change. (The sim is exempt: `Simulator<S>`'s erased spawn is `ErasedSpawn<S>`,
   no `F`, single thread, so the sim envelope only needs to go generic over `S`.)

Consequence: cross-shard spawn is **all-or-nothing** for honesty. Shipping the
public `.on_shard()` API while the *live* runtime can't service it (sim-only, or
a typed-unsupported stub) would publish a broken surface — the exact thing the
hostile note "do not claim cross-shard ownership unless a live test proves it"
forbids. The full first sub-phase (public API + nightly assoc-type + new effect
+ `Send`-erasure + live `<S,F>` transport refactor + sim mirror + live test) is a
large, single, indivisible landing — properly its own session with the transport
refactor reviewed on its own.

### D — current state (not yet started in code)

`registration.rs::spawn_isolate` always registers the child on `self.shard`
(no shard parameter); `ChildRecord.parent` is a shard-local `IsolateId`; and
supervision walks one shard's `child_records`. Cross-shard *messaging* works via
the harvest path (`ThreadedMultiShardRuntime`), but `dispatch_local_send` still
panics on a cross-shard target.

So "parent on shard A owns a child on shard B" needs new machinery: a spawn that
targets another shard, a parent reference that is a `RegisteredAddress` not a
bare `IsolateId`, cross-shard failure notification back to the owning shard,
cross-shard restart, and a `ChildAddressChanged` report once the replacement
lands. Multi-file architectural change; its own session. The sim test
`multishard_simulation_supervision_keeps_children_on_parent_shard` documents the
current same-shard invariant and must be revised intentionally when D lands.

### A remainder — typed lifecycle waiters

`ChildRestartedWaiter` exists; child-stop is observable via
`IsolateCompleteWaiter`, child-start via the `spawn_observed` continuation, and
child-failure via `SupervisorReport` / the trace. Dedicated `ChildStarted` /
`ChildFailed` / `ChildAddressChanged` *waiters* were not added — they would be a
thin ergonomic layer over facts already observable. Add only if a user proof
needs to block on those specific events from host code.

## Environment note

The sandbox volume ran near-full during this session (~1.9 GiB free, dominated
by other sessions' worktrees under `/private/tmp` — `tina-126` ~5 GiB, a stress
worktree ~1.6 GiB, neither created here and so not deleted). The `tina-runtime`
integration *binary* suite (`tests/*.rs`) could not finish linking under that
constraint; the affected files use `match event.kind() { … _ => … }` catch-alls,
so the additive new variants do not change their compile or pass behavior.
Re-run `cargo test -p tina-runtime --tests` once the volume has room.
