# 024 Mercury Plan Review

Status: superseded by Session B plan rewrite. The review remains as the
historical first review of the roadmap seed; the current `plan.md` now makes
the overload lab the first proof and moves monoio behind the practical Tokio
current-thread backend decision.

Reviewed:

- `ROADMAP.md` Mercury section as the current plan seed

Verdict: right next phase, not ready to implement from the roadmap entry alone.
Grug likes the target, but Mercury needs a real `plan.md` before build. The
roadmap now protects the ordering: Mercury before Gemini. Good. The missing
work is mostly pinning semantics and slice order so we do not accidentally
build three half-solutions to "tryable runtime."

## What Looks Right

- Mercury is correctly placed before Gemini. Docs/release can wait until the
  runtime contract is true.
- The phase target is honest: selected Tokio-shaped workloads, not broad Tokio
  replacement.
- The work list matches the Tina/Odin spirit: thread-per-shard runtime,
  bounded overload feedback, timeout request/reply, live supervision, and DST
  proof.
- The plan keeps monoio as a decision/spike rather than pretending current
  threaded+Betelgeuse is automatically production-shaped.

## Findings

1. **[P1] Roadmap seed is not an implementation plan.**
   Mercury currently exists as a roadmap row plus a short phase section. That
   is enough to preserve build order, but not enough to launch implementation.
   It needs `.intent/phases/024-mercury-production-shaped-runtime-contract/plan.md`
   with build steps, proof matrix, pause gates, and exact non-claims.

2. **[P1] User-visible backpressure semantics are not pinned.**
   This is the most load-bearing Mercury decision. Tina-Odin gets immediate
   `ctx_send` outcomes because send happens inside the handler. tina-rs handlers
   return effects, so a normal `send(...)` cannot synchronously return
   `Accepted` / `Full` / `Closed` to the same handler without changing the
   effect model. The plan must choose the semantic shape:
   - a send-attempt effect that later delivers a typed outcome message,
   - a context-side immediate `try_send` escape hatch,
   - or a narrower "observed send" call-like helper.

   Grug bias: preserve effect-returning handlers and make observed send a
   later-message outcome, then prove ordinary fire-and-forget `send(...)` stays
   simple.

3. **[P1] Isolate-to-isolate call depends on the backpressure decision.**
   A timeout call is likely "send request, wait for reply, timeout if no reply,"
   but request admission can itself be `Full` / `Closed`. The plan must pin how
   those outcomes compose: immediate call failure message, later call failure,
   trace-only failure, or panic. Mandatory timeout is good, but not sufficient.

4. **[P1] Monoio spike needs a narrow success/fallback bar.**
   "Spike monoio" can sprawl. The plan should define a smallest acceptable
   substrate proof: one shard, one current-thread monoio reactor, runtime-owned
   TCP echo, synchronous handlers unchanged, and oracle/substrate trace subset.
   If that fights the model, the phase should record a fallback decision and
   continue Mercury on threaded+Betelgeuse rather than stalling everything.

5. **[P1] One phase package needs explicit slice order.**
   The scope is right but large. Order matters:
   1. substrate seam audit + monoio decision spike
   2. observed send outcome
   3. timeout call
   4. runner lifecycle hardening
   5. live spawned-child cross-shard proof
   6. live supervision proof
   7. dogfood service
   8. capacity/allocation claim
   9. public-ish DST harness

   Dogfood should not start before backpressure and timeout semantics exist.

6. **[P2] Dogfood workload is not specified enough.**
   "Real service-shaped workload" is right, but closeout needs a named shape.
   Recommendation: TCP ingress with per-connection isolate, worker/session shard,
   timeout call to worker/session, overload policy on observed send/call
   failure, supervised worker restart, and simulator replay/checker over the
   same isolate logic.

7. **[P2] Capacity/allocation claim needs expected direction.**
   Mercury should probably not chase Tina-Odin's full no-hidden-allocation story
   yet. Pin expected direction as: bounded queues and bounded configured
   capacities are claimed; broad runtime allocation-free behavior is not
   claimed unless a focused probe proves a narrower path.

8. **[P2] Public-ish DST harness scope should be tiny.**
   Avoid turning Mercury into a docs/framework packaging phase. The harness can
   be one reusable test helper/API that runs a user workload under seed/replay
   and optional checker. It does not need a polished guide until Gemini.

## Suggested Done Means

- A real Mercury `plan.md` exists and resolves the semantic choices above.
- Monoio is either proven by a tiny substrate workload or explicitly deferred
  with reason.
- App code has one blessed way to observe send/call overload outcomes.
- Timeout request/reply is tested through runtime and simulator.
- Live substrate proves spawned-child cross-shard work and supervision restart.
- One named dogfood service exercises TCP ingress, timeout call, overload
  policy, restart, and replay/checker pressure.
- Claims say "try selected workloads" and still refuse broad production/Tokio
  replacement claims.
