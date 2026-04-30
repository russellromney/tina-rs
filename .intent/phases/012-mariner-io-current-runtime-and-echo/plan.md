# 012 Mariner I/O, Current Runtime, And Echo Plan

Session:

- A

## Goal

Land one coherent Mariner package that adds a runtime-owned I/O/call contract,
teaches `tina-runtime-current` to execute it on an explicit completion-driven
current-thread backend, and proves the result with an assertion-backed TCP echo
workload.

This package should move `tina-rs` from "proved internal runtime mechanics" to
"proved real external work through the effect model" without making the review
process the main product.

## Context

What already exists:

- `tina` has a closed synchronous effect model plus supervision policy types.
- `tina-runtime-current` already proves local send/spawn, stop-and-abandon,
  panic capture, address liveness, direct-child restart execution, supervised
  panic restart, dispatcher proof workloads, and generated trace/replay-style
  property tests.
- The runtime trace is already the semantic model for future simulation work.

What is still missing:

- there is no runtime-owned I/O/call effect vocabulary
- there is no honest completion-driven current-thread driver path for external
  I/O
- there is no Rust TCP echo proof or example

The roadmap already says echo should not land before the Rust I/O/timer/call
contract exists, so this package should treat those as one story, not three
disconnected chores.

## References

- `.intent/SYSTEM.md`
- `ROADMAP.md`, especially:
  - "Current evidence snapshot"
  - "Phase Mariner"
  - "Mariner I/O, current runtime, and echo"
- `tina/src/lib.rs`
- `tina-runtime-current/src/lib.rs`
- `tina-runtime-current/src/trace.rs`
- `tina-runtime-current/tests/task_dispatcher.rs`
- `tina-runtime-current/tests/runtime_properties.rs`

## Package Decisions

- This is one reviewed package with autonomous internal slices, not one giant
  unreviewed patch and not a string of tiny human-blocking review loops.
- The `tina` boundary grows by one runtime-owned call family, not one top-level
  effect variant per verb.
- Handlers remain synchronous. No `async fn handle`.
- Runtime-owned call completion arrives later as an ordinary isolate message.
  The call request carries the translation needed to turn a runtime result into
  one `Message` value for the isolate that issued it.
- Mariner should prefer a completion-driven explicit-step backend over a
  futures executor when both are plausible, because that better preserves
  Tina's visible stepping model and DST ambitions.
- Betelgeuse is the explicit backend target for this package: completion-based
  I/O, no hidden tasks, caller-driven stepping, simulation-friendly shape.
- Nightly Rust is acceptable for this proof-of-concept phase. Betelgeuse's
  nightly requirement is therefore not, by itself, a blocker for 012.
- The `tina` call contract still belongs to `tina`, not to Betelgeuse. The
  public boundary must stay backend-neutral even though the first Mariner
  implementation target is concrete.
- Betelgeuse's bus factor is acceptable at this stage because `tina-rs` is
  still a proof-of-concept Rust implementation of Tina's reference discipline.
  The load-bearing safety valve is the backend-neutral `tina` contract, not a
  precommitted fallback crate shape.
- Compio, monoio, glommio, and Tokio remain useful comparison points, but they
  are not the default Mariner direction for this package because they center an
  async runtime / `await` programming model rather than explicit completion
  ownership at the application state-machine layer.
- If Betelgeuse proves materially unworkable for Mariner, we must record the
  concrete reason in the artifacts before falling back to a different
  substrate.
- "Materially unworkable" should mean something concrete, such as:
  - missing TCP primitives required for the echo slice
  - completion behavior that prevents explicit runtime control over ordering
  - runtime invariants that force hidden task progression into Tina semantics
  - upstream stall serious enough to block the package from landing or being
    maintained honestly
- Runtime-owned resources are opaque ids in isolate-facing state/messages.
  Raw backend handles do not cross into isolate code.
- The first current-runtime call family for 012 covers TCP listener bind,
  accept, read, write, and close.
- Runtime-owned sleep/timer wake is still part of the broader project
  direction, but it is no longer a gate for this first Betelgeuse-backed TCP
  slice.
- The current-runtime call family may use current-runtime-specific request and
  completion types in `tina-runtime-current`; `tina` only owns the general
  effect slot, not backend-specific details.
- `CurrentRuntime::step()` stays synchronous. Backend integration is owned
  privately inside `tina-runtime-current`; we do not make `step()` async and we
  do not add a parallel async stepping API in this package.
- The runtime trace must stay the semantic source of truth. New I/O behavior
  gets visible trace outcomes with causal links.
- The call contract and driver path should preserve DST-friendly constraints:
  controllable completion ordering, no required wall-clock leakage into the
  `tina` boundary, and deterministic runtime-owned resource ids where the
  runtime rather than the OS is assigning identity.
- Live echo tests prove behavior and visible outcomes, not full replay-grade
  determinism under OS scheduling.
- No broad runtime allocation claim is added in this package.
- Keep the current runtime single-shard and current-thread.
- The call contract must still read as a plausible fit for future file I/O,
  UDP, HTTP client work, child-process spawn, and deferred timers. Echo is not
  allowed to force a contract that only makes sense for TCP.
- This package is about correctness of the first Rust I/O path, not about
  hitting Mariner's future 100k-connection evaluation target yet.

## Internal Slice Stack

This package should execute in this order under autonomous bucket mode.

### Slice 012.1: Call Contract

- extend `tina` with the new runtime-owned call effect family
- extend trait-surface tests to prove the shape
- confirm the Betelgeuse fit and define the current-runtime-specific call and
  completion vocabulary
- pin the completion-envelope shape after the package decision:
  call payload carries a translator from runtime result to later `Message`
- extend runtime trace vocabulary for call dispatch / completion / failure

### Slice 012.2: Current-Thread Driver

- teach `tina-runtime-current` to execute the current-runtime call family on
  the chosen completion-driven backend
- keep all resource ownership inside the runtime
- deliver completions back as ordinary isolate messages on later turns
- add focused runtime tests for non-network call behavior and trace outcomes

### Slice 012.3: Echo Proof And Example

- add an assertion-backed TCP echo integration test
- add a runnable `tcp_echo` example that mirrors the tested workload
- keep continuity with slice 011's reference shape: listener isolate supervises
  restartable connection-handler isolates
- prove network behavior and trace evidence together

### Slice 012.4: Package Closeout

- update `SYSTEM.md`, `ROADMAP.md`, `CHANGELOG.md`
- record reviews and commit hashes

## Mapping From Package To Code

- `tina/src/lib.rs`
  - add the new `Isolate::Call` associated type
  - add the new `Effect::Call(...)` variant
  - keep compile-fail/doc/downstream-style tests honest
- `tina-runtime-current/src/trace.rs`
  - add runtime event kinds for call dispatch attempt and visible outcomes
- `tina-runtime-current/src/lib.rs`
  - add current-runtime call execution
  - add resource tables for listeners/streams/timers
  - keep the runtime-owned envelope private
- `tina-runtime-current/tests/runtime_properties.rs`
  - extend generated histories only where a bounded invariant genuinely belongs
- `tina-runtime-current/tests/`
  - add focused I/O/call tests
  - add `tcp_echo.rs` integration test
- `tina-runtime-current/examples/`
  - add `tcp_echo.rs`

## Proposed Implementation Approach

1. Add the generic call slot to `tina`.
2. Update the trait-surface tests in `tina/tests/` so downstream-style code can
   define isolates that use a call effect without using async handlers.
3. Define the current-runtime-specific call/request vocabulary.
   It should be concrete enough for echo and timer wakeups without exposing raw
   backend handles.
4. Sketch and implement the completion-envelope shape:
   - call request bundles the information needed to turn a runtime result into
     one ordinary later isolate message
   - runtime stores no second public handler entry point
   - mailbox message shape remains the isolate's single `Message` type
5. Define the current-runtime-specific completion/event vocabulary.
   It must be representable as ordinary later messages.
6. Extend `tina-runtime-current`'s internal runtime message envelope so it can
   carry both user messages and runtime-generated call completions without
   changing the public runtime ingress surface.
7. Extend the runtime trace with visible call dispatch / completion / failure
   outcomes.
8. Implement current-thread execution for:
   - bind listener
   - accept
   - read
   - write
   - close
9. Add focused non-network tests first:
   - later-turn completion delivery
   - visible call completion/failure trace events
   - resource ownership never escapes into isolate code
10. Add the echo integration test:
   - one listener isolate supervising restartable connection-handler isolates
   - one or more connections
   - bind listener to a concrete high loopback port
   - read bytes
   - write bytes back
   - assert exact echoed payloads
   - assert visible trace outcomes for the call path
11. Add the runnable `tcp_echo` example:
    - keep it aligned with the test workload
    - keep assertions where possible
    - do not let it become the only proof surface
12. Extend `runtime_properties.rs` only if a general bounded invariant appears
    naturally from the new call path.
13. Close out docs and phase bookkeeping after implementation is accepted.

## Acceptance

- `tina` exposes a runtime-owned call effect family without making handlers
  async.
- The completion path returns through the isolate's ordinary `Message` type
  rather than a second public handler entry point.
- `tina-runtime-current` can execute its first concrete call family on the
  chosen completion-driven current-thread backend.
- `CurrentRuntime::step()` remains synchronous through the package.
- Call completion is delivered later as ordinary isolate messages.
- Listener/stream ownership stays inside the runtime; isolate code uses only
  opaque ids and messages.
- The runtime trace shows visible call dispatch outcomes with causal links.
- Focused current-runtime tests prove the new call path behavior without
  relying only on the TCP echo test.
- `tcp_echo.rs` proves request bytes are echoed back correctly through the real
  runtime.
- The echo proof topology keeps faith with Tina's isolate/supervision model
  rather than introducing a one-off bypass around restartable workers.
- The package leaves a plausible path for future file I/O, UDP, HTTP client
  work, child-process spawn, and deferred timers without redesigning the `tina`
  boundary.
- `examples/tcp_echo.rs` exists and mirrors the tested workload closely enough
  to be a truthful smoke surface.
- `make verify` passes.

## Pause Gates

Stop and ask for human input only if one of these happens:

- the `tina` call-effect shape has two or more materially plausible public API
  directions
- runtime trace additions would force Voyager to consume a different semantic
  model than Mariner emits
- the current-thread driver cannot support an honest echo proof without hidden
  side channels or handler-owned I/O
- the backend cannot support the TCP-first slice honestly without reconstructing
  `127.0.0.1:0` through a side probe; in that case, narrow the proof to a
  concrete loopback port instead of pretending ephemeral bind is solved
- Betelgeuse itself cannot support the TCP-first slice on nightly with the
  project's explicit-step requirements intact
- the call contract starts reading as TCP-only rather than a plausible fit for
  future file I/O, UDP, HTTP client work, child-process spawn, and deferred
  timers
- the chosen backend direction stops looking compatible with explicit stepping,
  caller-owned completion state, or future DST-style control
- Betelgeuse proves materially unworkable by the concrete criteria above rather
  than by vague discomfort or ecosystem anxiety
- the package needs a broader runtime allocation claim than the repo currently
  supports
- backend-specific concepts start leaking into `tina` itself instead of staying
  in `tina-runtime-current`

## Traps

- Do not make handlers async.
- Do not put raw backend socket handles into isolate state or messages.
- Do not add a top-level `Effect` variant for every I/O verb.
- Do not let the runtime silently complete I/O without trace evidence.
- Do not claim live socket tests are replay-grade deterministic.
- Do not let the echo example become the only proof of the new I/O path.
- Do not start background services outside the test/example process.
- Do not widen supervision semantics while doing I/O work.
- Do not quietly re-center the package on a futures executor just because it is
  more familiar.

## Files Likely To Change

- `tina/src/lib.rs`
- `tina/tests/sputnik_api.rs`
- `tina-runtime-current/src/lib.rs`
- `tina-runtime-current/src/trace.rs`
- `tina-runtime-current/tests/runtime_properties.rs`
- new focused current-runtime tests under `tina-runtime-current/tests/`
- `tina-runtime-current/examples/tcp_echo.rs`
- `CHANGELOG.md`
- `ROADMAP.md`
- `.intent/SYSTEM.md`
- `.intent/phases/012-mariner-io-current-runtime-and-echo/*`

## Areas That Should Not Be Touched

- `tina-mailbox-spsc` unsafe internals
- cross-shard runtime design
- `tina-supervisor` public surface, unless the call contract unexpectedly
  proves it must change
- simulator crates that do not exist yet
- `tina.png`

## Commands

- `make test`
- `make verify`

## Report Back

- whether the package-level contract stayed coherent through implementation
- any pause-gate trips
- what evidence now proves runtime-owned I/O works
