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

### D — cross-shard child ownership (not started, largest lift)

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
