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

Tests: `tina-runtime` lib (501) and `tina-sim` lib (54) green; sim
`supervision_simulation` (12) and `multishard_dispatcher` green; clippy clean on
`tina`, `tina-runtime`, `tina-sim`, `tina-tracing`, `tina-tokio-bridge`.

## Open capability gaps

### B — parent-stop child cleanup (not started)

Today (`tina-runtime/src/dispatch.rs::stop_entry_full`) a parent stop closes
only the parent's own mailbox and cancels only the parent's own calls. Children
keep running and are GC'd only once no child record references the stopped
entry.

Tension to respect: the existing test
`stopped_supervisor_rejects_later_child_failure_without_replacement` sends a
panic to a child *after* its supervisor parent has stopped and asserts
`SupervisorRestartRejected { SupervisorStopped }`. That only holds because the
child outlives the parent. So cascade-stop-on-parent-stop cannot become the
default `Effect::Stop` behavior without breaking a pinned guarantee.

Suggested shape: a separate opt-in "supervised shutdown" (e.g.
`Effect::StopChildren`, or a supervisor-config flag) that (1) marks the parent
stopping / stops admission, (2) walks `child_records` for that parent and stops
each child with a distinct cause, (3) emits a terminal report. Leave plain
`Effect::Stop` unchanged. The "owner stop while child has in-flight call settles
caller visibly" proof belongs here, alongside a no-leaked-leases/permits/pending
proof.

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
