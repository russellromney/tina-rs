# 010 Mariner Supervised Panic Restart

## In Plain English

`CurrentRuntime` can already restart a parent's direct restartable children when
that parent explicitly returns `Effect::RestartChildren`.

That is useful, but it is not supervision yet. Supervision means a child can
fail and the runtime can apply a parent-owned policy to decide which children
restart.

This slice makes the first narrow version of that real:

When a child handler panics, the runtime already turns that panic into
`HandlerPanicked`, stops the child, and keeps the round going. After this slice,
if the child has a configured supervisor parent, the runtime will apply that
parent's restart policy and restart the selected direct child records.

This is still not the whole supervision story. It only reacts to unwinding
handler panics. It does not restart on normal `Effect::Stop`, does not introduce
timed budget windows, does not add a task-dispatcher example yet, and does not
make replacement addresses public.

## What Changes

- Add a `tina-supervisor` crate to hold supervisor mechanism types that are more
  concrete than the shared policy vocabulary in `tina`.
- Add a `SupervisorConfig` type that combines:
  - `RestartPolicy`
  - `RestartBudget`
- Add a `CurrentRuntime::supervise(...)` API that marks one registered isolate
  as the supervisor for its direct children.
- Store supervisor runtime state privately in `CurrentRuntime`.
- When a child handler panics:
  - keep the existing panic-capture behavior
  - stop and abandon the failed child exactly as slice 004/003 already do
  - if the failed child has a configured live supervisor parent, apply the
    supervisor's restart policy to that parent's direct child records
  - restart selected restartable child records using the existing slice 009
    restart path
  - visibly skip selected non-restartable children
  - leave policy-excluded siblings untouched
- Restart-policy selection uses stable child ordinals and
  `ChildRelation::from_ordinals`.
- `RestartPolicy::OneForOne` restarts only the failed child record.
- `RestartPolicy::OneForAll` restarts every direct child record of the
  supervisor parent.
- `RestartPolicy::RestForOne` restarts the failed child record and younger
  siblings.
- The restart budget is checked before policy-selected restarts run.
- In this slice, the restart budget window is the runtime lifetime. There is no
  automatic reset yet.
- Budget accounting counts one supervised failure response, not one replacement
  child. A one-for-all restart caused by one failed child consumes one budget
  unit.
- If the budget is exhausted, the runtime emits a visible rejection event and
  does not restart any selected child.
- If the supervisor parent is already stopped, the runtime emits a visible
  rejection event and does not restart any selected child.
- Add trace events for supervised restart trigger/rejection so replay can tell
  the difference between:
  - an unsupervised panic
  - a supervised panic whose restart policy ran
  - a supervised panic rejected by budget or stopped supervisor
- Existing restart child events remain the child-level trace vocabulary:
  `RestartChildAttempted`, `RestartChildSkipped`, and
  `RestartChildCompleted`.

## What Does Not Change

- `Effect::RestartChildren` keeps its current manual semantics: restart all
  direct restartable children of the isolate that returned the effect.
- Normal `Effect::Stop` is not considered a failure and does not trigger
  supervisor restart behavior.
- Root isolate panics remain just panic-capture behavior unless a future API
  introduces root supervisors.
- Replacement addresses remain private runtime state in this slice.
- No logical-name registry or address refresh API is added.
- No public child-record inspection API is added.
- No task-dispatcher example is added in this slice.
- No I/O, timer, call, Tokio driver, simulator, or cross-shard behavior is
  added.
- Restart factory panics still propagate as runtime construction errors and are
  not caught as handler panics.
- The runtime does not reconstruct supervision state from the trace.

## How We Will Verify It

- A supervised one-for-one parent restarts only the failed child after that
  child panics.
- A supervised one-for-all parent restarts every direct restartable child after
  one child panics.
- A supervised rest-for-one parent restarts the failed child and younger
  siblings, while older siblings keep their existing isolate identity.
- Policy-selected non-restartable children emit `RestartChildSkipped {
  reason: NotRestartable }` and do not panic the runtime.
- Policy-excluded siblings are left running and do not emit child restart
  events.
- Budget exhaustion emits a supervised restart rejection event and creates no
  replacement children.
- A stopped supervisor parent causes a supervised restart rejection event and
  creates no replacement children.
- An unsupervised child panic preserves current behavior: panic event, stopped
  child, no restart subtree.
- A normal child `Effect::Stop` does not trigger supervisor restart behavior.
- Restarted child records preserve stable child ordinals and get fresh isolate
  identities.
- Old failed-child addresses return `Closed`.
- The trace remains deterministic and causally well formed across repeated
  identical runs and generated histories.

## Autonomy Note

This slice contains public API and mechanism-split choices, so it should be
reviewed before implementation.

The intended default choices are:

- `tina-supervisor` holds mechanism/config types.
- `CurrentRuntime` stores runtime state and executes the mechanism.
- Budget accounting is per supervised failure response for this slice.
- Budget windows last for the runtime lifetime until a later time/window slice
  introduces resets.
