# Plan Review 1

- [fixed] "durable queue or outbox" was too vague. The plan now chooses
  `DurableOutbox` first form with stable `WorkId`, bounded capacity, and
  at-least-once semantics.
- [fixed] Duplicate/replay behavior was under-specified. The plan now names
  replayed pending work, idempotent mark-complete, and typed duplicate outcomes.
- [fixed] The plan did not state persistence blast radius. Added non-change
  rules for snapshot/journal APIs, recovery outcomes, trace facts, and durable
  simulator image behavior.

Remaining risk: exactly-once wording can creep in during docs/specimens. Review
must reject any "once" claim stronger than the outbox semantics prove.
