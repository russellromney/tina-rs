# 030 Review

## Plan Review 1

Verdict: structurally on-shape and ready to hand to implementation after the
initial capability audit records exact test gaps.

What looks strong:

- The phase is framed as core runtime completion, not a demo/proof theater
  phase.
- It correctly starts from the real baseline: Tina already has a Betelgeuse
  live runtime, simulated I/O, `RuntimeDriver`, `tina-sim`, Ranger cancellation
  semantics, and Surveyor completion-slot release semantics.
- The expected direction is concrete: one composed local server workload with
  listener, connection, worker pool, supervisor, bounded overload, runtime-owned
  TCP/time/calls, and explicit shutdown.
- It refuses the right temptations: remoting, clustering, persistence,
  Tower/Axum, async handlers, a new general-purpose runtime, and demo logs as
  evidence.
- The proof bar is user-shaped: live Betelgeuse, Betelgeuse simulated I/O, and
  `tina-sim` where modeled, with direct assertions around overload, restart,
  timeout, shutdown, and replay.

Implementation cautions:

1. **Do not let the composed workload become a mini framework.**

   The phase needs production-shaped code, but not a reusable server framework
   yet. If helper APIs are required, they should be tiny and obviously part of
   the existing preferred Tina surface. Broader ergonomics belong to Joop den
   Uyl.

2. **Keep live-native and simulated-I/O claims separate.**

   Native Linux/macOS CI is good for real backend ownership and live TCP smoke.
   Simulated I/O is where slow peers, partial writes, delayed completions, and
   exact shutdown interleavings can be made deterministic. The implementation
   should not pretend native CI can prove every interleaving.

3. **Make backpressure direct, not inferred.**

   The important Tina story is visible overload. Tests should force
   `IngressFull`, mailbox `Full`, call timeout, and requester-closed paths
   without sleep-as-proof.

4. **Make shutdown the hard center of the phase.**

   If the server-shaped workload passes but pending accept/read/write/timer/call
   shutdown is not directly asserted, the phase is not done. Surveyor removed
   the leak wart; Willem Drees should prove users can rely on the result.

5. **Do not overclaim performance.**

   Allocation/backpressure probes should catch accidental unbounded buffering or
   obvious new hot-path cost. Full cost-model work belongs to Ruud Lubbers.

Recommended first implementation step:

- Audit the current tests listed in the plan and append an "Implementation Audit
  1" section here with exact existing coverage and exact missing proof. Then
  build the composed workload against the largest missing gap first, likely
  shutdown/backpressure under live threaded runtime plus simulated slow peer
  behavior.

## Plan Review 2

Verdict: ready to implement, with one hard instruction for the first build
step: do the capability audit before writing the composed workload.

Why this is the right next phase:

- 029 is closed enough to build on: local `make verify` passed, Linux/macOS CI
  passed, the shutdown leak fallback is gone, and Surveyor's typed lifecycle
  failure path is tested.
- The roadmap now says the next missing core claim is not docs, not remoting,
  not Akka parity, and not a Tokio bridge. It is the local production runtime
  story.
- 030 targets exactly that: one-process listener/connection/worker/supervisor
  behavior under pressure, with live Betelgeuse, Betelgeuse simulated I/O, and
  `tina-sim` oracle proof where each belongs.

Plan strengths:

- It refuses demo work and release-story polish.
- It keeps remoting, persistence, clustering, Tower/Axum, and async handlers out
  of scope.
- It requires one composed server-shaped workload, which should catch
  integration bugs the current narrow proofs can miss.
- It puts shutdown and backpressure at the center instead of treating them as
  edge cases.
- It requires direct assertions rather than logs or "works on my machine"
  behavior.

Implementation cautions:

1. **Audit first.**

   The repo already has a lot of TCP, shutdown, restart, replay, and overload
   proof. The first implementation step must classify what exists and name the
   missing direct proof. Otherwise 030 can accidentally duplicate old rocks.

2. **One workload, multiple substrates.**

   Prefer one canonical local-server workload shape, then run equivalent slices
   through live Betelgeuse, Betelgeuse simulated I/O, and `tina-sim`. Avoid
   three unrelated test stories that merely share vocabulary.

3. **Keep native and simulated claims honest.**

   Native CI should prove real backend ownership and ordinary TCP smoke.
   Simulated I/O should prove slow peers, partial writes, delayed completions,
   and exact shutdown interleavings. Do not pretend one proves the other.

4. **Do not invent a server framework.**

   If 030 needs helpers, they must be tiny and obviously part of the existing
   Tina shape. Bigger ergonomics belong to Joop den Uyl.

5. **Backpressure must be forced.**

   Tests should directly hit ingress full, worker/mailbox full, timeout, stale
   address, and requester-closed paths without sleeps-as-proof.

Decision:

- Start 030 implementation with an "Implementation Audit 1" section in this
  review file.
- Then build the first composed workload around the largest missing proof,
  likely live/simulated shutdown plus bounded worker pressure.
