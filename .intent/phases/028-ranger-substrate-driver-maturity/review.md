# 028 Review

## Plan Review 1

Verdict: ready to hand to implementation.

The plan now has the right phase identity: Ranger is not a service-demo phase,
not a Tokio bridge phase, and not a small polish pass. It is the core runtime
substrate completion phase. The size is intentionally allowed to be as large as
needed, because closing early would only move unfinished core questions into
Gemini, Apollo, or service-framework work.

What looks strong:

- The close criterion is clear: after Ranger, later phases should build on Tina
  core instead of reopening runtime/substrate fundamentals.
- The default substrate direction is pinned: continue hardening Betelgeuse
  unless it blocks a required core semantic. Tokio/Monoio/Glommio/Compio remain
  future adapters unless a pause gate deliberately changes that.
- Full-duplex TCP is named as a core question, with an expected lane model:
  listener accept, stream read, stream write, and close rejection while a lane is
  active.
- Cancellation and shutdown are treated as core semantics, not driver cleanup:
  stopped requester, explicit close, runtime shutdown, late completion,
  requester-mailbox-full completion, and timeout races all require direct proof.
- Live Betelgeuse, simulated Betelgeuse, and `tina-sim` parity is load-bearing.
  Divergence must be fixed or recorded as concrete non-overlap.
- Cost work is scoped correctly: allocation/operation counts for named hot
  paths, not wall-clock benchmark theater.
- The phase requires a core/non-core boundary so later service/docs/adapter
  phases know what they can depend on.

Implementation guardrails:

- Start with the capability audit. Do not begin by rewriting TCP or adding an
  adapter.
- Keep service-shaped work minimal. It exists only to catch lifecycle/substrate
  bugs that focused tests miss.
- Keep `review.md` as the progress ledger: capability gaps, design decisions,
  cost evidence, substrate decision, core boundary, and remaining non-claims.
- Use pause gates aggressively if Betelgeuse cannot support a required semantic
  or if a new core primitive appears necessary.

No blocking plan findings remain.
