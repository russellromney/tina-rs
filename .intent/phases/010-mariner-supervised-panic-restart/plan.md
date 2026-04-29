# 010 Mariner Supervised Panic Restart Plan

Session:

- A

## Goal

Make `CurrentRuntime` perform the first real supervised restart behavior:
when a direct child handler panics, a configured supervisor parent applies a
restart policy and budget to decide which direct child records restart.

This should be the payoff for the recent Mariner groundwork:

- parent-child lineage
- address liveness
- restartable child records
- direct-child restart execution
- deterministic causal trace trees

## Context

The runtime can already catch handler panics and stop the failed isolate. It can
also restart direct child records when a parent explicitly returns
`Effect::RestartChildren`.

What is missing is the supervision bridge:

- a parent-owned policy
- a budget tracker
- an automatic trigger when a child fails
- trace events that explain why restart did or did not happen

## Mapping From Spec Diff To Implementation

1. Add a new workspace crate:
   - `tina-supervisor`
   - depends on `tina`
   - exports `SupervisorConfig`
2. Update workspace membership and lockfile.
3. Add `tina-supervisor` as a dependency of `tina-runtime-current`.
4. Add private supervisor records in `CurrentRuntime`.
5. Add a public runtime API to configure supervision for a registered parent.
6. Extend the handler-panic path:
   - catch panic
   - emit `HandlerPanicked`
   - stop the failed child with the existing stop-and-abandon helper
   - ask the supervision path whether the failed child has a configured live
     supervisor parent
   - apply policy/budget if configured
7. Reuse the slice 009 restart helper for selected child records.
8. Extend generated-history property tests enough to include supervised panic
   restarts.

## Proposed Public API

In `tina-supervisor`:

```rust
pub struct SupervisorConfig {
    policy: tina::RestartPolicy,
    budget: tina::RestartBudget,
}
```

Likely methods:

```rust
impl SupervisorConfig {
    pub const fn new(policy: RestartPolicy, budget: RestartBudget) -> Self;
    pub const fn policy(&self) -> RestartPolicy;
    pub const fn budget(&self) -> RestartBudget;
}
```

In `tina-runtime-current`:

```rust
impl<S, F> CurrentRuntime<S, F>
where
    S: Shard,
    F: MailboxFactory,
{
    pub fn supervise<M: 'static>(
        &mut self,
        parent: Address<M>,
        config: SupervisorConfig,
    );
}
```

`supervise` is intentionally a runtime configuration API, not a handler effect.
Handlers still do not mutate supervision tables directly.

## Phase Decisions

- Supervised restart is triggered only by unwinding handler panic in this slice.
- Normal `Effect::Stop` is not a failure and does not trigger supervision.
- Manual `Effect::RestartChildren` keeps slice 009 semantics and does not
  consult supervisor policy or budget.
- A supervisor is configured by runtime API before the failure occurs.
- `supervise(...)` panics for unknown, stale, or cross-shard parent addresses.
  These are setup-time programmer errors, matching existing runtime API
  precedent.
- Calling `supervise(...)` before children exist is valid; the config applies
  to later child failures.
- Reconfiguring the same supervisor parent resets the budget tracker. A new
  config represents a new supervisory relationship.
- `supervise(...)` is called between `step()` invocations; no in-handler or
  concurrent configuration surface exists.
- Supervision applies only to direct child records of the configured parent.
- Root isolate panics do not restart because they have no parent child record.
- If a child has a parent but that parent has no supervisor config, current
  panic-capture behavior remains unchanged.
- If a supervisor parent is already stopped, the runtime traces a supervised
  restart rejection and creates no replacements.
- Budget windows last for the runtime lifetime in this slice. No time/reset
  behavior yet, and exhaustion is permanent within that runtime lifetime.
- Budget accounting counts one supervised failure response, not one replacement
  child. This matches OTP-style restart intensity: the budget limits failure
  frequency, not the amount of restart work caused by one failure.
- Policy-excluded siblings are not traced individually. The supervised trigger
  event records the policy and failed child ordinal; child-level restart events
  are emitted only for selected records. Replay tools derive exclusions from
  policy, failed ordinal, and stored child-record order.
- Selected non-restartable children emit existing `RestartChildSkipped` with
  `RestartSkippedReason::NotRestartable`.
- Replacement children get fresh isolate identities through the existing
  register/spawn path.
- Child records mutate in place and preserve stable ordinals.
- Supervisor records persist after the supervisor parent stops, like other
  runtime-owned records. Later child failures can still find the config and
  produce a `SupervisorStopped` rejection.
- The trace remains a deterministic causal tree. A supervised trigger may cause
  multiple child restart attempts and may share the panic event as a sibling of
  `IsolateStopped`. Supervised restart can make the tree arbitrary depth; trace
  consumers must walk causes recursively.

## Trace Vocabulary

Add runtime event kinds similar to:

```rust
SupervisorRestartTriggered {
    policy: RestartPolicy,
    failed_child: IsolateId,
    failed_ordinal: usize,
}

SupervisorRestartRejected {
    failed_child: IsolateId,
    failed_ordinal: usize,
    reason: SupervisionRejectedReason,
}
```

Add:

```rust
pub enum SupervisionRejectedReason {
    BudgetExceeded {
        attempted_restart: u32,
        max_restarts: u32,
    },
    SupervisorStopped,
}
```

Cause shape:

- `HandlerPanicked` causes `IsolateStopped` for the failed child.
- `HandlerPanicked` also causes `SupervisorRestartTriggered` when a live
  supervisor is configured and budget allows.
- `SupervisorRestartTriggered` causes each selected `RestartChildAttempted`.
- `RestartChildAttempted` continues to cause `RestartChildSkipped` or
  `RestartChildCompleted`.
- If the selected child was still running, `RestartChildAttempted` also causes
  that child's `IsolateStopped`.
- If supervision is configured but rejected, `HandlerPanicked` causes
  `SupervisorRestartRejected`.

Unsupervised child panics emit no supervised restart trigger/rejection event.

## Proposed Implementation Approach

1. Add `tina-supervisor` crate with `SupervisorConfig` and tests.
   This slice intentionally ships config vocabulary only; runtime mechanism
   remains in `tina-runtime-current`.
2. Extend `CurrentRuntime` with:
   - `supervisors: Vec<SupervisorRecord>`
   - `SupervisorRecord { parent, config, budget_state }`
3. Add `CurrentRuntime::supervise`.
   - panic on cross-shard parent
   - panic on unknown parent
   - panic on stale generation
   - replacing an existing supervisor config for the same parent resets the
     budget tracker
4. Add helpers:
   - find child record index by child address identity
   - find supervisor record by parent identity
   - collect policy-selected child-record indices by stable ordinal
   - execute supervised restart for a failed child record
5. Refactor the existing restart helper if needed so manual
   `RestartChildren` and supervised restart can share child-record restart
   execution without changing manual semantics.
6. Extend `runtime_properties.rs`:
   - generated histories include a supervised parent/child setup
   - panics can trigger supervised restart
   - every supervised trigger has visible child outcomes or a rejection

## Acceptance

- `tina-supervisor` has direct tests for `SupervisorConfig`.
- A configured one-for-one supervisor restarts only the failed direct child.
- A configured one-for-all supervisor restarts every direct child.
- A configured rest-for-one supervisor restarts failed and younger children,
  not older children.
- A selected non-restartable child is skipped with trace and no panic.
- Policy-excluded children keep their old isolate identity and do not emit
  restart child events.
- Budget exhaustion emits `SupervisorRestartRejected` and no replacement child
  records are created.
- A stopped supervisor emits `SupervisorRestartRejected` and no replacement
  child records are created.
- An unsupervised child panic still only stops the child.
- Normal child stop does not trigger supervision.
- Old failed-child addresses return `Closed`; replacement addresses from
  crate-private snapshots can receive messages and run later.
- Repeated identical runs produce identical trace, lineage, child-record, and
  supervisor-state snapshots.
- Generated-history tests cover supervised panic restart enough to keep causal
  links well formed and restart outcomes visible.
- `make test`, `make verify`, and `make miri` pass.

## Traps

- Do not make supervision state a trace reconstruction.
- Do not restart on normal `Effect::Stop`.
- Do not make manual `Effect::RestartChildren` consult supervisor policy.
- Do not expose child-record snapshots publicly.
- Do not expose replacement addresses publicly.
- Do not catch restart factory panics as handler panics.
- Do not make policy-excluded siblings look like failed or skipped children.
- Do not consume restart recipes on first use.
- Do not reuse isolate ids.
- Do not add timed budget windows in this slice.
- Do not add the task-dispatcher example in this slice.
- Do not add I/O/timer/call effects in this slice.
- Do not add a simulator hook or public trace query API in this slice.

## Files Likely To Change

- `Cargo.toml`
- `Cargo.lock`
- `tina-supervisor/Cargo.toml`
- `tina-supervisor/src/lib.rs`
- `tina-supervisor/tests/*.rs`
- `tina-runtime-current/Cargo.toml`
- `tina-runtime-current/src/lib.rs`
- `tina-runtime-current/tests/runtime_properties.rs`

## Areas That Should Not Be Touched

- `tina-mailbox-spsc`
- SPSC Loom/Miri tests
- I/O/timer effect vocabulary
- Tokio/current-thread driver work
- simulator/Voyager code
- README/ROADMAP unless review finds a stale statement or real contradiction

## Review Decisions

1. `CurrentRuntime::supervise` panics for stale, unknown, or cross-shard parent
   addresses.
2. Reconfiguring a supervisor resets its budget tracker.
3. Budget accounting is per supervised failure response, not per replacement
   child.
4. Policy-excluded children are silent in the trace; replay derives exclusion
   from policy plus child-record order.
5. `SupervisorRestartTriggered` is caused by `HandlerPanicked`, so the failed
   child stop and supervised response are sibling consequences of the panic.
