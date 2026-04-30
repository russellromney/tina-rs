# 015 Mariner Runtime-Owned Time And Retry Plan

Session:

- A

## What We Are Building

Add the first honest runtime-owned time capability to
`tina-runtime-current` and prove it through a small retry/backoff workload.

Concretely, this package adds:

- one-shot relative timer/sleep as a runtime-owned call
- runtime-owned wake delivery through the existing call translator path
- a retry/backoff workload that uses the timer path like a user would

## What Will Not Change

- This is a Mariner capability package, not Voyager simulator work.
- `tina` stays substrate-neutral.
- Handlers stay synchronous.
- `CurrentRuntime::step()` stays synchronous and caller-driven.
- The timer surface stays intentionally minimal:
  - one-shot
  - relative duration
  - no cancellation
  - no repeating timers
  - no absolute wall-clock deadlines
- Completion must stay on the existing runtime-owned call path:
  ordinary later `Message` values produced by the call translator.
- The package does not broaden into scheduler design, simulator
  architecture, periodic timers, or production timer-wheel claims.

## How We Will Build It

- Shipped runtime time semantics are pinned:
  - `CurrentRuntime` owns a monotonic clock
  - each `step()` samples that clock once at the start of the step
  - any timer due at or before that sampled instant is eligible in that step
  - `Sleep { after }` means "wake no earlier than `armed_at + after` on a
    future step"
- For equal-deadline timers, wake order is deterministic request order,
  using the runtime's insertion / call-order tie-break.
- The proof should avoid brittle wall-clock sleeps where possible. A
  crate-private runtime clock seam is acceptable if it preserves the shipped
  semantics above and makes timer behavior directly testable.
- The retry/backoff workload should stay small and bounded. This is about
  proving runtime-owned delay, not building a job scheduler.

## Build Steps

1. Extend the current runtime call vocabulary with a one-shot relative timer
   request/result pair.
2. Add runtime-owned timer tracking / wake delivery in
   `tina-runtime-current` while keeping `step()` synchronous.
3. If needed, introduce the smallest crate-private clock/timer seam that lets
   tests drive due-time behavior deterministically without changing the `tina`
   boundary.
4. Add direct timer proof coverage:
   - does not fire early
   - fires once
   - deterministic ordering for multiple timers with different delays
   - deterministic request-order tie-break for equal-deadline timers
   - late completion rejection after requester stop
5. Add a retry/backoff proof workload:
   - first attempt fails
   - isolate schedules retry through runtime-owned timer
   - later retry succeeds
   - trace proves the delayed retry came through the timer path
6. Add or refresh a runnable example if the workload is example-worthy and
   remains assertion-backed.
7. Close out docs and evidence:
   - `.intent/SYSTEM.md`
   - `ROADMAP.md`
   - `CHANGELOG.md`
   - phase `commits.txt`
   - review receipt

## How We Will Prove It

Direct proof for the changed behavior:

- Focused timer tests for:
  - single timer wake
  - no early wake
  - multiple timers with due-time ordering
  - equal-deadline deterministic request-order wake
  - requester-stopped rejection
- Focused retry/backoff integration/e2e-style workload test for:
  - initial failure
  - delayed retry
  - later success
  - trace evidence of timer dispatch/completion on the retry path
- If a crate-private clock seam exists, direct deterministic tests should use
  it instead of thread sleeps.

Proof modes expected in this package:

- unit proof:
  local timer bookkeeping and due-time ordering logic
- integration proof:
  timer request -> runtime wake -> translated `Message`
- e2e proof:
  retry/backoff workload through the public runtime path
- adversarial proof:
  late completion rejection after requester stop
- deterministic-order proof:
  both different-deadline ordering and equal-deadline tie-break ordering

Surrogate proof that helps but does not close the claim:

- any proof that only shows "a message arrived later" without proving it came
  from the timer path
- any proof that relies on wall-clock delays without pinning due-time
  semantics directly

## How We Will Prove We Did Not Break Earlier Intent

Blast-radius proof for already-shipped behavior:

- Existing TCP and batch proof surfaces still pass.
- Existing dispatcher, supervision, address-generation, and trace-shape
  proofs still pass through `make verify`.
- `make verify`

## Pause Gates

Stop and ask for human input only if one of these happens:

- the cleanest timer implementation appears to require a new public `tina`
  boundary shape instead of fitting the current call model
- deterministic proof seems to require simulator-style infrastructure that is
  too large for this package
- the timer design starts pulling in repeating timers, cancellation, absolute
  deadlines, or broader scheduler semantics
- the retry/backoff workload stops being a small proof workload and starts
  turning into a job system or queueing subsystem
- Betelgeuse itself would need deep substrate surgery unrelated to the timer
  proof we are actually trying to land

## Traps

- Do not implement time by sleeping inside handlers.
- Do not make the proof depend on brittle wall-clock timing if a direct
  runtime-driven seam can prove the behavior more honestly.
- Do not expose wall-clock queries at the `tina` trait boundary.
- Do not turn the timer primitive into a general callback surface.
- Do not skip direct timer proofs and rely only on the retry workload.
- Do not silently widen the scope into simulator work, periodic timers, or
  cancellation semantics.

## Files Likely To Change

- `tina-runtime-current/src/call.rs`
- `tina-runtime-current/src/io_backend.rs` and/or nearby runtime execution
  code if timer delivery belongs there
- `tina-runtime-current/src/lib.rs`
- `tina-runtime-current/tests/` for focused timer and retry/backoff proofs
- `tina-runtime-current/examples/` if the retry workload earns an example
- `.intent/SYSTEM.md`
- `ROADMAP.md`
- `CHANGELOG.md`

## Areas That Should Not Be Touched

- mailbox crates
- supervisor policy semantics beyond using the already-shipped surface
- multi-shard or simulator crates
- vendored Betelgeuse unless the chosen timer execution path genuinely depends
  on a small substrate extension
- Tokio bridge code

## Report Back

- Exact timer request/result vocabulary chosen
- Whether a crate-private clock seam was introduced
- Exact shipped runtime time-progression semantics implemented
- Exact retry/backoff workload shape
- How late-completion rejection was proved
- How equal-deadline ordering was implemented and proved
- Whether the runtime needed any new trace vocabulary
- What is directly proved vs only supported by surrogate proof
- Verification results, especially whether direct timer proofs avoided
  wall-clock flakiness
