# 011 Mariner Task Dispatcher Proof

Session:

- A

## In Plain English

The runtime now has enough supervision machinery to make Tina's core promise
visible in a small real workload: a dead worker should not be a dead system.

This slice adds a task-dispatcher proof workload package. A dispatcher isolate
owns restartable worker children and is the user-facing task ingress, and a
tiny registry isolate owns the current worker addresses. Tasks are routed
through the dispatcher, which delegates slot resolution to that registry.
Workers can panic while handling poison tasks. The runtime should stop failed
workers, apply the dispatcher's supervision policy, make stale worker addresses
fail safely, and continue processing later work through the replacement workers
after the registry is refreshed in user space.

This is an end-to-end runtime proof, not the deterministic simulator yet. It
uses the live `CurrentRuntime` and asserts on runtime state plus trace events.
The point is to prove the story humans care about before widening into I/O,
TCP echo, or `tina-sim`.

## What Changes

- Add a Rust task-dispatcher proof workload backed by assertions.
- The workload has one dispatcher isolate and multiple restartable worker
  child isolates.
- The workload also has a small registry isolate that stores the current worker
  addresses in user space.
- The dispatcher is configured as a supervisor for its direct children.
- The dispatcher accepts submitted tasks and routes them through the registry
  isolate rather than through a shared test-owned address table.
- Worker children record completed work through runtime-observable state in the
  test harness.
- In this reference workload, a missing registry slot is a loud programmer
  error: the registry panics instead of silently dropping work.
- At least one task makes a worker panic in each policy proof.
- Under `RestartPolicy::OneForOne`, after the panic:
  - the failed worker stops
  - the dispatcher's supervisor policy restarts only the failed child record
  - the replacement worker gets a fresh isolate identity
  - unaffected sibling workers keep their existing isolate identities
  - sending to the old worker address fails safely with `Closed`
  - after the proof workload refreshes the registry from the restart trace,
    later work can be routed to the replacement worker
- Under `RestartPolicy::OneForAll`, after one worker panics:
  - every worker child record gets a fresh isolate identity
  - every old worker address fails safely with `Closed`
  - after registry refresh, later work can be routed to every replacement
    worker
- Under `RestartPolicy::RestForOne`, after a middle worker panics:
  - the older sibling keeps its existing isolate identity and can keep working
  - the failed worker and younger siblings get fresh isolate identities
  - old addresses for restarted workers fail safely with `Closed`
  - after registry refresh, later work can be routed to the surviving older
    worker and to every replacement worker
- Under a small restart budget:
  - the first supervised failure response can restart the worker group
  - a later supervised failure response can be rejected by budget exhaustion
  - the rejection is visible in the trace and creates no replacement worker
- The proof asserts on the deterministic runtime trace, including panic,
  stop, supervised restart trigger/rejection, restart attempt, skip, and
  restart completion events as applicable.
- The proof asserts repeated identical runs produce identical traces and
  runtime-visible outcomes.
- Add a runnable task-dispatcher example in this slice if it can share the same
  workload shape honestly. The example is a secondary surface; the integration
  test remains the proof.

## What Does Not Change

- No new public runtime API is added unless implementation review proves the
  example cannot be written honestly without it.
- No public replacement-address refresh API is added.
- No logical-name registry is added to the runtime.
- No runtime-owned worker-name registry is added. The registry isolate is
  ordinary user-space application logic, not runtime infrastructure.
- No simulator, virtual time, seed/replay artifact, fault injection, checker
  framework, or `tina-sim` crate is added.
- No I/O, timer, call, TCP echo, Tokio driver, or network runtime behavior is
  added.
- No timed restart-budget window is added.
- No cross-shard behavior is added.
- No new scheduler behavior is added.
- The dispatcher proof does not become the only evidence for supervision
  correctness; existing focused runtime tests and generated-history property
  tests remain the primary semantic proof surface.

## How We Will Verify It

- A task sent before failure is handled by the originally selected worker.
- A task that panics produces `HandlerPanicked` for the selected worker.
- The failed worker produces the existing stop-and-abandon trace subtree.
- The same panic causes `SupervisorRestartTriggered`.
- The supervised restart emits `RestartChildAttempted` and
  `RestartChildCompleted` for the failed child ordinal.
- The replacement worker has a fresh isolate identity and the same stable child
  ordinal.
- A stale address for the failed worker returns `TrySendError::Closed`.
- A sibling worker keeps its original isolate identity and can keep processing
  work under `OneForOne`.
- Tests may observe `SendRejected { reason: Closed }` for stale worker
  addresses before the registry refresh happens. The runtime does not
  auto-route around stale addresses.
- A task submitted to an unregistered slot makes the registry panic visibly
  rather than disappearing behind `Effect::Noop`.
- After registry refresh, later dispatcher-routed work can be delivered to the
  replacement worker and is handled.
- Under `OneForAll`, all workers are replaced and later dispatcher-routed work
  completes through every replacement.
- Under `RestForOne`, the older worker survives while failed/younger workers
  are replaced and all usable addresses continue handling later work.
- Budget exhaustion emits `SupervisorRestartRejected` and creates no
  replacement worker for the rejected failure response.
- Repeated identical task-dispatcher runs produce identical traces and
  identical completed-work evidence.
- The test suite still passes with `make verify`.

## Notes

- This is the first reference-shaped workload proof package for Mariner. The
  registry isolate is part of the reference shape because downstream users are
  likely to copy it.
- Any registry refresh in this slice is still user-space application logic, not
  a product-level address-refresh API.
