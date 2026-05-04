# 034 Piet de Jong Plan Review

Verdict: structurally on-shape and ready to execute. Big rock, but correct big
rock.

What looks strong:

- The phase targets the actual bottleneck: local production readiness, not a
  release/demo story.
- The five workstreams map directly to current Tina gaps: substrate maturity,
  bridge breadth, hardening, performance envelope, and local-service API
  completeness.
- The plan keeps Tina's core refusals intact: no async handlers, no hidden
  unbounded queues, no arbitrary futures inside isolates, no broad Tokio
  replacement claim.
- E2E proof is user-shaped and cross-cutting, not isolated benchmark theater.
- Performance work is framed as an envelope with numbers, not a victory lap.
- API work keeps one preferred path as a hard constraint.

Load-bearing decisions pinned:

1. **No general runtime build.** The substrate work matures Tina's local runner
   and driver contract without becoming a new async ecosystem.
2. **Bridge remains an edge adapter.** Tokio/Tower/Axum integration enters Tina;
   it does not make isolate handlers async.
3. **Supported I/O remains explicit.** DNS/TLS/UDP/file/process/signal are
   decision topics, not silent scope creep.
4. **CI and stress are part of production readiness.** Local `make verify` alone
   is not enough for the next claim.
5. **Gemini stays blocked.** Public release-story work waits until this phase
   proves the core is worth explaining.

Main risk:

- This phase is intentionally large. It must land in reviewable commits with
  clear internal stopping points: runner lifecycle first, bridge breadth second,
  hardening/perf/API completion after the surfaces stop moving.

Implementation guidance:

- Start by auditing existing live runner, bridge, test harness, and scripts.
- Add failing tests for the highest-risk lifecycle/cancellation/bridge behaviors
  before widening APIs.
- Prefer small public helpers that make the canonical path obvious over broad
  abstractions that let users choose five ways to do the same thing.
- Treat every hidden queue/task/allocation as suspect until named and measured.

No plan changes required before launch.
