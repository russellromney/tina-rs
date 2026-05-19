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

# Plan Review 2

- [fixed] Storage/file/IPC completion was too small and conditional alone. It is
  now folded into durable local state because the user outcome is one thing:
  restartable local services that own local OS rails.
- [fixed] Platform support truth now belongs beside recovery truth. Directory
  sync and rename commit must return typed unsupported with capability evidence
  where the backend cannot prove them.
- [fixed] Local IPC and codecs now have e2e proof requirements, not only local
  parser tests.

Remaining risk: local OS semantics are platform sharp. Implementation review
must check macOS/Linux wording and tests independently.
