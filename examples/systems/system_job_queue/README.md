# Job Queue

A bounded worker pool with synchronous `Submit`, cancel-while-running, and
one-shot worker respawn on crash. Total admission cap is `workers`; a
queued layer on top would just delay the `Busy` reply.

Each scenario is a complete `LocalSystem` application. The host atomically
registers and bootstraps the queue, retains a typed split-service handle, calls
it through `call_blocking_request`, and uses `run_to_shutdown_reported` so a
workload error cannot bypass bounded terminal observation.

The queue spawns `N` worker isolates as observed children. Submit goes through
`RequestCall::defer_cancelable(call_cancelable_request(worker, ...))
.try_admit(&mut self.pending, JobId, ...)`, which returns the child effect only
after the parked caller is stored in a `PendingCancelableCallSet`. Cancel uses
`PendingCancelableCall::cancel` to close the process wait and route the parked
request context into the cancel continuation. A typed worker cancel request
then confirms release of the worker's deferred process slot before the queue
makes that worker available for refill. `Full` and `Timeout` cancel calls retry
through Tina-owned time with a fixed budget, `Closed` retires and respawns the
worker, and impossible replies, `Rejected`, or retry exhaustion stop the queue
instead of silently leaking one unit of admission capacity.

A `Poison` payload panics the worker; the queue sees `CallOutcome::Closed`,
respawns the worker into the same slot, and replies `Failed` to the parked
caller. Retry is intentionally not supported — see Findings.

## Run

```bash
cargo run --manifest-path examples/systems/system_job_queue/Cargo.toml
cargo test --manifest-path examples/systems/system_job_queue/Cargo.toml
```

## Findings

What felt good:
- `try_admit` is the right shape. The "admit caller authority before
  dispatching the child effect" rule is encoded by the type, so the easy
  bug ("dispatch first, then forget to store the token") is unreachable.
  Recovering on `Full` is a single `token.into_request_context()` away.
- `PendingCancelableCall::cancel(translator)` collapses the v1 two-step
  ("send `WorkerMsg::Cancel` AND keep the call handle alive in
  `PendingCallSet` so the late reply still routes through one place")
  into one effect. The cancel API caller is replied immediately; the
  parked submit caller is replied through the cancel continuation.
- `RequestCall::reply_and` makes its sequencing contract explicit: the cancel
  API caller is replied before the token-cancel and worker-cancel follow-ups.
- Worker cancellation remains exhaustive after that reply: `Full`/`Timeout`
  retry within a fixed budget, `Closed` replaces the worker, and protocol
  failures stop the queue rather than leaving the slot permanently charged.
- `LocalSystem` covers the entire application lifecycle without exposing a
  lower threaded owner: atomic root bootstrap, typed host calls, and guaranteed
  terminal observation all remain on one facade.
- Collapsing v1's parallel `PendingReplies` (parked callers) +
  `PendingCallSet` (call handles) into one `PendingCancelableCallSet`
  removed about 130 lines of accounting. The two halves were always keyed
  by the same `JobId`; one structure says it once.

What felt rough:
- **`try_admit` does not compose with retry-on-crash.** The pattern binds
  one `RequestContext` to one `CallHandle` at admission time. If the
  worker dies, the token's `RequestContext` is consumed (or the token is
  dropped) — there is no API for "take the request context out, keep it,
  and rebind it to a *fresh* `call_cancelable` to retry on a different
  worker." So v2 marks `Failed` on first crash. v1 had a retry budget.
  This is a real specimen finding, not a missing feature: a system that
  needs retry has to keep separate `pending_callers: HashMap<JobId,
  RequestContext>` and `in_flight_handles: HashMap<JobId, CallHandle>`
  structures, which is exactly what v1 did.
- **`cancel_call` closes the wait so cleanly that there is no late-reply
  observation surface.** v2 originally tried to count
  `late_replies_swallowed`; the counter was always 0 because the runtime
  rejects the worker's late reply before our translator can fire. That is
  the right behavior. The queue therefore uses a separate typed cancel request
  to confirm that the worker released its deferred process slot.
- A bootstrap event is still required because constructors cannot return
  effects. `LocalSystem::register_split_service_with_bootstrap` makes that
  first-message ordering atomic without exposing the private service envelope
  or lower threaded owner.
- There is still no in-isolate hook for "my child stopped." The queue
  detects a dead worker through `CallOutcome::Closed` on the in-flight
  call. That works for jobs in flight; a worker that dies *between* jobs
  is silently absent until the next dispatch tries it (and is then
  replaced reactively). `runtime.observe_child_restarted(parent)` exists,
  but only outside the isolate.

What we sidestepped — the natural-key trap:
- `PendingCancelableCallSet::try_insert` is single-slot per key and
  returns a loud `DuplicateKey` if the key is already present (correct
  ABA-safety behavior — see PR #92's docs). This specimen sidesteps the
  trap with monotonic `JobId`s. A service whose pending entries are
  keyed by something the *outside world* picks (worker index, session
  id, tenant id, externally-supplied request id) cannot use monotonic
  ids and would have to either swap to `PendingReplies` (which allows
  multiple slots per key but loses the cancelable-handle pairing) or
  hand-roll a `(natural_key, generation)` compound. Captured as a
  roadmap proposal — see "Natural-key admission for cancelable pending
  sets" in `ROADMAP.md`.

Tina capability pulled:
- `PendingCancelableCallSet` + `defer_cancelable(...).try_admit(...)` for
  one-step caller admission with typed `Full` recovery.
- `PendingCancelableCall::cancel(translator)` for atomic
  cancel-and-answer-original-caller.
- `spawn_observed(ChildDefinition::new(...))` for typed child refs.
- `LocalSystem::register_split_service_with_bootstrap` for atomic startup.
- `LocalSystem::call_blocking_request` and typed service handles for host work.
- `LocalSystem::run_to_shutdown_reported` for unconditional clean shutdown.
- Runtime-owned `sleep` as the worker's only async surface.

Suggested follow-up:
- An API for "take the request context out and rebind it to a fresh
  cancelable child call" if retry-on-crash is supposed to be expressible
  with this helper. Otherwise the docs should explicitly say "for retry
  semantics, do not use `PendingCancelableCallSet`."
- In-isolate child-stopped / child-restarted events. Same finding as the
  v1 README and `system_bounded_object_lane`.

Verdict:
- keep
