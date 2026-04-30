# 014 Mariner TCP Completeness Plan

Session:

- A

## Goal

Turn the current one-client TCP proof into a credible first TCP server
surface by making the listener re-arm and by proving the runtime can keep
serving across multiple client connections, using the smallest honest
`tina` boundary extension required to express that workflow.

## Decisions

- This is still a Mariner TCP package, not time/timers and not simulator
  work.
- The package should stay black-box and workload-oriented where possible:
  prove the server shape through integration tests and the runnable example,
  not through runtime internals.
- We are optimizing for an honest and understandable first server surface,
  not for high concurrency claims or benchmark numbers.
- 014 adds one new public effect shape: ordered batching of existing effects.
  This is the minimum change needed to express spawn-then-rearm honestly.
- Re-arm shape is pinned to self-messages plus ordered batching, not a new
  open-ended callback surface:
  - listener captures `ctx.current_address::<ListenerMsg>()`
  - `Accepted { stream }` can batch "spawn child" then "send self
    ReArmAccept"
  - any move toward post-spawn callbacks or another richer public shape trips
    the pause gate
- Graceful listener shutdown is in scope. The bounded reference workload should
  close the listener socket through `TcpListenerClose` and then stop cleanly.
- The existing one-client TCP echo proof stays as a smaller smoke alongside the
  new multi-client coverage.
- Example termination policy is pinned: accept exactly `N` clients, assert,
  close, exit.
- `has_in_flight_calls()` is sufficient for this package's wait loops unless
  implementation reveals a hidden non-call source of pending work.

## Package Steps

1. Add ordered batch semantics to `tina` and `tina-runtime-current`.
2. Prove the batch semantics directly:
   - left-to-right execution
   - non-terminal effects do not stop later effects
   - `Stop` short-circuits later effects
3. Rework the listener workflow so `TcpAccept` is re-armed through normal
   isolate control flow using self-targeted listener messages plus batched
   spawn/re-arm.
4. Update the connection lifecycle as needed so accepted children can be
   spawned/supervised repeatedly without one-shot assumptions.
5. Expand the TCP proof surface:
   - sequential multi-client round-trips
   - at least one run with two accepted connections simultaneously pending in
     `IoBackend`
   - continued trace assertions per call kind / lifecycle event
   - listener close / stop after the bounded workload completes
6. Refresh the runnable `tcp_echo` example to match the new server shape.
7. Close out docs and evidence:
   - `.intent/SYSTEM.md`
   - `ROADMAP.md`
   - `CHANGELOG.md`
   - phase `commits.txt` / review receipt

## Verification

- Focused TCP integration tests for:
  - multiple sequential client connections
  - at least two in-flight accepted connections
  - listener re-arm after a completed connection
  - continued supervised child boot/read/write/close path
  - graceful listener close / stop
- Focused batch-effect tests for:
  - left-to-right send/spawn/call sequencing
  - `Stop` short-circuit behavior
- Trace assertions for:
  - exactly one successful `TcpBind`
  - at least `N` successful `TcpAccept`
  - at least `N` connection-child spawns
  - at least `N` successful `TcpStreamClose`
  - no listener rebuild
- Runnable `tcp_echo` example still passes its own assertions
- CI deadline budget:
  - use a generous runtime deadline (10 s) for multi-client live tests
  - prefer waiting on observed state plus `has_in_flight_calls()` rather than
    brittle fixed step counts
- `make verify`

## Pause Gates

Stop and ask for human input only if one of these happens:

- re-arming `TcpAccept` reveals a need for a different public effect shape
- the cleanest server workflow appears to require a logical-name registry or
  other user-facing runtime API beyond the current package scope
- ordered batching proves too weak and would need hidden callbacks or runtime-
  special-case behavior to make the listener honest
- the package starts turning into timer work, simulator work, or broad socket
  API expansion rather than TCP server completeness
- proving the server shape honestly would require claims about concurrency or
  performance that we do not actually want to make yet

## Traps

- Do not reintroduce harness-side lifecycle kicks or manual trace-driven
  control flow.
- Do not silently keep the listener one-shot while broadening only the test
  wrapper.
- Do not let the example become a logs-only demo.
- Do not assert cross-stream ordering; assert per-stream payload integrity and
  runtime event multiplicity only.
- Do not bundle timers, benchmarks, or multi-shard concerns into this
  package.

## Files Likely To Change

- `tina-runtime-current/tests/tcp_echo.rs`
- `tina-runtime-current/examples/tcp_echo.rs`
- `tina/src/lib.rs`
- `tina/tests/sputnik_api.rs`
- `tina-runtime-current/src/io_backend.rs` only if proof needs a small runtime
  observation helper or accept-path tightening
- `tina-runtime-current/src/lib.rs` if listener lifecycle tests need a crate-
  private helper shape
- `tina-runtime-current/tests/trace_core.rs` and/or a focused runtime test file
  for ordered batch semantics

## Areas That Should Not Be Touched

- mailbox crates
- supervisor policy semantics beyond using the already-shipped surface
- timer / sleep vocabulary
- multi-shard or simulator code
- vendored Betelgeuse unless TCP proof exposes a real substrate bug unrelated
  to batching

## Report Back

- Exact listener message / state shape chosen in code
- Exact ordered-batch semantics chosen in code
- Whether `has_in_flight_calls()` stayed sufficient
- Verification results, especially live multi-client tests
- Any evidence that would force a bigger public runtime shape than this plan
