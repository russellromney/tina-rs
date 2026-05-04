# 026 Tina TCP Driver Contract Plan

## Purpose

Own the small TCP/time runtime substrate boundary inside Tina.

Betelgeuse is the best current live substrate, and 025 makes that path honest.
026 should make sure Tina is not permanently fused to one backend
implementation. The goal is a tiny Tina-owned driver contract that preserves
the synchronous isolate model while allowing native, simulated, and future
adapter backends to plug in.

This is **not** a plan to build Tokio, a futures runtime, or a broad actor
framework.

The first driver contract should be built around the effects Tina already
ships:

- sleep / timeout
- TCP bind
- TCP accept
- TCP read
- TCP write
- TCP stream/listener close
- cancellation and shutdown of pending completions

## Desired Shape

The runtime should talk to a small driver boundary shaped roughly like:

```rust
trait Driver {
    fn submit(&mut self, call_id: CallId, op: DriverOp);
    fn step(&mut self) -> Vec<DriverCompletion>;
    fn cancel(&mut self, call_id: CallId);
    fn shutdown(&mut self) -> Vec<DriverCompletion>;
}
```

Names may change during implementation. The meaning should not:

- Tina owns isolate scheduling, bounded mailboxes, trace events, supervision,
  and call outcomes.
- The driver owns substrate operations: time, TCP, completion readiness,
  cancellation, and wakeup.
- User handlers stay synchronous and return `Effect<Self>`.
- No user future enters the isolate turn.

## Build Scope

1. Inspect the current `IoBackend`, `BetelgeuseRuntime`, and simulated
   Betelgeuse TCP backend seams.
2. Define the smallest driver boundary that covers current runtime-owned
   calls: sleep, TCP bind, accept, read, write, close, cancellation, shutdown.
3. Refactor native Betelgeuse I/O behind that boundary.
4. Refactor Betelgeuse simulated I/O behind the same boundary.
5. Keep bounded ingress and cross-shard queues outside the driver as Tina
   runtime semantics.
6. Prove the same user-shaped workloads across:
   - explicit runtime oracle
   - native Betelgeuse-backed runtime
   - simulated-driver runtime
7. Add allocation/cost probes for the touched abstraction path.
8. Preserve the public Tina effect/helper surface. This is substrate work, not
   new user-facing syntax work.
9. Record any adapter possibilities discovered while working, but do not build
   Tokio, Monoio, Glommio, or Compio bridges in 026.

## TCP-Specific Proofs

026 should prove the driver contract through user-shaped TCP behavior, not only
unit-level driver calls:

- one-client TCP echo
- sequential multi-client TCP echo
- overlapping-client TCP echo where ordering claims stay honest
- partial-write retry
- pending accept canceled by shutdown
- pending read/write canceled by stopped requester or shutdown
- invalid listener/stream ids become typed runtime failures
- native Betelgeuse and simulated Betelgeuse exercise the same Tina driver
  path after the refactor

The simulated backend should stay a deterministic peer, not a protocol
framework.

## Refusals

- Do not add async isolate handlers.
- Do not expose raw backend handles to user isolates.
- Do not add unbounded queues to make a backend easier.
- Do not depend on Actix, Ractor, or another actor framework as Tina's core
  substrate unless it preserves explicit step, bounded queues, and replay.
- Do not build Tower/Axum integration here.
- Do not claim production performance without measured evidence.
- Do not push app/protocol semantics down into Betelgeuse or the driver.

## Done Means

- Tina has a backend-neutral TCP/time driver contract inside `tina-runtime`.
- Existing Betelgeuse native behavior still passes.
- Existing Betelgeuse simulated TCP proof still passes.
- Shutdown/cancel/timeout semantics remain visible through Tina trace events.
- The driver boundary does not weaken bounded queues, replay, or synchronous
  handler semantics.
- The roadmap can explain the production path without saying "trust one
  backend forever."
